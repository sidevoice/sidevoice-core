"""The microphone over WebRTC, negotiated for real: an aiortc peer stands in for the browser, sends a
recorded voice as its audio track, and the node's pipeline hears the turn from the track exactly as it
would from the socket. What cannot be exercised here is a real browser's RTCPeerConnection, a real
microphone and ICE across real networks (this pod has neither a browser nor an audio device)."""
import asyncio
import fractions
import json
import os
import socket
import tempfile
import time
import unittest
import wave
from pathlib import Path
from unittest.mock import patch

import aiohttp
import av
import numpy as np
import uvicorn
from aiortc import RTCPeerConnection, RTCSessionDescription
from aiortc.mediastreams import AudioStreamTrack, MediaStreamError

from sidevoice_core.control.history import RoomHistory
from sidevoice_core.control.room import Room
from sidevoice_core.pipeline.serializer import BrowserFrameSerializer
from sidevoice_core.server.app import create_app

SPEECH = Path(__file__).parent / 'fixtures' / 'hola-sala-16k.wav'


def free_port():
    with socket.socket() as probe:
        probe.bind(('127.0.0.1', 0))
        return probe.getsockname()[1]


async def until(check, timeout=20.0, every=0.05):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = check()
        if value:
            return value
        await asyncio.sleep(every)
    raise AssertionError('timed out waiting')


class RecordedVoice(AudioStreamTrack):
    """A microphone that says one sentence, then stays silent: 20 ms frames of 48 kHz mono, in real time."""

    def __init__(self, pcm16k):
        super().__init__()
        samples = np.frombuffer(pcm16k, dtype=np.int16)
        self.pcm = np.repeat(samples, 3).astype(np.int16)   # 16 kHz → 48 kHz, crude and good enough
        self.at = 0
        self.started = None

    async def recv(self):
        if self.readyState != 'live':
            raise MediaStreamError
        step = 960
        if self.started is None:
            self.started = time.monotonic()
        wait = self.started + self.at / 48000 - time.monotonic()
        if wait > 0:
            await asyncio.sleep(wait)
        chunk = self.pcm[self.at:self.at + step]
        if len(chunk) < step:
            chunk = np.concatenate([chunk, np.zeros(step - len(chunk), dtype=np.int16)])
        frame = av.AudioFrame.from_ndarray(chunk.reshape(1, -1), format='s16', layout='mono')
        frame.sample_rate, frame.pts, frame.time_base = 48000, self.at, fractions.Fraction(1, 48000)
        self.at += step
        return frame


