"""Verify the packaged app's version, bundled CLI, and ad-hoc code signature."""
import pathlib
import plistlib
import subprocess
import sys
import tomllib

root = pathlib.Path(__file__).resolve().parents[1]
bundle = pathlib.Path(sys.argv[1]).resolve()
version = tomllib.loads((root / "Cargo.toml").read_text())["package"]["version"]
with (bundle / "Contents/Info.plist").open("rb") as stream:
    info = plistlib.load(stream)
assert info["CFBundleIdentifier"] == "dev.vardr.desktop"
assert info["CFBundleShortVersionString"] == version
helper = bundle / "Contents/MacOS/vardr"
assert subprocess.check_output([str(helper), "--version"], text=True).strip() == f"vardr {version}"
subprocess.run(["codesign", "--verify", "--deep", "--strict", str(bundle)], check=True)
print(f"Verified Vardr.app {version} with bundled CLI")
