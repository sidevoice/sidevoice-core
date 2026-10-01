"""Transcription by place: the client itself (`device`), a provider from this node (`openai`), or the host —
which is not available yet. And what a client reports about the runtime it transcribes with."""
import asyncio
import unittest

from sidevoice_core.pipeline import transcription
from sidevoice_core.pipeline.call import browser_runtime
from sidevoice_core.pipeline.settings import LanguageSettings, Transcription
from sidevoice_core.pipeline.transcribers import ClientTranscriber, OpenAITranscriber

KEY = {'VOICE_STT_API_KEY': 'test-key'}


def settings(place='device', model='whisper-small', **options):
    return LanguageSettings(stt=Transcription(place=place, model=model, options=options))


class ResolveTests(unittest.TestCase):
    def test_on_the_device_the_client_transcribes_with_the_catalogue_model(self):
        choice = transcription.resolve(settings(language='es'), {})
        self.assertEqual(choice, {'place': 'device', 'model': 'whisper-small', 'language': 'es',
                                  'context': '', 'available': True})
        self.assertIsNone(transcription.resolve(settings(language='auto'), {})['language'], 'auto is detection')

    def test_a_provider_is_available_with_its_key_and_keeps_its_model(self):
        choice = transcription.resolve(settings('openai', 'gpt-5-mini-transcribe-2026-09-01'), KEY)
        self.assertEqual((choice['place'], choice['model'], choice['language'], choice['context'], choice['available']),
                         ('openai', 'gpt-5-mini-transcribe-2026-09-01', 'en', '', True))
        self.assertFalse(transcription.resolve(settings('openai', 'gpt-4o-transcribe'), {})['available'])

    def test_the_host_is_not_available_yet(self):
        self.assertFalse(transcription.resolve(settings('host'), KEY)['available'])

    def test_the_old_catalogue_and_its_lists_are_gone(self):
        for gone in ('BROWSER_MODELS', 'CATALOG', 'PROVIDERS'):
            self.assertFalse(hasattr(transcription, gone), gone)


class BuildTests(unittest.TestCase):
    def test_the_device_gets_the_turns_to_transcribe_itself(self):
        sent = []
        choice = transcription.resolve(settings(language='fr'), {})
        provider = transcription.build(choice, config={}, send=sent.append, session_id='s1')
        self.assertIsInstance(provider, ClientTranscriber)
        self.assertEqual((provider.session_id, provider.language), ('s1', 'fr'))
        with self.assertRaises(ValueError):
            transcription.build(choice, config={})

    def test_openai_is_called_from_here_with_the_key_the_model_the_language_and_the_context(self):
        choice = transcription.resolve(settings('openai', 'gpt-4o-mini-transcribe', language='es', context='Sidevoice, Pipecat'), KEY)
        provider = transcription.build(choice, config=KEY)
        self.assertIsInstance(provider, OpenAITranscriber)
        self.assertEqual((provider.model, provider.language, provider.prompt), ('gpt-4o-mini-transcribe', 'es', 'Sidevoice, Pipecat'))
        empty = transcription.build(transcription.resolve(settings('openai', 'whisper-1'), KEY), config=KEY)
        self.assertIsNone(empty.prompt, 'no context is no prompt')
        with self.assertRaises(ValueError):
            transcription.build(choice, config={})

    def test_nothing_is_built_on_the_host(self):
        with self.assertRaises(ValueError):
            transcription.build(transcription.resolve(settings('host'), {}), config={}, send=print, session_id='s1')


class RuntimeReportTests(unittest.TestCase):
    """What a client says it transcribes with: a catalogue model, one of its engines, an accelerator."""

    def test_a_catalogue_model_on_one_of_its_engines(self):
        self.assertEqual(browser_runtime({'model': 'whisper-small', 'engine': 'sherpa-onnx', 'accelerator': 'coreml', 'cached': True}),
                         {'model': 'whisper-small', 'engine': 'sherpa-onnx', 'accelerator': 'coreml', 'cached': True})
        self.assertFalse(browser_runtime({'model': 'whisper-tiny', 'engine': 'transformers-js', 'accelerator': 'wasm'})['cached'])
        self.assertIsNone(browser_runtime(None))

    def test_anything_else_is_refused(self):
        for report in ({'model': 'onnx-community/whisper-small', 'engine': 'transformers-js', 'accelerator': 'webgpu'},
                       {'model': 'kokoro-82m-v1.0', 'engine': 'sherpa-onnx', 'accelerator': 'cpu'},
                       {'model': 'whisper-small', 'engine': 'mlx-audio', 'accelerator': 'metal'},
                       {'model': 'whisper-small', 'engine': 'sherpa-onnx', 'accelerator': ''},
                       {'model': 'whisper-small', 'engine': 'sherpa-onnx'},
                       {'model': 'whisper-small', 'device': 'webgpu'},
                       {'model': ['whisper-small'], 'engine': 'sherpa-onnx', 'accelerator': 'cpu'}):
            with self.subTest(report=report), self.assertRaises(ValueError):
                browser_runtime(report)

    def test_a_fallback_keeps_its_reason_bounded(self):
        runtime = browser_runtime({'model': 'whisper-base', 'engine': 'transformers-js', 'accelerator': 'wasm', 'cached': False,
                                   'fallback_from': 'webgpu' * 10, 'fallback_error': 'GPU adapter lost' * 50})
        self.assertEqual((runtime['accelerator'], len(runtime['fallback_from']), len(runtime['fallback_error'])), ('wasm', 20, 300))
        self.assertNotIn('fallback_from', browser_runtime({'model': 'whisper-base', 'engine': 'transformers-js',
                                                           'accelerator': 'wasm', 'fallback_from': 'webgpu'}))


class RemoteCatalogueTests(unittest.TestCase):
    def test_remote_catalog_filters_to_transcription_models(self):
        class Response:
            status = 200
            async def __aenter__(self): return self
            async def __aexit__(self, *args): pass
            async def json(self):
                return {'data': [
                    {'id': 'gpt-4o-transcribe'}, {'id': 'gpt-4o-mini-transcribe'},
                    {'id': 'gpt-4o-realtime-preview'}, {'id': 'gpt-live-transcribe'}, {'id': 'gpt-4.1'}, {'id': 'whisper-1'},
                ]}
        class Http:
            def get(self, *args, **kwargs): return Response()
        models = asyncio.run(transcription._models(Http(), 'secret'))
        self.assertEqual([item['id'] for item in models],
                         ['gpt-4o-transcribe', 'gpt-4o-mini-transcribe', 'whisper-1'])


if __name__ == '__main__':
    unittest.main()
