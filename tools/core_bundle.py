"""Build, inspect and exercise the self-contained core runtime archives."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import zipfile
from pathlib import Path, PurePosixPath

ROOT = Path(__file__).resolve().parents[1]
PINS = ROOT / "packaging" / "python-build-standalone.json"
REQUIRED = (
    "python/bin/python3",
    "python/lib/python3.12/LICENSE.txt",
    "LICENSE",
    "LICENSE.python-build-standalone",
    "TRADEMARKS.md",
    "DEPENDENCIES.lock.txt",
    "BUNDLE-NOTICES.md",
    "python-build-standalone-licenses/LICENSE.libffi.txt",
    "python/lib/python3.12/site-packages/sidevoice_core/server/__main__.py",
)


def digest(path: Path) -> str:
    checksum = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            checksum.update(chunk)
    return checksum.hexdigest()


def safe_member_path(name: str) -> PurePosixPath:
    path = PurePosixPath(name)
    if path.is_absolute() or ".." in path.parts or (path.parts and ":" in path.parts[0]):
        raise ValueError(f"unsafe archive member path: {name!r}")
    return path


def safe_link_target(member: PurePosixPath, target: str, *, hardlink: bool) -> None:
    path = PurePosixPath(target)
    if path.is_absolute() or (path.parts and ":" in path.parts[0]):
        raise ValueError(f"absolute archive link target: {target!r}")
    combined = path if hardlink else member.parent / path
    depth = 0
    for part in combined.parts:
        if part == "..":
            depth -= 1
            if depth < 0:
                raise ValueError(f"archive link escapes its root: {member} -> {target}")
        elif part not in ("", "."):
            depth += 1


def _copy_stream(source, destination: Path, mode: int) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    with destination.open("wb") as output:
        shutil.copyfileobj(source, output, length=1024 * 1024)
    destination.chmod(mode & 0o777)


def extract_python_distribution(source: Path, destination: Path) -> None:
    """Extract only the pinned standalone distribution, refusing unsafe paths and links."""
    destination.mkdir(parents=True, exist_ok=True)
    with tarfile.open(source, mode="r:gz") as archive:
        members = archive.getmembers()
        seen = set()
        for member in members:
            path = safe_member_path(member.name)
            if not path.parts or path.parts[0] != "python":
                raise ValueError(f"unexpected top-level path in Python distribution: {member.name}")
            if path.as_posix() in seen:
                raise ValueError(f"duplicate member in Python distribution: {member.name}")
            seen.add(path.as_posix())
            if member.issym():
                safe_link_target(path, member.linkname, hardlink=False)
            elif member.islnk():
                safe_link_target(path, member.linkname, hardlink=True)
            elif not (member.isfile() or member.isdir()):
                raise ValueError(f"unsupported Python distribution member: {member.name}")

        # Apply the standard data filter in archive order as well as the lexical checks above.
        # The filter resolves each new link against links already extracted, so chains cannot
        # escape even when every individual target appears lexically inside the root.
        archive.extractall(destination, members=members, filter="data")


def extract_wheel(wheel: Path, site_packages: Path) -> None:
    """Install this pure-Python project wheel without writing a build-path direct_url.json."""
    site_packages.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(wheel) as archive:
        for member in archive.infolist():
            path = safe_member_path(member.filename)
            if not path.parts or path.parts[0].endswith(".data"):
                raise ValueError(f"unexpected wheel install scheme in {member.filename}")
            if member.is_dir():
                site_packages.joinpath(*path.parts).mkdir(parents=True, exist_ok=True)
                continue
            mode = (member.external_attr >> 16) & 0o777 or 0o644
            with archive.open(member) as source:
                _copy_stream(source, site_packages.joinpath(*path.parts), mode)


def remove_build_path_entrypoints(binary_directory: Path, python: Path) -> int:
    """Drop dependency command wrappers whose shebangs pin the temporary build location."""
    interpreter = os.fsencode(python)
    removed = 0
    for path in binary_directory.iterdir():
        if not path.is_file() or path.is_symlink():
            continue
        with path.open("rb") as source:
            first_line = source.readline(4096)
        if first_line.startswith(b"#!") and interpreter in first_line:
            path.unlink()
            removed += 1
    return removed


def remove_upstream_sboms(site_packages: Path, build_paths: tuple[Path, ...] = ()) -> int:
    """Drop wheel SBOMs that embed this build's checkout or temporary paths."""
    forbidden = tuple(os.fsencode(path) for path in build_paths if str(path))
    removed = 0
    for directory in site_packages.glob("*.dist-info/sboms"):
        for path in directory.rglob("*"):
            if path.is_file():
                content = path.read_bytes().lower()
                if any(marker.lower() in content for marker in forbidden):
                    path.unlink()
                    removed += 1
        if not any(directory.iterdir()):
            shutil.rmtree(directory)
    return removed


