"""The node's HTTP surface for its clients: the room's state, its history, its settings catalogues.

The room model is `sidevoice_core.control.room`; this module only exposes it. It serves no page:
whoever serves the web interface (the hosted room, a desktop shell) points it here, directly or
through a relay.
"""
import os
import re
import uuid
from urllib.parse import urlsplit

from ..control.room import Speech, client_error_report
from fastapi import HTTPException, Request
from pydantic import BaseModel, Field

THREAD_PATTERN = re.compile(r'^[A-Za-z0-9._:-]{1,200}$')


def allowed_origins():
    """Origins other than this node's own whose pages may use it: the room's public origin, and any a
    desktop shell or a relay is configured with (`SIDEVOICE_ALLOWED_ORIGINS`, comma-separated)."""
    named = [os.getenv('VOICE_PUBLIC_ORIGIN', '')] + os.getenv('SIDEVOICE_ALLOWED_ORIGINS', '').split(',')
    return {origin.strip().rstrip('/') for origin in named if origin.strip()}


def require_same_origin(request):
    """Browser-only endpoints: the Origin's host must be this node's host (scheme-agnostic, so a
    TLS proxy in front is fine), or a configured origin. Non-browser callers send no Origin."""
    origin = request.headers.get('origin')
    if not origin:
        return
    try:
        origin_host = urlsplit(origin).netloc.lower()
    except ValueError:
        origin_host = ''
    if origin.rstrip('/') in allowed_origins() or (origin_host and origin_host in {
            request.headers.get('host', '').lower(), request.url.netloc.lower()}):
        return
    raise HTTPException(403, 'Use the room from its own address.')


def require_room_page(request):
    """Endpoints that exist for the person in the room: a browser on the room's own page, and nothing else.
    Same-origin alone lets a call with no Origin through (a command line, a script), which is right for
    the room's data endpoints and wrong for handing out a pairing code or taking one away: reaching the
    address is not being in the room. The code is shown to the person; the person carries it to their
    machine, and only that same person may take the pairing back."""
    if not request.headers.get('origin'):
        raise HTTPException(403, 'This is done from the room\'s own page, not from an external client.')
    require_same_origin(request)


class TextMessage(BaseModel):
    text: str = Field(min_length=1, max_length=12000)
    session_id: str
    thread_id: str
    binding_id: str
    message_id: uuid.UUID


