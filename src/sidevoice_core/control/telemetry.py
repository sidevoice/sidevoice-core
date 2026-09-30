"""OpenTelemetry for the room: one trace per turn, one histogram per stage.

This is the only module that knows OpenTelemetry exists. Everything else calls
`CallTelemetry`, which is a no-op object whenever no collector is configured —
that is the point: `OTEL_EXPORTER_OTLP_ENDPOINT` unset must cost nothing and
fail nothing, so nothing here starts a provider, opens a socket or allocates a
span until an endpoint names one.

The span names are the stage keys the stats dialog already uses
(`apps/web/src/services/room-session-controller.js`, `LATENCY_STAGES`), so a
span and a stats row are the same thing. The marks they are built from belong to
`CallLatency`: this module reads them, it never keeps a second copy.

Privacy is not configurable. `ATTRIBUTES` is the whole vocabulary a span of this
room may carry — ids, timings, states, engine names — and `attributes()` drops
anything else. No transcript, no audio, no message text reaches a span, an
attribute or an event, whatever a caller passes.
"""
import os
import time

from loguru import logger

# The stage names, in the order a turn goes through them. Same keys as the browser's
# LATENCY_STAGES: a span, a histogram and a row of the stats dialog share one name.
STAGES = (
    'endpoint_silence',           # last voiced frame → the turn declared over
    'recognition',                # turn closed → transcript in hand
    'request_to_transcript',      # the browser's own Whisper, inside recognition
    'transcript_to_delivery',     # transcript → row accepted into the journal
    'delivery_to_read',           # harness accepted → the conversation admitted it
    'read_to_reply',              # admitted → first reply published
    'input_queued_to_reply',      # queued → first reply, the agent side end to end
    'reply_to_synthesis',         # reply published → synthesis started
    'provider_synthesis',         # the paid engine's own request
    'audio_received_to_playback',  # browser: audio event received → playback scheduled
)

# Everything a span of this room is allowed to say. Anything not named here is dropped,
# so a caller cannot widen the vocabulary by accident, and text can never arrive by mistake.
ATTRIBUTES = frozenset({
    'sidevoice.session_id', 'sidevoice.thread_id', 'sidevoice.turn_revision',
    'sidevoice.reply_revision', 'sidevoice.utterance_id',
    'sidevoice.status', 'sidevoice.reason', 'sidevoice.outcome', 'sidevoice.kind',
    'sidevoice.stt_provider', 'sidevoice.stt_model', 'sidevoice.stt_device',
    'sidevoice.tts_provider', 'sidevoice.tts_model', 'sidevoice.turn_end_mode',
    'sidevoice.harness', 'sidevoice.shared_audio', 'sidevoice.synthesis_attempt',
    'sidevoice.stage', 'sidevoice.duration_ms', 'sidevoice.audio_output',
    'sidevoice.audio_context', 'sidevoice.stalls', 'sidevoice.build_id',
})

# Ids and names are bounded like every other string the room keeps: a span is not a place
# to smuggle a payload through.
MAX_VALUE = 200

DEFAULT_SERVICE_NAME = 'sidevoice-room'
# Whatever the browser posts to /api/telemetry is forwarded unread; this is the ceiling.
MAX_TELEMETRY_BODY = 1_048_576

# One reference pair, taken once. Marks are monotonic (they are durations' clock and must
# stay so); spans need wall time. Converting through a single reference keeps every span of
# a process on the same footing instead of re-reading two clocks per mark.
_MONOTONIC_REFERENCE, _WALL_REFERENCE = time.monotonic(), time.time_ns()


def wall_time(mark):
    """The wall-clock nanosecond a monotonic mark stands for."""
    return None if mark is None else _WALL_REFERENCE + int((mark - _MONOTONIC_REFERENCE) * 1_000_000_000)


def _set_global(setter, provider):
    """Best effort: OpenTelemetry refuses a second global provider, and that is not an error here."""
    try:
        setter(provider)
    except Exception:  # pragma: no cover - the SDK only logs, it does not raise
        pass


