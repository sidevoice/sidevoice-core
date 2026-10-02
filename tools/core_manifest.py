"""Build and validate the release manifest for relocatable core bundles."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
from pathlib import Path
from urllib.parse import quote, unquote, urlsplit

TARGETS = (
    ("macos-aarch64", "macos", "aarch64"),
    ("linux-x86_64", "linux", "x86_64"),
    ("linux-aarch64", "linux", "aarch64"),
)
WHEEL_RE = re.compile(r"sidevoice_core-(?P<version>[A-Za-z0-9.!+_-]+)-py3-none-any\.whl\Z")
REPOSITORY = "sidevoice/sidevoice-core"
RELEASE_BASE = f"https://github.com/{REPOSITORY}/releases/download"


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def build_manifest(assets: Path, release_tag: str) -> dict:
    """Return the binding two-part shape using bytes already built for a release."""
    wheels = sorted(assets.glob("sidevoice_core-*-py3-none-any.whl"))
    if len(wheels) != 1:
        raise ValueError(f"expected one sidevoice_core wheel in {assets}, found {len(wheels)}")
    match = WHEEL_RE.fullmatch(wheels[0].name)
    if not match:
        raise ValueError(f"unsupported wheel filename: {wheels[0].name}")
    version = match.group("version")
    base_url = f"{RELEASE_BASE}/{quote(release_tag, safe='')}"
    bundles = []
    for target, os_name, arch in TARGETS:
        name = f"sidevoice-core-{version}-{target}.tar.zst"
        path = assets / name
        if not path.is_file():
            raise ValueError(f"missing bundle for {target}: {path}")
        bundles.append({
            "os": os_name,
            "arch": arch,
            "url": f"{base_url}/{quote(name, safe='')}",
            "sha256": sha256(path),
            "size": path.stat().st_size,
        })

    wheel = wheels[0]
    return {
        "bundles": bundles,
        "wheel": {
            "url": f"{base_url}/{quote(wheel.name, safe='')}",
            "sha256": sha256(wheel),
        },
    }


def validate_manifest(manifest: dict, assets: Path | None = None) -> None:
    """Reject drift from the manifest contract and, when available, compare local bytes."""
    if not isinstance(manifest, dict) or set(manifest) != {"bundles", "wheel"}:
        raise ValueError("manifest must have exactly bundles and wheel")
    bundles = manifest["bundles"]
    if not isinstance(bundles, list) or len(bundles) != len(TARGETS):
        raise ValueError("manifest must contain exactly the three supported bundles")

    for entry, (target, os_name, arch) in zip(bundles, TARGETS, strict=True):
        if not isinstance(entry, dict) or set(entry) != {"os", "arch", "url", "sha256", "size"}:
            raise ValueError(f"bundle {target} has an unexpected shape")
        if (entry["os"], entry["arch"]) != (os_name, arch):
            raise ValueError(f"bundle order or platform mismatch for {target}")
        if not isinstance(entry["url"], str):
            raise ValueError(f"bundle {target} has a non-release URL")
        parsed = urlsplit(entry["url"])
        parts = [unquote(part) for part in parsed.path.split("/")]
        if (
            parsed.scheme != "https"
            or parsed.netloc != "github.com"
            or parsed.query
            or parsed.fragment
            or len(parts) != 7
            or parts[:5] != ["", "sidevoice", "sidevoice-core", "releases", "download"]
            or any(part in {".", ".."} or "/" in part for part in parts[1:])
            or not parts[5]
        ):
            raise ValueError(f"bundle {target} has a non-release URL")
        name = parts[6]
        if not name.endswith(f"-{target}.tar.zst"):
            raise ValueError(f"bundle {target} URL has the wrong asset name")
        if not isinstance(entry["sha256"], str) or not re.fullmatch(r"[0-9a-f]{64}", entry["sha256"]):
            raise ValueError(f"bundle {target} has an invalid SHA-256")
        if isinstance(entry["size"], bool) or not isinstance(entry["size"], int) or entry["size"] <= 0:
            raise ValueError(f"bundle {target} has an invalid size")
        if assets is not None:
            path = assets / name
            if path.stat().st_size != entry["size"] or sha256(path) != entry["sha256"]:
                raise ValueError(f"bundle {target} size or SHA-256 does not match its asset")

    wheel = manifest["wheel"]
    if not isinstance(wheel, dict) or set(wheel) != {"url", "sha256"}:
        raise ValueError("wheel must have exactly url and sha256")
    if not isinstance(wheel["url"], str):
        raise ValueError("wheel has an invalid release URL")
    parsed = urlsplit(wheel["url"])
    parts = [unquote(part) for part in parsed.path.split("/")]
    if (
        parsed.scheme != "https"
        or parsed.netloc != "github.com"
        or parsed.query
        or parsed.fragment
        or len(parts) != 7
        or parts[:5] != ["", "sidevoice", "sidevoice-core", "releases", "download"]
        or any(part in {".", ".."} or "/" in part for part in parts[1:])
        or not parts[5]
    ):
        raise ValueError("wheel has an invalid release URL")
    wheel_name = parts[6]
    if not WHEEL_RE.fullmatch(wheel_name):
        raise ValueError("wheel has an invalid release URL")
    if not isinstance(wheel["sha256"], str) or not re.fullmatch(r"[0-9a-f]{64}", wheel["sha256"]):
        raise ValueError("wheel has an invalid SHA-256")
    if assets is not None and sha256(assets / wheel_name) != wheel["sha256"]:
        raise ValueError("wheel SHA-256 does not match its asset")


def write_manifest(assets: Path, release_tag: str, output: Path) -> dict:
    manifest = build_manifest(assets, release_tag)
    validate_manifest(manifest, assets)
    output.write_text(json.dumps(manifest, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    return manifest


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--assets", type=Path, required=True)
    parser.add_argument("--tag", required=True, help="release tag, or a non-published PR test tag")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    manifest = write_manifest(args.assets, args.tag, args.output)
    print(json.dumps(manifest, separators=(",", ":")))


if __name__ == "__main__":
    main()
