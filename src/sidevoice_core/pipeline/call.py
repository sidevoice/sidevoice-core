"""One call's turns: the pipeline that hears them and the flow that follows each one.

`CallPipeline` assembles Pipecat for one call around a transport it is handed — whatever carries
the audio is the caller's to build — and `VoiceCall` owns everything after a turn boundary: open
the epoch, transcribe once closed, merge a breath, hold a resumed sentence, deliver in order.

Neither knows what a room is. Everything they do to the call they serve goes through the methods
`CallPort` names; the control plane's client object is one implementation of it.
"""
import asyncio
import base64
import time
from typing import Protocol

from loguru import logger
from pipecat.audio.turn.smart_turn.base_smart_turn import SmartTurnParams
from pipecat.audio.turn.smart_turn.local_smart_turn_v3 import LocalSmartTurnAnalyzerV3
from pipecat.audio.vad.silero import SileroVADAnalyzer
from pipecat.audio.vad.vad_analyzer import VADParams
from pipecat.pipeline.pipeline import Pipeline
from pipecat.pipeline.worker import PipelineParams, PipelineWorker
from pipecat.processors.aggregators.llm_context import LLMContext
from pipecat.processors.aggregators.llm_response_universal import LLMContextAggregatorPair, LLMUserAggregatorParams
from pipecat.turns.user_start.vad_user_turn_start_strategy import VADUserTurnStartStrategy
from pipecat.turns.user_stop.speech_timeout_user_turn_stop_strategy import SpeechTimeoutUserTurnStopStrategy
from pipecat.turns.user_stop.turn_analyzer_user_turn_stop_strategy import TurnAnalyzerUserTurnStopStrategy
from pipecat.turns.user_turn_strategies import UserTurnStrategies
from pipecat.workers.runner import WorkerRunner

from .processors import NoInference, PresentationGate, PresentationPlayback
from .settings import catalogue_model, settings_from, unavailable
from .transcribers import TurnTranscriber


class CallPort(Protocol):
    """What a pipeline needs from the call it serves, and nothing more.

    The control plane's `RoomClient` is the implementation; a test's fake is another. Attributes
    are read and written by the turn flow (`VoiceCall`); methods are called by it and by the two
    processors that watch the output (`PresentationGate`, `PresentationPlayback`). `latency` and
    `telemetry` are the call's own measurement objects, used through the methods named beside them.
    """
    id: str
    connected: bool
    target: dict
    turn_target: dict
    turn_revision: int
    cancelled_turn: int | None
    error: str | None
    # Written by VoiceCall when it takes the call: what this call is made of, for the room to show.
    stt: object
    voice: object
    settings: object
    transcription: dict
    mic_settings: dict
    input_stats: dict
    audio_grace_seconds: float
    on_browser_event: object     # callable(message): something for this client to hear
    on_input_receipt: object     # callable(data): a receipt for this client's input
    tts: object                  # read by PresentationGate: `select_language(language)` when it has one
    latency: object              # .input(thread_id, revision, metrics)
    telemetry: object            # .turn_context(...), .audio_event(...), .turn_finished(...)

    def user_started(self): ...
    async def finish_user_turn(self): ...
    def enqueue_input(self, text, *, target=None, revision=None, message_id=None, history_id=None,
                      offline=None, at=None): ...
    def report_client_error(self, data): ...
    def report_audio_health(self, health): ...
    def is_current(self, uid, revision): ...
    def transition(self, uid, status, reason=None): ...
    def fail_active(self): ...
    async def playback_finished(self, uid, revision): ...


# The ceiling the room accepts for audio a browser captured while its socket was down. The page keeps
# the last 30 s of it; this leaves room for that and refuses anything that is not a gap.
CATCHUP_MAX_SECONDS = 35
CATCHUP_SLICE_BYTES = 128 * 1024
# How many earlier session ids of its own a page may name in its hello. A reconnection mints a
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


def audio_idle_timeout(config):
    try:
        return max(0.0, float(config.get('VOICE_AUDIO_IDLE_TIMEOUT', '5.0')))
    except (TypeError, ValueError):
        return 5.0


