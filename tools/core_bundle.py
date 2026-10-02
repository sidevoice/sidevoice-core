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
from contextlib import contextmanager
from pathlib import Path, PurePosixPath

ROOT = Path(__file__).resolve().parents[1]
PINS = ROOT / "packaging" / "python-build-standalone.json"
PILLOW_CODECS = ("JPEG", "JPEG2000", "PNG", "TIFF", "WEBP", "AVIF")
PILLOW_REQUIRED_FEATURES = (
    "jpg",
    "jpg_2000",
    "zlib",
    "libtiff",
    "freetype2",
    "littlecms2",
    "webp",
    "avif",
)
# Issue #35 permits only this exact upstream LC_RPATH on the locked Pillow arm64 wheel.
MACOS_PILLOW_WHEEL_SHA256 = "ffd0c5368496f41b0944be820fcb7a838aa6e623d250b01acf2643939c3f99d7"
MACOS_PILLOW_LIBJPEG_PATH = "python/lib/python3.12/site-packages/PIL/.dylibs/libjpeg.62.4.0.dylib"
MACOS_PILLOW_LIBJPEG_SHA256 = "cf7c4e5c2d2c007fc51afcb95b649415cfe0bc4d7137ace897a6eff6550fa967"
MACOS_PILLOW_UPSTREAM_RPATH = "/Users/runner/work/Pillow/Pillow/build/deps/darwin/lib"
MACOS_PILLOW_LIBJPEG_DEPENDENCIES = ("/usr/lib/libSystem.B.dylib",)
# Issue #36 permits this one upstream PyAV runpath only after a relocated runtime canary check.
MACOS_PYAV_WHEEL_SHA256 = "43ebbe977f19a7f2d2bd1a4e119675a0b15e05852cf7309846b6ab922ba7ffe9"
MACOS_PYAV_LIBSHARPYUV_PATH = "python/lib/python3.12/site-packages/av/.dylibs/libsharpyuv.0.1.2.dylib"
MACOS_PYAV_LIBSHARPYUV_SHA256 = "6ca31c87640662e5f6aef0460f3b824e192d01c5a19cb9ad10d458d8709719e4"
MACOS_PYAV_LIBSHARPYUV_ID = "/DLC/av/.dylibs/libsharpyuv.0.1.2.dylib"
MACOS_PYAV_UPSTREAM_RPATH = "/tmp/vendor/lib"
MACOS_PYAV_LIBSHARPYUV_DEPENDENCIES = ("/usr/lib/libSystem.B.dylib",)
MACOS_PYAV_LIBSHARPYUV_EMBEDDED_RPATH_NAMES = ()
MACOS_PYAV_VERSION = "17.1.0"
MACOS_PYAV_CANARY_NAMES = (
    "libsharpyuv.0.1.2.dylib",
    "libSystem.B.dylib",
    "libsidevoice_rpath_canary.dylib",
)
MACOS_TEMP_VENDOR_PATHS = ("/tmp/vendor/lib/", "/private/tmp/vendor/lib/")
MACOS_DYNAMIC_LOADER_SYMBOL_PREFIXES = (
    "_dlopen",
    "_dlsym",
    "_dladdr",
    "_NSCreateObjectFileImageFromFile",
    "_NSLinkModule",
    "_NSAddImage",
    "_CFBundleLoadExecutable",
    "_CFBundleLoadExecutableAndReturnError",
    "_OBJC_CLASS_$_NSBundle",
)
REQUIRED = (
    "python/bin/python3",
    "python/lib/python3.12/LICENSE.txt",
    "LICENSE",
    "LICENSE.python-build-standalone",
    "TRADEMARKS.md",
    "DEPENDENCIES.lock.txt",
    "BUNDLE-NOTICES.md",
    "python/lib/python3.12/site-packages/PIL/Image.py",
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
        pillow_notice = (
            "The macOS arm64 bundle uses hash-locked Pillow 12.3.0 wheel "
            f"SHA-256 {MACOS_PILLOW_WHEEL_SHA256}. Its {MACOS_PILLOW_LIBJPEG_PATH} binary has SHA-256 "
            f"{MACOS_PILLOW_LIBJPEG_SHA256} and carries one upstream LC_RPATH, "
            f"{MACOS_PILLOW_UPSTREAM_RPATH}. This command is allowlisted only for that exact binary digest and "
            "its inspected load graph, which contains only /usr/lib/libSystem.B.dylib and no @rpath dependency. "
            "Relocation verification also runs native imports and full Pillow codec read/write round trips. "
            "All other non-system absolute Mach-O paths remain rejected.\n\n"
            if target == "macos-aarch64"
            else ""
        )
        pyav_notice = (
            "The macOS arm64 bundle uses hash-locked PyAV 17.1.0 wheel "
            f"SHA-256 {MACOS_PYAV_WHEEL_SHA256}. Its {MACOS_PYAV_LIBSHARPYUV_PATH} binary has SHA-256 "
            f"{MACOS_PYAV_LIBSHARPYUV_SHA256}, LC_ID_DYLIB {MACOS_PYAV_LIBSHARPYUV_ID}, one LC_RPATH "
            f"{MACOS_PYAV_UPSTREAM_RPATH}, and only /usr/lib/libSystem.B.dylib as an LC_LOAD_DYLIB. "
            "The inspected binary imports no dynamic-loader API, has no initializer section, no @rpath dependency, "
            "and no embedded @rpath string. Its only load edge is libSystem. Its exact runpath "
            "is allowlisted only for that binary digest and load graph. Relocation verification populates "
            "/tmp/vendor/lib with harmless canary dylibs for every @rpath dependency name in the bundle graph, "
            "first proves dyld image capture with a positive control, then requires native imports, PyAV audio "
            "and WebP/sharpyuv round trips, Pillow codec round trips, and a relocated core start/health/clean-stop "
            "check to succeed without loading a canary from either /tmp or /private/tmp. All other "
            "unexpected absolute Mach-O paths remain rejected.\n\n"
            if target == "macos-aarch64"
            else ""
        )
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
            + pillow_notice
            + pyav_notice
            + f"{removed_sboms} upstream wheel SBOM file(s) containing absolute build-checkout paths were omitted.\n"
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
MACOS_SYSTEM_PATH_DIRECTORIES = (
    "/System/Library",
    "/System/Volumes/Preboot/Cryptexes/OS/System/Library",
    "/System/Volumes/Preboot/Cryptexes/OS/usr/lib",
    "/Library/Apple/System/Library",
    "/usr/lib",
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
    "import ctypes, json",
    "_dyld = ctypes.CDLL(None)",
    "_dyld._dyld_image_count.restype = ctypes.c_uint32",
    "_dyld._dyld_get_image_name.argtypes = [ctypes.c_uint32]",
    "_dyld._dyld_get_image_name.restype = ctypes.c_char_p",
    "_images = [_dyld._dyld_get_image_name(i).decode() for i in range(_dyld._dyld_image_count())]",
    "_vendor_images = [path for path in _images if '/tmp/vendor/lib/' in path or '/private/tmp/vendor/lib/' in path]",
    "print(json.dumps({'native_imports': 12, 'vendor_images': _vendor_images}))",
))
PIL_CODEC_ROUNDTRIP_PROBE = "\n".join((
    "import json",
    "from io import BytesIO",
    "from PIL import Image, features",
    "Image.init()",
    f"required_codecs = {PILLOW_CODECS!r}",
    f"required_features = {PILLOW_REQUIRED_FEATURES!r}",
    "missing_features = [feature for feature in required_features if not features.check(feature)]",
    "if missing_features:",
    "    raise RuntimeError(f'Pillow native features are unavailable: {missing_features}')",
    "missing_codecs = [codec for codec in required_codecs if codec not in Image.OPEN or codec not in Image.SAVE]",
    "if missing_codecs:",
    "    raise RuntimeError(f'Pillow codecs cannot both read and write: {missing_codecs}')",
    "source = Image.new('RGB', (3, 2), (31, 97, 173))",
    "for codec in required_codecs:",
    "    buffer = BytesIO()",
    "    options = {'compression': 'tiff_lzw'} if codec == 'TIFF' else {}",
    "    source.save(buffer, format=codec, **options)",
    "    buffer.seek(0)",
    "    with Image.open(buffer) as decoded:",
    "        decoded.load()",
    "        if decoded.size != source.size:",
    "            raise RuntimeError(f'{codec} round trip changed the image size')",
    "        if codec == 'TIFF' and decoded.tag_v2.get(259) != 5:",
    "            raise RuntimeError('TIFF round trip did not use LZW compression through libtiff')",
    "import sys",
    "if sys.platform == 'darwin':",
    "    import ctypes",
    "    _dyld = ctypes.CDLL(None)",
    "    _dyld._dyld_image_count.restype = ctypes.c_uint32",
    "    _dyld._dyld_get_image_name.argtypes = [ctypes.c_uint32]",
    "    _dyld._dyld_get_image_name.restype = ctypes.c_char_p",
    "    _images = [_dyld._dyld_get_image_name(i).decode() for i in range(_dyld._dyld_image_count())]",
    "    _vendor_images = [path for path in _images if '/tmp/vendor/lib/' in path or '/private/tmp/vendor/lib/' in path]",
    "else:",
    "    _vendor_images = []",
    "print(json.dumps({'codecs': list(required_codecs), 'features': list(required_features),",
    "                  'tiff_compression': 'tiff_lzw', 'vendor_images': _vendor_images}))",
))
PYAV_AUDIO_ROUNDTRIP_PROBE = "\n".join((
    "import io, json, av",
    "from av import AudioFrame, AudioResampler",
    f"expected_version = {MACOS_PYAV_VERSION!r}",
    "if av.__version__ != expected_version:",
    "    raise RuntimeError(f'PyAV version changed: {av.__version__!r}')",
    "encoded = io.BytesIO()",
    "with av.open(encoded, mode='w', format='wav') as container:",
    "    stream = container.add_stream('pcm_s16le', rate=16000)",
    "    frame = AudioFrame(format='s16', layout='mono', samples=320)",
    "    frame.sample_rate = 16000",
    "    for plane in frame.planes:",
    "        plane.update(bytes(plane.buffer_size))",
    "    for packet in stream.encode(frame):",
    "        container.mux(packet)",
    "    for packet in stream.encode(None):",
    "        container.mux(packet)",
    "with av.open(io.BytesIO(encoded.getvalue()), mode='r', format='wav') as container:",
    "    decoded = list(container.decode(audio=0))",
    "decoded_samples = sum(frame.samples for frame in decoded)",
    "if decoded_samples != 320:",
    "    raise RuntimeError(f'PyAV WAV decode returned {decoded_samples} samples')",
    "resampler = AudioResampler(format='s16', layout='mono', rate=8000)",
    "resampled = [out for frame in decoded for out in resampler.resample(frame)]",
    "resampled.extend(resampler.resample(None))",
    "resampled_samples = sum(frame.samples for frame in resampled)",
    "if not resampled or any(frame.sample_rate != 8000 for frame in resampled):",
    "    raise RuntimeError('PyAV AudioResampler did not produce 8 kHz audio')",
    "import sys",
    "if sys.platform == 'darwin':",
    "    import ctypes",
    "    _dyld = ctypes.CDLL(None)",
    "    _dyld._dyld_image_count.restype = ctypes.c_uint32",
    "    _dyld._dyld_get_image_name.argtypes = [ctypes.c_uint32]",
    "    _dyld._dyld_get_image_name.restype = ctypes.c_char_p",
    "    _images = [_dyld._dyld_get_image_name(i).decode() for i in range(_dyld._dyld_image_count())]",
    "    _vendor_images = [path for path in _images if '/tmp/vendor/lib/' in path or '/private/tmp/vendor/lib/' in path]",
    "else:",
    "    _vendor_images = []",
    "print(json.dumps({'version': av.__version__, 'container': 'wav', 'codec': 'pcm_s16le',",
    "                  'decoded_samples': decoded_samples, 'resampled_rate': 8000,",
    "                  'resampled_samples': resampled_samples, 'vendor_images': _vendor_images}))",
))
PYAV_WEBP_ROUNDTRIP_PROBE = "\n".join((
    "import json, av",
    "from io import BytesIO",
    "from av import VideoFrame",
    f"expected_version = {MACOS_PYAV_VERSION!r}",
    "if av.__version__ != expected_version:",
    "    raise RuntimeError(f'PyAV version changed: {av.__version__!r}')",
    "if 'libwebp' not in av.codecs_available or 'webp' not in av.formats_available:",
    "    raise RuntimeError('PyAV WebP encoder or container is unavailable')",
    "encoded = BytesIO()",
    "with av.open(encoded, mode='w', format='webp') as container:",
    "    stream = container.add_stream('libwebp', rate=1)",
    "    stream.width = 16",
    "    stream.height = 16",
    "    stream.pix_fmt = 'yuv420p'",
    "    frame = VideoFrame(16, 16, format='yuv420p')",
    "    for packet in stream.encode(frame):",
    "        container.mux(packet)",
    "    for packet in stream.encode(None):",
    "        container.mux(packet)",
    "webp_bytes = encoded.getvalue()",
    "if not webp_bytes.startswith(b'RIFF') or webp_bytes[8:12] != b'WEBP':",
    "    raise RuntimeError('PyAV did not encode a WebP image')",
    "with av.open(BytesIO(webp_bytes), mode='r', format='webp') as container:",
    "    frames = list(container.decode(video=0))",
    "if len(frames) != 1 or (frames[0].width, frames[0].height) != (16, 16):",
    "    raise RuntimeError(f'PyAV WebP decode returned unexpected frames: {len(frames)}')",
    "import sys",
    "if sys.platform == 'darwin':",
    "    import ctypes",
    "    _dyld = ctypes.CDLL(None)",
    "    _dyld._dyld_image_count.restype = ctypes.c_uint32",
    "    _dyld._dyld_get_image_name.argtypes = [ctypes.c_uint32]",
    "    _dyld._dyld_get_image_name.restype = ctypes.c_char_p",
    "    _images = [_dyld._dyld_get_image_name(i).decode() for i in range(_dyld._dyld_image_count())]",
    "    _vendor_images = [path for path in _images if '/tmp/vendor/lib/' in path or '/private/tmp/vendor/lib/' in path]",
    "    _sharpyuv_images = [path for path in _images if path.endswith('/av/.dylibs/libsharpyuv.0.1.2.dylib')]",
    "    if not _sharpyuv_images:",
    "        raise RuntimeError('PyAV WebP encode/decode did not load its bundled libsharpyuv dylib')",
    "else:",
    "    _vendor_images = []",
    "    _sharpyuv_images = []",
    "if sys.platform == 'darwin' and not _sharpyuv_images:",
    "    raise RuntimeError('PyAV WebP encode/decode did not load its bundled libsharpyuv dylib')",
    "print(json.dumps({'version': av.__version__, 'format': 'webp', 'codec': 'libwebp',",
    "                  'encoded_bytes': len(webp_bytes), 'decoded_frames': len(frames),",
    "                  'sharpyuv_images': _sharpyuv_images, 'vendor_images': _vendor_images}))",
))
MACHO_LOAD_COMMAND_RE = re.compile(r"^\s*cmd (LC_[A-Z0-9_]+)$")
MACHO_DEPENDENCY_COMMANDS = {
    "LC_LOAD_DYLIB",
    "LC_LOAD_WEAK_DYLIB",
    "LC_REEXPORT_DYLIB",
    "LC_LOAD_UPWARD_DYLIB",
    "LC_LAZY_LOAD_DYLIB",
}
MACHO_NAME_RE = re.compile(r"^\s*name (.+?) \(offset [0-9]+\)$")
MACHO_RPATH_RE = re.compile(r"^\s*path (.+?) \(offset [0-9]+\)$")


