"""`sidevoice-core` (or `python -m sidevoice_core.server`): serve this node on loopback and on its local socket.

This is the core job the service manager runs (a LaunchAgent on macOS, a `systemd --user` unit on Linux) — or, on a
machine without one, what a connector starts on demand. The handshake is a file: once the server is listening on
both, `core.json` in the data directory says where (`port` and `url` for code-paired browsers, `socket` for this
computer's connector and app), who (`pid`, `version`, and `launch_id`: the launch it was given, or one it made up),
what it speaks (`api`, the client surface; `protocol`, the connector link) and with which credential the connector
links (`connector_id`, `token`, mode 0600). That file is "ready"; it is removed when this process exits.

**The exit status is the contract with the service manager**, the one rule launchd (`KeepAlive {SuccessfulExit
false, Crashed true}`) and systemd (`Restart=on-failure`) share:

- a start that fails before ready says why in `core-failure.json` (0600, `{launch_id, step, key, message, at}`) and
  exits **0**, so a failure that would only repeat is not restarted in a loop: the data directory is not this
  user's alone (`identity.unsafe-directory`), the node's identity cannot be read (`identity.unreadable`), the port or
  the socket is taken (`bind.port-in-use`), a module it needs is missing (`import.missing-module`), anything else
  (`start.failed`, with the exception's last line). So that a missing module is one of these, nothing that can fail
  to import is imported before the guard in `main`;
- another core holds this data directory (`bind.core-running`): the report, and exit **75**, so the manager tries
  again later — the one failure before ready that goes away by itself;
- SIGTERM or SIGINT: a clean exit, 0;
- a crash after ready is not caught: non-zero (or the signal), and the manager restarts it.

One data directory has one core. Before it touches anything there, a core takes an exclusive lock on
`core.lock` in it (and in the socket's directory, when that is elsewhere) and holds it until it has removed its
socket and its ready file. Holding it, the core clears the failure report a previous start left: nothing else does.
Only the lock's holder may decide that a socket file is a dead core's and replace it.

The core writes its own log, `core.log`, beside its data directory (`--log-file`), rotated at 5 MB with two old ones
kept; stdout and stderr are the manager's, for what a crash prints.

`--self-test` imports everything serving needs and says whether it could, in one line, without binding or writing
anything: what an installer runs on a staged runtime before committing to it.

A core started on demand outlives the connector on purpose — a connector exits seconds after its last
conversation leaves, and a person may be mid-sentence in a call — and leaves on its own once nothing has used it
for a while: no connector linked and no client in a call for `--idle-exit` seconds. The service manager's job runs
it with `--idle-exit 0`.
"""
import argparse
import asyncio
import errno
import json
import os
import signal
import sys
import time
import traceback
import uuid
from datetime import datetime, timezone
from pathlib import Path

DEFAULT_PORT = 8768
DEFAULT_IDLE_SECONDS = 600
SOCKET_NAME = 'local.sock'
FAILURE_FILE = 'core-failure.json'
LOCK_FILE = 'core.lock'
LOG_FILE = 'core.log'
LOG_ROTATION = '5 MB'
LOG_KEPT = 2
CORE_RUNNING = 75   # EX_TEMPFAIL: another core holds the directory; the manager tries again later
# What serving imports inside functions, beyond this package's own modules (each imported by `--self-test`).
DEPENDENCIES = ('uvicorn', 'aiohttp', 'yarl', 'socketio', 'cryptography.hazmat.primitives.asymmetric.ec', 'aiortc',
                'av', 'opentelemetry.sdk.trace', 'opentelemetry.exporter.otlp.proto.http.trace_exporter',
                'opentelemetry.instrumentation.fastapi')


class StartFailure(Exception):
    """A start that cannot go on, for a reason the service manager's reader can name: `key`, at `step` of the
    start; `status` is what the process exits with (0: not to be restarted)."""

    def __init__(self, step, key, message, status=0):
        super().__init__(message)
        self.step, self.key, self.message, self.status = step, key, message, status


def last_line(error):
    return traceback.format_exception_only(error)[-1].strip()


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


