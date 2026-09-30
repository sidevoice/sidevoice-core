import unittest
from sidevoice.latency import CallLatency


class LatencyTest(unittest.TestCase):
    def setUp(self):
        self.now = 10
        self.trace = CallLatency('session', clock=lambda: self.now, limit=2)

    def row(self):
        return self.trace.snapshot()['replies'][0]

    def test_server_intervals_include_only_matching_original_turn(self):
        self.trace.turn('a', 1, 'queued')
        self.now = 10.2
        self.trace.turn('a', 1, 'delivery_accepted')
        self.now = 12
        self.trace.reply('u', 'a', 1)
        self.now = 13
        self.trace.mark('u', 'synthesis_started')
        self.now = 14
        self.trace.mark('u', 'audio_ready')
        self.assertEqual(self.row()['server_ms'], {
            'input_queued_to_delivery_accepted_ms': 200,
            'input_queued_to_reply_received_ms': 2000,
            'delivery_accepted_to_reply_received_ms': 1800,
            'reply_received_to_synthesis_started_ms': 1000,
            'synthesis_started_to_audio_ready_ms': 1000,
        })

    def test_duplicate_events_keep_first_time_and_missing_is_not_zero(self):
        self.trace.turn('a', 1, 'queued')
        self.now = 12
        self.trace.turn('a', 1, 'queued')
        self.trace.reply('u', 'a', 1)
        self.now = 15
        self.trace.reply('u', 'a', 1)
        self.assertEqual(self.row()['server_ms'], {'input_queued_to_reply_received_ms': 2000})

    def test_turns_and_replies_are_bounded_and_namespaced(self):
        for i in range(3):
            self.trace.turn('a', i, 'queued')
            self.trace.reply(str(i), 'b', i)
        self.assertEqual(len(self.trace.turns), 2)
        self.assertEqual(len(self.trace.replies), 2)
        self.assertEqual(self.row()['server_ms'], {})

    def test_client_durations_are_untrusted_bounded_and_separate(self):
        self.trace.reply('u', 'a', 1)
        self.trace.browser('u', {
            'audio_received_to_playback_scheduled_ms': 125.25,
            'turn_finished_event_to_playback_scheduled_ms': float('nan'),
            'vad_stop_event_to_turn_finished_event_ms': -1,
            'text': 'never retained',
        })
        self.trace.browser('u', {'turn_finished_event_to_playback_scheduled_ms': True})
        self.trace.browser('u', {'turn_finished_event_to_playback_scheduled_ms': 3600001})
        self.assertEqual(self.row()['browser_ms'], {'audio_received_to_playback_scheduled_ms': 125.25})
        self.assertEqual(self.row()['server_ms'], {})
        self.assertNotIn('never retained', str(self.trace.snapshot()))

    def test_input_durations_are_allowlisted_and_joined_to_the_reply(self):
        self.trace.input('a', 1, {
            'audio_ms': 8000, 'endpoint_silence_ms': 2500,
            'recognition_ms': 900, 'speech_end_to_transcript_ms': 3425,
            'transcript': 'never retained', 'negative': -1,
        })
        self.trace.turn('a', 1, 'queued')
        self.trace.reply('u', 'a', 1)
        self.assertEqual(self.row()['input_ms'], {
            'audio_ms': 8000, 'endpoint_silence_ms': 2500,
            'recognition_ms': 900, 'speech_end_to_transcript_ms': 3425,
        })
        self.assertNotIn('never retained', str(self.trace.snapshot()))

    def test_restarted_synthesis_does_not_mix_attempt_timings(self):
        self.trace.reply('u', 'a', 1)
        self.trace.start_synthesis('u')
        self.now = 12
        self.trace.mark('u', 'audio_ready')
        self.trace.provider('u', {'request_to_complete_ms': 2000})
        self.now = 15
        self.trace.start_synthesis('u')
        self.now = 16
        self.trace.mark('u', 'audio_ready')
        self.assertEqual(self.row()['synthesis_attempt'], 2)
        self.assertEqual(self.row()['server_ms']['synthesis_started_to_audio_ready_ms'], 1000)
        self.assertEqual(self.row()['provider_ms'], {})
        self.trace.browser('u', {'audio_received_to_playback_scheduled_ms': 10 ** 500})
        self.assertEqual(self.row()['browser_ms'], {})

    def test_provider_fields_are_allowlisted(self):
        self.trace.reply('u', 'a', 1)
        self.trace.provider('u', {'request_to_first_chunk_ms': 10, 'audio_base64': 'secret'})
        self.trace.status('u', 'failed')
        self.assertEqual(self.row()['provider_ms'], {'request_to_first_chunk_ms': 10})
        self.assertEqual(self.row()['status'], 'failed')


