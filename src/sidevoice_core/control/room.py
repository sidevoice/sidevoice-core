"""The room: one shared conversation, many browsers connected to it at once.

Room state is what everyone in the room shares — which conversation is selected,
the durable journal, the assistant's utterances and any audio the room paid a
provider to synthesize. Client state is what belongs to one browser alone: its
socket, its microphone turn, its transcription runtime, its playback queue, its
karaoke, its output device and its own latency trace.

Starting to speak is room-wide, because it makes the agent's previous answer
stale for everybody. Stopping the audio is local, because it only says what one
listener wants to hear. See `docs/MULTI_CLIENT_ROOM.md`.
"""
import asyncio
import json
import os
import time
import uuid
from collections import deque

from fastapi import HTTPException
from pipecat.frames.frames import InterruptionFrame

from .publication import PublicationClient, publication_decision
from .latency import CallLatency
from .pipeline_frames import PresentationBoundary, PresentationSpeech
from .synthesis_cache import SynthesisCache
from .telemetry import CallTelemetry

# A client's own playback verdict, past which no later event of its own may move it.
CLIENT_TERMINAL = {'interrupted', 'failed', 'disconnected', 'playback_finished'}
# What counts as *heard* by a browser, and is therefore never replayed to it when it comes back: the
# reply either ran to the end, or that listener stopped it on purpose. Everything else — queued,
# waiting, synthesizing, cut off mid-sentence when the socket went, failed — it never got through.
HEARD = {'playback_finished', 'interrupted'}
# Denials that mean "nobody on this conversation was listening", as opposed to "someone was and chose otherwise".
PARKABLE = {'session_changed', 'call_ended', 'focus_changed'}
# How many browsers one room carries at a time. What this bounds is the machine the room runs on, not
# the people in it and not the conversations: the room builds one Pipecat pipeline per browser, and
# each one loads a Silero VAD for the input plus a second detector and a Smart Turn v3 analyser for
# the turn end — two "Loading Silero VAD model" lines per call in the log — on top of the ~630 MB the
# worker already holds. One person with a laptop and a phone is two of these, and a device changing a
# setting that needs another pipeline is two for a moment.
#
# Eight was never what went wrong: seats that were never given back were (#63). So the default does
# not move, and a machine with room to spare raises it with VOICE_MAX_BROWSERS.
MAX_BROWSERS = 8


def browser_limit(environ=None):
    """How many browsers this room admits, as the machine running it was configured.

    Read where a room is built rather than at import, so a test — and a second room in one process —
    gets the environment it was given and not the one the module was first loaded in. Anything
    unreadable is the default: a room that carries fewer browsers than it can is a nuisance, and one
    that accepts more than its process can hold is the room falling over for everybody in it.
    """
    try:
        limit = int((os.environ if environ is None else environ).get('VOICE_MAX_BROWSERS'))
    except (AttributeError, TypeError, ValueError):
        return MAX_BROWSERS
    return max(1, limit)
# What the journal row says about an utterance: the furthest any listener got.
RANK = {'disconnected': 1, 'failed': 2, 'interrupted': 3, 'queued': 4,
        'waiting_for_turn': 5, 'waiting_for_pause': 5, 'synthesizing': 6,
        'playing': 7, 'playback_finished': 8}


class Utterance:
    """One assistant reply. The text and the epoch are the room's; playback is each client's."""

    def __init__(self, id, text, *, language=None, thread_id=None, revision=0, row_id=None, at=None):
        self.id, self.text, self.language = id, text, language
        self.thread_id, self.revision = thread_id, revision
        self.row_id = row_id
        self.at = time.time() if at is None else at   # when the room published it: what "recent enough" reads
        self.replay_of = None   # the reply this one repeats, when it is a catch-up rather than an answer
        self.requested = False  # a repetition the person asked for from the bubble, not a catch-up (#100)
        self.parked = False     # published when nobody on its conversation was listening: never rendered, never heard
        self.first_render = False  # a catch-up of a parked reply: its render is a first purchase, not a repeat
        self.rendered = False   # a paid engine was asked for it: repeating it must never buy it again
        self.clients = {}       # client id -> {'status': ..., 'reason': ...}
        self.published = None   # last (status, reason) written to the journal

    def _best(self):
        if not self.clients:
            return {'status': 'queued', 'reason': None}
        return max(self.clients.values(), key=lambda entry: RANK.get(entry['status'], 0))

    @property
    def status(self):
        return self._best()['status']

    @property
    def reason(self):
        return self._best()['reason']

    def result(self, session_id=None):
        return {'status': self.status, 'utterance_id': self.id,
                'session_id': session_id, 'revision': self.revision}

    def view(self, client_id):
        """What one browser is entitled to know about this utterance: its own playback."""
        entry = self.clients.get(client_id)
        return {'utterance_id': self.id, 'revision': self.revision, 'session_id': client_id,
                'status': entry['status'] if entry else 'queued'}

    def snapshot(self):
        return {'utterance_id': self.id, 'revision': self.revision, 'thread_id': self.thread_id,
                'status': self.status, 'replay_of': self.replay_of,
                'clients': {cid: entry['status'] for cid, entry in self.clients.items()}}


