"""The real connector against the real room, and the numbers PR #68 asked to be compared with.

Everything else in either suite tests one side against a stand-in. This one starts the room on a
loopback port, spawns the connector as the machine would, and drives it through the same local
socket a façade uses: joining, delivery and its acknowledgement, speech, the read receipt a
harness's own transcript produces, a conversation the room closes, a room that restarts and is
found again with everything re-registered, speech queued while there was no room to take it, and
a credential the room will not have.

It runs twice: once on `connector.mjs` as a checkout runs it, and once on the bundle `npm pack`
ships — because a bundle is a different program, and the first one built here died on its first
import while every test on the source stayed green (2026-09-22).
"""
import asyncio
import gc
import json
import os
import shutil
import socket
import statistics
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

import uvicorn
from fastapi import FastAPI

from sidevoice.connector_control import mount_connector_control
from sidevoice.room_history import RoomHistory
from test_connector_control import FakeHub

REPOSITORY = Path(__file__).resolve().parents[3]
PACKAGE = REPOSITORY / 'packages' / 'connector'
NODE = shutil.which('node')
DELIVERIES = 5          # enough for a median that is not one sample's bad luck

# How the connector is started: from the checkout's own modules, and from the one file that is
# published. `npm run build` writes the second; it is built here if it is not already there.
SHAPES = {'source': [str(PACKAGE / 'connector.mjs')],
          'bundle': [str(PACKAGE / 'dist' / 'cli.mjs'), 'connector']}


async def until(check, timeout=15.0, every=.002):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = check()
        if asyncio.iscoroutine(value):
            value = await value
        if value:
            return value
        await asyncio.sleep(every)
    raise AssertionError('timed out waiting')


def free_port():
    """A port to hold for the whole run, so the room can be stopped and started again on it."""
    with socket.socket() as probe:
        probe.bind(('127.0.0.1', 0))
        return probe.getsockname()[1]


