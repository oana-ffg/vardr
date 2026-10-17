"""Build/stage the matching CLI before Tauri packages the macOS application."""
import os
import sys
import json
import tomllib
import pathlib
import shutil
import subprocess

root = pathlib.Path(__file__).resolve().parents[1]
version = tomllib.loads((root / "Cargo.toml").read_text())["package"]["version"]
config = json.loads((root / "desktop/src-tauri/tauri.conf.json").read_text())
desktop = tomllib.loads((root / "desktop/src-tauri/Cargo.toml").read_text())
if config["version"] != version or desktop["package"]["version"] != version:
    raise SystemExit("Desktop and CLI versions must match")
if config["plugins"]["updater"]["pubkey"] != (root / "src/update-public-key.txt").read_text().strip():
    raise SystemExit("Desktop and CLI update trust keys must match")
target = os.environ.get("TAURI_ENV_TARGET_TRIPLE")
if not target:
    lines = subprocess.check_output(["rustc", "-vV"], text=True).splitlines()
    target = next(line.removeprefix("host: ") for line in lines if line.startswith("host: "))
if not target.endswith("apple-darwin"):
    raise SystemExit("The Vardr desktop app currently supports macOS only")
subprocess.run([sys.executable, str(root / "scripts/prepare-desktop-icons.py")], check=True)
subprocess.run(["cargo", "build", "--locked", "-p", "vardr", "--release", "--target", target], cwd=root, check=True)
output = pathlib.Path(os.environ.get("CARGO_TARGET_DIR", root / "target"))
if not output.is_absolute():
    output = root / output
destination = root / "desktop/src-tauri/binaries" / f"vardr-{target}"
destination.parent.mkdir(parents=True, exist_ok=True)
shutil.copy2(output / target / "release/vardr", destination)
print(f"Prepared Vardr CLI for {target}")
