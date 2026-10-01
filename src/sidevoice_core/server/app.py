"""The node's web surface, assembled once: the image, the connector's supervisor and the tests all
get this same thing. No LLM lives here, and no room logic either — this module only carries.

What it carries: a client's call socket (`/api/presentation/ws`) into `control.calls.run_call`, the
node's REST surface (`presentation.py`), and this machine's connector link (`connector_link.py`).
"""
import asyncio
import json
import os

from fastapi import HTTPException, WebSocket
from fastapi.responses import JSONResponse
from loguru import logger
from pipecat.transports.websocket.fastapi import FastAPIWebsocketParams, FastAPIWebsocketTransport

from ..control.calls import run_call
from ..control.history import RoomHistory
from ..control.refusal import Refusal
from ..control.room import Room
from ..pipeline import transcription
from ..pipeline.serializer import BrowserFrameSerializer
from ..runtime import data_dir
from .devices import DEVICE_KEY, DeviceAuth, call_subprotocol, mount_devices
from .models import mount_models
from .presentation import mount_presentation, require_same_origin
from .webrtc import mount_webrtc

HELLO_TIMEOUT = 10.0


async def room_is_full(room, websocket):
    """Refusing one browser is not tearing the room down for the ones already in it.

    The reason is said twice — a frame, then the close code — and a tunnel can lose both: a phone
    read only the page's own "la sala rechazó la conexión" while this sentence was written for it
    (2026-09-22). The page asks `/api/presentation/admission` when neither arrived; the sentence and
    the name of the reason are the room's, in one place, so all three say the same thing.
    """
    admission = room.admission()
    if admission['admitted']:
        return False
    await websocket.send_text(json.dumps({'type': 'error', 'data': {
        'message': admission['message'], 'reason': admission['reason']}}))
    await websocket.close(code=1013)  # Try again later.
    return True


async def client_hello(websocket):
    """The browser's first message names the device's microphone settings and its transcription runtime."""
    try:
        event = await asyncio.wait_for(websocket.receive(), HELLO_TIMEOUT)
    except asyncio.TimeoutError:
        return {}
    if event.get('type') == 'websocket.disconnect':
        return None
    try:
        message = json.loads(event.get('text') or '{}')
    except ValueError:
        return {}
    data = message.get('data') if isinstance(message, dict) and isinstance(message.get('data'), dict) else {}
    return data


async def refuse_hello(websocket, refusal):
    """A hello this node cannot build a call from: why, as a key the page translates and an English sentence
    for one that does not know it, then a policy close — the same settings would be refused again."""
    await websocket.send_text(json.dumps({'type': 'error', 'data': dict(refusal)}))
    await websocket.close(code=1008)


async def browser_call(room, websocket, config=None):
    """Every browser gets the same call, configured by what that browser brings in its first message."""
    from ..pipeline.settings import settings_from, unavailable
    config = dict(os.environ) if config is None else config
    if await room_is_full(room, websocket):
        return
    hello = await client_hello(websocket)
    if hello is None:
        # A socket that opened and went away before saying anything leaves no other trace, and the
        # page blames the room for it (2026-09-22).
        logger.info('A browser opened a socket and left before its first message')
        return
    settings, problem = settings_from(hello.get('settings'))
    refusal = unavailable(settings, config)
    if refusal:
        logger.info('A browser was refused: {}', refusal['key'])
        await refuse_hello(websocket, refusal)
        return
    choice = transcription.resolve(settings, config)
    serializer = BrowserFrameSerializer()
    transport = FastAPIWebsocketTransport(websocket, FastAPIWebsocketParams(
        audio_in_enabled=True, serializer=serializer, allowed_origins=[]))

    async def refuse(message):
        await websocket.send_text(json.dumps({'type': 'error', 'data': {'message': message}}))
        await websocket.close(code=1013)  # Try again later.

    await run_call(room, transport, serializer, settings=settings, config=config, choice=choice,
                   hello=hello, refuse=refuse, settings_problem=problem)


def mount_browser_call(app, room, config=None):
    @app.websocket('/api/presentation/ws')
    async def browser_socket(websocket: WebSocket):
        try:
            require_same_origin(websocket)
        except HTTPException:
            await websocket.close(code=1008)  # Policy violation: not this room's own page.
            return
        # A page offers `sidevoice` beside its device token (browsers cannot set headers on a socket), and a
        # browser that offered subprotocols refuses an answer that names none.
        await websocket.accept(subprotocol=call_subprotocol(websocket.scope))
        devices = getattr(websocket.app.state, 'devices', None)
        device = websocket.scope.get(DEVICE_KEY)
        if devices:
            devices.call_opened(device, websocket)
        try:
            await browser_call(room, websocket, config)
        finally:
            if devices:
                devices.call_closed(device, websocket)


