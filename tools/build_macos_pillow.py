"""Build the locked full-feature Pillow release from hash-pinned macOS sources."""

from __future__ import annotations

import argparse
import json
import os
import platform
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import urllib.parse
import urllib.request
import zipfile
from pathlib import Path, PurePosixPath

from tools.core_bundle import digest, safe_link_target, safe_member_path

ROOT = Path(__file__).resolve().parents[1]
PINS_PATH = ROOT / "packaging" / "pillow-macos-build.json"
FEATURE_SETTINGS = tuple(json.loads(PINS_PATH.read_text(encoding="utf-8"))["build_settings"]["pillow_config_settings"])
REQUIRED_CACHE_FILES = (
    "pkg-config-0.29.2.tar.gz",
    "xz-5.8.3.tar.gz",
    "2.3.3.tar.gz",
    "xcb-proto-1.17.0.tar.gz",
    "xorgproto-2025.1.tar.gz",
    "libXau-1.0.12.tar.gz",
    "libpthread-stubs-0.5.tar.gz",
    "libxcb-1.17.0.tar.gz",
    "libjpeg-turbo-3.1.4.1.tar.gz",
    "tiff-4.7.1.tar.gz",
    "libavif-1.4.2.tar.gz",
    "libpng-1.6.58.tar.gz",
    "lcms2-2.19.1.tar.gz",
    "v2.5.4.tar.gz",
    "libwebp-1.6.0.tar.gz",
    "brotli-1.2.0.tar.gz",
    "freetype-2.14.3.tar.gz",
    "harfbuzz-14.2.1.tar.xz",
)


def _load_pins() -> dict:
    pins = json.loads(PINS_PATH.read_text(encoding="utf-8"))
    for key in ("pillow_source", "multibuild", "dependency_cache"):
        item = pins[key]
        parsed = urllib.parse.urlsplit(item["url"])
        if parsed.scheme != "https" or parsed.netloc != "codeload.github.com":
            raise ValueError(f"Pillow {key} must use the pinned HTTPS source host")
        if not re.fullmatch(r"[0-9a-f]{40}", item["commit"]):
            raise ValueError(f"Pillow {key} has an invalid source commit")
        if item["commit"] not in parsed.path:
            raise ValueError(f"Pillow {key} URL does not name its pinned commit")
        if not re.fullmatch(r"[0-9a-f]{64}", item["sha256"]):
            raise ValueError(f"Pillow {key} has an invalid SHA-256")
        if item["size"] <= 0:
            raise ValueError(f"Pillow {key} has an invalid source size")
    settings = pins["build_settings"]
    if not settings["cmake_skip_rpath"]:
        raise ValueError("Pillow's native dependency build must disable CMake rpaths")
    if settings["rpath_linker_flag"] != "@loader_path":
        raise ValueError("Pillow's native library link rpath must use @loader_path")
    if settings["pillow_config_settings"] != list(FEATURE_SETTINGS):
        raise ValueError("Pillow build configuration changed; review its source-build lock")
    return pins


def _download(pin: dict, destination: Path) -> Path:
    request = urllib.request.Request(pin["url"], headers={"User-Agent": "sidevoice-core-pillow-builder"})
    partial = destination.with_suffix(destination.suffix + ".part")
    with urllib.request.urlopen(request, timeout=180) as response, partial.open("wb") as output:
        shutil.copyfileobj(response, output, length=1024 * 1024)
    if partial.stat().st_size != pin["size"] or digest(partial) != pin["sha256"]:
        partial.unlink(missing_ok=True)
        raise ValueError(f"Pillow pinned source size or SHA-256 mismatch: {destination.name}")
    partial.rename(destination)
    return destination


def _validate_archive_member(name: str, seen: set[str], expected_root: str) -> PurePosixPath:
    path = safe_member_path(name)
    if not path.parts or path.parts[0] != expected_root:
        raise ValueError(f"unexpected Pillow source archive member: {name!r}")
    if path.as_posix() in seen:
        raise ValueError(f"duplicate Pillow source archive member: {name!r}")
    seen.add(path.as_posix())
    return path


def _extract_tarball(archive_path: Path, destination: Path, expected_root: str) -> Path:
    destination.mkdir(parents=True, exist_ok=True)
    with tarfile.open(archive_path, mode="r:gz") as archive:
        members = archive.getmembers()
        seen: set[str] = set()
        for member in members:
            path = _validate_archive_member(member.name, seen, expected_root)
            if member.issym():
                safe_link_target(path, member.linkname, hardlink=False)
            elif member.islnk():
                safe_link_target(path, member.linkname, hardlink=True)
            elif not (member.isfile() or member.isdir()):
                raise ValueError(f"unsupported Pillow source archive member: {member.name}")
        archive.extractall(destination, members=members, filter="data")
    return destination / expected_root


