"""A device's settings: one shape per stage (`stt`, `tts`) — place, model, options, build — checked against the
model catalogue's option schemas; defaults from the resolver; the voice each reply is spoken with."""
import unittest
from unittest.mock import patch

from pydantic import ValidationError

from sidevoice_core.pipeline import settings as language_settings
from sidevoice_core.pipeline.settings import (LanguageSettings, Transcription, Voice, default_stage, load_settings,
                                              on_host, resolve_voice, settings_from)

KOKORO = 'kokoro-82m-v1.0'


def stt(place='device', model='whisper-small', options=None, build=None):
    return Transcription(place=place, model=model, options=options or {}, build=build)


def tts(place='device', model=KOKORO, options=None, build=None):
    return Voice(place=place, model=model, options=options or {}, build=build)


class PreferencesTest(unittest.TestCase):
    def test_the_node_keeps_no_settings_and_a_device_brings_its_own(self):
        self.assertEqual(load_settings(), LanguageSettings())
        self.assertFalse(hasattr(language_settings, 'save_settings'))
        settings, problem = settings_from({
            'stt': {'place': 'openai', 'model': 'gpt-4o-transcribe', 'options': {'language': 'es', 'context': 'Sidevoice'}},
            'tts': {'place': 'device', 'model': KOKORO, 'options': {'voice': {'en': 'bf_emma', 'es': 'em_alex'}, 'speed': 1.2}},
            'unknown_future_key': 1})
        self.assertIsNone(problem)
        self.assertEqual((settings.stt.place, settings.stt.model, settings.stt.options),
                         ('openai', 'gpt-4o-transcribe', {'language': 'es', 'context': 'Sidevoice'}))
        en = resolve_voice(settings, 'en')
        self.assertEqual((en['voice'], en['speed']), ('bf_emma', 1.2))
        self.assertEqual(resolve_voice(settings, 'es')['voice'], 'em_alex')
        # What a device stores is what the node hands back: the dump validates to the same settings.
        self.assertEqual(LanguageSettings.model_validate(settings.model_dump()), settings)

    def test_a_stage_that_is_not_valid_falls_back_whole_and_says_why(self):
        settings, problem = settings_from({
            'stt': {'place': 'openai', 'model': 'gpt-4o-transcribe'},
            'tts': {'place': 'device', 'model': KOKORO, 'options': {'voice': {'es': 'em_alex'}, 'speed': 9}},
            'audio_grace_seconds': 3})
        self.assertIn('tts', problem)
        self.assertIn('speed', problem)
        self.assertEqual(settings.tts, default_stage('tts'), 'the whole stage, its valid voice with it')
        # One refused stage no longer takes the rest with it: an iPhone on OpenAI stayed on OpenAI.
        self.assertEqual((settings.stt.place, settings.audio_grace_seconds), ('openai', 3))
        # A field outside the stages keeps its own fallback, alone.
        settings, problem = settings_from({'stt': {'place': 'openai', 'model': 'gpt-4o-transcribe'}, 'audio_grace_seconds': 99})
        self.assertIn('audio_grace_seconds', problem)
        self.assertEqual((settings.stt.place, settings.audio_grace_seconds), ('openai', 1.0))
        self.assertEqual(settings_from(None), (LanguageSettings(), None))
        self.assertEqual(settings_from({}), (LanguageSettings(), None))

    def test_the_old_fields_are_dropped_not_migrated(self):
        settings, problem = settings_from({'stt_provider': 'openai', 'stt_model': 'gpt-4o-transcribe', 'stt_device': 'webgpu',
                                           'spanish_voice': 'em_alex', 'default_model': 'eleven_v3', 'tts_speed': 1.2,
                                           'language_overrides': {'fr': {'voice': 'ff_siwis'}}})
        self.assertIsNone(problem)
        self.assertEqual(settings, LanguageSettings())
        for old in ('stt_provider', 'stt_device', 'stt_model', 'stt_language', 'stt_context', 'tts_execution',
                    'tts_device', 'default_model', 'spanish_model', 'english_model', 'default_voice', 'spanish_voice',
                    'english_voice', 'language_overrides', 'default_tts_language', 'tts_speed'):
            self.assertNotIn(old, LanguageSettings.model_fields)
        for gone in ('LanguageVoice', 'LOCAL_MODELS'):
            self.assertFalse(hasattr(language_settings, gone))

    def test_a_device_that_sent_nothing_gets_english(self):
        # The node cannot know the person's system; the device sends its own language, and English is the fallback.
        settings = LanguageSettings()
        self.assertEqual((settings.ui_language, settings.stt.options['language']), ('en', 'en'))
        self.assertEqual(resolve_voice(settings)['voice'], 'af_heart')
        # Speaking Spanish still takes a Spanish voice.
        self.assertEqual(resolve_voice(settings, 'es')['voice'], 'ef_dora')

    def test_default_turn_silence_is_two_and_a_half_seconds(self):
        settings = LanguageSettings()
        self.assertEqual(settings.user_speech_timeout, 2.5)
        self.assertEqual((settings.turn_end_mode, settings.smart_turn_min_silence, settings.smart_turn_max_silence), ('smart_turn', 0.9, 3.0))


