"""Integrations (#64): one key per provider, kept by the node, written by the owner, never read back."""
import json
import os
import stat
import tempfile
import unittest
from pathlib import Path
from unittest.mock import AsyncMock, patch

from starlette.testclient import TestClient

from sidevoice_core.pipeline import integrations, synthesis, transcription

PAGE = {'Origin': 'http://127.0.0.1:8768'}


class Keys(unittest.TestCase):
    """A temporary data directory, with no provider key in the environment."""

    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.file = self.root / 'integrations.json'
        for patcher in (patch.object(integrations, 'CREDENTIALS', self.file),
                        patch.dict(os.environ, {'SIDEVOICE_CORE_DATA_DIR': str(self.root)})):
            patcher.start()
            self.addCleanup(patcher.stop)
        for variable in ('VOICE_STT_API_KEY', 'VOICE_ELEVENLABS_API_KEY'):
            os.environ.pop(variable, None)

    def stored(self):
        return json.loads(self.file.read_text())


class StoreTests(Keys):
    def test_one_file_keyed_by_provider_private_to_the_node(self):
        integrations.save_key('openai', ' sk-openai-1234 ')
        integrations.save_key('elevenlabs', 'xi-eleven-5678')
        self.assertEqual(self.stored(), {'openai': 'sk-openai-1234', 'elevenlabs': 'xi-eleven-5678'})
        self.assertEqual(stat.S_IMODE(self.file.stat().st_mode), 0o600)
        self.assertEqual(integrations.key('elevenlabs'), 'xi-eleven-5678')
        self.assertEqual(integrations.key('openai'), 'sk-openai-1234')

    def test_the_environment_is_a_source_and_a_saved_key_wins(self):
        config = {'VOICE_STT_API_KEY': 'sk-from-env-aaaa', 'VOICE_ELEVENLABS_API_KEY': 'xi-from-env-bbbb'}
        self.assertEqual(integrations.credential_state('openai', config),
                         {'configured': True, 'source': 'environment', 'hint': '…aaaa'})
        self.assertEqual(integrations.credential_state('elevenlabs', config)['source'], 'environment')
        integrations.save_key('openai', 'sk-saved-cccc')
        self.assertEqual(integrations.key('openai', config), 'sk-saved-cccc')
        self.assertEqual(integrations.credential_state('openai', config)['source'], 'stored')
        integrations.clear_key('openai')
        self.assertEqual(integrations.credential_state('openai', config)['source'], 'environment',
                         'removing the saved key leaves the deployment\'s')

    def test_the_listing_never_carries_a_key(self):
        integrations.save_key('openai', 'sk-secret-value-9876')
        owner = integrations.listing(True, {})
        self.assertNotIn('sk-secret-value-9876', json.dumps(owner))
        rows = {row['id']: row for row in owner['providers']}
        self.assertEqual(rows['openai'], {'id': 'openai', 'label': 'OpenAI', 'capabilities': ['transcription'],
                                          'configured': True, 'source': 'stored', 'hint': '…9876',
                                          'environment': 'VOICE_STT_API_KEY'})
        self.assertEqual(rows['elevenlabs']['capabilities'], ['voice'])
        self.assertFalse(rows['elevenlabs']['configured'], 'the owner sees what can still be configured')

    def test_a_guest_sees_only_what_it_can_choose_and_nothing_about_the_key(self):
        integrations.save_key('elevenlabs', 'xi-secret-value-1111')
        guest = integrations.listing(False, {})
        self.assertEqual(guest, {'owner': False, 'providers': [
            {'id': 'elevenlabs', 'label': 'ElevenLabs', 'capabilities': ['voice'], 'configured': True}]})