def browser_runtime(data):
    """The transcription runtime a client reports (in its hello and in `voice-stt-ready`), or None when it reports
    none: a catalogue model of the stt task, on one of that model's engines, and the accelerator it runs on — what
    the stats show, not what the node obeys. A load that fell back keeps its reason where the stats can show it."""
    if not isinstance(data, dict):
        return None
    model, engine, accelerator = catalogue_model(data.get('model'), 'stt'), data.get('engine'), data.get('accelerator')
    if (model is None or engine not in {build['engine'] for build in model['builds']}
            or not isinstance(accelerator, str) or not 0 < len(accelerator) <= 40):
        raise ValueError('Unsupported transcription runtime.')
    runtime = {'model': model['id'], 'engine': engine, 'accelerator': accelerator, 'cached': data.get('cached') is True}
    if data.get('fallback_error'):
        runtime['fallback_from'] = str(data.get('fallback_from') or '')[:20]
        runtime['fallback_error'] = str(data['fallback_error'])[:300]
    return runtime


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

    def __init__(self, call, transcriber, send, *, settings, mic, choice, runtime=None, vad_stop_secs=0.2, vad=None,
                 config=None):
        self.config = config   # whose provider keys a live change is checked against (None: this node's own)
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
        # What the room may show about this call's transcription. The context is the person's own words to
        # the recogniser (names, jargon): it goes to the recogniser and nowhere else, not to other listeners.
        call.transcription = {key: value for key, value in {**choice, **(runtime or {})}.items() if key != 'context'}
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
        if getattr(self.call, 'echo_mode', False):
            if isinstance(message, dict) and message.get('type') == 'echo.speech-result':
                self.call.receive_speech_result(message.get('data'))
            return
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
            self.call.report_client_error(data)
            logger.warning('Call {}: interface error · {} · {}', self.call.id[:8],
                           str(data.get('kind') or '')[:40], str(data.get('message') or '')[:200])
            return
        if message['type'] == 'voice-audio-health':
            # What the browser's output did lately (stalls, cancels, refusals), so a stuck phone can be read from the room.
            health = data.get('health') if isinstance(data.get('health'), dict) else {}
            # The browser that reports a stuck output is usually reloaded seconds later: the call keeps the
            # report where it outlives this browser.
            reported = {'reason': str(data.get('reason') or '')[:40], 'at': time.time(),
                                           **{key: health.get(key) for key in ('context', 'clock', 'output', 'element', 'playing', 'stalls', 'resuming', 'rate', 'buffer_rate')},
                                           'events': [event for event in (health.get('events') or []) if isinstance(event, dict)][-24:]}
            self.call.report_audio_health(reported)
            # The same moments, on the call's own span: one trace, not a second channel.
            self.call.telemetry.audio_event(reported['reason'], {
                'sidevoice.audio_output': reported.get('output'),
                'sidevoice.audio_context': reported.get('context'),
                'sidevoice.stalls': reported.get('stalls')})
            logger.info('Call {}: audio output {} · {} · clock {} · stalls {} · {}', self.call.id[:8], reported['reason'],
                        reported.get('context'), reported.get('clock'), reported.get('stalls'),
                        ' | '.join(f"{e.get('kind')}{(' ' + str(e.get('detail'))) if e.get('detail') else ''}" for e in reported['events'][-8:]))
            return
        if message['type'] == 'voice-settings':
            settings, problem = settings_from(data.get('settings'))
            if problem:
                self.send({'type': 'error', 'data': {'message': problem}})
                return
            refusal = unavailable(settings, self.config)
            if refusal:
                # Refused whole, like at the hello: the settings in use stay, and the page is told why.
                self.send({'type': 'error', 'data': refusal})
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
        if not getattr(call, 'echo_mode', False):
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
        """A conversation switch ends the turn being spoken.

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
                failed = ('echo.transcription-failed' if getattr(call, 'echo_mode', False) else
                          'This browser\'s transcription did not answer. If the device cannot run Whisper, '
                          'switch the transcription engine to OpenAI in the settings.')
            except Exception as error:
                failed = ('echo.transcription-failed' if getattr(call, 'echo_mode', False) else
                          'Could not transcribe your turn: ' + (str(error) or type(error).__name__))
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
                if not getattr(call, 'echo_mode', False):
                    self.send({'type': 'voice-user-turn', 'data': {
                        'phase': 'cancelled', 'revision': revision, 'thread_id': target.get('thread_id'),
                        'text': text, 'merged': True}})
                return
            if not getattr(call, 'echo_mode', False):
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
                if getattr(call, 'echo_mode', False):
                    call.report_error(failed, stage='transcription', provider=call.transcription.get('place'),
                                      model=call.transcription.get('model'))
                else:
                    self.send({'type': 'error', 'data': {'message': failed}})
            if current and not closing:
                call.input_stats['pending'] = 0
                await call.finish_user_turn()

    def close(self):
        self.catchup = None
        for task in list(self.finishing):
            task.cancel()




class CallPipeline:
    """Pipecat for one call: the transport's input, this device's turn detection, one transcription
    provider, and the processors that watch what goes out. One per call, never shared.

    The transport is handed in — a WebSocket today, whatever carries the audio tomorrow — so the
    pipeline never learns what a socket is; the provider is handed in, so it never learns who
    transcribes.
    """

    def __init__(self, transport, *, mic, config, provider, language=None):
        self.transport = transport
        self.vad = vad_analyzer(mic, config)
        self.user, self.assistant = LLMContextAggregatorPair(LLMContext(), user_params=LLMUserAggregatorParams(
            audio_idle_timeout=audio_idle_timeout(config),
            vad_analyzer=self.vad,
            user_turn_strategies=UserTurnStrategies(start=[VADUserTurnStartStrategy()],
                                                    stop=[turn_stop_strategy(mic, config)]),
        ))
        self.transcriber = TurnTranscriber(provider, language=language)
        self.gate, self.playback = PresentationGate(), PresentationPlayback()
        self.pipeline = Pipeline([transport.input(), self.transcriber, self.user, NoInference(), self.gate,
                                  transport.output(), self.playback, self.assistant])
        self.worker = PipelineWorker(self.pipeline, params=PipelineParams(enable_metrics=True))
        self.runner = WorkerRunner(handle_sigint=False)

    @property
    def vad_stop_secs(self):
        return float(self.vad.params.stop_secs)

    async def serve(self, call, voice):
        """Bind this pipeline to the call it serves and to the flow that follows its turns."""
        await self.runner.add_workers(self.worker)
        call.worker = self.worker
        self.gate.client = self.playback.client = call

        @self.user.event_handler('on_user_turn_started')
        async def turn_started(aggregator, strategy):
            voice.turn_started()

        @self.user.event_handler('on_user_turn_stopped')
        async def turn_stopped(aggregator, strategy, message):
            voice.turn_stopped()

    async def feed(self, pcm, sample_rate=16000):
        """Microphone PCM from somewhere other than the transport's own frames (a WebRTC track): into the
        same input, the same detector, the same turn — the pipeline never knows the path."""
        from pipecat.frames.frames import InputAudioRawFrame
        await self.transport.input().push_audio_frame(
            InputAudioRawFrame(audio=pcm, sample_rate=sample_rate, num_channels=1))

    async def run(self):
        await self.runner.run()

    async def cancel(self):
        await self.runner.cancel()
