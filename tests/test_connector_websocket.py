import asyncio
import json
import socket
import tempfile
import time
import sys
from types import ModuleType
import unittest
from unittest.mock import patch
from pathlib import Path

import aiohttp
import socketio
import uvicorn
from fastapi import FastAPI

from sidevoice_core.control.connectors import ConnectorControl, InvalidConnectorAcknowledgement, PROTOCOL
from sidevoice_core.control.history import RoomHistory
from sidevoice_core.control.refusal import Refusal
from sidevoice_core.server.connector_link import mount_connector_socketio
from sidevoice_core.server.connector_websocket import (
    MAX_FRAME_BYTES, PATH, WireError, _decode, mount_connector_websocket,
    validate_delivery_acknowledgement,
)
from sidevoice_core.server.local import LocalListener, LocalOnly, LocalSocket
from test_connector_control import FakeHub

LOCAL = 'http://localhost'


async def until(check, timeout=10):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = check()
        if value:
            return value
        await asyncio.sleep(0.01)
    raise AssertionError('timed out waiting')


def through(path):
    return aiohttp.ClientSession(connector=aiohttp.UnixConnector(path=str(path)))


def free_port_socket():
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind(('127.0.0.1', 0))
    sock.listen()
    sock.setblocking(False)
    return sock


class ConnectorWebSocketTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.data = self.root / 'core'
        self.data.mkdir(mode=0o700)
        self.journal = RoomHistory(self.data / 'room.sqlite3')
        self.hub = FakeHub(self.journal)
        self.app = FastAPI()
        self.control = ConnectorControl(self.journal, self.hub, heartbeat_seconds=0.2, ack_timeout=1)
        mount_connector_socketio(self.app, self.control)
        mount_connector_websocket(self.app, self.control)
        self.app.state.devices = type('Devices', (), {'issue_code': lambda self: {'code': 'SV1.test'}})()
        self.app.add_middleware(LocalOnly)
        self.socket_path = self.data / 'local.sock'
        self.local = LocalSocket(self.socket_path)
        self.tcp_socket = free_port_socket()
        self.port = self.tcp_socket.getsockname()[1]
        self.node_server = uvicorn.Server(uvicorn.Config(self.app, log_level='error'))
        self.node_task = asyncio.create_task(self.node_server.serve(sockets=[self.tcp_socket]))
        await until(lambda: self.node_server.started)
        await self.control.start()
        self.local_server = LocalListener(self.app, self.node_server, log_level='error')
        self.local_task = asyncio.create_task(self.local_server.serve(sockets=[self.local.socket]))
        await until(lambda: self.local_server.started)
        self.local_http = through(self.socket_path)
        self.addAsyncCleanup(self.local_http.close)
        self.tcp_http = aiohttp.ClientSession()
        self.addAsyncCleanup(self.tcp_http.close)
        self.connector_id, self.token = self.journal.redeem_pairing_code(
            self.journal.create_pairing_code())
        self.websockets = []
        self.socketio_clients = []

    async def asyncTearDown(self):
        for ws in self.websockets:
            if not ws.closed:
                await ws.close()
        for client in self.socketio_clients:
            if client.connected:
                await client.disconnect()
        await self.control.stop()
        self.node_server.should_exit = True
        await self.node_task
        await self.local_task
        self.local.remove()

    async def connect_v3(self, *, token=None, autoping=True):
        ws = await self.local_http.ws_connect(f'{LOCAL}{PATH}', autoping=autoping)
        hello = {
            'jsonrpc': '2.0', 'id': 'c:hello', 'method': 'connector.hello',
            'params': {'protocol': 3, 'connector_id': self.connector_id, 'token': token or self.token,
                       'host': 'test-host', 'platform': 'linux x86_64', 'version': '0.1.0',
                       'harnesses': ['codex']},
        }
        await ws.send_json(hello)
        self.websockets.append(ws)
        reply = await self.receive(ws, request_id='c:hello')
        if reply.get('result') == {'protocol': 3}:
            welcome = await self.receive(ws, method='connector.welcome')
            self.assertEqual(welcome['params'], {'protocol': 3})
        return ws, reply

    async def receive(self, ws, *, method=None, request_id=None, timeout=5):
        deadline = asyncio.get_running_loop().time() + timeout
        while asyncio.get_running_loop().time() < deadline:
            frame = await ws.receive_json(timeout=max(0.01, deadline - asyncio.get_running_loop().time()))
            if ((method is None or frame.get('method') == method)
                    and (request_id is None or frame.get('id') == request_id)):
                return frame
        raise AssertionError(f'no matching JSON-RPC frame: method={method!r} id={request_id!r}')

    async def rpc(self, ws, request_id, method, params):
        await ws.send_json({'jsonrpc': '2.0', 'id': request_id, 'method': method, 'params': params})
        return await self.receive(ws, request_id=request_id)

    async def test_live_uds_hello_retries_malformed_acks_then_accepts_on_the_same_binding(self):
        ws, hello = await self.connect_v3(autoping=False)
        self.addAsyncCleanup(ws.close)
        self.assertEqual(hello['result'], {'protocol': 3})
        self.assertIn(self.connector_id, self.control.peers)
        identity, = self.journal.paired_connectors()
        self.assertEqual((identity['host'], identity['platform'], identity['version'], identity['harnesses']),
                         ('test-host', 'linux x86_64', '0.1.0', ['codex']))

        pong = asyncio.create_task(ws.receive(timeout=3))
        await ws.ping(b'v3-probe')
        pong_message = await pong
        self.assertEqual((pong_message.type, pong_message.data), (aiohttp.WSMsgType.PONG, b'v3-probe'))

        registered = await self.rpc(ws, 'c:1', 'binding.register', {
            'client_ref': 'session-v3', 'harness': 'codex', 'thread': 'session-v3'})
        binding = registered['result']['binding_id']
        row = self.journal.put(id='call:user-turn:v3', thread='session-v3', role='user', text='hello',
                               name='You', session='call', revision=1, status='pending',
                               payload={'thread_id': 'session-v3', 'text': 'hello', 'message_id': 'v3',
                                        'session_id': 'call', 'revision': 1})
        invalid_acks = (
            {'result': None}, {'result': {}}, {'result': []}, {'result': 1},
            {'result': {'status': 'invalid'}}, {'error': {'code': -32000, 'message': 'refused'}},
            {'error': None},
        )
        for attempt, acknowledgement in enumerate(invalid_acks, start=1):
            delivery = await self.receive(ws, method='input.deliver')
            self.assertEqual(delivery['params']['binding_id'], binding)
            self.assertEqual(delivery['params']['event_id'], row['id'])
            await ws.send_json({'jsonrpc': '2.0', 'id': delivery['id'], **acknowledgement})
            await until(lambda: not self.control.inflight and self.journal.get(row['id'])['status'] == 'pending')
            self.assertEqual(self.journal.get(row['id'])['attempts'], attempt)
            if attempt < len(invalid_acks):
                await self.control.tick(now=self.journal.get(row['id'])['next_attempt'] + 1)

        await self.control.tick(now=self.journal.get(row['id'])['next_attempt'] + 1)
        retry = await self.receive(ws, method='input.deliver')
        self.assertEqual(retry['params']['binding_id'], binding, 'the same binding recovers')
        await ws.send_json({'jsonrpc': '2.0', 'id': retry['id'], 'result': {'status': 'accepted'}})
        await until(lambda: self.journal.get(row['id'])['status'] == 'delivered')
        self.assertEqual(self.control.inflight, {})

    async def test_timeout_late_ack_is_ignored_and_the_live_link_retries(self):
        ws, _ = await self.connect_v3()
        self.addAsyncCleanup(ws.close)
        self.control.ack_timeout = 0.05
        registered = await self.rpc(ws, 'c:1', 'binding.register', {
            'client_ref': 'timeout-v3', 'harness': 'codex', 'thread': 'timeout-v3'})
        binding_id = registered['result']['binding_id']
        row = self.journal.put(id='call:user-turn:timeout', thread='timeout-v3', role='user', text='hello',
                               name='You', session='call', revision=1, status='pending',
                               payload={'thread_id': 'timeout-v3', 'text': 'hello', 'message_id': 'timeout',
                                        'session_id': 'call', 'revision': 1})
        timed_out = await self.receive(ws, method='input.deliver')
        await until(lambda: not self.control.inflight and self.journal.get(row['id'])['status'] == 'pending')
        await ws.send_json({'jsonrpc': '2.0', 'id': timed_out['id'], 'result': {'status': 'accepted'}})
        await asyncio.sleep(0.05)
        self.assertIn(self.connector_id, self.control.peers)
        self.assertEqual(self.journal.get(row['id'])['status'], 'pending', 'the late ACK cannot settle the retry')

        await self.control.tick(now=self.journal.get(row['id'])['next_attempt'] + 1)
        retry = await self.receive(ws, method='input.deliver')
        self.assertEqual(retry['params']['binding_id'], binding_id)
        await ws.send_json({'jsonrpc': '2.0', 'id': retry['id'], 'result': {'status': 'accepted'}})
        await until(lambda: self.journal.get(row['id'])['status'] == 'delivered')

    async def test_only_the_unoriginated_uds_can_open_the_v3_route(self):
        accepted, reply = await self.connect_v3()
        self.addAsyncCleanup(accepted.close)
        self.assertEqual(reply['result'], {'protocol': 3})
        with self.assertRaises(aiohttp.WSServerHandshakeError):
            await self.tcp_http.ws_connect(f'http://127.0.0.1:{self.port}{PATH}')
        with self.assertRaises(aiohttp.WSServerHandshakeError):
            await self.local_http.ws_connect(f'{LOCAL}{PATH}', headers={'Origin': 'tauri://localhost'})
        self.assertEqual(len(self.control.peers), 1)

    async def test_existing_socketio_v2_peer_still_connects_with_protocol_two(self):
        session = through(self.socket_path)
        client = socketio.AsyncClient(reconnection=False, http_session=session)
        self.socketio_clients.append(client)
        self.addAsyncCleanup(session.close)
        self.addAsyncCleanup(client.disconnect)
        welcome = asyncio.get_running_loop().create_future()
        client.on('connector.welcome', lambda data: welcome.set_result(data) if not welcome.done() else None,
                  namespace='/connectors')
        await client.connect(LOCAL, namespaces=['/connectors'], transports=['websocket'],
                             socketio_path='/api/connectors/link',
                             auth={'connector_id': self.connector_id, 'token': self.token,
                                   'protocol': PROTOCOL, 'host': 'v2-host'})
        self.assertEqual(await asyncio.wait_for(welcome, 3), {'protocol': PROTOCOL})
        self.assertIn(self.connector_id, self.control.peers)

    async def test_authentication_and_protocol_refusals_do_not_attach_a_peer(self):
        ws, reply = await self.connect_v3(token='wrong-secret')
        self.addAsyncCleanup(ws.close)
        self.assertEqual(reply['error']['code'], -32001)
        self.assertEqual(self.control.peers, {})
        ws = await self.local_http.ws_connect(f'{LOCAL}{PATH}')
        await ws.send_json({'jsonrpc': '2.0', 'id': 'c:old', 'method': 'connector.hello',
                            'params': {'protocol': 2, 'connector_id': self.connector_id, 'token': self.token}})
        old = await self.receive(ws, request_id='c:old')
        self.assertEqual(old['error']['code'], -32002)
        self.assertEqual(self.control.peers, {})
        await ws.close()

    async def test_core_originated_r2_requests_are_correlated_on_the_same_socket(self):
        ws, _ = await self.connect_v3()
        self.addAsyncCleanup(ws.close)
        peer = self.control.peers[self.connector_id]
        for event, params, result in (
            ('pair.request', {'room': 'https://room.example', 'code': 'pair-code'}, {'ok': True}),
            ('agents.list', {'rescan': True}, {'agents': [], 'custom': {}, 'scanned_at': None}),
            ('agents.connect', {'id': 'codex'}, {'agents': [], 'custom': {}, 'scanned_at': None}),
            ('agents.disconnect', {'id': 'codex'}, {'agents': [], 'custom': {}, 'scanned_at': None}),
            ('agents.dismiss', {'id': 'codex'}, {'agents': [], 'custom': {}, 'scanned_at': None}),
        ):
            pending = asyncio.create_task(peer.request(event, params, timeout=3))
            request = await self.receive(ws, method=event)
            self.assertEqual(request['params'], params)
            response = {'jsonrpc': '2.0', 'id': request['id'], 'result': result}
            await ws.send_json(response)
            if event == 'pair.request':
                await ws.send_json(response)  # a duplicate response must not finish or break the next call
            self.assertEqual(await pending, result)
        await peer.send('connector.error', {'key': 'proof-only', 'params': {'harness': 'codex'}})
        error = await self.receive(ws, method='connector.error')
        self.assertEqual(error['params']['key'], 'proof-only')
        await self.control.rendezvous_changed({'connected': True})
        rendezvous = await self.receive(ws, method='node.rendezvous')
        self.assertEqual(rendezvous['params'], {'connected': True})
        registered = await self.rpc(ws, 'c:1', 'binding.register', {
            'client_ref': 'close-v3', 'harness': 'codex', 'thread': 'close-v3'})
        record = self.journal.binding(registered['result']['binding_id'])
        await self.control.close_binding(record)
        closed = await self.receive(ws, method='binding.close')
        self.assertEqual(closed['params']['binding_id'], record['id'])

    async def test_connector_event_map_updates_bindings_speech_observations_and_pairing(self):
        ws, _ = await self.connect_v3()
        self.addAsyncCleanup(ws.close)
        registered = await self.rpc(ws, 'c:1', 'binding.register', {
            'client_ref': 'events-v3', 'harness': 'codex', 'thread': 'events-v3'})
        binding_id = registered['result']['binding_id']
        await ws.send_json({'jsonrpc': '2.0', 'method': 'input.working',
                            'params': {'binding_id': binding_id, 'working': True}})
        await until(lambda: self.hub.working == [('events-v3', True, {})])
        await ws.send_json({'jsonrpc': '2.0', 'method': 'input.engine',
                            'params': {'binding_id': binding_id, 'engine': {'model': 'codex-test'}}})
        await until(lambda: self.journal.binding(binding_id)['engine'] == {'model': 'codex-test'})
        user_row = self.journal.put(
            id='call:user-text:read-v3', thread='events-v3', role='user', text='heard', name='You',
            session='call', revision=1, status='unconfirmed',
            payload={'thread_id': 'events-v3', 'text': 'heard', 'message_id': 'read-v3',
                     'session_id': 'call', 'revision': 1})
        await ws.send_json({'jsonrpc': '2.0', 'method': 'input.read',
                            'params': {'binding_id': binding_id, 'message_id': 'read-v3'}})
        await until(lambda: self.journal.get(user_row['id'])['status'] == 'read')
        code = await self.rpc(ws, 'c:2', 'device.pairing_code', {})
        self.assertEqual(code['result']['code'], 'SV1.test')
        await ws.send_json({'jsonrpc': '2.0', 'method': 'binding.unregister',
                            'params': {'binding_id': binding_id}})
        await until(lambda: not self.control.is_live(binding_id))

    async def test_speech_refusals_have_structured_v3_outcomes_and_exact_ids(self):
        ws, _ = await self.connect_v3()
        self.addAsyncCleanup(ws.close)
        registered = await self.rpc(ws, 'c:1', 'binding.register', {
            'client_ref': 'refusal-v3', 'harness': 'codex', 'thread': 'refusal-v3'})
        binding_id = registered['result']['binding_id']

        room_module = ModuleType('sidevoice_core.control.room')

        class Speech:
            def __init__(self, *, thread_id, session_id, revision, text, utterance_id, language=None):
                self.thread_id, self.session_id, self.revision = thread_id, session_id, revision
                self.text, self.utterance_id, self.language = text, utterance_id, language

        room_module.Speech = Speech
        message = {'event_id': 'speech-event-refusal', 'utterance_id': 'utterance-refusal',
                   'binding_id': binding_id, 'session_id': 'call', 'revision': 1,
                   'text': 'reply', 'language': 'en'}

        async def refused_before_intake(_speech):
            raise Refusal(422, 'invalid reply')

        async def refused_after_intake(_speech):
            error = Refusal(409, 'stale audio turn')
            error.text_saved = True
            raise error

        with patch.dict(sys.modules, {'sidevoice_core.control.room': room_module}):
            self.hub.publish = refused_before_intake
            terminal = await self.rpc(ws, 'c:2', 'speech.publish', message)
            self.assertEqual(terminal['result'], {
                'event_id': 'speech-event-refusal', 'utterance_id': 'utterance-refusal',
                'status': 'rejected', 'error': 'invalid reply', 'terminal': True,
                'reason_code': 'application_refusal'})

            self.hub.publish = refused_after_intake
            retained = await self.rpc(ws, 'c:3', 'speech.publish', message)
            self.assertEqual(retained['result']['status'], 'rejected')
            self.assertNotIn('terminal', retained['result'])
            self.assertEqual(retained['result']['event_id'], message['event_id'])
            self.assertEqual(retained['result']['utterance_id'], message['utterance_id'])

            unknown = {**message, 'binding_id': 'binding-from-another-core'}
            replay = await self.rpc(ws, 'c:4', 'speech.publish', unknown)
            self.assertEqual(replay['result'], {
                'event_id': 'speech-event-refusal', 'utterance_id': 'utterance-refusal',
                'status': 'unknown_binding'})

    async def test_speech_storage_failure_returns_no_success_and_same_id_replay_saves_once(self):
        ws, _ = await self.connect_v3()
        self.addAsyncCleanup(ws.close)
        registered = await self.rpc(ws, 'c:1', 'binding.register', {
            'client_ref': 'speech-v3', 'harness': 'codex', 'thread': 'speech-v3'})
        binding_id = registered['result']['binding_id']

        room_module = ModuleType('sidevoice_core.control.room')

        class Speech:
            def __init__(self, *, thread_id, session_id, revision, text, utterance_id, language=None):
                self.thread_id, self.session_id, self.revision = thread_id, session_id, revision
                self.text, self.utterance_id, self.language = text, utterance_id, language

        room_module.Speech = Speech
        original_put = self.journal.put
        writes = 0

        def put_once(**values):
            nonlocal writes
            writes += 1
            if writes == 1:
                raise OSError('storage unavailable')
            return original_put(**values)

        async def publish(speech):
            record = self.journal.put(
                id=f'{speech.session_id}:voice:{speech.utterance_id}', thread=speech.thread_id,
                role='assistant', text=speech.text, name='Conversation', session=speech.session_id,
                revision=speech.revision, status='text_only', language=speech.language)
            return {'status': record['status'], 'text_saved': True,
                    'utterance_id': speech.utterance_id}

        self.hub.publish = publish
        message = {'event_id': 'speech-event-1', 'utterance_id': 'utterance-stable',
                   'binding_id': binding_id, 'session_id': 'call', 'revision': 1,
                   'text': 'A stored reply'}
        with patch.dict(sys.modules, {'sidevoice_core.control.room': room_module}), \
                patch.object(self.journal, 'put', side_effect=put_once):
            failed = await self.rpc(ws, 'c:2', 'speech.publish', message)
            self.assertIn('error', failed, 'a storage exception is a JSON-RPC error, never a terminal ACK')
            accepted = await self.rpc(ws, 'c:3', 'speech.publish', message)
            self.assertEqual(accepted['result'], {'status': 'text_only', 'text_saved': True,
                                                   'utterance_id': 'utterance-stable',
                                                   'event_id': 'speech-event-1'})
            replay = await self.rpc(ws, 'c:4', 'speech.publish', message)
            self.assertTrue(replay['result']['text_saved'])
        self.assertEqual(writes, 3)
        saved = [row for row in self.journal.messages.values() if row['id'].endswith(':voice:utterance-stable')]
        self.assertEqual(len(saved), 1)

    async def test_reconnect_replaces_the_previous_peer_for_the_same_connector(self):
        first, _ = await self.connect_v3()
        first_peer = self.control.peers[self.connector_id]
        second, _ = await self.connect_v3()
        second_peer = self.control.peers[self.connector_id]
        self.addAsyncCleanup(second.close)
        closed = await first.receive(timeout=3)
        self.assertEqual(closed.type, aiohttp.WSMsgType.CLOSE)
        self.assertTrue(first_peer.closed)
        self.assertIs(self.control.peers[self.connector_id], second_peer)


class DeliveryOutcomeTests(unittest.TestCase):
    def test_only_bounded_known_outcomes_can_settle_input(self):
        for result in (None, {}, [], 1, {'status': []}, {'status': {}}, {'status': 'other'},
                       {'status': 'accepted', 'detail': 1},
                       {'status': 'accepted', 'error': 'x' * 1001},
                       {'status': 'accepted', 'detail': 'x' * 4097}):
            with self.subTest(result=result if not isinstance(result, dict) else list(result)):
                with self.assertRaises(InvalidConnectorAcknowledgement):
                    validate_delivery_acknowledgement(result)
        self.assertEqual(validate_delivery_acknowledgement({'status': 'failed', 'error': 'retry'}),
                         {'status': 'failed', 'error': 'retry'})

    def test_one_json_rpc_object_cannot_exceed_the_websocket_frame_limit(self):
        with self.assertRaises(WireError):
            _decode(' ' * (MAX_FRAME_BYTES + 1))
        with self.assertRaises(WireError):
            _decode('{"jsonrpc":"2.0","method":"test","params":{"n":NaN}}')


if __name__ == '__main__':
    unittest.main()
