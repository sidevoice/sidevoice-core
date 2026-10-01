"""The node's local socket (`server.local`), against a real node serving both listeners: what only the socket
serves — the readiness probe, the app's own pairing and its undoing, the connector's link — and that none of it
exists over TCP or through a room's relay; that pairing the app again replaces it, calls included; and that every
other route on the socket still wants a device token."""
import asyncio
import errno
import json
import os
import socket
import stat
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import aiohttp
import socketio
from aiortc import RTCPeerConnection, RTCSessionDescription

from sidevoice_core.control.connectors import PROTOCOL as CONNECTOR_PROTOCOL
from sidevoice_core.control.devices import DeviceRegistry
from sidevoice_core.server.local import LocalSocket, local_only
from sidevoice_core.server.rendezvous import relayable
from sidevoice_core.storage import unsafe_directory
from test_rendezvous import LOCAL, DialTests, NodeTest, link_client, through, until
from test_webrtc import RecordedVoice

LOCAL_PATHS = [('GET', '/api/local/health'), ('POST', '/api/device/local/pair'), ('DELETE', '/api/device/local'),
               ('GET', '/api/connectors/link/?EIO=4&transport=polling')]
HELLO = json.dumps({'label': 'rtvi-ai', 'type': 'client-ready', 'id': 'x', 'data': {'settings': {'turn_end_mode': 'timer'}}})


def auth(token):
    return {'Authorization': f'Bearer {token}'}


