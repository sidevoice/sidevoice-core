"""`sidevoice-core` (or `python -m sidevoice_core.server`): serve this node on loopback and on its local socket.

This is what the machine's connector starts — under its login service, as a child it supervises — and what it
finds running afterwards. The handshake is a file: once the server is listening on both, `core.json` in the data
directory says where (`port` and `url` for code-paired browsers, `socket` for this computer's connector and app),
who (`pid`, `version`, and `launch_id`, the launch its supervisor named), what it speaks (`api`, the client
surface; `protocol`, the connector link) and with which credential the connector links (`connector_id`, `token`,
mode 0600). It is removed when this process exits.

A start that fails for a reason this process can name says so in `core-failure.json` (0600) before it exits:
`{launch_id, step, key, message, at}`, so the supervisor shows the cause rather than a log tail. The reasons:
the data directory is not this user's alone (`identity.unsafe-directory`), the node's identity cannot be read
(`identity.unreadable`), the port or the socket is taken (`bind.port-in-use`), a module it needs is missing
(`import.missing-module`). Anything else is an exit with a traceback in the log, as before. So that a missing
module is one of the named reasons, nothing that can fail to import is imported before the guard in `main`.

`--self-test` imports everything serving needs and says whether it could, in one line, without binding or writing
anything: what an installer runs on a staged runtime before committing to it.

The core outlives the connector on purpose — a connector exits seconds after its last conversation
leaves, and a person may be mid-sentence in a call — and leaves on its own once nothing has used
it for a while: no connector linked and no client in a call for `--idle-exit` seconds. A supervisor
starts it with `--idle-exit 0` and decides itself.
"""
import argparse
import asyncio
import errno
import json
import os
import signal
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

DEFAULT_PORT = 8768
DEFAULT_IDLE_SECONDS = 600
SOCKET_NAME = 'local.sock'
FAILURE_FILE = 'core-failure.json'
# What serving imports inside functions, beyond this package's own modules (each imported by `--self-test`).
DEPENDENCIES = ('uvicorn', 'aiohttp', 'yarl', 'socketio', 'cryptography.hazmat.primitives.asymmetric.ec', 'aiortc',
                'av', 'opentelemetry.sdk.trace', 'opentelemetry.exporter.otlp.proto.http.trace_exporter',
                'opentelemetry.instrumentation.fastapi')


class StartFailure(Exception):
    """A start that cannot go on, for a reason the supervisor can name: `key`, at `step` of the start."""

    def __init__(self, step, key, message):
        super().__init__(message)
        self.step, self.key, self.message = step, key, message


def write_ready(path, facts):
    from ..storage import write_private
    write_private(path, json.dumps(facts))


def remove_ready(path):
    """Only this process's own file: a newer core may already have written its own."""
    try:
        if json.loads(path.read_text(encoding='utf8')).get('pid') == os.getpid():
            path.unlink()
    except (OSError, ValueError):
        pass


def private_directory(path):
    """The directory this node keeps its secrets and its socket in: created 0700 when absent, refused when it is
    there and anyone but this user may enter it — the socket would hand them this machine's device tokens."""
    from ..storage import unsafe_directory
    try:
        Path(path).mkdir(mode=0o700, parents=True, exist_ok=True)
    except OSError:
        pass   # said below, as what is wrong with it
    problem = unsafe_directory(path)
    if problem:
        raise StartFailure('directory', 'identity.unsafe-directory', problem)


def bind(host, port):
    """The TCP listener, bound here rather than by uvicorn, so a port in use is told apart from every other
    failure to start."""
    import socket
    family, kind, proto, _, address = socket.getaddrinfo(host, port, type=socket.SOCK_STREAM,
                                                         flags=socket.AI_PASSIVE)[0]
    listener = socket.socket(family, kind, proto)
    try:
        listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        listener.bind(address)
    except BaseException:
        listener.close()
        raise
    return listener