def attributes(values):
    """Only what `ATTRIBUTES` names, bounded, and never a None."""
    kept = {}
    for key, value in (values or {}).items():
        if key not in ATTRIBUTES or value is None:
            continue
        kept[key] = value if isinstance(value, (bool, int, float)) else str(value)[:MAX_VALUE]
    return kept


class Telemetry:
    """The process-wide providers, or nothing at all.

    `enabled` is false until `configure()` finds an endpoint. Every call site checks it
    first, so an unconfigured room never touches OpenTelemetry beyond this attribute.
    """

    def __init__(self):
        self.enabled = False
        self.endpoint = None
        self.tracer = None
        self.propagator = None
        self.histograms = {}
        self.counters = {}
        self._tracer_provider = None
        self._meter_provider = None

    def configure(self, *, endpoint=None, environ=None, span_exporter=None, metric_reader=None):
        """Start the providers when an endpoint names a collector. Idempotent; safe to call twice."""
        if self.enabled:
            return self
        environ = os.environ if environ is None else environ
        self.endpoint = (endpoint or environ.get('OTEL_EXPORTER_OTLP_ENDPOINT') or '').strip().rstrip('/')
        if not self.endpoint and span_exporter is None:
            return self
        try:
            self._start(environ, span_exporter, metric_reader)
        except Exception as error:  # pragma: no cover - a broken collector config must not stop the room
            self.enabled = False
            logger.warning('Telemetry disabled: {}', error)
        return self

    def _start(self, environ, span_exporter, metric_reader):
        from opentelemetry import metrics, trace
        from opentelemetry.sdk.metrics import MeterProvider
        from opentelemetry.sdk.metrics.export import PeriodicExportingMetricReader
        from opentelemetry.sdk.resources import Resource
        from opentelemetry.sdk.trace import TracerProvider
        from opentelemetry.sdk.trace.export import BatchSpanProcessor
        from opentelemetry.trace.propagation.tracecontext import TraceContextTextMapPropagator

        from .paths import build_info
        info = build_info()
        resource = Resource.create({
            'service.name': environ.get('OTEL_SERVICE_NAME') or DEFAULT_SERVICE_NAME,
            'service.version': info.get('version') or 'dev',
            'sidevoice.web_build': info.get('web_build') or 'unknown',
        })
        if span_exporter is None:
            from opentelemetry.exporter.otlp.proto.http.trace_exporter import OTLPSpanExporter
            span_exporter = OTLPSpanExporter(endpoint=self.endpoint + '/v1/traces')
        self._tracer_provider = TracerProvider(resource=resource)
        self._tracer_provider.add_span_processor(BatchSpanProcessor(span_exporter))
        if metric_reader is None and self.endpoint:
            from opentelemetry.exporter.otlp.proto.http.metric_exporter import OTLPMetricExporter
            metric_reader = PeriodicExportingMetricReader(OTLPMetricExporter(endpoint=self.endpoint + '/v1/metrics'))
        self._meter_provider = MeterProvider(resource=resource,
                                             metric_readers=[metric_reader] if metric_reader else [])
        # The global providers are for the libraries that only know how to ask globally (FastAPI's
        # instrumentation). Everything this module emits goes through the providers it holds: the
        # global can only be set once in a process, and a room is not the only thing in a test run.
        _set_global(trace.set_tracer_provider, self._tracer_provider)
        _set_global(metrics.set_meter_provider, self._meter_provider)
        self.tracer = self._tracer_provider.get_tracer('sidevoice.room')
        self.propagator = TraceContextTextMapPropagator()
        meter = self._meter_provider.get_meter('sidevoice.room')
        # One histogram per stage, named exactly like the span and the stats row.
        self.histograms = {stage: meter.create_histogram(
            'sidevoice.turn.' + stage, unit='ms', description='Stage ' + stage + ' of one turn')
            for stage in STAGES}
        self.counters = {
            'stalls': meter.create_counter('sidevoice.audio.stalls', description='Playback stalls reported by a browser'),
            'cancels': meter.create_counter('sidevoice.turn.cancels', description='Turns or playbacks the room cancelled'),
            'receipts': meter.create_counter('sidevoice.input.receipts', description='Input receipts, by status'),
            'redeliveries': meter.create_counter('sidevoice.input.redeliveries', description='Input deliveries retried'),
        }
        self.enabled = True
        logger.info('Telemetry enabled · OTLP → {}', self.endpoint or 'in-memory exporter')

    def shutdown(self):
        for provider in (self._tracer_provider, self._meter_provider):
            if provider is not None:
                provider.shutdown()
        self.__init__()

    # ----- the primitives every call site uses -----

    def context_from(self, traceparent):
        """The remote span a W3C `traceparent` names, or None when there is none to continue."""
        if not self.enabled or not isinstance(traceparent, str) or not traceparent:
            return None
        return self.propagator.extract({'traceparent': traceparent[:MAX_VALUE]}) or None

    def span(self, name, *, start, end, parent=None, values=None, events=()):
        """One finished stage, placed on the clock by the marks it was measured from."""
        if not self.enabled or start is None or end is None or end < start:
            return None
        span = self.tracer.start_span(name, context=parent, start_time=wall_time(start),
                                      attributes=attributes({**(values or {}), 'sidevoice.stage': name}))
        for event_name, event_values, at in events:
            span.add_event(event_name, attributes=attributes(event_values), timestamp=wall_time(at))
        span.end(end_time=wall_time(end))
        return span

    def observe(self, stage, milliseconds, values=None):
        """The histogram half of a stage. Recorded once, by the room, for every stage there is."""
        if not self.enabled or stage not in self.histograms:
            return
        if not isinstance(milliseconds, (int, float)) or isinstance(milliseconds, bool) or milliseconds < 0:
            return
        self.histograms[stage].record(float(milliseconds), attributes(values))

    def count(self, name, values=None, amount=1):
        if self.enabled and name in self.counters:
            self.counters[name].add(amount, attributes(values))


