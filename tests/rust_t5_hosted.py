"""Offline T5 call proof using the committed speech WAV and pinned JS connector."""

import asyncio
import base64
import fractions
import http.server
import json
import os
import subprocess
import sys
import tempfile
import threading
import time
import uuid
import wave
from pathlib import Path

import av
import numpy as np
import websockets
from aiortc import RTCPeerConnection, RTCSessionDescription
from aiortc.mediastreams import AudioStreamTrack

from rust_t3_hosted import LineProcess, UnixBridge, frame, request, until

CORE = Path(sys.argv[1]).resolve()
JS_LINK = Path(sys.argv[2]).resolve()
SPEECH = Path("tests/fixtures/hola-sala-16k.wav")

# These calls share one conversation: a browser coming to it would first be handed every reply it
# never heard (replay on return, covered by T3), so the cases here ask for no catch-up.
NO_CATCH_UP = {"replay_on_return_seconds": 0}


class TtsFixture(http.server.ThreadingHTTPServer):
    def __init__(self):
        super().__init__(("127.0.0.1", 0), TtsHandler)
        self.requests = []
        threading.Thread(target=self.serve_forever, daemon=True).start()


class MetricCollector(http.server.ThreadingHTTPServer):
    def __init__(self):
        super().__init__(("127.0.0.1", 0), MetricHandler)
        self.received = []
        threading.Thread(target=self.serve_forever, daemon=True).start()


class MetricHandler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        assert self.path == "/v1/metrics"
        self.server.received.append(json.loads(self.rfile.read(int(self.headers["content-length"]))))
        self.send_response(200)
        self.end_headers()

    def log_message(self, _format, *_args):
        pass


class TtsHandler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        assert self.path.startswith("/v1/text-to-speech/fixturevoice/stream/with-timestamps")
        assert self.headers["xi-api-key"] == "fixture-key"
        body = json.loads(self.rfile.read(int(self.headers["content-length"])))
        self.server.requests.append(body)
        payload = Path("tests/rust_t4/eleven_timestamp_chunks.json").read_bytes()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, _format, *_args):
        pass


class RecordedVoice(AudioStreamTrack):
    def __init__(self, pcm, gate):
        super().__init__()
        self.pcm = np.repeat(np.frombuffer(pcm, dtype=np.int16), 3)
        self.gate = gate
        self.at = 0
        self.started = None

    async def recv(self):
        await self.gate.wait()
        if self.started is None:
            self.started = time.monotonic()
        wait = self.started + self.at / 48000 - time.monotonic()
        if wait > 0:
            await asyncio.sleep(wait)
        chunk = self.pcm[self.at:self.at + 960]
        if len(chunk) < 960:
            chunk = np.pad(chunk, (0, 960 - len(chunk)))
        result = av.AudioFrame.from_ndarray(chunk.reshape(1, -1), format="s16", layout="mono")
        result.sample_rate = 48000
        result.pts = self.at
        result.time_base = fractions.Fraction(1, 48000)
        self.at += 960
        return result


async def send_pcm(ws, pcm):
    for at in range(0, len(pcm), 640):
        await ws.send(pcm[at:at + 640])
        await asyncio.sleep(0.02)


async def assert_silence(ws):
    deadline = time.monotonic() + 0.8
    while time.monotonic() < deadline:
        try:
            event = json.loads(await asyncio.wait_for(ws.recv(), deadline - time.monotonic()))
        except TimeoutError:
            break
        assert event["type"] not in {"voice-transcribe", "voice-user-turn", "voice-input-receipt"}, event


async def complete_device_turn(ws, session, pcm, *, socket_audio=True):
    if socket_audio:
        await send_pcm(ws, pcm)
        await send_pcm(ws, b"\0" * 16_000 * 2 * 4)
    ask = await frame(ws, "voice-transcribe", timeout=40)
    assert ask["session_id"] == session
    recorded = base64.b64decode(ask["audio_base64"], validate=True)
    assert recorded[:4] == b"RIFF" and len(recorded) > 10_000
    await ws.send(json.dumps({"type": "voice-transcript", "data": {
        "session_id": session, "request_id": ask["request_id"], "text": "Hola from the recorded call"}}))
    finished = await frame(ws, "voice-user-turn", timeout=20)
    assert finished["phase"] == "finished" and finished["text"] == "Hola from the recorded call", finished
    receipt = await frame(ws, "voice-input-receipt", status="pending")
    assert receipt["session_id"] == session
    return finished, receipt


async def no_transcription(ws, seconds=0.7):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            event = json.loads(await asyncio.wait_for(ws.recv(), deadline - time.monotonic()))
        except TimeoutError:
            return
        assert event["type"] != "voice-transcribe", event