class RoomClient:
    """One browser. Owns its microphone turn and its playback queue, and nothing else's."""

    def __init__(self, session_id, room=None, *, worker=None, tts=None, stt=None):
        self.id = session_id
        self.room = room
        self.worker, self.tts, self.stt = worker, tts, stt
        self.connected = False
        self.closed = False
        self.speaking = False
        self.error = None
        self.sent = 0
        self.last_delivery = None
        # Only used when no journal is attached; the room's outbox is the durable path.
        self.input_queue = asyncio.Queue(maxsize=32)
        self.pending = deque()
        self.active = None
        self.quiet_until = 0
        self.dispatch_timer = None
        self.playback_watch = None   # the bound on the reply this browser was handed (#60)
        self.audio_grace_seconds = 1.0
        # Which conversation this browser talks to is this browser's own state (issue: the room
        # used to hold one selection for everyone). The room only routes.
        self.target = {}
        self.revision = 0          # this browser's turn epoch; only its own turns and selections advance it
        self.switching = False
        self.turn_target = {}
        self.turn_revision = 0
        self.turn_binding_id = None
        self.cancelled_turn = None
        self.on_input_receipt = None
        self.on_browser_event = None
        self.mic = None            # the serializer, when a PCM socket owns this client
        self.input_stats = None    # what the browser-side transcription transport reports
        self.transcription = None  # which STT engine this client resolved to
        self.mic_settings = None   # how this device's turns are detected
        self.voice = None          # the call flow driving this client's turns, when a pipeline owns it
        self.settings = None       # what this device configured; the room keeps no copy of its own
        self.audio_health = None   # the browser's last report about its audio output
        self.latency = CallLatency(self.id)
        # The same marks, said in OpenTelemetry. It reads this trace; it keeps no copy of it.
        self.telemetry = CallTelemetry(self.id, self.latency)
        if room is not None:
            room.join(self)

    # ----- identity and reporting -----

    @property
    def journal(self):
        return self.room.journal if self.room else None

    @property
    def utterances(self):
        return self.room.utterances if self.room else {}

    def input_receipt(self, payload, status):
        if payload.get('session_id', self.id) == self.id:
            if status == 'pending':
                self.latency.turn(payload['thread_id'], payload['revision'], 'queued')
            elif status == 'delivered':
                self.latency.turn(payload['thread_id'], payload['revision'], 'delivery_accepted')
            elif status == 'read':
                self.latency.turn(payload['thread_id'], payload['revision'], 'read')
            self.telemetry.receipt(payload['thread_id'], payload['revision'], status)
        if self.on_input_receipt:
            self.on_input_receipt({'revision': payload['revision'], 'history_id': payload.get('history_id'),
                                   'thread_id': payload['thread_id'],
                                   'session_id': payload.get('session_id', self.id), 'status': status})

    def identity(self):
        """What the room may say about a browser to the rest of the room."""
        return {'id': self.id, 'connected': self.connected, 'user_speaking': self.speaking,
                'turn_revision': self.turn_revision,
                'transport': (self.input_stats or {}).get('transport', 'pcm'),
                'transcription': self.transcription}

    def snapshot(self):
        return {'id': self.id, 'target': dict(self.target), 'connected': self.connected,
                'user_speaking': self.speaking, 'error': self.error, 'sent': self.sent,
                'last_delivery': self.last_delivery, 'revision': self.revision,
                'utterances': [u.view(self.id) for u in self.utterances.values() if self.id in u.clients],
                'tts': {'engine': 'Kokoro · browser'} if self.tts is None else getattr(self.tts, 'runtime_status', {}),
                'mic': ({
                    'frames': getattr(self.mic, 'audio_frames', 0),
                    'bytes': getattr(self.mic, 'audio_bytes', 0),
                    'last_gap_ms': getattr(self.mic, 'last_audio_gap_ms', 0),
                    'max_gap_ms': getattr(self.mic, 'max_audio_gap_ms', 0),
                    'gaps_over_250ms': getattr(self.mic, 'audio_gap_count', 0),
                } if self.mic else {}) | (self.input_stats or {}) or None,
                'mic_settings': self.mic_settings,
                'transcription': self.transcription,
                'audio_health': self.audio_health,
                'speech_filter': getattr(self.stt, 'filter_stats', {})}

    # ----- input this browser produced -----

    def enqueue_input(self, text, *, target=None, revision=None, message_id=None, history_id=None,
                      offline=None, at=None):
        """`offline` and `at` carry input this room did not hear as it happened: what a browser
        captured while its socket was down, with that browser's own clock. Delivery is unchanged —
        the harness gets the same envelope — and only the journal records where it came from."""
        if target is None and self.cancelled_turn == self.turn_revision:
            return
        target = self.turn_target if target is None else target
        revision = self.turn_revision if revision is None else revision
        history_id = history_id or self.id + ':user-turn:' + str(revision)
        if not text or not text.strip():
            return
        payload = {'thread_id': target.get('thread_id'), 'text': text,
                   'message_id': message_id or str(uuid.uuid4()), 'session_id': self.id,
                   'history_id': history_id, 'revision': revision, 'binding_id': target.get('binding_id'),
                   'title': target.get('title')}
        if not payload['thread_id']:
            self.error = 'Select a conversation before speaking.'
            self.input_receipt(payload, 'not_sent')
            return
        if self.journal:
            self.journal.put(id=history_id, thread=payload['thread_id'], role='user', text=text,
                             name='You', session=self.id, revision=revision, status='pending', payload=payload,
                             offline=offline, at=at)
        else:
            try:
                self.input_queue.put_nowait(payload)
            except asyncio.QueueFull:
                self.error = 'Queue full: the last message was not sent.'
                self.input_receipt(payload, 'not_sent')
                return
        self.input_receipt(payload, 'pending')
        return payload

    def user_started(self):
        """This browser's microphone opened a turn; the epoch it opens is the room's."""
        self.room.begin_turn(self)

    async def finish_user_turn(self):
        self.speaking = False
        self.room.quiet(self)
        await self.room.dispatch_all()

    # ----- playback, which is this browser's alone -----

    def is_current(self, uid, revision):
        utterance = self.utterances.get(uid)
        entry = utterance.clients.get(self.id) if utterance else None
        return bool(self.connected and not self.speaking and revision == self.revision
                    and self.active == uid and entry and entry['status'] not in CLIENT_TERMINAL)

    def transition(self, uid, status, reason=None):
        utterance = self.utterances.get(uid)
        entry = utterance.clients.get(self.id) if utterance else None
        if not entry or entry['status'] in CLIENT_TERMINAL:
            return
        previous = entry['status']
        entry['status'], entry['reason'] = status, reason
        # While a reply is coming out of this browser's speaker, only a voice clearly over it opens a turn.
        if self.voice is not None and hasattr(self.voice, 'listening_bar'):
            self.voice.listening_bar(status == 'playing')
        self.latency.status(uid, status)
        if status == 'playing':
            self.latency.mark(uid, 'playing_receipt')
        self.room.sync(utterance)
        # A catch-up that actually sounded says so on the reply it repeated, under this browser's own
        # id: without it the next return would ask the same question of the same reply and repeat it
        # again, because the entry it reads belongs to a session that no longer exists. One that never
        # left the queue — a new turn cancelled it — writes nothing: not playing it said nothing about
        # whether it was ever heard, and claiming otherwise would lose the reply for good.
        if utterance.replay_of and status in CLIENT_TERMINAL and (status == 'playback_finished' or previous == 'playing'):
            original = self.utterances.get(utterance.replay_of)
            if original is not None and self.id not in original.clients:
                original.clients[self.id] = {'status': status, 'reason': reason}
                self.room.sync(original)

    def halt(self, status, reason=None, *, preserve_waiting=False, announce=True):
        """Drop what this browser was going to play. Only this browser's entries move."""
        if self.dispatch_timer:
            self.dispatch_timer.cancel()
            self.dispatch_timer = None
        if announce and self.on_browser_event:
            self.on_browser_event({'type': 'voice-cancel', 'data': {'session_id': self.id, 'revision': self.revision}})
        waiting, dropped = [], 0
        for uid, utterance in self.utterances.items():
            entry = utterance.clients.get(self.id)
            if not entry:
                continue
            # A browser that renders its own audio can be handed again anything it never started
            # playing; a server-side pipeline cannot take it back. A catch-up is the exception and
            # is never held: cancelling it is exactly what a new turn is meant to do to it, and audio
            # the person has already moved past must not come back after they have spoken again.
            if (preserve_waiting and not utterance.replay_of
                    and (entry['status'] == 'waiting_for_turn' or self.on_browser_event
                         and entry['status'] in {'queued', 'synthesizing', 'waiting_for_pause'})):
                utterance.revision = self.revision
                self.transition(uid, 'waiting_for_turn', 'user_speaking')
                waiting.append(uid)
            else:
                dropped += entry['status'] not in CLIENT_TERMINAL
                self.transition(uid, status, reason)
        # A turn that interrupted nothing is not a cancellation: most halts find nothing to drop,
        # and counting them would make every turn look like one.
        if dropped:
            self.telemetry.cancelled(reason=reason or status, thread_id=self.target.get('thread_id'),
                                     revision=self.revision)
        self.pending.clear()
        self.pending.extend(waiting)
        self.active = None

    def disconnect(self):
        if self.playback_watch and not self.playback_watch.done():
            self.playback_watch.cancel()
        self.telemetry.call_ended('disconnected')
        if self.room:
            self.room.leave(self)
        else:
            self.connected, self.closed = False, True

    def fail_active(self):
        """An uncertain failure in this browser: nothing is replayed, and no one else is touched."""
        for uid in list(self.utterances):
            self.transition(uid, 'failed', 'playback_failed')
        self.pending.clear()
        self.active = None

    def browser_cancelled(self, uid, revision, started):
        """This listener stopped the audio. The message stays, and the others keep playing."""
        utterance = self.utterances.get(uid)
        entry = utterance.clients.get(self.id) if utterance else None
        if not utterance or not entry or revision > utterance.revision:
            return False
        if entry['status'] in CLIENT_TERMINAL:
            return True
        if started:
            self.transition(uid, 'interrupted', 'user_interrupted')
            self.pending = deque(item for item in self.pending if item != uid)
            if self.active == uid:
                self.active = None
                if self.on_browser_event:
                    self.on_browser_event({'type': 'voice-cancel', 'data': {'session_id': self.id, 'revision': self.revision}})
        elif revision == utterance.revision:
            self.transition(uid, 'waiting_for_turn', 'user_speaking')
            if uid not in self.pending:
                self.pending.appendleft(uid)
            if self.active == uid:
                self.active = None
        # An older unplayed cancellation cannot roll back a newer dispatch.
        return True

    async def skipped(self, uid, revision):
        """This listener skipped the reply on purpose: it is done for this browser, and the next one plays.

        Unlike an interruption there is no turn behind it, so nothing else stops and nothing waits: the
        reply is marked heard-enough (`interrupted`, `user_skipped`), never offered again, and the queue
        moves on at once instead of waiting for the person's next turn."""
        utterance = self.utterances.get(uid)
        entry = utterance.clients.get(self.id) if utterance else None
        if not utterance or not entry or revision > utterance.revision:
            return False
        if entry['status'] not in CLIENT_TERMINAL:
            self.transition(uid, 'interrupted', 'user_skipped')
        self.pending = deque(item for item in self.pending if item != uid)
        if self.active == uid:
            self.active = None
        await self.dispatch()
        return True

    async def playback_finished(self, uid, revision):
        if not self.is_current(uid, revision):
            return
        # Marker passed TTS serialization and transport audio queue. This is not
        # confirmation of browser playout, device audibility, or user comprehension.
        self.transition(uid, 'playback_finished')
        self.active = None
        await self.dispatch()

    async def _dispatch_after_pause(self, delay):
        await asyncio.sleep(delay)
        self.dispatch_timer = None
        await self.dispatch()

    def _queue_behind_active(self):
        # Nobody is talking: what is still held waits for the reply ahead of it, not for anybody's turn,
        # and saying otherwise tells the person to stop a voice nobody is using (#61).
        if not self.active or self.room.speaking:
            return
        for uid in self.pending:
            utterance = self.utterances.get(uid)
            entry = utterance.clients.get(self.id) if utterance else None
            if entry and entry['status'] == 'waiting_for_turn':
                self.transition(uid, 'queued', 'previous_reply')

    async def dispatch(self):
        self._queue_behind_active()
        if self.active or not self.pending or not self.connected or self.room.speaking:
            return
        remaining = self.quiet_until - asyncio.get_running_loop().time()
        if remaining > 0:
            self.transition(self.pending[0], 'waiting_for_pause', 'quiet_grace')
            if not self.dispatch_timer or self.dispatch_timer.done():
                self.dispatch_timer = asyncio.create_task(self._dispatch_after_pause(remaining))
            return
        uid = self.pending.popleft()
        self.active = uid
        self._queue_behind_active()
        utterance = self.utterances[uid]
        rev = utterance.revision
        try:
            if self.on_browser_event:
                await self.play_in_browser(utterance, rev)
                return
            await self.worker.queue_frames([
                PresentationBoundary(utterance_id=uid, revision=rev),
                PresentationSpeech(text=utterance.text, utterance_id=uid, revision=rev,
                                   language=utterance.language),
                PresentationBoundary(utterance_id=uid, revision=rev, end=True)])
        except Exception:
            self.fail_active()
            raise

    # A reply handed to a browser is not waited on for ever (#60): a killed tab, a network gone mid-playback
    # or a receipt lost in flight left the head of the queue "playing" and every later reply behind it. The
    # bound is generous and grows with the text — rendering in a slow browser included — and when it passes
    # without an ending receipt the reply is marked unconfirmed and the queue moves on.
    PLAYBACK_BASE_SECONDS = 60.0
    PLAYBACK_CHARS_PER_SECOND = 6.0

    def playback_bound(self, text):
        return self.PLAYBACK_BASE_SECONDS + len(text or '') / self.PLAYBACK_CHARS_PER_SECOND

    def watch_playback(self, utterance):
        if self.playback_watch and not self.playback_watch.done():
            self.playback_watch.cancel()
        uid, bound = utterance.id, self.playback_bound(utterance.text)

        async def expire():
            await asyncio.sleep(bound)
            entry = utterance.clients.get(self.id)
            if self.active != uid or not entry or entry['status'] in CLIENT_TERMINAL:
                return
            self.transition(uid, 'failed', 'unconfirmed')
            self.active = None
            await self.dispatch()

        self.playback_watch = asyncio.create_task(expire())

    async def play_in_browser(self, utterance, rev):
        """Hand this browser the reply to play. Kokoro it renders; a paid engine the room did."""
        from .language_settings import load_settings, resolve_voice
        uid = utterance.id
        choice = resolve_voice(self.settings or load_settings(), utterance.language)
        self.transition(uid, 'synthesizing')
        self.latency.start_synthesis(uid)
        trace = self.latency.replies.get(uid)
        reply_revision = trace['reply_revision'] if trace else rev
        common = {'session_id': self.id, 'revision': rev, 'utterance_id': uid,
                  'reply_revision': reply_revision, 'thread_id': self.target.get('thread_id'),
                  'text': utterance.text, 'history_id': utterance.row_id,
                  # The bubble has to say it is being repeated, or it reads as something just said.
                  **({'replay': True} if utterance.replay_of else {}),
                  # Asked for from the bubble: the page plays it even though it has heard it before.
                  **({'requested': True} if utterance.requested else {})}
        if choice['provider'] == 'kokoro':
            self.latency.mark(uid, 'audio_dispatched')
            self.telemetry.synthesis(uid, provider=choice['provider'], model=choice.get('model'))
            self.on_browser_event({'type': 'voice-speech', 'data': {**common, **choice}})
            self.watch_playback(utterance)
            return
        if utterance.replay_of and not utterance.first_render:
            # A paid engine renders once and the room keeps that render in a bounded cache. Repeating
            # what someone missed must not bill the account again, so a catch-up uses the render the
            # room already has or nothing at all: between the offer and this moment the cache may have
            # dropped it, and then this one is let go and the queue carries on. A parked reply is the
            # exception by definition: nobody ever heard it, so nothing was ever bought for it.
            audio, fresh = self.room.stored_audio(utterance, choice), False
            if audio is None:
                self.transition(uid, 'failed', 'replay_audio_gone')
                self.active = None
                await self.dispatch()
                return
        else:
            try:
                audio, fresh = await self.room.shared_audio(utterance, choice)
            except ValueError as error:
                self.fail_active()
                raise HTTPException(502, str(error)) from error
            original = self.room.utterances.get(utterance.replay_of) if utterance.replay_of else utterance
            if original is not None:
                original.rendered = True
        self.latency.mark(uid, 'audio_ready')
        # A listener that was handed someone else's render did not wait for the provider;
        # recording that request as its own would be a measurement it never made.
        self.latency.provider(uid, audio['timings_ms'] if fresh else {})
        self.latency.mark(uid, 'audio_dispatched')
        self.telemetry.synthesis(uid, provider=choice['provider'], model=choice.get('model'),
                                 shared=not fresh, provider_ms=audio['timings_ms'])
        self.on_browser_event({'type': 'voice-speech-audio', 'data': {
            **common, **choice, **audio,
            'timings_ms': audio['timings_ms'] if fresh else {}, 'shared': not fresh}})
        self.watch_playback(utterance)