def remove_install_source_metadata(site_packages: Path) -> int:
    """Drop installer-only origin records; the hash lock is the bundle's dependency provenance."""
    removed = 0
    for path in site_packages.glob("*.dist-info/direct_url.json"):
        path.unlink()
        removed += 1
    return removed


def _run(
    command: list[str], *, cwd: Path, env: dict[str, str] | None = None
) -> None:
    subprocess.run(command, cwd=cwd, env=env, check=True)


def _python_env() -> dict[str, str]:
    return {**os.environ, "PYTHONDONTWRITEBYTECODE": "1", "UV_COMPILE_BYTECODE": "0"}


def create_archive(bundle_root: Path, output: Path, *, epoch: int = 0) -> dict:
    """Write a deterministic tar.zst with normalized ownership and timestamps."""
    import zstandard

    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="sidevoice-core-tar-") as temporary:
        temporary_tar = Path(temporary) / "bundle.tar"
        unpacked_size = 0
        with tarfile.open(temporary_tar, mode="w", format=tarfile.PAX_FORMAT) as archive:
            for path in sorted(bundle_root.rglob("*"), key=lambda item: item.relative_to(bundle_root).as_posix()):
                name = path.relative_to(bundle_root).as_posix()
                info = archive.gettarinfo(str(path), arcname=name)
                info.uid = info.gid = 0
                info.uname = info.gname = ""
                info.mtime = epoch
                info.pax_headers = {}
                if info.isfile():
                    unpacked_size += info.size
                    with path.open("rb") as stream:
                        archive.addfile(info, stream)
                else:
                    archive.addfile(info)
        compressor = zstandard.ZstdCompressor(level=10, threads=0, write_checksum=True)
        with temporary_tar.open("rb") as source_stream, output.open("wb") as output_stream:
            compressor.copy_stream(source_stream, output_stream)
    return {"compressed_bytes": output.stat().st_size, "unpacked_bytes": unpacked_size}


