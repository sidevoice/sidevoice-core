"""This node's side of the rendezvous, against a stand-in room: the node dials out with the machine's
pairing, serves the room's relayed requests by making them to itself, carries a call socket both ways,
reports the link to its connector, and accepts a room that dials it only with the right key."""
import asyncio
import json
import socket
import tempfile
import time
import unittest
from pathlib import Path

import socketio
import uvicorn

from sidevoice_core.control.connectors import PROTOCOL as CONNECTOR_PROTOCOL
from sidevoice_core.control.history import RoomHistory
from sidevoice_core.control.room import Room
from sidevoice_core.server.app import create_app
from sidevoice_core.server.rendezvous import PROTOCOL, Rendezvous


def free_port():
    with socket.socket() as probe:
        probe.bind(('127.0.0.1', 0))
        return probe.getsockname()[1]


async def until(check, timeout=10.0, every=0.02):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = check()
        if value:
            return value
        await asyncio.sleep(every)
    raise AssertionError('timed out waiting')


class StandInRoom:
    """The hosted room's link, as much of it as a node sees: `/api/connectors/link`, namespace `/nodes`."""

    def __init__(self, refuse=None, public_url=None):
        self.server = socketio.AsyncServer(async_mode='asgi', namespaces=['/nodes'])
        self.auth, self.sid, self.frames, self.closed = None, None, [], []
        self.refuse, self.connections, self.public_url = refuse, 0, public_url
        s = self.server

        @s.event(namespace='/nodes')
        async def connect(sid, environ, auth):
            self.auth = auth
            self.connections += 1
            if self.refuse:
                raise socketio.exceptions.ConnectionRefusedError(self.refuse)
            self.sid = sid
            welcome = {'protocol': PROTOCOL, **({'public_url': self.public_url} if self.public_url else {})}
            await s.emit('node.welcome', welcome, to=sid, namespace='/nodes')

        @s.on('relay.data', namespace='/nodes')
        async def data(sid, payload):
            self.frames.append(payload)

        @s.on('relay.close', namespace='/nodes')
        async def close(sid, payload):
            self.closed.append(payload)

    async def ask(self, event, data):
        return await self.server.call(event, data, to=self.sid, namespace='/nodes', timeout=10)

    async def tell(self, event, data):
        await self.server.emit(event, data, to=self.sid, namespace='/nodes')


class NodeTest(unittest.IsolatedAsyncioTestCase):
    room_refuses = None
    room_public_url = None
    device_auth = False   # what the relay carries is the subject here; device tokens are test_device_pairing's

    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory()
        root = Path(self.temp.name)
        self.room_port, self.node_port = free_port(), free_port()
        self.room = StandInRoom(self.room_refuses, self.room_public_url)
        self.room_server = uvicorn.Server(uvicorn.Config(socketio.ASGIApp(self.room.server, socketio_path='/api/connectors/link'),
                                                         host='127.0.0.1', port=self.room_port, log_level='warning'))
        self.room_task = asyncio.create_task(self.room_server.serve())
        self.pairing = root / 'credentials.json'
        self.pairing.write_text(json.dumps({'url': f'http://127.0.0.1:{self.room_port}', 'connector_id': 'machine-1',
                                            'token': 'secret-token', 'protocol': 3, 'dial_key': 'the-dial-key'}))
        self.node_room = Room(RoomHistory(root / 'core' / 'room-state.json'))
        self.states = []
        self.rendezvous = Rendezvous(self.pairing, f'http://127.0.0.1:{self.node_port}', poll=0.1)
        self.app = app = create_app(self.node_room, config={'VOICE_BROWSER_HEARTBEAT_SECONDS': '0', 'SIDEVOICE_CORE_DATA_DIR': str(root / 'core')},
                                    rendezvous=self.rendezvous, device_auth=self.device_auth)
        app.state.devices.listen_url = f'http://127.0.0.1:{self.node_port}'
        original = self.rendezvous.on_state

        async def recorded(state):
            self.states.append(state)
            await original(state)
        self.rendezvous.on_state = recorded
        self.node_server = uvicorn.Server(uvicorn.Config(app, host='127.0.0.1', port=self.node_port, log_level='warning'))
        self.node_task = asyncio.create_task(self.node_server.serve())
        await until(lambda: self.room_server.started and self.node_server.started)
        await self.rendezvous.start()

    async def asyncTearDown(self):
        self.node_server.should_exit = True
        await self.node_task
        self.room_server.should_exit = True
        await self.room_task
        self.temp.cleanup()


