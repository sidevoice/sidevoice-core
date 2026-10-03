"""Build and verify the closed T7 native Core archive on its hosted target."""

import argparse
import hashlib
import http.client
import json
import os
import re
import shutil
import signal
import socket
import subprocess
import tarfile
import tempfile
import time
import urllib.request
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
TARGETS = ("linux-aarch64", "linux-x86_64", "macos-aarch64")
ENTRYPOINT = "bin/sidevoice-core-rust"
LICENSES = {
    "onnxruntime-license.txt": (
        "https://raw.githubusercontent.com/microsoft/onnxruntime/v1.22.0/LICENSE",
        "2f07c72751aed99790b8a4869cf2311df85a860b22ded05fa22803587a48922c"),
    "silero-vad-license.txt": (
        "https://raw.githubusercontent.com/snakers4/silero-vad/master/LICENSE",
        "2e63e9a38b6e8fc0c7bc37ce174caca1862870856c6daf5697cfb785e925520b"),
    "smart-turn-license.txt": (
        "https://raw.githubusercontent.com/pipecat-ai/smart-turn/main/LICENSE",
        "0d66364067f678c08586ebb60a16a2aed4fa081ec11057df35585759ce0e774f"),
    "opus-license.txt": (
        "https://raw.githubusercontent.com/xiph/opus/v1.5.2/COPYING",
        "01e1167d54a096d123cf6dfbbeb19587278845c6481d2d66d545669846079551"),
}


def digest(data):
    return hashlib.sha256(data).hexdigest()


def file_record(path, name):
    payload = path.read_bytes()
    return {"name": name, "size": len(payload), "sha256": digest(payload)}


def canonical(value):
    return (json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False) + "\n").encode()