def _extract_dependency_cache(archive_path: Path, destination: Path, expected_root: str) -> None:
    destination.mkdir(parents=True, exist_ok=True)
    seen: set[str] = set()
    with zipfile.ZipFile(archive_path) as archive:
        for item in archive.infolist():
            path = _validate_archive_member(item.filename, seen, expected_root)
            mode = item.external_attr >> 16
            if stat.S_ISLNK(mode):
                raise ValueError(f"Pillow dependency cache contains a symbolic link: {item.filename}")
            target = destination.joinpath(*path.parts[1:])
            if item.is_dir():
                target.mkdir(parents=True, exist_ok=True)
                continue
            target.parent.mkdir(parents=True, exist_ok=True)
            with archive.open(item) as source, target.open("wb") as output:
                shutil.copyfileobj(source, output, length=1024 * 1024)


def _offline_network_tools(directory: Path) -> Path:
    directory.mkdir(parents=True, exist_ok=True)
    for name in ("curl", "wget"):
        executable = directory / name
        executable.write_text(
            "#!/bin/sh\n"
            "echo 'unlocked network download attempted while building Pillow' >&2\n"
            "exit 97\n",
            encoding="utf-8",
        )
        executable.chmod(0o755)
    return directory


def _verify_dependency_cache(project: Path, cache: Path) -> None:
    dependencies = json.loads((project / ".github" / "dependencies.json").read_text(encoding="utf-8"))
    versions = {
        "pkg-config-0.29.2.tar.gz",
        f"xz-{dependencies['xz']}.tar.gz",
        f"{dependencies['zlib-ng']}.tar.gz",
        "xcb-proto-1.17.0.tar.gz",
        "xorgproto-2025.1.tar.gz",
        "libXau-1.0.12.tar.gz",
        "libpthread-stubs-0.5.tar.gz",
        f"libxcb-{dependencies['libxcb']}.tar.gz",
        f"libjpeg-turbo-{dependencies['jpegturbo']}.tar.gz",
        f"tiff-{dependencies['tiff']}.tar.gz",
        f"libavif-{dependencies['libavif']}.tar.gz",
        f"libpng-{dependencies['libpng']}.tar.gz",
        f"lcms2-{dependencies['lcms2']}.tar.gz",
        f"v{dependencies['openjpeg']}.tar.gz",
        f"libwebp-{dependencies['libwebp']}.tar.gz",
        f"brotli-{dependencies['brotli']}.tar.gz",
        f"freetype-{dependencies['freetype']}.tar.gz",
        f"harfbuzz-{dependencies['harfbuzz']}.tar.xz",
    }
    if versions != set(REQUIRED_CACHE_FILES):
        raise ValueError("Pillow release dependencies changed; review and update the native source lock")
    missing = sorted(name for name in versions if not (cache / name).is_file())
    if missing:
        raise ValueError(f"Pinned Pillow source cache is missing release inputs: {missing}")