LOOPBACK_HOSTS = {'127.0.0.1', 'localhost', '::1'}


def allowed_hosts():
    """The names this node answers to besides loopback: `SIDEVOICE_ALLOWED_HOSTS` (comma-separated), for a
    node someone made reachable, and the hosts of the origins it already allows."""
    from urllib.parse import urlsplit
    from .presentation import allowed_origins
    named = {host.strip().lower() for host in os.getenv('SIDEVOICE_ALLOWED_HOSTS', '').split(',') if host.strip()}
    return LOOPBACK_HOSTS | named | {urlsplit(origin).hostname for origin in allowed_origins() if urlsplit(origin).hostname}


class OwnHostsOnly:
    """Every request and socket must name this node by a name it answers to.

    The node's surface has no login: it is this machine's, on loopback. What loopback alone does not stop
    is a web page the person visits rebinding its own domain to 127.0.0.1 — the browser then sends the
    page's own name as the Host and as the Origin, and the origin check, which compares the two, agrees.
    Refusing any Host that is not loopback (or configured) closes that door for every route, the mounted
    connector and rendezvous links included."""

    def __init__(self, app):
        self.app = app

    async def __call__(self, scope, receive, send):
        if scope['type'] in {'http', 'websocket'}:
            host = dict(scope.get('headers') or []).get(b'host', b'').decode('latin-1').strip().lower()
            name = host[1:host.index(']')] if host.startswith('[') and ']' in host else host.rsplit(':', 1)[0] if host.count(':') == 1 else host
            if name not in allowed_hosts():
                if scope['type'] == 'websocket':
                    await send({'type': 'websocket.close', 'code': 1008})
                    return
                await send({'type': 'http.response.start', 'status': 421,
                            'headers': [(b'content-type', b'application/json')]})
                await send({'type': 'http.response.body', 'body': b'{"detail": "This node answers to its own name only."}'})
                return
        await self.app(scope, receive, send)


class CrossOrigin:
    """CORS for the pages this node accepts from another origin (`page_origins`): a desktop shell's bundled
    interface, a configured room. A page on this node's own origin needs none, and an origin not accepted
    gets no header at all — its browser then refuses the answer, and the origin check refuses the request.
    Preflights are answered here; Chromium-based webviews also ask for the private network (the node is
    loopback), which the same origins are granted."""

    def __init__(self, app):
        self.app = app

    async def __call__(self, scope, receive, send):
        from .presentation import page_origins
        if scope['type'] != 'http':
            return await self.app(scope, receive, send)
        headers = dict(scope.get('headers') or [])
        origin = headers.get(b'origin', b'').decode('latin-1').rstrip('/')
        if not origin or origin not in page_origins():
            return await self.app(scope, receive, send)
        allow = [(b'access-control-allow-origin', origin.encode('latin-1')), (b'vary', b'Origin')]
        if scope['method'] == 'OPTIONS' and b'access-control-request-method' in headers:
            extra = [(b'access-control-allow-private-network', b'true')] \
                if headers.get(b'access-control-request-private-network') == b'true' else []
            await send({'type': 'http.response.start', 'status': 204, 'headers': allow + extra + [
                (b'access-control-allow-methods', b'GET, POST, PUT, PATCH, DELETE'),
                (b'access-control-allow-headers', b'content-type, accept, authorization'),
                (b'access-control-max-age', b'600')]})
            await send({'type': 'http.response.body', 'body': b''})
            return

        async def answered(message):
            if message['type'] == 'http.response.start':
                message = {**message, 'headers': [*(message.get('headers') or []), *allow]}
            await send(message)
        await self.app(scope, receive, answered)


def instrument(app):
    """FastAPI's own server spans, so a request to the node is in the same trace as the turn."""
    from ..control.telemetry import telemetry
    if not telemetry.enabled:
        return app
    try:
        from opentelemetry.instrumentation.fastapi import FastAPIInstrumentor
        FastAPIInstrumentor.instrument_app(app)
    except Exception as error:  # pragma: no cover - instrumentation is a nicety, never a requirement
        logger.warning('FastAPI instrumentation unavailable: {}', error)
    return app


