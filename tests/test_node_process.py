"""The core as the connector starts it: a process that says where it listens in a file, lets this
machine's connector link through its local socket with the credential in that file, leaves once nothing
uses it, and — when it cannot start — says why in a file its supervisor reads."""
import asyncio
import json
import os
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import time
import unittest
from datetime import datetime
from pathlib import Path

import socketio

from sidevoice_core.control.connectors import PROTOCOL
from test_rendezvous import LOCAL, link_client


async def until(check, timeout=20.0, every=0.05):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = check()
        if value:
            return value
        await asyncio.sleep(every)
    raise AssertionError('timed out waiting')


def ready(path):
    try:
        return json.loads(path.read_text())
    except (OSError, ValueError):
        return None


def shadowing(root, module):
    """An environment in which importing `module` fails as a missing module does: a package of that name, first
    on the path, that raises what Python raises."""
    package = Path(root) / 'shadow' / module
    package.mkdir(parents=True)
    (package / '__init__.py').write_text(f'raise ModuleNotFoundError("No module named {module!r}", name={module!r})\n')
    return {**os.environ, 'PYTHONPATH': os.pathsep.join(filter(None, [str(package.parent), os.environ.get('PYTHONPATH')]))}


class NodeProcessTest(unittest.IsolatedAsyncioTestCase):
    def start(self, data, idle='2', *extra, env=None):
        process = subprocess.Popen([sys.executable, '-m', 'sidevoice_core.server', '--port', '0',
                                    '--data-dir', str(data), '--idle-exit', idle, *extra],
                                   stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True, env=env)
        self.addCleanup(lambda: process.poll() is None and process.kill())
        return process

    async def link(self, facts, **auth):
        client = link_client(self, facts['socket'])
        welcome = asyncio.get_running_loop().create_future()
        client.on('connector.welcome', lambda data: welcome.done() or welcome.set_result(data), namespace='/connectors')
        await client.connect(LOCAL, namespaces=['/connectors'], transports=['websocket'],
                             socketio_path='/api/connectors/link',
                             auth={'connector_id': facts['connector_id'], 'token': facts['token'],
                                   'protocol': PROTOCOL, 'host': 'test', **auth})
        return client, await asyncio.wait_for(welcome, 10)

    async def test_the_ready_file_carries_the_link_and_the_core_leaves_when_nothing_uses_it(self):
        with tempfile.TemporaryDirectory() as root:
            data = Path(root) / 'core'
            process = self.start(data, '2', '--launch-id', 'launch-1')
            facts = await until(lambda: ready(data / 'core.json'))
            self.assertEqual(stat.S_IMODE(os.stat(data / 'core.json').st_mode), 0o600, 'it holds a credential')
            self.assertEqual(stat.S_IMODE(data.stat().st_mode), 0o700, 'a directory it created is this user\'s alone')
            self.assertEqual(facts['pid'], process.pid)
            self.assertEqual(facts['url'], f"http://127.0.0.1:{facts['port']}")
            self.assertEqual(facts['protocol'], PROTOCOL)
            self.assertEqual((facts['launch_id'], facts['api']), ('launch-1', 1))
            self.assertEqual(facts['socket'], str((data / 'local.sock').absolute()))
            self.assertTrue(stat.S_ISSOCK(os.stat(facts['socket']).st_mode))
            client, welcome = await self.link(facts)
            self.assertEqual(welcome, {'protocol': PROTOCOL})
            # The connector's link is the socket's alone: over TCP the same credential reaches nothing.
            over_tcp = socketio.AsyncClient(reconnection=False)
            with self.assertRaises(socketio.exceptions.ConnectionError):
                await over_tcp.connect(facts['url'], namespaces=['/connectors'], transports=['websocket'],
                                       socketio_path='/api/connectors/link',
                                       auth={'connector_id': facts['connector_id'], 'token': facts['token'],
                                             'protocol': PROTOCOL})
            # A linked connector keeps the core alive past its idle budget.
            await asyncio.sleep(3)
            self.assertIsNone(process.poll(), 'a core with a connector linked does not leave')
            await client.disconnect()
            await until(lambda: process.poll() is not None, timeout=20)
            self.assertFalse((data / 'core.json').exists(), 'the file leaves with the process')
            self.assertFalse((data / 'local.sock').exists(), 'and so does the socket')
            # Started again, it keeps the same credential: a connector reconnecting needs no new one.
            again = self.start(data, '30')
            second = await until(lambda: ready(data / 'core.json'))
            self.assertEqual(second['pid'], again.pid)
            self.assertIsNone(second['launch_id'], 'no supervisor named this launch')
            self.assertEqual((second['connector_id'], second['token']), (facts['connector_id'], facts['token']))
            again.terminate()
            again.wait(10)

    async def test_a_credential_that_is_not_the_file_s_is_refused(self):
        with tempfile.TemporaryDirectory() as root:
            data = Path(root) / 'core'
            self.start(data, idle='30')
            facts = await until(lambda: ready(data / 'core.json'))
            with self.assertRaises(socketio.exceptions.ConnectionError):
                await self.link({**facts, 'token': 'guessed'})

    async def test_a_supervisor_s_sigterm_is_a_clean_exit(self):
        """Stopping the core leaves no file that says it is serving: not the ready file, not the socket."""
        with tempfile.TemporaryDirectory() as root:
            data = Path(root) / 'core'
            process = self.start(data, '0')
            await until(lambda: ready(data / 'core.json'))
            process.send_signal(signal.SIGTERM)
            self.assertEqual(process.wait(20), 0)
            self.assertEqual(sorted(p.name for p in data.iterdir() if p.name in {'core.json', 'local.sock'}), [])

    async def test_a_socket_a_dead_core_left_is_replaced(self):
        with tempfile.TemporaryDirectory() as root:
            data = Path(root) / 'core'
            data.mkdir(mode=0o700)
            stale = socket.socket(socket.AF_UNIX)
            stale.bind(str(data / 'local.sock'))
            stale.close()   # bound, never listening, nobody behind it: what a SIGKILLed core leaves
            self.start(data, '30')
            facts = await until(lambda: ready(data / 'core.json'))
            client, welcome = await self.link(facts)
            self.assertEqual(welcome, {'protocol': PROTOCOL})


