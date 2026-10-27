"""Verify an unsigned app candidate, its version and bundled CLI."""
import pathlib
import plistlib
import subprocess
import argparse
import tomllib

root = pathlib.Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("bundle")
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
print(f"Verified Vardr.app {version} with bundled CLI")
