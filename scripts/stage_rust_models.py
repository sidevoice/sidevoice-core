"""Stage the exact Rustvani model bytes into a build or runtime cache."""

import hashlib
import json
import sys
import tempfile
import urllib.request
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / "assets" / "rust-models.json"


def main(destination: Path) -> None:
    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    destination.mkdir(parents=True, exist_ok=True)
    revision = manifest["source_commit"]
    for model in manifest["models"]:
        target = destination / model["name"]
        expected = model["sha256"]
        if target.exists() and hashlib.sha256(target.read_bytes()).hexdigest() == expected:
            continue
        url = f"https://raw.githubusercontent.com/Allenmylath/rustvani/{revision}/{model['source_path']}"
        with urllib.request.urlopen(url, timeout=60) as response:
            payload = response.read()
        actual = hashlib.sha256(payload).hexdigest()
        if actual != expected:
            raise ValueError(f"{model['name']}: SHA-256 mismatch: {actual}")
        with tempfile.NamedTemporaryFile(dir=destination, delete=False) as staged:
            staged.write(payload)
            temporary = Path(staged.name)
        temporary.replace(target)
        print(f"staged {model['name']} {actual}")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("stage_rust_models.py DESTINATION")
    main(Path(sys.argv[1]))
