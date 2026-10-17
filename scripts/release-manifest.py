"""Build the update manifest from already-signed standalone binaries."""
import base64
import json
import pathlib
import re
import sys

directory, version, repository = sys.argv[1:]
if not re.fullmatch(r"\d+\.\d+\.\d+", version) or not re.fullmatch(r"[\w.-]+/[\w.-]+", repository):
    raise SystemExit("Invalid release version or repository")
root = pathlib.Path(directory)
platforms = {}
for platform in ("darwin-aarch64", "darwin-x86_64", "linux-x86_64"):
    asset = root / f"vardr-{platform}"
    if not asset.is_file():
        raise SystemExit(f"Missing artifact: {asset.name}")
    signature = asset.with_name(asset.name + ".sig").read_text().strip()
    base64.b64decode(signature, validate=True)
    platforms[platform] = {
        "url": f"https://github.com/{repository}/releases/download/v{version}/{asset.name}",
        "signature": signature,
    }
(root / "latest.json").write_text(json.dumps({"version": version, "platforms": platforms}, indent=2) + "\n")

desktop_platforms = {}
for platform in ("darwin-aarch64", "darwin-x86_64"):
    asset = root / f"Vardr-{platform}.app.tar.gz"
    if not asset.is_file() or not (root / f"Vardr-{platform}.dmg").is_file():
        raise SystemExit(f"Missing desktop artifact for {platform}")
    signature = asset.with_name(asset.name + ".sig").read_text().strip()
    base64.b64decode(signature, validate=True)
    desktop_platforms[platform] = {
        "url": f"https://github.com/{repository}/releases/download/v{version}/{asset.name}",
        "signature": signature,
    }
(root / "desktop-latest.json").write_text(json.dumps({"version": version, "platforms": desktop_platforms}, indent=2) + "\n")
