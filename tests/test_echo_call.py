"""The opt-in echo rides the call pipeline without joining the room or delivering a turn."""
import asyncio
import base64
import json
import tempfile
import wave
from pathlib import Path
from unittest import IsolatedAsyncioTestCase
from unittest.mock import patch

from starlette.testclient import TestClient
from starlette.websockets import WebSocketDisconnect, WebSocketState

SPEECH = Path(__file__).parent / 'fixtures' / 'hola-sala-16k.wav'


class FakeWebSocket:
    def __init__(self):
        self.client_state = self.application_state = WebSocketState.CONNECTED
        self.headers = {}
        self.incoming, self.sent = asyncio.Queue(), asyncio.Queue()
        self.sent_log = []
        self.close_code = None

    async def receive(self):
        return await self.incoming.get()

    async def send_text(self, value):
        self.sent_log.append(value)
        self.sent.put_nowait(value)

    async def send_bytes(self, value):
        self.sent.put_nowait(value)

    async def close(self, code=1000, reason=None):
        self.close_code = code
        self.client_state = self.application_state = WebSocketState.DISCONNECTED
        self.incoming.put_nowait({'type': 'websocket.disconnect', 'code': code})


class FakeProvider:
    kind = 'openai'

    def __init__(self, text='Hola desde el pipeline.', failure=None):
        self.text, self.failure, self.wavs = text, failure, []

    async def transcribe(self, wav):
        self.wavs.append(wav)
        if self.failure:
            raise self.failure
        from sidevoice_core.pipeline.transcribers import Transcript
        return Transcript(self.text)