def client_error_report(data, session_id=None):
    """One uncaught error in the interface, trimmed to what can be read from the room.

    The shape is owned here because two doors lead to it: a live call's socket and the beacon a page
    sends when it has no call. Never any transcript text — the message, its stack and which part of
    the page raised it."""
    data = data if isinstance(data, dict) else {}
    return {'session_id': session_id or (str(data.get('session_id'))[:64] or None if data.get('session_id') else None),
            'at': time.time(),
            'kind': str(data.get('kind') or '')[:40],
            'message': str(data.get('message') or '')[:400],
            'stack': str(data.get('stack') or '')[:2000],
            'component': str(data.get('component') or '')[:1000],
            'build': str(data.get('build') or '')[:40]}


def _build_info():
    from .paths import build_info
    return build_info()


class Room:
    """One conversation, one journal, one epoch — and as many browsers as people looking."""

    # What a browser over the limit is refused with, written once. The socket says it in a frame and
    # in a close code, and the admission endpoint says it again to a page that received neither
    # through its proxy (#63): three ways out, one sentence, and one name for the reason so a page
    # can say it in its own language when only the name survived the trip.
    FULL_MESSAGE = 'The room already has the maximum number of browsers connected.'
    MAX_UTTERANCES = 2048
    MAX_PENDING = 16
    # How many missed replies one browser is handed when it comes back. Coming out of a tunnel is
    # not an excuse to make somebody sit through a monologue: what is wanted is the last thing that
    # was said, and the recency setting is what really bounds this.
    MAX_REPLAY = 8

    def __init__(self, journal=None, assets=None, *, max_clients=None):
        self.max_clients = browser_limit() if max_clients is None else max(1, int(max_clients))
        self.clients = {}
        self.working = {}          # harness truth by thread; in-memory only, for browsers that select mid-turn
        self.sessions = deque(maxlen=64)   # ids we have known, so an older reply can be told apart
        self.utterances = {}
        self.audio_reports = deque(maxlen=30)   # browsers' reports about their audio output, kept past their leaving
        self.client_errors = deque(maxlen=20)   # uncaught errors in the interface: a phone has no console anyone can read
        self.journal = journal
        self.assets = assets if assets is not None else SynthesisCache()
        self.activation_lock = asyncio.Lock()
        self.control = None   # the connector control plane drains the journal; set when mounted

    async def start(self):
        self.journal.recover()

    async def stop(self):
        pass

    # ----- membership -----

    def join(self, client):
        if client.id in self.clients:
            return client
        if len(self.clients) >= self.max_clients:
            raise RuntimeError(self.FULL_MESSAGE)
        client.room = self
        self.clients[client.id] = client
        if client.id not in self.sessions:
            self.sessions.append(client.id)
        return client

    def admission(self):
        """Whether one more browser would be let in right now, and what it would be told if not."""
        full = len(self.clients) >= self.max_clients
        return {'admitted': not full, 'reason': 'room_is_full' if full else None,
                'message': self.FULL_MESSAGE if full else None,
                'clients': len(self.clients), 'max': self.max_clients}

    def leave(self, client):
        client.connected = False
        client.closed = True
        if self.clients.get(client.id) is client:
            del self.clients[client.id]
        # The room and every other browser survive this; the selection left with the browser that held it.
        client.halt('disconnected', 'call_ended', announce=False)

    def listeners(self):
        return [client for client in self.clients.values() if client.connected]

    @property
    def speaking(self):
        return any(client.speaking for client in self.clients.values())

    def conversation_working(self, thread_id, working, *, turn_id=None, turn_phase=None,
                             session_id=None, revision=None):
        """Told by the harness, not deduced from what was said: every browser on that conversation sees it."""
        self.working[thread_id] = working
        data = {'thread_id': thread_id, 'working': working}
        if turn_id is not None:
            data['turn_id'] = turn_id
        if turn_phase is not None:
            data['turn_phase'] = turn_phase
        if session_id is not None:
            data['session_id'] = session_id
        if revision is not None:
            data['revision'] = revision
        for client in self.audience(thread_id):
            if client.on_browser_event:
                client.on_browser_event({'type': 'voice-conversation', 'data': data})

    def clear_conversation_working(self, thread_id):
        self.working.pop(thread_id, None)

    def report_conversation_working(self, client):
        """Give a browser selecting mid-turn the current aggregate, without inventing a lifecycle event."""
        thread_id = client.target.get('thread_id')
        if thread_id in self.working and client.on_browser_event:
            client.on_browser_event({'type': 'voice-conversation',
                                     'data': {'thread_id': thread_id, 'working': self.working[thread_id]}})

    def audience(self, thread_id):
        """The connected browsers whose selected conversation is this one."""
        return [client for client in self.listeners() if thread_id and client.target.get('thread_id') == thread_id]

    # ----- a browser's turn epoch -----

    def begin_turn(self, client):
        """A browser's own turn interrupts that browser's playback and nobody else's."""
        client.revision += 1
        client.speaking = True
        client.turn_target = dict(client.target)
        client.turn_revision = client.revision
        client.turn_binding_id = client.target.get('binding_id')
        client.halt('interrupted', 'user_interrupted', preserve_waiting=True)

    def quiet(self, client):
        client.quiet_until = asyncio.get_running_loop().time() + client.audio_grace_seconds

    async def dispatch_all(self):
        await self.fan_out(self.listeners())

    async def fan_out(self, clients):
        """Every browser is dispatched at once, and a slow or broken one delays only itself.

        Dispatching concurrently is also what makes one paid render serve all of
        them: the second browser to ask finds the first one's render in flight.
        """
        outcomes = await asyncio.gather(*(client.dispatch() for client in clients), return_exceptions=True)
        for client, outcome in zip(clients, outcomes):
            if isinstance(outcome, asyncio.CancelledError):
                raise outcome
            if isinstance(outcome, BaseException):
                # That browser is already marked failed; the room and the rest carry on.
                client.error = getattr(outcome, 'detail', None) or (
                    'Could not play the audio in this browser: ' + type(outcome).__name__)

    def sync(self, utterance):
        status, reason = utterance.status, utterance.reason
        if (status, reason) == utterance.published:
            return
        utterance.published = (status, reason)
        # A catch-up writes nothing of its own: the journal already has one row for that reply, and
        # what repeating it did to one browser is said on that row through the reply itself.
        if self.journal and utterance.row_id and not utterance.replay_of:
            self.journal.update(utterance.row_id, status, reason)

    async def shared_audio(self, utterance, choice):
        """A paid engine is billed per character: one render per utterance, reused by every listener."""
        return await self.assets.obtain(choice, utterance.text)

    def stored_audio(self, utterance, choice):
        """The render the room already paid for, or None. Never asks a provider for a new one."""
        return self.assets.read(self.assets.key(choice, utterance.text))

    # ----- what a browser that came back never heard -----

    def missed_replies(self, client, *, seconds, sessions=()):
        """The replies on this browser's conversation that it never heard through, oldest first.

        The room does not guess at this: every utterance records what each browser did with it, and a
        browser that reconnects is a new client id, so the page names the ids it used before. Declaring
        a session can only take replies away from the answer, never add one, so a wrong id costs the
        person a repetition and can never hand them somebody else's.
        """
        thread = client.target.get('thread_id')
        if not thread or not seconds:
            return []
        mine = {client.id, *(session for session in sessions if isinstance(session, str))}
        floor = time.time() - seconds

        def left(entry):
            # Cut because this browser moved to another conversation: it was not heard, and the bubble
            # promises it again on return (#73), even if it had started.
            return entry is not None and entry['reason'] == 'focus_changed' and entry['status'] != 'playback_finished'

        missed = [utterance for utterance in self.utterances.values()
                  if utterance.thread_id == thread and not utterance.replay_of and utterance.at >= floor
                  and (client.id not in utterance.clients or left(utterance.clients[client.id]))
                  and not any(entry['status'] in HEARD and not left(entry)
                              for session, entry in utterance.clients.items() if session in mine)]
        return missed[-self.MAX_REPLAY:]

    async def replay(self, client, *, seconds=0, sessions=()):
        """Play a returning browser what it missed, oldest first, before anything new.

        Each one is queued as an utterance of its own, at this browser's current epoch and with no
        journal row: the reply's row already exists and says what it said. Being in the same queue as
        a live reply is what keeps a stale one from ever sounding over one — there is one output and
        one thing in it at a time — and being an ordinary entry in that queue is what makes a new turn
        cancel the lot, through the same halt that interrupts anything else.
        """
        from .language_settings import load_settings, resolve_voice
        queued, skipped = [], []
        for original in self.missed_replies(client, seconds=seconds, sessions=sessions):
            try:
                choice = resolve_voice(client.settings or load_settings(), original.language)
            except ValueError:
                choice = {'provider': 'kokoro'}
            bought = original.rendered and not original.parked
            if choice['provider'] != 'kokoro' and bought and self.stored_audio(original, choice) is None:
                # The room no longer has that audio and will not invent it or buy it again. A reply that was
                # never rendered — parked, or queued and left before its turn came — was never bought at all:
                # rendering it now is its first time, not a second.
                skipped.append({'history_id': original.row_id, 'reason': 'audio_gone'})
                continue
            echo = Utterance(original.id + ':replay:' + client.id, original.text,
                             language=original.language, thread_id=original.thread_id,
                             revision=client.revision, row_id=original.row_id, at=original.at)
            echo.replay_of = original.id
            echo.first_render = not bought
            echo.clients[client.id] = {'status': 'queued', 'reason': 'replay'}
            self.utterances[echo.id] = echo
            client.pending.append(echo.id)
            queued.append({'utterance_id': echo.id, 'history_id': original.row_id})
        if (queued or skipped) and client.on_browser_event:
            # The browser is told before any of it plays, so the bubbles say they are being repeated
            # rather than being mistaken for something the conversation has just said.
            client.on_browser_event({'type': 'voice-replay', 'data': {
                'session_id': client.id, 'thread_id': client.target.get('thread_id'),
                'replies': queued, 'skipped': skipped}})
        if queued:
            await self.fan_out([client])
        return {'replayed': queued, 'skipped': skipped}

    def replayable_rows(self, client):
        """Which replies this browser could hear again right now: the ones whose audio the room holds.
        Nothing is rendered for a repetition — not a paid render twice, not a browser render the room never
        saw (that changes when the browser is one more provider, #94). A bubble offers it only for these."""
        from .language_settings import load_settings, resolve_voice
        rows = set()
        for utterance in self.utterances.values():
            if utterance.replay_of or not utterance.row_id:
                continue
            try:
                choice = resolve_voice(client.settings or load_settings(), utterance.language)
            except ValueError:
                continue
            if choice['provider'] != 'kokoro' and self.stored_audio(utterance, choice) is not None:
                rows.add(utterance.row_id)
        return rows

    async def replay_one(self, client, row_id):
        """Play one reply again, on request, for this browser only (#100).

        Only from the audio the room holds: nothing is rendered or bought again. It goes to the head of this
        browser's queue, after what is playing."""
        from .language_settings import load_settings, resolve_voice
        original = next((u for u in reversed(list(self.utterances.values()))
                         if u.row_id == row_id and not u.replay_of), None)
        if original is None or original.thread_id != client.target.get('thread_id'):
            raise HTTPException(404, 'La sala ya no tiene esa respuesta.')
        try:
            choice = resolve_voice(client.settings or load_settings(), original.language)
        except ValueError:
            choice = {'provider': 'kokoro'}
        if choice['provider'] == 'kokoro' or self.stored_audio(original, choice) is None:
            raise HTTPException(410, 'La sala ya no tiene el audio de esa respuesta.')
        echo = Utterance(original.id + ':again:' + uuid.uuid4().hex[:8], original.text, language=original.language,
                         thread_id=original.thread_id, revision=client.revision, row_id=original.row_id, at=original.at)
        echo.replay_of = original.id
        echo.requested = True
        echo.clients[client.id] = {'status': 'queued', 'reason': 'replay'}
        self.utterances[echo.id] = echo
        client.pending.appendleft(echo.id)
        if client.on_browser_event:
            client.on_browser_event({'type': 'voice-replay', 'data': {
                'session_id': client.id, 'thread_id': original.thread_id,
                'replies': [{'utterance_id': echo.id, 'history_id': original.row_id}], 'skipped': []}})
        await self.fan_out([client])
        return {'utterance_id': echo.id, 'history_id': original.row_id}

    # ----- what the agent publishes -----

    async def speak(self, text, utterance_id, session_id, revision, language=None,
                    wait_for_quiet=False, thread_id=None, row_id=None):
        previous = self.utterances.get(utterance_id)
        if previous:
            if (previous.text, previous.revision, previous.language) != (text, revision, language):
                raise HTTPException(409, 'utterance_id ya usado con otro contenido.')
            return previous.result(session_id)
        if session_id not in self.sessions:
            raise HTTPException(409, 'The call changed; this reply belongs to another session.')
        asker = self.clients.get(session_id)
        if not asker or not asker.connected or asker.switching:
            raise HTTPException(409, 'No call is connected; no audio is kept for later.')
        if revision != asker.revision:
            raise HTTPException(409, 'Stale reply: the user has already started another turn.')
        thread_id = thread_id or asker.target.get('thread_id')
        listeners = self.audience(thread_id)
        if asker not in listeners:
            raise HTTPException(409, 'That browser is no longer on that conversation.')
        if asker.speaking and not wait_for_quiet:
            raise HTTPException(409, 'The user is speaking. Wait for their message before replying.')
        if len(self.utterances) >= self.MAX_UTTERANCES or any(len(c.pending) >= self.MAX_PENDING for c in listeners):
            raise HTTPException(429, 'Cola o historial de locuciones lleno.')
        utterance = Utterance(utterance_id, text, language=language, thread_id=thread_id, revision=revision,
                              row_id=row_id or (session_id + ':voice:' + utterance_id))
        for client in listeners:
            utterance.clients[client.id] = {
                'status': 'waiting_for_turn' if client.speaking else 'queued',
                'reason': 'user_speaking' if client.speaking else None}
            client.pending.append(utterance_id)
        self.utterances[utterance_id] = utterance
        # The row leaves 'text_only' as soon as it is queued, with the reason it is waiting for.
        self.sync(utterance)
        await self.fan_out(listeners)
        return utterance.result(session_id)

    async def publish(self, payload):
        row_id = payload.session_id + ':voice:' + payload.utterance_id
        # Store the conversational text even if its audio epoch has expired.
        asker = self.clients.get(payload.session_id)
        def facts(client):
            return PublicationClient(client.id, client.id in self.sessions, client.connected,
                                     client.target.get('thread_id'), client.switching,
                                     client.revision, client.turn_revision, client.speaking)

        decision = publication_decision(payload.session_id, payload.revision, payload.thread_id,
                                        facts(asker) if asker else None,
                                        tuple(facts(c) for c in self.audience(payload.thread_id)),
                                        session_exists=payload.session_id in self.sessions)
        # Only replacement changes the journal identity. Waiting uses the current audio
        # epoch below while preserving the original reply's journal revision.
        if decision.session_id != payload.session_id:
            asker = self.clients[decision.session_id]
            payload = payload.model_copy(update={'session_id': asker.id, 'revision': asker.revision})
            row_id = payload.session_id + ':voice:' + payload.utterance_id
        record = self.journal.binding_for_thread(payload.thread_id) if self.journal else None
        name = (record or {}).get('title') or (
            asker.target.get('title') if asker and asker.target.get('thread_id') == payload.thread_id else None) or 'Conversation'
        record = self.journal.put(id=row_id, thread=payload.thread_id, role='assistant',
                                  text=payload.text, name=name, session=payload.session_id,
                                  revision=payload.revision, status='text_only', language=payload.language)
        if record.get('_existing'):
            return {'status': record['status'], 'text_saved': True, 'utterance_id': payload.utterance_id}
        # Every browser on that conversation traces the same reply on its own clock.
        for client in self.audience(payload.thread_id):
            client.latency.reply(payload.utterance_id, payload.thread_id, payload.revision)
            client.telemetry.reply_received(payload.utterance_id, payload.thread_id, payload.revision)
        if not decision.can_speak:
            self.journal.update(row_id, 'text_only', decision.reason)
            # Nobody on this conversation was listening — the asker's session is gone, the call ended, or the
            # person was looking at another conversation. That is a first delivery delayed, not a reply
            # answered: it is kept, unrendered and unheard, so the next browser that returns to this
            # conversation gets it through the same replay as anything else it missed (#17, 2026-09-20).
            if (decision.reason in PARKABLE and payload.utterance_id not in self.utterances
                    and len(self.utterances) < self.MAX_UTTERANCES):
                parked = Utterance(payload.utterance_id, payload.text, language=payload.language,
                                   thread_id=payload.thread_id, revision=payload.revision, row_id=row_id)
                parked.parked = True
                self.utterances[payload.utterance_id] = parked
            return {'status': 'text_only', 'text_saved': True, 'reason': decision.reason}
        try:
            result = await self.speak(payload.text, payload.utterance_id, payload.session_id,
                                      decision.revision, payload.language,
                                      wait_for_quiet=decision.wait_for_quiet, thread_id=payload.thread_id, row_id=row_id)
        except HTTPException as error:
            if error.status_code not in {409, 429}:
                raise
            reason = 'expired_audio_turn' if error.status_code == 409 else 'queue_full'
            self.journal.update(row_id, 'text_only', reason)
            return {'status': 'text_only', 'text_saved': True, 'reason': reason}
        # The room already wrote the row as each listener moved; do not flatten its reason here.
        return {**result, 'text_saved': True}

    # ----- what a browser sends -----

    async def send_text(self, text, session_id, thread_id, binding_id, message_id):
        row_id = session_id + ':user-text:' + message_id
        previous = self.journal.get(row_id)
        if previous:
            if previous['text'] != text or previous['thread'] != thread_id:
                raise HTTPException(409, 'That identifier already belongs to another message.')
            return {'accepted': True, 'id': row_id, 'revision': previous['revision']}
        client = self.clients.get(session_id)
        if (not client or not client.connected
                or client.target.get('thread_id') != thread_id
                or client.target.get('binding_id') != binding_id):
            raise HTTPException(409, 'The connection or conversation changed. The text was not sent.')
        if not text.strip():
            raise HTTPException(422, 'Write a message.')
        # Typing is not barging in (#67): a text handed over while a reply plays leaves it playing, and the
        # replies already on their way stay current. Only a voice interrupts. The text rides this browser's
        # current epoch, so whatever answers it is as current as everything else.
        client.enqueue_input(text, target=dict(client.target), revision=client.revision,
                             message_id=message_id, history_id=row_id)
        return {'accepted': True, 'id': row_id, 'revision': client.revision}

    def delivery_status(self, row_id, status):
        # Receipts go to the browser that produced the input and to no other:
        # delivered means the harness accepted it, never that a human read it.
        row = self.journal.get(row_id)
        if not row:
            return
        payload = json.loads(row['payload'] or '{}')
        client = self.clients.get(payload.get('session_id'))
        if not client or not client.connected:
            return
        client.input_receipt(payload, status)
        if status == 'delivered' and client.target.get('thread_id') == payload.get('thread_id'):
            client.sent += 1

    # ----- which conversation the room is pointed at -----

    async def close_channel(self, thread_id):
        """Closing a conversation's voice from the room removes its binding, nothing more.

        The connector that served it is told and forgets it; the agent's next
        `voice_say` fails with that reason and it continues in writing; the room
        keeps no record, so re-enabling is just the agent joining again.
        """
        async with self.activation_lock:
            record = self.journal.binding_for_thread(thread_id)
            if record:
                if self.control:
                    await self.control.close_binding(record)
                else:
                    self.journal.deactivate_binding(record['connector'], record['id'])
            # Input still waiting for that conversation will not be delivered to a voice it no longer has.
            for row in self.journal.pending():
                if row['thread'] == thread_id:
                    self.journal.update(row['id'], 'not_sent', 'channel_closed')
            for client in list(self.clients.values()):
                if client.turn_target.get('thread_id') == thread_id:
                    client.cancelled_turn = client.turn_revision
                if client.target.get('thread_id') == thread_id:
                    await self._retarget(client, {})
            return {'status': 'closed', 'binding_id': record['id'] if record else None}

    async def select(self, session_id, thread_id, title=None):
        """One browser chooses which conversation it talks to. No other browser moves."""
        client = self.clients.get(session_id)
        if not client or not client.connected:
            raise HTTPException(409, 'That browser is not in the room.')
        async with self.activation_lock:
            current = client.target
            if current.get('thread_id') == thread_id and (not title or current.get('title') == title):
                return {'status': 'already_active', 'binding': dict(current)}
            new = await self._retarget(client, {'thread_id': thread_id, 'title': title})
        # Coming back to a conversation is a return like any other: what was missed on it plays now (#73).
        from .language_settings import load_settings
        await self.replay(client, seconds=(client.settings or load_settings()).replay_on_return_seconds)
        return {'status': 'activated', 'binding': new}

    async def deselect(self, session_id, binding_id):
        client = self.clients.get(session_id)
        if not client or not client.connected:
            raise HTTPException(409, 'That browser is not in the room.')
        async with self.activation_lock:
            if binding_id != client.target.get('binding_id'):
                raise HTTPException(409, 'The conversation changed. Refresh the room.')
            return {'status': 'activated', 'binding': await self._retarget(client, {})}

    async def _retarget(self, client, target):
        """Move one browser to another conversation (or to none): its own playback stops, its own epoch advances."""
        new = {'thread_id': target.get('thread_id'), 'title': target.get('title'), 'binding_id': str(uuid.uuid4())}
        try:
            client.switching = True
            # What was already said goes to the conversation it was said to, before anything moves (#93).
            if client.voice is not None and hasattr(client.voice, 'close_turn'):
                client.voice.close_turn()
            client.revision += 1
            client.halt('interrupted', 'focus_changed')
            if client.worker is not None:
                await client.worker.queue_frame(InterruptionFrame())
            else:
                client.speaking = False
            client.target = new
            if client.speaking:
                # The person is still talking: the rest of this turn is the new conversation's.
                client.turn_target = dict(new)
                client.turn_revision = client.revision
                client.turn_binding_id = new['binding_id']
                # The page shows the turn it is told about: without this the microphone looks idle while the
                # person goes on talking to the new conversation.
                if client.on_browser_event:
                    client.on_browser_event({'type': 'voice-user-turn', 'data': {
                        'phase': 'started', 'revision': client.turn_revision, 'thread_id': new['thread_id']}})
            client.sent = 0
            client.last_delivery = None
            client.error = None
        finally:
            client.switching = False
        self.report_conversation_working(client)
        return dict(new)

    # ----- reporting -----

    def snapshot(self, session_id=None):
        # `call` is the asking browser's own state; a page that has not joined,
        # or asks about someone else, is told about the room and nothing more.
        client = self.clients.get(session_id)
        return {'binding': (dict(client.target) if client.target.get('thread_id') else None) if client else None,
                'room': {'revision': client.revision if client else 0, 'speaking': client.speaking if client else self.speaking,
                         'switching': client.switching if client else False,
                         'clients': len(self.clients), 'audio': self.assets.stats(),
                         'utterances': [u.snapshot() for u in self.utterances.values()],
                         'audio_reports': list(self.audio_reports)[-10:],
                         'client_errors': list(self.client_errors)[-10:],
                         **_build_info()},
                'clients': [c.identity() for c in self.clients.values()],
                'call': client.snapshot() if client else None}

    def latency_snapshot(self, session_id=None):
        client = self.clients.get(session_id)
        return client.latency.snapshot() if client else {'session_id': None, 'replies': []}