class WebRTCTest(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.room = Room(RoomHistory(Path(self.temp.name) / 'room-state.json'))
        self.room.journal.register_binding('connector-a', harness='claude', thread='thread-a', title='A')
        self.port = free_port()
        self.base = f'http://127.0.0.1:{self.port}'
        # Host candidates only: the test stays off the network, and both peers are on this machine.
        self.environment = patch.dict(os.environ, {'SIDEVOICE_STUN_URLS': ''})
        self.environment.start()
        app = create_app(self.room, config={'VOICE_BROWSER_HEARTBEAT_SECONDS': '0'}, device_auth=False)
        self.server = uvicorn.Server(uvicorn.Config(app, host='127.0.0.1', port=self.port, log_level='warning'))
        self.task = asyncio.create_task(self.server.serve())
        await until(lambda: self.server.started)
        self.http = aiohttp.ClientSession()

    async def asyncTearDown(self):
        await self.http.close()
        self.server.should_exit = True
        await self.task
        self.environment.stop()
        self.temp.cleanup()

    async def call(self):
        ws = await self.http.ws_connect(self.base.replace('http', 'ws') + '/api/presentation/ws')
        await ws.send_str(json.dumps({'label': 'rtvi-ai', 'type': 'client-ready', 'id': 'x', 'data': {
            'settings': {'stt': {'place': 'device', 'model': 'whisper-tiny'}, 'turn_patience': 'fast'},
            'conversation': 'thread-a',
            'transcription': {'model': 'whisper-tiny', 'engine': 'transformers-js', 'accelerator': 'wasm', 'cached': True}}}))
        messages = []

        async def listen():
            async for frame in ws:
                if frame.type == aiohttp.WSMsgType.TEXT:
                    messages.append(json.loads(frame.data))
        reader = asyncio.create_task(listen())
        self.addAsyncCleanup(self.hang_up, ws, reader)
        session = (await until(lambda: next((m for m in messages if m['type'] == 'voice-session'), None)))['data']['session_id']
        return ws, messages, session

    @staticmethod
    async def hang_up(ws, reader):
        reader.cancel()
        await ws.close()

    async def test_the_config_says_what_this_node_takes(self):
        config = await (await self.http.get(self.base + '/api/presentation/rtc/config')).json()
        self.assertEqual(config, {'enabled': True, 'ice_servers': []})
        with patch.dict(os.environ, {'SIDEVOICE_STUN_URLS': 'stun:stun.example:3478'}):
            config = await (await self.http.get(self.base + '/api/presentation/rtc/config')).json()
        self.assertEqual(config['ice_servers'], [{'urls': ['stun:stun.example:3478']}])
        with patch.dict(os.environ, {'SIDEVOICE_WEBRTC': 'off'}):
            self.assertEqual((await (await self.http.get(self.base + '/api/presentation/rtc/config')).json())['enabled'], False)
            refused = await self.http.post(self.base + '/api/presentation/rtc/offer', json={'session_id': 's', 'sdp': 'v=0', 'type': 'offer'})
            self.assertEqual(refused.status, 503)

    async def test_an_offer_for_a_call_that_is_not_here_is_refused(self):
        answer = await self.http.post(self.base + '/api/presentation/rtc/offer', json={'session_id': 'nobody', 'sdp': 'v=0', 'type': 'offer'})
        self.assertEqual(answer.status, 409)

    async def test_a_turn_spoken_over_the_track_is_heard_like_one_spoken_over_the_socket(self):
        ws, messages, session = await self.call()
        with wave.open(str(SPEECH)) as recording:
            pcm = recording.readframes(recording.getnframes())
        voice = RecordedVoice(pcm + b'\x00\x00' * 16000 * 4)
        browser = RTCPeerConnection()
        self.addAsyncCleanup(browser.close)
        browser.addTrack(voice)
        await browser.setLocalDescription(await browser.createOffer())   # aiortc gathers before it returns
        answer = await self.http.post(self.base + '/api/presentation/rtc/offer', json={
            'session_id': session, 'sdp': browser.localDescription.sdp, 'type': 'offer'})
        self.assertEqual(answer.status, 200, await answer.text())
        await browser.setRemoteDescription(RTCSessionDescription(**await answer.json()))
        await until(lambda: browser.connectionState == 'connected', timeout=20)
        # The page names the path once it is connected; from then on the track is the microphone.
        await ws.send_str(json.dumps({'type': 'voice-media', 'data': {'session_id': session, 'path': 'webrtc'}}))
        ask = await until(lambda: next((m for m in messages if m['type'] == 'voice-transcribe'), None), timeout=30)
        self.assertGreater(len(ask['data']['audio_base64']), 10_000, 'the turn heard over the track came back as a WAV')
        await ws.send_str(json.dumps({'type': 'voice-transcript', 'data': {'session_id': session, 'request_id': ask['data']['request_id'],
                                                                         'text': 'Hola, esto es una prueba de voz de la sala.'}}))
        await until(lambda: self.room.journal.history('thread-a'), timeout=15)
        self.assertEqual(self.room.journal.history('thread-a')[0]['text'], 'Hola, esto es una prueba de voz de la sala.')
        client = self.room.clients[session]
        self.assertEqual(client.snapshot()['mic']['media'], 'webrtc')
        self.assertGreater(client.media_peer.frames, 100, 'the audio came from the track')
        # Hanging up closes the peer connection with the call.
        peer = client.media_peer
        await ws.close()
        await until(lambda: not self.room.clients, timeout=15)
        await until(lambda: peer.pc.connectionState == 'closed', timeout=15)
        self.assertIsNone(client.media_peer)


class MediaPathTests(unittest.TestCase):
    """One voice is never heard twice: the page's last word on the path decides which frames are audio."""

    def run_async(self, coroutine):
        return asyncio.new_event_loop().run_until_complete(coroutine)

    def test_socket_pcm_is_not_audio_while_the_page_says_webrtc_and_is_again_after_falling_back(self):
        serializer = BrowserFrameSerializer()
        pcm = b'\x01\x00' * 320
        self.assertIsNotNone(self.run_async(serializer.deserialize(pcm)))
        self.run_async(serializer.deserialize(json.dumps({'type': 'voice-media', 'data': {'path': 'webrtc'}})))
        before = serializer.last_frame_at
        time.sleep(0.01)
        self.assertIsNone(self.run_async(serializer.deserialize(pcm)), 'the track is the microphone now')
        self.assertGreater(serializer.last_frame_at, before, 'a frame still proves the page is there')
        self.run_async(serializer.deserialize(json.dumps({'type': 'voice-media', 'data': {'path': 'socket'}})))
        self.assertIsNotNone(self.run_async(serializer.deserialize(pcm)), 'the relay path is the microphone again')
        self.run_async(serializer.deserialize(json.dumps({'type': 'voice-media', 'data': {'path': 'carrier-pigeon'}})))
        self.assertEqual(serializer.media_path, 'socket', 'an unknown path changes nothing')


if __name__ == '__main__':
    unittest.main()
