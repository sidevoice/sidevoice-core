"""The core as the connector starts it: a process that says where it listens in a file, lets this
machine's connector link with the credential in that file, and leaves once nothing uses it."""
import asyncio
import json
import os
import stat
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

import socketio

from sidevoice_core.control.connectors import PROTOCOL


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


class NodeProcessTest(unittest.IsolatedAsyncioTestCase):
    def start(self, data, idle='2'):
        process = subprocess.Popen([sys.executable, '-m', 'sidevoice_core.server', '--port', '0',
                                    '--data-dir', str(data), '--idle-exit', idle],
                                   stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
        self.addCleanup(lambda: process.poll() is None and process.kill())
        return process

    async def link(self, facts, **auth):
        client = socketio.AsyncClient(reconnection=False)
        welcome = asyncio.get_running_loop().create_future()
        client.on('connector.welcome', lambda data: welcome.done() or welcome.set_result(data), namespace='/connectors')
        await client.connect(facts['url'], namespaces=['/connectors'], transports=['websocket'],
                             socketio_path='/api/connectors/link',
                             auth={'connector_id': facts['connector_id'], 'token': facts['token'],
                                   'protocol': PROTOCOL, 'host': 'test', **auth})
        return client, await asyncio.wait_for(welcome, 10)

    async def test_the_ready_file_carries_the_link_and_the_core_leaves_when_nothing_uses_it(self):
        with tempfile.TemporaryDirectory() as root:
            data = Path(root) / 'core'
            process = self.start(data)
            facts = await until(lambda: ready(data / 'core.json'))
            self.assertEqual(stat.S_IMODE(os.stat(data / 'core.json').st_mode), 0o600, 'it holds a credential')
            self.assertEqual(facts['pid'], process.pid)
            self.assertEqual(facts['url'], f"http://127.0.0.1:{facts['port']}")
            self.assertEqual(facts['protocol'], PROTOCOL)
            client, welcome = await self.link(facts)
            self.assertEqual(welcome, {'protocol': PROTOCOL})
            # A linked connector keeps the core alive past its idle budget.
            await asyncio.sleep(3)
            self.assertIsNone(process.poll(), 'a core with a connector linked does not leave')
            await client.disconnect()
            await until(lambda: process.poll() is not None, timeout=20)
            self.assertFalse((data / 'core.json').exists(), 'the file leaves with the process')
            # Started again, it keeps the same credential: a connector reconnecting needs no new one.
            again = self.start(data, idle='30')
            second = await until(lambda: ready(data / 'core.json'))
            self.assertEqual(second['pid'], again.pid)
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


if __name__ == '__main__':
    unittest.main()