def taken(error, what):
    if isinstance(error, OSError) and error.errno == errno.EADDRINUSE:
        return StartFailure('bind', 'bind.port-in-use', f'{what} is in use: {error.strerror or error}')
    return None


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


async def started(server, serving):
    while not server.started:
        if serving.done():
            await serving   # it could not start: uvicorn has said why
            raise SystemExit(1)
        await asyncio.sleep(0.02)


async def serve(arguments):
    data = Path(arguments.data_dir)
    socket_path = Path(arguments.socket).absolute() if arguments.socket else data.absolute() / SOCKET_NAME
    for directory in dict.fromkeys((data, socket_path.parent)):
        private_directory(directory)
    import uvicorn
    from loguru import logger
    from ..control.connectors import PROTOCOL, local_credential
    from ..control.devices import IdentityError
    from ..control.history import RoomHistory
    from ..control.room import Room
    from ..runtime import API, version
    from .app import create_app
    from .local import LocalListener, LocalSocket
    from .rendezvous import Rendezvous
    # Everything below reads the data directory from the environment when it needs it (the provider
    # keys included), so it is said once, here, where the process is assembled.
    os.environ['SIDEVOICE_CORE_DATA_DIR'] = str(data)
    room = Room(RoomHistory(data / 'room-state.json'))
    connector_id, token = local_credential(room.journal, data / 'connector-credential.json')
    # The link with the room needs this node's own address to relay to, known once it listens.
    rendezvous = Rendezvous(arguments.room_credential, None)
    app = create_app(room, rendezvous=rendezvous)
    app.state.launch_id = arguments.launch_id
    # The node's identity exists from its first start, and one that cannot be read stops it here, loudly:
    # every paired device pins it, so it is never replaced behind anyone's back.
    try:
        app.state.devices.store.identity
    except (IdentityError, OSError) as error:
        raise StartFailure('identity', 'identity.unreadable', str(error)) from error
    try:
        tcp = bind(arguments.host, arguments.port)
    except OSError as error:
        raise taken(error, f'{arguments.host}:{arguments.port}') or error
    try:
        local = LocalSocket(socket_path)
    except OSError as error:
        tcp.close()
        raise taken(error, str(socket_path)) or error
    server = uvicorn.Server(uvicorn.Config(app, log_level='info'))
    listener = LocalListener(app, server, log_level='info')

    def stop(*_):
        server.should_exit = True

    # A supervisor's SIGTERM (or ^C) is the same clean exit as going idle. uvicorn handles the signal while it
    # serves and raises it again once it has stopped: under the default handler the process would die right
    # there, leaving `core.json` and the socket behind for the next start to trip over.
    for signum in (signal.SIGTERM, signal.SIGINT):
        signal.signal(signum, stop)
    ready = Path(arguments.ready_file) if arguments.ready_file else data / 'core.json'
    serving = asyncio.create_task(server.serve(sockets=[tcp]))
    local_serving = None
    try:
        await started(server, serving)   # the app's lifespan runs here, once
        local_serving = asyncio.create_task(listener.serve(sockets=[local.socket]))
        await started(listener, local_serving)
        port = tcp.getsockname()[1]
        write_ready(ready, {'pid': os.getpid(), 'port': port, 'url': f'http://127.0.0.1:{port}',
                            'socket': str(socket_path), 'launch_id': arguments.launch_id,
                            'version': version(), 'api': API, 'protocol': PROTOCOL,
                            'connector_id': connector_id, 'token': token})
        logger.info('Sidevoice core {} listening on {}:{} and {} (data in {})', version(), arguments.host, port,
                    socket_path, data)
        rendezvous.base = f'http://127.0.0.1:{port}'
        app.state.devices.listen_url = rendezvous.base   # the first of a pairing code's `urls`
        await rendezvous.start()
        watcher = asyncio.create_task(watch_idle(room, server, arguments.idle_exit)) if arguments.idle_exit else None
        await serving
        if watcher:
            watcher.cancel()
    finally:
        listener.should_exit = True
        if local_serving is not None:
            await local_serving
        local.remove()
        remove_ready(ready)


