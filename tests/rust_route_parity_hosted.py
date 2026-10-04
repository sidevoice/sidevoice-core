"""Offline first-call preflight against the packaged web pin and frozen Python defaults."""

import asyncio
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

import websockets

CORE = Path(sys.argv[1]).resolve()
WEB = Path(sys.argv[2]).resolve()
assert CORE.is_file() and WEB.is_file(), "required Core binary or pinned web source missing"
source = WEB.read_text(encoding="utf-8")
assert "await api('/api/presentation/languages')" in source
assert "state.voicePreferences=await callPreferences()" in source


class UnixHTTP(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=5)
        self.path = str(path)

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(self.path)


def request(port, method, path, *, token=None, unix=None, body=None, origin=None):
    connection = UnixHTTP(unix) if unix else http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    headers = {"Host": "localhost", "Content-Type": "application/json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    if origin:
        headers["Origin"] = origin
    connection.request(method, path, json.dumps(body) if body is not None else None, headers)
    response = connection.getresponse()
    raw = response.read()
    connection.close()
    return response.status, raw


def ready(path, process):
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise AssertionError(f"Core exited {process.returncode}: {process.stderr.read()}")
        try:
            return json.loads(path.read_text())
        except (OSError, ValueError):
            time.sleep(0.05)
    raise AssertionError("Core did not become ready")


async def main():
    from sidevoice_core.pipeline.settings import load_settings

    expected = load_settings().model_dump()
    with tempfile.TemporaryDirectory(prefix="sidevoice-route-parity-") as temporary:
        data = Path(temporary) / "core"
        data.mkdir(mode=0o700)
        secret = "stored-openai-fixture-key"
        (data / "integrations.json").write_text(json.dumps({"openai": secret}), encoding="utf-8")
        (data / "integrations.json").chmod(0o600)
        env = {**os.environ, "VOICE_ELEVENLABS_API_KEY": "environment-elevenlabs-fixture-key"}
        core = subprocess.Popen([str(CORE), "--data-dir", str(data), "--port", "0", "--idle-exit", "0"],
                                stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True, env=env)
        try:
            info = ready(data / "core.json", core)
            port = info["port"]
            assert request(port, "GET", "/api/presentation/languages")[0] == 401
            assert request(port, "GET", "/api/presentation/languages", token="wrong")[0] == 401
            paired = request(port, "POST", "/api/device/local/pair", unix=data / "local.sock",
                             body={"name": "Route parity browser"})
            assert paired[0] == 200, paired
            token = json.loads(paired[1])["token"]
            status, raw = request(port, "GET", "/api/presentation/languages", token=token,
                                  origin="tauri://localhost")
            assert status == 200, (status, raw)
            preferences = json.loads(raw)
            assert preferences == expected, (preferences, expected)
            assert set(preferences) == set(expected)
            assert preferences["stt"]["build"] is None and preferences["tts"]["build"] is None
            assert type(preferences["replay_on_return_seconds"]) is type(expected["replay_on_return_seconds"])
            status, raw = request(port, "GET", "/api/presentation/integrations", token=token,
                                  origin="tauri://localhost")
            assert status == 200, (status, raw)
            listing = json.loads(raw)["providers"]
            assert [(row["id"], row["source"]) for row in listing] == [
                ("openai", "stored"), ("elevenlabs", "environment")]
            assert all(row["configured"] for row in listing)
            assert listing[0]["hint"] == "…-key" and listing[1]["hint"] == "…-key"
            assert secret.encode() not in raw and b"environment-elevenlabs-fixture-key" not in raw
            status, raw = request(port, "GET", "/api/models/catalog", token=token,
                                  origin="tauri://localhost")
            assert status == 200
            assert raw == Path("src/sidevoice_core/models/catalog.json").read_bytes()
            async with websockets.connect(f"ws://127.0.0.1:{port}/api/presentation/ws",
                                          origin="tauri://localhost",
                                          subprotocols=["sidevoice", f"sidevoice.token.{token}"]) as ws:
                await ws.send(json.dumps({"type": "voice-hello", "data": {"settings": preferences}}))
                while True:
                    message = json.loads(await asyncio.wait_for(ws.recv(), 10))
                    assert message.get("type") != "error", message
                    if message.get("type") == "voice-session":
                        assert message["data"]["session_id"]
                        assert message["data"]["sample_rate"] == 16000
                        break
            print("ROUTE PARITY PASS: frozen Python defaults, bearer guard, settings listing, catalogue, socket join")
        finally:
            if core.poll() is None:
                core.send_signal(signal.SIGTERM)
                assert core.wait(timeout=10) == 0, core.stderr.read()


if __name__ == "__main__":
    asyncio.run(main())
