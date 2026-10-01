"""Device pairing over HTTP: the node requires a device token on
every route but a few, redeems pairing secrets, proves its identity, and lists and revokes devices.

The logic — the key, the code, the registry — is `control.devices`; this module only maps it to requests.
"""
import json
import socket
from urllib.parse import urlsplit

from fastapi import HTTPException, Request
from loguru import logger
from pydantic import BaseModel, Field

from ..control.devices import NodeDevices, valid_nonce
from .local import is_local, local_only, scope_path
from .transport import credential_safe

SUBPROTOCOL = 'sidevoice'
TOKEN_SUBPROTOCOL = 'sidevoice.token.'
UNPAIRED_CLOSE = 4401
DEVICE_KEY = 'sidevoice.device'    # the scope key the authenticated device's id travels under

# What needs no device token (the contract's table): what this node is, the redemption itself (it carries the
# one-time secret), the identity proof (public), preflights, and the room's dialling link (it carries its own key).
# The open answers say no more than pairing needs: the node's key and fingerprint, never its host name or ids.
# What only the local socket serves (`server.local`) is open too, and only there: reaching the socket is this OS
# user's authority, which a token would add nothing to.
OPEN_ROUTES = frozenset({('GET', '/api/rendezvous'), ('POST', '/api/device/pair'), ('GET', '/api/device/identity')})
OPEN_MOUNTS = ('/api/rendezvous/link',)

UNPAIRED = 'Este dispositivo no está emparejado con esta máquina, o se revocó: empareja de nuevo.'
UNPAIRED_REASON = 'Dispositivo no emparejado con esta máquina.'   # a close reason fits in 123 bytes
REFUSED_SECRET = 'Ese código no vale: no existe, ya se usó o ha caducado. Pide uno nuevo en la máquina.'


def call_subprotocol(scope):
    """The subprotocol a call socket is accepted with: `sidevoice` when the client offered it (a browser that
    offers subprotocols refuses an answer naming none), else none."""
    return SUBPROTOCOL if SUBPROTOCOL in (scope.get('subprotocols') or ()) else None


def bearer(headers):
    value = dict(headers or []).get(b'authorization', b'').decode('latin-1').strip()
    scheme, _, token = value.partition(' ')
    return token.strip() if scheme.lower() == 'bearer' and token.strip() else None


def socket_token(scope):
    return next((offered[len(TOKEN_SUBPROTOCOL):] for offered in scope.get('subprotocols') or ()
                 if offered.startswith(TOKEN_SUBPROTOCOL)), None)


class DeviceAuth:
    """A device token on every request and socket, except the open ones. Sits inside `OwnHostsOnly` (a foreign
    Host is refused before anything) and `CrossOrigin` (preflights are answered, and a 401 carries the CORS
    headers an accepted page needs to read it).

    A socket without a valid token is accepted and closed at once with 4401: closed before its handshake
    completes, a real client only sees an HTTP 403 and code 1006, never the 4401 the page acts on."""

    def __init__(self, app, devices):
        self.app, self.devices = app, devices

    async def __call__(self, scope, receive, send):
        kind = scope['type']
        if kind not in {'http', 'websocket'}:
            return await self.app(scope, receive, send)
        path = scope_path(scope)
        if any(path == mount or path.startswith(mount + '/') for mount in OPEN_MOUNTS):
            return await self.app(scope, receive, send)
        if is_local(scope) and local_only(path):
            return await self.app(scope, receive, send)
        if kind == 'http' and (scope['method'] == 'OPTIONS' or (scope['method'], path) in OPEN_ROUTES):
            return await self.app(scope, receive, send)
        token = bearer(scope.get('headers')) if kind == 'http' else socket_token(scope)
        device_id = self.devices.store.registry.authenticate(token) if token else None
        if device_id is not None:
            scope[DEVICE_KEY] = device_id
            return await self.app(scope, receive, send)
        if kind == 'websocket':
            if (await receive()).get('type') != 'websocket.connect':
                return
            await send({'type': 'websocket.accept', 'subprotocol': call_subprotocol(scope)})
            await send({'type': 'websocket.close', 'code': UNPAIRED_CLOSE, 'reason': UNPAIRED_REASON})
            return
        await send({'type': 'http.response.start', 'status': 401,
                    'headers': [(b'content-type', b'application/json'), (b'www-authenticate', b'Bearer')]})
        await send({'type': 'http.response.body', 'body': json.dumps({'detail': UNPAIRED}).encode('utf8')})