def failed(arguments, failure):
    """Why this start ended, where the supervisor reads it, and in the log beside it."""
    from ..storage import write_private
    report = {'launch_id': arguments.launch_id, 'step': failure.step, 'key': failure.key, 'message': failure.message,
              'at': datetime.now(timezone.utc).isoformat(timespec='milliseconds').replace('+00:00', 'Z')}
    try:
        write_private(Path(arguments.data_dir) / FAILURE_FILE, json.dumps(report))
    except OSError as error:
        print(f'sidevoice-core: could not write {FAILURE_FILE}: {error}', file=sys.stderr)
    print(f'sidevoice-core could not start ({failure.key}): {failure.message}', file=sys.stderr)
    return 1


def self_test():
    """Every module this package has and every one serving imports later, imported now; one JSON line says how
    that went. Whatever an import prints goes to stderr, so stdout is that line alone."""
    import contextlib
    import importlib
    import pkgutil
    try:
        with contextlib.redirect_stdout(sys.stderr):
            import sidevoice_core
            own = [info.name for info in pkgutil.walk_packages(sidevoice_core.__path__, 'sidevoice_core.')
                   if not info.name.endswith('.__main__')]
            for name in (*DEPENDENCIES, *own):
                importlib.import_module(name)
    except ImportError as error:
        print(json.dumps({'ok': False, 'key': 'import.missing-module', 'message': str(error)}))
        return 1
    from ..runtime import version
    print(json.dumps({'ok': True, 'version': version()}))
    return 0


def main(argv=None):
    from ..runtime import data_dir
    parser = argparse.ArgumentParser(prog='sidevoice-core',
                                     description="Sidevoice core: this node's conversations and one voice pipeline per call.")
    parser.add_argument('--host', default=os.environ.get('SIDEVOICE_CORE_HOST') or '127.0.0.1',
                        help='where to listen (default loopback; a node the room dials needs a reachable address and SIDEVOICE_ALLOWED_HOSTS)')
    parser.add_argument('--port', type=int, default=int(os.environ.get('SIDEVOICE_CORE_PORT') or DEFAULT_PORT),
                        help=f'loopback port (default {DEFAULT_PORT}; 0 picks a free one)')
    parser.add_argument('--data-dir', default=str(data_dir()))
    parser.add_argument('--socket', default=None,
                        help=f'the local socket, for this OS user only (default <data-dir>/{SOCKET_NAME})')
    parser.add_argument('--ready-file', default=None, help='where to say it is listening (default <data-dir>/core.json)')
    parser.add_argument('--launch-id', default=None,
                        help='the launch a supervisor names, echoed in core.json, core-failure.json and /api/local/health')
    parser.add_argument('--room-credential', default=os.environ.get('SIDEVOICE_ROOM_CREDENTIAL'),
                        help="the machine's pairing with a room (the connector's credentials.json); none: no room")
    parser.add_argument('--idle-exit', type=float,
                        default=float(os.environ.get('SIDEVOICE_CORE_IDLE_SECONDS') or DEFAULT_IDLE_SECONDS),
                        help='seconds with no connector and no call before exiting (0: never)')
    parser.add_argument('--self-test', action='store_true',
                        help='import everything serving needs, print one JSON line, and exit (0: ok)')
    arguments = parser.parse_args(argv)
    if arguments.self_test:
        return self_test()
    try:
        asyncio.run(serve(arguments))
    except KeyboardInterrupt:
        pass
    except ImportError as error:
        return failed(arguments, StartFailure('import', 'import.missing-module', str(error)))
    except StartFailure as failure:
        return failed(arguments, failure)
    return 0


if __name__ == '__main__':
    sys.exit(main())