def build_bundle(
    target: str, wheel: Path, output: Path, *, epoch: int = 0, temp_dir: Path | None = None
) -> dict:
    """Download pinned CPython, install the universal lock and core wheel, then create a stable tar.zst."""
    try:
        import zstandard
    except ImportError as error:
        raise RuntimeError("zstandard is required; run `uv sync --locked --only-group bundle-build`") from error

    pins = json.loads(PINS.read_text(encoding="utf-8"))
    try:
        pin = pins["targets"][target]
    except KeyError as error:
        raise ValueError(f"unsupported core bundle target: {target}") from error
    if not wheel.is_file():
        raise ValueError(f"core wheel not found: {wheel}")

    if temp_dir is not None:
        temp_dir.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="sidevoice-core-bundle-", dir=temp_dir) as temporary:
        work = Path(temporary)
        source = work / pin["asset"]
        partial = source.with_suffix(source.suffix + ".part")
        import urllib.request

        request = urllib.request.Request(pin["url"], headers={"User-Agent": "sidevoice-core-bundle-builder"})
        with urllib.request.urlopen(request, timeout=120) as response, partial.open("wb") as output_stream:
            shutil.copyfileobj(response, output_stream, length=1024 * 1024)
        if partial.stat().st_size != pin["size"] or digest(partial) != pin["sha256"]:
            raise ValueError(f"python-build-standalone digest or size mismatch for {target}")
        partial.rename(source)

        bundle_root = work / "bundle"
        extract_python_distribution(source, bundle_root)
        python = bundle_root / "python" / "bin" / "python3"
        if not python.is_file():
            raise ValueError(f"standalone distribution has no {python.relative_to(bundle_root)}")
        license_file = bundle_root / "python" / "lib" / "python3.12" / "LICENSE.txt"
        if not license_file.is_file():
            raise ValueError("python-build-standalone did not include CPython's license")
        standalone_license = ROOT / "packaging" / "LICENSE.python-build-standalone"
        if digest(standalone_license) != pins["project_license_sha256"]:
            raise ValueError("python-build-standalone project license does not match its reviewed copy")

        requirements = work / "DEPENDENCIES.lock.txt"
        export = subprocess.run([
            "uv", "export", "--locked", "--no-dev", "--no-default-groups", "--no-group", "bundle-build",
            "--no-extra", "test", "--no-emit-project", "--format", "requirements.txt",
        ], cwd=ROOT, check=True, capture_output=True, text=True)
        requirements.write_text(export.stdout, encoding="utf-8")
        _run([
            "uv", "pip", "install", "--quiet", "--link-mode=copy", "--python", str(python), "--require-hashes",
            "--no-python-downloads", "--no-managed-python", "-r", str(requirements),
        ], cwd=ROOT, env=_python_env())
        removed_entrypoints = remove_build_path_entrypoints(python.parent, python)

        purelib = subprocess.check_output(
            [str(python), "-I", "-B", "-c", "import sysconfig; print(sysconfig.get_path('purelib'))"],
            cwd=work,
            env=_python_env(),
            text=True,
        ).strip()
        extract_wheel(wheel, Path(purelib))
        removed_sboms = remove_upstream_sboms(Path(purelib), (ROOT, work))
        removed_direct_urls = remove_install_source_metadata(Path(purelib))
        shutil.copy2(ROOT / "LICENSE", bundle_root / "LICENSE")
        shutil.copy2(standalone_license, bundle_root / "LICENSE.python-build-standalone")
        shutil.copy2(ROOT / "TRADEMARKS.md", bundle_root / "TRADEMARKS.md")
        shutil.copy2(requirements, bundle_root / "DEPENDENCIES.lock.txt")
        license_inventory = pins["license_files"]
        upstream_licenses = ROOT / "packaging" / "python-build-standalone-licenses"
        bundle_licenses = bundle_root / "python-build-standalone-licenses"
        bundle_licenses.mkdir()
        for name, expected_sha256 in license_inventory.items():
            if Path(name).name != name or not name.startswith("LICENSE."):
                raise ValueError(f"unsafe python-build-standalone license filename: {name}")
            license_path = upstream_licenses / name
            if not license_path.is_file() or digest(license_path) != expected_sha256:
                raise ValueError(f"python-build-standalone license does not match its reviewed copy: {name}")
            shutil.copy2(license_path, bundle_licenses / name)
        notice = (
            "Sidevoice Core runtime bundle\n"
            "\n"
            f"CPython {pins['python_version']} is redistributed from python-build-standalone release "
            f"{pins['release']} ({pin['asset']}, source commit {pins['source_commit']}). "
            f"The upstream archive SHA-256 is {pin['sha256']}.\n"
            "The python-build-standalone project license is retained at LICENSE.python-build-standalone; "
            "the upstream CPython license is at python/lib/python3.12/LICENSE.txt. Vendor notices from the pinned "
            "source commit are retained under python-build-standalone-licenses/; some cover optional upstream "
            "features not shipped in this target.\n"
            "\n"
            "The Sidevoice Core Apache-2.0 license and trademark notice are at LICENSE and TRADEMARKS.md.\n"
            "Runtime dependencies were installed from DEPENDENCIES.lock.txt with SHA-256 hashes. Their wheel license "
            "files are retained under each distribution's .dist-info/licenses directory when supplied.\n"
            "\n"
            f"{removed_sboms} upstream wheel SBOM file(s) containing absolute build-checkout paths were omitted.\n"
            "\n"
            f"{removed_direct_urls} installer origin record(s) were omitted; DEPENDENCIES.lock.txt preserves the "
            "version and hash of each runtime dependency.\n"
            "\n"
            "The runtime is launched as python/bin/python3 -I -m sidevoice_core.server. Dependency command wrappers "
            f"with build-path shebangs ({removed_entrypoints}) are omitted.\n"
        )
        (bundle_root / "BUNDLE-NOTICES.md").write_text(notice, encoding="utf-8")
        env = {"PATH": str(work / "empty-path"), "HOME": str(work), "TMPDIR": str(work)}
        (work / "empty-path").mkdir()
        self_test = subprocess.run(
            [str(python), "-I", "-B", "-m", "sidevoice_core.server", "--self-test"],
            cwd=work,
            env=env,
            check=True,
            stdout=subprocess.PIPE,
            text=True,
        )
        try:
            self_test_result = json.loads(self_test.stdout)
        except json.JSONDecodeError as error:
            raise ValueError("core bundle self-test did not return valid JSON") from error
        if self_test_result.get("ok") is not True:
            raise ValueError("core bundle self-test did not report success")

        stats = create_archive(bundle_root, output, epoch=epoch)

    return {
        **stats,
        "sha256": digest(output),
    }


