"""The core as the connector starts it: a process that says where it listens in a file, lets this
machine's connector link through its local socket with the credential in that file, leaves once nothing
uses it, and — when it cannot start — says why in a file, and by its exit status what the service manager does."""
import asyncio
import errno
import fcntl
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
import uuid
from datetime import datetime
from pathlib import Path
from unittest.mock import patch

import socketio

from sidevoice_core.control.connectors import PROTOCOL
from sidevoice_core.control.room import Room
from sidevoice_core.server import __main__ as core
from sidevoice_core.server.local import LocalListener
from test_rendezvous import LOCAL, link_client, through


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


# Fails the n-th listen() on a TCP (or Unix) socket with EADDRINUSE, as a rival bound to the same address and
# listening first would: (1) the core's own bind, (2) uvicorn starting its server on that socket, after the app's
# lifespan began.
FAIL_LISTEN = """import errno, os, socket
fail, calls, listen = int(os.environ['SIDEVOICE_TEST_FAIL_LISTEN']), [0], socket.socket.listen
families = (socket.AF_UNIX,) if os.environ['SIDEVOICE_TEST_FAIL_FAMILY'] == 'unix' else (socket.AF_INET, socket.AF_INET6)
def failing(self, *args):
    if self.family in families:
        calls[0] += 1
        if calls[0] == fail:
            raise OSError(errno.EADDRINUSE, os.strerror(errno.EADDRINUSE))
    return listen(self, *args)
socket.socket.listen = failing
"""


def failing_listen(root, which, family='inet'):
    """An environment whose interpreter fails the `which`-th listen() of `family` (`sitecustomize`, first on the
    path)."""
    site = Path(root) / 'site'
    site.mkdir()
    (site / 'sitecustomize.py').write_text(FAIL_LISTEN)
    return {**os.environ, 'SIDEVOICE_TEST_FAIL_LISTEN': str(which), 'SIDEVOICE_TEST_FAIL_FAMILY': family,
            'PYTHONPATH': os.pathsep.join(filter(None, [str(site), os.environ.get('PYTHONPATH')]))}


# A core that crashes right after saying it is ready: the first log line after the ready file is written raises.
CRASH_AFTER_READY = """from loguru._logger import Logger
info = Logger.info
def crashing(self, message, *args, **kwargs):
    if str(message).startswith('Sidevoice core {} listening'):
        raise RuntimeError('crashed after ready')
    return info(self, message, *args, **kwargs)
Logger.info = crashing
"""