def _parse_otool_dependencies(output: str) -> tuple[str, ...]:
    """Read actual LC_LOAD_* dependency names from `otool -l`, excluding LC_ID_DYLIB."""
    dependencies = []
    command = None
    for line in output.splitlines():
        if match := MACHO_LOAD_COMMAND_RE.match(line):
            command = match.group(1)
        elif command in MACHO_DEPENDENCY_COMMANDS and (match := MACHO_NAME_RE.match(line)) is not None:
            dependencies.append(match.group(1))
    return tuple(dependencies)


def _parse_otool_rpaths(output: str) -> tuple[str, ...]:
    """Read LC_RPATH entries from `otool -l` output."""
    return tuple(
        match.group(1)
        for line in output.splitlines()
        if (match := MACHO_RPATH_RE.match(line)) is not None
    )


def _parse_otool_install_names(output: str) -> tuple[str, ...]:
    """Read LC_ID_DYLIB install names without confusing them with dependency edges."""
    names = []
    command = None
    for line in output.splitlines():
        if match := MACHO_LOAD_COMMAND_RE.match(line):
            command = match.group(1)
        elif command == "LC_ID_DYLIB" and (match := MACHO_NAME_RE.match(line)) is not None:
            names.append(match.group(1))
    return tuple(names)


def _parse_nm_undefined_symbols(output: str) -> tuple[str, ...]:
    """Read undefined symbol names from Apple's `nm -u` output."""
    symbols = []
    for line in output.splitlines():
        fields = line.split()
        if len(fields) >= 2 and fields[0] == "U":
            symbols.append(fields[1])
        elif len(fields) >= 3 and fields[0] == "(undefined)" and fields[1] == "external":
            symbols.append(fields[2])
    return tuple(symbols)


