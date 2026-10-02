import asyncio
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from starlette.testclient import TestClient

from sidevoice_core.control.history import RoomHistory
from sidevoice_core.control.room import Room
from sidevoice_core.server.app import create_app
from sidevoice_core.server.local import local_only
from sidevoice_core.server.rendezvous import relayable, relayed
import sidevoice_core.server.host_agents as host_agents


def state(*, agents=None, scanned_at='2026-10-02T12:00:00Z', custom=None):
    return {
        'agents': agents if agents is not None else [{'id': 'claude', 'registration': 'not-connected'}],
        'scanned_at': scanned_at,
        'custom': custom if custom is not None else {'command': 'sidevoice mcp', 'snippet': '{"mcpServers":{}}'},
    }


class FakeLink:
    def __init__(self, answers=()):
        self.answers = list(answers)
        self.requests = []

    async def request(self, event, data, *, timeout):
        self.requests.append((event, data, timeout))
        if not self.answers:
            await asyncio.Event().wait()
        answer = self.answers.pop(0)
        if isinstance(answer, BaseException):
            raise answer
        return answer


class HostAgentRouteTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        data = Path(self.temp.name) / 'core'
        self.room = Room(RoomHistory(data / 'room-state.json'))
        self.app = create_app(self.room, config={
            'VOICE_BROWSER_HEARTBEAT_SECONDS': '0',
            'SIDEVOICE_CORE_DATA_DIR': str(data),
        })
        _, self.token, _ = self.app.state.devices.store.registry.pair_local('test app')
        self.control = self.room.control

    def request(self, method, path, *, token=None, **kwargs):
        headers = dict(kwargs.pop('headers', {}))
        if token:
            headers['authorization'] = f'Bearer {token}'
        with TestClient(self.app, base_url='http://127.0.0.1:8768') as client:
            return client.request(method, path, headers=headers, **kwargs)

    def test_list_is_device_authenticated_and_uses_a_twenty_second_local_ack(self):
        denied = self.request('GET', '/api/host/agents')
        self.assertEqual(denied.status_code, 401)
        missing = self.request('GET', '/api/host/agents', token=self.token)
        self.assertEqual((missing.status_code, missing.json()), (503, {'key': 'no-connector'}))

        peer = FakeLink([state()])
        self.control.peers['local'] = peer
        answer = self.request('GET', '/api/host/agents?rescan=1&watch=claude', token=self.token)
        self.assertEqual(answer.status_code, 200, answer.text)
        self.assertEqual(answer.json(), state())
        self.assertEqual(peer.requests, [('agents.list', {'rescan': True, 'watch': 'claude'}, 20.0)])

    def test_actions_forward_only_the_detected_agent_id_and_return_updated_state(self):
        for action in ('connect', 'disconnect', 'dismiss'):
            with self.subTest(action=action):
                peer = FakeLink([state(agents=[{'id': 'claude', 'registration': 'connected'}])])
                self.control.peers['local'] = peer
                answer = self.request('POST', f'/api/host/agents/claude/{action}', token=self.token,
                                      json={'command': '/tmp/attacker-chosen', 'path': '/tmp/attacker-chosen'})
                self.assertEqual(answer.status_code, 200, answer.text)
                self.assertEqual(answer.json()['agents'][0]['registration'], 'connected')
                self.assertEqual(peer.requests, [(f'agents.{action}', {'id': 'claude'}, 20.0)])

    def test_connector_errors_keep_the_key_and_safe_params_without_raw_output(self):
        peer = FakeLink([{'error': {
            'key': 'agent.connect-failed',
            'params': {'agent': 'claude', 'code': 2, 'stderr': 'secret output', 'detail': 'raw error'},
            'message': 'Could not connect Claude Code.',
        }}])
        self.control.peers['local'] = peer
        answer = self.request('POST', '/api/host/agents/claude/connect', token=self.token)
        self.assertEqual(answer.status_code, 409)
        self.assertEqual(answer.json(), {'error': {
            'key': 'agent.connect-failed',
            'params': {'agent': 'claude', 'code': 2},
            'message': 'Could not connect Claude Code.',
        }})
        self.assertNotIn('secret', answer.text)

    def test_timeout_is_enforced_even_for_a_peer_that_ignores_its_timeout_argument(self):
        peer = FakeLink()
        self.control.peers['local'] = peer
        with patch.object(host_agents, 'REQUEST_TIMEOUT_SECONDS', 0.02):
            answer = self.request('GET', '/api/host/agents', token=self.token)
        self.assertEqual((answer.status_code, answer.json()), (504, {'key': 'connector-timeout'}))
        self.assertEqual(peer.requests[0][2], 0.02)

    def test_relay_exposes_the_authenticated_host_api_but_not_the_local_connector_link(self):
        self.assertTrue(relayed('/api/host/agents'))
        self.assertTrue(relayable('http://127.0.0.1:8768', '/api/host/agents'))
        self.assertFalse(relayed('/api/connectors/link'))
        self.assertFalse(relayable('http://127.0.0.1:8768', '/api/connectors/link'))
        self.assertFalse(local_only('/api/host/agents'))
        self.assertTrue(local_only('/api/connectors/link'))

    def test_invalid_watch_ids_return_a_keyed_error_without_contacting_the_connector(self):
        peer = FakeLink([state()])
        self.control.peers['local'] = peer
        answer = self.request('GET', '/api/host/agents?watch=../../bin/sh', token=self.token)
        self.assertEqual((answer.status_code, answer.json()), (400, {'key': 'invalid-agent-id'}))
        bad_rescan = self.request('GET', '/api/host/agents?rescan=sometimes', token=self.token)
        self.assertEqual((bad_rescan.status_code, bad_rescan.json()), (400, {'key': 'invalid-rescan'}))
        self.assertEqual(peer.requests, [])

    def test_foreign_page_origins_are_refused_with_a_stable_key(self):
        peer = FakeLink([state()])
        self.control.peers['local'] = peer
        answer = self.request('GET', '/api/host/agents', token=self.token,
                              headers={'origin': 'https://attacker.example'})
        self.assertEqual((answer.status_code, answer.json()), (403, {'key': 'origin-not-allowed'}))
        self.assertEqual(peer.requests, [])


if __name__ == '__main__':
    unittest.main()