async def review_voice_cases(url, protocols, port, token, peer, pcm):
    settings = {"turn_end_mode": "timer", "user_speech_timeout": 0.5, "merge_window_secs": 0}
    async with websockets.connect(url, subprotocols=protocols) as ws:
        await ws.send(json.dumps({"type": "voice-hello", "data": {"settings": {**settings, **NO_CATCH_UP}}}))
        session = (await frame(ws, "voice-session"))["session_id"]
        assert request(port, "POST", "/api/presentation/select", token=token,
                       body={"session_id": session, "thread_id": "t3-js-thread"})[0] == 200
        revision = request(port, "GET", f"/api/presentation?session_id={session}", token=token)[1]["room"]["revision"]
        started = int(time.time() * 1000) - 1000
        for seq, part in enumerate((pcm[:40000], pcm[40000:])):
            await ws.send(json.dumps({"type": "voice-catchup", "data": {
                "session_id": session, "sample_rate": 16000, "seq": seq,
                "audio_base64": base64.b64encode(part).decode(), "started_at": started,
                "final": seq == 1}}))
        ask = await frame(ws, "voice-transcribe", timeout=15)
        assert wave.open(__import__("io").BytesIO(base64.b64decode(ask["audio_base64"]))).getframerate() == 16000
        await ws.send(json.dumps({"type": "voice-transcript", "data": {
            "session_id": session, "request_id": ask["request_id"], "text": "Words recorded offline"}}))
        catchup = await frame(ws, "voice-catchup-turn")
        assert catchup["history_id"] == f"{session}:user-catchup:1" and catchup["time"] == started, catchup
        assert catchup["offline"] == "buffered" and catchup["thread_id"] == "t3-js-thread", catchup
        assert (await frame(ws, "voice-input-receipt", status="pending"))["revision"] == 0
        row = request(port, "GET", "/api/presentation/history?thread_id=t3-js-thread", token=token)[1]["messages"][-1]
        assert (row["offline"], row["time"], row["revision"]) == ("buffered", started, 0), row
        assert request(port, "GET", f"/api/presentation?session_id={session}", token=token)[1]["room"]["revision"] == revision
        for seq in (0, 2):
            await ws.send(json.dumps({"type": "voice-catchup", "data": {
                "session_id": session, "sample_rate": 16000, "seq": seq,
                "audio_base64": base64.b64encode(pcm[:16000]).decode(), "final": seq == 2}}))
        await no_transcription(ws)
        own = request(port, "GET", "/api/presentation/history?thread_id=t3-js-thread", token=token)[1]["messages"]
        assert len([row for row in own if row["session"] == session]) == 1


        for cause in ("device", "timeout"):
            await send_pcm(ws, pcm)
            await send_pcm(ws, b"\0" * 16_000 * 2 * 4)
            ask = await frame(ws, "voice-transcribe", timeout=20)
            if cause == "device":
                await ws.send(json.dumps({"type": "voice-transcript-error", "data": {
                    "session_id": session, "request_id": ask["request_id"], "error": "fixture failure"}}))
            error = await frame(ws, "error", timeout=16)
            assert "message" in error and error["message"], error
            cancelled = await frame(ws, "voice-user-turn", timeout=8)
            assert cancelled["phase"] == "cancelled" and cancelled["text"] == "", cancelled
        own = request(port, "GET", "/api/presentation/history?thread_id=t3-js-thread", token=token)[1]["messages"]
        assert len([row for row in own if row["session"] == session]) == 1

    async with websockets.connect(url, subprotocols=protocols) as ws:
        await ws.send(json.dumps({"type": "voice-hello", "data": {"settings": {**NO_CATCH_UP,
            **settings, "merge_window_secs": 2.0}}}))
        session = (await frame(ws, "voice-session"))["session_id"]
        assert request(port, "POST", "/api/presentation/select", token=token,
                       body={"session_id": session, "thread_id": "t3-js-thread"})[0] == 200
        await send_pcm(ws, pcm)
        await send_pcm(ws, b"\0" * 16_000 * 2 * 2)
        first = await frame(ws, "voice-transcribe", timeout=20)
        await send_pcm(ws, pcm)
        await send_pcm(ws, b"\0" * 16_000 * 2 * 2)
        await no_transcription(ws, 0.4)
        await ws.send(json.dumps({"type": "voice-transcript", "data": {
            "session_id": session, "request_id": first["request_id"], "text": "First spoken"}}))
        second = await frame(ws, "voice-transcribe", timeout=20)
        await ws.send(json.dumps({"type": "voice-transcript", "data": {
            "session_id": session, "request_id": second["request_id"], "text": "Second spoken"}}))
        turn = await frame(ws, "voice-user-turn", timeout=10)
        while turn["phase"] != "finished":
            turn = await frame(ws, "voice-user-turn", timeout=10)
        assert turn["text"] == "First spoken Second spoken", turn
        await frame(ws, "voice-input-receipt", status="pending")
        await send_pcm(ws, pcm)
        await send_pcm(ws, b"\0" * 16_000 * 2 * 2)
        await frame(ws, "voice-transcribe", timeout=20)
        await ws.close()
        await asyncio.sleep(0.2)
        own = request(port, "GET", "/api/presentation/history?thread_id=t3-js-thread", token=token)[1]["messages"]
        assert len([row for row in own if row["session"] == session]) == 1


