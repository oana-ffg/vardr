"""Rebuild macOS icon resources from the original generated owl artwork."""
import pathlib
import shutil
import subprocess
import tempfile

root = pathlib.Path(__file__).resolve().parents[1]
source = root / "desktop/artwork/vardr-icon-v1.png"
destination = root / "desktop/src-tauri/icons"
destination.mkdir(parents=True, exist_ok=True)
with tempfile.TemporaryDirectory(prefix="vardr-icons-") as directory:
    iconset = pathlib.Path(directory) / "Vardr.iconset"
    iconset.mkdir()
    for size in (16, 32, 128, 256, 512):
        for scale in (1, 2):
            pixels = size * scale
            suffix = "@2x" if scale == 2 else ""
            output = iconset / f"icon_{size}x{size}{suffix}.png"
            subprocess.run(["sips", "--resampleHeightWidth", str(pixels), str(pixels), str(source), "--out", str(output)], check=True, stdout=subprocess.DEVNULL)
    subprocess.run(["iconutil", "--convert", "icns", str(iconset), "--output", str(destination / "icon.icns")], check=True)
    shutil.copy2(iconset / "icon_128x128.png", destination / "icon.png")
print("Prepared Vardr owl icon resources")