class StageShapeTest(unittest.TestCase):
    """Where a stage runs and what it names, whatever its options."""

    def test_a_place_is_the_device_the_host_or_a_provider_of_that_task(self):
        for place, model in (('device', 'whisper-small'), ('host', 'whisper-small'), ('openai', 'gpt-4o-transcribe')):
            with self.subTest(place=place):
                self.assertEqual(stt(place, model).place, place)
        self.assertEqual(tts('elevenlabs', 'eleven_v3').place, 'elevenlabs')
        for stage, place in ((stt, 'elevenlabs'), (tts, 'openai'), (stt, 'cloud'), (stt, 'browser'), (tts, 'kokoro')):
            with self.subTest(place=place), self.assertRaises(ValidationError):
                stage(place, 'some-model')

    def test_on_the_device_or_the_host_the_model_is_a_catalogue_model_of_the_stage(self):
        with self.assertRaises(ValidationError):
            stt(model=KOKORO)
        with self.assertRaises(ValidationError):
            tts(model='whisper-small')
        with self.assertRaises(ValidationError):
            stt(place='host', model='whisper-huge')
        with self.assertRaises(ValidationError, msg='the page ids are gone: one id, the catalogue\'s'):
            stt(model='onnx-community/whisper-small')

    def test_a_provider_s_model_ids_are_the_provider_s(self):
        self.assertEqual(stt('openai', 'gpt-9-transcribe-2027-01-01').model, 'gpt-9-transcribe-2027-01-01')
        self.assertEqual(tts('elevenlabs', 'eleven_v4_preview').model, 'eleven_v4_preview')
        for malformed in ('bad/id', '-leading', 'x' * 121, ''):
            with self.subTest(model=malformed), self.assertRaises(ValidationError):
                stt('openai', malformed)

    def test_a_build_names_one_of_the_model_s_engines_and_an_accelerator(self):
        chosen = stt(build={'engine': 'sherpa-onnx', 'accelerator': 'coreml'}).build
        self.assertEqual((chosen.engine, chosen.accelerator), ('sherpa-onnx', 'coreml'))
        self.assertEqual(stt(place='host', build={'engine': 'transformers-js', 'accelerator': 'webgpu'}).build.engine,
                         'transformers-js')
        for build in ({'engine': 'mlx-audio', 'accelerator': 'metal'}, {'engine': 'sherpa-onnx', 'accelerator': ''},
                      {'engine': 'sherpa-onnx'}, {'engine': 'sherpa-onnx', 'accelerator': 'cpu', 'dtype': 'q4'}):
            with self.subTest(build=build), self.assertRaises(ValidationError):
                stt(build=build)

    def test_a_provider_runs_its_own_models_so_no_build(self):
        with self.assertRaises(ValidationError):
            stt('openai', 'gpt-4o-transcribe', build={'engine': 'sherpa-onnx', 'accelerator': 'cpu'})
        self.assertIsNone(stt('openai', 'gpt-4o-transcribe', build=None).build)

    def test_a_stage_takes_its_four_fields_and_nothing_else(self):
        with self.assertRaises(ValidationError):
            Transcription(place='device', model='whisper-small', options={}, build=None, device='webgpu')
        with self.assertRaises(ValidationError):
            Transcription(model='whisper-small')

    def test_whatever_json_a_stage_holds_it_falls_back_and_never_raises(self):
        odd = [None, True, 7, 'x', [], [1], {}, {'es': {'a': 1}}, {'es': []}, {'es': None}, 'x' * 5000]
        for task in ('stt', 'tts'):
            for value in odd:
                for stage in (value, {'place': value, 'model': 'whisper-small'},
                              {'place': 'device', 'model': value},
                              {'place': 'device', 'model': KOKORO if task == 'tts' else 'whisper-small',
                               'options': {'voice': value, 'language': value, 'context': value, 'speed': value}},
                              {'place': 'device', 'model': 'whisper-small', 'build': value}):
                    settings, _ = settings_from({task: stage})
                    self.assertIsInstance(settings, LanguageSettings)

    def test_host_is_a_valid_setting_that_the_call_refuses(self):
        settings, problem = settings_from({'tts': {'place': 'host', 'model': KOKORO}})
        self.assertIsNone(problem)
        self.assertTrue(on_host(settings))
        self.assertFalse(on_host(LanguageSettings()))
        with self.assertRaises(ValueError):
            resolve_voice(settings, 'en')