async def two_listener_focus_case(url, protocols, port, token, peer, pcm):
    peer.send({"op": "register_second"})
    assert peer.event("second-binding")["binding"]["binding_id"]
    async with websockets.connect(url, subprotocols=protocols) as first, \
               websockets.connect(url, subprotocols=protocols) as second:
        settings = {"turn_end_mode": "timer", "user_speech_timeout": 0.5, "merge_window_secs": 0}
        for ws in (first, second):
            await ws.send(json.dumps({"type": "voice-hello", "data": {"settings": {**settings, **NO_CATCH_UP}}}))
        sessions = [(await frame(ws, "voice-session"))["session_id"] for ws in (first, second)]
        for session in sessions:
            assert request(port, "POST", "/api/presentation/select", token=token,
                           body={"session_id": session, "thread_id": "t3-js-thread"})[0] == 200
        revision = request(port, "GET", f"/api/presentation?session_id={sessions[0]}", token=token)[1]["room"]["revision"]
        uid = f"two-listeners-{uuid.uuid4()}"
        peer.send({"op": "publish", "session_id": sessions[0], "revision": revision,
                   "event_id": uid, "utterance_id": uid, "text": "Reply to both listeners"})
        assert peer.event("published")["answer"]["status"] == "queued"
        speeches = [await frame(ws, "voice-speech") for ws in (first, second)]
        for session, speech in zip(sessions, speeches):
            assert speech["session_id"] == session and speech["utterance_id"] == uid, speech
        for session, speech in zip(sessions, speeches):
            assert request(port, "POST", "/api/presentation/browser-receipt", token=token,
                           body={"session_id": session, "utterance_id": speech["utterance_id"],
                                 "revision": speech["revision"], "status": "playing"})[0] == 200
        for session, speech in zip(sessions, speeches):
            assert request(port, "POST", "/api/presentation/browser-receipt", token=token,
                           body={"session_id": session, "utterance_id": speech["utterance_id"],
                                 "revision": speech["revision"], "status": "playback_finished"})[0] == 200

        stream = asyncio.create_task(send_pcm(first, pcm + pcm + b"\0" * 16_000 * 2 * 2))
        started = await frame(first, "voice-user-turn", timeout=15)
        assert started["phase"] == "started" and started["thread_id"] == "t3-js-thread", started
        await asyncio.sleep(1.3)
        assert request(port, "POST", "/api/presentation/select", token=token,
                       body={"session_id": sessions[0], "thread_id": "t3-js-other"})[0] == 200
        await stream
        old = await frame(first, "voice-transcribe", timeout=20)
        await first.send(json.dumps({"type": "voice-transcript", "data": {
            "session_id": sessions[0], "request_id": old["request_id"], "text": "Before focus"}}))
        old_turn = await frame(first, "voice-user-turn", timeout=10)
        while old_turn["phase"] != "finished":
            old_turn = await frame(first, "voice-user-turn", timeout=10)
        assert old_turn["thread_id"] == "t3-js-thread" and old_turn["text"] == "Before focus", old_turn
        new = await frame(first, "voice-transcribe", timeout=20)
        assert new["request_id"] != old["request_id"], (old, new)
        await first.send(json.dumps({"type": "voice-transcript", "data": {
            "session_id": sessions[0], "request_id": new["request_id"], "text": "After focus"}}))
        seen = []
        deadline = time.monotonic() + 18
        new_turn = None
        while time.monotonic() < deadline:
            try:
                event = json.loads(await asyncio.wait_for(first.recv(), deadline - time.monotonic()))
            except TimeoutError:
                break
            seen.append(event)
            if event["type"] == "voice-user-turn" and event["data"]["phase"] == "finished":
                new_turn = event["data"]
                break
        if new_turn is None:
            snapshot = request(port, "GET", f"/api/presentation?session_id={sessions[0]}", token=token)
            old_rows = request(port, "GET", "/api/presentation/history?thread_id=t3-js-thread", token=token)
            new_rows = request(port, "GET", "/api/presentation/history?thread_id=t3-js-other", token=token)
            raise AssertionError(("focus completion", old_turn, new["request_id"], seen, snapshot, old_rows, new_rows))
        completed = [old_turn, new_turn]
        assert [(turn["thread_id"], turn["text"]) for turn in completed] == [
            ("t3-js-thread", "Before focus"), ("t3-js-other", "After focus")], completed
        old_rows = request(port, "GET", "/api/presentation/history?thread_id=t3-js-thread", token=token)[1]["messages"]
        new_rows = request(port, "GET", "/api/presentation/history?thread_id=t3-js-other", token=token)[1]["messages"]
        assert any(row["text"] == "Before focus" and row["session"] == sessions[0] for row in old_rows)
        assert any(row["text"] == "After focus" and row["session"] == sessions[0] for row in new_rows)


