"""Voice room server: browser audio in, durable delivery out. No LLM lives here."""
import asyncio
import base64
import json
import os
import time
import uuid
from dotenv import dotenv_values
from loguru import logger
from fastapi import HTTPException, WebSocket

from . import transcription
from .room import client_error_report
from pipecat.audio.turn.smart_turn.base_smart_turn import SmartTurnParams
from pipecat.audio.turn.smart_turn.local_smart_turn_v3 import LocalSmartTurnAnalyzerV3
from pipecat.audio.vad.silero import SileroVADAnalyzer
from pipecat.audio.vad.vad_analyzer import VADParams
from pipecat.frames.frames import OutputTransportMessageUrgentFrame
from pipecat.pipeline.pipeline import Pipeline
from pipecat.pipeline.worker import PipelineParams, PipelineWorker
from pipecat.processors.aggregators.llm_context import LLMContext
from pipecat.processors.aggregators.llm_response_universal import LLMContextAggregatorPair, LLMUserAggregatorParams
from pipecat.transports.websocket.fastapi import FastAPIWebsocketParams, FastAPIWebsocketTransport
from pipecat.turns.user_start.vad_user_turn_start_strategy import VADUserTurnStartStrategy
from pipecat.turns.user_stop.speech_timeout_user_turn_stop_strategy import SpeechTimeoutUserTurnStopStrategy
from pipecat.turns.user_stop.turn_analyzer_user_turn_stop_strategy import TurnAnalyzerUserTurnStopStrategy
from pipecat.turns.user_turn_strategies import UserTurnStrategies
from pipecat.workers.runner import WorkerRunner

from .browser_heartbeat import heartbeat_settings, watch as watch_heartbeat
from .browser_socket import BrowserFrameSerializer, session_message
from .presentation import (hub, RoomClient, NoInference, mount_presentation,
                          PresentationGate, PresentationPlayback, require_same_origin)
from .connector_control import mount_connector_control
from .paths import REPOSITORY_ROOT
from .transcribers import TurnTranscriber

HELLO_TIMEOUT = 10.0
# The ceiling the room accepts for audio a browser captured while its socket was down. The page keeps
# the last 30 s of it; this leaves room for that and refuses anything that is not a gap.
CATCHUP_MAX_SECONDS = 35
CATCHUP_SLICE_BYTES = 128 * 1024
# How many earlier session ids of its own a page may name in its hello (#52). A reconnection mints a
# new client id, so this is how a tab says which entries in the journal were its own; naming one can
# only take a reply out of the catch-up, never put somebody else's in.
MAX_PRIOR_SESSIONS = 8


def catchup_time(value):
    """The browser's own clock for audio it captured, when it is plausible; otherwise the room's."""
    if not isinstance(value, (int, float)) or isinstance(value, bool):
        return None
    now = time.time() * 1000
    return int(value) if now - 3600_000 <= value <= now + 60_000 else None


def prior_sessions(value):
    """The ids this tab used before, as it named them: strings, bounded, and nothing else."""
    if not isinstance(value, list):
        return []
    return [item for item in value if isinstance(item, str) and 0 < len(item) <= 64][-MAX_PRIOR_SESSIONS:]


async def room_is_full(websocket):
    """Refusing one browser is not tearing the room down for the ones already in it.

    The reason is said twice — a frame, then the close code — and a tunnel can lose both: a phone
    read only the page's own "la sala rechazó la conexión" while this sentence was written for it
    (2026-09-22). The page asks `/api/presentation/admission` when neither arrived; the sentence and
    the name of the reason are the room's, in one place, so all three say the same thing.
    """
    admission = hub.admission()
    if admission['admitted']:
        return False
    await websocket.send_text(json.dumps({'type': 'error', 'data': {
        'message': admission['message'], 'reason': admission['reason']}}))
    await websocket.close(code=1013)  # Try again later.
    return True


def audio_idle_timeout(config):
    try:
        return max(0.0, float(config.get('VOICE_AUDIO_IDLE_TIMEOUT', '5.0')))
    except (TypeError, ValueError):
        return 5.0


