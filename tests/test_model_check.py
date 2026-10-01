"""A model is checked before it takes effect (#124 §6, #90): the clips and phrases are this package's data, a
verdict judges what came back, a provider's model is checked here with the node's key, and a failure is an
answer that names its step and its reason — never an exception, never a stage saved."""
import array
import asyncio
import base64
import io
import math
import os
import tempfile
import unittest
import wave
from pathlib import Path
from unittest.mock import patch

from starlette.testclient import TestClient

from sidevoice_core.models import verdicts
from sidevoice_core.pipeline import integrations, model_check, synthesis, transcribers
from sidevoice_core.pipeline.settings import Transcription, Voice
from sidevoice_core.pipeline.transcribers import Transcript
from sidevoice_core.server.app import create_app
from sidevoice_core.server.rendezvous import relayed

PAGE = {'Origin': 'http://127.0.0.1:8768'}
SPANISH = verdicts.clip('es')[1]


def pcm(seconds, level=0.3, rate=16000):
    """`seconds` of a 220 Hz tone as ElevenLabs' pcm_16000, base64."""
    samples = array.array('h', (int(level * 32767 * math.sin(2 * math.pi * 220 * i / rate)) for i in range(int(seconds * rate))))
    return base64.b64encode(samples.tobytes()).decode('ascii')


class VerdictTest(unittest.TestCase):
    def test_a_clip_per_language_of_about_five_seconds_and_english_for_the_rest(self):
        for language in ('es', 'en'):
            with self.subTest(language=language):
                audio, text = verdicts.clip(language)
                with wave.open(io.BytesIO(audio)) as clip:
                    self.assertEqual((clip.getnchannels(), clip.getsampwidth(), clip.getframerate()), (1, 2, 16000))
                    self.assertTrue(4 <= clip.getnframes() / 16000 <= 7)
                self.assertTrue(text)
                self.assertEqual(verdicts.phrase(language), text, 'a voice check speaks what the clip says')
        for language in ('fr', 'auto', None):
            self.assertEqual(verdicts.language_for('stt', language), 'en')
            self.assertEqual(verdicts.clip(language), verdicts.clip('en'))

    def test_text_close_to_the_clip_passes_and_nothing_or_something_else_does_not(self):
        self.assertEqual(verdicts.word_error(SPANISH, SPANISH.upper()), 0)
        self.assertIsNone(verdicts.transcript_problem(SPANISH, ' hola, esto es una prueba de transcripcion para comprobar que el modelo entiende lo que digo'))
        # A small model's slips are not a failure.
        self.assertIsNone(verdicts.transcript_problem(SPANISH, 'Ola, esto es una prueba de transcripción para comprobar que el modelo entiende lo que dijo.'))
        self.assertEqual(verdicts.transcript_problem(SPANISH, ' ... ')['key'], 'check_silent')
        mismatch = verdicts.transcript_problem(SPANISH, 'Thank you for watching.')
        self.assertEqual(mismatch['key'], 'check_mismatch')
        self.assertEqual(mismatch['heard'], 'Thank you for watching.')
        self.assertTrue(mismatch['message'])

    def test_audio_must_be_audible_and_last_a_plausible_time(self):
        tone = [0.3 * math.sin(i / 10) for i in range(16000 * 5)]
        self.assertIsNone(verdicts.audio_problem(tone, 16000))
        self.assertEqual(verdicts.audio_problem([0.0] * 16000 * 5, 16000)['key'], 'check_silent')
        self.assertEqual(verdicts.audio_problem([], 16000)['key'], 'check_silent')
        short = verdicts.audio_problem(tone[:1600], 16000)
        self.assertEqual((short['key'], short['seconds']), ('check_duration', 0.1))

    def test_audio_that_is_not_numbers_is_never_audible(self):
        """Review R10: NaN or ±inf samples, or a rate that is not one, used to pass as loud enough."""
        tone = [0.3 * math.sin(i / 10) for i in range(16000 * 5)]
        for value in (math.nan, math.inf, -math.inf):
            with self.subTest(value=value):
                self.assertEqual(verdicts.audio_problem([value] * 16000 * 5, 16000)['key'], 'check_invalid_audio')
                one = list(tone)
                one[777] = value
                self.assertEqual(verdicts.audio_problem(one, 16000)['key'], 'check_invalid_audio')
        for rate in (math.nan, 0, math.inf):
            with self.subTest(rate=rate):
                self.assertEqual(verdicts.audio_problem(tone, rate)['key'], 'check_invalid_audio')

    def test_slowness_is_measured_against_the_comfort_line(self):
        line = verdicts.spec()['stt']['comfort_ms']
        self.assertFalse(verdicts.slow(line))
        self.assertTrue(verdicts.slow(line + 1))