class LatencyIntegrationTest(unittest.IsolatedAsyncioTestCase):
    async def test_original_revision_survives_playback_epoch_remapping(self):
        from unittest.mock import AsyncMock, MagicMock, patch
        from sidevoice.presentation import Speech
        from sidevoice.room import Room, RoomClient
        hub = Room(MagicMock())
        hub.journal.put.return_value = {}
        hub.journal.binding_for_thread.return_value = None
        call = RoomClient('s', hub, worker=AsyncMock())
        call.connected = True
        call.target = {'thread_id': 'a'}
        events = []
        call.on_browser_event = events.append
        call.user_started()
        payload = call.enqueue_input('test input')
        call.input_receipt(payload, 'delivered')
        call.user_started()
        call.speaking = False
        choice = {'provider': 'elevenlabs', 'model': 'test', 'voice': 'test', 'speed': 1}
        audio = {'mime_type': 'audio/mpeg', 'audio_base64': 'YQ==',
                 'timings_ms': {'request_to_first_chunk_ms': 10, 'request_to_complete_ms': 20}}
        with patch('sidevoice.language_settings.load_settings'), patch('sidevoice.language_settings.resolve_voice', return_value=choice), patch(
                'sidevoice.synthesis.synthesize', new=AsyncMock(return_value=audio)):
            await hub.publish(Speech(thread_id='a', session_id='s', revision=1, text='test reply', utterance_id='u'))
        sent = next(event['data'] for event in events if event['type'] == 'voice-speech-audio')
        self.assertEqual(sent['revision'], 2)
        self.assertEqual(sent['reply_revision'], 1)
        row = call.latency.snapshot()['replies'][0]
        self.assertEqual(row['reply_revision'], 1)
        self.assertIn('input_queued_to_reply_received_ms', row['server_ms'])
        self.assertEqual(row['provider_ms']['request_to_complete_ms'], 20)

    async def test_api_rejects_stale_receipts_and_exposes_only_valid_timings(self):
        from unittest.mock import AsyncMock, MagicMock, patch
        from fastapi import FastAPI, HTTPException
        from starlette.requests import Request
        from sidevoice.presentation import mount_presentation
        from sidevoice.room import Room, RoomClient, Utterance
        hub = Room(MagicMock())
        call = RoomClient('s', hub, worker=AsyncMock())
        call.connected = True
        call.target = {'thread_id': 'a'}
        call.revision = 2
        call.on_browser_event = lambda event: None
        call.active = 'u'
        utterance = Utterance('u', 'texto', thread_id='a', revision=2, row_id='s:voice:u')
        utterance.clients['s'] = {'status': 'synthesizing', 'reason': None}
        hub.utterances['u'] = utterance
        call.latency.reply('u', 'a', 1)
        app = FastAPI()
        request = Request({'type': 'http', 'method': 'POST', 'path': '/', 'headers': [],
                           'server': ('localhost', 80), 'scheme': 'http'})
        with patch('sidevoice.presentation.hub', hub):
            mount_presentation(app)
            routes = {route.path: route.endpoint for route in app.routes if hasattr(route, 'endpoint')}
            receipt = {'session_id': 'old', 'revision': 2, 'utterance_id': 'u',
                       'status': 'playing', 'timings_ms': {'audio_received_to_playback_scheduled_ms': 25}}
            with self.assertRaises(HTTPException):
                await routes['/api/presentation/browser-receipt'](receipt, request)
            self.assertEqual(call.latency.snapshot()['replies'][0]['browser_ms'], {})
            receipt['session_id'] = 's'
            await routes['/api/presentation/browser-receipt'](receipt, request)
            report = await routes['/api/presentation/latency']('s')
            # A browser that is not in the room is handed nobody else's measurements.
            self.assertEqual(await routes['/api/presentation/latency']('old'),
                             {'session_id': None, 'replies': []})
        self.assertEqual(report['replies'][0]['browser_ms'], {'audio_received_to_playback_scheduled_ms': 25})
        self.assertEqual(report['replies'][0]['status'], 'playing')
