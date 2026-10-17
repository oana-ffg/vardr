"""Create a drag-to-Applications installer without controlling Finder."""
import pathlib
import subprocess
import sys
import tempfile

bundle = pathlib.Path(sys.argv[1]).resolve()
destination = pathlib.Path(sys.argv[2]).resolve()
if destination.exists():
    raise SystemExit(f"Refusing to replace existing disk image: {destination}")
destination.parent.mkdir(parents=True, exist_ok=True)
with tempfile.TemporaryDirectory(prefix="vardr-installer-") as directory:
    staging = pathlib.Path(directory)
    subprocess.run(["ditto", str(bundle), str(staging / "Vardr.app")], check=True)
    (staging / "Applications").symlink_to("/Applications")
    subprocess.run(["hdiutil", "create", "-volname", "Vardr", "-srcfolder", str(staging), "-format", "UDZO", str(destination)], check=True)
print(destination)