def mount_presentation(app, hub):
    from ..pipeline.settings import load_settings
    from contextlib import asynccontextmanager
    previous_lifespan = app.router.lifespan_context
    @asynccontextmanager
    async def room_lifespan(application):
        async with previous_lifespan(application) as state:
            await hub.start()
            try:
                yield state
            finally:
                await hub.stop()
    app.router.lifespan_context = room_lifespan

    def client_for(session_id):
        """Every browser-scoped endpoint resolves its own client, and never anyone else's."""
        client = hub.clients.get(session_id) if isinstance(session_id, str) else None
        return client if client and client.connected else None

    @app.get('/api/presentation/history')
    async def history(thread_id: str | None = None, session_id: str | None = None):
        messages = hub.journal.history(thread_id)
        # Asked by a browser in the call, each reply also says whether that browser can hear it again (#100).
        client = client_for(session_id) if session_id else None
        if client is not None:
            again = hub.replayable_rows(client)
            messages = [{**row, 'replayable': True} if row.get('id') in again else row for row in messages]
        return {'messages': messages}

    @app.get('/api/presentation/voice-catalog')
    async def voice_catalog(request: Request):
        require_same_origin(request)
        from ..pipeline.settings import CATALOG
        from ..pipeline import synthesis
        eleven = await synthesis.catalog()
        catalog = {**CATALOG,
                   'models': [{**item, 'provider': 'kokoro'} for item in CATALOG['models']]
                             + [{**item, 'provider': 'elevenlabs'} for item in eleven['models']],
                   'providers': {'elevenlabs': eleven}}
        return catalog

    @app.get('/api/presentation/synthesis')
    async def synthesis_settings(request: Request):
        require_same_origin(request)
        from ..pipeline import synthesis
        return {'credentials': synthesis.credential_state(), 'catalog': await synthesis.catalog()}

    @app.post('/api/presentation/synthesis/credential')
    async def synthesis_credential(payload: dict, request: Request):
        if not request.headers.get('origin'):
            raise HTTPException(403, 'Save the key from the room, not from an external client.')
        require_same_origin(request)
        from ..pipeline import synthesis
        try:
            key = payload.get('key')
            if key is None or not str(key).strip():
                synthesis.clear_key()
            else:
                await synthesis.verify(str(key).strip())
                synthesis.save_key(str(key))
        except ValueError as error:
            raise HTTPException(422, str(error)) from error
        return {'credentials': synthesis.credential_state(), 'catalog': await synthesis.catalog()}

    @app.post('/api/presentation/synthesis/preview')
    async def synthesis_preview(payload: dict, request: Request):
        require_same_origin(request)
        from ..pipeline import synthesis
        try:
            return await synthesis.synthesize(str(payload.get('text') or ''),
                                             model=str(payload.get('model') or ''),
                                             voice=str(payload.get('voice') or ''),
                                             speed=float(payload.get('speed', 1)))
        except (TypeError, ValueError) as error:
            raise HTTPException(422, str(error)) from error

    @app.get('/api/presentation/transcription')
    async def transcription_settings(request: Request):
        require_same_origin(request)
        from ..pipeline import transcription
        return {'catalog': transcription.CATALOG,
                'credentials': transcription.credential_state()}

    @app.get('/api/presentation/transcription/models')
    async def transcription_models(provider: str, request: Request):
        require_same_origin(request)
        from ..pipeline import transcription
        try:
            return await transcription.catalog(provider)
        except ValueError as error:
            raise HTTPException(400, str(error)) from error

    @app.post('/api/presentation/transcription/credential')
    async def transcription_credential(payload: dict, request: Request):
        if not request.headers.get('origin'):
            raise HTTPException(403, 'Save the key from the room, not from an external client.')
        require_same_origin(request)
        from ..pipeline import transcription
        provider = payload.get('provider')
        if provider not in transcription.PROVIDERS:
            raise HTTPException(400, 'Proveedor desconocido.')
        key = payload.get('key')
        try:
            if key is None or not str(key).strip():
                transcription.clear_key(provider)
            else:
                await transcription.verify(provider, str(key).strip())
                transcription.save_key(provider, str(key))
        except ValueError as error:
            raise HTTPException(422, str(error)) from error
        return {'credentials': transcription.credential_state()}

    @app.get('/api/presentation/languages')
    async def languages():
        # Defaults only: each device keeps its own settings and brings them when it connects.
        return load_settings().model_dump()

    @app.get('/api/presentation')
    async def state(session_id: str | None = None):
        return hub.snapshot(session_id)

    @app.get('/api/presentation/admission')
    async def admission():
        """Why the room would refuse a browser right now.

        A refusal travels in a frame and in a close code, and a proxy can lose both — the page then
        shows its own generic sentence while the room had written the real one (#63). This is the
        third way, and the one nothing in between rewrites: a page whose socket closed before it had
        a session asks here and reads what the room would have told it.
        """
        return hub.admission()

    @app.get('/api/presentation/latency')
    async def latency(session_id: str | None = None):
        # A latency trace is one browser's own measurements; nobody else's are returned.
        return hub.latency_snapshot(session_id)

    def available_participants(session_id=None):
        # `selected` is the asking browser's own choice; nobody else has one that matters to it.
        client = client_for(session_id)
        current = client.target if client else {}
        entries = hub.control.participants() if hub.control else [{**b, 'connected': False} for b in hub.journal.bindings()]
        reach = hub.control.reachability if hub.control else (lambda b: {'state': 'offline', 'detail': None})
        # A conversation runs on a machine; the row says which, by the name the machine gave when it paired.
        hosts = {c['id']: c.get('host') for c in hub.journal.paired_connectors()}
        return [{'thread_id': b['thread'], 'title': b.get('title') or ('Conversation ' + b['thread'][:8]),
                 'harness': b.get('harness'), 'available': b['connected'],
                 'machine': {'id': b.get('connector'), 'host': hosts.get(b.get('connector'))},
                 'capabilities': b.get('capabilities'),
                 # What it thinks with, as its harness records it; absent while no harness has said.
                 'engine': b.get('engine'),
                 'reach': reach(b),
                 'selected': b['thread'] == current.get('thread_id')} for b in entries]

    @app.get('/api/presentation/participants')
    async def participants(session_id: str | None = None):
        return {'participants': available_participants(session_id)}

    @app.post('/api/presentation/select')
    async def select_participant(payload: dict, request: Request):
        require_same_origin(request)
        thread_id = payload.get('thread_id', '')
        if not isinstance(thread_id, str) or not THREAD_PATTERN.match(thread_id):
            raise HTTPException(400, 'Invalid conversation identifier.')
        record = hub.journal.binding_for_thread(thread_id)
        if not record:
            raise HTTPException(409, 'That conversation is not connected. Enable voice from its task.')
        if not client_for(payload.get('session_id')):
            raise HTTPException(409, 'That browser is not in the room.')
        return await hub.select(payload['session_id'], thread_id, record.get('title'))

    @app.post('/api/presentation/cancel-input')
    async def cancel_input(payload: dict, request: Request):
        require_same_origin(request)
        client = client_for(payload.get('session_id'))
        if not client or not client.speaking or payload.get('revision') != client.turn_revision:
            raise HTTPException(409, 'The message has already ended; it cannot be cancelled.')
        client.cancelled_turn = client.turn_revision
        if client.on_browser_event:
            client.on_browser_event({'type':'voice-user-turn', 'data':{
                'phase':'cancelled', 'revision':client.turn_revision,
                'thread_id':client.turn_target.get('thread_id')}})
        return {'status':'cancelled'}

    @app.post('/api/presentation/close')
    async def close_channel(payload: dict, request: Request):
        require_same_origin(request)
        thread_id = payload.get('thread_id', '')
        if not isinstance(thread_id, str) or not THREAD_PATTERN.match(thread_id):
            raise HTTPException(400, 'Invalid identifier.')
        if not hub.journal.binding_for_thread(thread_id) and not hub.journal.history(thread_id):
            raise HTTPException(409, 'That conversation was not found.')
        return await hub.close_channel(thread_id)

    @app.post('/api/presentation/leave')
    async def leave(payload: dict, request: Request):
        require_same_origin(request)
        if not client_for(payload.get('session_id')):
            raise HTTPException(409, 'That browser is not in the room.')
        return await hub.deselect(payload['session_id'], payload.get('binding_id'))

    @app.post('/api/presentation/text')
    async def typed_message(payload: TextMessage, request: Request):
        require_same_origin(request)
        return await hub.send_text(payload.text, payload.session_id, payload.thread_id,
                                   payload.binding_id, str(payload.message_id))

    @app.post('/api/presentation/client-error')
    async def client_error(payload: dict, request: Request):
        require_same_origin(request)
        # A page whose call is gone (or was never joined) still has a beacon. Nothing in the room moves
        # because of this: it is a note for whoever reads the room afterwards.
        hub.client_errors.append(client_error_report(payload))
        return {'status': 'recorded'}

    @app.post('/api/presentation/browser-receipt')
    async def browser_receipt(payload: dict, request: Request):
        require_same_origin(request)
        # A receipt moves the browser that sent it and nothing else in the room.
        client = client_for(payload.get('session_id'))
        uid, rev = payload.get('utterance_id'), payload.get('revision')
        if not client:
            raise HTTPException(409, 'Stale utterance.')
        if payload.get('status') == 'skipped':
            if not isinstance(rev, int) or not await client.skipped(uid, rev):
                raise HTTPException(409, 'Stale utterance.')
            return {'status': 'skipped'}
        if payload.get('status') in {'cancelled_unplayed', 'cancelled_playing'}:
            if (not isinstance(rev, int)
                    or not client.browser_cancelled(uid, rev, payload['status'] == 'cancelled_playing')):
                raise HTTPException(409, 'Stale utterance.')
            return {'status': payload['status']}
        if not client.is_current(uid, rev):
            raise HTTPException(409, 'Stale utterance.')
        status = payload.get('status')
        if status == 'playback_finished':
            await client.playback_finished(uid, rev)
        elif status == 'failed':
            client.error = 'Voice failed in the browser. Check the room; it will not repeat by itself.'
            client.fail_active()
        elif status == 'playing':
            client.latency.browser(uid, payload.get('timings_ms'))
            client.telemetry.playback(uid, payload.get('timings_ms'))
            client.transition(uid, status)
        else:
            raise HTTPException(400, 'Invalid state.')
        return {'status': status}

    @app.post('/api/presentation/replay')
    async def replay_reply(payload: dict, request: Request):
        """Play a reply again from its bubble, for the browser asking (#100)."""
        require_same_origin(request)
        client = client_for(payload.get('session_id'))
        if not client or not client.connected:
            raise HTTPException(409, 'Entra en la llamada para escucharla.')
        history_id = payload.get('history_id')
        if not isinstance(history_id, str) or not history_id:
            raise HTTPException(422, 'Falta la respuesta.')
        return await hub.replay_one(client, history_id)

    @app.post('/api/presentation/speak')
    async def speak(payload: Speech, request: Request):
        require_same_origin(request)
        try:
            return await hub.publish(payload)
        except ValueError as error:
            raise HTTPException(409, str(error)) from error
