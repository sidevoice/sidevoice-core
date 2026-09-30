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


async def browser_call(room, websocket, config=None):
    """Every browser gets the same call, configured by what that browser brings in its first message."""
    from ..pipeline.settings import settings_from
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
    choice = transcription.resolve(settings, config)
    if choice['provider'] == 'openai' and not choice.get('available'):
        logger.info('A browser was refused: OpenAI has no API key in this room')
        await websocket.send_text(json.dumps({'type': 'error', 'data': {
            'message': 'OpenAI necesita una clave de API antes de conectar.'}}))
        await websocket.close(code=1008)
        return
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
        await websocket.accept()
        await browser_call(room, websocket, config)


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


def create_app(room=None, *, config=None, link_options=None, rendezvous=None):
    """The whole node. `room` defaults to one whose durable state lives in the node's data directory;
    `rendezvous` is this node's link with the hosted room, when it has one (`server.__main__` makes it)."""
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
    mount_connector_link(app, room, **(link_options or {}))
    mount_browser_call(app, room, config)
    mount_webrtc(app, room)
    mount_rendezvous(app, room, rendezvous)
    instrument(app)
    app.add_middleware(OwnHostsOnly)
    return app


def mount_rendezvous(app, room, rendezvous):
    """What this node is, for a page or a shell deciding where it is (`GET /api/rendezvous`), and — when
    it has one — its link with the hosted room, started and stopped with the app."""
    from contextlib import asynccontextmanager
    from .rendezvous import read_pairing
    app.state.rendezvous = rendezvous

    @app.get('/api/rendezvous')
    async def what_this_is():
        pairing = read_pairing(rendezvous.pairing_path) if rendezvous and rendezvous.pairing_path else None
        told = (room.control.identity if room.control else None) or {}
        return {'kind': 'node', 'id': pairing and pairing['connector_id'], 'host': told.get('host'),
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