class OptionKindsTest(unittest.TestCase):
    """Options are validated by their kind, from the schema the catalogue gives the family or the provider."""

    def refused(self, stage, *args, **kwargs):
        with self.assertRaises(ValidationError):
            stage(*args, **kwargs)

    def test_language(self):
        for place, model in (('device', 'whisper-small'), ('openai', 'gpt-4o-transcribe')):
            with self.subTest(place=place):
                self.assertEqual(stt(place, model, {'language': 'es'}).options['language'], 'es')
                self.assertEqual(stt(place, model, {'language': 'auto'}).options['language'], 'auto')
                for wrong in ('de', 'ES', 5, None, ''):
                    self.refused(stt, place, model, {'language': wrong})

    def test_text(self):
        openai = ('openai', 'gpt-4o-transcribe')
        self.assertEqual(stt(*openai, {'context': 'Sidevoice, Pipecat, Kokoro'}).options['context'], 'Sidevoice, Pipecat, Kokoro')
        self.assertEqual(len(stt(*openai, {'context': 'x' * 400}).options['context']), 400)
        for wrong in ('x' * 401, 7, ['a'], None):
            with self.subTest(value=str(wrong)[:10]):
                self.refused(stt, *openai, {'context': wrong})

    def test_a_local_whisper_takes_no_context_it_would_discard(self):
        """No local path gives Whisper a prompt, so the device's schema has no context, and one sent
        anyway is refused rather than saved and ignored."""
        self.refused(stt, options={'context': 'Sidevoice'})

    def test_range(self):
        self.assertEqual(tts(options={'speed': 1.5}).options['speed'], 1.5)
        self.assertEqual((tts(options={'speed': 0.5}).options['speed'], tts(options={'speed': 2}).options['speed']), (0.5, 2.0))
        for wrong in (2.5, 0.4, True, '1', None, float('nan')):
            with self.subTest(value=wrong):
                self.refused(tts, options={'speed': wrong})
        # The bounds are the schema's own: ElevenLabs takes less than Kokoro does.
        self.assertEqual(tts('elevenlabs', 'eleven_v3', {'speed': 1.2}).options['speed'], 1.2)
        self.refused(tts, 'elevenlabs', 'eleven_v3', {'speed': 1.3})

    def test_a_model_s_voice_speaks_the_language_it_is_chosen_for(self):
        chosen = tts(options={'voice': {'es': 'em_alex', 'en': 'bf_emma', 'pt': 'pf_dora', 'fr': 'ff_siwis'}})
        self.assertEqual(chosen.options['voice'], {'es': 'em_alex', 'en': 'bf_emma', 'pt': 'pf_dora', 'fr': 'ff_siwis'})
        self.assertEqual(tts(options={'voice': {}}).options['voice'], {}, 'none chosen yet: resolved when spoken')
        for wrong in ({'es': 'af_heart'}, {'en': 'no-such-voice'}, {'de': 'af_heart'}, {'en-us': 'af_heart'},
                      'af_heart', ['af_heart'], {'es': 5}):
            with self.subTest(voice=wrong):
                self.refused(tts, options={'voice': wrong})

    def test_a_provider_s_voice_is_any_id_it_may_have(self):
        chosen = tts('elevenlabs', 'eleven_v3', {'voice': {'es': '21m00Tcm4TlvDq8ikWAM', 'en': 'a-cloned-voice'}})
        self.assertEqual(chosen.options['voice']['es'], '21m00Tcm4TlvDq8ikWAM')
        for wrong in ({'es': ''}, {'es': '   '}, {'es': 'x' * 121}, {'es': None}, {'xx': 'v'}, 'v'):
            with self.subTest(voice=str(wrong)[:20]):
                self.refused(tts, 'elevenlabs', 'eleven_v3', {'voice': wrong})

    def test_an_unknown_option_is_refused(self):
        self.refused(tts, options={'pitch': 1})
        self.refused(stt, 'openai', 'gpt-4o-transcribe', {'temperature': 0})
        self.refused(tts, 'elevenlabs', 'eleven_v3', {'stability': 0.5})

    def test_a_missing_option_takes_its_default_and_a_voice_none(self):
        self.assertEqual(stt().options, {'language': 'en'})
        self.assertEqual(tts().options, {'speed': 1.0})
        self.assertEqual(tts('elevenlabs', 'eleven_v3').options, {'speed': 1.0})
        self.assertEqual(stt('openai', 'gpt-4o-transcribe', {'context': 'x'}).options, {'language': 'en', 'context': 'x'})

    def test_the_rules_are_the_schema_s_not_a_model_s(self):
        """The same code reads any schema: a text with no `max`, a single voice, a remote single voice."""
        model = language_settings.MODELS[KOKORO]
        schema = [{'id': 'note', 'kind': 'text'},
                  {'id': 'voice', 'kind': 'voice', 'from': 'model.voices'},
                  {'id': 'narrator', 'kind': 'voice', 'from': 'remote.voices'}]
        options = language_settings._options(schema, {'note': 'x' * 1000, 'voice': 'ef_dora', 'narrator': 'abc'}, model)
        self.assertEqual(options, {'note': 'x' * 1000, 'voice': 'ef_dora', 'narrator': 'abc'})
        for given in ({'note': 'x' * 1001}, {'voice': 'nope'}, {'voice': {'es': 'ef_dora'}}, {'narrator': ''}):
            with self.subTest(given=str(given)[:30]), self.assertRaises(ValueError):
                language_settings._options(schema, given, model)