def built():
    """The published bundle, built if this checkout has not built it yet."""
    bundle = PACKAGE / 'dist' / 'cli.mjs'
    if not bundle.exists():
        subprocess.run(['npm', 'run', 'build'], cwd=PACKAGE, check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    return bundle


class Facade:
    """A conversation's side of the connector: newline-delimited JSON over its local socket."""

    def __init__(self, reader, writer):
        self.reader, self.writer, self.serial, self.waiting = reader, writer, 0, {}
        self.pump = asyncio.create_task(self.receive())

    @classmethod
    async def attach(cls, path):
        reader, writer = await asyncio.open_unix_connection(path)
        return cls(reader, writer)

    async def receive(self):
        while line := await self.reader.readline():
            reply = json.loads(line)
            waiter = self.waiting.pop(reply.get('id'), None)
            if waiter and not waiter.done():
                waiter.set_result(reply)

    async def call(self, method, **params):
        self.serial += 1
        waiter = asyncio.get_running_loop().create_future()
        self.waiting[self.serial] = waiter
        self.writer.write((json.dumps({'id': self.serial, 'method': method, 'params': params}) + '\n').encode())
        await self.writer.drain()
        reply = await asyncio.wait_for(waiter, timeout=20)
        if not reply.get('ok'):
            raise AssertionError(f'{method} failed: {reply.get("error")}')
        return reply['result']

    async def close(self):
        self.pump.cancel()
        self.writer.close()


class Inbox:
    """Claude Code's session inbox, as far as the connector can tell: a socket that takes the
    lines and says nothing, which is exactly what the real one does."""

    def __init__(self, path):
        self.path, self.received, self.server = path, [], None

    async def start(self):
        self.server = await asyncio.start_unix_server(self.serve, path=self.path)
        return self

    async def serve(self, reader, writer):
        while line := await reader.readline():
            self.received.append(json.loads(line))

    async def stop(self):
        self.server.close()


class Room:
    """The room on a loopback port, stoppable and startable again on the same one."""

    def __init__(self, app, port):
        self.app, self.port, self.server, self.task = app, port, None, None

    async def start(self):
        config = uvicorn.Config(self.app, host='127.0.0.1', port=self.port, log_level='error')
        self.server = uvicorn.Server(config)
        self.task = asyncio.create_task(self.server.serve())
        await until(lambda: self.server.started, timeout=20)

    async def stop(self):
        self.server.should_exit = True
        await asyncio.wait_for(self.task, timeout=20)


@unittest.skipIf(NODE is None, 'node is not on PATH')
class ConnectorInteropTests(unittest.IsolatedAsyncioTestCase):
    maxDiff = None

    async def test_the_same_connector_does_the_same_work_from_the_checkout_and_from_the_bundle(self):
        built()
        numbers = {}
        for shape in SHAPES:
            with self.subTest(shape=shape):
                numbers[shape] = await self.exercise(shape)
        report(numbers)
        for shape in SHAPES:
            self.assertGreater(numbers[shape]['delivery_to_ack'], 0)
            self.assertGreater(numbers[shape]['disconnect_to_registered'], 0)

    async def exercise(self, shape):
        def step(name):
            if os.environ.get('SIDEVOICE_INTEROP_TRACE'):
                print(f'  [{shape}] {name}', file=sys.stderr, flush=True)

        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        root = Path(temp.name)
        data, claude = root / 'sidevoice', root / 'claude'
        (data).mkdir(); (claude / 'sessions').mkdir(parents=True)
        (claude / 'projects' / '-home-someone-project').mkdir(parents=True)

        journal = RoomHistory(root / 'room.json')
        hub = FakeHub(journal)
        app = FastAPI()
        control = mount_connector_control(app, hub, heartbeat_seconds=5)
        connector_id, token = journal.redeem_pairing_code(journal.create_pairing_code())

        # Every delivery is timed from the room putting it on the wire to the connector's answer.
        acknowledged, settle = [], control.settle

        async def timed(connector, binding, peer, payload):
            started = time.perf_counter()
            try:
                return await settle(connector, binding, peer, payload)
            finally:
                acknowledged.append((time.perf_counter() - started) * 1000)
        control.settle = timed

        port = free_port()
        room = Room(app, port)
        await room.start()

        (data / 'credentials.json').write_text(json.dumps({
            'url': f'http://127.0.0.1:{port}', 'connector_id': connector_id, 'token': token, 'protocol': 2}))

        inbox = await Inbox(str(data / 'inbox.sock')).start()
        transcript = claude / 'projects' / '-home-someone-project' / 'sess-claude.jsonl'
        registry = claude / 'sessions' / '4242.json'
        registry.write_text(json.dumps({'sessionId': 'sess-claude', 'pid': 4242, 'status': 'idle'}))
        transcript.write_text('')

        connector = await asyncio.create_subprocess_exec(
            NODE, *SHAPES[shape],
            env={**os.environ, 'SIDEVOICE_DATA_DIR': str(data), 'SIDEVOICE_CONNECTOR_IDLE_MS': '120000',
                 'CLAUDE_CONFIG_DIR': str(claude), 'SIDEVOICE_WORK_POLL_MS': '30',
                 'SIDEVOICE_WORK_ANNOUNCE_MS': '5000'},
            stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL)

        async def finish():
            if connector.returncode is None:
                connector.terminate()
                await asyncio.wait_for(connector.wait(), timeout=10)
            await inbox.stop()
            await room.stop()
        self.addAsyncCleanup(finish)

        socket_path = data / 'connector.sock'
        try:
            await until(socket_path.exists, timeout=20)
            facade = await Facade.attach(str(socket_path))
            self.addAsyncCleanup(facade.close)

            # ----- joining -----
            delivered = []
            harness = await start_harness(delivered)
            self.addAsyncCleanup(stop_harness, harness)
            step('joining')
            joined = await facade.call('register', client_ref='thread-1', harness='http', thread='thread-1',
                                       title='Interop', engine={'model': 'claude-opus-5'},
                                       delivery={'kind': 'http', 'url': harness['url'], 'thread': 'thread-1'})
            self.assertFalse(joined['binding_id'].startswith('local-'), 'the room minted the id')
            self.assertTrue(joined['connected'])
            binding = journal.binding_for_thread('thread-1')
            self.assertTrue(control.is_live(binding['id']))
            self.assertEqual(journal.bindings()[0]['engine'], {'model': 'claude-opus-5'})
            self.assertEqual((await facade.call('status'))['protocol'], 2)

            # ----- delivery and its acknowledgement -----
            step('deliveries')
            for index in range(DELIVERIES):
                row = queue_input(journal, 'thread-1', f'mensaje {index}', f'm{index}')
                await until(lambda row=row: journal.get(row['id'])['status'] == 'delivered')
            self.assertEqual([event['text'] for event in delivered], [f'mensaje {i}' for i in range(DELIVERIES)])
            self.assertEqual(len(acknowledged), DELIVERIES)

            # ----- speech -----
            step('speech')
            published = await facade.call('publish', client_ref='thread-1', session_id='call', revision=1,
                                          text='Ya está', language='es')
            self.assertEqual(published['status'], 'queued')
            await until(lambda: [speech.text for speech in hub.published] == ['Ya está'])

            # ----- the read receipt, from what the harness itself writes -----
            step('read receipt')
            claude_binding = await facade.call('register', client_ref='sess-claude', harness='claude', thread='sess-claude',
                                               title='Claude', delivery={'kind': 'claude-uds', 'socket': inbox.path, 'token': 'tok'})
            read_row = queue_input(journal, 'sess-claude', 'hola desde la sala', 'm-read')
            await until(lambda: any(line.get('type') == 'user' for line in inbox.received), timeout=20)
            posted = next(line for line in inbox.received if line['type'] == 'user')['message']['content']
            self.assertIn('hola desde la sala', posted)
            self.assertEqual(journal.get(read_row['id'])['status'], 'sending', 'nothing is claimed before the session takes it')
            with transcript.open('a') as lines:
                lines.write(json.dumps({'type': 'user', 'promptId': 'p-1', 'message': {'role': 'user', 'content': posted}}) + '\n')
            await until(lambda: journal.get(read_row['id'])['status'] == 'read', timeout=20)

            # ----- the room closes a conversation's voice -----
            step('closing from the room')
            await control.close_binding(journal.binding(claude_binding['binding_id']))

            async def told():
                return 'sess-claude' in (await facade.call('status'))['closed_by_room']
            await until(told)

            # ----- the room goes, and the conversation comes back knowing what it is for -----
            # A restart, which is what a person does to the room and the only severing both ends
            # see at once. The binding is forgotten first, exactly as a restarted room forgets it,
            # so being live again can only mean the connector registered it a second time.
            step('reconnect')
            was = journal.binding_for_thread('thread-1')['id']
            journal.deactivate_binding(connector_id, was)
            control.live.pop(was, None)
            started = time.perf_counter()
            await room.stop()
            await room.start()

            def live_again():
                record = journal.binding_for_thread('thread-1')
                return bool(record) and control.is_live(record['id'])
            await until(live_again, timeout=30)
            reconnected = (time.perf_counter() - started) * 1000

            # ----- speech said while there was no room to take it -----
            step('outbox')
            await room.stop()

            async def roomless():
                return not (await facade.call('status'))['connected']
            await until(roomless, timeout=20)
            queued = await facade.call('publish', client_ref='thread-1', session_id='call', revision=2,
                                       text='Dicho sin sala', language='es')
            self.assertEqual(queued['status'], 'queued')
            self.assertEqual(json.loads((data / 'outbox.json').read_text())[0]['text'], 'Dicho sin sala')
            await room.start()
            await until(lambda: 'Dicho sin sala' in [speech.text for speech in hub.published], timeout=30)
            await until(lambda: json.loads((data / 'outbox.json').read_text()) == [], timeout=10)

            return {'delivery_to_ack': statistics.median(acknowledged), 'disconnect_to_registered': reconnected}
        except Exception:
            log = data / 'connector.log'
            if log.exists():
                print(f'\n--- connector log ({shape}) ---\n{log.read_text()}', file=sys.stderr)
            raise

    async def test_a_credential_this_room_will_not_have_is_refused_and_not_retried(self):
        """The acceptance criterion a person meets after upgrading: the room says what to do, the
        connector says it where they will look, and it stops asking."""
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        data = Path(temp.name) / 'sidevoice'
        data.mkdir()

        journal = RoomHistory(Path(temp.name) / 'room.json')
        app = FastAPI()
        mount_connector_control(app, FakeHub(journal), heartbeat_seconds=5)
        connector_id, _ = journal.redeem_pairing_code(journal.create_pairing_code())
        port = free_port()
        room = Room(app, port)
        await room.start()
        self.addAsyncCleanup(room.stop)

        (data / 'credentials.json').write_text(json.dumps({
            'url': f'http://127.0.0.1:{port}', 'connector_id': connector_id, 'token': 'not-the-token'}))
        connector = await asyncio.create_subprocess_exec(
            NODE, *SHAPES['source'],
            env={**os.environ, 'SIDEVOICE_DATA_DIR': str(data), 'SIDEVOICE_CONNECTOR_IDLE_MS': '120000'},
            stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL)

        async def finish():
            if connector.returncode is None:
                connector.terminate()
                await asyncio.wait_for(connector.wait(), timeout=10)
        self.addAsyncCleanup(finish)

        log = data / 'connector.log'
        await until(lambda: log.exists() and 'will not be asked again' in log.read_text(), timeout=20)
        said = log.read_text()
        self.assertIn('Emparejar máquina', said, 'it says where the code comes from')
        attempts = said.count('will not be asked again')
        await asyncio.sleep(3)
        self.assertEqual(log.read_text().count('will not be asked again'), attempts,
                         'it stopped, rather than asking a refused credential for ever')


def queue_input(journal, thread, text, message_id):
    return journal.put(id=f'call:user-turn:{message_id}', thread=thread, role='user', text=text, name='Tú',
                       session='call', revision=1, status='pending',
                       payload={'thread_id': thread, 'text': text, 'message_id': message_id,
                                'session_id': 'call', 'revision': 1})


async def start_harness(received):
    """A conversation that takes what it is given and says so at once, so what is measured between
    the event and its acknowledgement is the link and not somebody's inbox."""
    async def handle(reader, writer):
        request = await reader.readuntil(b'\r\n\r\n')
        length = next((int(line.split(b':')[1]) for line in request.split(b'\r\n') if line.lower().startswith(b'content-length')), 0)
        received.append(json.loads(await reader.readexactly(length)))
        writer.write(b'HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}')
        await writer.drain()
        writer.close()

    server = await asyncio.start_server(handle, '127.0.0.1', 0)
    port = server.sockets[0].getsockname()[1]
    return {'server': server, 'url': f'http://127.0.0.1:{port}/presentation/message'}


async def stop_harness(harness):
    harness['server'].close()
    await harness['server'].wait_closed()


def report(numbers):
    print('\n  connector   delivery → ack   room restart → re-registered')
    for shape, measured in numbers.items():
        print(f'  {shape:<10}  {measured["delivery_to_ack"]:>9.1f} ms   {measured["disconnect_to_registered"]:>21.1f} ms')
    print()


def tearDownModule():
    """Give back what this module borrowed, here, where nothing is being timed.

    Serving a room, spawning connectors and opening sockets leaves thousands of objects that only
    the cyclic collector can reclaim, and a full collection over this suite's heap costs about a
    tenth of a second. Left for the interpreter to schedule, that cost lands wherever the threshold
    happens to fall — which is somebody else's test, and the reason this module used to make one of
    them fail roughly two runs in three (#69). Reclaiming it at this boundary costs the same tenth
    of a second and spends it on the tests that made the garbage.
    """
    gc.collect()