def browser_runtime(data):
    """The local Whisper runtime a browser reports, or None when it reports none or an unsupported one."""
    if not isinstance(data, dict):
        return None
    model, device = data.get('model'), data.get('device')
    models = {item['id'] for item in transcription.PROVIDERS['browser']['models']}
    if model not in models or device not in {'webgpu', 'wasm'}:
        raise ValueError('Unsupported browser transcription engine.')
    return {'model': model, 'device': device}


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


def turn_stop_strategy(mic, config):
    """How a device's turn is declared over: a fixed silence, or smart-turn deciding from the audio."""
    if mic.turn_end_mode == 'smart_turn':
        analyzer = LocalSmartTurnAnalyzerV3(sample_rate=16000, params=SmartTurnParams(stop_secs=mic.smart_turn_max_silence))
        return TurnAnalyzerUserTurnStopStrategy(turn_analyzer=analyzer, wait_for_transcript=False)
    return SpeechTimeoutUserTurnStopStrategy(user_speech_timeout=mic.user_speech_timeout, wait_for_transcript=False)


def vad_analyzer(mic, config):
    # The VAD reports the pause once it has lasted this long; smart-turn is only asked then,
    # so a breath between words does not end the turn. A fixed timer needs no floor.
    default_stop = mic.smart_turn_min_silence if mic.turn_end_mode == 'smart_turn' else 0.2
    return SileroVADAnalyzer(params=VADParams(
        start_secs=mic.vad_start_secs,
        stop_secs=float(config.get('VOICE_VAD_STOP_SECS', default_stop)),
        confidence=mic.vad_confidence,
        min_volume=mic.vad_min_volume,
    ))


# How loud a voice must be to open a turn while a reply is playing out of this browser's speaker.
SPEAKING_MIN_VOLUME = 0.8