def crashing_after_ready(root):
    site = Path(root) / 'crash-site'
    site.mkdir()
    (site / 'sitecustomize.py').write_text(CRASH_AFTER_READY)
    return {**os.environ, 'PYTHONPATH': os.pathsep.join(filter(None, [str(site), os.environ.get('PYTHONPATH')]))}


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
            self.assertEqual(str(uuid.UUID(second['launch_id'])), second['launch_id'], 'none was given: one is made up')
            async with through(second['socket']) as http, http.get(LOCAL + '/api/local/health') as answer:
                self.assertEqual((await answer.json())['launch_id'], second['launch_id'])
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

    async def test_a_manager_s_sigterm_or_a_sigint_is_a_clean_exit(self):
        """Exit 0, not restarted; and no file that says it is serving is left: not the ready file, not the socket."""
        for signum in (signal.SIGTERM, signal.SIGINT):
            with self.subTest(signal=signum.name), tempfile.TemporaryDirectory() as root:
                data = Path(root) / 'core'
                process = self.start(data, '0')
                await until(lambda: ready(data / 'core.json'))
                process.send_signal(signum)
                self.assertEqual(process.wait(20), 0)
                self.assertEqual(sorted(p.name for p in data.iterdir() if p.name in {'core.json', 'local.sock'}), [])

    async def test_a_crash_after_ready_is_not_a_reported_failure_and_exits_non_zero(self):
        """A core that did serve and then broke is the manager's to restart: no report, a non-zero status."""
        with tempfile.TemporaryDirectory() as root:
            data = Path(root) / 'core'
            process = self.start(data, '0', env=crashing_after_ready(root))
            self.assertNotEqual(process.wait(60), 0)
            self.assertIn('crashed after ready', process.stderr.read(), 'what a crash prints is the manager\'s')
            self.assertFalse((data / 'core-failure.json').exists())

    async def test_a_start_clears_the_failure_report_a_previous_one_left(self):
        with tempfile.TemporaryDirectory() as root:
            data = Path(root) / 'core'
            data.mkdir(mode=0o700)
            (data / 'core-failure.json').write_text(json.dumps({'key': 'identity.unreadable', 'launch_id': 'old'}))
            self.start(data, '0')
            await until(lambda: ready(data / 'core.json'))
            self.assertFalse((data / 'core-failure.json').exists(), 'the core clears its own; nothing else does')

    async def test_the_core_writes_its_own_rotating_log_and_leaves_stdout_and_stderr_to_the_manager(self):
        with tempfile.TemporaryDirectory() as root:
            data = Path(root) / 'core'
            process = self.start(data, '0', '--launch-id', 'logged')
            facts = await until(lambda: ready(data / 'core.json'))
            log = Path(root) / 'core.log'
            await until(lambda: log.exists() and 'listening on' in log.read_text())
            self.assertEqual(stat.S_IMODE(log.stat().st_mode), 0o600)
            self.assertIn('Application startup complete', log.read_text(), 'uvicorn\'s own lines too')
            process.terminate()
            self.assertEqual(process.wait(20), 0)
            self.assertEqual(process.stderr.read(), '', 'nothing on stderr when nothing crashed')
            self.assertEqual(facts['launch_id'], 'logged')

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


    async def test_a_call_still_waiting_for_its_first_message_keeps_the_core_up_and_is_counted(self):
        """What an update reads before restarting, and what idle exit waits for, is every call socket open —
        from its acceptance, not from its first message."""
        with tempfile.TemporaryDirectory() as root:
            data = Path(root) / 'core'
            process = self.start(data, '2')
            facts = await until(lambda: ready(data / 'core.json'))
            async with through(facts['socket']) as http:
                async with http.post(LOCAL + '/api/device/local/pair', json={'name': 'app'}) as answer:
                    token = (await answer.json())['token']
                ws = await http.ws_connect('ws://localhost/api/presentation/ws',
                                           protocols=['sidevoice', f'sidevoice.token.{token}'])
                async with http.get(LOCAL + '/api/local/health') as answer:
                    self.assertEqual((await answer.json())['calls'], 1, 'before any hello')
                await asyncio.sleep(4)   # past the idle budget, well inside the ten seconds a hello may take
                self.assertIsNone(process.poll(), 'a call waiting for its first message is a call')
                await ws.close()
            await until(lambda: process.poll() is not None, timeout=20)

    async def test_a_relative_data_directory_is_one_directory(self):
        """Relative, its socket's directory (absolute) is the same directory: locked once, so the core starts."""
        with tempfile.TemporaryDirectory() as root:
            process = subprocess.Popen([sys.executable, '-m', 'sidevoice_core.server', '--port', '0', '--data-dir',
                                        'relative-core', '--idle-exit', '0'], cwd=root,
                                       stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
            self.addCleanup(lambda: process.poll() is None and process.kill())
            data = Path(root) / 'relative-core'
            facts = await until(lambda: ready(data / 'core.json') or process.poll() is not None)
            self.assertIsNone(process.poll(), process.stderr.read() if process.poll() is not None else '')
            self.assertEqual(facts['socket'], str(data / 'local.sock'))
            client, welcome = await self.link(facts)
            self.assertEqual(welcome, {'protocol': PROTOCOL})
            process.terminate()
            self.assertEqual(process.wait(20), 0)

    async def test_a_socket_named_through_an_alias_of_the_data_directory(self):
        """The socket's directory spelled through a link above it is still the data directory: one lock."""
        with tempfile.TemporaryDirectory() as root:
            data = Path(root) / 'core'
            (Path(root) / 'alias').symlink_to(root)
            aliased = Path(root) / 'alias' / 'core' / 'local.sock'
            process = self.start(data, '0', '--socket', str(aliased))
            facts = await until(lambda: ready(data / 'core.json') or process.poll() is not None)
            self.assertIsNone(process.poll(), process.stderr.read() if process.poll() is not None else '')
            self.assertEqual(facts['socket'], str(aliased))
            self.assertTrue((data / 'local.sock').is_socket(), 'one directory, two spellings')
            async with through(aliased) as http, http.get(LOCAL + '/api/local/health') as answer:
                self.assertEqual((await answer.json())['pid'], process.pid)

    async def test_overlapping_starts_leave_exactly_one_core(self):
        """Two starters at once on one data directory: whichever order their steps interleave in, one serves and
        the other says the socket is taken — never two cores, never one socket replacing another's."""
        for attempt in range(3):
            with self.subTest(attempt=attempt), tempfile.TemporaryDirectory() as root:
                data = Path(root) / 'core'
                data.mkdir(mode=0o700)
                rivals = [self.start(data, '0', '--launch-id', f'rival-{n}') for n in range(2)]
                facts = await until(lambda: ready(data / 'core.json'), timeout=60)
                loser = await until(lambda: next((r for r in rivals if r.poll() is not None), None), timeout=60)
                winner = next(r for r in rivals if r is not loser)
                await asyncio.sleep(1)
                self.assertIsNone(winner.poll(), 'the other one serves')
                self.assertEqual(loser.returncode, 75, 'tried again later by the manager')
                self.assertEqual(facts['pid'], winner.pid)
                report = json.loads((data / 'core-failure.json').read_text())
                self.assertEqual((report['key'], report['step']), ('bind.core-running', 'bind'))
                self.assertEqual(report['launch_id'], f'rival-{rivals.index(loser)}')
                self.assertIn(str(data / 'local.sock'), report['message'], 'the socket is named')
                async with through(data / 'local.sock') as http, http.get(LOCAL + '/api/local/health') as answer:
                    self.assertEqual((await answer.json())['pid'], winner.pid, 'the socket is the survivor\'s')
                winner.terminate()
                self.assertEqual(winner.wait(20), 0)


class StartFailureTest(unittest.TestCase):
    """Each reason a start fails for leaves its key, with the launch that failed, in `core-failure.json`."""

    def run_core(self, data, *extra, env=None, port='0'):
        return subprocess.run([sys.executable, '-m', 'sidevoice_core.server', '--port', port, '--data-dir', str(data),
                               '--idle-exit', '0', '--launch-id', 'launch-7', *extra],
                              capture_output=True, text=True, env=env, timeout=120)

    def failure(self, data, result, step, key, *, ready_file=False, status=0, launch_id='launch-7'):
        """The report a failed start left. Exit 0 unless said otherwise: a failure that would repeat is not one for
        the service manager to restart."""
        self.assertEqual(result.returncode, status, result.stderr[-2000:])
        path = data / 'core-failure.json'
        self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
        report = json.loads(path.read_text())
        self.assertEqual(set(report), {'launch_id', 'step', 'key', 'message', 'at'})
        self.assertEqual((report['launch_id'], report['step'], report['key']), (launch_id, step, key))
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

    def test_an_identity_that_is_not_even_text(self):
        for content in (b'\xff\xfe', b'{"private_key_pem": "\xc3\x28"}'):
            with self.subTest(content=content), tempfile.TemporaryDirectory() as root:
                data = Path(root) / 'core'
                data.mkdir(mode=0o700)
                (data / 'node-identity.json').write_bytes(content)
                self.failure(data, self.run_core(data), 'identity', 'identity.unreadable')
                self.assertEqual((data / 'node-identity.json').read_bytes(), content, 'never replaced')

    def test_any_other_failure_before_ready_is_start_failed(self):
        """Not one of the named reasons — here a ready file that cannot be written — is still a failed start: its
        report with the exception's last line, and exit 0."""
        with tempfile.TemporaryDirectory() as root:
            data = Path(root) / 'core'
            (Path(root) / 'a-file').write_text('')
            result = self.run_core(data, '--ready-file', str(Path(root) / 'a-file' / 'core.json'))
            report = self.failure(data, result, 'start', 'start.failed')
            self.assertRegex(report['message'], r'^\w+Error: .*a-file', 'the exception\'s last line')
            self.assertFalse((data / 'local.sock').exists())

    def test_a_failure_without_a_launch_id_reports_the_one_made_up(self):
        with tempfile.TemporaryDirectory() as root:
            data = Path(root) / 'core'
            data.mkdir(mode=0o700)
            (data / 'node-identity.json').write_text('not a key')
            result = subprocess.run([sys.executable, '-m', 'sidevoice_core.server', '--port', '0', '--data-dir', str(data),
                                     '--idle-exit', '0'], capture_output=True, text=True, timeout=120)
            report = json.loads((data / 'core-failure.json').read_text())
            self.assertEqual(result.returncode, 0)
            self.assertEqual(str(uuid.UUID(report['launch_id'])), report['launch_id'])
            self.assertIn(report['launch_id'], (Path(root) / 'core.log').read_text(), 'and the log says which')

    def test_a_directory_another_core_holds(self):
        """The lock is taken before anything is: a refused starter has written nothing but its report."""
        with tempfile.TemporaryDirectory() as root:
            data = Path(root) / 'core'
            data.mkdir(mode=0o700)
            with open(data / 'core.lock', 'w') as lock:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                report = self.failure(data, self.run_core(data), 'bind', 'bind.core-running', status=75)
            self.assertIn(str((data / 'local.sock').absolute()), report['message'])
            self.assertEqual(sorted(p.name for p in data.iterdir()), ['core-failure.json', 'core.lock'])

    def test_a_port_taken_between_bind_and_listen(self):
        """Taken at the core's own listen(), or at uvicorn's after the app's lifespan began: the same key, and
        whatever this start had bound is gone with it."""
        for which in (1, 2):
            with self.subTest(listen=which), tempfile.TemporaryDirectory() as root:
                data = Path(root) / 'core'
                result = self.run_core(data, env=failing_listen(root, which))
                report = self.failure(data, result, 'bind', 'bind.port-in-use')
                self.assertIn('127.0.0.1:0', report['message'])
                self.assertFalse((data / 'local.sock').exists(), 'no socket left behind')

    def test_a_socket_taken_when_uvicorn_listens_on_it(self):
        """The late Unix-socket case: after the TCP server and the app's lifespan started. The lifespan ends in
        full — nothing is cancelled under the event loop's teardown — and nothing this start bound is left."""
        with tempfile.TemporaryDirectory() as root:
            data = Path(root) / 'core'
            result = self.run_core(data, env=failing_listen(root, 2, 'unix'))
            report = self.failure(data, result, 'bind', 'bind.port-in-use')
            self.assertIn(str(data / 'local.sock'), report['message'])
            self.assertFalse((data / 'local.sock').exists())
            log = (Path(root) / 'core.log').read_text()
            self.assertNotIn('CancelledError', result.stderr + log)
            self.assertIn('Application shutdown complete', log)

    def test_a_data_directory_that_is_a_link_is_not_written_through(self):
        """However the link is spelled on the command line: as given, or with a trailing `/`, `/.` or `//` — which
        a path lookup would follow through to its target."""
        for suffix in ('', '/', '/.', '//'):
            with self.subTest(spelling=f'core{suffix}'), tempfile.TemporaryDirectory() as root:
                elsewhere = Path(root) / 'elsewhere'
                elsewhere.mkdir(mode=0o700)
                (elsewhere / 'core-failure.json').write_text('somebody else\'s')
                data = Path(root) / 'core'
                data.symlink_to(elsewhere)
                result = self.run_core(str(data) + suffix)
                self.assertEqual(result.returncode, 0)
                self.assertIn('identity.unsafe-directory', result.stderr, 'the cause is in the log')
                self.assertEqual((elsewhere / 'core-failure.json').read_text(), 'somebody else\'s', 'untouched')
                self.assertEqual(sorted(p.name for p in elsewhere.iterdir()), ['core-failure.json'], 'nothing else either')

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
            self.failure(data, self.run_core(data), 'bind', 'bind.core-running', ready_file=True, status=75)
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



class LogTests(unittest.TestCase):
    def test_the_log_rotates_at_five_megabytes_and_keeps_two(self):
        """`log_to` as `main` sets it up, in a process of its own (it takes over the process's logging)."""
        with tempfile.TemporaryDirectory() as root:
            code = (f'from sidevoice_core.server.__main__ import log_to\nfrom loguru import logger\nimport logging\n'
                    f'log_to({str(Path(root) / "core.log")!r})\n'
                    f'for i in range(16000): logger.info("{{}} {{}}", i, "x" * 1000)\n'
                    f'logging.getLogger("uvicorn.error").info("the standard library\'s too")\n')
            result = subprocess.run([sys.executable, '-c', code], capture_output=True, text=True, timeout=120)
            self.assertEqual((result.returncode, result.stdout, result.stderr), (0, '', ''))
            files = sorted(Path(root).iterdir())
            self.assertEqual(len(files), 3, [f.name for f in files])
            self.assertIn(Path(root) / 'core.log', files)
            for file in files:
                self.assertLessEqual(file.stat().st_size, 5_000_000 + 2000)
                self.assertEqual(stat.S_IMODE(file.stat().st_mode), 0o600)
            current = (Path(root) / 'core.log').read_text()
            self.assertIn('15999 ', current)
            self.assertIn("uvicorn.error: the standard library's too", current)


class ServeLifecycleTests(unittest.IsolatedAsyncioTestCase):
    """`serve()` itself, where a start that fails late is unwound: the app's lifespan ends in full while this core
    still holds its directory, then the socket and ready file go, then the lock — and no task outlives it."""

    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.data = Path(self.temp.name) / 'core'
        for signum in (signal.SIGTERM, signal.SIGINT):   # serve() installs its own
            self.addCleanup(signal.signal, signum, signal.getsignal(signum))
        environment = patch.dict(os.environ)
        environment.start()
        self.addCleanup(environment.stop)

    def lock(self):
        """Whether another core could take this directory now: a lock of its own, on its own descriptor."""
        descriptor = os.open(self.data / 'core.lock', os.O_RDWR)
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return 'held'
        finally:
            os.close(descriptor)
        return 'free'

    async def failing_start(self, failure, *patches):
        """`serve()` failing as `patches` make it; what the lock was at the lifespan's last step, and after."""
        seen = []
        last_step = Room.stop

        async def stopping(room):
            seen.append(self.lock())
            await last_step(room)
            seen.append(self.lock())
        with patch.object(Room, 'stop', stopping):
            for each in patches:
                each.start()
            try:
                with self.assertRaises(failure) as raised:
                    await core.serve(core.options(['--port', '0', '--data-dir', str(self.data), '--idle-exit', '0']))
            finally:
                for each in patches:
                    each.stop()
        self.assertEqual(seen, ['held', 'held'], 'the lifespan ran to its end, and the directory was held throughout')
        self.assertEqual(self.lock(), 'free', 'then let go')
        self.assertFalse((self.data / 'local.sock').exists())
        self.assertFalse((self.data / 'core.json').exists())
        self.assertEqual([task for task in asyncio.all_tasks() if task is not asyncio.current_task()], [],
                         'no task outlives serve()')
        return raised.exception

    async def test_the_socket_failing_at_uvicorn_s_listen(self):
        async def taken(listener, sockets=None):
            raise OSError(errno.EADDRINUSE, os.strerror(errno.EADDRINUSE))
        failure = await self.failing_start(core.StartFailure, patch.object(LocalListener, 'startup', taken))
        self.assertEqual((failure.step, failure.key), ('bind', 'bind.port-in-use'))
        self.assertIn(str(self.data / 'local.sock'), failure.message)

    async def test_the_ready_file_failing_to_be_written(self):
        failure = await self.failing_start(core.StartFailure, patch.object(core, 'write_ready', side_effect=OSError(
            errno.ENOSPC, os.strerror(errno.ENOSPC))))
        self.assertEqual((failure.step, failure.key, failure.status), ('start', 'start.failed', 0))
        self.assertEqual(failure.message, f'OSError: [Errno {errno.ENOSPC}] {os.strerror(errno.ENOSPC)}')


if __name__ == '__main__':
    unittest.main()