class DevicePairing:
    """What the web surface pairs with: the node's store, and what a code says about where the node is."""

    def __init__(self, store, room, rendezvous, config):
        self.store, self.room, self.rendezvous, self.config = store, room, rendezvous, config
        self.listen_url = None   # the node's own address, once it listens (`server.__main__`)
        self.calls = {}          # device id -> the call sockets it has open

    def call_opened(self, device_id, websocket):
        if device_id:
            self.calls.setdefault(device_id, set()).add(websocket)

    def call_closed(self, device_id, websocket):
        if device_id and device_id in self.calls:
            self.calls[device_id].discard(websocket)
            if not self.calls[device_id]:
                del self.calls[device_id]

    async def end_calls(self, device_id):
        """A revoked device's open calls end at once, with the same close as a refused token."""
        for websocket in list(self.calls.pop(device_id, ())):
            try:
                await websocket.close(code=UNPAIRED_CLOSE, reason=UNPAIRED_REASON)
            except Exception:
                pass

    def host(self):
        told = (self.room.control.identity if self.room.control else None) or {}
        return told.get('host') or socket.gethostname()

    def advertisable(self, url):
        """Whether a code may send a device's pairing secret and token to `url` (`server.transport`)."""
        if credential_safe(url, self.config):
            return True
        logger.warning('Not putting {} in a pairing code: a credential may go there only over https, '
                       'unless it is loopback or a trusted cluster host', url)
        return False

    def urls(self):
        """Where this node answers directly: its listen URL, then `SIDEVOICE_PUBLIC_URLS`; each one https, or
        plaintext only where `server.transport` allows it."""
        urls = [self.listen_url] if self.listen_url else []
        for url in str(self.config.get('SIDEVOICE_PUBLIC_URLS') or '').split(','):
            url = url.strip().rstrip('/')
            if url and urlsplit(url).scheme in {'http', 'https'} and url not in urls:
                urls.append(url)
        return [url for url in urls if self.advertisable(url)]

    def rv(self):
        """The room a device can reach this node through, if a credential may travel to it."""
        room = self.rendezvous.room_for_devices() if self.rendezvous is not None else None
        return room if room and self.advertisable(room['url']) else None

    def issue_code(self):
        return self.store.issue_code(host=self.host(), urls=self.urls(), rv=self.rv())

    def public_identity(self):
        """What anyone may learn without a token: the key a code's fingerprint names, nothing about the machine."""
        identity = self.store.identity
        return {'fingerprint': identity.fingerprint, 'public_key': identity.public_key}

    def node(self):
        """What a device that redeemed a code learns: the key it pins, and the machine's name to call it by."""
        return {**self.public_identity(), 'host': self.host()}


class Redeem(BaseModel):
    secret: str = Field(min_length=1, max_length=200)
    name: str | None = Field(default=None, max_length=200)


def mount_devices(app, room, rendezvous, config):
    """The pairing routes. Returns the `DevicePairing` the rest of the node (the connector link, the
    middleware, `server.__main__`) reaches through `app.state.devices`."""
    from ..runtime import data_dir
    from .presentation import require_same_origin
    devices = app.state.devices = DevicePairing(NodeDevices(data_dir(config)), room, rendezvous, config)

    @app.post('/api/device/pair')
    async def pair(body: Redeem, request: Request):
        require_same_origin(request)
        redeemed = devices.store.registry.redeem(body.secret, body.name)
        if redeemed is None:
            raise HTTPException(403, REFUSED_SECRET)
        device_id, token = redeemed
        return {'device_id': device_id, 'token': token, 'node': devices.node()}

    @app.get('/api/device/identity')
    async def identity(request: Request, nonce: str = ''):
        require_same_origin(request)
        if not valid_nonce(nonce):
            raise HTTPException(400, 'nonce must be base64url of 16 to 64 bytes.')
        return {**devices.public_identity(), 'signature': devices.store.identity.sign(nonce)}

    @app.get('/api/device/devices')
    async def listing(request: Request):
        require_same_origin(request)
        return {'devices': devices.store.registry.listing(request.scope.get(DEVICE_KEY))}

    @app.delete('/api/device/devices/{device_id}')
    async def revoke(device_id: str, request: Request):
        require_same_origin(request)
        if not devices.store.registry.revoke(device_id):
            raise HTTPException(404, 'Ese dispositivo no está emparejado con esta máquina.')
        # Its token is refused from now on; a call it already has open ends now too (a revoked laptop must not
        # keep speaking and listening until it happens to disconnect).
        await devices.end_calls(device_id)
        return {'ok': True}

    return devices
