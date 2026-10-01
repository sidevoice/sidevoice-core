"""Integrations (#64): one key per provider, kept by the node, written from any paired device, never read back — private
from the moment the file exists, and in the order the changes were asked for."""
import asyncio
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
        listed = integrations.listing({})
        self.assertNotIn('sk-secret-value-9876', json.dumps(listed))
        self.assertEqual(list(listed), ['providers'], 'no owner, no guest: one listing for every paired device')
        rows = {row['id']: row for row in listed['providers']}
        self.assertEqual(rows['openai'], {'id': 'openai', 'label': 'OpenAI', 'capabilities': ['transcription'],
                                          'configured': True, 'source': 'stored', 'hint': '…9876',
                                          'environment': 'VOICE_STT_API_KEY'})
        self.assertEqual(rows['elevenlabs']['capabilities'], ['voice'])
        self.assertFalse(rows['elevenlabs']['configured'], 'a provider with no key is listed, to be configured')


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

    def test_any_paired_device_lists_every_provider_and_writes_keys(self):
        """A paired device has the node's full authority: it sees every provider, keyed or not, and can write."""
        listing = self.client.get('/api/presentation/integrations').json()
        self.assertEqual([(row['id'], row['configured']) for row in listing['providers']], [('openai', False), ('elevenlabs', False)])
        self.assertNotIn('owner', listing)
        self.assertEqual(self.put('openai', 'sk-any-device-7777').status_code, 200)
        self.assertEqual(self.stored(), {'openai': 'sk-any-device-7777'})



class PrivateFileTests(Keys):
    def observe_creation(self):
        """Every file the write creates, as it is at creation: its name, flags and mode, before a byte is in it."""
        created, real_open = [], os.open

        def observed(path, flags, mode=0o777, *args, **kwargs):
            descriptor = real_open(path, flags, mode, *args, **kwargs)
            if flags & os.O_CREAT:
                info = os.fstat(descriptor)
                created.append((Path(path), flags, stat.S_IMODE(info.st_mode), info.st_size))
            return descriptor
        return created, patch('sidevoice_core.storage.os.open', side_effect=observed)

    def test_the_keys_file_is_private_from_its_creation_under_a_name_of_its_own(self):
        created, observing = self.observe_creation()
        previous = os.umask(0)   # the loosest a process can run with: nothing but the creation mode protects it
        try:
            with observing:
                integrations.save_key('openai', 'sk-first-1111')
                integrations.save_key('elevenlabs', 'xi-second-2222')
        finally:
            os.umask(previous)
        self.assertEqual(len(created), 2, 'each write creates its file through the private path')
        for path, flags, mode, size in created:
            self.assertEqual((mode, size), (0o600, 0), 'private before a byte of the secret is written')
            self.assertTrue(flags & os.O_EXCL, 'created, never reused')
            self.assertEqual(path.parent, self.file.parent)
            self.assertNotEqual(path.name, 'integrations.tmp', 'no fixed name another writer could share')
        self.assertNotEqual(created[0][0], created[1][0])
        self.assertEqual([entry.name for entry in self.root.iterdir()], ['integrations.json'], 'no temporary is left behind')
        self.assertEqual(stat.S_IMODE(self.file.stat().st_mode), 0o600)

    def test_every_secret_this_node_keeps_is_private_from_its_creation(self):
        """The connector's credential, the journal's connector state and the ready file hold secrets too: each is
        written through the same private path as the keys (review F17), never written first and chmodded after."""
        from sidevoice_core.control.connectors import local_credential
        from sidevoice_core.control.history import RoomHistory
        from sidevoice_core.server.__main__ import write_ready
        created, observing = self.observe_creation()
        previous = os.umask(0)
        try:
            with observing:
                local_credential(RoomHistory(self.root / 'journal'), self.root / 'connector-credential.json')
                write_ready(self.root / 'core.json', {'pid': 1, 'port': 2})
        finally:
            os.umask(previous)
        names = [path.name for path, *_ in created]
        for secret in ('connector-credential.json', 'room-state.json', 'core.json'):
            self.assertTrue(any(name.startswith('.' + secret + '.') for name in names), f'{secret} through the private path: {names}')
        for path, flags, mode, size in created:
            self.assertEqual((mode, size), (0o600, 0), path.name)
            self.assertTrue(flags & os.O_EXCL, path.name)

    def test_a_write_that_fails_leaves_the_old_file_and_no_temporary(self):
        integrations.save_key('openai', 'sk-working-0000')
        with patch('sidevoice_core.storage.os.replace', side_effect=OSError('disk full')), self.assertRaises(OSError):
            integrations.save_key('openai', 'sk-never-1111')
        self.assertEqual(self.stored(), {'openai': 'sk-working-0000'})
        self.assertEqual([entry.name for entry in self.root.iterdir()], ['integrations.json'])

    def test_the_data_directory_has_one_name(self):
        from sidevoice_core.runtime import data_dir
        self.assertEqual(data_dir({'SIDEVOICE_CORE_DATA_DIR': '/srv/node'}), Path('/srv/node'))
        self.assertEqual(data_dir({'VOICE_RUNTIME_ROOT': '/srv/room'}), Path.home() / '.sidevoice' / 'core',
                         'the room\'s old name is not read')


