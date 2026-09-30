"""The link itself: a real `python-socketio` client against the room's real namespace.

`test_connector_control` says what each event means with a peer that carries nothing. This one
says that the events arrive at all — over `/api/connectors/link`, through uvicorn, with the
handshake that authenticates and the acknowledgements the library gives us. What it must never do
is re-test the control plane's decisions: it checks that the wire reaches them.
"""
import asyncio
import gc
import socket
import tempfile
import unittest
from pathlib import Path

import socketio
import uvicorn
from fastapi import FastAPI

from sidevoice.connector_control import PROTOCOL, mount_connector_control
from sidevoice.connector_socketio import NAMESPACE, PATH
from sidevoice.room_history import RoomHistory
from test_connector_control import FakeHub


async def until(check, timeout=10.0, every=.002):
    deadline = asyncio.get_running_loop().time() + timeout
    while asyncio.get_running_loop().time() < deadline:
        if check():
            return True
        await asyncio.sleep(every)
    raise AssertionError('timed out waiting')


def free_port():
    with socket.socket() as probe:
        probe.bind(('127.0.0.1', 0))
        return probe.getsockname()[1]


class LinkTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.journal = RoomHistory(Path(self.temp.name) / 'room.sqlite3')
        self.hub = FakeHub(self.journal)
        app = FastAPI()
        # A keepalive budget shorter than the suite, so a room that stops answering is noticed here.
        self.control = mount_connector_control(app, self.hub, heartbeat_seconds=0.2, ack_timeout=1)
        self.connector_id, self.token = self.journal.redeem_pairing_code(self.journal.create_pairing_code())
        self.port = free_port()
        self.server = uvicorn.Server(uvicorn.Config(app, host='127.0.0.1', port=self.port, log_level='error'))
        self.serving = asyncio.create_task(self.server.serve())
        await until(lambda: self.server.started, timeout=20)

    async def asyncTearDown(self):
        self.server.should_exit = True
        await asyncio.wait_for(self.serving, timeout=20)

    def credential(self, **overrides):
        return {'connector_id': self.connector_id, 'token': self.token, 'protocol': PROTOCOL,
                'host': 'a-laptop', 'version': '0.4.3', **overrides}

    async def connect(self, **overrides):
        """One connector, connected. `refused` collects what the room said when it would not have
        it: this client reports only that a namespace failed, so the reason is read where it
        arrives — which is also where the JavaScript client puts it, in `connect_error`."""
        client = socketio.AsyncClient(reconnection=False)
        self.addAsyncCleanup(client.disconnect)
        client.refused = []
        welcomed = asyncio.get_running_loop().create_future()
        client.on('connector.welcome', lambda data: welcomed.done() or welcomed.set_result(data),
                  namespace=NAMESPACE)
        client.on('connect_error', lambda data: client.refused.append(data), namespace=NAMESPACE)
        try:
            await client.connect(f'http://127.0.0.1:{self.port}', socketio_path=PATH, namespaces=[NAMESPACE],
                                 transports=['websocket'], auth=self.credential(**overrides))
        except socketio.exceptions.ConnectionError as error:
            error.refused = client.refused
            raise
        return client, await asyncio.wait_for(welcomed, timeout=10)

    @staticmethod
    def reason(error):
        return ' '.join(str(said.get('message', said)) for said in getattr(error, 'refused', []))

    async def test_the_handshake_authenticates_and_the_room_welcomes_the_protocol_it_speaks(self):
        client, welcome = await self.connect()
        self.assertEqual(welcome, {'protocol': PROTOCOL})
        self.assertIn(self.connector_id, self.control.peers)

    async def test_a_credential_the_room_does_not_know_is_refused_with_what_to_do_about_it(self):
        with self.assertRaises(socketio.exceptions.ConnectionError) as refusal:
            await self.connect(token='not-the-token')
        self.assertIn('Emparejar máquina', self.reason(refusal.exception),
                      'the message says where the code comes from, and reaches the client')
        self.assertEqual(self.control.peers, {}, 'a refused credential never reaches an event')

    async def test_a_pairing_taken_away_from_the_room_is_refused_saying_so(self):
        # Not "we do not know you": the person revoked it on purpose, and the machine's conversations
        # must be able to say which of the two happened.
        client, _ = await self.connect()
        await self.control.revoke(self.connector_id)
        await until(lambda: not client.connected)
        with self.assertRaises(socketio.exceptions.ConnectionError) as refusal:
            await self.connect()
        self.assertIn('revoked', self.reason(refusal.exception))
        self.assertIn('Emparejar máquina', self.reason(refusal.exception), 'and says how to come back')
        self.assertEqual(self.control.peers, {})

    async def test_the_handshake_carries_what_the_machine_says_about_itself(self):
        await self.connect(host='macbook-pro', platform='darwin arm64', version='0.5.0',
                           harnesses=['claude', 'codex'])
        row, = self.journal.paired_connectors()
        self.assertEqual((row['host'], row['platform'], row['version'], row['harnesses']),
                         ('macbook-pro', 'darwin arm64', '0.5.0', ['claude', 'codex']))

    async def test_a_connector_from_before_this_version_is_told_to_pair_again(self):
        with self.assertRaises(socketio.exceptions.ConnectionError) as refusal:
            await self.connect(protocol=PROTOCOL - 1)
        self.assertIn(str(PROTOCOL), self.reason(refusal.exception))
        self.assertIn('pair this machine again', self.reason(refusal.exception).lower())
        self.assertEqual(self.control.peers, {})

    async def test_every_event_a_connector_sends_reaches_the_control_plane(self):
        client, _ = await self.connect()
        registered = await client.call('binding.register', {'client_ref': 'sess-1', 'harness': 'claude',
                                                            'thread': 'sess-1', 'title': 'Trabajo'},
                                       namespace=NAMESPACE, timeout=10)
        self.assertEqual(registered['thread'], 'sess-1')
        self.assertTrue(self.control.is_live(registered['binding_id']))

        # A registration the room refuses is the acknowledgement saying so, not a dropped event.
        refused = await client.call('binding.register', {'client_ref': 'bad', 'harness': 'claude',
                                                         'thread': 'not a thread!'}, namespace=NAMESPACE, timeout=10)
        self.assertIn('error', refused)

        published = await client.call('speech.publish', {'event_id': 'e1', 'binding_id': registered['binding_id'],
                                                         'session_id': 'call', 'revision': 2, 'text': 'Hola'},
                                      namespace=NAMESPACE, timeout=10)
        self.assertEqual(published['status'], 'queued')
        self.assertEqual([speech.text for speech in self.hub.published], ['Hola'])

        await client.emit('input.working', {'binding_id': registered['binding_id'], 'working': True}, namespace=NAMESPACE)
        await until(lambda: self.hub.working == [('sess-1', True, {})])

        row = self.journal.put(id='call:user-turn:m1', thread='sess-1', role='user', text='hola', name='Tú',
                               session='call', revision=1, status='unconfirmed',
                               payload={'thread_id': 'sess-1', 'text': 'hola', 'message_id': 'm1',
                                        'session_id': 'call', 'revision': 1})
        await client.emit('input.read', {'binding_id': registered['binding_id'], 'message_id': 'm1',
                                         'session_id': 'call', 'revision': 1}, namespace=NAMESPACE)
        await until(lambda: self.journal.get(row['id'])['status'] == 'read')

        await client.emit('binding.unregister', {'binding_id': registered['binding_id']}, namespace=NAMESPACE)
        await until(lambda: not self.control.is_live(registered['binding_id']))

    async def test_a_delivery_is_answered_by_the_connectors_acknowledgement(self):
        client, _ = await self.connect()
        registered = await client.call('binding.register', {'client_ref': 'sess-1', 'harness': 'claude',
                                                            'thread': 'sess-1'}, namespace=NAMESPACE, timeout=10)
        delivered = []

        @client.on('input.deliver', namespace=NAMESPACE)
        async def deliver(data):
            delivered.append(data)
            return {'status': 'accepted'}

        row = self.journal.put(id='call:user-turn:m1', thread='sess-1', role='user', text='hola', name='Tú',
                               session='call', revision=1, status='pending',
                               payload={'thread_id': 'sess-1', 'text': 'hola', 'message_id': 'm1',
                                        'session_id': 'call', 'revision': 1})
        await until(lambda: self.journal.get(row['id'])['status'] == 'delivered')
        self.assertEqual([(event['text'], event['binding_id']) for event in delivered],
                         [('hola', registered['binding_id'])])

    async def test_a_delivery_nobody_answers_is_given_back_on_the_rooms_own_clock(self):
        client, _ = await self.connect()
        await client.call('binding.register', {'client_ref': 'sess-1', 'harness': 'claude', 'thread': 'sess-1'},
                          namespace=NAMESPACE, timeout=10)

        @client.on('input.deliver', namespace=NAMESPACE)
        async def deliver(data):
            await asyncio.sleep(30)   # a harness that never comes back

        row = self.journal.put(id='call:user-turn:m1', thread='sess-1', role='user', text='hola', name='Tú',
                               session='call', revision=1, status='pending',
                               payload={'thread_id': 'sess-1', 'text': 'hola', 'message_id': 'm1',
                                        'session_id': 'call', 'revision': 1})
        await until(lambda: self.journal.get(row['id'])['status'] == 'sending')
        await until(lambda: self.journal.get(row['id'])['status'] == 'pending', timeout=15)
        self.assertEqual(self.journal.get(row['id'])['attempts'], 1, 'backed off, not hammered')
        self.assertEqual(self.control.inflight, {})

    async def test_the_room_closing_a_binding_reaches_the_connector(self):
        client, _ = await self.connect()
        registered = await client.call('binding.register', {'client_ref': 'sess-1', 'harness': 'claude',
                                                            'thread': 'sess-1'}, namespace=NAMESPACE, timeout=10)
        closed = []
        client.on('binding.close', lambda data: closed.append(data), namespace=NAMESPACE)
        await self.control.close_binding(self.journal.binding(registered['binding_id']))
        await until(lambda: closed)
        self.assertEqual(closed[0]['reason'], 'closed_from_room')

    async def test_a_newer_connection_from_the_same_connector_takes_the_older_ones_place(self):
        first, _ = await self.connect()
        second, _ = await self.connect()
        await until(lambda: not first.connected)
        self.assertTrue(second.connected)
        self.assertIn(self.connector_id, self.control.peers, 'the winner keeps the room')
        registered = await second.call('binding.register', {'client_ref': 'sess-1', 'harness': 'claude',
                                                            'thread': 'sess-1'}, namespace=NAMESPACE, timeout=10)
        self.assertTrue(self.control.is_live(registered['binding_id']))

    async def test_a_connection_that_goes_frees_its_bindings(self):
        client, _ = await self.connect()
        registered = await client.call('binding.register', {'client_ref': 'sess-1', 'harness': 'claude',
                                                            'thread': 'sess-1'}, namespace=NAMESPACE, timeout=10)
        await client.disconnect()
        await until(lambda: not self.control.is_live(registered['binding_id']))
        self.assertEqual(self.control.peers, {})


def tearDownModule():
    """Give back what this module borrowed, here, where nothing is being timed.

    Serving a room and opening sockets leaves thousands of objects that only the cyclic collector
    can reclaim. Left for the interpreter to schedule, that cost lands wherever the threshold
    happens to fall — which is somebody else's test (#69). Reclaiming it at this boundary spends
    it on the tests that made the garbage.
    """
    gc.collect()
