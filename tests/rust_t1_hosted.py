"""Hosted T1 contract check against disposable Python-created state and the Rust process."""

import asyncio
import base64
import hashlib
import http.client
import json
import os
import signal
import socket
import sqlite3
import stat
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import websockets
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.hazmat.primitives.asymmetric.utils import encode_dss_signature
from cryptography.exceptions import InvalidSignature

from sidevoice_core.control.devices import DeviceRegistry, NodeIdentity
from sidevoice_core.control.history import RoomHistory
from sidevoice_core.control.connectors import local_credential


BINARY = Path(sys.argv[1]).resolve()


class UnixHTTP(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost")
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(str(self.path))


def request(port, method, path, *, token=None, origin=None, host=None, body=None, unix=None, extra_headers=None):
    connection = UnixHTTP(unix) if unix else http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    headers = {"Host": host or "localhost", "Content-Type": "application/json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    if origin:
        headers["Origin"] = origin
    if extra_headers:
        headers.update(extra_headers)
    connection.request(method, path, json.dumps(body) if body is not None else None, headers)
    answer = connection.getresponse()
    content = answer.read()
    result = (answer.status, json.loads(content) if content else None, dict(answer.getheaders()))
    connection.close()
    return result


def wait_ready(path, process, timeout=30):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if process.poll() is not None:
            raise AssertionError(f"core stopped before ready: {process.returncode}; {process.stderr.read()}")
        try:
            return json.loads(path.read_text())
        except (OSError, ValueError):
            time.sleep(0.05)
    raise AssertionError("core did not become ready")


def start(data, *extra):
    return subprocess.Popen([str(BINARY), "--data-dir", str(data), "--port", "0", "--idle-exit", "0", *extra],
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)


def mode(path):
    return stat.S_IMODE(path.stat().st_mode)


async def closed_by_revocation(port, first, second):
    async with websockets.connect(f"ws://127.0.0.1:{port}/api/presentation/ws",
                                  subprotocols=["sidevoice", f"sidevoice.token.{first}"]) as connection:
        assert connection.subprotocol == "sidevoice"
        assert request(port, "GET", "/api/device/devices", token=second)[0] == 200
        listing = request(port, "GET", "/api/device/devices", token=second)[1]["devices"]
        first_id = next(row["id"] for row in listing if row["current"] is False)
        assert request(port, "DELETE", f"/api/device/devices/{first_id}", token=second)[0] == 200
        try:
            await asyncio.wait_for(connection.recv(), 5)
        except websockets.exceptions.ConnectionClosed as error:
            assert error.code == 4401, error
        else:
            raise AssertionError("revoked socket stayed open")
    return first_id


def prove_identity(port, fingerprint, public_key):
    nonce = base64.urlsafe_b64encode(os.urandom(32)).rstrip(b"=").decode()
    status, proof, _ = request(port, "GET", f"/api/device/identity?nonce={nonce}")
    assert status == 200
    assert proof["fingerprint"] == fingerprint and proof["public_key"] == public_key
    der = base64.b64decode(public_key)
    assert fingerprint == base64.urlsafe_b64encode(hashlib.sha256(der).digest()).rstrip(b"=").decode()
    raw = base64.urlsafe_b64decode(proof["signature"] + "==")
    assert len(raw) == 64
    signature = encode_dss_signature(int.from_bytes(raw[:32], "big"), int.from_bytes(raw[32:], "big"))
    serialization.load_der_public_key(der).verify(signature, f"sidevoice-node-identity:{nonce}".encode(), ec.ECDSA(hashes.SHA256()))
    try:
        serialization.load_der_public_key(der).verify(signature, b"sidevoice-node-identity:different", ec.ECDSA(hashes.SHA256()))
    except InvalidSignature:
        pass
    else:
        raise AssertionError("signature verified a different nonce")
    assert request(port, "GET", "/api/device/identity?nonce=bad")[0] == 400


def main():
    with tempfile.TemporaryDirectory() as root:
        root = Path(root)
        data = root / "core"
        data.mkdir(mode=0o700)
        python_identity = NodeIdentity.load_or_create(data / "node-identity.json")
        registry = DeviceRegistry(data / "devices.json")
        old_id, old_token = registry.redeem(registry.issue_secret()[0], "Python device")
        journal = RoomHistory(data / "room-state.json")
        connector = local_credential(journal, data / "connector-credential.json")
        before_room = json.loads((data / "room-state.json").read_text())

        process = start(data, "--launch-id", "t1-synthetic")
        try:
            ready = wait_ready(data / "core.json", process)
            port = ready["port"]
            assert ready["launch_id"] == "t1-synthetic" and ready["api"] == 1
            assert ready["protocol"] == 2 and ready["connector_protocols"] == [2, 3]
            assert (ready["connector_id"], ready["token"]) == connector
            assert ready["socket"] == str(data / "local.sock")
            assert mode(data) == 0o700
            for name in ("node-identity.json", "devices.json", "room-state.json", "connector-credential.json", "core.json", "local.sock"):
                assert mode(data / name) == 0o600, name
            assert json.loads((data / "room-state.json").read_text()) == before_room
            other_user = subprocess.run(["runuser", "-u", "nobody", "--", sys.executable, "-c",
                "import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1])", str(data / "local.sock")],
                capture_output=True, text=True)
            assert other_user.returncode != 0, "a different OS user opened the local socket"
            other_read = subprocess.run(["runuser", "-u", "nobody", "--", "cat", str(data / "node-identity.json")],
                                        capture_output=True, text=True)
            assert other_read.returncode != 0, "a different OS user read the private key"
            prove_identity(port, python_identity.fingerprint, python_identity.public_key)
            assert request(port, "GET", "/api/rendezvous")[1] == {"kind": "node", "fingerprint": python_identity.fingerprint, "api": 1}
            unauthenticated = request(port, "GET", "/api/device/devices")
            assert unauthenticated[0] == 401 and set(unauthenticated[1]) == {"detail"}
            assert unauthenticated[2]["www-authenticate"] == "Bearer"
            assert request(port, "GET", "/api/device/devices", token=old_token)[0] == 200
            for status, method, path, options in (
                (421, "GET", "/api/device/devices", {"token": old_token, "host": "attacker.example"}),
                (403, "GET", "/api/device/devices", {"token": old_token, "origin": "https://attacker.example"}),
                (403, "POST", "/api/device/pair", {"body": {"secret": "guessed"}}),
                (404, "POST", "/api/device/local/pair", {"body": {"name": "x"}}),
                (404, "GET", "/api/local/health", {}),
            ):
                actual = request(port, method, path, **options)
                assert actual[0] == status and set(actual[1]) == {"detail"}, (path, actual)
            spoofed_local = {"Sidevoice.Local": "true", "X-Sidevoice-Local": "true",
                             "X-Forwarded-For": "127.0.0.1", "X-Forwarded-Proto": "http"}
            for method, path, body in (("GET", "/api/local/health", None),
                                       ("POST", "/api/device/local/pair", {"name": "spoofed"}),
                                       ("DELETE", "/api/device/local", None)):
                status, payload, _ = request(port, method, path, body=body, extra_headers=spoofed_local)
                assert status == 404 and set(payload) == {"detail"}, (path, status, payload)
            assert request(port, "GET", "/api/local/health", unix=data / "local.sock")[1]["fingerprint"] == python_identity.fingerprint
            status, local, _ = request(port, "POST", "/api/device/local/pair", unix=data / "local.sock", body={"name": "Local app"})
            assert status == 200 and local["node"]["fingerprint"] == python_identity.fingerprint
            assert DeviceRegistry(data / "devices.json").authenticate(local["token"]) == local["device_id"]
            assert request(port, "GET", "/api/device/devices", token=local["token"])[0] == 200
            assert request(port, "GET", "/api/device/local/pair", unix=data / "local.sock", origin="tauri://localhost")[0] == 404
            removed = asyncio.run(closed_by_revocation(port, old_token, local["token"]))
            assert removed == old_id
            assert request(port, "GET", "/api/device/devices", token=old_token)[0] == 401
            assert request(port, "DELETE", "/api/device/local", unix=data / "local.sock")[1] == {"ok": True, "revoked": True}
            assert request(port, "GET", "/api/device/devices", token=local["token"])[0] == 401
            second = start(data, "--launch-id", "locked")
            assert second.wait(timeout=10) == 75
            assert json.loads((data / "core-failure.json").read_text())["key"] == "bind.core-running"
            assert json.loads((data / "core.json").read_text())["pid"] == process.pid
            assert request(port, "GET", "/api/rendezvous")[0] == 200
        finally:
            process.send_signal(signal.SIGTERM)
            assert process.wait(timeout=15) == 0, process.stderr.read()
        assert not (data / "core.json").exists() and not (data / "local.sock").exists()
        assert DeviceRegistry(data / "devices.json").devices == {}
        assert NodeIdentity.load_or_create(data / "node-identity.json").fingerprint == python_identity.fingerprint
        assert json.loads((data / "room-state.json").read_text()) == before_room
        assert not list(data.glob(".*.tmp"))

        malformed = root / "malformed"
        malformed.mkdir(mode=0o700)
        identity_file = malformed / "node-identity.json"
        identity_file.write_text('{"private_key_pem":"not a key"}')
        rejected = start(malformed)
        assert rejected.wait(timeout=10) == 0
        assert json.loads((malformed / "core-failure.json").read_text())["key"] == "identity.unreadable"
        assert identity_file.read_text() == '{"private_key_pem":"not a key"}'

        unsafe = root / "unsafe"
        unsafe.mkdir(mode=0o755)
        refused = start(unsafe)
        assert refused.wait(timeout=10) == 0
        assert json.loads((unsafe / "core-failure.json").read_text())["key"] == "identity.unsafe-directory"
        assert not (unsafe / "node-identity.json").exists()

        fresh = root / "fresh"
        newly_started = start(fresh)
        fresh_ready = wait_ready(fresh / "core.json", newly_started)
        rust_fingerprint = request(fresh_ready["port"], "GET", "/api/local/health", unix=fresh / "local.sock")[1]["fingerprint"]
        newly_started.terminate()
        assert newly_started.wait(timeout=15) == 0
        assert (fresh / "node-identity.json").exists()
        rust_identity = NodeIdentity.load_or_create(fresh / "node-identity.json")
        assert rust_identity.fingerprint == rust_fingerprint
        assert mode(fresh / "node-identity.json") == 0o600

        legacy = root / "legacy"
        legacy.mkdir(mode=0o700)
        db = sqlite3.connect(legacy / "room-history.sqlite3")
        db.execute("CREATE TABLE connectors (id TEXT, token_hash TEXT, host TEXT, created INTEGER, last_seen INTEGER, revoked INTEGER)")
        db.execute("INSERT INTO connectors VALUES (?, ?, ?, ?, ?, ?)",
                   ("legacy-id", hashlib.sha256(b"legacy-token").hexdigest(), "old-machine", 1, 2, 0))
        db.commit()
        db.close()
        imported = start(legacy)
        wait_ready(legacy / "core.json", imported)
        imported.terminate()
        assert imported.wait(timeout=15) == 0
        kept = RoomHistory(legacy / "room-state.json").connectors["legacy-id"]
        assert kept["host"] == "old-machine" and kept["token_hash"] == hashlib.sha256(b"legacy-token").hexdigest()
        print(json.dumps({"ok": True, "python_to_rust_to_python": True, "identity": python_identity.fingerprint,
                          "lock": "contended", "revoke": "closed_4401"}))


if __name__ == "__main__":
    main()