class LocalSocketTests(NodeTest):
    device_auth = True   # the node as `server.__main__` runs it

    async def asyncSetUp(self):
        await super().asyncSetUp()
        self.local_http = through(self.socket_path)
        # A connection per request: what these check is the node's answer, never whether a pooled connection the
        # node may be closing at that moment (keep-alive) is still there.
        self.tcp_http = aiohttp.ClientSession(connector=aiohttp.TCPConnector(force_close=True))
        self.addAsyncCleanup(self.local_http.close)
        self.addAsyncCleanup(self.tcp_http.close)
        self.tcp = f'http://127.0.0.1:{self.node_port}'

    async def via_socket(self, method, path, **options):
        async with self.local_http.request(method, LOCAL + path, **options) as answer:
            return answer.status, await answer.json(content_type=None)

    async def over_tcp(self, method, path, **options):
        async with self.tcp_http.request(method, self.tcp + path, **options) as answer:
            return answer.status, await answer.text()

    async def pair_local(self, name='Sidevoice (app)'):
        status, body = await self.via_socket('POST', '/api/device/local/pair', json={'name': name})
        self.assertEqual(status, 200, body)
        return body

    async def call(self, token, session=None):
        """A call socket opened with `token` through the socket (or `session`)."""
        return await (session or self.local_http).ws_connect(
            'ws://localhost/api/presentation/ws', protocols=['sidevoice', f'sidevoice.token.{token}'])

    async def test_the_socket_is_this_user_s_alone(self):
        self.assertEqual(stat.S_IMODE(os.stat(self.socket_path).st_mode), 0o600)

    async def test_the_health_probe_says_which_launch_and_what_it_is(self):
        self.app.state.launch_id = 'launch-42'
        status, health = await self.via_socket('GET', '/api/local/health')
        self.assertEqual(status, 200)
        identity = self.app.state.devices.store.identity
        self.assertEqual(set(health), {'launch_id', 'pid', 'version', 'api', 'fingerprint', 'public_key', 'host', 'calls'})
        self.assertEqual((health['launch_id'], health['pid'], health['api'], health['calls']), ('launch-42', os.getpid(), 1, 0))
        self.assertEqual((health['fingerprint'], health['public_key']), (identity.fingerprint, identity.public_key))
        self.assertEqual(health['host'], self.app.state.devices.host())
        # A call in progress is counted: a supervisor waits for none before it restarts the core.
        ws = await self.call((await self.pair_local())['token'])
        await ws.send_str(HELLO)
        await until(lambda: self.node_room.clients, timeout=30)
        self.assertEqual((await self.via_socket('GET', '/api/local/health'))[1]['calls'], 1)
        await ws.close()
        await until(lambda: not self.node_room.clients, timeout=15)

    async def test_a_call_socket_counts_from_its_acceptance_to_its_close(self):
        """Before its first message too: a supervisor that read `calls: 0` there would restart under a call."""
        ws = await self.call((await self.pair_local())['token'])
        await until(lambda: self.app.state.devices.open_calls() == 1)
        self.assertEqual((await self.via_socket('GET', '/api/local/health'))[1]['calls'], 1, 'no hello sent')
        self.assertEqual(self.node_room.clients, {}, 'not in the room yet: still a call')
        await ws.close()
        await until(lambda: self.app.state.devices.open_calls() == 0)
        self.assertEqual((await self.via_socket('GET', '/api/local/health'))[1]['calls'], 0)

    async def initialised_call(self, token):
        """A call past its hello, in the room, with its microphone moved to WebRTC: what a revoked app must lose."""
        ws = await self.call(token)
        await ws.send_str(HELLO)
        client = await until(lambda: next(iter(self.node_room.clients.values()), None), timeout=30)
        await until(lambda: client.connected and client.feed_audio is not None, timeout=30)
        browser = RTCPeerConnection()
        self.addAsyncCleanup(browser.close)
        browser.addTrack(RecordedVoice(b'\x00\x00' * 16000))
        await browser.setLocalDescription(await browser.createOffer())
        status, answer = await self.via_socket('POST', '/api/presentation/rtc/offer', headers=auth(token), json={
            'session_id': client.id, 'sdp': browser.localDescription.sdp, 'type': 'offer'})
        self.assertEqual(status, 200, answer)
        await browser.setRemoteDescription(RTCSessionDescription(**answer))
        await until(lambda: browser.connectionState == 'connected', timeout=20)
        self.assertEqual((await self.via_socket('GET', '/api/local/health'))[1]['calls'], 1, 'media is not a second call')
        return ws, client, client.media_peer

    async def assert_ended(self, ws, client, peer):
        message = await ws.receive(timeout=10)
        while message.type == aiohttp.WSMsgType.TEXT:   # what the call was saying before it was ended
            message = await ws.receive(timeout=10)
        self.assertEqual((message.type, ws.close_code), (aiohttp.WSMsgType.CLOSE, 4401))
        await until(lambda: not self.node_room.clients, timeout=15)
        await until(lambda: peer.pc.connectionState == 'closed', timeout=15)
        self.assertIsNone(client.media_peer, 'its media went with it')
        await until(lambda: self.app.state.devices.open_calls() == 0)

    async def test_replacing_or_removing_the_app_ends_a_call_in_progress_media_included(self):
        with patch.dict(os.environ, {'SIDEVOICE_STUN_URLS': ''}):   # host candidates only: no network
            first = await self.pair_local('first')
            call = await self.initialised_call(first['token'])
            await self.pair_local('second')
            await self.assert_ended(*call)
            second_call = await self.initialised_call((await self.pair_local('third'))['token'])
            self.assertEqual(await self.via_socket('DELETE', '/api/device/local'), (200, {'ok': True, 'revoked': True}))
            await self.assert_ended(*second_call)

    async def test_a_removal_that_could_not_be_written_is_still_there_to_finish(self):
        """A failed write leaves the app paired, in memory as on disk: the retry finds it, revokes it for good and
        ends its call — never a 'nothing to revoke' while its token still works after a restart."""
        paired = await self.pair_local()
        ws = await self.call(paired['token'])
        await until(lambda: self.app.state.devices.open_calls() == 1)
        registry = self.app.state.devices.store.registry
        with patch.object(registry, 'save', side_effect=OSError('disk full')):
            async with self.local_http.delete(LOCAL + '/api/device/local') as failed:
                self.assertEqual(failed.status, 500)
        self.assertEqual(registry.authenticate(paired['token']), paired['device_id'], 'still paired')
        self.assertEqual(self.app.state.devices.open_calls(), 1, 'and its call goes on: nothing was revoked')
        self.assertEqual(await self.via_socket('DELETE', '/api/device/local'), (200, {'ok': True, 'revoked': True}))
        message = await ws.receive(timeout=10)
        self.assertEqual((message.type, ws.close_code), (aiohttp.WSMsgType.CLOSE, 4401))
        reloaded = DeviceRegistry(Path(self.temp.name) / 'core' / 'devices.json')
        self.assertIsNone(reloaded.authenticate(paired['token']), 'gone from the file too')
        self.assertEqual(reloaded.devices, {})

    async def test_the_app_pairs_with_no_code_and_unpairs_itself(self):
        paired = await self.pair_local()
        self.assertEqual(set(paired), {'device_id', 'token', 'node'})
        self.assertEqual(paired['node'], self.app.state.devices.node())
        status, listing = await self.via_socket('GET', '/api/device/devices', headers=auth(paired['token']))
        self.assertEqual(status, 200)
        self.assertEqual([(d['id'], d['name'], d['kind'], d['current']) for d in listing['devices']],
                         [(paired['device_id'], 'Sidevoice (app)', 'local', True)])
        saved = json.loads((Path(self.temp.name) / 'core' / 'devices.json').read_text())['devices']
        self.assertEqual(saved[paired['device_id']]['kind'], 'local', 'kept with the device')
        # The token works over TCP too, like any device's (the app's proxy is what carries it there).
        self.assertEqual((await self.over_tcp('GET', '/api/device/devices', headers=auth(paired['token'])))[0], 200)
        self.assertEqual(await self.via_socket('DELETE', '/api/device/local'), (200, {'ok': True, 'revoked': True}))
        self.assertEqual((await self.via_socket('GET', '/api/device/devices', headers=auth(paired['token'])))[0], 401)
        self.assertEqual(await self.via_socket('DELETE', '/api/device/local'), (200, {'ok': True, 'revoked': False}))

    async def test_pairing_the_app_again_revokes_the_previous_one_and_ends_its_call(self):
        code = self.app.state.devices.issue_code()
        async with self.tcp_http.post(self.tcp + '/api/device/pair', json={'secret': code['payload']['secret'], 'name': 'Móvil'}) as answer:
            phone = await answer.json()
        first = await self.pair_local('first')
        ws = await self.call(first['token'])
        second = await self.pair_local('second')
        message = await ws.receive(timeout=10)
        self.assertEqual((message.type, ws.close_code), (aiohttp.WSMsgType.CLOSE, 4401), 'its call ends now')
        self.assertEqual((await self.via_socket('GET', '/api/device/devices', headers=auth(first['token'])))[0], 401)
        status, listing = await self.via_socket('GET', '/api/device/devices', headers=auth(second['token']))
        self.assertEqual(sorted((d['id'], d['kind']) for d in listing['devices']),
                         sorted([(phone['device_id'], 'code'), (second['device_id'], 'local')]),
                         'one local device; a code-paired one is untouched')
        await until(lambda: first['device_id'] not in self.app.state.devices.calls)

    async def test_what_only_the_socket_serves_does_not_exist_over_tcp(self):
        for method, path in LOCAL_PATHS:
            with self.subTest(path=path):
                status, body = await self.over_tcp(method, path, json={'name': 'x'} if method == 'POST' else None)
                self.assertEqual((status, json.loads(body)), (404, {'detail': 'Not Found'}), 'what an unknown route gets')
                # Not even with a device token, and not spelled another way.
                token = (await self.pair_local())['token']
                self.assertEqual((await self.over_tcp(method, path, headers=auth(token)))[0], 404)
        for path in ('/api//local/health', '/api/./local/health', '/api/device//local'):
            with self.subTest(path=path):
                self.assertEqual((await self.over_tcp('GET', path))[0], 404)
        self.assertEqual([d['kind'] for d in self.app.state.devices.store.registry.listing()], ['local'],
                         'nothing over TCP paired anything')
        # The link's socket over TCP is closed before its handshake.
        with self.assertRaises(aiohttp.WSServerHandshakeError):
            await self.tcp_http.ws_connect(self.tcp + '/api/connectors/link/?EIO=4&transport=websocket')

    async def test_a_page_reaching_the_socket_gets_none_of_it(self):
        # What the desktop app's proxy forwards always carries the page's Origin; native callers never send one.
        for method, path in LOCAL_PATHS:
            with self.subTest(path=path):
                status, _ = await self.via_socket(method, path, headers={'Origin': 'tauri://localhost'},
                                                  json={'name': 'x'} if method == 'POST' else None)
                self.assertEqual(status, 404)
        self.assertEqual(self.app.state.devices.store.registry.listing(), [], 'nothing was paired')
        with self.assertRaises(aiohttp.WSServerHandshakeError):
            await self.local_http.ws_connect('ws://localhost/api/connectors/link/?EIO=4&transport=websocket',
                                             headers={'Origin': 'tauri://localhost'})

    async def test_the_connector_links_through_the_socket_and_not_over_tcp(self):
        connector_id, token = self.node_room.journal.redeem_pairing_code(self.node_room.journal.create_pairing_code())
        credential = {'connector_id': connector_id, 'token': token, 'protocol': CONNECTOR_PROTOCOL, 'host': 'this-laptop'}
        over_tcp = socketio.AsyncClient(reconnection=False)
        with self.assertRaises(socketio.exceptions.ConnectionError):
            await over_tcp.connect(self.tcp, socketio_path='/api/connectors/link', namespaces=['/connectors'],
                                   transports=['websocket'], auth=credential)
        self.assertEqual(self.node_room.control.peers, {}, 'the credential reached nothing')
        connector = link_client(self, self.socket_path)
        await connector.connect(LOCAL, socketio_path='/api/connectors/link', namespaces=['/connectors'],
                                transports=['websocket'], auth=credential)
        await until(lambda: connector_id in self.node_room.control.peers)
        issued = await connector.call('device.pairing_code', {}, namespace='/connectors', timeout=10)
        self.assertTrue(issued['code'].startswith('SV1.'), 'minting codes is the socket\'s connector\'s')

    async def test_every_other_route_on_the_socket_still_wants_a_token(self):
        for method, path in (('GET', '/api/presentation/admission'), ('GET', '/api/device/devices'), ('GET', '/api/connectors'),
                             ('GET', '/api/models/catalog')):
            with self.subTest(path=path):
                self.assertEqual((await self.via_socket(method, path))[0], 401)
                self.assertEqual((await self.via_socket(method, path, headers=auth('guessed')))[0], 401)
        token = (await self.pair_local())['token']
        self.assertEqual((await self.via_socket('GET', '/api/presentation/admission', headers=auth(token)))[0], 200)
        self.assertEqual((await self.via_socket('GET', '/api/rendezvous'))[1]['api'], 1, 'and the open ones stay open')
        ws = await self.local_http.ws_connect('ws://localhost/api/presentation/ws', protocols=['sidevoice'])
        message = await ws.receive(timeout=10)
        self.assertEqual((message.type, ws.close_code), (aiohttp.WSMsgType.CLOSE, 4401))

    async def test_the_room_relays_none_of_it(self):
        await until(lambda: self.room.sid)
        paths = ['/api/device/local', '/api/device/local/pair', '/api/device/local/', '/api/local/health', '/api/local',
                 '/api/device/%6cocal/pair', '/api/device/local%2fpair', '/api/device//local/pair', '/api/device/./local',
                 '/api/device/%2e/local', '/api/device/%252e/local/pair', '/api/device/x/../local', '/api/device/pair/../local',
                 '/api/presentation/%2e%2e/local/health', '/api/device/%2E/local/pair']
        for path in paths:
            for method in ('GET', 'POST', 'DELETE'):
                with self.subTest(path=path, method=method):
                    answer = await self.room.ask('relay.http', {
                        'method': method, 'path': path, 'query': '', 'body': json.dumps({'name': 'relayed'}).encode(),
                        'headers': {'content-type': 'application/json', 'accept': 'application/json'}})
                    self.assertEqual((answer['status'], json.loads(answer['body'])), (404, {'detail': 'Not relayed.'}),
                                     'refused by name, before any request is made')
        self.assertEqual(self.app.state.devices.store.registry.devices, {}, 'nothing was paired through the room')
        await self.assert_the_link_is_not_relayed(self.room.ask)
        # What sits beside them under the relayed prefix still is relayed.
        self.assertTrue(relayable(self.tcp, '/api/device/pair'))
        self.assertTrue(relayable(self.tcp, '/api/device/localhost'))

    async def assert_the_link_is_not_relayed(self, ask):
        """The connector link, exactly, and spelled other ways: neither its HTTP transport nor its socket."""
        for path in ('/api/connectors/link', '/api/connectors/link/', '/api/device/%2e%2e/connectors/link/',
                     '/api/presentation/../connectors/link/'):
            for transport in ('polling', 'websocket'):
                with self.subTest(path=path, transport=transport):
                    answer = await ask('relay.http', {'method': 'GET', 'path': path, 'query': f'EIO=4&transport={transport}',
                                                      'headers': {'accept': '*/*'}, 'body': None})
                    self.assertEqual((answer['status'], json.loads(answer['body'])), (404, {'detail': 'Not relayed.'}))
            with self.subTest(path=path, socket=True):
                opened = await ask('relay.open', {'channel': f'link-{path}', 'path': path, 'query': 'EIO=4&transport=websocket',
                                                  'protocols': []})
                self.assertEqual((opened['ok'], opened['status']), (False, 404))
        self.assertEqual(self.node_room.control.peers, {}, 'no connector linked through the room')

    async def test_a_room_that_dials_in_relays_none_of_it_either(self):
        room, hello = await DialTests.dial(self, 'the-dial-key')
        self.addAsyncCleanup(room.disconnect)
        await asyncio.wait_for(hello, 5)
        await until(lambda: self.rendezvous.state['via'] == 'dial')

        async def ask(event, data):
            return await room.call(event, data, namespace='/room', timeout=10)
        await self.assert_the_link_is_not_relayed(ask)
        for path in ('/api/device/local/pair', '/api/local/health', '/api/device/%2e/local'):
            with self.subTest(path=path):
                answer = await ask('relay.http', {'method': 'POST', 'path': path, 'query': '', 'body': b'{}',
                                                  'headers': {'content-type': 'application/json'}})
                self.assertEqual(answer['status'], 404)
        self.assertEqual(self.app.state.devices.store.registry.devices, {})


