"""This computer's own way in: the node's app served a second time, on a Unix socket only this OS user can open
(`local.sock` in the node's data directory, mode 0600 inside a 0700 directory).

The trust boundary is the OS user. A process of this user can already read everything this node keeps, so the
socket hands it, with no code and no token, what loopback TCP must never hand anyone: the desktop app's device
token (`POST /api/device/local/pair`) and this machine's connector link, the credential that can mint pairing
codes. Another OS user cannot open the socket; a web page and a room's relay reach only TCP. What came through the
socket is said by the listener, never by the request: `LocalListener` marks every request it serves
(`scope['sidevoice.local']`), and `LocalOnly` answers what is served only with that mark as if it did not exist
anywhere else — the same 404 an unknown route gets.

Everything else on the socket is the node as on TCP: every other route still wants a device token.
"""
import errno
import os
import socket
from contextlib import contextmanager
from pathlib import Path

import uvicorn
from pydantic import BaseModel, Field

MARKER = 'sidevoice.local'
# Served only with the mark: the readiness probe, the app's own pairing, and the connector link.
LOCAL_ONLY = ('/api/local', '/api/device/local', '/api/connectors/link')
NOT_FOUND = b'{"detail":"Not Found"}'


def is_local(scope):
    return scope.get(MARKER) is True


def local_only(path):
    """Whether `path` names something only the socket serves, however its slashes and `.` segments are spelled."""
    path = '/' + '/'.join(segment for segment in (path or '').split('/') if segment not in {'', '.'})
    return any(path == prefix or path.startswith(prefix + '/') for prefix in LOCAL_ONLY)


def scope_path(scope):
    path, root = scope.get('path') or '', scope.get('root_path') or ''
    return path[len(root):] if root and path.startswith(root) else path


def marked(app):
    """`app` as the socket's listener serves it: every request and socket carries the mark."""
    async def local_app(scope, receive, send):
        if scope['type'] in {'http', 'websocket'}:
            scope = {**scope, MARKER: True}
        await app(scope, receive, send)
    return local_app


def from_a_page(scope):
    """Whether a page sent this: browsers always say their Origin on what the socket serves (a fetch that is not a
    GET, every socket), native callers — the app's own code, the connector — never do."""
    return any(name == b'origin' for name, _ in scope.get('headers') or ())


class LocalOnly:
    """What is served only on the socket does not exist anywhere else: over TCP, and so through a room's relay
    (which makes its requests to this node over TCP), it gets the answer an unknown route gets — 404, or a socket
    closed before its handshake. Sits inside `OwnHostsOnly` and outside everything else, device auth included.

    Nor does it exist for a page that reaches the socket through the desktop app's proxy: the proxy refuses those
    paths itself, and anything carrying an Origin is refused here too, so the app's page never pairs, unpairs or
    links on its own."""

    def __init__(self, app):
        self.app = app

    async def __call__(self, scope, receive, send):
        if scope['type'] in {'http', 'websocket'} and local_only(scope_path(scope)) \
                and (not is_local(scope) or from_a_page(scope)):
            if scope['type'] == 'websocket':
                await send({'type': 'websocket.close', 'code': 1000})
                return
            await send({'type': 'http.response.start', 'status': 404,
                        'headers': [(b'content-type', b'application/json')]})
            await send({'type': 'http.response.body', 'body': NOT_FOUND})
            return
        await self.app(scope, receive, send)


class LocalPair(BaseModel):
    name: str | None = Field(default=None, max_length=200)


def mount_local(app):
    """The routes served only on the socket. `app.state.launch_id` is this launch (`--launch-id`, or one made
    up), so a reader can tell this core from one that answered before it."""
    from ..runtime import API, version
    app.state.launch_id = None

    @app.get('/api/local/health')
    async def health():
        """The readiness probe: which launch, which process, and what it is."""
        devices = app.state.devices
        identity = devices.store.identity
        return {'launch_id': app.state.launch_id, 'pid': os.getpid(), 'version': version(), 'api': API,
                'fingerprint': identity.fingerprint, 'public_key': identity.public_key, 'host': devices.host(),
                'calls': devices.open_calls()}

    @app.post('/api/device/local/pair')
    async def pair_local(body: LocalPair = LocalPair()):
        """This computer's app, paired with no code: the socket is the proof. The app it replaces — a reset, a
        second install — loses its token and its open calls now, so at most one local device exists."""
        devices = app.state.devices
        device_id, token, replaced = devices.store.registry.pair_local(body.name)
        for old in replaced:
            await devices.end_calls(old)
        return {'device_id': device_id, 'token': token, 'node': devices.node()}

    @app.delete('/api/device/local')
    async def unpair_local():
        """The app taking its own pairing back (an app-only reset): its token and its calls end."""
        devices = app.state.devices
        revoked = devices.store.registry.revoke_local()
        for device_id in revoked:
            await devices.end_calls(device_id)
        return {'ok': True, 'revoked': bool(revoked)}


class LocalSocket:
    """The socket's file, bound, 0600 and listening at once: from then on it answers, so no other starter can take
    it for a dead core's. A file left by a core that died is replaced; one a live core still answers on is not
    taken from it (EADDRINUSE). Only the holder of the data directory's lock (`server.__main__`) judges which."""

    def __init__(self, path):
        self.path = Path(path)
        if self.path.is_socket():
            probe = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            probe.settimeout(1)
            try:
                probe.connect(str(self.path))
            except (ConnectionRefusedError, FileNotFoundError):
                self.path.unlink(missing_ok=True)
            except OSError:
                pass   # it answers, slowly: still somebody's
            else:
                raise OSError(errno.EADDRINUSE, f'Another core is serving {self.path}')
            finally:
                probe.close()
        self.socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            self.socket.bind(str(self.path))
            # The directory already keeps everyone else out; the socket says so too.
            os.chmod(self.path, 0o600)
            self.inode = os.stat(self.path).st_ino
            self.socket.listen()
        except BaseException:
            self.socket.close()
            raise

    def remove(self):
        """This listener's file only: a newer core may already have bound its own."""
        try:
            if os.stat(self.path).st_ino == self.inode:
                self.path.unlink()
        except OSError:
            pass


class LocalListener(uvicorn.Server):
    """The socket's server: the node's same app, marked. A second listener, not a second node: the app's lifespan
    runs once, in the TCP server (`node`), so this one has none; signals are that server's to catch, and this one
    stops when it does."""

    def __init__(self, app, node, *, log_level='info'):
        super().__init__(uvicorn.Config(marked(app), lifespan='off', log_config=None, log_level=log_level))
        self.node = node

    @contextmanager
    def capture_signals(self):
        yield

    async def on_tick(self, counter):
        return await super().on_tick(counter) or self.node.should_exit
