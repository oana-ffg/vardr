"""Sign only the local publisher's exact Apple-verified draft bytes in CI."""
import argparse
import json
import os
import hashlib
import plistlib
from pathlib import Path
import tempfile

from macos_signing import APP, extract_updater, run, verify_publisher
from release_support import (APP_ID, CLI_ID, INSTALLER_ID, PLATFORMS, REPOSITORY,
                             require_candidate_run, require_draft, update_names, validate_hashes,
                             validate_manifests, verify_updates, version)


def verify_apple(directory, team):
    for platform in PLATFORMS:
        cli = directory / f"vardr-{platform}"
        verify_publisher(cli, CLI_ID, team)
        run("codesign", "--verify", "--strict", "-R=notarized", "--check-notarization", cli)
        with tempfile.TemporaryDirectory(prefix="vardr-archive-verify-") as temporary:
            extracted = Path(temporary)
            extract_updater(directory / f"Vardr-{platform}.app.tar.gz", extracted)
            app = extracted / APP
            info = plistlib.loads((app / "Contents/Info.plist").read_bytes())
            if info.get("CFBundleShortVersionString") != version() or info.get("CFBundleIdentifier") != APP_ID:
                raise ValueError("App identity/version differs from reviewed source")
            helper = app / "Contents/MacOS/vardr"
            if hashlib.sha256(helper.read_bytes()).digest() != hashlib.sha256(cli.read_bytes()).digest():
                raise ValueError("Standalone CLI differs from the bundled notarized helper")
            architecture = "arm64" if platform == "darwin-aarch64" else "x86_64"
            for binary in [helper, app / "Contents/MacOS" / info["CFBundleExecutable"]]:
                if run("lipo", "-archs", binary).decode().split() != [architecture]:
                    raise ValueError("App/CLI architecture mismatch")
            verify_publisher(app, APP_ID, team)
            verify_publisher(app / "Contents/MacOS/vardr", CLI_ID, team)
            run("xcrun", "stapler", "validate", app)
            run("/usr/sbin/spctl", "--assess", "--type", "execute", app)
        installer = directory / f"Vardr-{platform}.dmg"
        verify_publisher(installer, INSTALLER_ID, team, runtime=False)
        run("xcrun", "stapler", "validate", installer)
        run("/usr/sbin/spctl", "--assess", "--type", "open", "--context", "context:primary-signature", installer)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["prepare", "sign", "upload"])
    parser.add_argument("--directory", type=Path, required=True)
    args = parser.parse_args()
    try:
        if os.environ.get("GITHUB_ACTIONS") != "true" or os.environ.get("GITHUB_REF") != "refs/heads/main":
            raise ValueError("Update signing requires the main-only release environment")
        tag = os.environ["RELEASE_TAG"]
        commit = os.environ["GITHUB_SHA"]
        directory = args.directory.resolve()
        expected = json.loads(os.environ["VERIFIED_HASHES"])
        if args.command == "prepare":
            require_draft(tag, commit)
            directory.mkdir(parents=True, exist_ok=False)
            run("gh", "release", "download", tag, "--repo", REPOSITORY, "--dir", directory)
        validate_hashes(directory, expected)
        if args.command == "prepare":
            candidate_run = os.environ["CANDIDATE_RUN_ID"]
            require_candidate_run(candidate_run, commit)
            with tempfile.TemporaryDirectory(prefix="vardr-linux-provenance-") as temporary:
                run("gh", "run", "download", candidate_run, "--repo", REPOSITORY,
                    "--name", "linux-x86_64", "--dir", temporary)
                built = Path(temporary) / "vardr-linux-x86_64"
                if hashlib.sha256(built.read_bytes()).hexdigest() != expected["vardr-linux-x86_64"]:
                    raise ValueError("Linux binary differs from the exact main candidate run")
            verify_apple(directory, os.environ["APPLE_TEAM_ID"])
        elif args.command == "sign":
            # The old updater key is Actions-only. Apple authority is never supplied here.
            if not os.environ.get("TAURI_SIGNING_PRIVATE_KEY"):
                raise ValueError("Existing Vardr updater key is missing")
            for name in sorted(update_names()):
                run("node", "desktop/node_modules/@tauri-apps/cli/tauri.js", "signer", "sign",
                    "--app-version", version(), directory / name, sensitive=True)
        else:
            verify_updates(directory)
            run("python3", "scripts/release-manifest.py", directory, version(), REPOSITORY)
            validate_manifests(directory)
            require_draft(tag, commit)
            uploads = sorted(directory.glob("*.sig")) + [directory / "latest.json", directory / "desktop-latest.json"]
            run("gh", "release", "upload", tag, *uploads, "--repo", REPOSITORY, "--clobber")
            print("Uploaded verified update signatures/manifests to the unpublished draft")
    except (ValueError, OSError, KeyError) as error:
        parser.exit(1, str(error) + "\n")


if __name__ == "__main__":
    main()
