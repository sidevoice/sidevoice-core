"""Control plane for outbound connectors: pairing, presence, delivery and speech intake.

It knows nothing about Claude, Codex or browser audio. A connector pairs once,
authenticates each connection, registers a binding per conversation it serves,
receives the room's queued input for those bindings and returns exact-event
acknowledgements; speech it publishes lands in the room like any other.
The room's journal is the only delivery state; nothing here is a second outbox.

What carries the events is not this module's business: it talks to a *peer* — one
connector, reachable — and `connector_socketio` is what implements it.
"""
import asyncio
import json
import re
import time
import uuid
from fastapi import HTTPException, Request
from loguru import logger
from pydantic import BaseModel, Field

from .telemetry import redelivered

PROTOCOL = 2
HEARTBEAT_SECONDS = 15.0
HEARTBEAT_MISSES = 2
ACK_TIMEOUT_SECONDS = 60.0
THREAD_PATTERN = re.compile(r'^[A-Za-z0-9._:-]{1,200}$')
HARNESS_CAPABILITIES = ('deliver', 'inspectInbound', 'working', 'endOfTurn', 'sessionIdentity')
CAPABILITY_STATES = {'supported', 'unsupported'}


def engine_of(value):
    """Which model answers this conversation, as its harness read it — from its own launch line before
    it has answered once, and from what the harness records about the conversation after. Absent rather
    than guessed: no model is asked to say what it is."""
    if not isinstance(value, dict):
        return None
    kept = {key: str(value[key])[:60] for key in ('model', 'effort', 'thinking') if value.get(key)}
    return kept or None


def harness_capabilities(value):
    """Normalize the wire declaration. Missing and invalid values stay unknown, never false."""
    declared = value if isinstance(value, dict) else {}
    return {name: declared.get(name) if declared.get(name) in CAPABILITY_STATES else 'unknown'
            for name in HARNESS_CAPABILITIES}


class ConnectorPeer:
    """One connector, reachable — whatever carries the events.

    `send` says something and moves on. `request` asks, and answers with that connector's
    acknowledgement of *this* event; it raises `TimeoutError` when the answer never came inside
    the budget it was given, and anything else when the question could not be put at all. The
    room acts on that difference — an unanswered delivery is backed off, an unasked one goes
    straight back into the outbox — which is why the difference is in the interface.
    """

    async def send(self, event, data):
        raise NotImplementedError

    async def request(self, event, data, *, timeout):
        raise NotImplementedError

    async def disconnect(self):
        """A newer connection from the same connector won: let this one go."""


class PairingRequest(BaseModel):
    """The code, and what the machine says about itself. Everything but the code is the machine's own
    description and is kept as given: the room never fills any of it in."""
    code: str = Field(min_length=4, max_length=32)
    host: str = Field(default='', max_length=200)
    platform: str = Field(default='', max_length=60)
    version: str = Field(default='', max_length=40)
    harnesses: list[str] = Field(default_factory=list, max_length=8)


REVOKED_REASON = ('The room revoked this machine\'s pairing: pair it again with the code the room shows '
                  'under "Emparejar máquina" (Pair a machine).')


class RedemptionLimit:
    """How many wrong codes the room will hear before it stops listening for a while.

    A pairing code is 60 bits and lives three minutes; what makes that a wall rather than a budget is
    that guessing is cut off. The count is for the whole room, not per caller: behind a proxy a source
    address is whatever the last hop says, and this room has one user, for whom a lockout means
    waiting out the window rather than losing anything. A correct code is refused during the lockout
    too — silently accepting it would tell a guesser which attempts were the right ones."""

    def __init__(self, failures=10, window=600):
        self.failures, self.window, self.recent = failures, window, []

    def blocked(self, now=None):
        now = now if now is not None else time.time()
        self.recent = [at for at in self.recent if at > now - self.window]
        return len(self.recent) >= self.failures

    def failed(self, now=None):
        self.recent.append(now if now is not None else time.time())

    def retry_after(self, now=None):
        now = now if now is not None else time.time()
        return max(1, int(self.recent[0] + self.window - now)) if self.recent else 0