def build(output_dir: Path, *, temp_dir: Path | None = None) -> dict:
    if sys.platform != "darwin":
        raise RuntimeError("the full-feature Pillow source build is only supported on macOS")
    if platform.machine().lower() not in {"arm64", "aarch64"}:
        raise RuntimeError("the full-feature Pillow source build requires an arm64 macOS runner")
    if sys.version_info[:2] != (3, 12):
        raise RuntimeError("the full-feature Pillow source build requires the locked CPython 3.12 toolchain")
    for executable in ("uv", "cmake", "meson", "ninja", "pip", "delocate-wheel"):
        if shutil.which(executable) is None:
            raise RuntimeError(f"locked Pillow build tool is missing: {executable}")

    pins = _load_pins()
    if output_dir.exists() and any(output_dir.iterdir()):
        raise ValueError(f"Pillow output directory is not empty: {output_dir}")
    output_dir.mkdir(parents=True, exist_ok=True)
    if temp_dir is not None:
        temp_dir.mkdir(parents=True, exist_ok=True)

    with tempfile.TemporaryDirectory(prefix="sidevoice-pillow-macos-", dir=temp_dir) as temporary:
        work = Path(temporary)
        pillow_archive = _download(pins["pillow_source"], work / "pillow-source.tar.gz")
        multibuild_archive = _download(pins["multibuild"], work / "multibuild-source.tar.gz")
        cache_archive = _download(pins["dependency_cache"], work / "pillow-dependencies.zip")

        pillow = _extract_tarball(
            pillow_archive,
            work / "source",
            f"Pillow-{pins['pillow_source']['commit']}",
        )
        multibuild = _extract_tarball(
            multibuild_archive,
            work / "submodule",
            f"multibuild-{pins['multibuild']['commit']}",
        )
        multibuild_target = pillow / "wheels" / "multibuild"
        if multibuild_target.exists():
            shutil.rmtree(multibuild_target)
        shutil.copytree(multibuild, multibuild_target, symlinks=True)

        cache = pillow / "build" / "darwin" / "pillow-depends-main"
        _extract_dependency_cache(
            cache_archive,
            cache,
            f"pillow-depends-{pins['dependency_cache']['commit']}",
        )
        _verify_dependency_cache(pillow, cache)

        prefix = pillow / "build" / "deps" / "darwin"
        offline_bin = _offline_network_tools(work / "offline-bin")
        builder_bin = Path(sys.executable).parent
        build_path = os.pathsep.join((str(builder_bin), os.environ["PATH"]))
        offline_proxy = "http://127.0.0.1:9"
        env = {
            **os.environ,
            "CIBW_ARCHS": "arm64",
            "CIBW_PLATFORM": "macos",
            "CFLAGS": f"-g0 -ffile-prefix-map={work}=.",
            "CXXFLAGS": f"-g0 -ffile-prefix-map={work}=.",
            "CPPFLAGS": f"-I{prefix / 'include'}",
            "CMAKE_PREFIX_PATH": str(prefix),
            "HOST_CMAKE_FLAGS": "-DCMAKE_SKIP_RPATH=ON" if pins["build_settings"]["cmake_skip_rpath"] else "",
            "LDFLAGS": (
                f"-L{prefix / 'lib'} -Wl,-rpath,{pins['build_settings']['rpath_linker_flag']}"
            ),
            "MACOSX_DEPLOYMENT_TARGET": pins["build_settings"]["macosx_deployment_target"],
            "PATH": os.pathsep.join((str(offline_bin), str(prefix / "bin"), build_path)),
            "HTTP_PROXY": offline_proxy,
            "HTTPS_PROXY": offline_proxy,
            "ALL_PROXY": offline_proxy,
            "http_proxy": offline_proxy,
            "https_proxy": offline_proxy,
            "all_proxy": offline_proxy,
            "NO_PROXY": "",
            "no_proxy": "",
            "PKG_CONFIG_PATH": os.pathsep.join((str(prefix / "lib" / "pkgconfig"), str(prefix / "share" / "pkgconfig"))),
            "PIP_NO_INDEX": "1",
            "PKG_CONFIG": str(prefix / "bin" / "pkg-config"),
            "PYTHONDONTWRITEBYTECODE": "1",
            "TMPDIR": str(work),
        }
        subprocess.run(
            ["bash", ".github/workflows/wheels-dependencies.sh"],
            cwd=pillow,
            env=env,
            check=True,
        )

        raw_wheels = work / "raw-wheels"
        raw_wheels.mkdir()
        build_command = [
            shutil.which("uv") or "uv",
            "build",
            str(pillow),
            "--wheel",
            "--out-dir",
            str(raw_wheels),
            "--python",
            sys.executable,
            "--no-build-isolation",
            "--no-config",
        ]
        for setting in FEATURE_SETTINGS:
            build_command.extend(("--config-setting", setting))
        subprocess.run(build_command, cwd=ROOT, env=env, check=True)

        raw_candidates = sorted(raw_wheels.glob(f"pillow-{pins['version']}-*.whl"))
        if len(raw_candidates) != 1:
            raise ValueError(f"expected one source-built Pillow wheel, found {len(raw_candidates)}")
        repaired_dir = work / "repaired-wheels"
        repaired_dir.mkdir()
        subprocess.run(
            [
                shutil.which("delocate-wheel") or "delocate-wheel",
                "--require-archs",
                "arm64",
                "--verbose",
                "-w",
                str(repaired_dir),
                str(raw_candidates[0]),
            ],
            cwd=ROOT,
            env=env,
            check=True,
        )
        repaired_candidates = sorted(repaired_dir.glob(f"pillow-{pins['version']}-*.whl"))
        if len(repaired_candidates) != 1:
            raise ValueError(f"expected one repaired Pillow wheel, found {len(repaired_candidates)}")
        if "macosx_11_0_arm64" not in repaired_candidates[0].name:
            raise ValueError(f"Pillow source wheel has an unexpected macOS deployment tag: {repaired_candidates[0].name}")
        output = output_dir / repaired_candidates[0].name
        shutil.copy2(repaired_candidates[0], output)

    return {
        "pillow_version": pins["version"],
        "pillow_source_commit": pins["pillow_source"]["commit"],
        "pillow_source_sha256": pins["pillow_source"]["sha256"],
        "multibuild_commit": pins["multibuild"]["commit"],
        "multibuild_sha256": pins["multibuild"]["sha256"],
        "dependency_cache_commit": pins["dependency_cache"]["commit"],
        "dependency_cache_sha256": pins["dependency_cache"]["sha256"],
        "wheel": output.name,
        "wheel_sha256": digest(output),
        "wheel_size": output.stat().st_size,
        "macosx_deployment_target": pins["build_settings"]["macosx_deployment_target"],
        "features": list(FEATURE_SETTINGS),
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--temp-dir", type=Path)
    args = parser.parse_args()
    print(json.dumps(build(args.output_dir, temp_dir=args.temp_dir), sort_keys=True, separators=(",", ":")))


if __name__ == "__main__":
    main()