class DefaultStageTest(unittest.TestCase):
    def test_with_no_capabilities_the_catalogue_s_first_model_of_the_task(self):
        self.assertEqual(default_stage('stt').model_dump(),
                         {'place': 'device', 'model': 'whisper-tiny', 'options': {'language': 'en'}, 'build': None})
        self.assertEqual(default_stage('tts').model_dump(),
                         {'place': 'device', 'model': KOKORO, 'options': {'speed': 1.0}, 'build': None})
        self.assertEqual((load_settings().stt, load_settings().tts), (default_stage('stt'), default_stage('tts')))

    def test_with_a_page_s_capabilities_the_resolver_s_first_offer(self):
        wasm = {'runs': 'page', 'has': ['wasm']}
        self.assertEqual(default_stage('stt', wasm).model, 'whisper-tiny')
        self.assertEqual(default_stage('tts', wasm).model, KOKORO)
        # The first offer of the task is taken as the resolver gives it, not re-ranked here.
        told = []

        def resolver(catalog, capabilities, place):
            told.append((capabilities, place))
            return [{'model': KOKORO, 'task': 'tts'}, {'model': 'whisper-small', 'task': 'stt'},
                    {'model': 'whisper-tiny', 'task': 'stt'}]
        with patch.object(language_settings, 'offers', side_effect=resolver):
            self.assertEqual(default_stage('stt', wasm).model, 'whisper-small')
        self.assertEqual(told, [(wasm, 'device')])
        # A place that can run nothing of the task still gets a model to show.
        self.assertEqual(default_stage('stt', {'runs': 'native', 'os': 'linux', 'arch': 'riscv64', 'has': ['cpu']}).model,
                         'whisper-tiny')

    def test_the_language_is_the_device_s_when_the_option_knows_it(self):
        self.assertEqual(default_stage('stt', language='es').options, {'language': 'es'})
        for unknown in ('de', 'auto', None):
            with self.subTest(language=unknown):
                self.assertEqual(default_stage('stt', language=unknown).options, {'language': 'en'})
        self.assertEqual(default_stage('tts', language='es').options, {'speed': 1.0}, 'a voice is chosen when spoken')


