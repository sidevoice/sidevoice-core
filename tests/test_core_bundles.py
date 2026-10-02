"""Contract tests for release manifest generation and archive relocation."""

import json
import io
import tarfile
import unittest
from pathlib import Path
from urllib.parse import quote, unquote, urlsplit

from tools.core_bundle import (
    MACOS_PILLOW_LIBJPEG_DEPENDENCIES,
    MACOS_PILLOW_LIBJPEG_PATH,
    MACOS_PILLOW_LIBJPEG_SHA256,
    MACOS_PILLOW_UPSTREAM_RPATH,
    MACOS_PILLOW_WHEEL_SHA256,
    MACOS_PYAV_LIBSHARPYUV_DEPENDENCIES,
    MACOS_PYAV_LIBSHARPYUV_EMBEDDED_RPATH_NAMES,
    MACOS_PYAV_LIBSHARPYUV_ID,
    MACOS_PYAV_LIBSHARPYUV_PATH,
    MACOS_PYAV_LIBSHARPYUV_SHA256,
    MACOS_PYAV_UPSTREAM_RPATH,
    MACOS_PYAV_WHEEL_SHA256,
    MACOS_SCIPY_FBLAS_DEPENDENCIES,
    MACOS_SCIPY_FBLAS_LC_RPATHS,
    MACOS_SCIPY_FBLAS_PATH,
    MACOS_SCIPY_FBLAS_SHA256,
    MACOS_SCIPY_UPSTREAM_RPATHS,
    MACOS_SCIPY_WHEEL_SHA256,
    PIL_CODEC_ROUNDTRIP_PROBE,
    PYAV_AUDIO_ROUNDTRIP_PROBE,
    PYAV_WEBP_ROUNDTRIP_PROBE,
    SCIPY_RUNTIME_PROBE,
    PILLOW_CODECS,
    PILLOW_REQUIRED_FEATURES,
    _parse_otool_dependencies,
    _parse_otool_install_names,
    _parse_otool_rpaths,
    _parse_nm_undefined_symbols,
    _has_macho_initializers,
    _rpath_candidate_name,
    _validate_macho_path,
    _validate_pillow_rpath_exception,
    _validate_pyav_rpath_exception,
    _validate_pyav_audio_runtime_evidence,
    _validate_scipy_rpath_exception,
    _require_no_vendor_trace,
    _require_no_reviewed_rpath_trace,
    _validate_rpath_positive_control,
    _validate_rpath_positive_control_load_commands,
    _make_short_private_socket_location,
    create_archive,
    extract_python_distribution,
    inspect_archive,
    remove_build_path_entrypoints,
    remove_install_source_metadata,
    remove_upstream_sboms,
    safe_link_target,
    safe_member_path,
    verify_relocation,
)
from tools.core_manifest import TARGETS, sha256, validate_manifest, write_manifest