def _check_build_paths(data: bytes, forbidden: tuple[bytes, ...], *, member: str = "bundle content") -> None:
    lowered = data.lower()
    for path in forbidden:
        if path and path.lower() in lowered:
            raise ValueError(f"{member} contains build path {path.decode(errors='replace')}")


MACHO_MAGICS = {
    b"\xce\xfa\xed\xfe", b"\xfe\xed\xfa\xce",
    b"\xcf\xfa\xed\xfe", b"\xfe\xed\xfa\xcf",
    b"\xca\xfe\xba\xbe", b"\xbe\xba\xfe\xca",
    b"\xca\xfe\xba\xbf", b"\xbf\xba\xfe\xca",
}
MACOS_SYSTEM_PATH_PREFIXES = (
    "/System/Library/",
    "/System/Volumes/Preboot/Cryptexes/OS/System/Library/",
    "/System/Volumes/Preboot/Cryptexes/OS/usr/lib/",
    "/Library/Apple/System/Library/",
    "/usr/lib/",
)
MACOS_NATIVE_IMPORT_PROBE = "\n".join((
    "import aiohttp._http_parser",
    "import aiortc",
    "import av",
    "import cryptography.hazmat.bindings._rust",
    "import llvmlite.binding",
    "import numpy",
    "import onnxruntime",
    "import PIL.Image",
    "import scipy.signal",
    "import soundfile",
    "import soxr",
    "import yaml._yaml",
))
MACHO_DEPENDENCY_RE = re.compile(r"^\s*(.+?) \(compatibility version [^)]*\)")
MACHO_RPATH_RE = re.compile(r"^\s*path (.+?) \(offset [0-9]+\)$")


def _parse_otool_dependencies(output: str) -> tuple[str, ...]:
    """Read dependency install names from `otool -L` output."""
    return tuple(
        match.group(1)
        for line in output.splitlines()
        if (match := MACHO_DEPENDENCY_RE.match(line)) is not None
    )


def _parse_otool_rpaths(output: str) -> tuple[str, ...]:
    """Read LC_RPATH entries from `otool -l` output."""
    return tuple(
        match.group(1)
        for line in output.splitlines()
        if (match := MACHO_RPATH_RE.match(line)) is not None
    )


def _is_within(path: Path, root: Path) -> bool:
    try:
        path.resolve(strict=False).relative_to(root.resolve(strict=False))
    except ValueError:
        return False
    return True