class ConnectorControl:
    def __init__(self, journal, hub, *, heartbeat_seconds=HEARTBEAT_SECONDS, ack_timeout=ACK_TIMEOUT_SECONDS):
        self.journal, self.hub = journal, hub
        self.peers = {}      # connector_id -> ConnectorPeer, one per live connection
        self.live = {}       # binding_id -> connector_id, only while that connector is connected
        self.inflight = {}   # binding_id -> (event_id, started_at, task): one delivery at a time per binding
        self.heartbeat_seconds, self.ack_timeout = heartbeat_seconds, ack_timeout
        self.pump_task = None

    async def start(self):
        self.pump_task = asyncio.create_task(self.pump())

    async def stop(self):
        if self.pump_task:
            self.pump_task.cancel()
            await asyncio.gather(self.pump_task, return_exceptions=True)

    # ----- presence -----

    def is_live(self, binding_id):
        return self.live.get(binding_id) in self.peers

    async def attach(self, connector_id, peer):
        """This connection now speaks for that connector; a newer one from the same connector wins."""
        previous = self.peers.get(connector_id)
        self.peers[connector_id] = peer
        if previous is not None and previous is not peer:
            await previous.disconnect()

    def detach(self, connector_id, peer):
        """That connection is gone. Its bindings stop being live, none of them is working any more —
        a conversation nobody can reach is not thinking, and the room would otherwise keep showing the
        last thing it was told for ever — and whatever was in flight goes back into the room's outbox,
        to be delivered again when the connector returns."""
        if not connector_id or self.peers.get(connector_id) is not peer:
            return
        del self.peers[connector_id]
        for binding_id in [b for b, c in self.live.items() if c == connector_id]:
            record = self.journal.binding(binding_id)
            del self.live[binding_id]
            if record:
                self.hub.clear_conversation_working(record['thread'])
            inflight = self.drop_inflight(binding_id)
            if inflight:
                self.journal.defer(inflight[0], immediate=True)

    def drop_inflight(self, binding_id):
        """Forget the delivery in flight for this binding and stop waiting for its acknowledgement —
        unless it is that very wait asking, which would be cancelling the ground under itself."""
        entry = self.inflight.pop(binding_id, None)
        if entry and entry[2] is not None and entry[2] is not asyncio.current_task():
            entry[2].cancel()
        return entry

    def participants(self):
        return [{**binding, 'capabilities': harness_capabilities(binding.get('capabilities')),
                 'connected': self.is_live(binding['id'])} for binding in self.journal.bindings()]

    @staticmethod
    def reachability(binding):
        """Three states, because 'connected' and 'will receive what you say' are not the same thing."""
        if not binding.get('connected'):
            return {'state': 'offline',
                    'detail': 'That conversation is not connected to the room. Ask it to connect.'}
        inbound = binding.get('inbound')
        if isinstance(inbound, str):
            try:
                inbound = json.loads(inbound)
            except ValueError:
                inbound = None
        if isinstance(inbound, dict) and inbound.get('ok') is False:
            return {'state': 'holding',
                    'detail': inbound.get('reason') or 'Su harness retiene lo que enviamos en vez de entregarlo.',
                    'remedy': inbound.get('remedy')}
        return {'state': 'listening', 'detail': None}

    # ----- delivery: the room's outbox drains through live bindings -----

    async def pump(self):
        while True:
            try:
                await self.tick()
            except asyncio.CancelledError:
                raise
            except Exception as error:
                logger.warning('Connector delivery tick failed: {}', type(error).__name__)
            await asyncio.sleep(.25)

    async def tick(self, now=None):
        # Nothing here watches the clock on a delivery already asked: the acknowledgement's budget
        # is the peer's to keep, and `settle` is what hears it run out. One clock, one owner.
        now = now if now is not None else time.time()
        # What the room has held too long stops being input before anything is chosen to send.
        for row in self.journal.expire_pending(now):
            self.hub.delivery_status(row['id'], 'not_sent')
        for row in self.journal.pending(now):
            binding = self.journal.binding_for_thread(row['thread'])
            if not binding or binding['id'] in self.inflight:
                continue
            connector_id = self.live.get(binding['id']) or ''
            peer = self.peers.get(connector_id)
            if peer is None:
                continue
            payload = json.loads(row['payload'] or '{}')
            data = {'event_id': row['id'], 'binding_id': binding['id'],
                    'thread': row['thread'], 'text': row['text'],
                    'channel': payload.get('channel', 'voice'), 'session_id': payload.get('session_id'),
                    'revision': payload.get('revision'), 'message_id': payload.get('message_id')}
            self.journal.update(row['id'], 'sending')
            self.inflight[binding['id']] = (row['id'], now, asyncio.create_task(
                self.settle(connector_id, binding, peer, data)))

    async def settle(self, connector_id, binding, peer, data):
        """Puts one delivery to a connector and waits for its acknowledgement. How it is carried is
        the peer's business; what the answer means is read in one place.

        The asking lives here, and not in the tick that chose it, so that a connection lost in
        between takes the whole question away with it rather than leaving one nobody will ask. A
        harness that never answered is backed off like one that refused — it may well be busy —
        while a delivery that could not be put at all goes straight back, because the next
        connector to hold this binding can have it now."""
        event_id = data['event_id']
        try:
            acknowledgement = await peer.request('input.deliver', data, timeout=self.ack_timeout)
        except asyncio.CancelledError:
            raise
        except Exception as error:
            entry = self.inflight.get(binding['id'])
            if entry and entry[0] == event_id:
                self.inflight.pop(binding['id'], None)
                if isinstance(error, TimeoutError):
                    self.journal.defer(event_id)
                else:
                    self.journal.defer(event_id, immediate=True)
                    redelivered(binding['thread'], binding.get('harness'))
            return
        if acknowledgement:
            await self.acknowledge(connector_id, event_id, acknowledgement)

    async def acknowledge(self, connector_id, event_id, message):
        """What one delivery's acknowledgement means. Which delivery it answers is the transport's
        to know — it carries the answer on the question — so the event id is passed in rather than
        read back out of what the connector said."""
        status = message.get('status')
        for binding_id, (inflight_event, _, _) in list(self.inflight.items()):
            if inflight_event != event_id:
                continue
            if self.live.get(binding_id) != connector_id:
                return  # Not this connector's delivery: ignore, never let a stranger settle it.
            self.drop_inflight(binding_id)
            row = self.journal.get(event_id)
            if row and row['status'] == 'read':
                # The conversation already admitted it (a hook said so, faster than this acknowledgement
                # came back): nothing the delivery path learns later can take the second tick away.
                return
            if status == 'accepted':
                self.journal.update(event_id, 'delivered')
                self.hub.delivery_status(event_id, 'delivered')
            elif status == 'unknown':
                # The harness offers no acknowledgement: it was written, and that is all
                # anyone knows. Retrying would duplicate without ever learning more.
                self.journal.update(event_id, 'unconfirmed', message.get('detail'))
                self.hub.delivery_status(event_id, 'unconfirmed')
            else:
                self.journal.defer(event_id)
                self.hub.delivery_status(event_id, 'pending')
                redelivered((row or {}).get('thread'), (self.journal.binding(binding_id) or {}).get('harness'))
            return

    async def read(self, connector_id, message):
        """A harness hook, through its connector, says the conversation admitted this message: the second tick."""
        binding = self.journal.binding(message.get('binding_id'))
        if not binding or self.live.get(binding['id']) != connector_id:
            return
        row = self.journal.find_message(message.get('message_id'))
        if not row or row['thread'] != binding['thread'] or row['role'] != 'user':
            return
        if row['status'] in {'read', 'not_sent'}:
            return
        self.journal.update(row['id'], 'read')
        self.hub.delivery_status(row['id'], 'read')

    async def working(self, connector_id, message):
        """The harness itself says whether that conversation is busy. The room shows it while it lasts."""
        binding = self.journal.binding(message.get('binding_id'))
        if (not binding or self.live.get(binding['id']) != connector_id
                or not isinstance(message.get('working'), bool)):
            return
        metadata = {}
        turn_id, turn_phase = message.get('turn_id'), message.get('turn_phase')
        if isinstance(turn_id, str) and turn_id and turn_phase in {'start', 'end'}:
            metadata.update(turn_id=turn_id, turn_phase=turn_phase)
            session_id, revision = message.get('session_id'), message.get('revision')
            if isinstance(session_id, str) and session_id and type(revision) is int and revision >= 0:
                metadata.update(session_id=session_id, revision=revision)
        self.hub.conversation_working(binding['thread'], message['working'], **metadata)

    async def engine(self, connector_id, message):
        """The harness says which model that conversation thinks with, observed where the harness records
        it. It replaces what the launch line said; a report that names no model changes nothing."""
        binding = self.journal.binding(message.get('binding_id'))
        if not binding or self.live.get(binding['id']) != connector_id:
            return
        engine = engine_of(message.get('engine'))
        if not engine or not engine.get('model'):
            return
        self.journal.set_binding_engine(binding['id'], engine)

    # ----- bindings and speech -----

    async def register(self, connector_id, message):
        """Returns what acknowledges `binding.register`, or raises ValueError with the reason it was
        refused — a rejection the transport says in its own words."""
        client_ref = message.get('client_ref')
        thread = message.get('thread')
        if not isinstance(thread, str) or not THREAD_PATTERN.match(thread):
            raise ValueError('Invalid conversation identifier')
        inbound = message.get('inbound') if isinstance(message.get('inbound'), dict) else None
        binding = self.journal.register_binding(connector_id, harness=str(message.get('harness') or 'unknown')[:40],
                                                thread=thread, title=(message.get('title') or None) and str(message['title'])[:200],
                                                binding_id=message.get('binding_id'), inbound=inbound,
                                                capabilities=harness_capabilities(message.get('capabilities')),
                                                engine=engine_of(message.get('engine')))
        self.live[binding['id']] = connector_id
        self.hub.clear_conversation_working(binding['thread'])
        # A conversation joining the room selects itself for nobody: which conversation a browser
        # talks to is that browser's choice (and the reason a call must never jump on a connect).
        return {'client_ref': client_ref, 'binding_id': binding['id'], 'thread': binding['thread']}

    async def close_binding(self, record):
        """The user closed this conversation's voice from the room: its connector forgets the binding."""
        connector_id = self.live.pop(record['id'], None)
        self.journal.deactivate_binding(record['connector'], record['id'])
        self.drop_inflight(record['id'])
        self.hub.clear_conversation_working(record['thread'])
        peer = self.peers.get(connector_id or '')
        if peer is not None:
            try:
                await peer.send('binding.close', {'binding_id': record['id'], 'thread': record['thread'],
                                                  'reason': 'closed_from_room'})
            except Exception:
                pass

    async def revoke(self, connector_id):
        """The person took this machine's pairing away from the room's page. It stops serving now, not
        on its next connection: the conversations it carried lose their voice the way they do when the
        room closes a channel, and the socket goes with the same reason its next handshake will get.

        Returns the conversations that lost their voice, which is what the page shows the person."""
        records = [binding for binding in self.journal.bindings() if binding['connector'] == connector_id]
        self.journal.revoke_connector(connector_id)
        for record in records:
            self.live.pop(record['id'], None)
            self.drop_inflight(record['id'])
            self.hub.clear_conversation_working(record['thread'])
        peer = self.peers.pop(connector_id, None)
        if peer is not None:
            for record in records:
                try:
                    await peer.send('binding.close', {'binding_id': record['id'], 'thread': record['thread'],
                                                      'reason': 'connector_revoked'})
                except Exception:
                    pass
            # Said before the socket goes, because after it there is nowhere to say it: a connector told
            # why stops asking and tells its conversations, instead of reading "io server disconnect".
            try:
                await peer.send('connector.revoked', {'reason': REVOKED_REASON})
            except Exception:
                pass
            try:
                await peer.disconnect()
            except Exception:
                pass
        return records

    async def unregister(self, connector_id, message):
        binding_id = message.get('binding_id')
        if self.live.get(binding_id) == connector_id:
            record = self.journal.binding(binding_id)
            del self.live[binding_id]
            self.journal.deactivate_binding(connector_id, binding_id)
            if record:
                self.hub.clear_conversation_working(record['thread'])
            inflight = self.drop_inflight(binding_id)
            if inflight:
                self.journal.defer(inflight[0], immediate=True)

    async def speech(self, connector_id, message):
        """Returns what acknowledges `speech.publish`. A refusal is that same answer with a status,
        not an error: the connector's outbox must be able to stop holding what the room will never
        take."""
        from .presentation import Speech
        reply = {'event_id': message.get('event_id')}
        binding = self.journal.binding(message.get('binding_id'))
        if not binding or self.live.get(binding['id']) != connector_id:
            return {**reply, 'status': 'rejected', 'error': 'Unknown binding'}
        try:
            speech = Speech(thread_id=binding['thread'], session_id=str(message.get('session_id') or ''),
                            revision=int(message.get('revision') or 0), text=str(message.get('text') or ''),
                            utterance_id=str(message.get('utterance_id') or uuid.uuid4()), language=message.get('language'))
            return {**reply, **await self.hub.publish(speech)}
        except HTTPException as error:
            return {**reply, 'status': 'rejected', 'error': str(error.detail)}
        except Exception as error:
            return {**reply, 'status': 'rejected', 'error': str(error) or type(error).__name__}