class StartFailureTest(unittest.TestCase):
    """Each reason a start fails for leaves its key, with the launch that failed, in `core-failure.json`."""

    def run_core(self, data, *extra, env=None, port='0'):
        return subprocess.run([sys.executable, '-m', 'sidevoice_core.server', '--port', port, '--data-dir', str(data),
                               '--idle-exit', '0', '--launch-id', 'launch-7', *extra],
                              capture_output=True, text=True, env=env, timeout=120)

    def failure(self, data, result, step, key, *, ready_file=False):
        self.assertEqual(result.returncode, 1, result.stderr[-2000:])
        path = data / 'core-failure.json'
        self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
        report = json.loads(path.read_text())
        self.assertEqual(set(report), {'launch_id', 'step', 'key', 'message', 'at'})
        self.assertEqual((report['launch_id'], report['step'], report['key']), ('launch-7', step, key))
        self.assertTrue(report['message'])
        self.assertEqual(datetime.fromisoformat(report['at']).utcoffset().total_seconds(), 0, 'UTC')
        self.assertIn(key, result.stderr, 'the log says it too')
        if not ready_file:
            self.assertFalse((data / 'core.json').exists(), 'a core that did not start never says it is ready')
        return report

    def test_an_identity_that_cannot_be_read(self):
        with tempfile.TemporaryDirectory() as root:
            data = Path(root) / 'core'
            data.mkdir(mode=0o700)
            (data / 'node-identity.json').write_text('{"private_key_pem": "not a key"}')
            self.failure(data, self.run_core(data), 'identity', 'identity.unreadable')
            self.assertEqual((data / 'node-identity.json').read_text(), '{"private_key_pem": "not a key"}', 'never replaced')

    def test_a_directory_other_users_may_enter(self):
        with tempfile.TemporaryDirectory() as root:
            data = Path(root) / 'core'
            data.mkdir(mode=0o700)
            data.chmod(0o750)
            report = self.failure(data, self.run_core(data), 'directory', 'identity.unsafe-directory')
            self.assertIn('0750', report['message'])
            self.assertFalse((data / 'local.sock').exists())
            self.assertFalse((data / 'node-identity.json').exists(), 'no secret is written into it')

    def test_a_port_in_use(self):
        with tempfile.TemporaryDirectory() as root, socket.socket() as holder:
            holder.bind(('127.0.0.1', 0))
            holder.listen()
            data = Path(root) / 'core'
            self.failure(data, self.run_core(data, port=str(holder.getsockname()[1])), 'bind', 'bind.port-in-use')

    def test_a_socket_a_live_core_serves_is_not_taken_from_it(self):
        with tempfile.TemporaryDirectory() as root:
            data = Path(root) / 'core'
            first = subprocess.Popen([sys.executable, '-m', 'sidevoice_core.server', '--port', '0', '--data-dir', str(data),
                                      '--idle-exit', '0'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            self.addCleanup(lambda: first.poll() is None and first.kill())
            deadline = time.monotonic() + 30
            while not ready(data / 'core.json') and time.monotonic() < deadline:
                time.sleep(0.05)
            self.assertEqual(ready(data / 'core.json')['pid'], first.pid)
            self.failure(data, self.run_core(data), 'bind', 'bind.port-in-use', ready_file=True)
            self.assertIsNone(first.poll(), 'the first core is untouched')
            self.assertEqual(ready(data / 'core.json')['pid'], first.pid, 'and so is the file that says it is serving')
            self.assertTrue((data / 'local.sock').exists())
            first.terminate()
            first.wait(20)

    def test_a_missing_module(self):
        with tempfile.TemporaryDirectory() as root:
            data = Path(root) / 'core'
            report = self.failure(data, self.run_core(data, env=shadowing(root, 'uvicorn')), 'import',
                                  'import.missing-module')
            self.assertIn('uvicorn', report['message'], 'the message names the module')


class SelfTestTest(unittest.TestCase):
    """`--self-test`: what an installer runs on a staged runtime. One JSON line; nothing bound, nothing written."""

    def self_test(self, env):
        return subprocess.run([sys.executable, '-m', 'sidevoice_core.server', '--self-test'], capture_output=True,
                              text=True, env=env, timeout=120)

    def test_it_passes_on_a_complete_install_and_touches_nothing(self):
        with tempfile.TemporaryDirectory() as root:
            data = Path(root) / 'core'
            result = self.self_test({**os.environ, 'SIDEVOICE_CORE_DATA_DIR': str(data)})
            self.assertEqual(result.returncode, 0, result.stderr[-2000:])
            self.assertEqual(json.loads(result.stdout), {'ok': True, 'version': json.loads(result.stdout)['version']})
            self.assertEqual(len(result.stdout.strip().splitlines()), 1)
            self.assertFalse(data.exists(), 'nothing written')

    def test_it_fails_with_the_missing_module_named(self):
        with tempfile.TemporaryDirectory() as root:
            result = self.self_test(shadowing(root, 'aiortc'))
            self.assertEqual(result.returncode, 1)
            said = json.loads(result.stdout)
            self.assertEqual((said['ok'], said['key']), (False, 'import.missing-module'))
            self.assertIn('aiortc', said['message'])


if __name__ == '__main__':
    unittest.main()