class LocalSocketFileTests(unittest.TestCase):
    def test_a_socket_is_listening_from_its_binding_so_a_second_starter_cannot_take_it(self):
        """The review's sequence: two starters bind one path, one after the other, before either has served. The
        second meets a socket that answers, and leaves it alone."""
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / 'local.sock'
            first = LocalSocket(path)
            self.addCleanup(first.socket.close)
            with self.assertRaises(OSError) as taken:
                LocalSocket(path)
            self.assertEqual(taken.exception.errno, errno.EADDRINUSE)
            self.assertIn(str(path), str(taken.exception))
            self.assertEqual(os.stat(path).st_ino, first.inode, 'the first one\'s file, still')


class LocalOnlyRuleTests(unittest.TestCase):
    def test_the_rule_reads_the_path_however_it_is_spelled(self):
        for path in ('/api/local', '/api/local/health', '/api/device/local', '/api/device/local/pair', '/api//device/local',
                     '/api/./device/local/', '/api/connectors/link', '/api/connectors/link/'):
            self.assertTrue(local_only(path), path)
        for path in ('/api/device/pair', '/api/localhost', '/api/device/localx', '/api/connectors', '/api/presentation/ws'):
            self.assertFalse(local_only(path), path)


class DirectoryTests(unittest.TestCase):
    def test_only_a_directory_this_user_alone_may_enter_is_safe(self):
        with tempfile.TemporaryDirectory() as root:
            safe = Path(root) / 'core'
            safe.mkdir(mode=0o700)
            self.assertIsNone(unsafe_directory(safe))
            for mode in (0o750, 0o705, 0o770, 0o777, 0o701):
                with self.subTest(mode=oct(mode)):
                    safe.chmod(mode)
                    self.assertIn(f'{mode:04o}', unsafe_directory(safe))
            safe.chmod(0o700)
            link = Path(root) / 'link'
            link.symlink_to(safe)
            self.assertIsNotNone(unsafe_directory(link), 'a link to a directory is not the directory')
            (Path(root) / 'file').write_text('')
            self.assertIsNotNone(unsafe_directory(Path(root) / 'file'))
            self.assertIsNotNone(unsafe_directory(Path(root) / 'absent'))

    def test_another_user_s_directory_is_refused(self):
        """Every directory here is this user's; `/` (root's) stands for another user's where tests run unprivileged."""
        if os.getuid() == 0:
            self.skipTest('root owns /')
        self.assertIn('another user', unsafe_directory('/'))


if __name__ == '__main__':
    unittest.main()
