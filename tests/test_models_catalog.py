"""The model catalogue (rubasace/sidevoice#124 §3–§4): the shipped file is sound, the validator refuses what
must never ship, the reference resolver passes the shared vectors, the catalogue agrees with the other lists
this node still keeps, and the node serves it."""
import copy
import json
import unittest

from starlette.testclient import TestClient

from sidevoice_core.models.catalog import catalog_text, check, load, vectors
from sidevoice_core.models.offers import UnknownPlace, offers
from sidevoice_core.pipeline.settings import CATALOG as VOICE_CATALOG
from sidevoice_core.server.app import create_app


def model(catalog, name):
    return next(item for item in catalog['models'] if item['id'] == name)


def build(item, engine):
    return next(entry for entry in item['builds'] if entry['engine'] == engine)


class CatalogTest(unittest.TestCase):
    def test_the_shipped_catalogue_is_sound(self):
        catalog = load()
        self.assertEqual(check(catalog), [])
        tasks = {catalog['families'][item['family']]['task'] for item in catalog['models']}
        self.assertEqual(tasks, {'stt', 'tts'})
        self.assertTrue(any(voice['language'] == 'es' for voice in model(catalog, 'kokoro-82m-v1.0')['voices']))

    def test_what_runs_today_and_nothing_more(self):
        catalog = load()
        self.assertEqual([item['id'] for item in catalog['models']],
                         ['whisper-tiny', 'whisper-base', 'whisper-small', 'whisper-large-v3-turbo', 'kokoro-82m-v1.0'])
        self.assertEqual({engine['id'] for engine in catalog['engines']}, {'sherpa-onnx', 'transformers-js'})
        self.assertEqual({provider['id']: provider['tasks'] for provider in catalog['providers']},
                         {'openai': ['stt'], 'elevenlabs': ['tts']})
        self.assertFalse(any('requires' in item for item in catalog['models']), 'no memory figure until measured')

    def test_check_catches_broken_entries(self):
        broken = load()
        broken['models'][0]['builds'].append({**broken['models'][0]['builds'][0], 'engine': 'nope'})
        broken['engines'][0]['packages'][0]['download']['sha256'] = 'short'
        broken['engines'][0]['packages'][0]['libraries'][0]['sha256'] = ''
        broken['engines'][0]['packages'][0]['libraries'][1]['path'] = '../evil.dylib'
        broken['families']['whisper']['options'].append({'id': 'mood', 'kind': 'colour-wheel'})
        problems = check(broken)
        self.assertTrue(any('unknown engine nope' in problem for problem in problems))
        self.assertTrue(any('download with sha256' in problem for problem in problems))
        self.assertEqual(len([problem for problem in problems if 'library' in problem]), 2)
        self.assertTrue(any("unknown kind 'colour-wheel'" in problem for problem in problems))

    def test_a_build_must_suit_its_engine(self):
        for change, expected in [
                (lambda c: build(model(c, 'kokoro-82m-v1.0'), 'sherpa-onnx').update(format='ggml'), 'format ggml not read'),
                (lambda c: c['engines'][0]['families'].remove('kokoro'), 'does not run the kokoro family'),
                (lambda c: model(c, 'whisper-tiny')['builds'].append(model(c, 'whisper-tiny')['builds'][0]), 'two builds on sherpa-onnx'),
                (lambda c: build(model(c, 'whisper-tiny'), 'sherpa-onnx').pop('download'), 'native build needs an https download'),
                (lambda c: build(model(c, 'whisper-tiny'), 'sherpa-onnx')['download'].update(url='http://example.org/x'), 'https'),
                (lambda c: build(model(c, 'whisper-tiny'), 'transformers-js').update(accelerators=['cuda']), 'never uses'),
                (lambda c: model(c, 'kokoro-82m-v1.0').update(voices=[]), 'lists no voices'),
                (lambda c: model(c, 'whisper-tiny').update(family='parakeet'), 'unknown family parakeet'),
                (lambda c: model(c, 'whisper-tiny').update(requires={'memory_mb': 'lots'}), 'memory_mb'),
                (lambda c: c['providers'][0].update(id='host'), 'not a place'),
                (lambda c: c['families']['kokoro']['options'][1].update(default=3), 'default outside'),
                (lambda c: c['providers'][0]['stt']['options'][1].update(max='400'), 'text context needs a positive whole max'),
                (lambda c: c['providers'][0]['stt']['options'][1].update(max=True), 'text context needs a positive whole max'),
                (lambda c: c['ranking']['default'].append('mlx-audio'), 'ranking: unknown engine mlx-audio'),
                (lambda c: c.update(version=1), 'version must be 2')]:
            catalog = load()
            change(catalog)
            with self.subTest(expected=expected):
                self.assertTrue(any(expected in problem for problem in check(catalog)), check(catalog))

    def test_a_bundled_package_downloads_nothing_and_needs_no_hash(self):
        catalog = load()
        catalog['engines'][0]['packages'].append({'os': 'ios', 'bundled': True, 'accelerators': ['coreml', 'cpu']})
        self.assertEqual(check(catalog), [])