class VoiceCall:
    """What one browser's turns do to the room: open the epoch, transcribe once closed, deliver in order.

    The pipeline reports turn boundaries; this object owns everything after them,
    so the same flow serves any turn-end strategy and any transcription provider.
    """

    def __init__(self, call, transcriber, send, *, settings, mic, choice, runtime=None, vad_stop_secs=0.2, vad=None):
        self.call, self.transcriber, self.send = call, transcriber, send
        self.vad, self.mic = vad, mic
        self.bar_raised = False
        self.vad_stop_secs = vad_stop_secs
        self.finishing = set()
        self.lock = asyncio.Lock()
        self.held = None   # text of a turn the user resumed before it was delivered; the next turn carries it
        self.merge_window = mic.merge_window_secs   # how long a finished turn waits, in case it was a breath
        self.catchup = None   # the slices of a gap recording still arriving from the browser
        self.catchups = 0     # how many of them this call has already turned into messages
        call.stt = transcriber
        call.voice = self
        call.settings = settings
        call.transcription = {**choice, **(runtime or {})}
        call.mic_settings = mic.model_dump()
        call.input_stats = {'transport': 'pcm', 'turns': 0, 'audio_ms': 0, 'recognition_ms': 0, 'pending': 0}
        call.on_browser_event = send
        call.on_input_receipt = lambda data: send({'type': 'voice-input-receipt', 'data': data})
        call.audio_grace_seconds = settings.audio_grace_seconds
        transcriber.on_message = self.browser_message

    def listening_bar(self, speaking):
        """How loud a voice must be to open a turn while this browser is playing a reply.

        A phone's speaker feeds its own microphone: at the bar that suits a quiet room, the room heard
        itself, opened a turn, cut the reply that was still playing and delivered its own words back as a
        message (2026-09-20, word for word). Interrupting still works — it just has to be someone talking
        over the room rather than the room talking over itself. The microphone is never paused: that was
        ruled out the first day, because barge-in is the point.
        """
        if self.vad is None or speaking == self.bar_raised:
            return
        self.bar_raised = speaking
        from pipecat.audio.vad.vad_analyzer import VADParams
        self.vad.set_params(VADParams(
            start_secs=self.mic.vad_start_secs,
            stop_secs=self.vad_stop_secs,
            confidence=self.mic.vad_confidence,
            min_volume=max(self.mic.vad_min_volume, SPEAKING_MIN_VOLUME) if speaking else self.mic.vad_min_volume,
        ))

    def browser_message(self, message):
        """What a connected browser tells the room about itself, beyond audio.

        A switched local Whisper is recorded if the room can run it; new settings
        apply to this call at once for what needs no pipeline (voice, speed, grace).
        Transcription and microphone changes are this pipeline's own shape: the browser
        brings them in the hello of another socket, and lets this one go once that
        one answers. A catch-up is audio from before this session existed; it is
        recognised on its own and never reaches this pipeline's detector.
        """
        if not isinstance(message, dict) or message.get('type') not in {
                'voice-stt-ready', 'voice-settings', 'voice-audio-health', 'voice-catchup', 'voice-turn-trace',
                'voice-client-error'}:
            return
        data = message.get('data') if isinstance(message.get('data'), dict) else {}
        if data.get('session_id') != self.call.id:
            return
        if message['type'] == 'voice-catchup':
            return self.catch_up_slice(data)
        if message['type'] == 'voice-turn-trace':
            # The browser opened the root span for the turn the room just announced, and says so with a
            # W3C traceparent. Everything the room measures of that turn hangs from it.
            self.call.telemetry.turn_context(data.get('thread_id'), data.get('revision'), data.get('traceparent'))
            return
        if message['type'] == 'voice-client-error':
            # An uncaught error in the interface. React unmounts on one, so the room goes blank exactly when
            # the person it happens to cannot look at the screen: the room keeps the last few instead.
            if self.call.room is not None:
                self.call.room.client_errors.append(client_error_report(data, self.call.id))
            logger.warning('Call {}: interface error · {} · {}', self.call.id[:8],
                           str(data.get('kind') or '')[:40], str(data.get('message') or '')[:200])
            return
        if message['type'] == 'voice-audio-health':
            # What the browser's output did lately (stalls, cancels, refusals), so a stuck phone can be read from the room.
            health = data.get('health') if isinstance(data.get('health'), dict) else {}
            self.call.audio_health = {'reason': str(data.get('reason') or '')[:40], 'at': time.time(),
                                      **{key: health.get(key) for key in ('context', 'clock', 'output', 'element', 'playing', 'stalls', 'resuming', 'rate', 'buffer_rate')},
                                      'events': [event for event in (health.get('events') or []) if isinstance(event, dict)][-24:]}
            # The browser that reports a stuck output is usually reloaded seconds later: the report outlives it.
            if self.call.room is not None:
                self.call.room.audio_reports.append({'session_id': self.call.id, **self.call.audio_health})
            # The same moments, on the call's own span: one trace, not a second channel.
            self.call.telemetry.audio_event(self.call.audio_health['reason'], {
                'sidevoice.audio_output': self.call.audio_health.get('output'),
                'sidevoice.audio_context': self.call.audio_health.get('context'),
                'sidevoice.stalls': self.call.audio_health.get('stalls')})
            logger.info('Call {}: audio output {} · {} · clock {} · stalls {} · {}', self.call.id[:8], self.call.audio_health['reason'],
                        self.call.audio_health.get('context'), self.call.audio_health.get('clock'), self.call.audio_health.get('stalls'),
                        ' | '.join(f"{e.get('kind')}{(' ' + str(e.get('detail'))) if e.get('detail') else ''}" for e in self.call.audio_health['events'][-8:]))
            return
        if message['type'] == 'voice-settings':
            from .language_settings import settings_from
            settings, problem = settings_from(data.get('settings'))
            if problem:
                self.send({'type': 'error', 'data': {'message': problem}})
                return
            self.call.settings = settings
            self.call.audio_grace_seconds = settings.audio_grace_seconds
            return
        try:
            runtime = browser_runtime(data)
        except ValueError as error:
            self.send({'type': 'error', 'data': {'message': str(error)}})
            return
        if runtime:
            self.call.transcription = {**self.call.transcription, **runtime}

    # ----- what this browser captured while the room was unreachable -----

    def catch_up_slice(self, data):
        """One slice of the audio a browser buffered while its socket was down.

        It arrives base64 in text frames, never as the socket's binary frames. Binary frames are
        microphone PCM and go straight to the detector; this audio was spoken to a session that no
        longer exists, so it must not be able to open a turn here, and a text frame makes that
        impossible by construction rather than by care. Slices also keep every frame small enough
        that no proxy's message limit can drop the one thing this feature exists to save.
        """
        rate, seq = data.get('sample_rate'), data.get('seq')
        if not isinstance(seq, int) or isinstance(seq, bool) or not isinstance(rate, int) or not 8000 <= rate <= 48000:
            self.catchup = None
            return None
        if seq == 0:
            self.catchup = {'pcm': bytearray(), 'seq': 0, 'rate': rate,
                            'truncated': bool(data.get('truncated')), 'at': catchup_time(data.get('started_at'))}
        pending = self.catchup
        # A slice out of order means the recording is no longer what the browser sent: drop the lot
        # rather than transcribe a sentence with a hole in it.
        if pending is None or seq != pending['seq'] or rate != pending['rate']:
            self.catchup = None
            return None
        try:
            audio = base64.b64decode(data.get('audio_base64') or '', validate=True)
        except (ValueError, TypeError):
            self.catchup = None
            return None
        if len(audio) > CATCHUP_SLICE_BYTES or len(pending['pcm']) + len(audio) > CATCHUP_MAX_SECONDS * rate * 2:
            self.catchup = None
            self.send({'type': 'error', 'data': {'message': 'The audio captured offline was too long.'}})
            return None
        pending['pcm'].extend(audio)
        pending['seq'] += 1
        if not data.get('final'):
            return None
        self.catchup = None
        task = asyncio.create_task(self.catch_up(bytes(pending['pcm']), rate,
                                                 truncated=pending['truncated'], at=pending['at']))
        self.finishing.add(task)
        task.add_done_callback(self.finishing.discard)
        return task

    async def catch_up(self, pcm, sample_rate, *, truncated=False, at=None):
        """What the person said while this browser had no socket, as one message of its own.

        It is not a live turn and is never made to look like one: the epoch, the detector and the
        text this session is holding all belong to audio the room actually heard. This is recognised
        on its own, under the same lock so it cannot interleave with a turn, and written to the
        journal with the browser's own clock and a mark saying where it came from. The PCM is
        dropped the moment it has been recognised; the room stores none of it.
        """
        call = self.call
        self.catchups += 1
        history_id = f'{call.id}:user-catchup:{self.catchups}'
        started_at = time.monotonic()
        async with self.lock:
            target = dict(call.target)
            try:
                result = await self.transcriber.transcribe_audio(pcm, sample_rate)
            except Exception as error:
                failed = 'Could not transcribe what was captured offline: ' + (str(error) or type(error).__name__)
                call.error = failed
                self.send({'type': 'error', 'data': {'message': failed}})
                return None
            text = (result.text if result else '').strip()
            logger.info('Call {}: {} ms captured offline, recognised in {} ms · {} · {}', call.id[:8],
                        round(len(pcm) / (sample_rate * 2) * 1000), round((time.monotonic() - started_at) * 1000),
                        'truncated' if truncated else 'complete', 'became a message' if text else 'nothing voiced')
            # A gap that held no words is not a message and not an incident: there is nothing to show.
            if not text:
                return None
            offline = 'truncated' if truncated else 'buffered'
            # The browser must have the bubble before its receipt arrives, exactly as with a live turn.
            self.send({'type': 'voice-catchup-turn', 'data': {
                'session_id': call.id, 'history_id': history_id, 'thread_id': target.get('thread_id'),
                'text': text, 'offline': offline, 'time': at}})
            # Revision 0 is this browser's epoch before it ever opened a turn here, which is exactly
            # where this audio belongs: it interrupts nothing and no live turn can ever carry it.
            return call.enqueue_input(text, target=target, revision=0, history_id=history_id,
                                      offline=offline, at=at)

    def turn_started(self):
        call = self.call
        call.user_started()
        call.input_stats['pending'] = 1
        # The transport already tells the browser about speaking state; this names the turn.
        self.send({'type': 'voice-user-turn', 'data': {
            'phase': 'started', 'revision': call.turn_revision,
            'thread_id': call.turn_target.get('thread_id'),
        }})

    def turn_stopped(self):
        task = asyncio.create_task(self.finish_turn(self.call.turn_revision, dict(self.call.turn_target), time.monotonic()))
        self.finishing.add(task)
        task.add_done_callback(self.finishing.discard)
        return task

    def close_turn(self):
        """A conversation switch ends the turn being spoken (#93).

        What was said so far belongs to the conversation it was said to. Its audio is taken now, before
        the switch, so nothing spoken afterwards can join it, and it is delivered to that conversation
        whether or not the person is still talking; what comes next is a turn of the new one.
        """
        call = self.call
        pcm = self.transcriber.take_turn_audio()
        if not pcm and not self.held:
            return None
        task = asyncio.create_task(self.finish_turn(call.turn_revision, dict(call.turn_target), time.monotonic(),
                                                    pcm=pcm, closing=True))
        self.finishing.add(task)
        task.add_done_callback(self.finishing.discard)
        return task

    async def finish_turn(self, revision, target, stopped_at=None, *, pcm=None, closing=False):
        call = self.call
        stopped_at = stopped_at or time.monotonic()
        # The detector reports the pause once it has lasted vad_stop_secs, so speech ended that much earlier.
        vad_stopped_at = getattr(self.transcriber, 'vad_stopped_at', None)
        speech_end = (vad_stopped_at - self.vad_stop_secs) if vad_stopped_at else None
        # Turns are transcribed and delivered in the order they were spoken.
        async with self.lock:
            text, failed, metrics = '', None, {}
            try:
                result = await (self.transcriber.transcribe_turn(pcm) if closing else self.transcriber.transcribe_turn())
                if result is not None:
                    text, metrics = result.text.strip(), dict(result.metrics or {})
            except asyncio.TimeoutError:
                failed = ('This browser\'s transcription did not answer. If the device cannot run Whisper, '
                          'switch the transcription engine to OpenAI in the settings.')
            except Exception as error:
                failed = 'Could not transcribe your turn: ' + (str(error) or type(error).__name__)
            transcript_at = time.monotonic()
            if text or metrics:
                # Server-side stages of this turn, on one clock: what the browser measured stays as it came.
                metrics.setdefault('recognition_ms', round((transcript_at - stopped_at) * 1000, 1))
                if speech_end is not None and speech_end <= stopped_at:
                    metrics['endpoint_silence_ms'] = round((stopped_at - speech_end) * 1000, 1)
                    metrics['speech_end_to_transcript_ms'] = round((transcript_at - speech_end) * 1000, 1)
            if metrics:
                call.input_stats.update({
                    'audio_ms': max(0, int(metrics.get('audio_ms') or 0)),
                    'recognition_ms': max(0, int(metrics.get('recognition_ms') or 0)),
                })
                call.latency.input(target.get('thread_id'), revision, metrics)
            call.input_stats['turns'] += 1
            current = revision == call.turn_revision
            # A pause is not always an ending. Before delivering, the turn waits the window this device's
            # patience buys it: if the person carries on inside it, what they said next belongs to this same
            # message and the hold below does the joining (asked for in the room, 2026-09-20).
            if current and not closing and text and not failed and call.cancelled_turn != revision and self.merge_window:
                deadline = time.monotonic() + self.merge_window
                while (time.monotonic() < deadline and revision == call.turn_revision
                       and call.cancelled_turn != revision and call.connected):
                    await asyncio.sleep(0.05)
                current = revision == call.turn_revision
            # The decision that makes a resumed sentence one message or two, on the record (a live case on
            # 2026-09-19 resumed 75 ms after the cut and was still delivered as two).
            logger.info('Call {}: turn {} transcribed in {} ms · open turn {} · {} · held before {}', call.id[:8], revision,
                        round((transcript_at - stopped_at) * 1000), call.turn_revision,
                        'current' if current else 'conversation switched: delivered on its own' if closing or call.turn_target.get('thread_id') != target.get('thread_id') else 'user resumed: holding', bool(self.held))
            if call.cancelled_turn == revision:
                # Cancelling the draft cancels what was being held for it too.
                self.held = None
            elif self.held and not failed:
                # The previous turn was cut while the user was still going: it belongs to this message.
                text, self.held = (self.held + ' ' + text).strip(), None
            cancelled = bool(failed or call.cancelled_turn == revision or not text)
            # A turn that was closed by a switch, or whose successor is spoken to another conversation, is
            # never held: joining it to the next turn would deliver it to a conversation it was not said to.
            moved = closing or call.turn_target.get('thread_id') != target.get('thread_id')
            if not cancelled and not current and not moved:
                # The user started speaking again before this text was delivered: a breath, not a
                # new message. Hold it for the turn now open instead of sending half a sentence.
                self.held = text
                self.send({'type': 'voice-user-turn', 'data': {
                    'phase': 'cancelled', 'revision': revision, 'thread_id': target.get('thread_id'),
                    'text': text, 'merged': True}})
                return
            self.send({'type': 'voice-user-turn', 'data': {
                'phase': 'cancelled' if cancelled else 'finished', 'revision': revision,
                'thread_id': target.get('thread_id'), 'text': text,
            }})
            # The browser must create the final bubble before its receipt arrives.
            if not cancelled:
                call.enqueue_input(text, target=target, revision=revision)
                delivered_at = time.monotonic()
                call.latency.input(target.get('thread_id'), revision, {
                    'transcript_to_delivery_ms': round((delivered_at - transcript_at) * 1000, 1)})
                # The stages of getting a spoken turn into the journal, as spans under the browser's
                # root span for it. They are the same marks the stats dialog reads, said once more.
                call.telemetry.turn_finished(target.get('thread_id'), revision, speech_end=speech_end,
                                             turn_closed=stopped_at, transcript=transcript_at,
                                             delivered=delivered_at, metrics=metrics)
            if failed:
                call.error = failed
                self.send({'type': 'error', 'data': {'message': failed}})
            if current and not closing:
                call.input_stats['pending'] = 0
                await call.finish_user_turn()

    def close(self):
        self.catchup = None
        for task in list(self.finishing):
            task.cancel()


