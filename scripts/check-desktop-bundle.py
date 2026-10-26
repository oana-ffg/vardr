"""Verify app version, bundled CLI, and optional publisher signing requirements."""
import pathlib
import plistlib
import subprocess
import argparse
import tomllib

from macos_signing import verify_publisher

root = pathlib.Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("bundle")
parser.add_argument("--publisher-team-id")
args = parser.parse_args()
bundle = pathlib.Path(args.bundle).resolve()
version = tomllib.loads((root / "Cargo.toml").read_text())["package"]["version"]
with (bundle / "Contents/Info.plist").open("rb") as stream:
    info = plistlib.load(stream)
assert info["CFBundleIdentifier"] == "dev.vardr.desktop"
assert info["CFBundleShortVersionString"] == version
helper = bundle / "Contents/MacOS/vardr"
assert subprocess.check_output([str(helper), "--version"], text=True).strip() == f"vardr {version}"
subprocess.run(["codesign", "--verify", "--deep", "--strict", str(bundle)], check=True)
if args.publisher_team_id is not None:
    verify_publisher(helper, "dev.vardr.cli", args.publisher_team_id)
    verify_publisher(bundle, "dev.vardr.desktop", args.publisher_team_id)
print(f"Verified Vardr.app {version} with bundled CLI")