def _validate_macho_path(value: str, *, binary: Path, root: Path, executable_directory: Path) -> None:
    """Require every Mach-O load name to resolve within the bundle or an OS library directory."""
    if value.startswith("/"):
        absolute_parts = PurePosixPath(value).parts
        if any(part in {".", ".."} for part in absolute_parts):
            raise ValueError(f"unsafe absolute Mach-O load path: {value} in {binary}")
        if value.startswith(MACOS_SYSTEM_PATH_PREFIXES):
            return
        raise ValueError(f"non-system absolute Mach-O load path: {value} in {binary}")
    for token, base in (("@loader_path", binary.parent), ("@executable_path", executable_directory)):
        if value == token or value.startswith(token + "/"):
            suffix = value[len(token):].lstrip("/")
            candidate = base.joinpath(*PurePosixPath(suffix).parts)
            if not _is_within(candidate, root):
                raise ValueError(f"Mach-O load path escapes the relocated bundle: {value} in {binary}")
            return
    if value == "@rpath" or value.startswith("@rpath/"):
        suffix = value[len("@rpath"):].lstrip("/")
        if not suffix or ".." in PurePosixPath(suffix).parts:
            raise ValueError(f"unsafe Mach-O @rpath load name: {value} in {binary}")
        return
    raise ValueError(f"unsupported Mach-O load path: {value} in {binary}")


def _verify_macos_native_runtime(root: Path, env: dict[str, str]) -> dict:
    """Import key native dependencies and check every Mach-O load name after relocation."""
    machine = platform.machine().lower()
    if machine not in {"arm64", "aarch64"}:
        raise ValueError(f"macOS bundle verification requires an arm64 runner, found {machine}")
    interpreter = root / "python" / "bin" / "python3"
    probe = subprocess.run(
        [str(interpreter), "-I", "-B", "-c", MACOS_NATIVE_IMPORT_PROBE],
        cwd=root,
        env=env,
        check=False,
        capture_output=True,
        text=True,
    )
    if probe.returncode:
        raise ValueError(f"relocated macOS native import probe failed: {probe.stderr.strip()}")

    otool = shutil.which("otool")
    if otool is None:
        raise ValueError("otool is required to verify macOS Mach-O load paths")
    executable_directory = interpreter.parent
    inspected = 0
    for binary in sorted(path for path in root.rglob("*") if path.is_file()):
        try:
            with binary.open("rb") as stream:
                is_macho = stream.read(4) in MACHO_MAGICS
        except OSError:
            continue
        if not is_macho:
            continue
        dependencies = subprocess.run([otool, "-L", str(binary)], check=True, capture_output=True, text=True)
        load_commands = subprocess.run([otool, "-l", str(binary)], check=True, capture_output=True, text=True)
        paths = (*_parse_otool_dependencies(dependencies.stdout), *_parse_otool_rpaths(load_commands.stdout))
        for value in paths:
            _validate_macho_path(value, binary=binary, root=root, executable_directory=executable_directory)
        inspected += 1
    if inspected == 0:
        raise ValueError("no Mach-O files found in the relocated macOS core bundle")
    return {"native_imports": len(MACOS_NATIVE_IMPORT_PROBE.splitlines()), "macho_files": inspected}