class OffersTest(unittest.TestCase):
    def test_the_shared_vectors(self):
        shared = vectors()
        self.assertEqual(check(shared['fixture']), [], 'the fixture is a sound catalogue itself')
        catalogs = {'shipped': load(), 'fixture': shared['fixture']}
        for vector in shared['vectors']:
            with self.subTest(vector=vector['name']):
                catalog = copy.deepcopy(catalogs[vector['catalog']])
                if 'error' in vector:
                    with self.assertRaises(UnknownPlace):
                        offers(catalog, vector['capabilities'], vector['place'])
                else:
                    self.assertEqual(offers(catalog, vector['capabilities'], vector['place']), vector['offers'])

    def test_the_vectors_cover_both_catalogues_and_every_way_of_choosing(self):
        shared = vectors()
        reasons = {offer['reason'].split(': ', 1)[1] for vector in shared['vectors'] for offer in vector.get('offers', [])}
        self.assertEqual(reasons, {'the only build that runs here', 'this model ranks it first on macos-aarch64',
                                   "first in the catalogue's engine order"})
        self.assertEqual({vector['catalog'] for vector in shared['vectors']}, {'shipped', 'fixture'})
        self.assertTrue(any('error' in vector for vector in shared['vectors']))

    def test_the_resolver_leaves_the_catalogue_as_it_was(self):
        catalog = load()
        offers(catalog, {'runs': 'page', 'has': ['webgpu', 'webgpu-f16', 'wasm']}, 'device')
        self.assertEqual(catalog, load())


class OtherListsAgreeTest(unittest.TestCase):
    """What this node still keeps elsewhere, until later phases move it onto the catalogue: the voice
    catalogue's voices, and the integrations' providers."""

    def test_the_voice_catalogue_names_no_models(self):
        """Which models exist is the model catalogue's alone: the voice catalogue keeps languages, their voices'
        names and samples, and no model list of its own (review R12)."""
        from sidevoice_core.pipeline.settings import CATALOG
        self.assertNotIn('models', CATALOG)

    def test_kokoro_speaks_the_voices_the_voice_catalogue_offers(self):
        voices = [voice['id'] for voice in model(load(), 'kokoro-82m-v1.0')['voices']]
        self.assertEqual(voices, [voice[0] for language in VOICE_CATALOG['languages'] for voice in language['voices']])
        for language in VOICE_CATALOG['languages']:
            spoken = {voice['language'].split('-')[0] for voice in model(load(), 'kokoro-82m-v1.0')['voices']
                      if voice['id'] in {v[0] for v in language['voices']}}
            self.assertEqual(spoken, {language['id']}, language['id'])

    def test_the_providers_are_the_integrations_own(self):
        from sidevoice_core.pipeline import integrations
        tasks = {'transcription': 'stt', 'voice': 'tts'}
        listed = {name: {'label': entry['label'], 'tasks': [tasks[c] for c in entry['capabilities']]}
                  for name, entry in integrations.PROVIDERS.items()}
        catalogued = {provider['id']: {'label': provider['label'], 'tasks': provider['tasks']} for provider in load()['providers']}
        self.assertEqual(catalogued, listed)
        self.assertTrue(all(provider['key'] for provider in load()['providers']), 'every integration takes a key')

    def test_openai_takes_whisper_s_language_and_a_context_the_device_does_not(self):
        """A device switching between its own Whisper and OpenAI keeps its language: the two schemas say the same
        about it. The context is OpenAI's alone: its prompt uses it, and no local Whisper path does, so the device's
        schema does not offer a setting that would be accepted and discarded (review R09)."""
        catalog = load()
        whisper = catalog['families']['whisper']['options']
        openai = next(provider for provider in catalog['providers'] if provider['id'] == 'openai')['stt']['options']
        self.assertEqual([option['id'] for option in whisper], ['language'])
        self.assertEqual(openai[0], whisper[0])
        self.assertEqual(openai[1], {'id': 'context', 'kind': 'text', 'max': 400})
        from sidevoice_core.pipeline.settings import LanguageSettings
        self.assertEqual(LanguageSettings().stt.options['language'],
                         next(option for option in whisper if option['id'] == 'language')['default'])


class PageBuildTest(unittest.TestCase):
    def test_every_page_build_says_what_it_downloads_on_each_accelerator(self):
        """What a page asks consent for before it downloads a model (#124 §6 step 1): one size per accelerator,
        since each loads its own precision of the model."""
        for item in load()['models']:
            for entry in item['builds']:
                if entry['engine'] != 'transformers-js':
                    continue
                with self.subTest(model=item['id']):
                    config = entry['config']
                    self.assertEqual(set(config['sizes']), set(config['dtype']))
                    self.assertTrue(all(isinstance(size, int) and size > 0 for size in config['sizes'].values()))


class EndpointTest(unittest.TestCase):
    def test_the_node_serves_the_file_as_shipped(self):
        app = create_app(device_auth=False)
        with TestClient(app, base_url='http://127.0.0.1:8768') as client:
            answer = client.get('/api/models/catalog')
            self.assertEqual(answer.status_code, 200)
            self.assertEqual(answer.headers['content-type'], 'application/json')
            self.assertEqual(answer.text, catalog_text())
            self.assertEqual(answer.json(), load())
            self.assertEqual(client.get('/api/models/catalog', headers={'Origin': 'https://elsewhere.example'}).status_code, 403)


if __name__ == '__main__':
    unittest.main()
