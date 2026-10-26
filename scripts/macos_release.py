"""Finish unsigned CI candidates locally without sending Apple authority to CI."""
import argparse
import json
import os
from pathlib import Path
import plistlib
import re
import shutil
import signal
import sys

from macos_signing import (APP, extract_updater, notarize, pack_updater, run,
                           sign, signing_identity, validate_candidate_archive, verify_publisher)
from release_support import (APP_ID, CLI_ID, INSTALLER_ID, PLATFORMS, REPOSITORY,
                             final_names, hashes, require_candidate_run, require_draft,
                             require_new_version, reviewed_commit, validate_hashes,
                             validate_manifests, validate_run, verify_updates, version)


def release(args):
    if sys.platform != "darwin" or os.environ.get("GITHUB_ACTIONS"):
        raise ValueError("Apple publisher signing must run on the local Mac")
    commit = reviewed_commit()
    require_candidate_run(args.run_id, commit)
    require_new_version()
    run("cargo", "build", "--release", "--locked", "-p", "vardr")
    output = args.output.expanduser().resolve()
    output.mkdir(mode=0o700, parents=True, exist_ok=False)
    downloads = output / "candidates"
    artifacts = output / "artifacts"
    artifacts.mkdir()
    run("gh", "run", "download", args.run_id, "--repo", REPOSITORY, "--dir", downloads)
    shutil.copyfile(downloads / "linux-x86_64/vardr-linux-x86_64", artifacts / "vardr-linux-x86_64")
    prepared = []
    for platform, architecture in zip(PLATFORMS, ["arm64", "x86_64"]):
        archive = downloads / platform / f"Vardr-candidate-{platform}.zip"
        validate_candidate_archive(archive)
        destination = output / platform
        run("ditto", "-x", "-k", archive, destination)
        app = destination / APP
        info = plistlib.loads((app / "Contents/Info.plist").read_bytes())
        if info.get("CFBundleIdentifier") != APP_ID or info.get("CFBundleShortVersionString") != version():
            raise ValueError("Candidate identity/version mismatch")
        for binary in [app / "Contents/MacOS/vardr", app / "Contents/MacOS" / info["CFBundleExecutable"]]:
            if run("lipo", "-archs", binary).decode().split() != [architecture]:
                raise ValueError("Candidate architecture mismatch")
        prepared.append((platform, app))
    with signing_identity(args.certificate, args.signing_key_entry) as identity:
        for _, app in prepared:
            sign(app / "Contents/MacOS/vardr", CLI_ID, args.team_id, identity)
            sign(app, APP_ID, args.team_id, identity)
    # Notarization waits happen only after the signing Keychain is closed.
    for platform, app in prepared:
        archive = output / f"{platform}-notary.zip"
        run("ditto", "--norsrc", "--noextattr", "--noacl", "-c", "-k", "--keepParent", app, archive)
        notarize(archive, args.notary_entry, output / f"{platform}-notary.json")
        run("xcrun", "stapler", "staple", app)
        run("xcrun", "stapler", "validate", app)
        run("/usr/sbin/spctl", "--assess", "--type", "execute", "--verbose=2", app)
        updater = artifacts / f"Vardr-{platform}.app.tar.gz"
        pack_updater(app, updater)
        extracted = output / f"{platform}-archive-check"
        extracted.mkdir()
        extract_updater(updater, extracted)
        verify_publisher(extracted / APP, APP_ID, args.team_id)
        verify_publisher(extracted / APP / "Contents/MacOS/vardr", CLI_ID, args.team_id)
        run("xcrun", "stapler", "validate", extracted / APP)
        run("/usr/sbin/spctl", "--assess", "--type", "execute", "--verbose=2", extracted / APP)
        cli = artifacts / f"vardr-{platform}"
        shutil.copyfile(app / "Contents/MacOS/vardr", cli)
        cli.chmod(0o755)
        # This identical binary's ticket is covered by the accepted app submission.
        run("codesign", "--verify", "--strict", "-R=notarized", "--check-notarization", cli)
        run("python3", "scripts/create-desktop-dmg.py", app, artifacts / f"Vardr-{platform}.dmg")
    with signing_identity(args.certificate, args.signing_key_entry) as identity:
        for platform in PLATFORMS:
            sign(artifacts / f"Vardr-{platform}.dmg", INSTALLER_ID, args.team_id, identity, runtime=False)
    for platform in PLATFORMS:
        installer = artifacts / f"Vardr-{platform}.dmg"
        notarize(installer, args.notary_entry, output / f"{platform}-dmg-notary.json")
        run("xcrun", "stapler", "staple", installer)
        run("xcrun", "stapler", "validate", installer)
        run("/usr/sbin/spctl", "--assess", "--type", "open", "--context", "context:primary-signature", installer)
    expected = hashes(artifacts)
    (output / "apple-verified-hashes.json").write_text(json.dumps(expected, indent=2) + "\n")
    print(f"Apple-verified artifacts (update signatures still pending): {artifacts}")
    if args.draft:
        require_new_version()
        tag = "v" + version()
        run("gh", "release", "create", tag, *sorted(artifacts.iterdir()), "--repo", REPOSITORY,
            "--draft", "--target", commit, "--title", "Vardr " + version(), "--generate-notes")
        dispatch = run("gh", "workflow", "run", "sign-release.yml", "--repo", REPOSITORY, "--ref", "main", "--json",
                       data=json.dumps({"tag": tag, "hashes": json.dumps(expected), "candidate_run": args.run_id}).encode())
        match = re.search(rb"/actions/runs/(\d+)", dispatch)
        if not match:
            raise ValueError("Draft retained; could not identify the dispatched update-signing run")
        run_id = match[1].decode()
        run("gh", "run", "watch", run_id, "--repo", REPOSITORY, "--exit-status", "--interval", "60")
        metadata = json.loads(run("gh", "run", "view", run_id, "--repo", REPOSITORY,
                                  "--json", "event,headBranch,headSha,conclusion,workflowName"))
        validate_run(metadata, commit, "Sign verified update artifacts")
        require_draft(tag, commit)
        readback = output / "draft-readback"
        run("gh", "release", "download", tag, "--repo", REPOSITORY, "--dir", readback)
        validate_hashes(readback, expected)
        if {path.name for path in readback.iterdir()} != final_names():
            raise ValueError("Draft must contain exactly the fourteen verified final files")
        verify_updates(readback)
        validate_manifests(readback)
        for path in readback.iterdir():
            if not (artifacts / path.name).exists():
                shutil.copyfile(path, artifacts / path.name)
        print(f"Verified unpublished draft {tag}, including uploaded-byte and update-signature readback")


def interrupted(signum, _frame):
    raise InterruptedError(f"Interrupted by signal {signum}; cleaning up credentials")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--certificate", type=lambda v: Path(v).expanduser().resolve(), required=True)
    parser.add_argument("--signing-key-entry", required=True)
    parser.add_argument("--team-id", required=True)
    parser.add_argument("--notary-entry", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--draft", action="store_true")
    args = parser.parse_args()
    handlers = {s: signal.signal(s, interrupted) for s in (signal.SIGTERM, signal.SIGHUP)}
    try:
        release(args)
    except (ValueError, OSError, KeyError) as error:
        parser.exit(1, str(error) + "\n")
    finally:
        for signum, handler in handlers.items():
            signal.signal(signum, handler)


if __name__ == "__main__":
    main()