def _has_macho_initializers(output: str) -> bool:
    """Detect Mach-O initializer routines and constructor sections in `otool -l` output."""
    return bool(re.search(r"\bLC_ROUTINES(?:_64)?\b|\bsectname __mod_init_func\b", output))


def _rpath_candidate_name(value: str) -> str | None:
    """Return a safe relative name that an @rpath lookup could request."""
    if not value.startswith("@rpath/"):
        return None
    candidate = value[len("@rpath/"):]
    if not re.fullmatch(r"[A-Za-z0-9_+@.-]+(?:/[A-Za-z0-9_+@.-]+)*", candidate):
        raise ValueError(f"unsafe @rpath candidate: {value}")
    relative = PurePosixPath(candidate)
    if not relative.parts or relative.is_absolute() or any(part in {"", ".", ".."} for part in relative.parts):
        raise ValueError(f"unsafe @rpath candidate: {value}")
    return relative.as_posix()


def _embedded_rpath_candidate_names(binary: Path) -> tuple[str, ...]:
    """Find literal @rpath names in one Mach-O image, including names used only with dlopen."""
    data = binary.read_bytes()
    candidates = set()
    for match in re.finditer(rb"@rpath/([^\x00\s\"'<>]{1,512})", data):
        value = "@rpath/" + match.group(1).decode("ascii", errors="ignore")
        candidate = _rpath_candidate_name(value)
        if candidate is not None:
            candidates.add(candidate)
    return tuple(sorted(candidates))


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
        if value in MACOS_SYSTEM_PATH_DIRECTORIES or value.startswith(MACOS_SYSTEM_PATH_PREFIXES):
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