def exact_json(payload):
    def unique(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError(f"Duplicate JSON key: {key}")
            result[key] = value
        return result

    value = json.loads(payload, object_pairs_hook=unique)
    if canonical(value) != payload:
        raise ValueError("Noncanonical JSON")
    return value


def run(*args, **kwargs):
    return subprocess.run(args, check=True, text=True, **kwargs)


def validate_name(name):
    parts = name.split("/")
    if not name or any(part in ("", ".", "..") for part in parts):
        raise ValueError(f"Unsafe member path: {name}")
    if not name.startswith("sidevoice-core-rust/"):
        raise ValueError(f"Wrong archive root: {name}")


def stage_notices(notices):
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--format-version", "1"], cwd=ROOT))
    packages = sorted(({"name": package["name"], "version": package["version"],
                        "license": package.get("license"), "source": package.get("source")}
                       for package in metadata["packages"]),
                      key=lambda package: (package["name"], package["version"]))
    (notices / "rust-dependencies.json").write_bytes(canonical(packages))
    rustvani = next(package for package in metadata["packages"] if package["name"] == "rustvani")
    source = Path(rustvani["manifest_path"]).parent
    for original, destination in (("LICENSE", "rustvani-license.txt"),
                                  ("THIRD_PARTY_NOTICES.md", "rustvani-third-party.md")):
        shutil.copyfile(source / original, notices / destination)
    for name, (url, expected) in LICENSES.items():
        with urllib.request.urlopen(url, timeout=30) as response:
            payload = response.read()
        if digest(payload) != expected:
            raise ValueError(f"Pinned notice digest changed: {name}")
        (notices / name).write_bytes(payload)
    (notices / "sources.json").write_bytes(canonical({
        "ort": "onnxruntime 1.22.0 selected by ort-sys 2.0.0-rc.10",
        "rustvani": "d01f33e671f7a4d8a128e7bfe55dbf0e8963cb21",
        "models": json.loads((ROOT / "assets/rust-models.json").read_text()),
        "license_sha256": {name: sha for name, (_, sha) in LICENSES.items()},
    }))


def linux_libraries(binary, library_dir):
    output = subprocess.check_output(["ldd", str(binary)], text=True)
    needed = {}
    for line in output.splitlines():
        match = re.match(r"\s*(\S+)\s+=>\s+(\S+)", line)
        if match:
            needed[match.group(1)] = match.group(2)
    for name in ("libopus.so.0",):
        if name not in needed or not Path(needed[name]).is_file():
            raise ValueError(f"Missing native dependency {name}: {output}")
    for name, source in needed.items():
        if name.startswith(("libopus.so", "libonnxruntime.so", "libssl.so", "libcrypto.so")):
            if not Path(source).is_file():
                raise ValueError(f"Unresolved native dependency {name}: {source}")
            shutil.copyfile(Path(source).resolve(), library_dir / name)
    run("patchelf", "--set-rpath", "$ORIGIN/../lib", str(binary))
    for library in library_dir.iterdir():
        run("patchelf", "--set-rpath", "$ORIGIN", str(library))
    return output


def mac_libraries(binary, library_dir):
    output = subprocess.check_output(["otool", "-L", str(binary)], text=True)
    libraries = [line.strip().split(" (", 1)[0] for line in output.splitlines()[1:]]
    opus = [name for name in libraries if "libopus" in name]
    if len(opus) > 1:
        raise ValueError(f"Unexpected multiple Opus dependencies: {output}")
    for original in libraries:
        if "libopus" not in original and "libonnxruntime" not in original:
            continue
        source = Path(original)
        if not source.exists():
            candidate = binary.parent / source.name
            if not candidate.exists():
                raise ValueError(f"Missing native library {original}")
            source = candidate
        name = "libopus.0.dylib" if "libopus" in original else "libonnxruntime.dylib"
        destination = library_dir / name
        shutil.copyfile(source.resolve(), destination)
        run("install_name_tool", "-change", original,
            f"@executable_path/../lib/{name}", str(binary))
        run("install_name_tool", "-id", f"@loader_path/{name}", str(destination))
        run("codesign", "--force", "--sign", "-", str(destination))
    run("codesign", "--force", "--sign", "-", str(binary))
    return output


def build(args):
    if args.target not in TARGETS:
        raise ValueError("Unsupported target")
    if not re.fullmatch(r"[0-9a-f]{40}", args.source_sha):
        raise ValueError("Invalid source commit")
    output = Path(args.output).resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="rust-native-build-") as temporary:
        stage = Path(temporary) / "sidevoice-core-rust"
        for name in ("bin", "lib", "models", "checks", "notices"):
            (stage / name).mkdir(parents=True)
        binary = stage / ENTRYPOINT
        shutil.copyfile(args.binary, binary)
        binary.chmod(0o755)
        models = json.loads((ROOT / "assets/rust-models.json").read_text())
        for model in models["models"]:
            original = Path(args.models) / model["name"]
            if digest(original.read_bytes()) != model["sha256"]:
                raise ValueError(f"Model digest mismatch: {model['name']}")
            shutil.copyfile(original, stage / "models" / model["name"])
        shutil.copyfile(ROOT / "tests/fixtures/hola-sala-16k.wav",
                        stage / "checks/detector-16k.wav")
        stage_notices(stage / "notices")
        if args.target.startswith("linux-"):
            links = linux_libraries(binary, stage / "lib")
        else:
            links = mac_libraries(binary, stage / "lib")
        if not any((stage / "lib").iterdir()):
            (stage / "lib").rmdir()
        records = [file_record(path, path.relative_to(stage).as_posix())
                   for path in sorted(stage.rglob("*")) if path.is_file()]
        inventory = {"schema": 1, "kind": "rust-native-v1", "target": args.target,
                     "source_sha": args.source_sha, "entrypoint": ENTRYPOINT, "files": records}
        (stage / "native-core.json").write_bytes(canonical(inventory))
        raw_tar = Path(temporary) / "bundle.tar"
        with tarfile.open(raw_tar, "w", format=tarfile.PAX_FORMAT) as archive:
            for path in [stage, *sorted(stage.rglob("*"))]:
                relative = path.relative_to(stage.parent).as_posix()
                info = tarfile.TarInfo(relative + ("/" if path.is_dir() else ""))
                info.uid = info.gid = 0
                info.uname = info.gname = ""
                info.mtime = args.epoch
                if path.is_dir():
                    info.type = tarfile.DIRTYPE
                    info.mode = 0o755
                    archive.addfile(info)
                else:
                    info.size = path.stat().st_size
                    info.mode = 0o755 if path == binary else 0o644
                    with path.open("rb") as source:
                        archive.addfile(info, source)
        with output.open("wb") as destination:
            run("zstd", "-q", "-19", "-c", str(raw_tar), stdout=destination)
        print(json.dumps({"target": args.target, "name": output.name,
                          "size": output.stat().st_size, "sha256": digest(output.read_bytes()),
                          "files": len(records), "links_before_relocation": links}, sort_keys=True))


