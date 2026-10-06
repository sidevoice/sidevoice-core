"""Offline first-call preflight against the packaged web pin and frozen Python defaults."""

import asyncio
import base64
import http.client
import http.server
import json
import math
import os
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

import websockets

CORE = Path(sys.argv[1]).resolve()
WEB = Path(sys.argv[2]).resolve()
assert CORE.is_file() and WEB.is_file(), "required Core binary or pinned web source missing"
source = WEB.read_text(encoding="utf-8")
assert "await api('/api/presentation/languages')" in source
assert "state.voicePreferences=await callPreferences()" in source


class ProviderFixture(http.server.ThreadingHTTPServer):
    def __init__(self):
        super().__init__(("127.0.0.1", 0), ProviderHandler)
        self.requests = []
        self.slow_entered = threading.Event()
        self.slow_release = threading.Event()
        self.trial_entered = threading.Event()
        self.trial_release = threading.Event()
        self.pcm = b"".join(struct.pack("<h", int(6000 * math.sin(2 * math.pi * 220 * i / 16000)))
                            for i in range(5 * 16000))
        self.thread = threading.Thread(target=self.serve_forever, daemon=True)
        self.thread.start()


class ProviderHandler(http.server.BaseHTTPRequestHandler):
    def read_body(self):
        if self.headers.get("Transfer-Encoding", "").lower() == "chunked":
            parts = []
            while True:
                size = int(self.rfile.readline().strip().split(b";", 1)[0], 16)
                if size == 0:
                    while self.rfile.readline() not in (b"\r\n", b"\n"):
                        pass
                    break
                parts.append(self.rfile.read(size))
                assert self.rfile.read(2) == b"\r\n"
            return b"".join(parts)
        return self.rfile.read(int(self.headers.get("Content-Length", "0")))

    def answer(self, code, payload, content_type="application/json"):
        if not isinstance(payload, bytes):
            payload = json.dumps(payload).encode()
        self.send_response(code)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_GET(self):
        self.server.requests.append(("GET", self.path, self.headers.get("Authorization"),
                                     self.headers.get("xi-api-key")))
        if self.headers.get("Authorization") == "Bearer slow-key":
            self.server.slow_entered.set()
            assert self.server.slow_release.wait(8)
        if self.headers.get("Authorization") == "Bearer bad-key":
            return self.answer(401, {"error": {"message": "rejected"}})
        if self.path == "/v1/models" and self.headers.get("xi-api-key"):
            return self.answer(200, json.loads(Path("tests/rust_t4/eleven_models.json").read_text()))
        if self.path == "/v1/models":
            return self.answer(200, {"object": "list", "data": [
                {"id": "gpt-4o-transcribe", "object": "model", "created": 0, "owned_by": "fixture"},
                {"id": "ordinary-model", "object": "model", "created": 0, "owned_by": "fixture"}]})
        if self.path.startswith("/v2/voices"):
            return self.answer(200, json.loads(Path("tests/rust_t4/eleven_voices_sparse.json").read_text()))
        self.answer(404, {"error": "fixture path missing"})

    def do_POST(self):
        body = self.read_body()
        self.server.requests.append(("POST", self.path, self.headers.get("Authorization"),
                                     self.headers.get("xi-api-key"), body))
        if self.path == "/v1/audio/transcriptions":
            assert b'audio.wav' in body and b'RIFF' in body and b'gpt-' in body, body[:300]
            if b"gpt-slow-transcribe" in body:
                self.server.trial_entered.set()
                assert self.server.trial_release.wait(8)
            checks = json.loads(Path("assets/catalog/models/checks/checks.json").read_text())
            return self.answer(200, {"text": checks["stt"]["clips"]["en"]["text"]})
        if self.path.startswith("/v1/text-to-speech/") and "pcm_16000" in self.path:
            return self.answer(200, self.server.pcm, "audio/pcm")
        if self.path.startswith("/v1/text-to-speech/"):
            return self.answer(200, b"ID3fixture", "audio/mpeg")
        self.answer(404, {"error": "fixture path missing"})

    def log_message(self, _format, *_args):
        pass


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
        fixture = ProviderFixture()
        data = Path(temporary) / "core"
        data.mkdir(mode=0o700)
        secret = "stored-openai-fixture-key"
        (data / "integrations.json").write_text(json.dumps({"openai": secret}), encoding="utf-8")
        (data / "integrations.json").chmod(0o600)
        env = {**os.environ, "VOICE_ELEVENLABS_API_KEY": "environment-elevenlabs-fixture-key",
               "SIDEVOICE_OPENAI_FIXTURE_BASE": f"http://127.0.0.1:{fixture.server_port}/v1",
               "SIDEVOICE_ELEVENLABS_FIXTURE_BASE": f"http://127.0.0.1:{fixture.server_port}"}
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
            assert request(port, "PUT", "/api/presentation/integrations/openai", token=token,
                           body={"key": "good-key"})[0] == 403
            assert request(port, "DELETE", "/api/presentation/integrations/unknown", token=token,
                           origin="tauri://localhost")[0] == 404
            status, raw = request(port, "GET", "/api/models/catalog", token=token,
                                  origin="tauri://localhost")
            assert status == 200
            assert raw == Path("assets/catalog/models/catalog.json").read_bytes()
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
            print("FIRST CALL PASS: authenticated Python defaults and browser socket join", flush=True)
            status, raw = request(port, "GET", "/api/presentation/transcription/models?provider=openai",
                                  token=token, origin="tauri://localhost")
            assert status == 200, (status, raw)
            assert [row["id"] for row in json.loads(raw)["models"]] == ["gpt-4o-transcribe"]
            status, raw = request(port, "GET", "/api/presentation/voice-catalog", token=token,
                                  origin="tauri://localhost")
            assert status == 200, (status, raw)
            voice_catalog = json.loads(raw)
            assert voice_catalog["languages"] == json.loads(Path("assets/catalog/pipeline/catalog.json").read_text())["languages"]
            assert voice_catalog["providers"]["elevenlabs"]["configured"] is True
            assert voice_catalog["providers"]["elevenlabs"]["models"]
            assert voice_catalog["providers"]["elevenlabs"]["voices"]
            status, raw = request(port, "PUT", "/api/presentation/integrations/openai", token=token,
                                  origin="tauri://localhost", body={"key": "bad-key"})
            assert status == 422 and json.loads(raw)["detail"], (status, raw)
            assert json.loads((data / "integrations.json").read_text())["openai"] == secret
            status, raw = request(port, "PUT", "/api/presentation/integrations/openai", token=token,
                                  origin="tauri://localhost", body={"key": "good-key"})
            assert status == 200, (status, raw)
            assert json.loads((data / "integrations.json").read_text())["openai"] == "good-key"
            delayed = []
            worker = threading.Thread(target=lambda: delayed.append(request(
                port, "PUT", "/api/presentation/integrations/openai", token=token,
                origin="tauri://localhost", body={"key": "slow-key"})))
            worker.start()
            assert fixture.slow_entered.wait(5)
            status, raw = request(port, "DELETE", "/api/presentation/integrations/openai", token=token,
                                  origin="tauri://localhost")
            assert status == 200, (status, raw)
            fixture.slow_release.set()
            worker.join(10)
            assert delayed and delayed[0][0] == 409, delayed
            assert "openai" not in json.loads((data / "integrations.json").read_text())
            trial_body = {"place": "openai", "model": "gpt-4o-transcribe",
                          "options": {"language": "auto", "context": "fixture context"},
                          "audio": {"encoding": "pcm_s16le", "sample_rate": 16000,
                                    "data_base64": base64.b64encode(fixture.pcm).decode()}}
            status, raw = request(port, "POST", "/api/models/transcription/preview", token=token,
                                  origin="tauri://localhost", body=trial_body)
            assert status == 409 and json.loads(raw)["detail"]["key"] == "trial.provider_unavailable", (status, raw)
            status, raw = request(port, "PUT", "/api/presentation/integrations/openai", token=token,
                                  origin="tauri://localhost", body={"key": "good-key"})
            assert status == 200, (status, raw)
            assert request(port, "POST", "/api/models/transcription/preview", body=trial_body)[0] == 401
            status, raw = request(port, "POST", "/api/models/transcription/preview", token=token,
                                  origin="https://foreign.example", body=trial_body)
            assert status == 403, (status, raw)
            provider_calls = len(fixture.requests)
            for invalid, code, key in [
                ({**trial_body, "model": "invalid model"}, 422, "trial.invalid_stage"),
                ({**trial_body, "audio": {**trial_body["audio"], "encoding": "wav"}}, 400, "trial.invalid_audio"),
                ({**trial_body, "audio": {**trial_body["audio"], "data_base64": "bad?"}}, 400, "trial.invalid_audio"),
            ]:
                status, raw = request(port, "POST", "/api/models/transcription/preview", token=token,
                                      origin="tauri://localhost", body=invalid)
                assert status == code and json.loads(raw)["detail"]["key"] == key, (status, raw)
            assert len(fixture.requests) == provider_calls, "invalid trial reached provider"
            history_before = request(port, "GET", "/api/presentation/history", token=token)[1]
            slow_trial = []
            worker = threading.Thread(target=lambda: slow_trial.append(request(
                port, "POST", "/api/models/transcription/preview", token=token,
                origin="tauri://localhost", body={**trial_body, "model": "gpt-slow-transcribe"})))
            worker.start()
            assert fixture.trial_entered.wait(5)
            status, raw = request(port, "POST", "/api/models/transcription/preview", token=token,
                                  origin="tauri://localhost", body=trial_body)
            assert status == 429 and json.loads(raw)["detail"]["key"] == "trial.busy", (status, raw)
            fixture.trial_release.set()
            worker.join(10)
            assert slow_trial and slow_trial[0][0] == 200, slow_trial
            for _ in range(5):
                status, raw = request(port, "POST", "/api/models/transcription/preview", token=token,
                                      origin="tauri://localhost", body=trial_body)
                assert status == 200 and json.loads(raw)["text"], (status, raw)
            trial_calls = [item for item in fixture.requests if item[0] == "POST" and item[1] == "/v1/audio/transcriptions"]
            assert len(trial_calls) == 6
            assert all(b"fixture context" in item[4] for item in trial_calls)
            assert any(b"gpt-slow-transcribe" in item[4] for item in trial_calls)
            status, raw = request(port, "POST", "/api/models/transcription/preview", token=token,
                                  origin="tauri://localhost", body=trial_body)
            assert status == 429 and json.loads(raw)["detail"]["key"] == "trial.busy", (status, raw)
            assert request(port, "GET", "/api/presentation/history", token=token)[1] == history_before
            status, raw = request(port, "POST", "/api/presentation/synthesis/preview", token=token,
                                  origin="tauri://localhost", body={"text": "Hello", "model": "eleven_multilingual_v2",
                                                                   "voice": "sparse-voice", "speed": 1})
            assert status == 200 and json.loads(raw)["audio_base64"], (status, raw)
            stt_check = {"stage": "stt", "place": "openai", "model": "gpt-4o-transcribe",
                         "options": {"language": "en"}, "language": "en"}
            status, raw = request(port, "POST", "/api/models/check", token=token,
                                  origin="tauri://localhost", body=stt_check)
            assert status == 200 and json.loads(raw)["ok"] is True, (status, raw)
            status, raw = request(port, "POST", "/api/models/check", token=token,
                                  origin="tauri://localhost", body=stt_check)
            assert status == 200 and json.loads(raw)["remembered"] is True, (status, raw)
            status, raw = request(port, "POST", "/api/models/check", token=token,
                                  origin="tauri://localhost", body={"stage": "stt", "place": "device"})
            assert status == 400 and json.loads(raw)["detail"]["key"] == "check_on_device", (status, raw)
            tts_check = {"stage": "tts", "place": "elevenlabs", "model": "eleven_multilingual_v2",
                         "options": {"voice": {"en": "sparse-voice"}, "speed": 1}, "language": "en"}
            status, raw = request(port, "POST", "/api/models/check", token=token,
                                  origin="tauri://localhost", body=tts_check)
            assert status == 200 and json.loads(raw)["ok"] is True, (status, raw)
            for index in range(4):
                distinct = {**stt_check, "model": f"gpt-fixture-{index}-transcribe"}
                status, raw = request(port, "POST", "/api/models/check", token=token,
                                      origin="tauri://localhost", body=distinct)
                assert status == 200 and json.loads(raw)["ok"] is True, (status, raw)
            status, raw = request(port, "POST", "/api/models/check", token=token,
                                  origin="tauri://localhost",
                                  body={**stt_check, "model": "gpt-fixture-over-budget-transcribe"})
            assert status == 429 and json.loads(raw)["detail"]["key"] == "check_rate_limited", (status, raw)
            print("ROUTE PARITY PASS: provider settings, ordered keys, STT/TTS tries", flush=True)
        finally:
            fixture.slow_release.set()
            fixture.trial_release.set()
            if core.poll() is None:
                core.send_signal(signal.SIGTERM)
                assert core.wait(timeout=10) == 0, core.stderr.read()
            fixture.shutdown()


if __name__ == "__main__":
    asyncio.run(main())
