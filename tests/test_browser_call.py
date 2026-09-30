import asyncio
import base64
import json
import os
import time
import tempfile
from pathlib import Path
from unittest import IsolatedAsyncioTestCase
from unittest.mock import patch
from starlette.websockets import WebSocketState


class FakeWebSocket:
    def __init__(self):
        self.client_state = self.application_state = WebSocketState.CONNECTED
        self.headers = {}
        self.incoming, self.sent = asyncio.Queue(), asyncio.Queue()

    async def receive(self):
        return await self.incoming.get()

    async def send_text(self, text):
        self.sent.put_nowait(text)

    async def send_bytes(self, data):
        self.sent.put_nowait(data)

    async def close(self, code=1000, reason=None):
        self.client_state = self.application_state = WebSocketState.DISCONNECTED
        self.incoming.put_nowait({'type': 'websocket.disconnect', 'code': code})


class FakeTranscriber:
    """Stands in for the pipeline's transcriber: hands back scripted results, one per finished turn."""

    def __init__(self, results, offline=()):
        self.results, self.calls = list(results), 0
        self.offline, self.offline_audio = list(offline), []
        self.turn_audio = b''   # what the open turn has heard so far, for a switch to take

    async def transcribe_audio(self, pcm, sample_rate=None):
        """The catch-up path: audio the room never heard live, recognised on its own."""
        self.offline_audio.append((pcm, sample_rate))
        result = self.offline.pop(0)
        if isinstance(result, Exception):
            raise result
        return result

    def take_turn_audio(self):
        pcm, self.turn_audio = self.turn_audio, b''
        return pcm

    async def transcribe_turn(self, pcm=None):
        self.calls += 1
        result = self.results.pop(0)
        if callable(result):
            return await result()
        if isinstance(result, Exception):
            raise result
        return result


PATIENT_HELLO = {'conversation': 'thread-a', 'mic': {'turn_patience': 'calm'},
                 'transcription': {'model': 'onnx-community/whisper-tiny', 'device': 'wasm'}}
TIMER_HELLO = PATIENT_HELLO  # kept while older tests still name it


