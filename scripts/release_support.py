"""Public release provenance and distributable checks shared by both publishers."""
import hashlib
import json
import re
import tomllib

from macos_signing import ROOT, run

REPOSITORY = "oana-ffg/vardr"
PLATFORMS = ("darwin-aarch64", "darwin-x86_64")
APP_ID = "com.firefungames.vardr"
CLI_ID = "com.firefungames.vardr.cli"
INSTALLER_ID = "com.firefungames.vardr.installer"


def version():
    value = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
    if not re.fullmatch(r"\d+\.\d+\.\d+", value):
        raise ValueError("Release version must be a stable semantic version")
    return value


def unsigned_names():
    return {"vardr-linux-x86_64"} | {
        name for platform in PLATFORMS
        for name in (f"vardr-{platform}", f"Vardr-{platform}.app.tar.gz", f"Vardr-{platform}.dmg")
    }


def update_names():
    return {name for name in unsigned_names() if not name.endswith(".dmg")}


def final_names():
    return unsigned_names() | {name + ".sig" for name in update_names()} | {"latest.json", "desktop-latest.json"}


def hashes(directory):
    return {name: hashlib.sha256((directory / name).read_bytes()).hexdigest()
            for name in sorted(unsigned_names())}


def validate_hashes(directory, expected):
    if not isinstance(expected, dict) or set(expected) != unsigned_names() or any(
        not isinstance(value, str) or not re.fullmatch(r"[a-f0-9]{64}", value)
        for value in expected.values()
    ):
        raise ValueError("Expected hashes must cover exactly the seven distributables")
    found = {path.name for path in directory.iterdir()}
    if not unsigned_names() <= found <= final_names():
        raise ValueError("Unexpected or missing draft files")
    if any(not path.is_file() or path.is_symlink() for path in directory.iterdir()):
        raise ValueError("Draft contains links or non-file entries")
    if hashes(directory) != expected:
        raise ValueError("Draft bytes differ from the local Apple-verified distributables")


def reviewed_commit():
    if run("git", "status", "--porcelain").strip():
        raise ValueError("Release requires a clean checkout")
    commit = run("git", "rev-parse", "HEAD").decode().strip()
    remote = run("git", "ls-remote", "origin", "refs/heads/main").decode().split()
    if not remote or remote[0] != commit:
        raise ValueError("Check out the reviewed origin/main commit before publishing")
    return commit


def require_candidate_run(run_id, commit):
    data = json.loads(run("gh", "run", "view", run_id, "--repo", REPOSITORY,
                          "--json", "event,headBranch,headSha,conclusion,workflowName"))
    validate_run(data, commit, "Build release candidates")


def validate_run(data, commit, workflow):
    if (data.get("event") != "workflow_dispatch" or data.get("headBranch") != "main"
            or data.get("headSha") != commit or data.get("conclusion") != "success"
            or data.get("workflowName") != workflow):
        raise ValueError("Expected a successful main dispatch for this exact commit")


def require_new_version():
    tag = "v" + version()
    if run("git", "ls-remote", "--tags", "--refs", "origin", "refs/tags/" + tag).strip():
        raise ValueError(f"Tag {tag} already exists")
    pages = json.loads(run("gh", "api", "--paginate", "--slurp", f"repos/{REPOSITORY}/releases"))
    if any(release.get("tag_name") == tag for page in pages for release in page):
        raise ValueError(f"Release {tag} already exists, including drafts")


def require_draft(tag, commit):
    if tag != "v" + version():
        raise ValueError("Draft tag does not match the reviewed version")
    data = json.loads(run("gh", "release", "view", tag, "--repo", REPOSITORY,
                          "--json", "isDraft,targetCommitish,tagName"))
    if not data.get("isDraft") or data.get("targetCommitish") != commit or data.get("tagName") != tag:
        raise ValueError("Only an unpublished draft for this exact commit may be signed")


def verify_updates(directory):
    verifier = ROOT / "target/release/vardr"
    for name in sorted(update_names()):
        run(verifier, "verify", directory / name, directory / (name + ".sig"),
            "--app-version", version())


def validate_manifests(directory):
    expected = {"latest.json": {"darwin-aarch64", "darwin-x86_64", "linux-x86_64"},
                "desktop-latest.json": set(PLATFORMS)}
    for filename, platforms in expected.items():
        data = json.loads((directory / filename).read_text())
        if data.get("version") != version() or set(data.get("platforms", {})) != platforms:
            raise ValueError("Release manifest version/platform mismatch")
        for platform, asset in data["platforms"].items():
            name = f"vardr-{platform}" if filename == "latest.json" else f"Vardr-{platform}.app.tar.gz"
            url = f"https://github.com/{REPOSITORY}/releases/download/v{version()}/{name}"
            if asset.get("url") != url or asset.get("signature") != (directory / (name + ".sig")).read_text().strip():
                raise ValueError("Release manifest does not describe the verified artifact")
