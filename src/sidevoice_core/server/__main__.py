"""`sidevoice-core` (or `python -m sidevoice_core.server`): serve this node on loopback.

This is what the machine's connector starts when a conversation first needs voice, and what it
finds running afterwards. The handshake is a file: once the server is listening, `core.json` in the
data directory says where (`port`, `url`), who (`pid`, `version`) and with which credential the
connector links (`connector_id`, `token`, mode 0600). It is removed when this process exits.

The core outlives the connector on purpose — a connector exits seconds after its last conversation
leaves, and a person may be mid-sentence in a call — and leaves on its own once nothing has used
it for a while: no connector linked and no client in a call for `--idle-exit` seconds.
"""
import argparse
import asyncio
import json
import os
import time
from pathlib import Path

DEFAULT_PORT = 8768
DEFAULT_IDLE_SECONDS = 600


def write_ready(path, facts):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + f'.{os.getpid()}.tmp')
    temporary.write_text(json.dumps(facts), encoding='utf8')
    os.chmod(temporary, 0o600)
    temporary.replace(path)


def remove_ready(path):
    """Only this process's own file: a newer core may already have written its own."""
    try:
        if json.loads(path.read_text(encoding='utf8')).get('pid') == os.getpid():
            path.unlink()
    except (OSError, ValueError):
        pass


def idle(room):
    control = room.control
    return not room.clients and not (control and control.peers)


async def watch_idle(room, server, seconds, *, every=5.0):
    quiet_since = time.monotonic()
    while not server.should_exit:
        await asyncio.sleep(min(every, seconds))
        if not idle(room):
            quiet_since = time.monotonic()
        elif time.monotonic() - quiet_since >= seconds:
            from loguru import logger
            logger.info('No connector and no call for {:.0f}s; this core exits', seconds)
            server.should_exit = True


async def serve(arguments):
    import uvicorn
    from loguru import logger
    from ..control.connectors import PROTOCOL, local_credential
    from ..control.history import RoomHistory
    from ..control.room import Room
    from ..runtime import version
    from .app import create_app
    data = Path(arguments.data_dir)
    # Everything below reads the data directory from the environment when it needs it (the provider
    # keys included), so it is said once, here, where the process is assembled.
    os.environ['SIDEVOICE_CORE_DATA_DIR'] = str(data)
    room = Room(RoomHistory(data / 'room-state.json'))
    connector_id, token = local_credential(room.journal, data / 'connector-credential.json')
    app = create_app(room)
    server = uvicorn.Server(uvicorn.Config(app, host=arguments.host, port=arguments.port, log_level='info'))
    ready = Path(arguments.ready_file) if arguments.ready_file else data / 'core.json'
    serving = asyncio.create_task(server.serve())
    try:
        while not server.started:
            if serving.done():
                await serving   # it could not bind: uvicorn has said why
                raise SystemExit(1)
            await asyncio.sleep(0.02)
        port = server.servers[0].sockets[0].getsockname()[1]
        write_ready(ready, {'pid': os.getpid(), 'port': port, 'url': f'http://127.0.0.1:{port}',
                            'version': version(), 'protocol': PROTOCOL,
                            'connector_id': connector_id, 'token': token})
        logger.info('Sidevoice core {} listening on 127.0.0.1:{} (data in {})', version(), port, data)
        watcher = asyncio.create_task(watch_idle(room, server, arguments.idle_exit)) if arguments.idle_exit else None
        await serving
        if watcher:
            watcher.cancel()
    finally:
        remove_ready(ready)


def main(argv=None):
    from ..runtime import data_dir
    parser = argparse.ArgumentParser(prog='sidevoice-core',
                                     description="Sidevoice core: this node's conversations and one voice pipeline per call.")
    parser.add_argument('--host', default='127.0.0.1')
    parser.add_argument('--port', type=int, default=int(os.environ.get('SIDEVOICE_CORE_PORT') or DEFAULT_PORT),
                        help=f'loopback port (default {DEFAULT_PORT}; 0 picks a free one)')
    parser.add_argument('--data-dir', default=str(data_dir()))
    parser.add_argument('--ready-file', default=None, help='where to say it is listening (default <data-dir>/core.json)')
    parser.add_argument('--idle-exit', type=float,
                        default=float(os.environ.get('SIDEVOICE_CORE_IDLE_SECONDS') or DEFAULT_IDLE_SECONDS),
                        help='seconds with no connector and no call before exiting (0: never)')
    arguments = parser.parse_args(argv)
    try:
        asyncio.run(serve(arguments))
    except KeyboardInterrupt:
        pass


if __name__ == '__main__':
    main()
