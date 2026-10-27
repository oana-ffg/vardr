"""Verify an unsigned app candidate, its version and bundled CLI."""
import pathlib
import plistlib
import subprocess
import argparse

from release_support import APP_ID, version

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("bundle")
args = parser.parse_args()
bundle = pathlib.Path(args.bundle).resolve()
expected = version()
with (bundle / "Contents/Info.plist").open("rb") as stream:
    info = plistlib.load(stream)
assert info["CFBundleIdentifier"] == APP_ID
assert info["CFBundleShortVersionString"] == expected
helper = bundle / "Contents/MacOS/vardr"
assert subprocess.check_output([str(helper), "--version"], text=True).strip() == f"vardr {expected}"
subprocess.run(["codesign", "--verify", "--deep", "--strict", str(bundle)], check=True)
print(f"Verified Vardr.app {expected} with bundled CLI")
