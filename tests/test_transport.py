"""Where a credential may travel in clear (`server.transport`): loopback, and the cluster hosts whoever deploys
the node named. No host name is trusted for its spelling."""
import asyncio
import unittest

from sidevoice_core.server.rendezvous import Rendezvous
from sidevoice_core.server.transport import credential_safe, plaintext_allowed, trusted_cluster_hosts

CLUSTER = {'SIDEVOICE_TRUSTED_CLUSTER_HOSTS': ' .svc.cluster.local , room.internal ,, . '}


class PlaintextTests(unittest.TestCase):
    def test_loopback_only_by_default(self):
        for host in ('127.0.0.1', 'localhost', 'LOCALHOST', '::1', '[::1]'):
            self.assertTrue(plaintext_allowed(host, {}), host)
        for host in ('room.voice.svc', 'room.voice.svc.cluster.local', 'node.team.svc.example.com', '10.0.0.2',
                     '192.168.1.20', 'room.example', '', None):
            self.assertFalse(plaintext_allowed(host, {}), host)

    def test_configured_suffixes_and_hosts(self):
        self.assertEqual(trusted_cluster_hosts(CLUSTER), ('.svc.cluster.local', 'room.internal'))
        for host in ('room.voice.svc.cluster.local', 'ROOM.VOICE.SVC.CLUSTER.LOCAL', 'room.internal', 'room.internal.'):
            self.assertTrue(plaintext_allowed(host, CLUSTER), host)
        for host in ('svc.cluster.local', 'evilsvc.cluster.local', 'room.voice.svc.cluster.local.attacker.net',
                     'node.team.svc.example.com', 'a.room.internal', 'room.internal.attacker.net'):
            self.assertFalse(plaintext_allowed(host, CLUSTER), host)

    def test_a_credential_needs_https_outside_those(self):
        for url in ('https://room.example', 'wss://room.example/link', 'http://127.0.0.1:8768', 'http://[::1]:8768',
                    'ws://localhost:1'):
            self.assertTrue(credential_safe(url, {}), url)
        for url in ('http://room.example', 'ws://room.example', 'http://192.168.1.20:8768', 'ftp://room.example',
                    'http://room.voice.svc.cluster.local', 'https://', 'not a url', 'http://[::1'):
            self.assertFalse(credential_safe(url, {}), url)
        self.assertTrue(credential_safe('http://room.voice.svc.cluster.local:8080', CLUSTER))


class DialTests(unittest.IsolatedAsyncioTestCase):
    async def test_the_node_never_dials_a_room_in_clear_outside_loopback(self):
        heard = []

        async def on_state(state):
            heard.append(state)
        rendezvous = Rendezvous(None, 'http://127.0.0.1:1', on_state=on_state)
        dialled = await asyncio.wait_for(rendezvous.dial_room(
            {'origin': 'http://room.voice.svc.cluster.local', 'connector_id': 'm', 'token': 't'}), 5)
        self.assertFalse(dialled)
        self.assertIn('https://', heard[-1]['error'])
        self.assertIsNone(rendezvous.client, 'the credential went nowhere')


if __name__ == '__main__':
    unittest.main()