def mount_connector_control(app, hub, **options):
    redemption = options.pop('redemption_limit', {})
    control = ConnectorControl(hub.journal, hub, **options)
    hub.control = control
    from contextlib import asynccontextmanager
    previous_lifespan = app.router.lifespan_context

    @asynccontextmanager
    async def control_lifespan(application):
        async with previous_lifespan(application) as state:
            await control.start()
            try:
                yield state
            finally:
                await control.stop()
    app.router.lifespan_context = control_lifespan

    from .presentation import require_same_origin as browser_only, require_room_page
    from .connector_socketio import mount_connector_socketio

    mount_connector_socketio(app, control)

    @app.post('/api/connectors/pairing-code')
    async def pairing_code(request: Request):
        # Only the page in the room asks for a code, and it shows it to the person: never a client.
        require_room_page(request)
        return {'code': hub.journal.create_pairing_code(), 'expires_in': hub.journal.PAIRING_TTL}

    limit = RedemptionLimit(**redemption)

    @app.post('/api/connectors/pair')
    async def pair(payload: PairingRequest):
        if limit.blocked():
            raise HTTPException(429, 'Too many wrong codes; the room accepts no pairing for a few minutes.',
                                headers={'Retry-After': str(limit.retry_after())})
        credential = hub.journal.redeem_pairing_code(payload.code, payload.model_dump(exclude={'code'}))
        if credential is None:
            limit.failed()
            raise HTTPException(403, 'Pairing code invalid or expired.')
        return {'connector_id': credential[0], 'token': credential[1], 'protocol': PROTOCOL}

    @app.get('/api/connectors')
    async def connectors(request: Request):
        browser_only(request)
        return {'connectors': [{**c, 'connected': c['id'] in control.peers} for c in hub.journal.paired_connectors()],
                'bindings': control.participants()}

    @app.delete('/api/connectors/{connector_id}')
    async def revoke_connector(connector_id: str, request: Request):
        """Taking a machine's pairing away is the person's act, in the room, and nobody else's: the same
        guard as the code that granted it. Asked once it revokes — the machine stops serving now and stays
        listed as revoked, because a row that vanished would say nothing to whoever wonders why that
        machine went quiet. Asked again, it takes the row away."""
        require_room_page(request)
        paired = {row['id']: row for row in hub.journal.paired_connectors()}
        if connector_id not in paired:
            raise HTTPException(404, 'This room has no paired machine with that identifier.')
        if paired[connector_id]['revoked']:
            hub.journal.forget_connector(connector_id)
            return {'status': 'removed', 'connector_id': connector_id}
        records = await control.revoke(connector_id)
        return {'status': 'revoked', 'connector_id': connector_id,
                'threads': [record['thread'] for record in records]}

    return control