class EchoCallTests(IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        from sidevoice_core.control.history import RoomHistory
        from sidevoice_core.control.room import Room
        self.temp = tempfile.TemporaryDirectory()
        self.room = Room(RoomHistory(Path(self.temp.name) / 'room-state.json'))
        self.room.journal.register_binding('connector-a', harness='claude', thread='thread-a', title='A')

    async def asyncTearDown(self):
        self.temp.cleanup()

    def settings(self, *, voice_place='elevenlabs', stt_place='openai', ui_language='es'):
        voice = ({'place': 'elevenlabs', 'model': 'eleven_flash_v2_5',
                  'options': {'voice': {'es': 'voice-a'}, 'speed': 1.0}}
                 if voice_place == 'elevenlabs' else
                 {'place': 'device', 'model': 'kokoro-82m-v1.0',
                  'options': {'voice': {'es': 'em_alex'}, 'speed': 1.0}})
        return {
            'ui_language': ui_language,
            'stt': ({'place': 'openai', 'model': 'gpt-4o-transcribe', 'options': {'language': 'es'}}
                    if stt_place == 'openai' else
                    {'place': 'device', 'model': 'whisper-tiny', 'options': {'language': 'es'}}),
            'tts': voice,
            'turn_patience': 'fast',
        }

    def hello(self, *, voice_place='elevenlabs', stt_place='openai', ui_language='es'):
        return {'test': 'echo', 'conversation': 'thread-a', 'settings': self.settings(
            voice_place=voice_place, stt_place=stt_place, ui_language=ui_language),
                'mic': {'turn_patience': 'fast'}}

    async def start(self, *, hello=None):
        from sidevoice_core.server.app import browser_call
        socket = FakeWebSocket()
        socket.incoming.put_nowait({'type': 'websocket.receive', 'text': json.dumps(
            {'type': 'client-ready', 'data': hello if hello is not None else self.hello()})})
        task = asyncio.create_task(browser_call(self.room, socket, {'VOICE_BROWSER_HEARTBEAT_SECONDS': '0'}))
        self.addAsyncCleanup(self.stop, socket, task)
        return socket, task

    async def stop(self, socket, task):
        if task.done():
            return
        socket.incoming.put_nowait({'type': 'websocket.disconnect', 'code': 1000})
        try:
            await asyncio.wait_for(task, 15)
        except (asyncio.CancelledError, asyncio.TimeoutError):
            task.cancel()

    async def event(self, socket, kind, timeout=20):
        while True:
            raw = await asyncio.wait_for(socket.sent.get(), timeout)
            if isinstance(raw, str):
                message = json.loads(raw)
                if message.get('type') == kind:
                    return message['data']

    def microphone(self, socket, *, speech=True):
        if speech:
            with wave.open(str(SPEECH), 'rb') as recording:
                pcm = recording.readframes(recording.getnframes())
            pcm += bytes(2 * 16_000 * 4)
        else:
            pcm = bytes(640 * 8)
        for offset in range(0, len(pcm), 640):
            socket.incoming.put_nowait({'type': 'websocket.receive', 'bytes': pcm[offset:offset + 640]})

    async def test_real_pipeline_echoes_a_final_transcript_as_spoken_audio_without_room_effects(self):
        from sidevoice_core.pipeline import transcription
        from sidevoice_core.pipeline.speech_filter import SpeechEvidence
        audio = b'fake but rendered output'
        provider = FakeProvider()
        with patch('sidevoice_core.pipeline.integrations.key', return_value='test-key'), \
                patch.object(transcription, 'build', return_value=provider), \
                patch('sidevoice_core.pipeline.speech_filter.SegmentSpeechGate.assess',
                      return_value=SpeechEvidence(True, 'deterministic_audio', 2_600, 0.99)), \
                patch('sidevoice_core.pipeline.synthesis.synthesize', return_value={
                    'mime_type': 'audio/mpeg', 'audio_base64': base64.b64encode(audio).decode('ascii'),
                    'timings_ms': {}, 'alignment': None}) as synthesize:
            socket, task = await self.start()
            session = await self.event(socket, 'echo.session')
            self.assertEqual(session['max_duration_seconds'], 60)
            self.microphone(socket)
            transcript = await self.event(socket, 'echo.transcript')
            speech = await self.event(socket, 'echo.speech')

            self.assertEqual(transcript['text'], 'Hola desde el pipeline.')
            self.assertEqual(speech['text'], 'Te he oído: Hola desde el pipeline.')
            self.assertEqual(speech['voice']['place'], 'elevenlabs')
            self.assertEqual(base64.b64decode(speech['audio_base64']), audio)
            self.assertTrue(provider.wavs, 'the call pipeline passed a final audio segment to the STT boundary')
            synthesize.assert_awaited_once()
            event_types = [json.loads(message)['type'] for message in socket.sent_log if isinstance(message, str)]
            self.assertNotIn('voice-user-turn', event_types, 'echo does not publish ordinary call turn events')
            self.assertIn('echo.transcript', event_types)
            self.assertIn('echo.speech', event_types)

            socket.incoming.put_nowait({'type': 'websocket.receive', 'text': json.dumps({
                'type': 'echo.speech-result', 'data': {'session_id': session['session_id'],
                                                       'request_id': speech['request_id'], 'status': 'played'}})})
            await asyncio.sleep(0.05)

        self.assertEqual(self.room.clients, {})
        self.assertEqual(self.room.journal.history('thread-a'), [])
        self.assertEqual([item['thread'] for item in self.room.journal.bindings()], ['thread-a'])
        self.assertEqual(socket.close_code, None, 'the call remains open until the device hangs up or the deadline')

    async def test_real_pipeline_uses_device_transcription_and_voice_stages(self):
        from sidevoice_core.pipeline.speech_filter import SpeechEvidence
        hello = self.hello(voice_place='device', stt_place='device')
        with patch('sidevoice_core.pipeline.speech_filter.SegmentSpeechGate.assess',
                   return_value=SpeechEvidence(True, 'deterministic_audio', 2_600, 0.99)):
            socket, task = await self.start(hello=hello)
            session = await self.event(socket, 'echo.session')
            self.microphone(socket)
            request = await self.event(socket, 'voice-transcribe')
            self.assertEqual(request['session_id'], session['session_id'])
            self.assertEqual(request['language'], 'es')
            socket.incoming.put_nowait({'type': 'websocket.receive', 'text': json.dumps({
                'type': 'voice-transcript', 'data': {'session_id': session['session_id'],
                                                     'request_id': request['request_id'],
                                                     'text': 'Hola desde el dispositivo.'}})})
            transcript = await self.event(socket, 'echo.transcript')
            speech = await self.event(socket, 'echo.speech')
            self.assertEqual(transcript['text'], 'Hola desde el dispositivo.')
            self.assertEqual(speech['voice']['place'], 'device')
            self.assertEqual(speech['text'], 'Te he oído: Hola desde el dispositivo.')
            socket.incoming.put_nowait({'type': 'websocket.receive', 'text': json.dumps({
                'type': 'echo.speech-result', 'data': {'session_id': session['session_id'],
                                                       'request_id': speech['request_id'], 'status': 'played'}})})
            await asyncio.sleep(0.05)
        self.assertEqual(self.room.clients, {})
        self.assertEqual(self.room.journal.history('thread-a'), [])

    async def test_device_hosted_voice_is_requested_from_the_page(self):
        from sidevoice_core.control.echo import EchoCall
        from sidevoice_core.pipeline.settings import settings_from
        from sidevoice_core.i18n import translate
        settings, _ = settings_from(self.settings(voice_place='device'))
        sent = []
        call = EchoCall('echo-session', settings=settings, config={}, send=sent.append)
        call.connected = True
        call.enqueue_input('Hola')
        await asyncio.sleep(0)
        request = next(message['data'] for message in sent if message['type'] == 'echo.speech')
        self.assertEqual(request['voice']['place'], 'device')
        self.assertEqual(request['voice']['model'], 'kokoro-82m-v1.0')
        self.assertEqual(request['text'], translate('echo.prefix', 'es', text='Hola'))
        call.receive_speech_result({'session_id': call.id, 'request_id': request['request_id'], 'status': 'played'})
        await asyncio.gather(*call.speech_tasks)
        self.assertEqual(call.spoken, 1)
        call.disconnect()

    async def test_echo_messages_use_the_requested_language_and_fall_back_to_english(self):
        from sidevoice_core.i18n import translate
        self.assertEqual(translate('echo.prefix', 'es', text='Hola'), 'Te he oído: Hola')
        self.assertEqual(translate('echo.prefix', 'fr', text='Hello'), 'I heard: Hello')

    async def test_transcription_and_voice_failures_use_keys_and_hide_provider_errors(self):
        from sidevoice_core.control.echo import EchoCall
        from sidevoice_core.pipeline.call import VoiceCall
        from sidevoice_core.pipeline.settings import settings_from, mic_settings
        from sidevoice_core.pipeline import transcription

        settings, _ = settings_from(self.settings())
        mic, _ = mic_settings(settings, {'turn_patience': 'fast'})
        sent = []

        class FailedTranscriber:
            on_message = None

            async def transcribe_turn(self, pcm=None):
                raise RuntimeError('provider secret must not reach the page')

        call = EchoCall('echo-session', settings=settings, config={}, send=sent.append)
        call.connected = True
        voice = VoiceCall(call, FailedTranscriber(), sent.append, settings=settings, mic=mic,
                          choice=transcription.resolve(settings, {'VOICE_STT_API_KEY': 'test-key'}),
                          runtime=None, config={}, vad_stop_secs=0.9, vad=None)
        voice.turn_started()
        await voice.turn_stopped()
        error = next(message['data'] for message in sent if message['type'] == 'echo.error')
        self.assertEqual(error['key'], 'echo.transcription-failed')
        self.assertNotIn('provider secret', json.dumps(error))
        voice.close()
        call.disconnect()

        sent = []
        device_settings, _ = settings_from(self.settings())
        call = EchoCall('echo-session', settings=device_settings, config={}, send=sent.append)
        call.connected = True
        call.enqueue_input('Hola')
        with patch('sidevoice_core.pipeline.synthesis.synthesize', side_effect=RuntimeError('provider secret')):
            await asyncio.gather(*call.speech_tasks)
        error = next(message['data'] for message in sent if message['type'] == 'echo.error')
        self.assertEqual(error['key'], 'echo.voice-failed')
        self.assertNotIn('provider secret', json.dumps(error))
        call.transcripts = 0
        self.assertEqual(call.deadline_error(0)['key'], 'echo.microphone-unavailable')
        self.assertEqual(call.deadline_error(1)['key'], 'echo.silence-timeout')
        call.disconnect()

    async def test_deadline_ends_and_cleans_up_a_quiet_echo_socket(self):
        from sidevoice_core.control import echo
        from sidevoice_core.pipeline import transcription
        provider = FakeProvider()
        with patch('sidevoice_core.pipeline.integrations.key', return_value='test-key'), \
                patch.object(transcription, 'build', return_value=provider), \
                patch.object(echo, 'MAX_SECONDS', 0.05):
            socket, task = await self.start()
            await self.event(socket, 'echo.session')
            error = await self.event(socket, 'echo.error')
            ended = await self.event(socket, 'echo.session-ended')
            await asyncio.wait_for(task, 10)
        self.assertEqual(error['key'], 'echo.microphone-unavailable')
        self.assertEqual(ended['reason'], 'deadline')
        self.assertEqual(socket.close_code, 1000)
        self.assertEqual(self.room.clients, {})

    async def test_unsupported_test_values_are_keyed_and_do_not_open_a_pipeline(self):
        socket, task = await self.start(hello={'test': 'unknown'})
        error = await self.event(socket, 'error')
        await task
        self.assertEqual(error['key'], 'echo.unsupported-test')
        self.assertEqual(socket.close_code, 1008)
        self.assertEqual(self.room.clients, {})

    async def test_echo_refuses_an_unavailable_stage_with_translatable_parameters(self):
        from sidevoice_core.pipeline import integrations
        with patch.object(integrations, 'key', return_value=None):
            socket, task = await self.start()
            error = await self.event(socket, 'error')
            await task
        self.assertEqual(error['key'], 'echo.stage-unavailable')
        self.assertEqual(error['params'], {'stage': 'transcription', 'reason': 'provider_key_missing',
                                           'provider': 'openai'})
        self.assertEqual(socket.close_code, 1008)

    async def test_call_socket_echo_still_requires_a_paired_device_token(self):
        from sidevoice_core.control.history import RoomHistory
        from sidevoice_core.control.room import Room
        from sidevoice_core.server.app import create_app

        data_dir = Path(self.temp.name) / 'core'
        room = Room(RoomHistory(data_dir / 'room-state.json'))
        app = create_app(room, config={'SIDEVOICE_CORE_DATA_DIR': str(data_dir),
                                        'VOICE_BROWSER_HEARTBEAT_SECONDS': '0'})
        with TestClient(app, base_url='http://127.0.0.1:8768') as client:
            with self.assertRaises(WebSocketDisconnect) as refused:
                with client.websocket_connect('ws://127.0.0.1:8768/api/presentation/ws',
                                              subprotocols=['sidevoice']) as socket:
                    socket.send_text(json.dumps({'type': 'client-ready', 'data': {'test': 'echo'}}))
                    socket.receive_text()
            self.assertEqual(refused.exception.code, 4401)

    async def test_authenticated_paired_device_can_open_the_echo_call_socket(self):
        from sidevoice_core.control.history import RoomHistory
        from sidevoice_core.control.room import Room
        from sidevoice_core.server.app import create_app

        data_dir = Path(self.temp.name) / 'paired-core'
        room = Room(RoomHistory(data_dir / 'room-state.json'))
        app = create_app(room, config={'SIDEVOICE_CORE_DATA_DIR': str(data_dir),
                                        'VOICE_BROWSER_HEARTBEAT_SECONDS': '0'})
        with TestClient(app, base_url='http://127.0.0.1:8768') as client:
            code = app.state.devices.issue_code()
            paired = client.post('/api/device/pair', json={'secret': code['payload']['secret'], 'name': 'Echo test'})
            self.assertEqual(paired.status_code, 200, paired.text)
            token = paired.json()['token']
            with client.websocket_connect('ws://127.0.0.1:8768/api/presentation/ws',
                                          subprotocols=['sidevoice', f'sidevoice.token.{token}']) as socket:
                socket.send_text(json.dumps({'type': 'client-ready', 'data': self.hello(
                    voice_place='device', stt_place='device')}))
                socket.receive_json()  # Existing call session frame.
                echo_session = socket.receive_json()
                self.assertEqual(echo_session['type'], 'echo.session')
                self.assertEqual(echo_session['data']['max_duration_seconds'], 60)
            self.assertEqual(room.clients, {})
            self.assertEqual(room.journal.history('thread-a'), [])