async def voice_call(websocket, settings, config, choice, hello, settings_problem=None):
    """One pipeline for every call: PCM in, the device's turn detection, and a transcription provider.

    The provider is OpenAI or the browser itself; the pipeline never knows which.

    A device that changes a setting this pipeline was built from opens a second socket instead of
    hanging up, so the same browser may hold two of these at once for as long as the swap takes.
    Nothing here is shared between them: each has its own client id, its own selection and its own
    epoch, and the one being replaced leaves without touching the one that replaced it.
    """
    from .language_settings import mic_settings
    mic, problem = mic_settings(settings, hello.get('mic'))
    problem = settings_problem or problem
    serializer = BrowserFrameSerializer()
    transport = FastAPIWebsocketTransport(websocket, FastAPIWebsocketParams(
        audio_in_enabled=True, serializer=serializer, allowed_origins=[]))
    vad = vad_analyzer(mic, config)
    user, assistant = LLMContextAggregatorPair(LLMContext(), user_params=LLMUserAggregatorParams(
        audio_idle_timeout=audio_idle_timeout(config),
        vad_analyzer=vad,
        user_turn_strategies=UserTurnStrategies(start=[VADUserTurnStartStrategy()], stop=[turn_stop_strategy(mic, config)]),
    ))
    outbox = asyncio.Queue()
    send = outbox.put_nowait

    async def deliver():
        while True:
            message = await outbox.get()
            await transport.output().send_message(OutputTransportMessageUrgentFrame(message=message))

    try:
        call = RoomClient(str(uuid.uuid4()), hub)
    except RuntimeError as error:
        # The room filled up between the check before the hello and this join. A browser changing a
        # setting that needs another pipeline holds two sockets for a moment, so the race is real:
        # refusing the newcomer is the whole point, and it disturbs nobody already in the room.
        await websocket.send_text(json.dumps({'type': 'error', 'data': {'message': str(error)}}))
        await websocket.close(code=1013)  # Try again later.
        return
    returning = prior_sessions(hello.get('sessions'))
    wanted = hello.get('conversation')
    if isinstance(wanted, str) and wanted:
        # The browser names the conversation it was talking to (its own state, kept across a reload);
        # it is honoured only if that conversation is still connected to the room.
        record = hub.journal.binding_for_thread(wanted) if hub.journal else None
        if record:
            call.target = {'thread_id': wanted, 'title': record.get('title'), 'binding_id': str(uuid.uuid4())}
    provider = transcription.build(settings, choice, config=config, send=send, session_id=call.id)
    transcriber = TurnTranscriber(provider, language=None if settings.stt_language == 'auto' else settings.stt_language)
    runtime, runtime_problem = None, None
    try:
        runtime = browser_runtime(hello.get('transcription'))
    except ValueError as error:
        runtime_problem = str(error)
    reported = hello.get('transcription') if isinstance(hello.get('transcription'), dict) else {}
    if runtime and reported.get('fallback_error'):
        # The browser offered a GPU and could not load Whisper on it: keep the reason where the stats can show it.
        runtime['fallback_from'] = str(reported.get('fallback_from') or '')[:20]
        runtime['fallback_error'] = str(reported['fallback_error'])[:300]
        logger.warning('Call {}: local Whisper fell back from {} to {}: {}', call.id[:8], runtime['fallback_from'],
                       runtime['device'], runtime['fallback_error'])
    voice = VoiceCall(call, transcriber, send, settings=settings, mic=mic, choice=choice, runtime=runtime,
                      vad_stop_secs=float(vad.params.stop_secs), vad=vad)
    # The hello carries the browser's call span, so the room's turns are inside the browser's call
    # and not a trace of their own. What this call is made of goes on it once, never on every turn.
    told = hello.get('telemetry') if isinstance(hello.get('telemetry'), dict) else {}
    call.telemetry.call_started(told.get('traceparent'), {
        'sidevoice.stt_provider': choice['provider'], 'sidevoice.stt_model': call.transcription.get('model'),
        'sidevoice.stt_device': call.transcription.get('device'), 'sidevoice.turn_end_mode': mic.turn_end_mode})
    call.mic = serializer
    problems = [message for message in (problem, runtime_problem) if message]
    logger.info('Call {}: transcription {} · {} ({}), turn end {}', call.id[:8], choice['provider'],
                call.transcription.get('model'), choice['reason'], mic.turn_end_mode)

    gate, playback = PresentationGate(), PresentationPlayback()
    pipeline = Pipeline([transport.input(), transcriber, user, NoInference(), gate,
                         transport.output(), playback, assistant])
    worker = PipelineWorker(pipeline, params=PipelineParams(enable_metrics=True))
    runner = WorkerRunner(handle_sigint=False)
    await runner.add_workers(worker)
    call.worker = worker
    gate.client = playback.client = call
    sender = asyncio.create_task(deliver())

    @user.event_handler('on_user_turn_started')
    async def turn_started(aggregator, strategy):
        voice.turn_started()

    @user.event_handler('on_user_turn_stopped')
    async def turn_stopped(aggregator, strategy, message):
        voice.turn_stopped()

    @transport.event_handler('on_client_connected')
    async def connected(transport, client):
        call.connected = True
        await transport.output().send_message(
            OutputTransportMessageUrgentFrame(message=session_message(call.id, serializer)))
        # Only now can anything reach the browser: what its hello got wrong goes right after the session.
        for message in problems:
            send({'type': 'error', 'data': {'message': message}})
        call.room.report_conversation_working(call)
        # A person coming back from a tunnel cannot read the transcript. What this browser never heard
        # through goes to it now, oldest first and ahead of anything new, for as long back as this
        # device asked for (#52). Nothing is stored for it: the room already had every one of them.
        caught_up = await call.room.replay(call, seconds=settings.replay_on_return_seconds,
                                           sessions=returning)
        if caught_up['replayed'] or caught_up['skipped']:
            logger.info('Call {}: replaying {} replies this browser never heard, {} without audio',
                        call.id[:8], len(caught_up['replayed']), len(caught_up['skipped']))

    @transport.event_handler('on_client_disconnected')
    async def disconnected(transport, client):
        call.disconnect()
        await runner.cancel()

    # A browser that stopped answering leaves by the door above, and this is what knocks on it: a
    # socket nobody is at the other end of never closes by itself behind a proxy, so the room asks,
    # and a browser that has missed its budget of answers is disconnected exactly as if its socket
    # had closed. Nothing downstream is told it was a timeout, because nothing downstream differs.
    interval, misses = heartbeat_settings(config)

    def ask():
        send({'type': 'voice-ping', 'data': {'session_id': call.id}})

    async def drop(silence):
        logger.warning('Call {}: nothing from this browser for {:.0f}s; its seat goes back to the room',
                       call.id[:8], silence)
        call.disconnect()   # the seat is free now, not whenever the socket admits it is gone
        await runner.cancel()

    heartbeat = asyncio.create_task(watch_heartbeat(
        lambda: time.monotonic() - serializer.last_frame_at, ask, drop,
        interval=interval, misses=misses)) if interval else None

    try:
        await runner.run()
    finally:
        if heartbeat:
            heartbeat.cancel()
        voice.close()
        call.disconnect()
        sender.cancel()


