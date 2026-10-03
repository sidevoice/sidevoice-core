"""Offline T5 call proof using the committed speech WAV and pinned JS connector."""

import asyncio
import base64
import fractions
import json
import os
import subprocess
import sys
import tempfile
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


async def main():
    with wave.open(str(SPEECH), "rb") as recording:
        assert recording.getframerate() == 16000 and recording.getnchannels() == 1
        pcm = recording.readframes(recording.getnframes())
    with tempfile.TemporaryDirectory(prefix="sidevoice-t5-") as temporary:
        root = Path(temporary)
        data = root / "core"
        data.mkdir(mode=0o700)
        env = {**os.environ, "SIDEVOICE_STUN_URLS": ""}
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
                    await ws.send(json.dumps({"type": "voice-hello", "data": {"settings": settings}}))
                    session = (await frame(ws, "voice-session"))["session_id"]
                    chosen = request(port, "POST", "/api/presentation/select", token=token,
                                     body={"session_id": session, "thread_id": "t3-js-thread"})
                    assert chosen[0] == 200, chosen
                    await send_pcm(ws, b"\0" * 16_000 * 2)
                    await assert_silence(ws)
                    finished, receipt = await complete_device_turn(ws, session, pcm)
                    assert receipt["revision"] == finished["revision"]
                    assert peer.event("from-core")["method"] == "input.deliver"
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
            async with websockets.connect(url, subprotocols=protocols) as ws:
                await ws.send(json.dumps({"type": "voice-hello", "data": {"settings": {
                    "turn_end_mode": "timer", "user_speech_timeout": 0.5, "merge_window_secs": 0}}}))
                session = (await frame(ws, "voice-session"))["session_id"]
                chosen = request(port, "POST", "/api/presentation/select", token=token,
                                 body={"session_id": session, "thread_id": "t3-js-thread"})
                assert chosen[0] == 200, chosen
                before = request(port, "GET", "/api/presentation/history?thread_id=t3-js-thread", token=token)[1]["messages"]
                gate = asyncio.Event()
                browser = RTCPeerConnection()
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
                    await ws.send(json.dumps({"type": "voice-media", "data": {
                        "session_id": session, "path": "socket"}}))
                    await complete_device_turn(ws, session, pcm)
                    after = request(port, "GET", "/api/presentation/history?thread_id=t3-js-thread", token=token)[1]["messages"]
                    own = [row for row in after if row["role"] == "user" and row["session"] == session]
                    assert len(after) == len(before) + 2 and len(own) == 2, own
                finally:
                    await browser.close()
            print("T5 PASS: recorded speech through native SmartTurn and timer, device STT/TTS, pinned JS reply, real WebRTC to WS switch")
        except Exception:
            if core.poll() is None:
                core.terminate()
                core.wait(timeout=10)
            print(core.stderr.read()[-4000:], file=sys.stderr)
            raise
        finally:
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