def _validate_pillow_rpath_exception(
    value: str,
    *,
    binary: Path,
    root: Path,
    dependencies: tuple[str, ...],
) -> dict:
    """Allow the reviewed LC_RPATH only on the exact locked Pillow libjpeg binary."""
    try:
        relative_path = binary.relative_to(root).as_posix()
    except ValueError as error:
        raise ValueError(f"Pillow LC_RPATH exception binary is outside the relocated bundle: {binary}") from error
    if relative_path != MACOS_PILLOW_LIBJPEG_PATH:
        raise ValueError(f"upstream Pillow LC_RPATH appears in an unreviewed Mach-O binary: {binary}")
    if value != MACOS_PILLOW_UPSTREAM_RPATH:
        raise ValueError(f"unreviewed absolute LC_RPATH in locked Pillow libjpeg: {value}")
    if digest(binary) != MACOS_PILLOW_LIBJPEG_SHA256:
        raise ValueError(f"locked Pillow libjpeg binary digest changed: {binary}")
    if dependencies != MACOS_PILLOW_LIBJPEG_DEPENDENCIES:
        raise ValueError(f"locked Pillow libjpeg load graph changed: {dependencies!r}")
    return {
        "binary": relative_path,
        "binary_sha256": MACOS_PILLOW_LIBJPEG_SHA256,
        "wheel_sha256": MACOS_PILLOW_WHEEL_SHA256,
        "lc_rpath": value,
        "dependencies": list(dependencies),
    }


def _validate_pyav_rpath_exception(
    value: str,
    *,
    binary: Path,
    root: Path,
    dependencies: tuple[str, ...],
    rpaths: tuple[str, ...],
    install_names: tuple[str, ...],
    undefined_symbols: tuple[str, ...],
    has_initializers: bool,
) -> dict:
    """Allow the exact locked PyAV LC_RPATH only when its complete load surface stays unchanged."""
    try:
        relative_path = binary.relative_to(root).as_posix()
    except ValueError as error:
        raise ValueError(f"PyAV LC_RPATH exception binary is outside the relocated bundle: {binary}") from error
    if relative_path != MACOS_PYAV_LIBSHARPYUV_PATH:
        raise ValueError(f"PyAV upstream LC_RPATH appears in an unreviewed Mach-O binary: {binary}")
    if value != MACOS_PYAV_UPSTREAM_RPATH or rpaths != (MACOS_PYAV_UPSTREAM_RPATH,):
        raise ValueError(f"unreviewed PyAV LC_RPATH command set: {rpaths!r}")
    if digest(binary) != MACOS_PYAV_LIBSHARPYUV_SHA256:
        raise ValueError(f"locked PyAV libsharpyuv binary digest changed: {binary}")
    if dependencies != MACOS_PYAV_LIBSHARPYUV_DEPENDENCIES:
        raise ValueError(f"locked PyAV libsharpyuv load graph changed: {dependencies!r}")
    if install_names != (MACOS_PYAV_LIBSHARPYUV_ID,):
        raise ValueError(f"locked PyAV libsharpyuv install name changed: {install_names!r}")
    dynamic_loader_symbols = tuple(
        symbol for symbol in undefined_symbols
        if any(symbol.startswith(prefix) for prefix in MACOS_DYNAMIC_LOADER_SYMBOL_PREFIXES)
    )
    if dynamic_loader_symbols:
        raise ValueError(f"locked PyAV libsharpyuv imports dynamic-loader APIs: {dynamic_loader_symbols!r}")
    if has_initializers:
        raise ValueError("locked PyAV libsharpyuv contains Mach-O initializers")
    rpath_names = _embedded_rpath_candidate_names(binary)
    if rpath_names != MACOS_PYAV_LIBSHARPYUV_EMBEDDED_RPATH_NAMES:
        raise ValueError(f"locked PyAV libsharpyuv embedded @rpath names changed: {rpath_names!r}")
    return {
        "binary": relative_path,
        "binary_sha256": MACOS_PYAV_LIBSHARPYUV_SHA256,
        "wheel_sha256": MACOS_PYAV_WHEEL_SHA256,
        "install_name": MACOS_PYAV_LIBSHARPYUV_ID,
        "lc_rpath": value,
        "dependencies": list(dependencies),
        "embedded_rpath_names": list(rpath_names),
        "dynamic_loader_symbols": list(dynamic_loader_symbols),
        "has_initializers": has_initializers,
    }