class OutboundTests(NodeTest):
    async def test_the_node_dials_the_paired_room_and_serves_its_relayed_requests(self):
        await until(lambda: self.room.sid)
        self.assertEqual((self.room.auth['connector_id'], self.room.auth['token'], self.room.auth['protocol']),
                         ('machine-1', 'secret-token', PROTOCOL))
        self.assertIn('core', self.room.auth, 'the node says which core it runs')
        await until(lambda: self.rendezvous.state['connected'])
        self.assertEqual(self.rendezvous.state['via'], 'outbound')
        # A relayed request is the node's own route, answered by the node's own code.
        answer = await self.room.ask('relay.http', {'method': 'GET', 'path': '/api/presentation/admission',
                                                    'query': '', 'headers': {'accept': 'application/json'}, 'body': None})
        self.assertEqual(answer['status'], 200)
        self.assertEqual(json.loads(answer['body']), self.node_room.admission())
        refused = await self.room.ask('relay.http', {'method': 'GET', 'path': '/api/connectors', 'query': '',
                                                     'headers': {}, 'body': None})
        self.assertEqual(refused['status'], 404, 'only the client surface is relayed, never the connector\'s')
        # Changing a key insists on an Origin: a relayed page's is carried as this node's own.
        form = await self.room.ask('relay.http', {'method': 'DELETE', 'path': '/api/presentation/integrations/openai',
                                                  'query': '', 'headers': {'origin': 'https://room.example'}, 'body': None})
        self.assertEqual(form['status'], 200, form['body'])

    async def test_a_relayed_call_socket_is_a_call_like_any_other(self):
        await until(lambda: self.room.sid)
        opened = await self.room.ask('relay.open', {'channel': 'c-1', 'path': '/api/presentation/ws', 'query': ''})
        self.assertEqual(opened, {'ok': True})
        await self.room.tell('relay.data', {'channel': 'c-1', 'data': json.dumps(
            {'label': 'rtvi-ai', 'type': 'client-ready', 'id': 'x', 'data': {'settings': {'turn_end_mode': 'timer'}}})})
        session = await until(lambda: next((json.loads(f['data'])['data'] for f in self.room.frames
                                            if isinstance(f['data'], str) and json.loads(f['data']).get('type') == 'voice-session'), None), timeout=30)
        self.assertIn(session['session_id'], self.node_room.clients, 'the relayed browser joined the node\'s room')
        await self.room.tell('relay.data', {'channel': 'c-1', 'data': b'\x00\x00' * 320})   # microphone PCM, binary
        await self.room.tell('relay.close', {'channel': 'c-1', 'code': 1000})
        await until(lambda: not self.node_room.clients, timeout=15)

    async def test_the_connector_hears_whether_the_room_is_reachable(self):
        # A connector linked to this core: what it is told about the room arrives as `node.rendezvous`.
        credential = self.node_room.journal.redeem_pairing_code(self.node_room.journal.create_pairing_code())
        heard = []
        connector = socketio.AsyncClient(reconnection=False)
        connector.on('node.rendezvous', lambda data: heard.append(data), namespace='/connectors')
        await connector.connect(f'http://127.0.0.1:{self.node_port}', socketio_path='/api/connectors/link',
                                namespaces=['/connectors'], transports=['websocket'],
                                auth={'connector_id': credential[0], 'token': credential[1], 'protocol': CONNECTOR_PROTOCOL,
                                      'host': 'this-laptop', 'platform': 'Linux x86_64', 'version': '0.7.0'})
        try:
            await until(lambda: any(state['connected'] for state in heard))
            self.assertEqual(heard[-1]['room'], f'http://127.0.0.1:{self.room_port}')
            # The room revokes the machine: the connector hears the refusal, which is what lets its conversations go.
            await self.room.tell('node.revoked', {'reason': 'revoked from the page'})
            await until(lambda: heard and heard[-1].get('refused') == 'revoked from the page')
            self.assertFalse(heard[-1]['connected'])
            # And it says so on /api/rendezvous too, with the machine as its connector described it.
            import aiohttp
            async with aiohttp.ClientSession() as http:
                async with http.get(f'http://127.0.0.1:{self.node_port}/api/rendezvous') as answer:
                    told = await answer.json()
            self.assertEqual((told['kind'], told['id'], told['host']), ('node', 'machine-1', 'this-laptop'))
        finally:
            await connector.disconnect()