class RouteTests(Keys):
    def setUp(self):
        super().setUp()
        from sidevoice_core.server.app import create_app
        self.client = TestClient(create_app(device_auth=False), base_url='http://127.0.0.1:8768')
        self.client.__enter__()
        self.addCleanup(self.client.__exit__, None, None, None)
        self.verify_openai = AsyncMock()
        self.verify_eleven = AsyncMock()
        for patcher in (patch.object(transcription, 'verify', self.verify_openai),
                        patch.object(synthesis, 'verify', self.verify_eleven)):
            patcher.start()
            self.addCleanup(patcher.stop)

    def put(self, provider, key, headers=PAGE):
        return self.client.put(f'/api/presentation/integrations/{provider}', json={'key': key}, headers=headers)

    def test_a_key_is_verified_stored_and_never_returned(self):
        answer = self.put('openai', 'sk-brand-new-key-4242')
        self.assertEqual(answer.status_code, 200, answer.text)
        self.verify_openai.assert_awaited_once_with('openai', 'sk-brand-new-key-4242')
        self.assertEqual(self.stored(), {'openai': 'sk-brand-new-key-4242'})
        for response in (answer, self.client.get('/api/presentation/integrations')):
            self.assertNotIn('sk-brand-new-key-4242', response.text)
        rows = {row['id']: row for row in answer.json()['providers']}
        self.assertEqual((rows['openai']['configured'], rows['openai']['hint']), (True, '…4242'))
        self.put('elevenlabs', 'xi-voices-1357')
        self.verify_eleven.assert_awaited_once_with('xi-voices-1357')

    def test_a_key_the_provider_refuses_is_not_stored_and_the_old_one_stays(self):
        integrations.save_key('openai', 'sk-working-0000')
        self.verify_openai.side_effect = ValueError('OpenAI rejected the key.')
        answer = self.put('openai', 'sk-typo-1111')
        self.assertEqual(answer.status_code, 422)
        self.assertEqual(answer.json()['detail'], 'OpenAI rejected the key.')
        self.assertEqual(self.stored(), {'openai': 'sk-working-0000'})

    def test_delete_removes_the_saved_key_and_keeps_the_environment(self):
        integrations.save_key('elevenlabs', 'xi-saved-2222')
        with patch.dict(os.environ, {'VOICE_ELEVENLABS_API_KEY': 'xi-env-3333'}):
            answer = self.client.delete('/api/presentation/integrations/elevenlabs', headers=PAGE)
        self.assertEqual(answer.status_code, 200)
        self.assertEqual(self.stored(), {})
        row = next(row for row in answer.json()['providers'] if row['id'] == 'elevenlabs')
        self.assertEqual((row['configured'], row['source'], row['hint']), (True, 'environment', '…3333'))

    def test_writes_come_from_a_page_and_name_a_known_provider_and_a_key(self):
        self.assertEqual(self.put('openai', 'sk-x', headers={}).status_code, 403, 'not from a script with no Origin')
        self.assertEqual(self.client.delete('/api/presentation/integrations/openai').status_code, 403)
        self.assertEqual(self.put('someone', 'k').status_code, 404)
        self.assertEqual(self.put('openai', '  ').status_code, 422)
        self.assertFalse(self.file.exists())
        self.verify_openai.assert_not_awaited()

    def test_only_the_owner_writes_and_a_guest_sees_only_configured_providers(self):
        integrations.save_key('openai', 'sk-owner-7777')
        with patch('sidevoice_core.server.devices.is_owner', return_value=False):
            self.assertEqual(self.put('elevenlabs', 'xi-guest').status_code, 403)
            self.assertEqual(self.client.delete('/api/presentation/integrations/openai', headers=PAGE).status_code, 403)
            listing = self.client.get('/api/presentation/integrations').json()
        self.assertEqual(listing, {'owner': False, 'providers': [
            {'id': 'openai', 'label': 'OpenAI', 'capabilities': ['transcription'], 'configured': True}]})
        self.assertEqual(self.stored(), {'openai': 'sk-owner-7777'})
        self.assertTrue(self.client.get('/api/presentation/integrations').json()['owner'],
                        'every paired device is the owner until guests exist')



if __name__ == '__main__':
    unittest.main()
