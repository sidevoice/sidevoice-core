"""A desktop shell pointed straight at this node (sidevoice-desktop, docs/RENDEZVOUS.md "Web client
contract"): its bundled page lives on another origin, so the node answers CORS for the origins it accepts,
accepts the shell's own origins without configuration, and lets that page pair this machine with a room —
by asking the connector, which owns the pairing."""
import tempfile
import unittest
from pathlib import Path

from starlette.testclient import TestClient

from sidevoice_core.control.history import RoomHistory
from sidevoice_core.control.room import Room
from sidevoice_core.server.app import create_app

APP = 'tauri://localhost'
BASE = 'http://127.0.0.1:8768'


class FakeConnector:
    """This machine's connector, as the control plane holds it: it answers `pair.request`."""

    def __init__(self, answer):
        self.answer, self.asked = answer, []

    async def send(self, event, data):
        pass

    async def request(self, event, data, *, timeout):
        self.asked.append((event, data))
        if isinstance(self.answer, Exception):
            raise self.answer
        return self.answer

    async def disconnect(self):
        pass


class DesktopShellTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.room = Room(RoomHistory(Path(self.temp.name) / 'room-state.json'))
        self.app = create_app(self.room, config={'VOICE_BROWSER_HEARTBEAT_SECONDS': '0'})

    def client(self):
        return TestClient(self.app, base_url=BASE)

    def test_a_desktop_shell_gets_cors_and_a_stranger_does_not(self):
        with self.client() as client:
            answer = client.get('/api/rendezvous', headers={'Origin': APP})
            self.assertEqual(answer.status_code, 200)
            self.assertEqual(answer.json()['kind'], 'node')
            self.assertEqual(answer.headers['access-control-allow-origin'], APP)
            self.assertIn('Origin', answer.headers['vary'])
            for windows in ('http://tauri.localhost', 'https://tauri.localhost'):
                self.assertEqual(client.get('/api/rendezvous', headers={'Origin': windows}).headers['access-control-allow-origin'], windows)
            stranger = client.get('/api/rendezvous', headers={'Origin': 'https://evil.example'})
            self.assertNotIn('access-control-allow-origin', stranger.headers)
            # The origin check is what refuses the stranger's page; CORS only lets the shell read answers.
            self.assertEqual(client.get('/api/connectors', headers={'Origin': 'https://evil.example'}).status_code, 403)
            self.assertEqual(client.get('/api/connectors', headers={'Origin': APP}).status_code, 200)

    def test_preflights_are_answered_for_accepted_origins_only(self):
        with self.client() as client:
            ok = client.options('/api/presentation/text', headers={
                'Origin': APP, 'Access-Control-Request-Method': 'POST', 'Access-Control-Request-Headers': 'content-type',
                'Access-Control-Request-Private-Network': 'true'})
            self.assertEqual(ok.status_code, 204)
            self.assertEqual(ok.headers['access-control-allow-origin'], APP)
            self.assertIn('POST', ok.headers['access-control-allow-methods'])
            self.assertIn('content-type', ok.headers['access-control-allow-headers'])
            self.assertEqual(ok.headers['access-control-allow-private-network'], 'true')
            no = client.options('/api/presentation/text', headers={'Origin': 'https://evil.example', 'Access-Control-Request-Method': 'POST'})
            self.assertNotIn('access-control-allow-origin', no.headers)

    def test_a_configured_origin_gets_cors_too(self):
        import os
        from unittest.mock import patch
        with patch.dict(os.environ, {'SIDEVOICE_ALLOWED_ORIGINS': 'https://shell.example'}), self.client() as client:
            self.assertEqual(client.get('/api/rendezvous', headers={'Origin': 'https://shell.example'}).headers['access-control-allow-origin'],
                             'https://shell.example')

    def test_a_foreign_host_is_still_refused_first(self):
        with TestClient(self.app, base_url='http://attacker.example') as client:
            answer = client.get('/api/rendezvous', headers={'Origin': APP})
            self.assertEqual(answer.status_code, 421)

    def pair(self, client, origin=APP, body=None):
        headers = {'Origin': origin} if origin else {}
        return client.post('/api/rendezvous/pair', json=body or {'room': ' https://room.example ', 'code': ' ABCD-1234 '}, headers=headers)

    def test_the_page_pairs_this_machine_through_its_connector(self):
        connector = FakeConnector({'ok': True, 'origin': 'https://room.example', 'connector_id': 'machine-9'})
        with self.client() as client:
            self.room.control.peers['machine-9'] = connector
            answer = self.pair(client)
            self.assertEqual(answer.status_code, 200, answer.text)
            self.assertEqual(answer.json(), {'ok': True, 'room': 'https://room.example', 'connector_id': 'machine-9'})
            self.assertEqual(answer.headers['access-control-allow-origin'], APP)
        self.assertEqual(connector.asked, [('pair.request', {'room': 'https://room.example', 'code': 'ABCD-1234'})])

    def test_pairing_is_a_page_s_act_not_a_script_s(self):
        connector = FakeConnector({'ok': True})
        with self.client() as client:
            self.room.control.peers['machine-9'] = connector
            self.assertEqual(self.pair(client, origin=None).status_code, 403)
            self.assertEqual(self.pair(client, origin='https://evil.example').status_code, 403)
        self.assertEqual(connector.asked, [])

    def test_what_goes_wrong_is_said(self):
        with self.client() as client:
            none = self.pair(client)
            self.assertEqual(none.status_code, 503)
            self.assertIn('conector', none.json()['detail'])
            self.room.control.peers['m'] = FakeConnector({'ok': False, 'detail': 'Pairing failed: code expired'})
            refused = self.pair(client)
            self.assertEqual(refused.status_code, 400)
            self.assertEqual(refused.json()['detail'], 'Pairing failed: code expired')
            self.room.control.peers['m'] = FakeConnector(TimeoutError('pair.request went unacknowledged'))
            self.assertEqual(self.pair(client).status_code, 504)
            self.assertEqual(client.post('/api/rendezvous/pair', json={'room': '', 'code': 'x'}, headers={'Origin': APP}).status_code, 422)