class Answer:
    def __init__(self, status_code, body):
        self.status_code, self.text = status_code, body.decode('utf8')

    def json(self):
        return json.loads(self.text)


async def call(app, method, path, body=None):
    """One request straight into the ASGI app, as its own client: no HTTP library, no socket, no lifespan."""
    content = json.dumps(body).encode() if body is not None else b''
    headers = [(b'host', b'127.0.0.1:8768'), (b'content-type', b'application/json'),
               *((name.lower().encode(), value.encode()) for name, value in PAGE.items())]
    scope = {'type': 'http', 'asgi': {'version': '3.0'}, 'http_version': '1.1', 'method': method, 'scheme': 'http',
             'path': path, 'raw_path': path.encode(), 'query_string': b'', 'root_path': '', 'headers': headers,
             'client': ('127.0.0.1', 50000), 'server': ('127.0.0.1', 8768)}
    sent, status, chunks = False, None, []

    async def receive():
        nonlocal sent
        if sent:
            await asyncio.Event().wait()   # the client stays connected until the answer is out
        sent = True
        return {'type': 'http.request', 'body': content, 'more_body': False}

    async def send(message):
        nonlocal status
        if message['type'] == 'http.response.start':
            status = message['status']
        elif message['type'] == 'http.response.body':
            chunks.append(message.get('body', b''))
    await app(scope, receive, send)
    return Answer(status, b''.join(chunks))


class OrderingTests(unittest.IsolatedAsyncioTestCase, Keys):
    """A key is verified before it is saved; a removal or a newer key asked for meanwhile, from any client, wins."""

    async def asyncSetUp(self):
        from sidevoice_core.server.app import create_app
        self.app = create_app(device_auth=False, config={'SIDEVOICE_CORE_DATA_DIR': str(self.root)})
        self.checking, self.release = asyncio.Event(), asyncio.Event()

        async def verify(provider, key):
            if key.startswith('sk-slow'):   # the provider is taking its time with this one
                self.checking.set()
                await self.release.wait()
        patcher = patch.object(transcription, 'verify', verify)
        patcher.start()
        self.addCleanup(patcher.stop)

    def put(self, key):
        """A PUT from its own client, running on its own."""
        return asyncio.create_task(call(self.app, 'PUT', '/api/presentation/integrations/openai', {'key': key}))

    async def test_a_removal_while_a_key_is_being_verified_is_not_undone_by_it(self):
        integrations.save_key('openai', 'sk-installed-0000')
        pending = self.put('sk-slow-1111')
        await asyncio.wait_for(self.checking.wait(), 5)
        removed = await call(self.app, 'DELETE', '/api/presentation/integrations/openai')
        self.assertEqual(removed.status_code, 200)
        self.assertIsNone(integrations.stored_key('openai'))
        self.release.set()
        answer = await asyncio.wait_for(pending, 5)
        self.assertEqual(answer.status_code, 409)
        self.assertEqual(answer.json()['detail'], {
            'key': 'integration_superseded',
            'message': 'This key was replaced or removed while it was being checked, so it was not saved.'})
        self.assertIsNone(integrations.stored_key('openai'), 'the removal stands')
        self.assertEqual(self.stored(), {})

    async def test_a_newer_key_wins_over_an_older_one_still_being_verified(self):
        older = self.put('sk-slow-1111')
        await asyncio.wait_for(self.checking.wait(), 5)
        newer = await self.put('sk-newer-2222')
        self.assertEqual(newer.status_code, 200, newer.text)
        self.release.set()
        self.assertEqual((await asyncio.wait_for(older, 5)).status_code, 409)
        self.assertEqual(self.stored(), {'openai': 'sk-newer-2222'})
        # Nothing was pending this time: a key is saved as ever.
        self.assertEqual((await self.put('sk-plain-3333')).status_code, 200)
        self.assertEqual(self.stored(), {'openai': 'sk-plain-3333'})


if __name__ == '__main__':
    unittest.main()