def _verify_pillow_codec_runtime(root: Path, env: dict[str, str]) -> dict:
    """Require the locked Pillow native features and image codecs after relocation."""
    interpreter = root / "python" / "bin" / "python3"
    result = subprocess.run(
        [str(interpreter), "-I", "-B", "-c", PIL_CODEC_ROUNDTRIP_PROBE],
        cwd=root,
        env=env,
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode:
        raise ValueError(f"relocated Pillow codec round-trip probe failed: {result.stderr.strip()}")
    _require_no_vendor_trace(result.stderr, "Pillow codec round trips")
    try:
        evidence = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise ValueError("relocated Pillow codec probe did not return valid JSON") from error
    expected = {
        "codecs": list(PILLOW_CODECS),
        "features": list(PILLOW_REQUIRED_FEATURES),
        "tiff_compression": "tiff_lzw",
        "vendor_images": [],
    }
    if evidence != expected:
        raise ValueError(f"relocated Pillow codec probe returned unexpected evidence: {evidence!r}")
    return {
        "pillow_codecs": evidence["codecs"],
        "pillow_features": evidence["features"],
        "tiff_compression": evidence["tiff_compression"],
    }


def _inspect_macos_native_runtime(root: Path) -> dict:
    """Validate the relocated Mach-O graph and collect names for an adversarial runpath probe."""
    machine = platform.machine().lower()
    if machine not in {"arm64", "aarch64"}:
        raise ValueError(f"macOS bundle verification requires an arm64 runner, found {machine}")
    interpreter = root / "python" / "bin" / "python3"
    otool = shutil.which("otool")
    if otool is None:
        raise ValueError("otool is required to verify macOS Mach-O load paths")
    nm = shutil.which("nm")
    if nm is None:
        raise ValueError("nm is required to inspect PyAV dynamic-loader imports")
    executable_directory = interpreter.parent
    inspected = 0
    pillow_rpath_evidence = []
    pyav_rpath_evidence = []
    rpath_candidate_names = set(MACOS_PYAV_CANARY_NAMES)
    for binary in sorted(path for path in root.rglob("*") if path.is_file()):
        try:
            with binary.open("rb") as stream:
                is_macho = stream.read(4) in MACHO_MAGICS
        except OSError:
            continue
        if not is_macho:
            continue
        load_commands = subprocess.run([otool, "-l", str(binary)], check=True, capture_output=True, text=True)
        dependencies = _parse_otool_dependencies(load_commands.stdout)
        rpaths = _parse_otool_rpaths(load_commands.stdout)
        relative_path = binary.relative_to(root).as_posix()
        for value in dependencies:
            _validate_macho_path(value, binary=binary, root=root, executable_directory=executable_directory)
            if candidate := _rpath_candidate_name(value):
                rpath_candidate_names.add(candidate)
        for value in rpaths:
            if value == MACOS_PILLOW_UPSTREAM_RPATH:
                pillow_rpath_evidence.append(
                    _validate_pillow_rpath_exception(
                        value,
                        binary=binary,
                        root=root,
                        dependencies=dependencies,
                    )
                )
            elif value == MACOS_PYAV_UPSTREAM_RPATH:
                if relative_path != MACOS_PYAV_LIBSHARPYUV_PATH:
                    raise ValueError(f"PyAV upstream LC_RPATH appears in an unreviewed Mach-O binary: {binary}")
                ids = _parse_otool_install_names(load_commands.stdout)
                symbols = subprocess.run([nm, "-u", str(binary)], check=True, capture_output=True, text=True)
                pyav_rpath_evidence.append(
                    _validate_pyav_rpath_exception(
                        value,
                        binary=binary,
                        root=root,
                        dependencies=dependencies,
                        rpaths=rpaths,
                        install_names=ids,
                        undefined_symbols=_parse_nm_undefined_symbols(symbols.stdout),
                        has_initializers=_has_macho_initializers(load_commands.stdout),
                    )
                )
            else:
                _validate_macho_path(value, binary=binary, root=root, executable_directory=executable_directory)
        rpath_candidate_names.update(_embedded_rpath_candidate_names(binary))
        inspected += 1
    if inspected == 0:
        raise ValueError("no Mach-O files found in the relocated macOS core bundle")
    if len(pillow_rpath_evidence) != 1:
        raise ValueError(f"expected exactly one reviewed Pillow LC_RPATH, found {len(pillow_rpath_evidence)}")
    if len(pyav_rpath_evidence) != 1:
        raise ValueError(f"expected exactly one reviewed PyAV LC_RPATH, found {len(pyav_rpath_evidence)}")
    return {
        "macho_files": inspected,
        "pillow_lc_rpath_exception": pillow_rpath_evidence,
        "pyav_lc_rpath_exception": pyav_rpath_evidence,
        "rpath_canary_names": sorted(rpath_candidate_names),
    }


@contextmanager
def _populated_macos_rpath_directory(root: Path, candidate_names: list[str], marker: Path):
    """Populate the embedded runpath and prove dyld image capture with a positive control."""
    vendor_root = Path("/tmp/vendor")
    vendor_lib = vendor_root / "lib"
    if vendor_root.is_symlink() or (vendor_root.exists() and not vendor_root.is_dir()):
        raise ValueError("refusing to use a non-directory or symlink at /tmp/vendor")
    if vendor_lib.is_symlink() or (vendor_lib.exists() and not vendor_lib.is_dir()):
        raise ValueError("refusing to use a non-directory or symlink at /tmp/vendor/lib")
    if vendor_lib.exists() and any(vendor_lib.iterdir()):
        raise ValueError("refusing to overwrite existing entries in /tmp/vendor/lib for the runpath probe")

    clang = shutil.which("clang")
    if clang is None:
        raise ValueError("clang is required to populate /tmp/vendor/lib with runpath canaries")
    otool = shutil.which("otool")
    if otool is None:
        raise ValueError("otool is required to validate the runpath canary positive control")
    marker.parent.mkdir(parents=True, exist_ok=True)
    created_vendor_root = not vendor_root.exists()
    created_vendor_lib = not vendor_lib.exists()
    created_directories = []
    created_links = []
    canary_workspace = tempfile.TemporaryDirectory(prefix="sidevoice-rpath-canary-", dir=root)
    canary_root = Path(canary_workspace.name)
    canary_source = canary_root / "canary.c"
    canary_binary = vendor_lib / "__sidevoice_rpath_canary__.dylib"
    positive_source = canary_root / "positive.c"
    positive_binary = canary_root / "positive-control"
    marker_literal = json.dumps(str(marker))
    canary_source.write_text(
        "#include <fcntl.h>\n"
        "#include <unistd.h>\n"
        "int sidevoice_rpath_canary_probe(void) { return 0; }\n"
        "__attribute__((constructor)) static void sidevoice_rpath_canary_loaded(void) {\n"
        f"    int fd = open({marker_literal}, O_WRONLY | O_CREAT | O_APPEND, 0600);\n"
        "    if (fd >= 0) { const char value[] = \"loaded\\n\"; (void)write(fd, value, sizeof(value) - 1); close(fd); }\n"
        "}\n",
        encoding="utf-8",
    )
    positive_source.write_text(
        "#include <mach-o/dyld.h>\n"
        "#include <stdio.h>\n"
        "#include <string.h>\n"
        "extern int sidevoice_rpath_canary_probe(void);\n"
        "int main(void) {\n"
        "    if (sidevoice_rpath_canary_probe() != 0) return 2;\n"
        "    int found = 0;\n"
        "    for (uint32_t i = 0; i < _dyld_image_count(); ++i) {\n"
        "        const char *image = _dyld_get_image_name(i);\n"
        "        if (image && (strstr(image, \"/tmp/vendor/lib/\") || strstr(image, \"/private/tmp/vendor/lib/\"))) {\n"
        "            puts(image); found = 1;\n"
        "        }\n"
        "    }\n"
        "    return found ? 0 : 3;\n"
        "}\n",
        encoding="utf-8",
    )
    try:
        vendor_lib.mkdir(parents=True, exist_ok=True)
        subprocess.run(
            [
                clang, "-dynamiclib", "-arch", "arm64", str(canary_source),
                "-Wl,-install_name,@rpath/libsidevoice_rpath_canary.dylib", "-o", str(canary_binary),
            ],
            check=True,
            capture_output=True,
            text=True,
        )
        for name in sorted(set(candidate_names)):
            relative = PurePosixPath(name)
            if relative.is_absolute() or not relative.parts or any(part in {"", ".", ".."} for part in relative.parts):
                raise ValueError(f"unsafe runpath canary name: {name!r}")
            destination = vendor_lib.joinpath(*relative.parts)
            parent = destination.parent
            missing_parents = []
            current = parent
            while current != vendor_lib and not current.exists():
                missing_parents.append(current)
                current = current.parent
            for directory in reversed(missing_parents):
                directory.mkdir()
                created_directories.append(directory)
            if destination.exists() or destination.is_symlink():
                raise ValueError(f"runpath canary path already exists: {destination}")
            destination.symlink_to(canary_binary)
            created_links.append(destination)

        positive_link = vendor_lib / "libsidevoice_rpath_canary.dylib"
        if not positive_link.exists():
            positive_link.symlink_to(canary_binary)
            created_links.append(positive_link)
        subprocess.run(
            [
                clang, "-arch", "arm64", str(positive_source), "-L", str(vendor_lib),
                "-lsidevoice_rpath_canary", "-Wl,-rpath,/tmp/vendor/lib", "-o", str(positive_binary),
            ],
            check=True,
            capture_output=True,
            text=True,
        )
        positive_load_commands = subprocess.run(
            [otool, "-l", str(positive_binary)], check=True, capture_output=True, text=True
        ).stdout
        positive_dependencies = _parse_otool_dependencies(positive_load_commands)
        positive_rpaths = _parse_otool_rpaths(positive_load_commands)
        expected_positive_dependency = "@rpath/libsidevoice_rpath_canary.dylib"
        if expected_positive_dependency not in positive_dependencies or "/tmp/vendor/lib" not in positive_rpaths:
            raise ValueError(
                "runpath canary positive control is not linked through the expected @rpath: "
                f"dependencies={positive_dependencies!r}, rpaths={positive_rpaths!r}"
            )
        marker.unlink(missing_ok=True)
        positive = subprocess.run(
            [str(positive_binary)],
            check=False,
            capture_output=True,
            text=True,
            env={**os.environ, "DYLD_PRINT_LIBRARIES": "1"},
        )
        positive_images = [line.strip() for line in positive.stdout.splitlines() if line.strip()]
        positive_trace = positive.stdout + positive.stderr
        if positive.returncode or not marker.is_file() or not any(
            token in image for image in positive_images for token in MACOS_TEMP_VENDOR_PATHS
        ) or not any(token in positive_trace for token in MACOS_TEMP_VENDOR_PATHS):
            raise ValueError(
                "runpath canary positive control did not capture an image loaded from /tmp/vendor/lib: "
                f"status={positive.returncode}, stdout={positive.stdout!r}, stderr={positive.stderr!r}"
            )
        canonical_directory = str(vendor_lib.resolve(strict=True))
        marker.unlink()
        yield {
            "directory": MACOS_PYAV_UPSTREAM_RPATH,
            "canonical_directory": canonical_directory,
            "canary_names": sorted(set(candidate_names)),
            "canary_count": len(set(candidate_names)),
            "loaded_image_capture_positive_control": positive_images,
            "positive_control_marker": True,
        }
    finally:
        for link in reversed(created_links):
            link.unlink(missing_ok=True)
        canary_binary.unlink(missing_ok=True)
        for directory in reversed(created_directories):
            directory.rmdir()
        if created_vendor_lib and vendor_lib.exists():
            vendor_lib.rmdir()
        if created_vendor_root and vendor_root.exists():
            vendor_root.rmdir()
        canary_workspace.cleanup()


def _verify_pyav_audio_runtime(root: Path, env: dict[str, str]) -> dict:
    """Encode, decode and resample audio through the relocated, locked PyAV runtime."""
    interpreter = root / "python" / "bin" / "python3"
    probe = subprocess.run(
        [str(interpreter), "-I", "-B", "-c", PYAV_AUDIO_ROUNDTRIP_PROBE],
        cwd=root,
        env=env,
        check=False,
        capture_output=True,
        text=True,
    )
    if probe.returncode:
        raise ValueError(f"relocated PyAV audio round-trip probe failed: {probe.stderr.strip()}")
    _require_no_vendor_trace(probe.stderr, "PyAV audio encode/decode/resample")
    try:
        evidence = json.loads(probe.stdout)
    except json.JSONDecodeError as error:
        raise ValueError("relocated PyAV probe did not return valid JSON") from error
    expected = {
        "version": MACOS_PYAV_VERSION,
        "container": "wav",
        "codec": "pcm_s16le",
        "decoded_samples": 320,
        "resampled_rate": 8000,
        "resampled_samples": 160,
        "vendor_images": [],
    }
    if evidence != expected:
        raise ValueError(f"relocated PyAV audio probe returned unexpected evidence: {evidence!r}")
    return evidence


def _verify_pyav_webp_runtime(root: Path, env: dict[str, str]) -> dict:
    """Exercise PyAV's WebP/sharpyuv path and capture the loaded image paths after relocation."""
    interpreter = root / "python" / "bin" / "python3"
    probe = subprocess.run(
        [str(interpreter), "-I", "-B", "-c", PYAV_WEBP_ROUNDTRIP_PROBE],
        cwd=root,
        env=env,
        check=False,
        capture_output=True,
        text=True,
    )
    if probe.returncode:
        raise ValueError(f"relocated PyAV WebP round-trip probe failed: {probe.stderr.strip()}")
    _require_no_vendor_trace(probe.stderr, "PyAV WebP/sharpyuv encode/decode")
    try:
        evidence = json.loads(probe.stdout)
    except json.JSONDecodeError as error:
        raise ValueError("relocated PyAV WebP probe did not return valid JSON") from error
    if (
        evidence.get("version") != MACOS_PYAV_VERSION
        or evidence.get("format") != "webp"
        or evidence.get("codec") != "libwebp"
        or evidence.get("decoded_frames") != 1
        or not evidence.get("sharpyuv_images")
        or evidence.get("vendor_images") != []
    ):
        raise ValueError(f"relocated PyAV WebP probe returned unexpected evidence: {evidence!r}")
    return evidence


def _verify_relocated_core_start(root: Path, env: dict[str, str]) -> dict:
    """Start the relocated core, probe its local readiness socket, then require clean shutdown."""
    import http.client
    import signal
    import socket
    import time

    interpreter = root / "python" / "bin" / "python3"
    data_directory = root / "core-start-probe"
    ready_file = data_directory / "core.json"
    socket_path = data_directory / "local.sock"
    dyld_log_path = root / "core-dyld-trace.log"
    launch_id = "r4-a-relocated-core-start"
    dyld_log = dyld_log_path.open("w", encoding="utf-8")
    process = subprocess.Popen(
        [
            str(interpreter), "-I", "-B", "-m", "sidevoice_core.server",
            "--port", "0", "--data-dir", str(data_directory), "--idle-exit", "0", "--launch-id", launch_id,
        ],
        cwd=root,
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=dyld_log,
    )

    class UnixHTTPConnection(http.client.HTTPConnection):
        def connect(self):
            self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            self.sock.settimeout(self.timeout)
            self.sock.connect(str(socket_path))

    try:
        deadline = time.monotonic() + 60
        while not ready_file.is_file():
            if process.poll() is not None:
                failure = data_directory / "core-failure.json"
                detail = failure.read_text(encoding="utf-8") if failure.is_file() else "no core-failure.json"
                raise ValueError(f"relocated core exited before ready ({process.returncode}): {detail}")
            if time.monotonic() >= deadline:
                raise ValueError("relocated core did not become ready within 60 seconds")
            time.sleep(0.1)

        facts = json.loads(ready_file.read_text(encoding="utf-8"))
        if facts.get("pid") != process.pid or facts.get("launch_id") != launch_id or not socket_path.exists():
            raise ValueError(f"relocated core ready file does not match the running process: {facts!r}")
        ready_mode = ready_file.stat().st_mode & 0o777
        if ready_mode != 0o600:
            raise ValueError(f"relocated core ready file mode is not 0600: {ready_mode:o}")

        connection = UnixHTTPConnection("localhost", timeout=10)
        connection.request("GET", "/api/local/health")
        response = connection.getresponse()
        body = response.read()
        connection.close()
        if response.status != 200:
            raise ValueError(f"relocated core local health returned HTTP {response.status}: {body[:300]!r}")
        health = json.loads(body)
        if health.get("pid") != process.pid or health.get("launch_id") != launch_id:
            raise ValueError(f"relocated core health does not match the running process: {health!r}")

        process.send_signal(signal.SIGTERM)
        try:
            return_code = process.wait(timeout=30)
        except subprocess.TimeoutExpired as error:
            raise ValueError("relocated core did not stop within 30 seconds after SIGTERM") from error
        dyld_log.flush()
        dyld_log.close()
        dyld_trace = dyld_log_path.read_text(encoding="utf-8", errors="replace")
        _require_no_vendor_trace(dyld_trace, "relocated core startup and shutdown")
        dyld_log_path.unlink(missing_ok=True)
        if return_code != 0 or ready_file.exists() or socket_path.exists():
            raise ValueError(
                f"relocated core did not shut down cleanly: status={return_code}, "
                f"ready_exists={ready_file.exists()}, socket_exists={socket_path.exists()}"
            )
        return {
            "started": True,
            "local_health": "ok",
            "clean_exit": return_code,
            "ready_mode": f"{ready_mode:04o}",
            "dyld_trace_checked": True,
            "vendor_images": [],
        }
    finally:
        if process.poll() is None:
            process.kill()
            process.wait(timeout=10)
        if not dyld_log.closed:
            dyld_log.close()
        dyld_log_path.unlink(missing_ok=True)


def _verify_macos_native_runtime(root: Path, env: dict[str, str], graph: dict) -> dict:
    """Import native dependencies and execute the locked PyAV media path after relocation."""
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
    _require_no_vendor_trace(probe.stderr, "relocated macOS native imports")
    try:
        imports = json.loads(probe.stdout)
    except json.JSONDecodeError as error:
        raise ValueError("relocated macOS native import probe did not return valid JSON") from error
    if imports != {"native_imports": 12, "vendor_images": []}:
        raise ValueError(f"relocated macOS native import probe returned unexpected evidence: {imports!r}")
    audio = _verify_pyav_audio_runtime(root, env)
    webp = _verify_pyav_webp_runtime(root, env)
    return {
        **imports,
        **graph,
        "pyav_audio_roundtrip": audio,
        "pyav_webp_roundtrip": webp,
    }


def _require_no_vendor_trace(trace: str, stage: str) -> None:
    matches = [line.strip() for line in trace.splitlines() if any(path in line for path in MACOS_TEMP_VENDOR_PATHS)]
    if matches:
        raise ValueError(f"dyld loaded an image from /tmp/vendor/lib during {stage}: {matches!r}")


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
        if sys.platform == "darwin":
            graph = _inspect_macos_native_runtime(second)
            marker = root / "pyav-rpath-canary-loaded.marker"
            if marker.exists():
                raise ValueError("runpath canary marker unexpectedly exists before the relocated runtime checks")
            with _populated_macos_rpath_directory(root, graph["rpath_canary_names"], marker) as canary:
                macos_env = {
                    **env,
                    "DYLD_PRINT_LIBRARIES": "1",
                    "DYLD_PRINT_LIBRARIES_POST_LAUNCH": "1",
                }
                result = subprocess.run(
                    relocated_command,
                    cwd=second,
                    env=macos_env,
                    check=True,
                    capture_output=True,
                    text=True,
                )
                if '"ok": true' not in result.stdout and '"ok":true' not in result.stdout:
                    raise ValueError(f"relocated bundle self-test did not report success: {result.stdout.strip()}")
                _require_no_vendor_trace(result.stderr, "relocated self-test")
                _require_unloaded_rpath_canary(marker, canary, "relocated self-test")
                pillow_check = _verify_pillow_codec_runtime(second, macos_env)
                _require_unloaded_rpath_canary(marker, canary, "Pillow codec round trips")
                native_check = _verify_macos_native_runtime(second, macos_env, graph)
                _require_unloaded_rpath_canary(marker, canary, "native imports and PyAV audio round trip")
                core_start = _verify_relocated_core_start(second, macos_env)
                _require_unloaded_rpath_canary(marker, canary, "running core health probe")
                native_check["relocated_core_start"] = core_start
                native_check["rpath_canary"] = {**canary, "loaded_during_runtime": marker.exists()}
        else:
            result = subprocess.run(relocated_command, cwd=second, env=env, check=True, capture_output=True, text=True)
            if '"ok": true' not in result.stdout and '"ok":true' not in result.stdout:
                raise ValueError(f"relocated bundle self-test did not report success: {result.stdout.strip()}")
            pillow_check = _verify_pillow_codec_runtime(second, env)
            native_check = {}
    return {**info, **pillow_check, **native_check}


def _require_unloaded_rpath_canary(marker: Path, canary: dict, stage: str) -> None:
    if marker.exists():
        raise ValueError(
            f"/tmp/vendor/lib runpath canary was loaded during {stage}; "
            f"candidate names were {canary['canary_names']!r}"
        )


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
