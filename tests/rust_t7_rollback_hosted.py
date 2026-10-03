"""Isolated same-state Python/Rust start and failed-native-stage rollback proof."""

import http.client
import json
import os
import signal
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from sidevoice_core.control.devices import DeviceRegistry, NodeIdentity


# A venv interpreter is commonly a symlink to its base executable; resolving
# that symlink would silently drop the venv's installed packages.
PYTHON = Path(sys.argv[1]).absolute()
RUST = Path(sys.argv[2]).resolve()
ARCHIVE = Path(sys.argv[3]).resolve()
VERIFY = Path(sys.argv[4]).resolve()


class UnixHTTP(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=8)
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.connect(str(self.path))


def request(socket_path, route, token=None, port=None):
    connection = (http.client.HTTPConnection("127.0.0.1", port, timeout=8)
                  if port else UnixHTTP(socket_path))
    headers = {"Host": "localhost"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    connection.request("GET", route, headers=headers)
    answer = connection.getresponse()
    payload = json.loads(answer.read())
    connection.close()
    return answer.status, payload


def started(command, data, launch_id, env):
    process = subprocess.Popen([*command, "--data-dir", str(data), "--port", "0",
                                "--launch-id", launch_id, "--idle-exit", "0"],
                               env=env, stdin=subprocess.DEVNULL,
                               stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
    end = time.monotonic() + 50
    while time.monotonic() < end:
        if process.poll() is not None:
            raise AssertionError(f"Core exited {process.returncode}: {process.stderr.read()[-3000:]}")
        if (data / "core.json").exists():
            ready = json.loads((data / "core.json").read_text())
            if ready["launch_id"] == launch_id:
                return process, ready
        time.sleep(0.1)
    process.terminate()
    raise AssertionError("Core startup timed out")


def stopped(process, data):
    process.send_signal(signal.SIGTERM)
    assert process.wait(timeout=15) == 0, process.stderr.read()[-3000:]
    assert not (data / "core.json").exists()
    assert not (data / "local.sock").exists()


def check_state(data, ready, expected_fingerprint, live_token, revoked_token, launch_id):
    socket_path = data / "local.sock"
    status, health = request(socket_path, "/api/local/health")
    assert status == 200 and health["launch_id"] == launch_id
    assert health["fingerprint"] == expected_fingerprint
    assert request(socket_path, "/api/device/devices", live_token, ready["port"])[0] == 200
    assert request(socket_path, "/api/device/devices", revoked_token, ready["port"])[0] == 401


def main():
    with tempfile.TemporaryDirectory(prefix="sidevoice-t7-rollback-") as temporary:
        root = Path(temporary)
        data = root / "state"
        data.mkdir(mode=0o700)
        fingerprint = NodeIdentity.load_or_create(data / "node-identity.json").fingerprint
        registry = DeviceRegistry(data / "devices.json")
        live_id, live_token = registry.redeem(registry.issue_secret()[0], "Live fixture")
        revoked_id, revoked_token = registry.redeem(registry.issue_secret()[0], "Revoked fixture")
        assert registry.revoke(revoked_id)
        assert registry.authenticate(live_token) == live_id
        assert registry.authenticate(revoked_token) is None
        identity_bytes = (data / "node-identity.json").read_bytes()
        python_env = {**os.environ, "PYTHONPATH": str(Path.cwd() / "src"),
                      "SIDEVOICE_STUN_URLS": ""}
        rust_env = {**os.environ, "RUSTVANI_CACHE_DIR": str(RUST.parent.parent / "models"),
                    "SIDEVOICE_STUN_URLS": ""}
        rust_env.pop("ORT_DYLIB_PATH", None)

        original, ready = started([str(PYTHON), "-m", "sidevoice_core.server"],
                                  data, "t7-python-before", python_env)
        try:
            check_state(data, ready, fingerprint, live_token, revoked_token, "t7-python-before")
        finally:
            stopped(original, data)

        candidate, ready = started([str(RUST)], data, "t7-rust-candidate", rust_env)
        try:
            check_state(data, ready, fingerprint, live_token, revoked_token, "t7-rust-candidate")
        finally:
            stopped(candidate, data)
        assert (data / "node-identity.json").read_bytes() == identity_bytes
        devices_bytes = (data / "devices.json").read_bytes()

        staged = root / "bad-candidate.tar.zst"
        payload = bytearray(ARCHIVE.read_bytes())
        payload[len(payload) // 2] ^= 0xFF
        staged.write_bytes(payload)
        checked = subprocess.run([str(PYTHON), str(VERIFY), "verify", "--archive", str(staged)],
                                 capture_output=True, text=True, timeout=30)
        assert checked.returncode != 0, "Corrupted candidate was accepted"
        assert (data / "node-identity.json").read_bytes() == identity_bytes
        assert (data / "devices.json").read_bytes() == devices_bytes

        restored, ready = started([str(PYTHON), "-m", "sidevoice_core.server"],
                                  data, "t7-python-rollback", python_env)
        try:
            check_state(data, ready, fingerprint, live_token, revoked_token, "t7-python-rollback")
        finally:
            stopped(restored, data)
        print(json.dumps({"same_state": True, "identity_retained": True,
                          "live_device_retained": True, "revocation_retained": True,
                          "bad_native_stage_rejected": True,
                          "prior_python_restarted": True}, sort_keys=True))


if __name__ == "__main__":
    main()