class FakeTranscriber:
    """OpenAITranscriber's place: each pass answers the next of `answers` (text, or an exception to raise)."""
    answers, made = [], []

    def __init__(self, key, *, model, language=None, prompt=None):
        FakeTranscriber.made.append({'key': key, 'model': model, 'language': language})

    async def transcribe(self, wav):
        answer = FakeTranscriber.answers.pop(0)
        if isinstance(answer, Exception):
            raise answer
        return Transcript(answer)


class AuthenticationError(Exception):
    """Named like the OpenAI SDK's."""


class APIConnectionError(Exception):
    """Named like the OpenAI SDK's."""


class Keys(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        for patcher in (patch.object(integrations, 'CREDENTIALS', Path(temp.name) / 'integrations.json'),
                        patch.object(transcribers, 'OpenAITranscriber', FakeTranscriber)):
            patcher.start()
            self.addCleanup(patcher.stop)
        for variable in ('VOICE_STT_API_KEY', 'VOICE_ELEVENLABS_API_KEY'):
            os.environ.pop(variable, None)
        FakeTranscriber.answers, FakeTranscriber.made = [], []


class ProviderCheckTest(Keys):
    async def test_no_key_fails_at_the_key_step_without_calling_anyone(self):
        result = await model_check.check(Transcription(place='openai', model='gpt-4o-transcribe'))
        self.assertEqual((result['ok'], result['step'], result['reason']['key']), (False, 'key', 'provider_key_missing'))
        self.assertEqual(result['reason']['provider'], 'openai')
        self.assertEqual(FakeTranscriber.made, [])

    async def test_a_transcription_passes_twice_and_the_second_is_the_one_measured(self):
        integrations.save_key('openai', 'sk-test')
        FakeTranscriber.answers = [SPANISH, SPANISH]
        result = await model_check.check(Transcription(place='openai', model='gpt-4o-transcribe', options={'language': 'es'}))
        self.assertTrue(result['ok'])
        self.assertEqual((result['step'], result['language'], len(result['passes'])), ('done', 'es', 2))
        self.assertEqual(result['latency_ms'], result['passes'][1]['latency_ms'])
        self.assertFalse(result['slow'])
        self.assertEqual(FakeTranscriber.made, [{'key': 'sk-test', 'model': 'gpt-4o-transcribe', 'language': 'es'}])

    async def test_automatic_language_checks_in_the_language_asked_for_or_english(self):
        integrations.save_key('openai', 'sk-test')
        FakeTranscriber.answers = [SPANISH, SPANISH, verdicts.clip('en')[1], verdicts.clip('en')[1]]
        self.assertEqual((await model_check.check(Transcription(place='openai', model='whisper-1', options={'language': 'auto'}), language='es'))['language'], 'es')
        self.assertEqual((await model_check.check(Transcription(place='openai', model='whisper-1', options={'language': 'auto'}), language='hi'))['language'], 'en')

    async def test_a_refused_key_an_unreachable_provider_and_a_wrong_transcript_each_name_their_step(self):
        integrations.save_key('openai', 'sk-test')
        stage = Transcription(place='openai', model='whisper-1', options={'language': 'es'})
        cases = [([AuthenticationError('401')], 'key', 'provider_key_refused'),
                 ([APIConnectionError('down')], 'check', 'provider_unreachable'),
                 ([''], 'check', 'check_silent'),
                 ([SPANISH, 'Subtítulos realizados por la comunidad de Amara.org'], 'check', 'check_mismatch')]
        for answers, step, key in cases:
            with self.subTest(key=key):
                FakeTranscriber.answers = list(answers)
                result = await model_check.check(stage)
                self.assertEqual((result['ok'], result['step'], result['reason']['key']), (False, step, key))
                self.assertTrue(result['reason']['message'])

    async def test_a_voice_is_audible_and_measured_by_its_first_audio(self):
        integrations.save_key('elevenlabs', 'xi-test')
        asked = []

        async def speak(text, **kwargs):
            asked.append({'text': text, **kwargs})
            return {'audio_base64': pcm(5), 'timings_ms': {'request_to_first_chunk_ms': 210.4, 'request_to_complete_ms': 1000}}
        stage = Voice(place='elevenlabs', model='eleven_flash_v2_5', options={'voice': {'es': 'v-es', 'en': 'v-en'}})
        with patch.object(synthesis, 'synthesize', speak):
            result = await model_check.check(stage, language='es')
        self.assertTrue(result['ok'], result)
        self.assertEqual((result['voice'], result['latency_ms'], len(result['passes'])), ('v-es', 210, 2))
        self.assertEqual(result['passes'][1], {'first_audio_ms': 210, 'total_ms': 1000, 'audio_seconds': 5.0, 'realtime': 5.0})
        self.assertEqual(asked[0]['text'], verdicts.phrase('es'))
        self.assertEqual((asked[0]['voice'], asked[0]['output_format']), ('v-es', 'pcm_16000'))

    async def test_a_silent_voice_or_a_refused_key_fails(self):
        integrations.save_key('elevenlabs', 'xi-test')
        stage = Voice(place='elevenlabs', model='eleven_flash_v2_5', options={'voice': {'en': 'v-en'}})

        async def silent(text, **kwargs):
            return {'audio_base64': pcm(5, level=0), 'timings_ms': {'request_to_complete_ms': 900}}

        async def refused(text, **kwargs):
            raise synthesis.ProviderAnswered('ElevenLabs rejected the key.', 401)
        for speak, step, key in ((silent, 'check', 'check_silent'), (refused, 'key', 'provider_key_refused')):
            with self.subTest(key=key), patch.object(synthesis, 'synthesize', speak):
                result = await model_check.check(stage, language='es')
                self.assertEqual((result['ok'], result['step'], result['reason']['key']), (False, step, key))


class EndpointTest(Keys):
    def client(self):
        return TestClient(create_app(device_auth=False), base_url='http://127.0.0.1:8768')

    def test_a_provider_is_checked_and_a_failure_is_an_answer(self):
        with self.client() as client:
            answer = client.post('/api/models/check', headers=PAGE,
                                 json={'stage': 'stt', 'place': 'openai', 'model': 'gpt-4o-transcribe', 'options': {'language': 'es'}})
            self.assertEqual(answer.status_code, 200)
            self.assertEqual(answer.json()['reason']['key'], 'provider_key_missing')
            integrations.save_key('openai', 'sk-test')
            FakeTranscriber.answers = [SPANISH, SPANISH]
            answer = client.post('/api/models/check', headers=PAGE,
                                 json={'stage': 'stt', 'place': 'openai', 'model': 'gpt-4o-transcribe', 'options': {'language': 'es'}})
            self.assertEqual(answer.status_code, 200)
            self.assertEqual({key: answer.json()[key] for key in ('ok', 'stage', 'place', 'model')},
                             {'ok': True, 'stage': 'stt', 'place': 'openai', 'model': 'gpt-4o-transcribe'})

    def test_what_cannot_be_checked_here_is_refused_with_a_key(self):
        cases = [({'stage': 'stt', 'place': 'host', 'model': 'whisper-tiny'}, 409, 'place_host_unavailable'),
                 ({'stage': 'tts', 'place': 'device', 'model': 'kokoro-82m-v1.0'}, 400, 'check_on_device'),
                 ({'stage': 'llm', 'place': 'openai', 'model': 'x'}, 422, 'check_invalid'),
                 ({'stage': 'stt', 'place': 'elevenlabs', 'model': 'scribe'}, 422, 'check_invalid'),
                 ({'stage': 'stt', 'place': 'openai', 'model': 'whisper-1', 'options': {'nope': 1}}, 422, 'check_invalid')]
        with self.client() as client:
            for body, status, key in cases:
                with self.subTest(body=body):
                    answer = client.post('/api/models/check', headers=PAGE, json=body)
                    self.assertEqual(answer.status_code, status)
                    self.assertEqual(answer.json()['detail']['key'], key)
                    self.assertTrue(answer.json()['detail']['message'])
            self.assertEqual(client.post('/api/models/check', headers={'Origin': 'https://elsewhere.example'},
                                         json=cases[0][0]).status_code, 403)

    def test_the_check_is_relayed_like_the_catalogue(self):
        self.assertTrue(relayed('/api/models/check'))


if __name__ == '__main__':
    unittest.main()


class BudgetTest(unittest.IsolatedAsyncioTestCase):
    """Review R08: checks of a paid provider are bounded however often an authorised device asks."""

    def setUp(self):
        from sidevoice_core.control.check_budget import CheckBudget
        self.now = [0.0]
        self.budget = CheckBudget(clock=lambda: self.now[0], per_device=(3, 60), per_provider=(5, 60), remember=600)
        self.runs = 0

    async def work(self, ok=True):
        self.runs += 1
        await asyncio.sleep(0)
        return {'ok': ok, 'passes': []}

    async def test_a_passed_check_answers_for_itself_until_it_is_old_or_the_key_changes(self):
        from sidevoice_core.control.check_budget import check_key
        same = check_key('stt', 'openai', 'whisper-1', {'language': 'es'}, 'es', 'sk-one')
        for _ in range(20):
            await self.budget.run(same, device='a', provider='openai', work=self.work)
        self.assertEqual(self.runs, 1)
        self.assertTrue((await self.budget.run(same, device='a', provider='openai', work=self.work))['remembered'])
        self.assertNotIn('sk-one', same, 'the key itself is never part of what is kept')
        await self.budget.run(check_key('stt', 'openai', 'whisper-1', {'language': 'es'}, 'es', 'sk-two'), device='a', provider='openai', work=self.work)
        self.assertEqual(self.runs, 2, 'a new key is checked afresh')
        self.now[0] += 601
        await self.budget.run(same, device='a', provider='openai', work=self.work)
        self.assertEqual(self.runs, 3, 'and an old answer is not trusted forever')

    async def test_a_failed_check_is_not_remembered_and_runs_are_budgeted_per_device_and_per_provider(self):
        from sidevoice_core.control.check_budget import Limited
        for model in ('m1', 'm2', 'm3'):
            await self.budget.run(model, device='a', provider='openai', work=lambda: self.work(ok=False))
        with self.assertRaises(Limited) as refused:
            await self.budget.run('m4', device='a', provider='openai', work=self.work)
        self.assertEqual((refused.exception.scope, refused.exception.retry_after), ('device', 60))
        await self.budget.run('m1', device='b', provider='openai', work=self.work)
        await self.budget.run('m2', device='b', provider='openai', work=self.work)
        with self.assertRaises(Limited) as refused:
            await self.budget.run('m3', device='c', provider='openai', work=self.work)
        self.assertEqual(refused.exception.scope, 'provider', 'every device together has a bound too')
        await self.budget.run('x', device='c', provider='elevenlabs', work=self.work)
        self.now[0] += 60
        await self.budget.run('m9', device='a', provider='openai', work=self.work)
        self.assertEqual(self.runs, 7)

    async def test_identical_checks_asked_together_share_one_run(self):
        answers = await asyncio.gather(*(self.budget.run('same', device=str(i), provider='openai', work=self.work) for i in range(8)))
        self.assertEqual(self.runs, 1)
        self.assertTrue(all(answer['ok'] for answer in answers))


class BudgetedEndpointTest(Keys):
    """The reviewer's probe: a paired device, auth on, twenty checks in a row."""

    def test_twenty_checks_in_a_row_cost_one_run_and_a_burst_of_different_ones_is_refused_with_a_key(self):
        from sidevoice_core.control.history import RoomHistory
        from sidevoice_core.control.room import Room
        data = Path(tempfile.mkdtemp()) / 'core'
        app = create_app(Room(RoomHistory(data / 'room-state.json')), config={'SIDEVOICE_CORE_DATA_DIR': str(data), 'VOICE_BROWSER_HEARTBEAT_SECONDS': '0'})
        app.state.devices.listen_url = 'http://127.0.0.1:8768'
        integrations.save_key('openai', 'sk-test')
        FakeTranscriber.answers = [SPANISH] * 400
        with TestClient(app, base_url='http://127.0.0.1:8768') as client:
            code = app.state.devices.issue_code()
            token = client.post('/api/device/pair', json={'secret': code['payload']['secret'], 'name': 'probe'}).json()['token']
            auth = {**PAGE, 'Authorization': f'Bearer {token}'}
            body = {'stage': 'stt', 'place': 'openai', 'model': 'gpt-4o-transcribe', 'options': {'language': 'es'}}
            self.assertEqual(client.post('/api/models/check', headers=PAGE, json=body).status_code, 401)
            answers = [client.post('/api/models/check', headers=auth, json=body) for _ in range(20)]
            self.assertTrue(all(answer.status_code == 200 and answer.json()['ok'] for answer in answers))
            self.assertEqual(len(FakeTranscriber.made), 1, 'one run (its two passes); the rest answered from memory')
            self.assertNotIn('sk-test', ''.join(answer.text for answer in answers))
            statuses = [client.post('/api/models/check', headers=auth, json={**body, 'model': f'gpt-{n}-transcribe'}) for n in range(8)]
            refused = [answer for answer in statuses if answer.status_code == 429]
            self.assertTrue(refused, 'a burst of different checks meets the budget')
            self.assertEqual(refused[0].json()['detail']['key'], 'check_rate_limited')
            self.assertTrue(int(refused[0].headers['retry-after']) > 0)