telemetry = Telemetry()


def configure(**options):
    return telemetry.configure(**options)


def instrument(app):
    """FastAPI's own server spans, so a request to the room is in the same trace as the turn."""
    if not telemetry.enabled:
        return app
    try:
        from opentelemetry.instrumentation.fastapi import FastAPIInstrumentor
        FastAPIInstrumentor.instrument_app(app)
    except Exception as error:  # pragma: no cover - instrumentation is a nicety, never a requirement
        logger.warning('FastAPI instrumentation unavailable: {}', error)
    return app


class CallTelemetry:
    """One browser's traces. Reads the marks `CallLatency` owns; keeps none of its own.

    A turn's spans hang from the root span the browser opened, whose `traceparent` arrives
    over the socket. When it has not (a turn shorter than one round trip, or a browser with
    telemetry off), the stages are still emitted — as their own roots, rather than not at all.
    """

    # A browser may not hold more turn contexts than the latency trace holds turns.
    MAX_TURNS = 128

    def __init__(self, session_id, latency):
        self.session_id, self.latency = session_id, latency
        self.call_context = None
        self.call_span = None
        self.turn_contexts = {}
        self.facts = {}
        self.emitted = set()

    @property
    def enabled(self):
        return telemetry.enabled

    def common(self, **extra):
        return {'sidevoice.session_id': self.session_id, **self.facts, **extra}

    # ----- the call this browser is in -----

    def call_started(self, traceparent, facts=None):
        """The hello's `traceparent`: everything this browser does is inside the browser's call span."""
        if not telemetry.enabled:
            return
        self.facts = attributes(facts or {})
        self.call_context = telemetry.context_from(traceparent)
        self.call_span = telemetry.tracer.start_span(
            'voice.call', context=self.call_context, attributes=attributes(self.common()))

    def call_ended(self, reason=None):
        if self.call_span is not None:
            self.call_span.set_attributes(attributes({'sidevoice.reason': reason}))
            self.call_span.end()
            self.call_span = None

    def audio_event(self, kind, values=None):
        """What the browser's output did, on the call span. Not a second channel: the same trace."""
        if self.call_span is None:
            return
        self.call_span.add_event('voice.audio.' + str(kind)[:40],
                                 attributes=attributes(self.common(**(values or {}))))
        if kind == 'stall':
            telemetry.count('stalls', self.common())

    # ----- one turn -----

    def turn_context(self, thread_id, revision, traceparent):
        """The browser answered the turn-start event with the root span it opened for it."""
        if not telemetry.enabled:
            return
        context = telemetry.context_from(traceparent)
        if context is None:
            return
        self.turn_contexts[(thread_id, revision)] = context
        while len(self.turn_contexts) > self.MAX_TURNS:
            self.turn_contexts.pop(next(iter(self.turn_contexts)))

    def parent(self, thread_id, revision):
        return self.turn_contexts.get((thread_id, revision)) or self.call_context

    def stage(self, thread_id, revision, name, *, start, end, values=None):
        """A stage, as a span placed on the clock and as a measurement in its histogram."""
        if not telemetry.enabled or start is None or end is None or end < start:
            return
        milliseconds = round((end - start) * 1000, 2)
        common = self.common(**{'sidevoice.thread_id': thread_id, 'sidevoice.turn_revision': revision,
                                **(values or {})})
        telemetry.span(name, start=start, end=end, parent=self.parent(thread_id, revision), values=common)
        telemetry.observe(name, milliseconds, common)

    def duration_stage(self, thread_id, revision, name, milliseconds, *, ends_at, values=None):
        """A stage somebody else measured (the browser's Whisper, a provider's HTTP request).

        Its duration is the measurement; where it sits comes from the mark it ends on, which is
        this room's. Two clocks are never subtracted — one of them only places the interval.
        """
        if not telemetry.enabled or ends_at is None:
            return
        if not isinstance(milliseconds, (int, float)) or isinstance(milliseconds, bool) or not 0 <= milliseconds <= 3_600_000:
            return
        self.stage(thread_id, revision, name, start=ends_at - milliseconds / 1000, end=ends_at, values=values)

    def turn_finished(self, thread_id, revision, *, speech_end, turn_closed, transcript, delivered=None, metrics=None):
        """The four stages of getting a spoken turn into the journal, once the turn is over."""
        if not telemetry.enabled:
            return
        metrics = metrics or {}
        self.stage(thread_id, revision, 'endpoint_silence', start=speech_end, end=turn_closed)
        self.stage(thread_id, revision, 'recognition', start=turn_closed, end=transcript)
        self.duration_stage(thread_id, revision, 'request_to_transcript',
                            metrics.get('request_to_transcript_ms'), ends_at=transcript)
        self.stage(thread_id, revision, 'transcript_to_delivery', start=transcript, end=delivered)

    def receipt(self, thread_id, revision, status):
        """A receipt for a turn: one counter, and `delivery_to_read` the moment it is read."""
        if not telemetry.enabled:
            return
        telemetry.count('receipts', self.common(**{'sidevoice.status': status,
                                                   'sidevoice.thread_id': thread_id}))
        if status != 'read':
            return
        marks = self.latency.turns.get((thread_id, revision), {})
        self.stage(thread_id, revision, 'delivery_to_read',
                   start=marks.get('delivery_accepted') or marks.get('queued'), end=marks.get('read'))

    def reply_received(self, utterance_id, thread_id, revision):
        """The agent answered: the two stages that measure the agent side, ending on this mark."""
        if not telemetry.enabled or not self._once(('reply', utterance_id)):
            return
        marks = self.latency.turns.get((thread_id, revision), {})
        received = (self.latency.replies.get(utterance_id) or {}).get('marks', {}).get('received')
        values = {'sidevoice.utterance_id': utterance_id, 'sidevoice.reply_revision': revision}
        self.stage(thread_id, revision, 'read_to_reply', start=marks.get('read'), end=received, values=values)
        self.stage(thread_id, revision, 'input_queued_to_reply', start=marks.get('queued'), end=received, values=values)

    def synthesis(self, utterance_id, *, provider=None, model=None, shared=False, provider_ms=None):
        """Reply → synthesis, and the provider's own request when this listener is the one who paid for it."""
        if not telemetry.enabled:
            return
        row = self.latency.replies.get(utterance_id)
        if not row:
            return
        thread_id, revision = row['thread_id'], row['reply_revision']
        marks = row['marks']
        values = {'sidevoice.utterance_id': utterance_id, 'sidevoice.reply_revision': revision,
                  'sidevoice.tts_provider': provider, 'sidevoice.tts_model': model,
                  'sidevoice.shared_audio': bool(shared), 'sidevoice.synthesis_attempt': row['synthesis_attempt']}
        self.stage(thread_id, revision, 'reply_to_synthesis',
                   start=marks.get('received'), end=marks.get('synthesis_started'), values=values)
        # A listener handed someone else's render never made that request: it has no provider stage.
        if not shared:
            self.duration_stage(thread_id, revision, 'provider_synthesis',
                                (provider_ms or {}).get('request_to_complete_ms'),
                                ends_at=marks.get('audio_ready'), values=values)

    def playback(self, utterance_id, timings):
        """What the browser measured between the audio arriving and Web Audio scheduling it.

        The span for it is the browser's own; the room records the histogram, so every stage
        has exactly one place that counts it.
        """
        if not telemetry.enabled or not isinstance(timings, dict):
            return
        row = self.latency.replies.get(utterance_id) or {}
        telemetry.observe('audio_received_to_playback', timings.get('audio_received_to_playback_scheduled_ms'),
                          self.common(**{'sidevoice.utterance_id': utterance_id,
                                         'sidevoice.thread_id': row.get('thread_id'),
                                         'sidevoice.reply_revision': row.get('reply_revision')}))

    def cancelled(self, reason=None, thread_id=None, revision=None):
        telemetry.count('cancels', self.common(**{'sidevoice.reason': reason, 'sidevoice.thread_id': thread_id,
                                                  'sidevoice.turn_revision': revision}))

    def _once(self, key):
        """A stage is emitted once per turn even when the room passes its call site twice."""
        if key in self.emitted:
            return False
        self.emitted.add(key)
        while len(self.emitted) > self.MAX_TURNS * 4:
            self.emitted.pop()
        return True