def unpack_checked(archive_path, destination):
    raw_tar = destination / "bundle.tar"
    with raw_tar.open("wb") as output:
        run("zstd", "-q", "-d", "-c", str(archive_path), stdout=output)
    seen = set()
    directories = set()
    with tarfile.open(raw_tar, "r") as archive:
        for member in archive:
            name = member.name.removesuffix("/")
            if name != "sidevoice-core-rust":
                validate_name(name)
            elif not member.isdir():
                raise ValueError("Native root must be a directory")
            if name in seen or not (member.isdir() or member.isfile()):
                raise ValueError(f"Duplicate or forbidden tar entry: {name}")
            seen.add(name)
            target = destination / name
            if member.isdir():
                directories.add(name)
                target.mkdir(parents=True, exist_ok=True)
            else:
                target.parent.mkdir(parents=True, exist_ok=True)
                with target.open("wb") as output:
                    shutil.copyfileobj(archive.extractfile(member), output)
                target.chmod(member.mode & 0o777)
    raw_tar.unlink()
    root = destination / "sidevoice-core-rust"
    inventory = exact_json((root / "native-core.json").read_bytes())
    if set(inventory) != {"schema", "kind", "target", "source_sha", "entrypoint", "files"}:
        raise ValueError("Wrong inventory fields")
    if inventory["schema"] != 1 or inventory["kind"] != "rust-native-v1" or inventory["entrypoint"] != ENTRYPOINT:
        raise ValueError("Wrong native kind or launcher")
    if inventory["target"] not in TARGETS or not re.fullmatch(r"[0-9a-f]{40}", inventory["source_sha"]):
        raise ValueError("Wrong target or source commit")
    files = inventory["files"]
    names = [record["name"] for record in files]
    if names != sorted(set(names)):
        raise ValueError("Unsorted or duplicated inventory")
    actual = {path.relative_to(root).as_posix() for path in root.rglob("*") if path.is_file()}
    if actual != set(names) | {"native-core.json"}:
        raise ValueError(f"Unexpected or missing files: {actual ^ (set(names) | {'native-core.json'})}")
    expected_directories = {"sidevoice-core-rust"}
    for name in actual:
        parts = name.split("/")
        expected_directories.update("sidevoice-core-rust/" + "/".join(parts[:index])
                                    for index in range(1, len(parts)))
    if directories != expected_directories:
        raise ValueError(f"Unexpected or missing directories: {directories ^ expected_directories}")
    for record in files:
        validate_name("sidevoice-core-rust/" + record["name"])
        if set(record) != {"name", "size", "sha256"} or record != file_record(root / record["name"], record["name"]):
            raise ValueError(f"Inventory mismatch: {record['name']}")
    return root, inventory


class UnixHTTP(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=5)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.connect(str(self.path))