async def playback_gate_case(url, protocols, port, token, peer, pcm):
    async with websockets.connect(url, subprotocols=protocols) as ws:
        await ws.send(json.dumps({"type": "voice-hello", "data": {"settings": {**NO_CATCH_UP,
            "turn_end_mode": "timer", "user_speech_timeout": 0.5, "merge_window_secs": 0}}}))
        session = (await frame(ws, "voice-session"))["session_id"]
        assert request(port, "POST", "/api/presentation/select", token=token,
                       body={"session_id": session, "thread_id": "t3-js-thread"})[0] == 200

        async def reply_playing():
            revision = request(port, "GET", f"/api/presentation?session_id={session}", token=token)[1]["room"]["revision"]
            uid = f"playback-gate-{uuid.uuid4()}"
            peer.send({"op": "publish", "session_id": session, "revision": revision,
                       "event_id": uid, "utterance_id": uid, "text": "Speaker output"})
            assert peer.event("published")["answer"]["status"] == "queued"
            speech = await frame(ws, "voice-speech")
            assert request(port, "POST", "/api/presentation/browser-receipt", token=token,
                           body={"session_id": session, "utterance_id": uid,
                                 "revision": speech["revision"], "status": "playing"})[0] == 200
            return uid, speech["revision"]

        uid, revision = await reply_playing()
        await send_pcm(ws, pcm)
        await send_pcm(ws, b"\0" * 16_000 * 2 * 2)
        await no_transcription(ws)
        assert request(port, "POST", "/api/presentation/browser-receipt", token=token,
                       body={"session_id": session, "utterance_id": uid,
                             "revision": revision, "status": "playback_finished"})[0] == 200
        await send_pcm(ws, pcm)
        await send_pcm(ws, b"\0" * 16_000 * 2 * 2)
        ask = await frame(ws, "voice-transcribe", timeout=20)
        await ws.send(json.dumps({"type": "voice-transcript", "data": {
            "session_id": session, "request_id": ask["request_id"], "text": "Normal voice after playback"}}))
        assert (await frame(ws, "voice-user-turn", timeout=10))["phase"] == "finished"
        await frame(ws, "voice-input-receipt", status="pending")

        uid, revision = await reply_playing()
        loud = np.clip(np.frombuffer(pcm, dtype=np.int16).astype(np.int32) * 8,
                       -32768, 32767).astype(np.int16).tobytes()
        await send_pcm(ws, loud)
        await send_pcm(ws, b"\0" * 16_000 * 2 * 2)
        await frame(ws, "voice-cancel", timeout=15)
        ask = await frame(ws, "voice-transcribe", timeout=20)
        assert request(port, "POST", "/api/presentation/browser-receipt", token=token,
                       body={"session_id": session, "utterance_id": uid,
                             "revision": revision, "status": "playing"})[0] == 409
        await ws.send(json.dumps({"type": "voice-transcript", "data": {
            "session_id": session, "request_id": ask["request_id"], "text": "Louder barge in"}}))
        assert (await frame(ws, "voice-user-turn", timeout=10))["phase"] == "finished"