def create_app(room=None, *, config=None, link_options=None, rendezvous=None, device_auth=True):
    """The whole node. `room` defaults to one whose durable state lives in the node's data directory;
    `rendezvous` is this node's link with the hosted room, when it has one (`server.__main__` makes it).

    `device_auth=False` is for tests only: a running node always requires a paired device's token
    (docs/DEVICE_PAIRING.md), and nothing in its environment can turn that off."""
    from fastapi import FastAPI
    from ..control.telemetry import configure as configure_telemetry
    from .connector_link import mount_connector_link
    config = dict(os.environ) if config is None else config
    if room is None:
        room = Room(RoomHistory(data_dir(config) / 'room-state.json'))
    # No schema, no playground: this node serves the endpoints its clients call, and FastAPI's
    # defaults would publish a map of all of them to anyone who asks (2026-09-21).
    app = FastAPI(title='Sidevoice core', docs_url=None, redoc_url=None, openapi_url=None)
    app.state.room = room

    @app.exception_handler(Refusal)
    async def refused(request, error: Refusal):
        return JSONResponse({'detail': error.detail}, status_code=error.status_code)

    # With no OTEL_EXPORTER_OTLP_ENDPOINT this starts nothing at all.
    configure_telemetry(environ=config)
    mount_presentation(app, room)
    mount_models(app)
    mount_connector_link(app, room, **(link_options or {}))
    mount_browser_call(app, room, config)
    mount_webrtc(app, room)
    devices = mount_devices(app, room, rendezvous, config)
    mount_rendezvous(app, room, rendezvous)
    instrument(app)
    if device_auth:
        app.add_middleware(DeviceAuth, devices=devices)   # inside CrossOrigin: preflights answered, a 401 readable
    app.add_middleware(CrossOrigin)
    app.add_middleware(OwnHostsOnly)   # added last, so it runs first: a Host this node is not is refused before anything
    return app


def mount_rendezvous(app, room, rendezvous):
    """What this node is, for a page or a shell deciding where it is (`GET /api/rendezvous`), and — when
    it has one — its link with the hosted room, started and stopped with the app."""
    from contextlib import asynccontextmanager
    from .rendezvous import read_pairing
    app.state.rendezvous = rendezvous

    from fastapi import Request
    from pydantic import BaseModel, Field
    from .presentation import require_room_page

    class PairWithRoom(BaseModel):
        room: str = Field(min_length=1, max_length=2048)
        code: str = Field(min_length=1, max_length=64)

    @app.post('/api/rendezvous/pair')
    async def pair_with_room(request: Request, body: PairWithRoom):
        """Pair this machine with a room, from a page talking to this node directly (a desktop shell): the
        same act as the connector's `voice_pair`, with the code the person read from that room. Pairing is
        the connector's to do — it owns `credentials.json` — so it is asked to; the rendezvous then follows
        the new pairing on its own. Only from a page (an Origin this node accepts), never a bare script."""
        require_room_page(request)
        peers = list(room.control.peers.values()) if room.control else []
        if not peers:
            raise Refusal(503, 'El conector de esta máquina no está conectado a su núcleo: no hay quien empareje.')
        try:
            answer = await peers[-1].request('pair.request', {'room': body.room.strip(), 'code': body.code.strip()}, timeout=25)
        except TimeoutError:
            raise Refusal(504, 'El conector no respondió a tiempo al emparejamiento.')
        answer = answer if isinstance(answer, dict) else {}
        if not answer.get('ok'):
            raise Refusal(400, answer.get('detail') or 'El emparejamiento falló.')
        if rendezvous is not None:
            rendezvous.poke()
        return {'ok': True, 'room': answer.get('origin'), 'connector_id': answer.get('connector_id')}

    @app.get('/api/rendezvous')
    async def what_this_is():
        pairing = read_pairing(rendezvous.pairing_path) if rendezvous and rendezvous.pairing_path else None
        told = (room.control.identity if room.control else None) or {}
        return {'kind': 'node', 'id': pairing and pairing['connector_id'], 'host': told.get('host'),
                'fingerprint': app.state.devices.store.identity.fingerprint,
                'room': rendezvous.state if rendezvous else None}

    if rendezvous is None:
        return
    rendezvous.control = room.control
    rendezvous.on_state = room.control.rendezvous_changed
    rendezvous.mount_dial(app)
    previous_lifespan = app.router.lifespan_context

    @asynccontextmanager
    async def rendezvous_lifespan(application):
        async with previous_lifespan(application) as state:
            try:
                yield state
            finally:
                await rendezvous.stop()
    app.router.lifespan_context = rendezvous_lifespan
