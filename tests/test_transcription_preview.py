"""The human's one-shot provider STT sample: bounded, paired, and separate from calls/history."""
import array
import asyncio
import base64
import io
import json
import os
import tempfile
import unittest
import wave
from pathlib import Path
from unittest.mock import patch

from starlette.testclient import TestClient

from sidevoice_core.pipeline import integrations
from sidevoice_core.pipeline.transcribers import Transcript
from sidevoice_core.server import models as model_routes
from sidevoice_core.server.app import create_app

PAGE = {'Origin': 'http://127.0.0.1:8768'}


def payload(seconds=0.5, **changes):
    samples = array.array('h', [1200] * int(16000 * seconds))
    body = {
        'place': 'openai',
        'model': 'gpt-4o-transcribe',
        'options': {'language': 'es', 'context': 'Sidevoice names'},
        'audio': {'encoding': 'pcm_s16le', 'sample_rate': 16000,
                  'data_base64': base64.b64encode(samples.tobytes()).decode('ascii')},
    }
    body.update(changes)
    return body


class FakeTranscriber:
    made = []
    answers = []

    def __init__(self, key, *, model, language=None, prompt=None):
        self.request = {'key': key, 'model': model, 'language': language, 'prompt': prompt}
        type(self).made.append(self.request)

    async def transcribe(self, wav):
        self.request['wav'] = wav
        answer = type(self).answers.pop(0)
        if isinstance(answer, Exception):
            raise answer
        return answer


class PreviewTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.old_data_dir = os.environ.get('SIDEVOICE_CORE_DATA_DIR')
        os.environ['SIDEVOICE_CORE_DATA_DIR'] = self.temp.name
        self.addCleanup(self._restore_data_dir)
        self.credentials = Path(self.temp.name) / 'integrations.json'
        self.credentials_patch = patch.object(integrations, 'CREDENTIALS', self.credentials)
        self.credentials_patch.start()
        self.addCleanup(self.credentials_patch.stop)
        self.provider_patch = patch.object(model_routes, 'PREVIEW_TIMEOUT', 30.0)
        self.provider_patch.start()
        self.addCleanup(self.provider_patch.stop)
        FakeTranscriber.made, FakeTranscriber.answers = [], []
        self.transcriber_patch = patch('sidevoice_core.pipeline.transcribers.OpenAITranscriber', FakeTranscriber)
        self.transcriber_patch.start()
        self.addCleanup(self.transcriber_patch.stop)
        self.app = create_app(device_auth=False)
        self.client = TestClient(self.app, base_url='http://127.0.0.1:8768')
        self.addCleanup(self.client.close)

    def _restore_data_dir(self):
        if self.old_data_dir is None:
            os.environ.pop('SIDEVOICE_CORE_DATA_DIR', None)
        else:
            os.environ['SIDEVOICE_CORE_DATA_DIR'] = self.old_data_dir

    def post(self, body, headers=None):
        return self.client.post('/api/models/transcription/preview', headers=headers or PAGE, json=body)

    def test_selected_stage_becomes_wav_and_only_usable_transcript_is_returned(self):
        integrations.save_key('openai', 'sk-local-test')
        FakeTranscriber.answers = [Transcript('  hola, ya te escucho  ', confidence=-0.1)]
        history_before = dict(self.app.state.room.journal.messages)

        answer = self.post(payload())

        self.assertEqual(answer.status_code, 200)
        self.assertEqual(answer.json(), {'text': 'hola, ya te escucho'})
        self.assertEqual(FakeTranscriber.made[0]['key'], 'sk-local-test')
        self.assertEqual(FakeTranscriber.made[0]['model'], 'gpt-4o-transcribe')
        self.assertEqual(FakeTranscriber.made[0]['language'], 'es')
        self.assertEqual(FakeTranscriber.made[0]['prompt'], 'Sidevoice names')
        with wave.open(io.BytesIO(FakeTranscriber.made[0]['wav'])) as wav:
            self.assertEqual((wav.getnchannels(), wav.getsampwidth(), wav.getframerate()), (1, 2, 16000))
            self.assertEqual(wav.getnframes(), 8000)
        self.assertEqual(self.app.state.room.journal.messages, history_before)

    def test_automatic_language_is_not_forced_and_context_is_optional(self):
        integrations.save_key('openai', 'sk-local-test')
        FakeTranscriber.answers = [Transcript('the selected model heard this')]
        body = payload()
        body['options'] = {'language': 'auto'}

        answer = self.post(body)

        self.assertEqual(answer.status_code, 200)
        self.assertEqual(FakeTranscriber.made[0]['language'], None)
        self.assertIsNone(FakeTranscriber.made[0]['prompt'])

    def test_missing_key_is_refused_before_provider_work(self):
        answer = self.post(payload())
        self.assertEqual(answer.status_code, 409)
        self.assertEqual(answer.json()['detail']['key'], 'trial.provider_unavailable')
        self.assertTrue(answer.json()['detail']['message'])
        self.assertEqual(FakeTranscriber.made, [])

    def test_invalid_stage_and_audio_are_keyed_and_never_call_provider(self):
        integrations.save_key('openai', 'sk-local-test')
        invalid_stage = payload()
        invalid_stage['model'] = 'invalid model id'
        invalid_audio = payload()
        invalid_audio['audio']['data_base64'] = '%%%'
        short_audio = payload(seconds=0.2)
        wrong_rate = payload()
        wrong_rate['audio']['sample_rate'] = 44100

        for body, status, key in ((invalid_stage, 422, 'trial.invalid_stage'),
                                  (invalid_audio, 400, 'trial.invalid_audio'),
                                  (short_audio, 400, 'trial.invalid_audio'),
                                  (wrong_rate, 400, 'trial.invalid_audio')):
            with self.subTest(key=key, status=status):
                answer = self.post(body)
                self.assertEqual(answer.status_code, status)
                self.assertEqual(answer.json()['detail']['key'], key)
        self.assertEqual(FakeTranscriber.made, [])

    def test_malformed_json_is_keyed_without_entering_provider_work(self):
        integrations.save_key('openai', 'sk-local-test')

        answer = self.client.post('/api/models/transcription/preview', headers=PAGE, content=b'{malformed')

        self.assertEqual(answer.status_code, 422)
        self.assertEqual(answer.json()['detail']['key'], 'trial.invalid_stage')
        self.assertEqual(FakeTranscriber.made, [])

    def test_provider_owned_model_id_is_passed_to_openai_and_unknown_ids_fail_without_raw_errors(self):
        integrations.save_key('openai', 'sk-local-test')
        FakeTranscriber.answers = [RuntimeError('provider says model is not supported')]
        body = payload()
        body['model'] = 'some-future-openai-stt-model'

        answer = self.post(body)

        self.assertEqual(answer.status_code, 502)
        self.assertEqual(answer.json()['detail']['key'], 'trial.provider_failed')
        self.assertNotIn('provider says model is not supported', answer.text)
        self.assertEqual(FakeTranscriber.made[0]['model'], 'some-future-openai-stt-model')

    def test_decoded_and_encoded_audio_caps_are_enforced(self):
        integrations.save_key('openai', 'sk-local-test')
        too_long = payload(seconds=10.01)
        too_large_body = {'data_base64': 'A' * (512 * 1024)}
        for body in (too_long, too_large_body):
            answer = self.post(body)
            self.assertEqual(answer.status_code, 413)
            self.assertEqual(answer.json()['detail']['key'], 'trial.audio_too_large')
        self.assertEqual(FakeTranscriber.made, [])

    def test_silent_unusable_provider_failure_and_timeout_have_stable_keys(self):
        integrations.save_key('openai', 'sk-local-test')
        cases = ((Transcript('...'), 'trial.silent'),
                 (Transcript('uncertain words', confidence=-5.0), 'trial.unusable'),
                 (Transcript('привет'), 'trial.unusable'),
                 (RuntimeError('sensitive upstream details'), 'trial.provider_failed'))
        for result, key in cases:
            FakeTranscriber.answers = [result]
            answer = self.post(payload())
            self.assertEqual(answer.status_code, 422 if key in ('trial.silent', 'trial.unusable') else 502)
            self.assertEqual(answer.json()['detail']['key'], key)
            self.assertNotIn('sensitive upstream details', answer.text)

        class Stalled(FakeTranscriber):
            cancelled = False

            async def transcribe(self, wav):
                try:
                    await asyncio.sleep(10)
                except asyncio.CancelledError:
                    type(self).cancelled = True
                    raise

        with patch('sidevoice_core.pipeline.transcribers.OpenAITranscriber', Stalled), \
                patch.object(model_routes, 'PREVIEW_TIMEOUT', 0.01):
            answer = self.post(payload())
        self.assertEqual(answer.status_code, 504)
        self.assertEqual(answer.json()['detail']['key'], 'trial.provider_timeout')
        self.assertTrue(Stalled.cancelled)

    def test_preview_budget_allows_six_starts_per_device_and_only_one_active_request(self):
        now = [0.0]
        budget = model_routes.TranscriptionPreviewBudget(clock=lambda: now[0])
        budget.start('device-a')
        with self.assertRaises(model_routes.PreviewBusy):
            budget.start('device-a')
        budget.start('device-b')
        budget.finish('device-b')
        budget.finish('device-a')
        for _ in range(5):
            budget.start('device-a')
            budget.finish('device-a')
        with self.assertRaises(model_routes.PreviewBusy) as limited:
            budget.start('device-a')
        self.assertEqual(limited.exception.retry_after, 60)
        now[0] = 60
        budget.start('device-a')
        budget.finish('device-a')

    def test_request_disconnect_cancels_provider_work_without_recording_a_turn(self):
        integrations.save_key('openai', 'sk-local-test')
        started, cancelled = asyncio.Event(), asyncio.Event()

        class Stalled:
            def __init__(self, *args, **kwargs):
                pass

            async def transcribe(self, wav):
                started.set()
                try:
                    await asyncio.Event().wait()
                except asyncio.CancelledError:
                    cancelled.set()
                    raise

        async def run():
            raw = json.dumps(payload()).encode('utf8')
            incoming = asyncio.Queue()
            split = len(raw) // 2
            await incoming.put({'type': 'http.request', 'body': raw[:split], 'more_body': True})
            await incoming.put({'type': 'http.request', 'body': raw[split:], 'more_body': False})
            async def receive():
                return await incoming.get()
            async def send(message):
                return None
            scope = {
                'type': 'http', 'asgi': {'version': '3.0', 'spec_version': '2.4'},
                'http_version': '1.1', 'method': 'POST', 'scheme': 'http',
                'path': '/api/models/transcription/preview', 'raw_path': b'/api/models/transcription/preview',
                'query_string': b'', 'root_path': '',
                'headers': [(b'host', b'127.0.0.1:8768'), (b'origin', b'http://127.0.0.1:8768'),
                            (b'content-type', b'application/json')],
                'client': ('127.0.0.1', 12345), 'server': ('127.0.0.1', 8768), 'state': {},
            }
            task = asyncio.create_task(self.app(scope, receive, send))
            await asyncio.wait_for(started.wait(), 2)
            await incoming.put({'type': 'http.disconnect'})
            with self.assertRaises(asyncio.CancelledError):
                await asyncio.wait_for(task, 2)
            self.assertTrue(cancelled.is_set())
            self.assertEqual(self.app.state.room.journal.messages, {})

        with patch('sidevoice_core.pipeline.transcribers.OpenAITranscriber', Stalled):
            asyncio.run(run())


class PairedPreviewTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.credentials_patch = patch.object(integrations, 'CREDENTIALS', Path(self.temp.name) / 'integrations.json')
        self.credentials_patch.start()
        self.addCleanup(self.credentials_patch.stop)
        FakeTranscriber.made, FakeTranscriber.answers = [], [Transcript('actual words')]
        self.provider_patch = patch('sidevoice_core.pipeline.transcribers.OpenAITranscriber', FakeTranscriber)
        self.provider_patch.start()
        self.addCleanup(self.provider_patch.stop)
        integrations.save_key('openai', 'sk-local-test')
        self.app = create_app(config={'SIDEVOICE_CORE_DATA_DIR': self.temp.name})
        self.client = TestClient(self.app, base_url='http://127.0.0.1:8768')
        self.addCleanup(self.client.close)
        code = self.app.state.devices.issue_code()
        answer = self.client.post('/api/device/pair', headers=PAGE,
                                  json={'secret': code['payload']['secret'], 'name': 'trial test'})
        self.assertEqual(answer.status_code, 200)
        self.headers = {**PAGE, 'Authorization': f"Bearer {answer.json()['token']}"}

    def test_route_requires_a_paired_device_and_same_origin_before_provider_work(self):
        unauthenticated = self.client.post('/api/models/transcription/preview', headers=PAGE, json=payload())
        self.assertEqual(unauthenticated.status_code, 401)
        wrong_origin = self.client.post('/api/models/transcription/preview',
                                        headers={**self.headers, 'Origin': 'https://elsewhere.example'},
                                        json=payload())
        self.assertEqual(wrong_origin.status_code, 403)
        self.assertEqual(FakeTranscriber.made, [])

        answer = self.client.post('/api/models/transcription/preview', headers=self.headers, json=payload())
        self.assertEqual(answer.status_code, 200)
        self.assertEqual(answer.json(), {'text': 'actual words'})


if __name__ == '__main__':
    unittest.main()