def redelivered(thread_id=None, harness=None):
    """An input the room is handing to a harness again. Module level: the journal has no browser."""
    telemetry.count('redeliveries', {'sidevoice.thread_id': thread_id, 'sidevoice.harness': harness})


async def forward(body, content_type):
    """Hand the browser's OTLP batch to the collector, unread, or drop it when there is none.

    The browser talks to the room and to nothing else; the room is the only thing that knows
    where the traces go. With no endpoint configured nothing is sent, nothing is parsed and
    nothing fails — the browser is told so in advance and does not even build the batch.
    """
    if not telemetry.enabled or not telemetry.endpoint:
        return False
    import aiohttp
    try:
        async with aiohttp.ClientSession(timeout=aiohttp.ClientTimeout(total=10)) as http:
            async with http.post(telemetry.endpoint + '/v1/traces', data=body,
                                 headers={'Content-Type': content_type or 'application/json'}) as response:
                if response.status >= 400:
                    logger.warning('Telemetry collector refused a browser batch: {}', response.status)
                return response.status < 400
    except Exception as error:
        logger.warning('Telemetry collector unreachable: {}', error)
        return False


def mount_telemetry(app):
    """The browser's only telemetry address. It never learns the collector's."""
    from fastapi import HTTPException, Request, Response

    from .presentation import require_same_origin

    @app.get('/api/telemetry')
    async def telemetry_state():
        # The page asks before loading anything: with no collector it loads no SDK at all.
        return {'enabled': bool(telemetry.enabled and telemetry.endpoint)}

    @app.post('/api/telemetry')
    async def telemetry_batch(request: Request):
        require_same_origin(request)
        body = await request.body()
        if len(body) > MAX_TELEMETRY_BODY:
            raise HTTPException(413, 'Telemetry batch too large.')
        await forward(body, request.headers.get('content-type'))
        # Accepted or dropped, the browser is told the same thing: this is not its problem.
        return Response(status_code=204)

    return app