def inspect_archive(archive_path: Path, forbidden_paths: tuple[Path, ...] = ()) -> dict:
    """Check entry paths, links, licenses, package layout and embedded build-path leakage."""
    import zstandard

    forbidden = tuple(os.fsencode(path) for path in forbidden_paths if str(path))
    names = set()
    unpacked_size = 0
    with archive_path.open("rb") as compressed:
        with zstandard.ZstdDecompressor().stream_reader(compressed) as tar_stream:
            with tarfile.open(fileobj=tar_stream, mode="r|") as archive:
                for member in archive:
                    path = safe_member_path(member.name)
                    if not path.parts:
                        raise ValueError("empty archive member path")
                    if path.as_posix() in names:
                        raise ValueError(f"duplicate archive member: {member.name}")
                    names.add(path.as_posix())
                    if member.issym():
                        safe_link_target(path, member.linkname, hardlink=False)
                    elif member.islnk():
                        safe_link_target(path, member.linkname, hardlink=True)
                    elif member.isfile():
                        unpacked_size += member.size
                        source = archive.extractfile(member)
                        if source is None:
                            raise ValueError(f"could not read archive member: {member.name}")
                        tail = b""
                        while chunk := source.read(1024 * 1024):
                            combined = tail + chunk
                            _check_build_paths(combined, forbidden, member=member.name)
                            tail = combined[-max((len(item) for item in forbidden), default=1):]
                    elif not member.isdir():
                        raise ValueError(f"unsupported archive member type: {member.name}")

    missing = set(REQUIRED) - names
    if missing:
        raise ValueError(f"bundle is missing required paths: {sorted(missing)}")
    if "direct_url.json" in names or any(name.endswith(".dist-info/direct_url.json") for name in names):
        raise ValueError("bundle contains a local-install direct_url.json")
    return {"compressed_bytes": archive_path.stat().st_size, "unpacked_bytes": unpacked_size, "files": len(names)}


def _extract_bundle(archive_path: Path, destination: Path) -> None:
    import zstandard

    destination.mkdir(parents=True)
    with archive_path.open("rb") as compressed:
        with zstandard.ZstdDecompressor().stream_reader(compressed) as tar_stream:
            with tarfile.open(fileobj=tar_stream, mode="r|") as archive:
                archive.extractall(destination, filter="data")


def verify_relocation(archive_path: Path, *, build_paths: tuple[Path, ...] = ()) -> dict:
    """Unpack outside the checkout, self-test with an empty PATH, then move and test again."""
    info = inspect_archive(archive_path, build_paths)
    with tempfile.TemporaryDirectory(prefix="sidevoice-core-relocation-") as temporary:
        root = Path(temporary)
        if root == ROOT or ROOT in root.parents:
            raise ValueError("relocation test must unpack outside the source checkout")
        first, second = root / "first-location", root / "second-location"
        _extract_bundle(archive_path, first)
        pathless = root / "empty-path"
        pathless.mkdir()
        env = {"PATH": str(pathless), "HOME": str(root), "TMPDIR": str(root)}
        command = [str(first / "python" / "bin" / "python3"), "-I", "-B", "-m", "sidevoice_core.server", "--self-test"]
        for location in (first,):
            result = subprocess.run(command, cwd=location, env=env, check=True, capture_output=True, text=True)
            if '"ok": true' not in result.stdout and '"ok":true' not in result.stdout:
                raise ValueError(f"bundle self-test did not report success: {result.stdout.strip()}")
        shutil.move(first, second)
        relocated_command = [str(second / "python" / "bin" / "python3"), *command[1:]]
        result = subprocess.run(relocated_command, cwd=second, env=env, check=True, capture_output=True, text=True)
        if '"ok": true' not in result.stdout and '"ok":true' not in result.stdout:
            raise ValueError(f"relocated bundle self-test did not report success: {result.stdout.strip()}")
        native_check = _verify_macos_native_runtime(second, env) if sys.platform == "darwin" else {}
    return {**info, **native_check}


def main() -> None:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)
    build = subparsers.add_parser("build")
    build.add_argument("--target", required=True)
    build.add_argument("--wheel", type=Path, required=True)
    build.add_argument("--output", type=Path, required=True)
    build.add_argument("--epoch", type=int, default=0)
    build.add_argument("--temp-dir", type=Path)
    verify = subparsers.add_parser("verify")
    verify.add_argument("--archive", type=Path, required=True)
    verify.add_argument("--build-path", type=Path, action="append", default=[])
    args = parser.parse_args()

    if args.command == "build":
        result = build_bundle(args.target, args.wheel, args.output, epoch=args.epoch, temp_dir=args.temp_dir)
    else:
        result = verify_relocation(args.archive, build_paths=tuple(args.build_path))
    print(json.dumps(result, sort_keys=True, separators=(",", ":")))


if __name__ == "__main__":
    main()