class CoreBundleTests(unittest.TestCase):
    def test_pillow_rpath_exception_is_bound_to_the_locked_wheel(self):
        import tomllib

        project = Path(__file__).resolve().parents[1]
        lock = tomllib.loads((project / "uv.lock").read_text(encoding="utf-8"))
        pillow = next(package for package in lock["package"] if package["name"] == "pillow")
        arm_wheel = next(
            wheel for wheel in pillow["wheels"] if "macosx_11_0_arm64.whl" in wheel["url"]
        )
        self.assertEqual(arm_wheel["hash"].removeprefix("sha256:"), MACOS_PILLOW_WHEEL_SHA256)
        self.assertRegex(MACOS_PILLOW_LIBJPEG_SHA256, r"^[0-9a-f]{64}$")

    def test_scipy_rpath_exception_is_bound_to_the_locked_wheel(self):
        import tomllib

        project = Path(__file__).resolve().parents[1]
        lock = tomllib.loads((project / "uv.lock").read_text(encoding="utf-8"))
        scipy = next(package for package in lock["package"] if package["name"] == "scipy")
        arm_wheel = next(
            wheel for wheel in scipy["wheels"] if "cp312-cp312-macosx_14_0_arm64.whl" in wheel["url"]
        )
        self.assertEqual(arm_wheel["hash"].removeprefix("sha256:"), MACOS_SCIPY_WHEEL_SHA256)
        self.assertRegex(MACOS_SCIPY_FBLAS_SHA256, r"^[0-9a-f]{64}$")

    def test_pillow_codec_probe_requires_native_features_and_lzw_tiff(self):
        import subprocess
        import sys

        result = subprocess.run(
            [sys.executable, "-I", "-B", "-c", PIL_CODEC_ROUNDTRIP_PROBE],
            check=True,
            capture_output=True,
            text=True,
        )
        evidence = json.loads(result.stdout)
        self.assertEqual(evidence["codecs"], list(PILLOW_CODECS))
        self.assertEqual(evidence["features"], list(PILLOW_REQUIRED_FEATURES))
        self.assertEqual(evidence["tiff_compression"], "tiff_lzw")
        self.assertEqual(evidence["vendor_images"], [])
        self.assertEqual(evidence["rpath_images"], [])

    def test_python_build_standalone_pins_match_their_release_urls(self):
        pins_path = Path(__file__).resolve().parents[1] / "packaging/python-build-standalone.json"
        pins = json.loads(pins_path.read_text(encoding="utf-8"))
        for target, pin in pins["targets"].items():
            parsed = urlsplit(pin["url"])
            self.assertEqual(parsed.scheme, "https")
            self.assertEqual(parsed.netloc, "github.com")
            self.assertEqual(unquote(parsed.path.rsplit("/", 1)[-1]), pin["asset"], target)
            self.assertEqual(
                parsed.path,
                f"/astral-sh/python-build-standalone/releases/download/{pins['release']}/{quote(pin['asset'], safe='')}",
                target,
            )
            self.assertRegex(pin["sha256"], r"^[0-9a-f]{64}$")
            self.assertGreater(pin["size"], 0)
        license_directory = pins_path.parent / "python-build-standalone-licenses"
        self.assertEqual(set(pins["license_files"]), {path.name for path in license_directory.iterdir()})
        for name, expected in pins["license_files"].items():
            self.assertEqual(sha256(license_directory / name), expected, name)

    def test_removes_sboms_with_this_build_paths_but_keeps_upstream_provenance(self):
        import tempfile

        with tempfile.TemporaryDirectory() as temporary:
            site_packages = Path(temporary) / "site-packages"
            sboms = site_packages / "jiter-0.dist-info/sboms"
            sboms.mkdir(parents=True)
            dist_info = sboms.parent
            (dist_info / "direct_url.json").write_text('{"url":"file:///build/pip.whl"}', encoding="utf-8")
            checkout = Path(temporary) / "sidevoice-core"
            temp_root = Path(temporary) / "runner-temp"
            (sboms / "checkout.json").write_text(str(checkout), encoding="utf-8")
            (sboms / "temp.json").write_text(str(temp_root), encoding="utf-8")
            (sboms / "upstream.json").write_text(
                "file:///Users/runner/work/PyAV/PyAV/src/av/core.py", encoding="utf-8"
            )
            (sboms / "safe.json").write_text('{"bomFormat":"CycloneDX"}', encoding="utf-8")
            self.assertEqual(remove_upstream_sboms(site_packages, (checkout, temp_root)), 2)
            self.assertEqual(remove_install_source_metadata(site_packages), 1)
            self.assertFalse((sboms / "checkout.json").exists())
            self.assertFalse((sboms / "temp.json").exists())
            self.assertTrue((sboms / "upstream.json").is_file())
            self.assertTrue((sboms / "safe.json").is_file())
            self.assertFalse((dist_info / "direct_url.json").exists())

    def test_macho_load_path_parser_and_validation(self):
        import tempfile

        load_commands = """Load command 1
          cmd LC_ID_DYLIB
             name libitcl4.3.8.dylib (offset 24)
Load command 2
          cmd LC_LOAD_DYLIB
             name @rpath/libavcodec.dylib (offset 24)
Load command 3
          cmd LC_LOAD_WEAK_DYLIB
             name /usr/lib/libSystem.B.dylib (offset 24)
Load command 4
          cmd LC_RPATH
      cmdsize 40
         path @loader_path/.dylibs (offset 12)
"""
        self.assertEqual(
            _parse_otool_dependencies(load_commands),
            ("@rpath/libavcodec.dylib", "/usr/lib/libSystem.B.dylib"),
        )
        self.assertEqual(_parse_otool_rpaths(load_commands), ("@loader_path/.dylibs",))

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "relocated"
            binary = root / "python/lib/python3.12/site-packages/av/_core.abi3.so"
            executable_directory = root / "python/bin"
            binary.parent.mkdir(parents=True)
            executable_directory.mkdir(parents=True)
            _validate_macho_path(
                "@loader_path/.dylibs/libavcodec.dylib",
                binary=binary,
                root=root,
                executable_directory=executable_directory,
            )
            _validate_macho_path(
                "/usr/lib/libSystem.B.dylib",
                binary=binary,
                root=root,
                executable_directory=executable_directory,
            )
            _validate_macho_path(
                "/usr/lib",
                binary=binary,
                root=root,
                executable_directory=executable_directory,
            )
            with self.assertRaisesRegex(ValueError, "non-system absolute Mach-O load path"):
                _validate_macho_path(
                    "/usr/library",
                    binary=binary,
                    root=root,
                    executable_directory=executable_directory,
                )
            with self.assertRaisesRegex(ValueError, "escapes the relocated bundle"):
                _validate_macho_path(
                    "@loader_path/../../../../../../outside.dylib",
                    binary=binary,
                    root=root,
                    executable_directory=executable_directory,
                )
            with self.assertRaisesRegex(ValueError, "non-system absolute Mach-O load path"):
                _validate_macho_path(
                    "/Users/runner/work/sidevoice-core/sidevoice-core/libsecret.dylib",
                    binary=binary,
                    root=root,
                    executable_directory=executable_directory,
                )
            with self.assertRaisesRegex(ValueError, "non-system absolute Mach-O load path"):
                _validate_macho_path(
                    "/Users/runner/work/PyAV/PyAV/libavcodec.dylib",
                    binary=binary,
                    root=root,
                    executable_directory=executable_directory,
                )
            with self.assertRaisesRegex(ValueError, "unsafe absolute Mach-O load path"):
                _validate_macho_path(
                    "/usr/lib/../Users/runner/work/PyAV/PyAV/libavcodec.dylib",
                    binary=binary,
                    root=root,
                    executable_directory=executable_directory,
                )

    def test_exact_locked_pillow_rpath_exception_checks_binary_hash_and_load_graph(self):
        import tempfile
        from unittest.mock import patch

        load_commands = """Load command 8
          cmd LC_RPATH
      cmdsize 64
         path /Users/runner/work/Pillow/Pillow/build/deps/darwin/lib (offset 12)
"""
        rpaths = _parse_otool_rpaths(load_commands)
        self.assertEqual(rpaths, (MACOS_PILLOW_UPSTREAM_RPATH,))

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "relocated"
            binary = root / MACOS_PILLOW_LIBJPEG_PATH
            binary.parent.mkdir(parents=True)
            binary.write_bytes(b"locked Pillow dylib fixture")
            with patch("tools.core_bundle.digest", return_value=MACOS_PILLOW_LIBJPEG_SHA256):
                evidence = _validate_pillow_rpath_exception(
                    rpaths[0],
                    binary=binary,
                    root=root,
                    dependencies=MACOS_PILLOW_LIBJPEG_DEPENDENCIES,
                )
            self.assertEqual(evidence["binary"], MACOS_PILLOW_LIBJPEG_PATH)
            self.assertEqual(evidence["binary_sha256"], MACOS_PILLOW_LIBJPEG_SHA256)
            self.assertEqual(evidence["wheel_sha256"], MACOS_PILLOW_WHEEL_SHA256)
            self.assertEqual(evidence["dependencies"], list(MACOS_PILLOW_LIBJPEG_DEPENDENCIES))

            with patch("tools.core_bundle.digest", return_value="0" * 64):
                with self.assertRaisesRegex(ValueError, "binary digest changed"):
                    _validate_pillow_rpath_exception(
                        rpaths[0],
                        binary=binary,
                        root=root,
                        dependencies=MACOS_PILLOW_LIBJPEG_DEPENDENCIES,
                    )
            with patch("tools.core_bundle.digest", return_value=MACOS_PILLOW_LIBJPEG_SHA256):
                with self.assertRaisesRegex(ValueError, "load graph changed"):
                    _validate_pillow_rpath_exception(
                        rpaths[0],
                        binary=binary,
                        root=root,
                        dependencies=("@rpath/libjpeg.dylib",),
                    )
                other_binary = root / "python/lib/python3.12/site-packages/PIL/.dylibs/libpng.dylib"
                other_binary.parent.mkdir(parents=True, exist_ok=True)
                other_binary.write_bytes(b"different dylib")
                with self.assertRaisesRegex(ValueError, "unreviewed Mach-O binary"):
                    _validate_pillow_rpath_exception(
                        rpaths[0],
                        binary=other_binary,
                        root=root,
                        dependencies=MACOS_PILLOW_LIBJPEG_DEPENDENCIES,
                    )

            with self.assertRaisesRegex(ValueError, "non-system absolute Mach-O load path"):
                _validate_macho_path(
                    MACOS_PILLOW_UPSTREAM_RPATH,
                    binary=binary,
                    root=root,
                    executable_directory=root / "python/bin",
                )

    def test_pyav_rpath_exception_is_bound_to_the_locked_wheel(self):
        import tomllib

        project = Path(__file__).resolve().parents[1]
        lock = tomllib.loads((project / "uv.lock").read_text(encoding="utf-8"))
        pyav = next(package for package in lock["package"] if package["name"] == "av")
        macos_arm64 = [
            wheel for wheel in pyav["wheels"]
            if urlsplit(wheel["url"]).path.endswith("av-17.1.0-cp311-abi3-macosx_14_0_arm64.whl")
        ]
        self.assertEqual(len(macos_arm64), 1)
        self.assertEqual(macos_arm64[0]["hash"], f"sha256:{MACOS_PYAV_WHEEL_SHA256}")

    def test_pyav_audio_and_webp_runtime_probes(self):
        import subprocess
        import sys

        for probe in (PYAV_AUDIO_ROUNDTRIP_PROBE, PYAV_WEBP_ROUNDTRIP_PROBE):
            result = subprocess.run(
                [sys.executable, "-I", "-B", "-c", probe],
                check=True,
                capture_output=True,
                text=True,
            )
            evidence = json.loads(result.stdout)
            self.assertEqual(evidence["version"], "17.1.0")
            self.assertEqual(evidence["vendor_images"], [])
            self.assertEqual(evidence["rpath_images"], [])
            if probe == PYAV_WEBP_ROUNDTRIP_PROBE:
                self.assertEqual(evidence["sharp_conversion"]["function"], "SharpYuvConvert")
                self.assertEqual(evidence["sharp_conversion"]["input_rgb_bytes"], 16 * 16 * 3)
                self.assertRegex(evidence["sharp_conversion"]["output_sha256"], r"^[0-9a-f]{64}$")
            else:
                self.assertEqual(_validate_pyav_audio_runtime_evidence(evidence), evidence)
                incomplete = dict(evidence)
                del incomplete["rpath_images"]
                with self.assertRaisesRegex(ValueError, "unexpected evidence"):
                    _validate_pyav_audio_runtime_evidence(incomplete)

    def test_scipy_runtime_probe_executes_blas_and_solve(self):
        import subprocess
        import sys

        result = subprocess.run(
            [sys.executable, "-I", "-B", "-c", SCIPY_RUNTIME_PROBE],
            check=True,
            capture_output=True,
            text=True,
        )
        evidence = json.loads(result.stdout)
        self.assertEqual(evidence["scipy_version"], "1.18.1")
        self.assertEqual(evidence["dgemm"], [[19.0, 22.0], [43.0, 50.0]])
        self.assertEqual(evidence["solve"], [1.0, 2.0])
        self.assertEqual(evidence["rpath_images"], [])

    def test_dyld_trace_rejects_both_tmp_path_spellings(self):
        for path in ("/tmp/vendor/lib/libcandidate.dylib", "/private/tmp/vendor/lib/libcandidate.dylib"):
            with self.subTest(path=path), self.assertRaisesRegex(ValueError, "dyld loaded an image"):
                _require_no_vendor_trace(f"dyld: {path}\n", "test probe")
        for path in (
            MACOS_PILLOW_UPSTREAM_RPATH + "/libjpeg.62.dylib",
            *[rpath + "/libgcc_s.1.1.dylib" for rpath in MACOS_SCIPY_UPSTREAM_RPATHS],
        ):
            with self.subTest(path=path), self.assertRaisesRegex(ValueError, "reviewed upstream LC_RPATH"):
                _require_no_reviewed_rpath_trace(f"dyld: loaded: {path}\n", "test probe")

    def test_rpath_positive_control_requires_stderr_trace_separate_from_image_capture(self):
        with self.assertRaisesRegex(ValueError, "DYLD_PRINT_LIBRARIES capture"):
            _validate_rpath_positive_control(
                return_code=0,
                marker_loaded=True,
                loaded_image_lines=["/tmp/vendor/lib/libsidevoice_rpath_canary.dylib"],
                trace_lines=[],
                canonical_directory="/tmp/vendor/lib",
            )
        evidence = _validate_rpath_positive_control(
            return_code=0,
            marker_loaded=True,
            loaded_image_lines=["/private/tmp/vendor/lib/libsidevoice_rpath_canary.dylib"],
            trace_lines=["dyld[123]: loaded: /private/tmp/vendor/lib/libsidevoice_rpath_canary.dylib"],
            canonical_directory="/private/tmp/vendor/lib",
        )
        self.assertTrue(evidence["loaded_image_capture"])
        self.assertTrue(evidence["dyld_stderr_trace"])

    def test_rpath_positive_control_pins_canary_and_system_load_edges(self):
        load_commands = """Load command 1
          cmd LC_LOAD_DYLIB
             name @rpath/libsidevoice_rpath_canary.dylib (offset 24)
Load command 2
          cmd LC_LOAD_DYLIB
             name /usr/lib/libSystem.B.dylib (offset 24)
Load command 3
          cmd LC_RPATH
      cmdsize 40
         path /tmp/vendor/lib (offset 12)
"""
        self.assertEqual(
            _validate_rpath_positive_control_load_commands(load_commands, "/tmp/vendor/lib"),
            {
                "dependencies": [
                    "@rpath/libsidevoice_rpath_canary.dylib",
                    "/usr/lib/libSystem.B.dylib",
                ],
                "rpaths": ["/tmp/vendor/lib"],
            },
        )
        for altered in (
            load_commands.replace("/usr/lib/libSystem.B.dylib", "/usr/lib/libobjc.A.dylib"),
            load_commands.replace("/tmp/vendor/lib", "/tmp/other/lib"),
        ):
            with self.subTest(altered=altered), self.assertRaisesRegex(
                ValueError, "runpath positive control is not linked"
            ):
                _validate_rpath_positive_control_load_commands(altered, "/tmp/vendor/lib")

    def test_relocated_core_probe_uses_a_private_short_unix_socket_path(self):
        socket_directory, socket_path = _make_short_private_socket_location()
        try:
            self.assertEqual(socket_directory.stat().st_mode & 0o777, 0o700)
            self.assertEqual(socket_path.parent, socket_directory)
            self.assertLess(len(str(socket_path).encode()), 80)
        finally:
            socket_directory.rmdir()

    def test_pyav_load_command_parsers_and_exact_runpath_exception(self):
        import tempfile
        from unittest.mock import patch

        load_commands = f"""Load command 1
          cmd LC_ID_DYLIB
             name {MACOS_PYAV_LIBSHARPYUV_ID} (offset 24)
Load command 2
          cmd LC_LOAD_DYLIB
             name /usr/lib/libSystem.B.dylib (offset 24)
Load command 3
          cmd LC_RPATH
      cmdsize 40
         path /tmp/vendor/lib (offset 12)
"""
        dependencies = _parse_otool_dependencies(load_commands)
        rpaths = _parse_otool_rpaths(load_commands)
        install_names = _parse_otool_install_names(load_commands)
        symbols = _parse_nm_undefined_symbols("                 U _malloc\n(undefined) external _free (from libSystem)\n")
        self.assertEqual(dependencies, MACOS_PYAV_LIBSHARPYUV_DEPENDENCIES)
        self.assertEqual(rpaths, (MACOS_PYAV_UPSTREAM_RPATH,))
        self.assertEqual(install_names, (MACOS_PYAV_LIBSHARPYUV_ID,))
        self.assertEqual(symbols, ("_malloc", "_free"))
        self.assertEqual(_parse_nm_undefined_symbols("                 _dlopen\n _dlsym\n"), ("_dlopen", "_dlsym"))
        self.assertEqual(_parse_nm_undefined_symbols("dyld_stub_binder\n"), ("dyld_stub_binder",))
        with self.assertRaisesRegex(ValueError, "unrecognized `nm -u` output"):
            _parse_nm_undefined_symbols("unexpected nm diagnostic")
        self.assertFalse(_has_macho_initializers(load_commands))
        self.assertTrue(_has_macho_initializers("cmd LC_ROUTINES_64\nsectname __mod_init_func"))
        self.assertEqual(_rpath_candidate_name("@rpath/libcodec.dylib"), "libcodec.dylib")
        self.assertEqual(_rpath_candidate_name("@loader_path/libcodec.dylib"), None)
        with self.assertRaisesRegex(ValueError, "unsafe @rpath candidate"):
            _rpath_candidate_name("@rpath/../outside.dylib")

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "relocated"
            binary = root / MACOS_PYAV_LIBSHARPYUV_PATH
            binary.parent.mkdir(parents=True)
            binary.write_bytes(b"locked PyAV libsharpyuv fixture")
            with patch("tools.core_bundle.digest", return_value=MACOS_PYAV_LIBSHARPYUV_SHA256):
                evidence = _validate_pyav_rpath_exception(
                    rpaths[0],
                    binary=binary,
                    root=root,
                    dependencies=dependencies,
                    rpaths=rpaths,
                    install_names=install_names,
                    undefined_symbols=symbols,
                    has_initializers=False,
                )
            self.assertEqual(evidence["binary"], MACOS_PYAV_LIBSHARPYUV_PATH)
            self.assertEqual(evidence["binary_sha256"], MACOS_PYAV_LIBSHARPYUV_SHA256)
            self.assertEqual(evidence["wheel_sha256"], MACOS_PYAV_WHEEL_SHA256)
            self.assertEqual(evidence["install_name"], MACOS_PYAV_LIBSHARPYUV_ID)
            self.assertEqual(evidence["dependencies"], list(MACOS_PYAV_LIBSHARPYUV_DEPENDENCIES))
            self.assertEqual(evidence["embedded_rpath_names"], list(MACOS_PYAV_LIBSHARPYUV_EMBEDDED_RPATH_NAMES))
            self.assertEqual(evidence["dynamic_loader_symbols"], [])
            self.assertFalse(evidence["has_initializers"])

            with patch("tools.core_bundle.digest", return_value="0" * 64):
                with self.assertRaisesRegex(ValueError, "binary digest changed"):
                    _validate_pyav_rpath_exception(
                        rpaths[0],
                        binary=binary,
                        root=root,
                        dependencies=dependencies,
                        rpaths=rpaths,
                        install_names=install_names,
                        undefined_symbols=symbols,
                        has_initializers=False,
                    )
            with patch("tools.core_bundle.digest", return_value=MACOS_PYAV_LIBSHARPYUV_SHA256):
                with self.assertRaisesRegex(ValueError, "load graph changed"):
                    _validate_pyav_rpath_exception(
                        rpaths[0],
                        binary=binary,
                        root=root,
                        dependencies=("@rpath/libsharpyuv.0.1.2.dylib",),
                        rpaths=rpaths,
                        install_names=install_names,
                        undefined_symbols=symbols,
                        has_initializers=False,
                    )
                with self.assertRaisesRegex(ValueError, "install name changed"):
                    _validate_pyav_rpath_exception(
                        rpaths[0],
                        binary=binary,
                        root=root,
                        dependencies=dependencies,
                        rpaths=rpaths,
                        install_names=("@rpath/libsharpyuv.0.1.2.dylib",),
                        undefined_symbols=symbols,
                        has_initializers=False,
                    )
                with self.assertRaisesRegex(ValueError, "dynamic-loader APIs"):
                    _validate_pyav_rpath_exception(
                        rpaths[0],
                        binary=binary,
                        root=root,
                        dependencies=dependencies,
                        rpaths=rpaths,
                        install_names=install_names,
                        undefined_symbols=("_dlopen",),
                        has_initializers=False,
                    )
                with self.assertRaisesRegex(ValueError, "Mach-O initializers"):
                    _validate_pyav_rpath_exception(
                        rpaths[0],
                        binary=binary,
                        root=root,
                        dependencies=dependencies,
                        rpaths=rpaths,
                        install_names=install_names,
                        undefined_symbols=symbols,
                        has_initializers=True,
                    )
                binary.write_bytes(b"@rpath/unexpected.dylib\x00")
                with self.assertRaisesRegex(ValueError, "embedded @rpath names changed"):
                    _validate_pyav_rpath_exception(
                        rpaths[0],
                        binary=binary,
                        root=root,
                        dependencies=dependencies,
                        rpaths=rpaths,
                        install_names=install_names,
                        undefined_symbols=symbols,
                        has_initializers=False,
                    )
                binary.write_bytes(b"locked PyAV libsharpyuv fixture")
                with self.assertRaisesRegex(ValueError, "unreviewed PyAV LC_RPATH command set"):
                    _validate_pyav_rpath_exception(
                        rpaths[0],
                        binary=binary,
                        root=root,
                        dependencies=dependencies,
                        rpaths=(MACOS_PYAV_UPSTREAM_RPATH, MACOS_PYAV_UPSTREAM_RPATH),
                        install_names=install_names,
                        undefined_symbols=symbols,
                        has_initializers=False,
                    )
                with self.assertRaisesRegex(ValueError, "non-system absolute Mach-O load path"):
                    _validate_macho_path(
                        MACOS_PYAV_UPSTREAM_RPATH,
                        binary=binary,
                        root=root,
                        executable_directory=root / "python/bin",
                    )
            other_binary = root / "python/lib/python3.12/site-packages/av/.dylibs/libwebp.dylib"
            other_binary.parent.mkdir(parents=True, exist_ok=True)
            other_binary.write_bytes(b"another dylib")
            with patch("tools.core_bundle.digest", return_value=MACOS_PYAV_LIBSHARPYUV_SHA256):
                with self.assertRaisesRegex(ValueError, "unreviewed Mach-O binary"):
                    _validate_pyav_rpath_exception(
                        rpaths[0],
                        binary=other_binary,
                        root=root,
                        dependencies=dependencies,
                        rpaths=rpaths,
                        install_names=install_names,
                        undefined_symbols=symbols,
                        has_initializers=False,
                    )

    def test_scipy_exact_rpath_exception_pins_file_hash_and_complete_load_surface(self):
        import tempfile
        from unittest.mock import patch

        self.assertEqual(MACOS_SCIPY_FBLAS_LC_RPATHS[0], "@loader_path")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "relocated"
            binary = root / MACOS_SCIPY_FBLAS_PATH
            binary.parent.mkdir(parents=True)
            binary.write_bytes(b"locked SciPy _fblas fixture")
            with patch("tools.core_bundle.digest", return_value=MACOS_SCIPY_FBLAS_SHA256):
                evidence = _validate_scipy_rpath_exception(
                    binary=binary,
                    root=root,
                    dependencies=MACOS_SCIPY_FBLAS_DEPENDENCIES,
                    rpaths=MACOS_SCIPY_FBLAS_LC_RPATHS,
                    install_names=(),
                    undefined_symbols=("_dgemm_", "_malloc"),
                    has_initializers=False,
                )
            self.assertEqual(evidence["binary"], MACOS_SCIPY_FBLAS_PATH)
            self.assertEqual(evidence["binary_sha256"], MACOS_SCIPY_FBLAS_SHA256)
            self.assertEqual(evidence["wheel_sha256"], MACOS_SCIPY_WHEEL_SHA256)
            self.assertEqual(evidence["lc_rpaths"], list(MACOS_SCIPY_FBLAS_LC_RPATHS))
            self.assertEqual(evidence["dependencies"], list(MACOS_SCIPY_FBLAS_DEPENDENCIES))
            self.assertEqual(evidence["dynamic_loader_symbols"], [])
            self.assertFalse(evidence["has_initializers"])

            with patch("tools.core_bundle.digest", return_value="0" * 64):
                with self.assertRaisesRegex(ValueError, "binary digest changed"):
                    _validate_scipy_rpath_exception(
                        binary=binary,
                        root=root,
                        dependencies=MACOS_SCIPY_FBLAS_DEPENDENCIES,
                        rpaths=MACOS_SCIPY_FBLAS_LC_RPATHS,
                        install_names=(),
                        undefined_symbols=(),
                        has_initializers=False,
                    )
            with patch("tools.core_bundle.digest", return_value=MACOS_SCIPY_FBLAS_SHA256):
                with self.assertRaisesRegex(ValueError, "LC_RPATH command set"):
                    _validate_scipy_rpath_exception(
                        binary=binary,
                        root=root,
                        dependencies=MACOS_SCIPY_FBLAS_DEPENDENCIES,
                        rpaths=MACOS_SCIPY_FBLAS_LC_RPATHS[:-1],
                        install_names=(),
                        undefined_symbols=(),
                        has_initializers=False,
                    )
                with self.assertRaisesRegex(ValueError, "load graph changed"):
                    _validate_scipy_rpath_exception(
                        binary=binary,
                        root=root,
                        dependencies=("@rpath/libunexpected.dylib",),
                        rpaths=MACOS_SCIPY_FBLAS_LC_RPATHS,
                        install_names=(),
                        undefined_symbols=(),
                        has_initializers=False,
                    )
                with self.assertRaisesRegex(ValueError, "dynamic-loader APIs"):
                    _validate_scipy_rpath_exception(
                        binary=binary,
                        root=root,
                        dependencies=MACOS_SCIPY_FBLAS_DEPENDENCIES,
                        rpaths=MACOS_SCIPY_FBLAS_LC_RPATHS,
                        install_names=(),
                        undefined_symbols=("_dlopen",),
                        has_initializers=False,
                    )
                with self.assertRaisesRegex(ValueError, "Mach-O initializers"):
                    _validate_scipy_rpath_exception(
                        binary=binary,
                        root=root,
                        dependencies=MACOS_SCIPY_FBLAS_DEPENDENCIES,
                        rpaths=MACOS_SCIPY_FBLAS_LC_RPATHS,
                        install_names=(),
                        undefined_symbols=(),
                        has_initializers=True,
                    )
                with patch("tools.core_bundle._embedded_rpath_candidate_names", return_value=("libexternal.dylib",)):
                    with self.assertRaisesRegex(ValueError, "embedded @rpath names changed"):
                        _validate_scipy_rpath_exception(
                            binary=binary,
                            root=root,
                            dependencies=MACOS_SCIPY_FBLAS_DEPENDENCIES,
                            rpaths=MACOS_SCIPY_FBLAS_LC_RPATHS,
                            install_names=(),
                            undefined_symbols=(),
                            has_initializers=False,
                        )
            with self.assertRaisesRegex(ValueError, "outside the relocated bundle"):
                _validate_scipy_rpath_exception(
                    binary=Path(temporary) / "other.dylib",
                    root=root,
                    dependencies=MACOS_SCIPY_FBLAS_DEPENDENCIES,
                    rpaths=MACOS_SCIPY_UPSTREAM_RPATHS,
                    install_names=(),
                    undefined_symbols=(),
                    has_initializers=False,
                )
        with self.assertRaisesRegex(ValueError, "non-system absolute Mach-O load path"):
            _validate_macho_path(
                MACOS_SCIPY_UPSTREAM_RPATHS[0],
                binary=Path("_fblas.dylib"),
                root=Path("/bundle"),
                executable_directory=Path("/bundle/python/bin"),
            )

    def test_python_archive_filter_contains_a_symlink_chain(self):
        import tempfile

        with tempfile.TemporaryDirectory() as temporary:
            temporary_path = Path(temporary)
            archive_path = temporary_path / "crafted-python.tar.gz"
            with tarfile.open(archive_path, "w:gz") as archive:
                root = tarfile.TarInfo("python")
                root.type = tarfile.DIRTYPE
                root.mode = 0o755
                archive.addfile(root)
                first = tarfile.TarInfo("python/a")
                first.type = tarfile.SYMTYPE
                first.linkname = "."
                archive.addfile(first)
                second = tarfile.TarInfo("python/b")
                second.type = tarfile.SYMTYPE
                second.linkname = "a/../.."
                archive.addfile(second)
                escaped = tarfile.TarInfo("python/b/escaped.txt")
                escaped.size = 6
                archive.addfile(escaped, io.BytesIO(b"escape"))

            extracted = temporary_path / "extracted"
            extract_python_distribution(archive_path, extracted)
            self.assertFalse((temporary_path / "escaped.txt").exists())
            self.assertEqual((extracted / "escaped.txt").read_bytes(), b"escape")

    def test_removes_dependency_entrypoints_with_temporary_interpreter_shebangs(self):
        import tempfile

        with tempfile.TemporaryDirectory() as temporary:
            binary = Path(temporary) / "python/bin"
            binary.mkdir(parents=True)
            interpreter = binary / "python3"
            interpreter.write_bytes(b"python")
            generated = binary / "uvicorn"
            generated.write_text(f"#!{interpreter}\nprint('wrapper')\n", encoding="utf-8")
            generated.chmod(0o755)
            stable = binary / "python3-config"
            stable.write_text("#!/bin/sh\necho config\n", encoding="utf-8")
            stable.chmod(0o755)

            self.assertEqual(remove_build_path_entrypoints(binary, interpreter), 1)
            self.assertFalse(generated.exists())
            self.assertTrue(stable.exists())

    def test_manifest_records_hashes_sizes_and_only_contract_fields(self):
        import tempfile

        with tempfile.TemporaryDirectory() as temporary:
            assets = Path(temporary) / "assets"
            assets.mkdir()
            wheel = assets / "sidevoice_core-0.1.0-py3-none-any.whl"
            wheel.write_bytes(b"wheel payload")
            expected_assets = {}
            for target, _, _ in TARGETS:
                path = assets / f"sidevoice-core-0.1.0-{target}.tar.zst"
                path.write_bytes(f"bundle for {target}".encode())
                expected_assets[path.name] = path

            output = assets / "core-manifest.json"
            manifest = write_manifest(assets, "nightly", output)
            validate_manifest(manifest, assets)
            self.assertEqual(set(manifest), {"bundles", "wheel"})
            self.assertEqual(
                [set(entry) for entry in manifest["bundles"]],
                [{"os", "arch", "url", "sha256", "size"}] * 3,
            )
            self.assertEqual(set(manifest["wheel"]), {"url", "sha256"})
            self.assertEqual(manifest["wheel"]["sha256"], sha256(wheel))
            for entry, (target, os_name, arch) in zip(manifest["bundles"], TARGETS, strict=True):
                path = expected_assets[f"sidevoice-core-0.1.0-{target}.tar.zst"]
                self.assertEqual((entry["os"], entry["arch"]), (os_name, arch))
                self.assertEqual(entry["size"], path.stat().st_size)
                self.assertEqual(entry["sha256"], sha256(path))
                self.assertIn(f"/nightly/{path.name}", entry["url"])

            original = output.read_bytes()
            write_manifest(assets, "nightly", output)
            self.assertEqual(output.read_bytes(), original)
            self.assertEqual(json.loads(original), manifest)

    def test_manifest_rejects_missing_platform_and_shape_drift(self):
        import tempfile

        with tempfile.TemporaryDirectory() as temporary:
            assets = Path(temporary)
            (assets / "sidevoice_core-0.1.0-py3-none-any.whl").write_bytes(b"wheel")
            for target, _, _ in TARGETS[:-1]:
                (assets / f"sidevoice-core-0.1.0-{target}.tar.zst").write_bytes(target.encode())
            with self.assertRaisesRegex(ValueError, "missing bundle"):
                write_manifest(assets, "nightly", assets / "core-manifest.json")

            (assets / "sidevoice-core-0.1.0-linux-aarch64.tar.zst").write_bytes(b"arm bundle")
            manifest = write_manifest(assets, "nightly", assets / "core-manifest.json")
            manifest["bundles"][0]["unexpected"] = True
            with self.assertRaisesRegex(ValueError, "unexpected shape"):
                validate_manifest(manifest, assets)

    def test_archive_contains_runtime_layout_and_survives_a_move(self):
        import tempfile

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "source-root"
            python = root / "python" / "bin" / "python3"
            python.parent.mkdir(parents=True)
            python.write_text(
                "#!/bin/sh\n"
                "[ \"$1 $2 $3 $4 $5\" = \"-I -B -m sidevoice_core.server --self-test\" ] || exit 7\n"
                "printf '{\"ok\": true}\\n'\n",
                encoding="utf-8",
            )
            python.chmod(0o755)
            (root / "python/lib/python3.12/site-packages/sidevoice_core/server").mkdir(parents=True)
            (root / "python/lib/python3.12/site-packages/sidevoice_core/server/__main__.py").write_text("\n")
            (root / "python/lib/python3.12/site-packages/PIL").mkdir(parents=True)
            (root / "python/lib/python3.12/site-packages/PIL/Image.py").write_text("\n")
            (root / "python/lib/python3.12/LICENSE.txt").write_text("PSF license\n")
            for name in ("LICENSE", "LICENSE.python-build-standalone", "TRADEMARKS.md", "DEPENDENCIES.lock.txt", "BUNDLE-NOTICES.md"):
                (root / name).write_text(f"{name}\n")
            (root / "python-build-standalone-licenses").mkdir()
            (root / "python-build-standalone-licenses/LICENSE.libffi.txt").write_text("libffi license\n")

            archive = Path(temporary) / "sidevoice-core-test.tar.zst"
            created = create_archive(root, archive, epoch=123)
            inspected = inspect_archive(archive)
            self.assertEqual(created["compressed_bytes"], archive.stat().st_size)
            self.assertEqual(inspected["compressed_bytes"], archive.stat().st_size)
            self.assertGreater(inspected["unpacked_bytes"], 0)
            from unittest.mock import patch

            with patch("tools.core_bundle._verify_pillow_codec_runtime", return_value={"pillow_codecs": []}):
                verified = verify_relocation(archive, build_paths=(root,))
            self.assertEqual(verified["compressed_bytes"], archive.stat().st_size)

    def test_archive_rejects_this_build_path_but_allows_upstream_native_source_strings(self):
        import tempfile

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "source-root"
            python = root / "python/bin/python3"
            python.parent.mkdir(parents=True)
            python.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            python.chmod(0o755)
            (root / "python/lib/python3.12/site-packages/sidevoice_core/server").mkdir(parents=True)
            (root / "python/lib/python3.12/site-packages/sidevoice_core/server/__main__.py").write_text("\n")
            (root / "python/lib/python3.12/site-packages/PIL").mkdir(parents=True)
            (root / "python/lib/python3.12/site-packages/PIL/Image.py").write_text("\n")
            (root / "python/lib/python3.12/LICENSE.txt").write_text("PSF license\n")
            for name in ("LICENSE", "LICENSE.python-build-standalone", "TRADEMARKS.md", "DEPENDENCIES.lock.txt", "BUNDLE-NOTICES.md"):
                (root / name).write_text(f"{name}\n")
            (root / "python-build-standalone-licenses").mkdir()
            (root / "python-build-standalone-licenses/LICENSE.libffi.txt").write_text("libffi license\n")
            native_binary = root / "python/lib/python3.12/site-packages/av/_core.abi3.so"
            native_binary.parent.mkdir(parents=True, exist_ok=True)
            native_binary.write_bytes(b"Mach-O fixture: /Users/runner/work/PyAV/PyAV/src/av/core.py\0")
            archive = Path(temporary) / "leaked.tar.zst"
            create_archive(root, archive)
            self.assertGreater(inspect_archive(archive, forbidden_paths=(root,))["files"], 0)

            (root / "leaked.txt").write_text(f"this build checkout: {root}", encoding="utf-8")
            create_archive(root, archive)
            with self.assertRaisesRegex(ValueError, r"leaked.txt contains build path"):
                inspect_archive(archive, forbidden_paths=(root, Path(temporary) / "runner-temp"))

            (root / "leaked.txt").write_text(
                f"this temporary build root: {Path(temporary) / 'runner-temp'}", encoding="utf-8"
            )
            create_archive(root, archive)
            with self.assertRaisesRegex(ValueError, r"leaked.txt contains build path"):
                inspect_archive(archive, forbidden_paths=(root, Path(temporary) / "runner-temp"))
            with self.assertRaisesRegex(ValueError, "unsafe archive member"):
                safe_member_path("python/../../outside")
            with self.assertRaisesRegex(ValueError, "escapes its root"):
                safe_link_target(Path("python/bin/python3"), "../../../outside", hardlink=False)


if __name__ == "__main__":
    unittest.main()
