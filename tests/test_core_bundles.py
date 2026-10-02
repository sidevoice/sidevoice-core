"""Contract tests for release manifest generation and archive relocation."""

import json
import io
import tarfile
import unittest
from pathlib import Path
from urllib.parse import quote, unquote, urlsplit

from tools.core_bundle import (
    _parse_otool_dependencies,
    _parse_otool_rpaths,
    _validate_macho_path,
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