def claim(directory):
    """This process's exclusive hold on `directory` (a descriptor to keep open), or None when another core has it.
    flock: the kernel lets go of it with the process, however that process ends, so a dead core never leaves a
    lock behind that a new one would have to judge stale."""
    import fcntl
    descriptor = os.open(Path(directory) / LOCK_FILE, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
    try:
        fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        os.close(descriptor)
        return None
    except BaseException:
        os.close(descriptor)
        raise
    return descriptor


def bind(host, port):
    """The TCP listener, bound and listening here rather than in uvicorn, so a port in use is told apart from every
    other failure to start, and a port bound is a port held: no other starter can listen on it in between."""
    import socket
    family, kind, proto, _, address = socket.getaddrinfo(host, port, type=socket.SOCK_STREAM,
                                                         flags=socket.AI_PASSIVE)[0]
    listener = socket.socket(family, kind, proto)
    try:
        listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        listener.bind(address)
        listener.listen()
    except BaseException:
        listener.close()
        raise
    return listener


def taken(error, what):
    if isinstance(error, OSError) and error.errno == errno.EADDRINUSE:
        return StartFailure('bind', 'bind.port-in-use', f'{what} is in use: {error.strerror or error}')
    return None


def idle(app):
    """No call socket open (one still waiting for its first message counts) and no connector linked."""
    control = app.state.room.control
    return not app.state.devices.open_calls() and not (control and control.peers)


async def watch_idle(app, server, seconds, *, every=5.0):
    quiet_since = time.monotonic()
    while not server.should_exit:
        await asyncio.sleep(min(every, seconds))
        if not idle(app):
            quiet_since = time.monotonic()
        elif time.monotonic() - quiet_since >= seconds:
            from loguru import logger
            logger.info('No connector and no call for {:.0f}s; this core exits', seconds)
            server.should_exit = True


async def started(server, serving, what):
    while not server.started:
        if serving.done():
            try:
                await serving   # it could not start: uvicorn has said why
            except OSError as error:
                # The listener failed after the app's lifespan had started: it ends as uvicorn ends it when its own
                # bind fails, rather than being cancelled under the event loop's teardown.
                lifespan = getattr(server, 'lifespan', None)
                if lifespan is not None:
                    await lifespan.shutdown()
                raise taken(error, what) or error
            except SystemExit:   # uvicorn's own exit when the app's lifespan would not start: it logged why
                raise StartFailure('start', 'start.failed', f'{what}: the app did not start (see {LOG_FILE})')
            raise StartFailure('start', 'start.failed', f'{what}: the server stopped before it started')
        await asyncio.sleep(0.02)


def directories(paths):
    """`paths` as the directories they are: two spellings of one directory (an alias through a link above it) are
    one directory, locked once — a second lock on it through another descriptor would be refused by the first."""
    seen = {}
    for path in paths:
        found = os.stat(path)
        seen.setdefault((found.st_dev, found.st_ino), path)
    return list(seen.values())


async def serve(arguments):
    data, socket_path = Path(arguments.data_dir), Path(arguments.socket)
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
    ready = Path(arguments.ready_file) if arguments.ready_file else data / 'core.json'
    held = []
    tcp = local = server = listener = serving = local_serving = watcher = None
    is_ready = False
    try:
        for directory in directories((data, socket_path.parent)):
            descriptor = claim(directory)
            if descriptor is None:
                raise StartFailure('bind', 'bind.core-running', f'Another core is serving {socket_path}: it holds '
                                   f'{Path(directory) / LOCK_FILE}.', CORE_RUNNING)
            held.append(descriptor)
        # This directory is this core's now, and so is its last failure report: it was about a start that is over.
        (data / FAILURE_FILE).unlink(missing_ok=True)
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
        address = f'{arguments.host}:{arguments.port}'
        try:
            tcp = bind(arguments.host, arguments.port)
        except OSError as error:
            raise taken(error, address) or error
        try:
            local = LocalSocket(socket_path)
        except OSError as error:
            raise taken(error, str(socket_path)) or error
        # Logging is the process's (`main`): uvicorn's goes where everything else does.
        server = uvicorn.Server(uvicorn.Config(app, log_config=None, log_level='info'))
        listener = LocalListener(app, server, log_level='info')

        def stop(*_):
            server.should_exit = True

        # A supervisor's SIGTERM (or ^C) is the same clean exit as going idle. uvicorn handles the signal while it
        # serves and raises it again once it has stopped: under the default handler the process would die right
        # there, leaving `core.json` and the socket behind for the next start to trip over.
        for signum in (signal.SIGTERM, signal.SIGINT):
            signal.signal(signum, stop)
        serving = asyncio.create_task(server.serve(sockets=[tcp]))
        await started(server, serving, address)   # the app's lifespan runs here, once
        local_serving = asyncio.create_task(listener.serve(sockets=[local.socket]))
        await started(listener, local_serving, str(socket_path))
        port = tcp.getsockname()[1]
        write_ready(ready, {'pid': os.getpid(), 'port': port, 'url': f'http://127.0.0.1:{port}',
                            'socket': str(socket_path), 'launch_id': arguments.launch_id,
                            'version': version(), 'api': API, 'protocol': PROTOCOL,
                            'connector_protocols': [PROTOCOL, 3],
                            'connector_id': connector_id, 'token': token})
        is_ready = True
        logger.info('Sidevoice core {} listening on {}:{} and {} (data in {}, launch {})', version(), arguments.host,
                    port, socket_path, data, arguments.launch_id)
        rendezvous.base = f'http://127.0.0.1:{port}'
        app.state.devices.listen_url = rendezvous.base   # the first of a pairing code's `urls`
        await rendezvous.start()
        watcher = asyncio.create_task(watch_idle(app, server, arguments.idle_exit)) if arguments.idle_exit else None
        await serving
    except StartFailure:
        raise
    except Exception as error:
        if is_ready:
            raise   # a crash: the manager restarts a core that did serve
        if isinstance(error, ImportError):
            raise StartFailure('import', 'import.missing-module', str(error)) from error
        raise StartFailure('start', 'start.failed', last_line(error)) from error
    finally:
        # However the start or the run ended, in this order: both servers stop and are awaited — the TCP one runs
        # the app's lifespan, which ends in full rather than being cancelled under the event loop's teardown — then
        # the socket and the ready file go, and only then the hold on the directory: no other core can start while
        # this one is still unwinding.
        if watcher is not None:
            watcher.cancel()
        for running in (listener, server):
            if running is not None:
                running.should_exit = True
        await asyncio.gather(*(task for task in (watcher, local_serving, serving) if task is not None),
                             return_exceptions=True)
        if tcp is not None:
            tcp.close()
        if local is not None:
            local.socket.close()
            local.remove()
        remove_ready(ready)
        for descriptor in held:
            os.close(descriptor)


def failed(arguments, failure):
    """Why this start ended, where the supervisor reads it, and in the log beside it."""
    from ..storage import write_private_into
    report = {'launch_id': arguments.launch_id, 'step': failure.step, 'key': failure.key, 'message': failure.message,
              'at': datetime.now(timezone.utc).isoformat(timespec='milliseconds').replace('+00:00', 'Z')}
    try:
        from loguru import logger
        logger.error('This core (launch {}) could not start ({}): {}', arguments.launch_id, failure.key, failure.message)
    except ImportError:
        pass
    # Only into the data directory itself, this user's own: one that is a link to elsewhere, or someone else's, is
    # not written through (it was refused for that), and the cause stays in the log beside the supervisor's fallback.
    # One that is merely open to others still takes the report, which holds no secret: that is how its key is known.
    try:
        write_private_into(arguments.data_dir, FAILURE_FILE, json.dumps(report))
    except OSError as error:
        print(f'sidevoice-core: {FAILURE_FILE} not written ({error})', file=sys.stderr)
    print(f'sidevoice-core could not start ({failure.key}): {failure.message}', file=sys.stderr)
    return failure.status


def log_to(path):
    """This process's log: `path`, 0600, rotated at `LOG_ROTATION` with `LOG_KEPT` old ones kept, and nothing on
    stdout or stderr but what a crash prints there. The standard library's logging (uvicorn's) goes the same way."""
    import logging
    from loguru import logger
    Path(path).parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    logger.remove()
    logger.add(path, rotation=LOG_ROTATION, retention=LOG_KEPT, level='INFO', enqueue=False,
               opener=lambda name, flags: os.open(name, flags, 0o600))

    class Intercepted(logging.Handler):
        def emit(self, record):
            try:
                level = logger.level(record.levelname).name
            except ValueError:
                level = record.levelno
            logger.opt(exception=record.exc_info).log(level, '{}: {}', record.name, record.getMessage())
    logging.basicConfig(handlers=[Intercepted()], level=logging.INFO, force=True)


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


def options(argv=None):
    """The command line, with every path made absolute lexically — no link followed or resolved — once, here: the
    directory checked is the directory locked, written into and reported into, however it was spelled (relative,
    a trailing `/`, `/.`, `//`)."""
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
                        help='this launch, echoed in core.json, core-failure.json, /api/local/health and the log '
                             '(default: a new one)')
    parser.add_argument('--log-file', default=None,
                        help=f'the log, rotated at {LOG_ROTATION} (default {LOG_FILE} beside the data directory)')
    parser.add_argument('--room-credential', default=os.environ.get('SIDEVOICE_ROOM_CREDENTIAL'),
                        help="the machine's pairing with a room (the connector's credentials.json); none: no room")
    parser.add_argument('--idle-exit', type=float,
                        default=float(os.environ.get('SIDEVOICE_CORE_IDLE_SECONDS') or DEFAULT_IDLE_SECONDS),
                        help='seconds with no connector and no call before exiting (0: never)')
    parser.add_argument('--self-test', action='store_true',
                        help='import everything serving needs, print one JSON line, and exit (0: ok)')
    arguments = parser.parse_args(argv)
    arguments.data_dir = os.path.abspath(arguments.data_dir)
    arguments.socket = os.path.abspath(arguments.socket or os.path.join(arguments.data_dir, SOCKET_NAME))
    arguments.log_file = os.path.abspath(arguments.log_file or os.path.join(os.path.dirname(arguments.data_dir), LOG_FILE))
    arguments.launch_id = arguments.launch_id or str(uuid.uuid4())
    return arguments


def main(argv=None):
    arguments = options(argv)
    if arguments.self_test:
        return self_test()
    try:
        log_to(arguments.log_file)
        asyncio.run(serve(arguments))
    except KeyboardInterrupt:
        pass
    except ImportError as error:   # before `serve` could say so itself: the log's own module
        return failed(arguments, StartFailure('import', 'import.missing-module', str(error)))
    except StartFailure as failure:
        return failed(arguments, failure)
    except OSError as error:   # the log could not be opened
        return failed(arguments, StartFailure('start', 'start.failed', last_line(error)))
    return 0


if __name__ == '__main__':
    sys.exit(main())