class BrowserCallTest(IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        from sidevoice.room import Room
        from sidevoice.room_history import RoomHistory
        self.temp = tempfile.TemporaryDirectory()
        self.hub = Room(RoomHistory(Path(self.temp.name) / 'history.sqlite3'))
        self.hub.journal.register_binding('connector-a', harness='claude', thread='thread-a', title='A')
        self.patches = [patch('sidevoice.app.hub', self.hub)]
        for active in self.patches: active.start()

    async def asyncTearDown(self):
        for active in self.patches: active.stop()
        self.temp.cleanup()

    async def received(self, socket, kind, timeout=5):
        while True:
            raw = await asyncio.wait_for(socket.sent.get(), timeout)
            if isinstance(raw, (bytes, bytearray)):
                continue
            message = json.loads(raw)
            if message['type'] == kind: return message

    async def settled(self):
        for _ in range(20):
            await asyncio.sleep(0)

    def hello(self, socket, data=None):
        socket.incoming.put_nowait({'type': 'websocket.receive', 'text': json.dumps(
            {'label': 'rtvi-ai', 'type': 'client-ready', 'id': 'x', 'data': data if data is not None else TIMER_HELLO})})

    async def join(self, socket, hello=None):
        from sidevoice.app import browser_call
        task = asyncio.create_task(browser_call(socket))

        async def hang_up():
            # A failed assertion must still end the call the way a browser does, or the runner outlives the test.
            if not task.done():
                socket.incoming.put_nowait({'type': 'websocket.disconnect'})
                try:
                    await asyncio.wait_for(task, 5)
                except (asyncio.TimeoutError, asyncio.CancelledError, Exception):
                    task.cancel()
        self.addAsyncCleanup(hang_up)
        self.hello(socket, hello)
        session_id = (await self.received(socket, 'voice-session'))['data']['session_id']
        return task, self.hub.clients[session_id]

    async def leave(self, socket, task):
        socket.incoming.put_nowait({'type': 'websocket.disconnect'})
        await asyncio.wait_for(task, 5)

    # ----- the call: one pipeline, the device's settings, PCM in -----

    async def test_hello_configures_the_device_and_the_room_owns_its_turns(self):
        socket = FakeWebSocket()
        task, client = await self.join(socket)
        session = self.hub.snapshot(client.id)['call']
        self.assertEqual(session['mic_settings']['turn_end_mode'], 'smart_turn', "how a turn ends is the room's")
        self.assertEqual(session['mic_settings']['smart_turn_min_silence'], 1.3, 'this device asked for calm')
        self.assertEqual(session['mic_settings']['merge_window_secs'], 1.5)
        self.assertEqual(session['mic_settings']['vad_confidence'], 0.6)
        self.assertEqual(session['mic_settings']['vad_start_secs'], 0.4, "the onset is the room's, four tenths")
        self.assertEqual((session['transcription']['provider'], session['transcription']['model'],
                          session['transcription']['device']), ('browser', 'onnx-community/whisper-tiny', 'wasm'))
        self.assertEqual(session['mic']['transport'], 'pcm')
        self.assertEqual(client.stt.provider.kind, 'browser')
        self.assertIs(client.voice.transcriber, client.stt)
        # Microphone frames reach the room's pipeline instead of being refused.
        socket.incoming.put_nowait({'type': 'websocket.receive', 'bytes': bytes(640)})
        for _ in range(50):
            await asyncio.sleep(0.01)
            if client.mic.audio_frames:
                break
        self.assertEqual(client.mic.audio_frames, 1)
        self.assertTrue(socket.sent.empty() or all(
            json.loads(m)['type'] != 'error' for m in list(socket.sent._queue) if isinstance(m, str)))
        await self.leave(socket, task)
        self.assertFalse(client.connected)
        self.assertEqual(self.hub.clients, {})

    async def test_smart_turn_is_the_default_and_builds_the_analyzer(self):
        from sidevoice.app import turn_stop_strategy
        from sidevoice.language_settings import LanguageSettings, MicSettings, mic_settings
        from pipecat.turns.user_stop.turn_analyzer_user_turn_stop_strategy import TurnAnalyzerUserTurnStopStrategy
        from pipecat.turns.user_stop.speech_timeout_user_turn_stop_strategy import SpeechTimeoutUserTurnStopStrategy
        mic, problem = mic_settings(LanguageSettings(), {})
        self.assertIsNone(problem)
        self.assertEqual(mic.turn_end_mode, 'smart_turn')
        strategy = turn_stop_strategy(mic, {})
        self.assertIsInstance(strategy, TurnAnalyzerUserTurnStopStrategy)
        self.assertFalse(strategy.wait_for_transcript)
        self.assertEqual(strategy._turn_analyzer.params.stop_secs, 3.0)
        from sidevoice.app import vad_analyzer
        self.assertEqual(vad_analyzer(mic, {}).params.stop_secs, 0.9)
        self.assertEqual(vad_analyzer(mic, {}).params.start_secs, 0.4)
        self.assertEqual(vad_analyzer(mic_settings(LanguageSettings(), {'vad_start_secs': 0.05})[0], {}).params.start_secs, 0.4,
                         'a device cannot tune the detector: that is the room\'s, fixed in one place for everyone')
        floor, _ = mic_settings(LanguageSettings(), {'smart_turn_min_silence': 1.2})
        self.assertEqual(vad_analyzer(floor, {}).params.stop_secs, 0.9, 'the floor is the room\'s too')
        calm, _ = mic_settings(LanguageSettings(), {'turn_patience': 'calm'})
        self.assertEqual(vad_analyzer(calm, {}).params.stop_secs, 1.3, 'patience is what a device may ask for')
        # The timer strategy is still built when the room itself is configured that way.
        timer = MicSettings(turn_end_mode='timer', user_speech_timeout=4)
        self.assertIsInstance(turn_stop_strategy(timer, {}), SpeechTimeoutUserTurnStopStrategy)

    async def test_the_device_brings_every_setting_and_can_update_the_live_ones(self):
        socket = FakeWebSocket()
        task, client = await self.join(socket, {'settings': {'stt_provider': 'browser', 'stt_model': 'onnx-community/whisper-base',
                                                             'spanish_voice': 'em_alex', 'audio_grace_seconds': 4,
                                                             'turn_patience': 'fast'}})
        self.assertEqual(client.settings.spanish_voice, 'em_alex')
        self.assertEqual(client.audio_grace_seconds, 4)
        self.assertEqual(client.transcription['model'], 'onnx-community/whisper-base')
        self.assertEqual(client.mic_settings['user_speech_timeout'], 2.0, 'fast is a whole shape, not one number')
        # Voices and grace change without a reconnect; invalid updates are refused and reported.
        client.voice.browser_message({'type': 'voice-settings', 'data': {'session_id': client.id, 'settings': {'spanish_voice': 'ef_dora', 'audio_grace_seconds': 1}}})
        self.assertEqual((client.settings.spanish_voice, client.audio_grace_seconds), ('ef_dora', 1))
        client.voice.browser_message({'type': 'voice-settings', 'data': {'session_id': client.id, 'settings': {'tts_speed': 9}}})
        self.assertEqual(client.settings.spanish_voice, 'ef_dora')
        self.assertIn('device settings were not valid', (await self.received(socket, 'error'))['data']['message'])
        # A voice or an engine is resolved per utterance out of these settings, so the change lands on
        # the next reply over this very socket: no second pipeline, and nothing to reconnect.
        from sidevoice.language_settings import resolve_voice
        self.assertEqual(resolve_voice(client.settings, 'es')['voice'], 'ef_dora')
        client.voice.browser_message({'type': 'voice-settings', 'data': {'session_id': client.id, 'settings': {
            'default_model': 'eleven_flash_v2_5', 'default_voice': 'una-voz', 'spanish_voice': 'inherit', 'tts_speed': 1.1}}})
        chosen = resolve_voice(client.settings, 'es')
        self.assertEqual((chosen['provider'], chosen['voice'], chosen['speed']), ('elevenlabs', 'una-voz', 1.1))
        self.assertEqual(list(self.hub.clients), [client.id], 'a voice change never opens a second session')
        # And a provider key is read where the audio is made, not where the pipeline was built.
        from sidevoice import synthesis
        with patch.object(synthesis, 'key', return_value=None):
            with self.assertRaises(ValueError):
                await synthesis.synthesize('Hola', model='eleven_flash_v2_5', voice='una-voz', speed=1.0)
        await self.leave(socket, task)

    async def test_how_turns_are_detected_is_the_rooms_and_a_device_that_sends_its_own_is_ignored(self):
        # A device that saved the old controls (an old page, a curious user) kept them after the room had
        # changed its mind, and the fix never reached the person it was written for (2026-09-20). Now the
        # room's numbers are the room's, and the only thing a device says about turns is how patient it wants
        # the room to be with it.
        socket = FakeWebSocket()
        task, client = await self.join(socket, {'mic': {'turn_end_mode': 'timer', 'user_speech_timeout': 99,
                                                        'smart_turn_min_silence': 0.1, 'vad_confidence': 5,
                                                        'vad_start_secs': 0.05}})
        self.assertEqual(client.mic_settings['turn_end_mode'], 'smart_turn')
        self.assertEqual(client.mic_settings['user_speech_timeout'], 2.5)
        self.assertEqual(client.mic_settings['smart_turn_min_silence'], 0.9)
        self.assertEqual(client.mic_settings['vad_confidence'], 0.6)
        self.assertEqual(client.mic_settings['vad_start_secs'], 0.4)
        await self.leave(socket, task)

    async def test_patience_is_the_one_turn_choice_a_device_makes(self):
        socket = FakeWebSocket()
        task, client = await self.join(socket, {'mic': {'turn_patience': 'calm'}})
        self.assertEqual(client.mic_settings['smart_turn_min_silence'], 1.3)
        self.assertEqual(client.mic_settings['merge_window_secs'], 1.5)
        self.assertEqual(client.mic_settings['vad_start_secs'], 0.4, "patience does not touch the detector's onset")
        await self.leave(socket, task)

    async def test_a_patience_this_room_does_not_know_keeps_the_rooms_own(self):
        socket = FakeWebSocket()
        task, client = await self.join(socket, {'mic': {'turn_patience': 'zen'}})
        error = await self.received(socket, 'error')
        self.assertIn('Unknown patience', error['data']['message'])
        self.assertEqual(client.mic_settings['smart_turn_min_silence'], 0.9)
        await self.leave(socket, task)

    async def test_a_gpu_fallback_reported_by_the_browser_is_kept_with_its_reason(self):
        socket = FakeWebSocket()
        task, client = await self.join(socket, {'mic': TIMER_HELLO['mic'], 'transcription': {
            'model': 'onnx-community/whisper-tiny', 'device': 'wasm', 'fallback_from': 'webgpu', 'fallback_error': 'GPU adapter lost'}})
        self.assertEqual((client.transcription['device'], client.transcription['fallback_from'], client.transcription['fallback_error']),
                         ('wasm', 'webgpu', 'GPU adapter lost'))
        await self.leave(socket, task)

    async def test_incompatible_runtime_is_rejected_but_the_call_stays(self):
        socket = FakeWebSocket()
        task, client = await self.join(socket, {'mic': TIMER_HELLO['mic'],
                                                'transcription': {'model': 'server-whisper', 'device': 'cuda'}})
        error = await self.received(socket, 'error')
        self.assertIn('Unsupported', error['data']['message'])
        self.assertTrue(client.connected)
        # The rejected runtime never overrides what the room resolved.
        self.assertEqual((client.transcription['model'], client.transcription['device']), ('onnx-community/whisper-tiny', 'auto'))
        await self.leave(socket, task)

    async def test_openai_provider_is_built_from_the_saved_key(self):
        socket = FakeWebSocket()
        choice = {'provider': 'openai', 'available': True, 'model': 'gpt-4o-transcribe', 'reason': 'explicit'}
        with patch('sidevoice.app.transcription.resolve', return_value=choice), \
                patch('sidevoice.transcription.stored_key', return_value='sk-test-not-used'):
            task, client = await self.join(socket)
            self.assertEqual(client.stt.provider.kind, 'openai')
            self.assertEqual(client.stt.provider.model, 'gpt-4o-transcribe')
            await self.leave(socket, task)

    async def test_openai_without_key_fails_before_accepting_audio(self):
        from sidevoice.app import browser_call
        socket = FakeWebSocket()
        choice = {'provider': 'openai', 'available': False, 'model': 'gpt-4o-transcribe'}
        with patch('sidevoice.app.transcription.resolve', return_value=choice):
            await browser_call(socket)
        error = json.loads(socket.sent.get_nowait())
        self.assertEqual(error['type'], 'error')
        self.assertIn('clave de API', error['data']['message'])
        self.assertEqual(socket.application_state, WebSocketState.DISCONNECTED)

    # ----- what a turn does, whatever closed it -----

    def voice(self, results, session_id='s1', offline=()):
        from sidevoice.app import VoiceCall
        from sidevoice.room import RoomClient
        from sidevoice.language_settings import LanguageSettings, MicSettings
        sent = []
        client = RoomClient(session_id, self.hub)
        client.connected = True
        client.target = {'thread_id': 'thread-a', 'title': 'A', 'binding_id': 'bind-a'}
        voice = VoiceCall(client, FakeTranscriber(results, offline), sent.append, settings=LanguageSettings(), mic=MicSettings(),
                          choice={'provider': 'browser', 'model': 'onnx-community/whisper-tiny', 'reason': 'explicit'},
                          runtime={'model': 'onnx-community/whisper-tiny', 'device': 'webgpu'})
        return voice, client, sent

    async def test_the_session_message_and_the_snapshot_say_which_build_the_room_serves(self):
        from unittest.mock import patch as patch_
        from sidevoice.browser_socket import session_message, BrowserFrameSerializer
        with patch_('sidevoice.paths.build_info', return_value={'version': '9.9.9', 'web_build': 'abc123'}):
            message = session_message('call-1', BrowserFrameSerializer())
            self.assertEqual(message['data']['room'], {'version': '9.9.9', 'web_build': 'abc123'})
            self.assertEqual(self.hub.snapshot()['room']['web_build'], 'abc123')
            self.assertEqual(self.hub.snapshot()['room']['version'], '9.9.9')

    async def test_the_browser_can_report_its_audio_output_and_the_room_shows_it_for_that_call(self):
        voice, client, sent = self.voice([])
        report = {'type': 'voice-audio-health', 'data': {'session_id': client.id, 'reason': 'stall', 'health': {
            'context': 'running', 'clock': 12.5, 'output': 'element', 'element': {'paused': False, 'readyState': 4},
            'playing': True, 'stalls': 1, 'resuming': True, 'events': [{'at': 1, 'kind': 'stall', 'detail': 'running · element playing · 1'}, 'junk']}}}
        voice.browser_message(report)
        shown = client.snapshot()['audio_health']
        self.assertEqual((shown['reason'], shown['stalls'], shown['context'], shown['output']), ('stall', 1, 'running', 'element'))
        self.assertEqual(shown['events'], [{'at': 1, 'kind': 'stall', 'detail': 'running · element playing · 1'}])
        self.assertTrue(shown['at'] > 0)
        self.assertEqual(self.hub.snapshot()['room']['audio_reports'][-1]['session_id'], client.id, 'the report outlives the browser')
        # Another call's report never lands here.
        voice.browser_message({**report, 'data': {**report['data'], 'session_id': 'someone-else', 'reason': 'fail'}})
        self.assertEqual(client.snapshot()['audio_health']['reason'], 'stall')

    async def test_an_uncaught_interface_error_is_kept_by_the_room_and_carries_no_transcript(self):
        voice, client, sent = self.voice([])
        voice.browser_message({'type': 'voice-client-error', 'data': {
            'session_id': client.id, 'kind': 'render:transcript', 'message': "null is not an object",
            'stack': 'at reconcileSession', 'component': 'at MessageList', 'build': 'mu8y5e4k'}})
        kept = self.hub.snapshot()['room']['client_errors'][-1]
        self.assertEqual((kept['session_id'], kept['kind'], kept['message']),
                         (client.id, 'render:transcript', 'null is not an object'))
        self.assertEqual((kept['component'], kept['build']), ('at MessageList', 'mu8y5e4k'))
        self.assertTrue(kept['at'] > 0)
        self.assertNotIn('text', kept, 'an error report never carries what anyone said')
        # Another call's report is not this call's business, like every other browser message.
        voice.browser_message({'type': 'voice-client-error', 'data': {'session_id': 'someone-else', 'message': 'otro'}})
        self.assertEqual(len(self.hub.snapshot()['room']['client_errors']), 1)

    async def test_a_finished_turn_is_transcribed_once_and_delivered(self):
        from sidevoice.transcribers import Transcript
        voice, client, sent = self.voice([Transcript('Hola desde el navegador', metrics={'audio_ms': 850, 'recognition_ms': 120})])
        voice.turn_started()
        # Starting to speak also cancels this browser's own pending audio, like everyone else's.
        self.assertEqual([m['type'] for m in sent], ['voice-cancel', 'voice-user-turn'])
        self.assertEqual(sent[1]['data'], {'phase': 'started', 'revision': 1, 'thread_id': 'thread-a'})
        self.assertTrue(client.speaking)
        await voice.turn_stopped()
        phases = [m['data']['phase'] for m in sent if m['type'] == 'voice-user-turn']
        self.assertEqual(phases, ['started', 'finished'])
        finished = [m for m in sent if m['type'] == 'voice-user-turn'][-1]['data']
        self.assertEqual((finished['text'], finished['revision']), ('Hola desde el navegador', 1))
        rows = self.hub.journal.history('thread-a')
        self.assertEqual(rows[-1]['text'], 'Hola desde el navegador')
        measured = client.latency.turns[('thread-a', 1)]['input_ms']
        self.assertEqual((measured['audio_ms'], measured['recognition_ms']), (850, 120))
        self.assertGreaterEqual(measured['transcript_to_delivery_ms'], 0)
        self.assertEqual((client.input_stats['turns'], client.input_stats['recognition_ms'], client.input_stats['pending']), (1, 120, 0))
        self.assertFalse(client.speaking)
        self.assertEqual(voice.transcriber.calls, 1)

    async def test_a_model_switch_in_the_browser_updates_what_the_room_reports(self):
        voice, client, sent = self.voice([])
        voice.browser_message({'type': 'voice-stt-ready', 'data': {'session_id': client.id, 'model': 'onnx-community/whisper-small', 'device': 'webgpu'}})
        self.assertEqual(client.transcription['model'], 'onnx-community/whisper-small')
        voice.browser_message({'type': 'voice-stt-ready', 'data': {'session_id': 'other', 'model': 'onnx-community/whisper-tiny', 'device': 'wasm'}})
        self.assertEqual(client.transcription['model'], 'onnx-community/whisper-small')
        voice.browser_message({'type': 'voice-stt-ready', 'data': {'session_id': client.id, 'model': 'server-whisper', 'device': 'cuda'}})
        self.assertEqual(sent[-1]['type'], 'error')
        self.assertEqual(client.transcription['model'], 'onnx-community/whisper-small')

    async def test_empty_failed_or_cancelled_turns_are_not_delivered(self):
        from sidevoice.transcribers import Transcript
        voice, client, sent = self.voice([Transcript(''), RuntimeError('worker died'), Transcript('Descarta esto')])
        for expectation in ('empty', 'failed', 'cancelled'):
            del sent[:]
            voice.turn_started()
            if expectation == 'cancelled':
                client.cancelled_turn = client.turn_revision
            await voice.turn_stopped()
            outcome = [m for m in sent if m['type'] == 'voice-user-turn'][-1]['data']
            self.assertEqual(outcome['phase'], 'cancelled', expectation)
            self.assertFalse(client.speaking, expectation)
        self.assertEqual(self.hub.journal.history('thread-a'), [])
        errors = [m for m in sent if m['type'] == 'error']
        self.assertEqual(errors, [])
        self.assertIn('worker died', client.error)

    async def test_a_turn_cut_while_the_user_kept_going_joins_the_next_one(self):
        from sidevoice.transcribers import Transcript
        async def slow():
            await asyncio.sleep(0.05)
            return Transcript('Pero bueno,')
        voice, client, sent = self.voice([slow, Transcript('lo que te iba a proponer es otra cosa.'), Transcript('Y esto va aparte.')])
        voice.turn_started(); first = voice.turn_stopped()
        voice.turn_started(); second = voice.turn_stopped()   # resumed before the first text was delivered
        await asyncio.gather(first, second)
        rows = self.hub.journal.history('thread-a')
        self.assertEqual([(r['text'], r['revision']) for r in rows], [('Pero bueno, lo que te iba a proponer es otra cosa.', 2)])
        turns = [m['data'] for m in sent if m['type'] == 'voice-user-turn']
        self.assertEqual([(t['phase'], t['revision'], t.get('merged', False)) for t in turns],
                         [('started', 1, False), ('started', 2, False), ('cancelled', 1, True), ('finished', 2, False)])
        self.assertFalse(client.speaking)
        # A turn that starts after delivery is its own message.
        voice.turn_started(); await voice.turn_stopped()
        self.assertEqual([r['text'] for r in self.hub.journal.history('thread-a')][-1], 'Y esto va aparte.')

    async def test_switching_conversation_mid_turn_delivers_what_was_said_to_the_one_it_was_said_to(self):
        # Seen on 2026-09-26: words spoken to one conversation arrived at the next one selected (#93).
        from unittest.mock import AsyncMock
        from sidevoice.transcribers import Transcript
        voice, client, sent = self.voice([Transcript('Esto era para A.'), Transcript('Y esto para B.')])
        client.worker = AsyncMock()
        voice.turn_started()
        voice.transcriber.turn_audio = b'pcm'
        await self.hub._retarget(client, {'thread_id': 'thread-b', 'title': 'B'})
        await asyncio.gather(*voice.finishing)
        self.assertEqual([r['text'] for r in self.hub.journal.history('thread-a')], ['Esto era para A.'])
        self.assertTrue(client.speaking, 'the person is still talking: the switch closed a turn, not the microphone')
        opened = [m['data'] for m in sent if m['type'] == 'voice-user-turn' and m['data']['phase'] == 'started'][-1]
        self.assertEqual(opened, {'phase': 'started', 'revision': client.turn_revision, 'thread_id': 'thread-b'},
                         'and the page is told, or it shows a microphone that is not recording')
        await voice.turn_stopped()
        self.assertEqual([r['text'] for r in self.hub.journal.history('thread-b')], ['Y esto para B.'])
        self.assertEqual([r['text'] for r in self.hub.journal.history('thread-a')], ['Esto era para A.'])
        self.assertFalse(client.speaking)

    async def test_a_turn_waiting_for_a_breath_is_not_joined_to_a_turn_spoken_to_another_conversation(self):
        from sidevoice.transcribers import Transcript
        voice, client, sent = self.voice([Transcript('Lo de A.'), Transcript('Lo de B.')])
        voice.merge_window = 0.2
        voice.turn_started()
        first = asyncio.ensure_future(voice.turn_stopped())
        await asyncio.sleep(0.05)
        await self.hub._retarget(client, {'thread_id': 'thread-b', 'title': 'B'})
        voice.turn_started()                      # carried on, but to the other conversation
        second = voice.turn_stopped()
        await asyncio.gather(first, second)
        self.assertEqual([r['text'] for r in self.hub.journal.history('thread-a')], ['Lo de A.'])
        self.assertEqual([r['text'] for r in self.hub.journal.history('thread-b')], ['Lo de B.'])

    async def test_switching_with_nothing_said_changes_nothing(self):
        voice, client, sent = self.voice([])
        await self.hub._retarget(client, {'thread_id': 'thread-b', 'title': 'B'})
        self.assertEqual((voice.finishing, voice.transcriber.calls), (set(), 0))

    async def test_the_bar_to_open_a_turn_rises_while_this_browser_is_playing_a_reply(self):
        # The room answered itself on 2026-09-20: its own voice out of the phone's speaker opened a turn,
        # cut the reply that was still playing and came back as a message with the room's own words.
        from sidevoice.app import SPEAKING_MIN_VOLUME
        voice, client, sent = self.voice([])
        changes = []
        voice.vad = type('FakeVAD', (), {'set_params': lambda _self, params: changes.append(params.min_volume)})()
        voice.mic = type('Mic', (), {'vad_start_secs': 0.4, 'vad_confidence': 0.6, 'vad_min_volume': 0.5})()
        voice.listening_bar(True)
        voice.listening_bar(True)
        self.assertEqual(changes, [SPEAKING_MIN_VOLUME], 'raised once, not on every frame of the same reply')
        voice.listening_bar(False)
        self.assertEqual(changes, [SPEAKING_MIN_VOLUME, 0.5], 'and it comes back down when the room is quiet')

    async def test_a_finished_turn_waits_a_moment_in_case_the_pause_was_a_breath(self):
        # Asked for in the room on 2026-09-20: the detector will sometimes end a turn mid-sentence, and two
        # halves of one thought arriving as two messages is worse than answering a moment later.
        from sidevoice.transcribers import Transcript
        voice, client, sent = self.voice([Transcript('Lo que te quería decir'), Transcript('es que esto va junto.')])
        voice.merge_window = 0.2
        voice.turn_started()
        first = asyncio.ensure_future(voice.turn_stopped())
        await asyncio.sleep(0.05)
        self.assertEqual(self.hub.journal.history('thread-a'), [], 'nothing is delivered while the window is open')
        voice.turn_started()                      # the person carried on inside the window
        second = voice.turn_stopped()
        await asyncio.gather(first, second)
        rows = self.hub.journal.history('thread-a')
        self.assertEqual([r['text'] for r in rows], ['Lo que te quería decir es que esto va junto.'])

    async def test_a_turn_nobody_resumes_is_delivered_once_the_window_closes(self):
        from sidevoice.transcribers import Transcript
        voice, client, sent = self.voice([Transcript('Esto va solo.')])
        voice.merge_window = 0.1
        voice.turn_started()
        await voice.turn_stopped()
        self.assertEqual([r['text'] for r in self.hub.journal.history('thread-a')], ['Esto va solo.'])

    async def test_held_text_survives_a_noise_turn_but_not_an_explicit_cancel(self):
        from sidevoice.transcribers import Transcript
        async def slow():
            await asyncio.sleep(0.05)
            return Transcript('Sigo aquí')
        voice, client, sent = self.voice([slow, Transcript(''), slow, Transcript('nada')])
        voice.turn_started(); first = voice.turn_stopped()
        voice.turn_started(); second = voice.turn_stopped()   # noise: empty transcript, held text still delivered
        await asyncio.gather(first, second)
        self.assertEqual([r['text'] for r in self.hub.journal.history('thread-a')], ['Sigo aquí'])
        voice.turn_started(); third = voice.turn_stopped()
        voice.turn_started(); client.cancelled_turn = client.turn_revision; fourth = voice.turn_stopped()
        await asyncio.gather(third, fourth)
        self.assertEqual([r['text'] for r in self.hub.journal.history('thread-a')], ['Sigo aquí'])
        self.assertIsNone(voice.held)

    # ----- several browsers -----

    async def test_two_devices_stay_in_the_room_and_neither_ends_the_other(self):
        first_socket, second_socket = FakeWebSocket(), FakeWebSocket()
        first_task, first = await self.join(first_socket)
        second_task, second = await self.join(second_socket)

        self.assertNotEqual(first.id, second.id)
        self.assertEqual(len(self.hub.clients), 2)
        self.assertTrue(first.connected and second.connected)
        self.assertEqual(first_socket.application_state, WebSocketState.CONNECTED)

        # A microphone turn on one device is that device's alone: the other keeps its epoch and its audio.
        second.voice.turn_started()
        self.assertEqual((await self.received(second_socket, 'voice-cancel'))['data'],
                         {'session_id': second.id, 'revision': 1})
        await self.settled()
        self.assertFalse(any(json.loads(raw)['type'] == 'voice-cancel' for raw in list(first_socket.sent._queue)
                             if not isinstance(raw, (bytes, bytearray))))
        self.assertEqual((second.turn_revision, second.revision), (1, 1))
        self.assertEqual((first.turn_revision, first.revision), (0, 0))

        # One device leaving takes nothing else with it.
        await self.leave(first_socket, first_task)
        self.assertFalse(first.connected)
        self.assertTrue(second.connected)
        self.assertEqual(list(self.hub.clients), [second.id])
        self.assertEqual(second.revision, 1, 'the other device kept its own epoch')
        await self.leave(second_socket, second_task)
        self.assertEqual(self.hub.clients, {})

    async def test_one_device_may_hold_two_sockets_while_it_changes_what_the_pipeline_is_built_from(self):
        """Changing the transcription provider or the turn detection must not hang up.

        The browser opens the second socket with the new hello while the first still carries the
        call, and only lets the first go once the room has answered the second.
        """
        old_socket, new_socket = FakeWebSocket(), FakeWebSocket()
        old_task, old = await self.join(old_socket)
        self.assertEqual(old.transcription['provider'], 'browser')
        self.assertEqual(old.mic_settings['smart_turn_min_silence'], 1.3, 'this device asked for calm')

        choice = {'provider': 'openai', 'available': True, 'model': 'gpt-4o-transcribe', 'reason': 'explicit'}
        with patch('sidevoice.app.transcription.resolve', return_value=choice), \
                patch('sidevoice.transcription.stored_key', return_value='sk-test-not-used'):
            new_task, new = await self.join(new_socket, {
                'conversation': 'thread-a',
                'settings': {'stt_provider': 'openai', 'stt_model': 'gpt-4o-transcribe',
                             'turn_patience': 'fast'}})

        # Two sockets, two pipelines, two clients: the second is built from the new hello alone.
        self.assertNotEqual(old.id, new.id)
        self.assertEqual(len(self.hub.clients), 2)
        self.assertTrue(old.connected and new.connected)
        self.assertEqual(new.transcription['provider'], 'openai')
        self.assertEqual((new.mic_settings['smart_turn_min_silence'], new.mic_settings['vad_confidence']), (0.6, 0.6),
                         "the new hello brought its own patience; the detector stayed the room's")
        self.assertEqual(old.transcription['provider'], 'browser', 'the call still running is untouched')
        # The tab's conversation travelled in the hello, so the new session is already on it.
        self.assertEqual(new.target['thread_id'], 'thread-a')
        self.assertNotEqual(new.target['binding_id'], old.target['binding_id'])

        # The old one leaving is the end of the swap, and it disturbs nothing.
        new.user_started()
        await self.leave(old_socket, old_task)
        self.assertFalse(old.connected)
        self.assertEqual(list(self.hub.clients), [new.id])
        self.assertTrue(new.connected)
        self.assertEqual((new.revision, new.turn_revision), (1, 1))
        self.assertEqual(new.target['thread_id'], 'thread-a')
        new.speaking = False
        new.enqueue_input('Sigo hablando después del cambio')
        self.assertEqual(self.hub.journal.pending()[-1]['text'], 'Sigo hablando después del cambio')
        await self.leave(new_socket, new_task)

    async def test_a_swap_that_would_overflow_the_room_is_refused_and_the_call_it_came_from_survives(self):
        from sidevoice.app import browser_call
        joined = []
        for _ in range(self.hub.max_clients):
            socket = FakeWebSocket()
            joined.append((socket, *await self.join(socket)))
        # The room filled up after the check that precedes the hello: the join itself must refuse.
        refused = FakeWebSocket()
        with patch('sidevoice.app.room_is_full', return_value=False):
            self.hello(refused)
            await browser_call(refused)
        error = await self.received(refused, 'error')
        self.assertIn('maximum number of browsers', error['data']['message'])
        self.assertEqual(refused.application_state, WebSocketState.DISCONNECTED)
        self.assertEqual(len(self.hub.clients), self.hub.max_clients)
        self.assertTrue(all(client.connected for _, _, client in joined))
        for socket, task, _ in joined:
            await self.leave(socket, task)

    # ----- what a browser captured while it had no socket -----

    def catchup(self, voice, pcm, *, rate=16000, truncated=False, at=None, slice_bytes=8000, session=None):
        """The browser's upload, exactly as the page sends it: base64 slices in text frames."""
        task, total = None, max(1, -(-len(pcm) // slice_bytes))
        for index in range(total):
            chunk = pcm[index * slice_bytes:(index + 1) * slice_bytes]
            task = voice.browser_message({'type': 'voice-catchup', 'data': {
                'session_id': voice.call.id if session is None else session,
                'sample_rate': rate, 'seq': index, 'audio_base64': base64.b64encode(chunk).decode('ascii'),
                'final': index == total - 1, 'truncated': truncated, 'started_at': at}})
        return task

    async def test_audio_captured_offline_becomes_one_message_and_never_a_turn(self):
        from sidevoice.transcribers import Transcript
        voice, client, sent = self.voice([], offline=[Transcript('Esto lo dije sin sala')])
        pcm, spoken_at = b'\x10\x00' * 16000, int(time.time() * 1000) - 9000
        self.assertIsNotNone(await self.catchup(voice, pcm, at=spoken_at))
        # The whole recording reached recognition once, at the rate the browser declared.
        self.assertEqual(voice.transcriber.offline_audio, [(pcm, 16000)])
        self.assertEqual(voice.transcriber.calls, 0, 'a catch-up is never transcribed as a live turn')
        # Nothing about this browser's turn-taking moved: no epoch, no open turn, no held text.
        self.assertEqual((client.revision, client.turn_revision, client.speaking, voice.held), (0, 0, False, None))
        turn = next(m for m in sent if m['type'] == 'voice-catchup-turn')
        history_id = client.id + ':user-catchup:1'
        self.assertEqual(turn['data'], {'session_id': client.id, 'history_id': history_id, 'thread_id': 'thread-a',
                                        'text': 'Esto lo dije sin sala', 'offline': 'buffered', 'time': spoken_at})
        self.assertFalse([m for m in sent if m['type'] == 'voice-user-turn'])
        # One journal row, marked as captured offline and stamped with the browser's own clock.
        row = self.hub.journal.get(history_id)
        self.assertEqual((row['offline'], row['time'], row['status'], row['text'], row['revision']),
                         ('buffered', spoken_at, 'pending', 'Esto lo dije sin sala', 0))
        self.assertEqual([r['offline'] for r in self.hub.journal.history('thread-a')], ['buffered'],
                         'the transcript carries the mark, so a reload still says where the message came from')
        self.assertIsNone(voice.catchup, 'the audio is dropped the moment it has been recognised')

    async def test_a_gap_that_held_no_words_produces_nothing_at_all(self):
        from sidevoice.transcribers import Transcript
        voice, client, sent = self.voice([], offline=[Transcript('  ')])
        await self.catchup(voice, b'\x00\x00' * 8000)
        self.assertEqual(sent, [], 'silence is not a message, and not an incident either')
        self.assertEqual(self.hub.journal.history('thread-a'), [])

    async def test_a_buffer_that_overflowed_says_so_instead_of_shortening_in_silence(self):
        from sidevoice.transcribers import Transcript
        voice, client, sent = self.voice([], offline=[Transcript('…y por eso te lo cuento')])
        await self.catchup(voice, b'\x10\x00' * 32000, truncated=True)
        turn = next(m for m in sent if m['type'] == 'voice-catchup-turn')
        self.assertEqual(turn['data']['offline'], 'truncated')
        self.assertEqual(self.hub.journal.get(client.id + ':user-catchup:1')['offline'], 'truncated')

    async def test_a_recording_that_arrived_broken_is_dropped_rather_than_transcribed_with_a_hole(self):
        voice, client, sent = self.voice([], offline=[])
        data = lambda seq, final=False: {'type': 'voice-catchup', 'data': {
            'session_id': client.id, 'sample_rate': 16000, 'seq': seq,
            'audio_base64': base64.b64encode(b'\x10\x00' * 800).decode('ascii'), 'final': final}}
        self.assertIsNone(voice.browser_message(data(0)))
        self.assertIsNone(voice.browser_message(data(2, final=True)), 'a slice out of order ends the upload')
        self.assertIsNone(voice.catchup)
        # A first slice that does not start at zero is not an upload this room is in the middle of.
        self.assertIsNone(voice.browser_message(data(1, final=True)))
        self.assertEqual(voice.transcriber.offline_audio, [])
        self.assertEqual(self.hub.journal.history('thread-a'), [])

    async def test_more_audio_than_any_gap_could_hold_is_refused_and_said_so(self):
        from sidevoice.app import CATCHUP_MAX_SECONDS
        voice, client, sent = self.voice([], offline=[])
        for index in range(CATCHUP_MAX_SECONDS + 5):
            voice.browser_message({'type': 'voice-catchup', 'data': {
                'session_id': client.id, 'sample_rate': 16000, 'seq': index,
                'audio_base64': base64.b64encode(b'\x10\x00' * 16000).decode('ascii'), 'final': False}})
        self.assertIsNone(voice.catchup)
        self.assertIn('too long', next(m for m in sent if m['type'] == 'error')['data']['message'])
        self.assertEqual(voice.transcriber.offline_audio, [])

    async def test_a_browser_clock_that_makes_no_sense_leaves_the_room_s_own(self):
        from sidevoice.app import catchup_time
        from sidevoice.transcribers import Transcript
        now = time.time() * 1000
        self.assertIsNone(catchup_time(0))
        self.assertIsNone(catchup_time(now + 600_000))
        self.assertIsNone(catchup_time('ayer'))
        self.assertIsNone(catchup_time(True))
        self.assertEqual(catchup_time(now - 1000), int(now - 1000))
        voice, client, sent = self.voice([], offline=[Transcript('Hola')])
        await self.catchup(voice, b'\x10\x00' * 8000, at=0)
        self.assertGreater(self.hub.journal.get(client.id + ':user-catchup:1')['time'], now - 1000)

    async def test_a_catch_up_that_could_not_be_transcribed_says_so_and_invents_no_message(self):
        voice, client, sent = self.voice([], offline=[RuntimeError('GPU perdida')])
        await self.catchup(voice, b'\x10\x00' * 8000)
        self.assertIn('GPU perdida', next(m for m in sent if m['type'] == 'error')['data']['message'])
        self.assertEqual(self.hub.journal.history('thread-a'), [])
        self.assertFalse([m for m in sent if m['type'] == 'voice-catchup-turn'])

    async def test_a_catch_up_and_a_live_turn_are_two_messages_and_neither_takes_the_other_s_place(self):
        from sidevoice.transcribers import Transcript
        voice, client, sent = self.voice([Transcript('Y ahora esto')], offline=[Transcript('Lo de antes')])
        voice.turn_started()
        catch_up = self.catchup(voice, b'\x10\x00' * 8000)
        await voice.turn_stopped()
        await catch_up
        rows = {row['id']: row for row in self.hub.journal.history('thread-a')}
        self.assertEqual({row['text'] for row in rows.values()}, {'Lo de antes', 'Y ahora esto'})
        self.assertEqual(rows[client.id + ':user-catchup:1']['offline'], 'buffered')
        self.assertEqual(rows[client.id + ':user-turn:1']['offline'], None,
                         'what the room heard live is not marked as captured offline')
        self.assertEqual(client.revision, 1, 'only the live turn moved this browser\'s epoch')

    async def test_a_catch_up_for_another_session_is_not_this_call_s(self):
        voice, client, sent = self.voice([], offline=[])
        self.assertIsNone(self.catchup(voice, b'\x10\x00' * 800, session='someone-else'))
        self.assertIsNone(voice.catchup)
        self.assertEqual(voice.transcriber.offline_audio, [])

    async def test_a_browser_over_the_limit_is_refused_without_disturbing_the_room(self):
        from sidevoice.app import browser_call
        joined = []
        for _ in range(self.hub.max_clients):
            socket = FakeWebSocket()
            joined.append((socket, *await self.join(socket)))
        refused = FakeWebSocket()
        await browser_call(refused)
        error = await self.received(refused, 'error')
        self.assertIn('maximum number of browsers', error['data']['message'])
        # The frame names the reason as well as saying it, so a page whose proxy kept the sentence
        # but lost the close code still knows which sentence of its own to show (#63).
        self.assertEqual(error['data']['reason'], 'room_is_full')
        self.assertEqual(refused.application_state, WebSocketState.DISCONNECTED)
        self.assertEqual(len(self.hub.clients), self.hub.max_clients)
        self.assertTrue(all(client.connected for _, _, client in joined))
        for socket, task, _ in joined:
            await self.leave(socket, task)

    # ----- a seat is held by a browser that answers, and by nobody else (#63) -----

    async def test_a_browser_that_stops_answering_loses_its_seat(self):
        # Behind a tunnel a closed tab never closes its socket: this is the room noticing on its own.
        with patch.dict(os.environ, {'VOICE_BROWSER_HEARTBEAT_SECONDS': '0.4'}):
            socket = FakeWebSocket()
            task, client = await self.join(socket)
            self.assertIn(client.id, self.hub.clients)
            asked = await self.received(socket, 'voice-ping')
            self.assertEqual(asked['data']['session_id'], client.id, 'the room asks this browser by name')
            await asyncio.wait_for(task, 5)   # nothing answers, and the call ends by itself
        self.assertEqual(self.hub.clients, {}, 'the seat went back to the room')
        self.assertFalse(client.connected)
        self.assertTrue(client.closed, 'it left by the door an ordinary disconnect uses')

    async def test_a_browser_that_answers_keeps_its_seat_and_still_leaves_when_it_says_so(self):
        with patch.dict(os.environ, {'VOICE_BROWSER_HEARTBEAT_SECONDS': '0.2'}):
            socket = FakeWebSocket()
            task, client = await self.join(socket)
            for _ in range(4):   # four answers over twice the budget this room allows
                await asyncio.sleep(0.1)
                socket.incoming.put_nowait({'type': 'websocket.receive', 'text': json.dumps(
                    {'type': 'voice-pong', 'data': {'session_id': client.id}})})
            self.assertIn(client.id, self.hub.clients, 'a browser that answers is nobody to evict')
            self.assertFalse(task.done())
            # And the ordinary way out is untouched: the socket closes, the seat is free at once.
            await self.leave(socket, task)
        self.assertEqual(self.hub.clients, {})
        self.assertFalse(client.connected)

    # ----- what this browser never heard, when it comes back (#52) -----

    async def test_the_hello_names_this_tab_s_earlier_sessions_and_the_room_plays_back_what_it_missed(self):
        from sidevoice.presentation import Speech
        from unittest.mock import patch as patch_
        first = FakeWebSocket()
        first_task, first_client = await self.join(first)
        with patch_('sidevoice.language_settings.resolve_voice',
                    return_value={'provider': 'kokoro', 'model': 'kokoro', 'voice': 'ef_dora',
                                  'speed': 1.0, 'language': 'es', 'device': 'auto'}):
            await self.hub.publish(Speech(thread_id='thread-a', session_id=first_client.id, revision=0,
                                          text='Lo último que te dije', utterance_id='u-1'))
            # The tunnel: the socket goes, the call does not, and that reply was never heard through.
            await self.leave(first, first_task)
            self.assertEqual(self.hub.utterances['u-1'].clients[first_client.id]['status'], 'disconnected')
            second = FakeWebSocket()
            hello = {**TIMER_HELLO, 'sessions': [first_client.id],
                     'settings': {'replay_on_return_seconds': 120}}
            second_task, second_client = await self.join(second, hello)
            announcement = await self.received(second, 'voice-replay')
            speech = await self.received(second, 'voice-speech')
        self.assertEqual(announcement['data']['replies'],
                         [{'utterance_id': 'u-1:replay:' + second_client.id,
                           'history_id': first_client.id + ':voice:u-1'}])
        self.assertEqual(announcement['data']['skipped'], [])
        self.assertEqual((speech['data']['text'], speech['data']['replay']),
                         ('Lo último que te dije', True))
        self.assertEqual(speech['data']['history_id'], first_client.id + ':voice:u-1')
        # Nothing new in the journal: the reply already had its row and still has exactly one.
        self.assertEqual([row['id'] for row in self.hub.journal.history('thread-a')],
                         [first_client.id + ':voice:u-1'])
        await self.leave(second, second_task)

    async def test_a_device_that_turned_the_catch_up_off_is_played_nothing_and_a_hello_declares_only_strings(self):
        from sidevoice.app import prior_sessions, MAX_PRIOR_SESSIONS
        from sidevoice.presentation import Speech
        from unittest.mock import patch as patch_
        first = FakeWebSocket()
        first_task, first_client = await self.join(first)
        with patch_('sidevoice.language_settings.resolve_voice',
                    return_value={'provider': 'kokoro', 'model': 'kokoro', 'voice': 'ef_dora',
                                  'speed': 1.0, 'language': 'es', 'device': 'auto'}):
            await self.hub.publish(Speech(thread_id='thread-a', session_id=first_client.id, revision=0,
                                          text='Lo último que te dije', utterance_id='u-1'))
        await self.leave(first, first_task)
        second = FakeWebSocket()
        second_task, second_client = await self.join(second, {
            **TIMER_HELLO, 'sessions': [first_client.id], 'settings': {'replay_on_return_seconds': 0}})
        await self.settled()
        self.assertFalse(second_client.pending, 'off means the room offers nothing at all')
        self.assertFalse([raw for raw in list(second.sent._queue)
                          if isinstance(raw, str) and json.loads(raw)['type'] == 'voice-replay'])
        await self.leave(second, second_task)
        # What a page may claim as its own earlier sessions: strings, bounded, and nothing else.
        self.assertEqual(prior_sessions(['a', 7, None, '', 'b']), ['a', 'b'])
        self.assertEqual(prior_sessions('a'), [])
        self.assertEqual(len(prior_sessions([str(n) for n in range(50)])), MAX_PRIOR_SESSIONS)
        self.assertEqual(prior_sessions(['x' * 65]), [])


if __name__ == '__main__':
    import unittest; unittest.main()