class VoiceResolutionTest(unittest.TestCase):
    def settings(self, **stage):
        return LanguageSettings(tts=tts(**stage))

    def test_the_voice_chosen_for_the_language_and_the_speed(self):
        settings = self.settings(options={'voice': {'es': 'em_santa', 'en': 'bm_george'}, 'speed': 1.4})
        self.assertEqual(resolve_voice(settings, 'es'),
                         {'place': 'device', 'model': KOKORO, 'voice': 'em_santa', 'language': 'es', 'speed': 1.4})
        self.assertEqual(resolve_voice(settings, 'en')['voice'], 'bm_george')

    def test_a_language_with_no_voice_chosen_takes_the_model_s_first_of_it(self):
        settings = self.settings(options={'voice': {'es': 'em_alex'}})
        for language, item in language_settings.LANGUAGES.items():
            with self.subTest(language=language):
                voice = resolve_voice(settings, language)['voice']
                self.assertIn(voice, {v[0] for v in item['voices']})
        self.assertEqual(resolve_voice(settings, 'es')['voice'], 'em_alex')
        self.assertEqual(resolve_voice(settings, 'fr')['voice'], 'ff_siwis')

    def test_no_language_is_the_interface_s(self):
        settings = LanguageSettings(ui_language='es', tts=tts(options={'voice': {'es': 'em_alex'}}))
        self.assertEqual((resolve_voice(settings)['language'], resolve_voice(settings)['voice']), ('es', 'em_alex'))
        with self.assertRaises(ValueError):
            resolve_voice(settings, 'ja')

    def test_a_provider_s_voice_for_the_language_else_the_first_chosen(self):
        settings = self.settings(place='elevenlabs', model='eleven_v3',
                                 options={'voice': {'es': 'voz-espanola', 'en': 'english-voice'}, 'speed': 1.1})
        self.assertEqual(resolve_voice(settings, 'en'),
                         {'place': 'elevenlabs', 'model': 'eleven_v3', 'voice': 'english-voice', 'language': 'en', 'speed': 1.1})
        self.assertEqual(resolve_voice(settings, 'fr')['voice'], 'voz-espanola', 'a provider voice often speaks several')
        with self.assertRaises(ValueError):
            resolve_voice(self.settings(place='elevenlabs', model='eleven_v3'), 'es')
        self.assertEqual(resolve_voice(self.settings(place='elevenlabs', model='eleven_v3',
                                                     options={'voice': {'es': 'v'}}), 'es')['speed'], 1.0)



class NativeElevenLabsSpeedTest(unittest.IsolatedAsyncioTestCase):
    async def test_speed_is_sent_to_synthesis_with_provider_limits(self):
        from sidevoice_core.pipeline import integrations, synthesis
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

        with patch.object(integrations, 'key', return_value='test-only'), patch.object(
                __import__("aiohttp"), "ClientSession", return_value=Client()):
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
        from sidevoice_core.pipeline import synthesis
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
        from sidevoice_core.pipeline import synthesis
        voice = synthesis._voice_entry({
            'voice_id': 'multilingual-voice',
            'name': 'Polyglot',
            'verified_languages': [
                {'language': 'EN-us'}, {'language': 'de_DE'}, {'language': 'en'},
            ],
        })
        self.assertEqual(voice['languages'], ['de', 'en'])