class LinkLifeTests(NodeTest):
    async def test_a_link_the_room_let_go_is_dialled_again(self):
        # The room keeps one link per node and lets the older go; the library does not come back from that.
        first = await until(lambda: self.room.sid)
        await self.room.server.disconnect(first, namespace='/nodes')
        await until(lambda: self.room.sid != first and self.room.connections >= 2, timeout=15)
        await until(lambda: self.rendezvous.state['connected'])

    async def test_a_refusal_met_while_coming_back_is_reported_and_not_asked_again(self):
        await until(lambda: self.rendezvous.state['connected'])
        # The room goes away; while it is away the machine is revoked; it comes back refusing it.
        self.room_server.should_exit = True
        await self.room_task
        self.room.refuse = 'The room revoked this machine\'s pairing.'
        self.room_server = uvicorn.Server(uvicorn.Config(socketio.ASGIApp(self.room.server, socketio_path='/api/connectors/link'),
                                                         host='127.0.0.1', port=self.room_port, log_level='warning'))
        self.room_task = asyncio.create_task(self.room_server.serve())
        await until(lambda: self.rendezvous.state['refused'], timeout=30)
        self.assertEqual(self.rendezvous.state['refused'], self.room.refuse)
        tried = self.room.connections
        await asyncio.sleep(1.5)
        self.assertEqual(self.room.connections, tried, 'no retry against a refusal')


class UnreachableRoomTests(NodeTest):
    async def test_a_room_that_is_not_there_yet_is_waited_for_not_taken_as_a_refusal(self):
        await until(lambda: self.rendezvous.state['connected'])
        self.room_server.should_exit = True
        await self.room_task
        await until(lambda: not self.rendezvous.state['connected'], timeout=15)
        await asyncio.sleep(1.5)
        self.assertIsNone(self.rendezvous.state['refused'], 'unreachable is not refused')
        self.room_server = uvicorn.Server(uvicorn.Config(socketio.ASGIApp(self.room.server, socketio_path='/api/connectors/link'),
                                                         host='127.0.0.1', port=self.room_port, log_level='warning'))
        self.room_task = asyncio.create_task(self.room_server.serve())
        await until(lambda: self.rendezvous.state['connected'], timeout=30)


class RefusedTests(NodeTest):
    room_refuses = 'The room revoked this machine\'s pairing.'

    async def test_a_room_that_will_not_have_this_machine_is_not_asked_again(self):
        await until(lambda: self.rendezvous.state['refused'])
        self.assertEqual(self.rendezvous.state['refused'], self.room_refuses)
        attempts = self.room.auth
        await asyncio.sleep(0.5)
        self.assertIs(self.room.auth, attempts, 'no retry against a refusal')
        # Pairing again is a new file: the node tries again with it.
        self.room.auth = None
        self.pairing.write_text(json.dumps({'url': f'http://127.0.0.1:{self.room_port}', 'connector_id': 'machine-2',
                                            'token': 'new-token'}))
        await until(lambda: self.room.auth and self.room.auth['connector_id'] == 'machine-2')


class DialTests(NodeTest):
    """The room opens the link: it must show this machine's dial key, then the node proves itself."""

    async def dial(self, key):
        room = socketio.AsyncClient(reconnection=False)
        hello = asyncio.get_running_loop().create_future()

        @room.on('node.hello', namespace='/room')
        async def on_hello(data):
            hello.set_result(data)
            return {'protocol': PROTOCOL}
        await room.connect(f'http://127.0.0.1:{self.node_port}', socketio_path='/api/rendezvous/link',
                           namespaces=['/room'], transports=['websocket'],
                           auth={'connector_id': 'machine-1', 'dial_key': key}, wait_timeout=5)
        return room, hello

    async def test_only_the_paired_room_may_dial_and_the_node_proves_itself_back(self):
        with self.assertRaises(socketio.exceptions.ConnectionError):
            await self.dial('guessed')
        room, hello = await self.dial('the-dial-key')
        try:
            told = await asyncio.wait_for(hello, 5)
            self.assertEqual((told['connector_id'], told['token'], told['protocol']), ('machine-1', 'secret-token', PROTOCOL))
            await until(lambda: self.rendezvous.state['via'] == 'dial')
            answer = await room.call('relay.http', {'method': 'GET', 'path': '/api/presentation/admission', 'query': '',
                                                    'headers': {}, 'body': None}, namespace='/room', timeout=10)
            self.assertEqual(answer['status'], 200)
            # Paired again elsewhere: the link the old room dialled belongs to the old pairing and goes.
            self.pairing.write_text(json.dumps({'url': f'http://127.0.0.1:{self.room_port}', 'connector_id': 'machine-9',
                                                'token': 'other'}))
            await until(lambda: not room.connected, timeout=15)
        finally:
            await room.disconnect()


if __name__ == '__main__':
    unittest.main()