async def main():
    with wave.open(str(SPEECH), "rb") as recording:
        assert recording.getframerate() == 16000 and recording.getnchannels() == 1
        pcm = recording.readframes(recording.getnframes())
    with tempfile.TemporaryDirectory(prefix="sidevoice-t5-") as temporary:
        root = Path(temporary)
        data = root / "core"
        data.mkdir(mode=0o700)
        integrations = data / "integrations.json"
        integrations.write_text(json.dumps({"elevenlabs": "fixture-key"}))
        integrations.chmod(0o600)
        replay_gate = root / "replay-render-gate"
        fixture = TtsFixture()
        collector = MetricCollector()
        env = {key: value for key, value in os.environ.items() if key != "VOICE_ELEVENLABS_API_KEY"}
        env.update({"SIDEVOICE_STUN_URLS": "",
               "SIDEVOICE_FIXTURE_STT_TIMEOUT_MS": "12000",
               "SIDEVOICE_FIXTURE_REPLAY_RENDER_GATE": str(replay_gate),
               "OTEL_EXPORTER_OTLP_ENDPOINT": f"http://127.0.0.1:{collector.server_port}",
               "SIDEVOICE_ELEVENLABS_FIXTURE_BASE": f"http://127.0.0.1:{fixture.server_port}"})
        core = subprocess.Popen([str(CORE), "--data-dir", str(data), "--port", "0", "--idle-exit", "0"],
                                stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True, env=env)
        bridge = None
        peer = None
        try:
            ready = until(lambda: json.loads((data / "core.json").read_text()) if (data / "core.json").exists() else None)
            port = ready["port"]
            paired = request(port, "POST", "/api/device/local/pair", unix=data / "local.sock",
                             body={"name": "T5 browser"})
            assert paired[0] == 200, paired
            token = paired[1]["token"]
            bridge = UnixBridge(data / "local.sock")
            origin = f"http://127.0.0.1:{bridge.server_address[1]}"
            peer = LineProcess(["node", str(Path(__file__).with_name("rust_t3_v2_peer.mjs")),
                                str(JS_LINK), origin, ready["connector_id"], ready["token"]])
            assert peer.event("welcome")["welcome"]["protocol"] == 2
            peer.event("binding")
            url = f"ws://127.0.0.1:{port}/api/presentation/ws"
            protocols = ["sidevoice", f"sidevoice.token.{token}"]
            for mode in ("smart_turn", "timer"):
                async with websockets.connect(url, subprotocols=protocols) as ws:
                    settings = {"turn_end_mode": mode, "merge_window_secs": 0,
                                "user_speech_timeout": 0.5, "smart_turn_min_silence": 0.5,
                                "smart_turn_max_silence": 1.0}
                    await ws.send(json.dumps({"type": "voice-hello", "data": {"settings": {**settings, **NO_CATCH_UP}}}))
                    session = (await frame(ws, "voice-session"))["session_id"]
                    chosen = request(port, "POST", "/api/presentation/select", token=token,
                                     body={"session_id": session, "thread_id": "t3-js-thread"})
                    assert chosen[0] == 200, chosen
                    await send_pcm(ws, b"\0" * 16_000 * 2)
                    await assert_silence(ws)
                    finished, receipt = await complete_device_turn(ws, session, pcm)
                    assert receipt["revision"] == finished["revision"]
                    peer.method("input.deliver")
                    await frame(ws, "voice-input-receipt", status="delivered")
                    revision = request(port, "GET", f"/api/presentation?session_id={session}", token=token)[1]["room"]["revision"]
                    uid = f"t5-{mode}-{uuid.uuid4()}"
                    peer.send({"op": "publish", "session_id": session, "revision": revision,
                               "event_id": uid, "utterance_id": uid, "text": "Reply from pinned peer"})
                    assert peer.event("published")["answer"]["status"] == "queued"
                    speech = await frame(ws, "voice-speech")
                    assert speech["utterance_id"] == uid and speech["place"] == "device"
                    for status in ("playing", "playback_finished"):
                        result = request(port, "POST", "/api/presentation/browser-receipt", token=token,
                                         body={"session_id": session, "utterance_id": uid,
                                               "revision": speech["revision"], "status": status})
                        assert result[0] == 200, result
                    status, trace = request(port, "GET", f"/api/presentation/latency?session_id={session}", token=token)
                    assert status == 200 and trace["session_id"] == session, (status, trace)
                    row = next(row for row in trace["replies"] if row["utterance_id"] == uid)
                    assert row["input_ms"]["audio_ms"] > 0 and row["server_ms"]["input_queued_to_reply_received_ms"] >= 0, row
                    assert request(port, "GET", f"/api/presentation/latency?session_id={session}")[0] == 401
                    peer.send({"op": "issue_code"})
                    secret = peer.event("pairing-code")["answer"]["payload"]["secret"]
                    second = request(port, "POST", "/api/device/pair",
                                     body={"secret": secret, "name": "Other T5 browser"})[1]["token"]
                    assert request(port, "GET", f"/api/presentation/latency?session_id={session}", token=second)[0] == 404
                    if mode == "timer":
                        stale_uid = f"t5-barge-{uuid.uuid4()}"
                        peer.send({"op": "publish", "session_id": session, "revision": revision,
                                   "event_id": stale_uid, "utterance_id": stale_uid, "text": "Interrupted reply"})
                        assert peer.event("published")["answer"]["status"] == "queued"
                        stale_speech = await frame(ws, "voice-speech")
                        await send_pcm(ws, pcm)
                        await send_pcm(ws, b"\0" * 16_000 * 2 * 4)
                        await frame(ws, "voice-cancel")
                        ask = await frame(ws, "voice-transcribe", timeout=30)
                        stale_receipt = request(port, "POST", "/api/presentation/browser-receipt", token=token,
                                                body={"session_id": session, "utterance_id": stale_uid,
                                                      "revision": stale_speech["revision"], "status": "playing"})
                        assert stale_receipt[0] == 409, stale_receipt
                        await ws.send(json.dumps({"type": "voice-transcript", "data": {
                            "session_id": session, "request_id": ask["request_id"], "text": "Follow-up after interruption"}}))
                        assert (await frame(ws, "voice-user-turn"))["phase"] == "finished"
            async with websockets.connect(url, subprotocols=protocols) as ws:
                await ws.send(json.dumps({"type": "voice-hello", "data": {"settings": {**NO_CATCH_UP,
                    "turn_end_mode": "timer", "user_speech_timeout": 0.5, "merge_window_secs": 0,
                    "tts": {"place": "elevenlabs", "model": "eleven_v3",
                            "options": {"voice": {"en": "fixturevoice"}}}}}}))
                session = (await frame(ws, "voice-session"))["session_id"]
                assert request(port, "POST", "/api/presentation/select", token=token,
                               body={"session_id": session, "thread_id": "t3-js-thread"})[0] == 200
                await complete_device_turn(ws, session, pcm)
                peer.method("input.deliver")
                revision = request(port, "GET", f"/api/presentation?session_id={session}", token=token)[1]["room"]["revision"]
                uid = f"t5-mixed-{uuid.uuid4()}"
                peer.send({"op": "publish", "session_id": session, "revision": revision,
                           "event_id": uid, "utterance_id": uid, "text": "Cloud fixture reply"})
                assert peer.event("published")["answer"]["status"] == "queued"
                audio = await frame(ws, "voice-speech-audio", timeout=20)
                assert audio["utterance_id"] == uid and audio["place"] == "elevenlabs", audio
                assert base64.b64decode(audio["audio_base64"]) == bytes((1, 2, 3, 4))
                assert fixture.requests[-1]["text"] == "Cloud fixture reply"
                for status in ("playing", "playback_finished"):
                    assert request(port, "POST", "/api/presentation/browser-receipt", token=token,
                                   body={"session_id": session, "utterance_id": uid,
                                         "revision": audio["revision"], "status": status})[0] == 200
                history_id = audio["history_id"]
                history = request(port, "GET", f"/api/presentation/history?thread_id=t3-js-thread&session_id={session}",
                                  token=token)[1]["messages"]
                assert next(row for row in history if row["id"] == history_id)["replayable"] is True
                rendered_before = len(fixture.requests)
                status, replayed = request(port, "POST", "/api/presentation/replay", token=token,
                                           body={"session_id": session, "history_id": history_id})
                assert status == 200 and replayed["history_id"] == history_id, (status, replayed)
                replay_notice = await frame(ws, "voice-replay")
                assert replay_notice["replies"][0]["utterance_id"] == replayed["utterance_id"]
                replay_audio = await frame(ws, "voice-speech-audio", timeout=20)
                assert replay_audio["utterance_id"] == replayed["utterance_id"]
                assert replay_audio["audio_base64"] == audio["audio_base64"]
                assert len(fixture.requests) == rendered_before, "replay invoked paid synthesis"
                for status in ("playing", "playback_finished"):
                    assert request(port, "POST", "/api/presentation/browser-receipt", token=token,
                                   body={"session_id": session, "utterance_id": replayed["utterance_id"],
                                         "revision": replay_audio["revision"], "status": status})[0] == 200
                burst = []
                for _ in range(16):
                    status, again = request(port, "POST", "/api/presentation/replay", token=token,
                                            body={"session_id": session, "history_id": history_id})
                    assert status == 200, (status, again)
                    burst.append(again["utterance_id"])
                status, refused = request(port, "POST", "/api/presentation/replay", token=token,
                                          body={"session_id": session, "history_id": history_id})
                room_full = json.loads(Path("rust/messages/en.json").read_text())["room.replay_full"]
                assert status == 429 and refused["detail"] == room_full, (status, refused)
                live_uid = f"t5-after-replays-{uuid.uuid4()}"
                peer.send({"op": "publish", "session_id": session, "revision": revision,
                           "event_id": live_uid, "utterance_id": live_uid, "text": "Live after replay burst"})
                assert peer.event("published")["answer"]["status"] == "queued"
                for expected_uid in [burst[0], *reversed(burst[1:])]:
                    replay_audio = await frame(ws, "voice-speech-audio", timeout=20)
                    assert replay_audio["utterance_id"] == expected_uid, replay_audio
                    assert replay_audio["audio_base64"] == audio["audio_base64"]
                    for state in ("playing", "playback_finished"):
                        assert request(port, "POST", "/api/presentation/browser-receipt", token=token,
                                       body={"session_id": session, "utterance_id": expected_uid,
                                             "revision": replay_audio["revision"], "status": state})[0] == 200
                live_audio = await frame(ws, "voice-speech-audio", timeout=20)
                assert live_audio["utterance_id"] == live_uid, live_audio
                assert fixture.requests[-1]["text"] == "Live after replay burst"
                assert len(fixture.requests) == rendered_before + 1, "replays invoked paid synthesis"
                for state in ("playing", "playback_finished"):
                    assert request(port, "POST", "/api/presentation/browser-receipt", token=token,
                                   body={"session_id": session, "utterance_id": live_uid,
                                         "revision": live_audio["revision"], "status": state})[0] == 200
                snapshot = request(port, "GET", f"/api/presentation?session_id={session}", token=token)[1]
                assert all(row["replay_of"] is None for row in snapshot["room"]["utterances"])
                original = request(port, "GET", "/api/presentation/history?thread_id=t3-js-thread",
                                   token=token)[1]["messages"]
                assert next(row for row in original if row["id"] == history_id)["status"] == "playback_finished"
                replay_gate.write_text("hold old dispatch")
                status, held = request(port, "POST", "/api/presentation/replay", token=token,
                                       body={"session_id": session, "history_id": history_id})
                assert status == 200, (status, held)
                assert (await frame(ws, "voice-replay"))["replies"][0]["utterance_id"] == held["utterance_id"]
                entered = Path(f"{replay_gate}.entered")
                until(lambda: entered.read_text() if entered.exists() else None)
                assert entered.read_text() == held["utterance_id"]
                held_input = asyncio.create_task(send_pcm(ws, pcm))
                started = await frame(ws, "voice-user-turn", timeout=10)
                assert started["phase"] == "started", started
                integrations.write_text("{}")
                replay_gate.unlink()
                await held_input
                await send_pcm(ws, b"\0" * 16_000 * 2 * 4)
                ask = await frame(ws, "voice-transcribe", timeout=30)
                await ws.send(json.dumps({"type": "voice-transcript", "data": {
                    "session_id": session, "request_id": ask["request_id"], "text": "Resume held replay"}}))
                finished = await frame(ws, "voice-user-turn", timeout=10)
                assert finished["phase"] == "finished", finished
                held_audio = await frame(ws, "voice-speech-audio", timeout=20)
                assert held_audio["utterance_id"] == held["utterance_id"]
                assert held_audio["audio_base64"] == audio["audio_base64"]
                assert len(fixture.requests) == rendered_before + 1, "held replay rerendered"
                for state in ("playing", "playback_finished"):
                    assert request(port, "POST", "/api/presentation/browser-receipt", token=token,
                                   body={"session_id": session, "utterance_id": held["utterance_id"],
                                         "revision": held_audio["revision"], "status": state})[0] == 200
                status, delivered = request(port, "POST", "/api/presentation/replay", token=token,
                                            body={"session_id": session, "history_id": history_id})
                assert status == 200, (status, delivered)
                assert (await frame(ws, "voice-replay"))["replies"][0]["utterance_id"] == delivered["utterance_id"]
                delivered_audio = await frame(ws, "voice-speech-audio", timeout=20)
                assert delivered_audio["audio_base64"] == audio["audio_base64"]
                delivered_input = asyncio.create_task(send_pcm(ws, pcm))
                started = await frame(ws, "voice-user-turn", timeout=10)
                assert started["phase"] == "started", started
                await delivered_input
                await send_pcm(ws, b"\0" * 16_000 * 2 * 4)
                ask = await frame(ws, "voice-transcribe", timeout=30)
                await ws.send(json.dumps({"type": "voice-transcript", "data": {
                    "session_id": session, "request_id": ask["request_id"], "text": "Resume delivered replay"}}))
                finished = await frame(ws, "voice-user-turn", timeout=10)
                assert finished["phase"] == "finished", finished
                resumed_audio = await frame(ws, "voice-speech-audio", timeout=20)
                assert resumed_audio["utterance_id"] == delivered["utterance_id"]
                assert resumed_audio["audio_base64"] == audio["audio_base64"]
                assert len(fixture.requests) == rendered_before + 1, "delivered replay rerendered"
                for state in ("playing", "playback_finished"):
                    assert request(port, "POST", "/api/presentation/browser-receipt", token=token,
                                   body={"session_id": session, "utterance_id": delivered["utterance_id"],
                                         "revision": resumed_audio["revision"], "status": state})[0] == 200
                cancelled_audio = asyncio.create_task(send_pcm(ws, pcm))
                started = await frame(ws, "voice-user-turn", timeout=10)
                assert started["phase"] == "started", started
                status, cancelled = request(port, "POST", "/api/presentation/cancel-input", token=token,
                                            body={"session_id": session, "revision": started["revision"]})
                assert status == 200 and cancelled["status"] == "cancelled", (status, cancelled)
                await cancelled_audio
                turn_event = await frame(ws, "voice-user-turn", timeout=10)
                assert turn_event["phase"] == "cancelled" and turn_event["revision"] == started["revision"]
                assert request(port, "POST", "/api/presentation/cancel-input", token=token,
                               body={"session_id": session, "revision": started["revision"]})[0] == 409
                history = request(port, "GET", "/api/presentation/history?thread_id=t3-js-thread",
                                  token=token)[1]["messages"]
                assert not any(row["id"] == f"{session}:user-turn:{started['revision']}" for row in history)
            async with websockets.connect(url, subprotocols=protocols) as ws:
                await ws.send(json.dumps({"type": "voice-hello", "data": {"settings": {**NO_CATCH_UP,
                    "turn_end_mode": "timer", "user_speech_timeout": 0.5, "merge_window_secs": 3.0}}}))
                session = (await frame(ws, "voice-session"))["session_id"]
                assert request(port, "POST", "/api/presentation/select", token=token,
                               body={"session_id": session, "thread_id": "t3-js-thread"})[0] == 200
                for part in ("First thought", "continued thought"):
                    await send_pcm(ws, pcm)
                    await send_pcm(ws, b"\0" * 16_000 * 2)
                    ask = await frame(ws, "voice-transcribe", timeout=30)
                    await ws.send(json.dumps({"type": "voice-transcript", "data": {
                        "session_id": session, "request_id": ask["request_id"], "text": part}}))
                merged = await frame(ws, "voice-user-turn", timeout=10)
                while merged["phase"] != "finished":
                    assert merged.get("merged") is True, merged
                    merged = await frame(ws, "voice-user-turn", timeout=10)
                assert merged["phase"] == "finished" and merged["text"] == "First thought continued thought", merged
                assert (await frame(ws, "voice-input-receipt", status="pending"))["revision"] == merged["revision"]
            await review_voice_cases(url, protocols, port, token, peer, pcm)
            await two_listener_focus_case(url, protocols, port, token, peer, pcm)
            await playback_gate_case(url, protocols, port, token, peer, pcm)
            async with websockets.connect(url, subprotocols=protocols) as ws:
                await ws.send(json.dumps({"type": "voice-hello", "data": {"settings": {**NO_CATCH_UP,
                    "turn_end_mode": "timer", "user_speech_timeout": 0.5, "merge_window_secs": 0}}}))
                session = (await frame(ws, "voice-session"))["session_id"]
                chosen = request(port, "POST", "/api/presentation/select", token=token,
                                 body={"session_id": session, "thread_id": "t3-js-thread"})
                assert chosen[0] == 200, chosen
                before = request(port, "GET", "/api/presentation/history?thread_id=t3-js-thread", token=token)[1]["messages"]
                gate = asyncio.Event()
                browser = RTCPeerConnection()
                replacement = RTCPeerConnection()
                try:
                    browser.addTrack(RecordedVoice(pcm + b"\0" * 16_000 * 2 * 4, gate))
                    await browser.setLocalDescription(await browser.createOffer())
                    answered = request(port, "POST", "/api/presentation/rtc/offer", token=token,
                                       body={"session_id": session, "type": "offer", "sdp": browser.localDescription.sdp},
                                       timeout=30)
                    assert answered[0] == 200, answered
                    await browser.setRemoteDescription(RTCSessionDescription(**answered[1]))
                    await ws.send(json.dumps({"type": "voice-media", "data": {
                        "session_id": session, "path": "webrtc"}}))
                    gate.set()
                    await complete_device_turn(ws, session, pcm, socket_audio=False)
                    replacement_gate = asyncio.Event()
                    replacement.addTrack(RecordedVoice(pcm, replacement_gate))
                    await replacement.setLocalDescription(await replacement.createOffer())
                    second_answer = request(port, "POST", "/api/presentation/rtc/offer", token=token,
                                            body={"session_id": session, "type": "offer",
                                                  "sdp": replacement.localDescription.sdp}, timeout=30)
                    assert second_answer[0] == 200, second_answer
                    await replacement.setRemoteDescription(RTCSessionDescription(**second_answer[1]))
                    await ws.send(json.dumps({"type": "voice-media", "data": {
                        "session_id": session, "path": "socket"}}))
                    await complete_device_turn(ws, session, pcm)
                    after = request(port, "GET", "/api/presentation/history?thread_id=t3-js-thread", token=token)[1]["messages"]
                    own = [row for row in after if row["role"] == "user" and row["session"] == session]
                    assert len(after) == len(before) + 2 and len(own) == 2, own
                    revoked = request(port, "DELETE", "/api/device/local", unix=data / "local.sock")
                    assert revoked[0] == 200 and revoked[1]["revoked"] is True, revoked
                    await asyncio.wait_for(ws.wait_closed(), 8)
                    assert ws.close_code == 4401, ws.close_code
                finally:
                    await browser.close()
                    await replacement.close()
            print("T5 PASS: recorded SmartTurn/timer, mixed SDK/device reply, barge-in, RTC replacement/WS fallback, revoke")
            until(lambda: any("sidevoice.turn.endpoint_silence" in json.dumps(item)
                              for item in collector.received), timeout=5)
        except Exception:
            if core.poll() is None:
                core.terminate()
                core.wait(timeout=10)
            print(core.stderr.read()[-4000:], file=sys.stderr)
            raise
        finally:
            fixture.shutdown()
            fixture.server_close()
            collector.shutdown()
            collector.server_close()
            if peer:
                peer.stop()
            if bridge:
                bridge.shutdown()
                bridge.server_close()
            if core.poll() is None:
                core.terminate()
                core.wait(timeout=10)


if __name__ == "__main__":
    asyncio.run(main())