def verify(args):
    archive = Path(args.archive).resolve()
    with tempfile.TemporaryDirectory(prefix="T7 relocated tree with spaces ") as temporary:
        destination = Path(temporary)
        root, inventory = unpack_checked(archive, destination)
        binary = root / ENTRYPOINT
        env = {**os.environ, "RUSTVANI_CACHE_DIR": str(root / "models"), "SIDEVOICE_STUN_URLS": ""}
        env.pop("ORT_DYLIB_PATH", None)
        detector = run(str(binary), "--self-test", str(root / "checks/detector-16k.wav"),
                       str(root / "models"), env=env, capture_output=True)
        self_test = json.loads(detector.stdout)
        readout = self_test["detectors"]
        if readout["sample_rate"] != 16000 or readout["max_voice_confidence"] <= 0.5 or not readout["smart_turn_complete"]:
            raise ValueError(f"Detector failed: {readout}")
        if self_test["opus_decoded_samples"] != 320:
            raise ValueError("Opus decode self-test failed")
        data = destination / "private state"
        data.mkdir(mode=0o700)
        launch_id = "t7-relocated-native"
        process = subprocess.Popen([str(binary), "--data-dir", str(data), "--port", "0",
                                    "--launch-id", launch_id, "--idle-exit", "0"],
                                   stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True, env=env)
        try:
            end = time.monotonic() + 40
            while time.monotonic() < end and not (data / "core.json").exists():
                if process.poll() is not None:
                    raise ValueError(f"Core exited: {process.stderr.read()[-3000:]}")
                time.sleep(0.1)
            ready = json.loads((data / "core.json").read_text())
            connection = UnixHTTP(data / "local.sock")
            connection.request("GET", "/api/local/health", headers={"Host": "localhost"})
            response = connection.getresponse()
            health = json.loads(response.read())
            connection.close()
            if response.status != 200 or ready["launch_id"] != launch_id or health["launch_id"] != launch_id:
                raise ValueError("Ready/health launch identity mismatch")
            if ready["pid"] != process.pid or health["pid"] != process.pid or not health["fingerprint"]:
                raise ValueError("Ready/health process or node identity mismatch")
        finally:
            process.send_signal(signal.SIGTERM)
            if process.wait(timeout=15) != 0:
                raise ValueError(f"Core shutdown failed: {process.stderr.read()[-3000:]}")
        if (data / "core.json").exists() or (data / "local.sock").exists():
            raise ValueError("Core left ready/socket after shutdown")
        print(json.dumps({"target": inventory["target"], "source_sha": inventory["source_sha"],
                          "detectors": readout, "opus_decoded_samples": self_test["opus_decoded_samples"],
                          "launch_id": launch_id, "fingerprint": health["fingerprint"],
                          "files": len(inventory["files"]), "relocation": True}, sort_keys=True))


def manifest(args):
    folder = Path(args.assets)
    bundles = {}
    for target in TARGETS:
        name = f"sidevoice-core-rust-{args.source_sha}-{target}.tar.zst"
        path = folder / name
        bundles[target] = file_record(path, name)
    value = {"schema": 1, "kind": "rust-native-v1", "source_sha": args.source_sha,
             "cargo_lock_sha256": digest((ROOT / "Cargo.lock").read_bytes()),
             "entrypoint": ENTRYPOINT, "bundles": bundles}
    (folder / "native-core-manifest.json").write_bytes(canonical(value))
    print(json.dumps(value, sort_keys=True))


def main():
    parser = argparse.ArgumentParser()
    commands = parser.add_subparsers(dest="command", required=True)
    build_parser = commands.add_parser("build")
    for flag in ("target", "binary", "models", "output", "source-sha", "epoch"):
        build_parser.add_argument("--" + flag, required=True)
    verify_parser = commands.add_parser("verify")
    verify_parser.add_argument("--archive", required=True)
    manifest_parser = commands.add_parser("manifest")
    manifest_parser.add_argument("--assets", required=True)
    manifest_parser.add_argument("--source-sha", required=True)
    args = parser.parse_args()
    if args.command == "build":
        args.epoch = int(args.epoch)
        build(args)
    elif args.command == "verify":
        verify(args)
    else:
        manifest(args)


if __name__ == "__main__":
    main()
