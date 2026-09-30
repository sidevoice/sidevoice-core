import unittest
from unittest.mock import patch
from sidevoice import language_settings

class PreferencesTest(unittest.TestCase):
    def test_the_room_keeps_no_settings_and_a_device_brings_its_own(self):
        self.assertEqual(language_settings.load_settings(), language_settings.LanguageSettings())
        self.assertFalse(hasattr(language_settings, 'save_settings'))
        settings, problem = language_settings.settings_from(
            {'english_voice': 'bf_emma', 'spanish_voice': 'em_alex', 'tts_speed': 1.2, 'unknown_future_key': 1})
        self.assertIsNone(problem)
        en = language_settings.resolve_voice(settings, 'en')
        self.assertEqual((en['voice'], en['speed']), ('bf_emma', 1.2))
        self.assertEqual(language_settings.resolve_voice(settings, 'es')['voice'], 'em_alex')

    def test_an_invalid_device_setting_falls_back_alone_and_says_which(self):
        settings, problem = language_settings.settings_from({'tts_speed': 9, 'stt_provider': 'browser'})
        self.assertEqual(settings, language_settings.LanguageSettings())
        self.assertIn('tts_speed', problem)
        # One refused value no longer takes the rest with it: an iPhone on OpenAI stayed on OpenAI (#39).
        settings, problem = language_settings.settings_from(
            {'tts_speed': 9, 'stt_provider': 'openai', 'stt_model': 'gpt-4o-transcribe', 'spanish_voice': 'em_alex'})
        self.assertIn('tts_speed', problem)
        self.assertEqual((settings.stt_provider, settings.stt_model, settings.spanish_voice),
                         ('openai', 'gpt-4o-transcribe', 'em_alex'))
        self.assertEqual(settings.tts_speed, language_settings.LanguageSettings().tts_speed)
        self.assertEqual(language_settings.settings_from(None), (language_settings.LanguageSettings(), None))
        self.assertEqual(language_settings.settings_from({}), (language_settings.LanguageSettings(), None))

    def test_default_turn_silence_is_two_and_a_half_seconds(self):
        settings = language_settings.LanguageSettings()
        self.assertEqual(settings.user_speech_timeout, 2.5)
        self.assertEqual((settings.turn_end_mode, settings.smart_turn_min_silence, settings.smart_turn_max_silence), ('smart_turn', 0.9, 3.0))

class VoiceResolutionTest(unittest.TestCase):
    def test_default_and_language_override(self):
        p=language_settings.LanguageSettings(default_voice='af_bella',spanish_voice='inherit',english_voice='inherit',tts_speed=1.5)
        self.assertEqual(language_settings.resolve_voice(p,'en')['voice'],'af_bella')
        self.assertEqual(language_settings.resolve_voice(p,'es')['voice'],'ef_dora')
        p.spanish_voice='em_alex'
        self.assertEqual(language_settings.resolve_voice(p,'es')['voice'],'em_alex')
        self.assertEqual(language_settings.resolve_voice(p,'es')['speed'],1.5)

    def test_catalog_languages_resolve_and_cross_language_voice_is_rejected(self):
        from pydantic import ValidationError
        for language, item in language_settings.LANGUAGES.items():
            p=language_settings.LanguageSettings(language_overrides={language:{'voice':'inherit'}})
            voice=language_settings.resolve_voice(p,language)['voice']
            self.assertIn(voice, {v[0] for v in item['voices']})
        with self.assertRaises(ValidationError):
            language_settings.LanguageSettings(language_overrides={'fr':{'voice':'ef_dora'}})

    def test_speed_override_and_reset(self):
        p=language_settings.LanguageSettings(tts_speed=1.5,language_overrides={'en':{'speed':0.85}})
        self.assertEqual(language_settings.resolve_voice(p,'en')['speed'],0.85)
        self.assertEqual(language_settings.resolve_voice(p,'es')['speed'],1.5)
        p.language_overrides['en'].speed=None
        self.assertEqual(language_settings.resolve_voice(p,'en')['speed'],1.5)

class NativeElevenLabsSpeedTest(unittest.IsolatedAsyncioTestCase):
    async def test_speed_is_sent_to_synthesis_with_provider_limits(self):
        from sidevoice import synthesis
        requests = []

        class Response:
            status = 200
            async def __aenter__(self): return self
            async def __aexit__(self, *args): pass
            @property
            def content(self): return self
            async def iter_any(self):
                yield b'fake-'
                yield b'mp3'

        class Client:
            async def __aenter__(self): return self
            async def __aexit__(self, *args): pass
            def post(self, url, **kwargs):
                requests.append(kwargs['json'])
                return Response()

        with patch.object(synthesis, 'key', return_value='test-only'), patch.object(
                synthesis.aiohttp, 'ClientSession', return_value=Client()):
            for requested, effective in [(0.85, 0.85), (1.15, 1.15), (2, 1.2), (0.5, 0.7)]:
                audio = await synthesis.synthesize('Hola', model='eleven_flash_v2_5',
                                                   voice='test-voice', speed=requested)
                self.assertEqual(requests[-1]['voice_settings']['speed'], effective)
                self.assertEqual(audio['mime_type'], 'audio/mpeg')
                import base64
                self.assertEqual(base64.b64decode(audio['audio_base64']), b'fake-mp3')
                timings = audio['timings_ms']
                self.assertLessEqual(timings['request_to_headers_ms'], timings['request_to_first_chunk_ms'])
                self.assertLessEqual(timings['request_to_first_chunk_ms'], timings['request_to_complete_ms'])


class ElevenLabsVoiceCatalogTest(unittest.TestCase):
    def test_primary_language_wins_over_multilingual_previews(self):
        from sidevoice import synthesis
        voice = synthesis._voice_entry({
            'voice_id': 'spanish-voice',
            'name': 'Lucia',
            'category': 'premade',
            'labels': {'language': 'es'},
            'verified_languages': [{'language': 'en'}, {'language': 'fr'}],
        })
        self.assertEqual(voice['languages'], ['es'])
        self.assertEqual(voice['label'], 'Lucia')
        self.assertEqual(voice['description'], 'premade')

    def test_verified_languages_are_a_fallback_when_primary_is_missing(self):
        from sidevoice import synthesis
        voice = synthesis._voice_entry({
            'voice_id': 'multilingual-voice',
            'name': 'Polyglot',
            'verified_languages': [
                {'language': 'EN-us'}, {'language': 'de_DE'}, {'language': 'en'},
            ],
        })
        self.assertEqual(voice['languages'], ['de', 'en'])


class CloudTranscriptionSettingsTests(unittest.TestCase):
    def test_openai_transcription_does_not_fail_validation_on_a_meaningless_local_device(self):
        from sidevoice.language_settings import settings_from
        settings, problem = settings_from({'stt_provider': 'openai', 'stt_model': 'gpt-4o-transcribe', 'stt_device': ''})
        self.assertIsNone(problem)
        self.assertEqual((settings.stt_provider, settings.stt_device), ('openai', 'auto'))
        # A local provider still has to name a real runtime.
        _, problem = settings_from({'stt_provider': 'browser', 'stt_device': ''})
        self.assertIn('stt_device', problem or '')