async def browser_call(websocket):
    """Every browser gets the same call, configured by what that browser brings in its first message."""
    from .language_settings import settings_from
    config = {**dotenv_values(REPOSITORY_ROOT / '.env.voice'), **os.environ}
    if await room_is_full(websocket):
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
    await voice_call(websocket, settings, config, choice, hello, problem)


def mount_browser_call(app):
    @app.websocket('/api/presentation/ws')
    async def browser_socket(websocket: WebSocket):
        try:
            require_same_origin(websocket)
        except HTTPException:
            await websocket.close(code=1008)  # Policy violation: not this room's own page.
            return
        await websocket.accept()
        await browser_call(websocket)


def create_app():
    """The whole room, assembled once: the image, start.sh and the tests all get this same thing."""
    from fastapi import FastAPI
    from fastapi.responses import RedirectResponse
    from .telemetry import configure as configure_telemetry, instrument, mount_telemetry
    # No schema, no playground: this room serves one interface and the endpoints that interface calls,
    # and FastAPI's defaults would publish a map of all of them to anyone who asks (2026-09-21).
    app = FastAPI(title='Sidevoice', docs_url=None, redoc_url=None, openapi_url=None)
    # The room is the interface: arriving at its address means arriving at the room.
    app.get('/', include_in_schema=False)(lambda: RedirectResponse('/voice/'))
    app.get('/voice', include_in_schema=False)(lambda: RedirectResponse('/voice/'))
    # Telemetry reads the same configuration the call does, so a room started without start.sh
    # still sees .env.voice. With no OTEL_EXPORTER_OTLP_ENDPOINT this starts nothing at all.
    configure_telemetry(environ={**dotenv_values(REPOSITORY_ROOT / '.env.voice'), **os.environ})
    mount_presentation(app)
    mount_connector_control(app, hub)
    mount_browser_call(app)
    mount_telemetry(app)
    instrument(app)
    return app


if __name__ == "__main__":
    import argparse
    import uvicorn
    parser = argparse.ArgumentParser(description="Sidevoice room: browser voice in, durable delivery out.")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8767)
    arguments = parser.parse_args()
    uvicorn.run(create_app(), host=arguments.host, port=arguments.port)
