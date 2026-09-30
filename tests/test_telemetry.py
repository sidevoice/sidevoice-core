"""What the room says in a trace, and — the part that is not negotiable — what it never says."""
import os
import re
import unittest
from unittest.mock import AsyncMock, MagicMock, patch

from opentelemetry.sdk.metrics.export import InMemoryMetricReader
from opentelemetry.sdk.trace.export import SimpleSpanProcessor
from opentelemetry.sdk.trace.export.in_memory_span_exporter import InMemorySpanExporter

from sidevoice import telemetry as module
from sidevoice.latency import CallLatency
from sidevoice.telemetry import ATTRIBUTES, CallTelemetry, STAGES, attributes, telemetry

# A browser's root span for one turn, as the traceparent frame carries it.
TRACEPARENT = '00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01'
TRACE_ID = 0x4bf92f3577b34da6a3ce929d0e0e4736
# Everything a person actually said in the tests below; none of it may reach an exporter.
SECRETS = ['la transcripción de mi intervención', 'la respuesta del agente', 'sk-secret-key']


class TelemetryHarness(unittest.TestCase):
    """Every test runs with a real SDK and an exporter that keeps the spans in memory."""

    def setUp(self):
        telemetry.shutdown()
        self.spans = InMemorySpanExporter()
        self.metrics = InMemoryMetricReader()
        module.configure(endpoint='http://collector.invalid:4318', span_exporter=self.spans,
                         metric_reader=self.metrics)
        # The batch processor would hold the spans until a timer; tests want them as they end.
        telemetry._tracer_provider.add_span_processor(SimpleSpanProcessor(self.spans))
        self.addCleanup(telemetry.shutdown)

    def finished(self):
        return {span.name: span for span in self.spans.get_finished_spans()}

    @staticmethod
    def span_ms(span):
        """A span's own length in milliseconds. Spans are placed in nanoseconds from float
        seconds, so the last nanosecond is arithmetic, not a measurement."""
        return round((span.end_time - span.start_time) / 1_000_000, 3)

    def measurements(self):
        """{instrument name: [data points]} out of the reader, whatever the exporter shape is."""
        found = {}
        data = self.metrics.get_metrics_data()
        for resource in getattr(data, 'resource_metrics', []):
            for scope in resource.scope_metrics:
                for metric in scope.metrics:
                    found.setdefault(metric.name, []).extend(metric.data.data_points)
        return found


class ConfigurationTest(unittest.TestCase):
    def tearDown(self):
        telemetry.shutdown()

    def test_no_endpoint_starts_nothing_and_every_call_is_a_no_op(self):
        telemetry.shutdown()
        module.configure(environ={})
        self.assertFalse(telemetry.enabled)
        self.assertIsNone(telemetry.tracer)
        call = CallTelemetry('s', CallLatency('s'))
        call.call_started(TRACEPARENT, {'sidevoice.stt_provider': 'openai'})
        call.turn_context('a', 1, TRACEPARENT)
        call.turn_finished('a', 1, speech_end=1, turn_closed=2, transcript=3, delivered=4)
        call.audio_event('stall')
        call.receipt('a', 1, 'read')
        call.cancelled('user_speaking')
        module.redelivered('a', 'claude')
        # Nothing was opened, so nothing can leak, block or fail: that is what unconfigured means.
        self.assertFalse(call.enabled)
        self.assertIsNone(call.call_span)
        self.assertEqual(call.turn_contexts, {})

    def test_an_unconfigured_room_does_not_even_import_opentelemetry(self):
        """The cheapest guarantee there is: with no endpoint the SDK is never loaded at all.

        Every OpenTelemetry import in `telemetry.py` lives inside the function that starts a
        provider, so a room nobody asked to trace pays for none of it. This has to run in its own
        interpreter, because by now this test module has imported the SDK itself.
        """
        import subprocess
        import sys
        from pathlib import Path
        probe = ('import sys;'
                 'import sidevoice.room, sidevoice.connector_control;'
                 'from sidevoice.telemetry import configure, telemetry;'
                 'configure(environ={});'
                 "print(telemetry.enabled, telemetry.tracer, telemetry._tracer_provider, telemetry._meter_provider,"
                 " sorted(m for m in sys.modules if m.startswith('opentelemetry')))")
        environment = {key: value for key, value in os.environ.items() if key != 'OTEL_EXPORTER_OTLP_ENDPOINT'}
        environment['PYTHONPATH'] = str(Path(__file__).resolve().parents[1])
        result = subprocess.run([sys.executable, '-c', probe], capture_output=True, text=True, env=environment)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('False None None None []', result.stdout)

    def test_the_endpoint_comes_from_the_environment_it_is_handed(self):
        telemetry.shutdown()
        module.configure(environ={'OTEL_EXPORTER_OTLP_ENDPOINT': 'http://collector:4318/',
                                  'OTEL_SERVICE_NAME': 'room-under-test'},
                         span_exporter=InMemorySpanExporter(), metric_reader=InMemoryMetricReader())
        self.assertTrue(telemetry.enabled)
        self.assertEqual(telemetry.endpoint, 'http://collector:4318')


class AttributeTest(unittest.TestCase):
    def test_only_the_allowlist_survives_and_nothing_carries_text(self):
        kept = attributes({'sidevoice.thread_id': 'a', 'sidevoice.turn_revision': 3,
                           'text': SECRETS[0], 'transcript': SECRETS[0], 'audio_base64': 'YQ==',
                           'sidevoice.reason': None})
        self.assertEqual(kept, {'sidevoice.thread_id': 'a', 'sidevoice.turn_revision': 3})

    def test_the_vocabulary_itself_names_no_content(self):
        # A key added to ATTRIBUTES that named content would pass every other test in this file, as
        # long as nobody happened to populate it. This one fails on the name alone.
        content = {'text', 'transcript', 'content', 'prompt', 'body', 'payload', 'message',
                   'key', 'token', 'base64', 'words', 'caption', 'title', 'speech', 'utterance'}
        for name in ATTRIBUTES:
            self.assertTrue(name.startswith('sidevoice.'), name)
            # What an attribute holds is what its last word says: an id, a state, a count, an engine.
            self.assertNotIn(name.rsplit('.', 1)[-1].rsplit('_', 1)[-1], content, name)

    def test_values_are_bounded(self):
        kept = attributes({'sidevoice.thread_id': 'x' * 500})
        self.assertEqual(len(kept['sidevoice.thread_id']), module.MAX_VALUE)


class ContractTest(unittest.TestCase):
    """`@sidevoice/protocol` owns the vocabulary; Python cannot import it, so it repeats it.

    This is the one place that checks the copy still says the same thing. Without it the room and
    the page could name the same stage differently and nothing would ever complain.
    """

    def contract(self, name):
        from pathlib import Path
        source = Path(__file__).resolve().parents[3] / 'packages' / 'protocol' / 'src' / 'index.ts'
        if not source.exists():
            self.skipTest('the protocol package is not checked out next to the room')
        body = source.read_text().split('export const ' + name + ' = [', 1)
        self.assertEqual(len(body), 2, name + ' is not declared in the protocol package')
        return re.findall(r'"([^"]+)"', body[1].split(']', 1)[0])

    def test_the_stage_names_are_the_ones_the_page_uses(self):
        self.assertEqual(self.contract('TURN_STAGES'), list(STAGES))

    def test_the_attribute_vocabulary_is_the_one_the_page_uses(self):
        self.assertEqual(sorted(self.contract('TELEMETRY_ATTRIBUTES')), sorted(ATTRIBUTES))


class TurnTraceTest(TelemetryHarness):
    def call(self):
        latency = CallLatency('s')
        call = CallTelemetry('s', latency)
        call.call_started(TRACEPARENT, {'sidevoice.stt_provider': 'browser'})
        call.turn_context('a', 1, TRACEPARENT)
        return call, latency

    def test_the_turn_continues_the_browser_trace_and_names_the_stages(self):
        call, latency = self.call()
        call.turn_finished('a', 1, speech_end=100.0, turn_closed=100.5, transcript=101.2,
                           delivered=101.25, metrics={'request_to_transcript_ms': 400})
        names = self.finished()
        self.assertEqual(set(names) & set(STAGES),
                         {'endpoint_silence', 'recognition', 'request_to_transcript', 'transcript_to_delivery'})
        for stage in ('endpoint_silence', 'recognition', 'request_to_transcript', 'transcript_to_delivery'):
            # One trace from the browser's root span through every stage the room measured.
            self.assertEqual(names[stage].context.trace_id, TRACE_ID, stage)
            self.assertEqual(names[stage].attributes['sidevoice.stage'], stage)
        self.assertEqual(self.span_ms(names['endpoint_silence']), 500)

    def test_a_turn_without_a_traceparent_is_still_measured(self):
        """A turn shorter than one round trip has no browser span to hang from. It is not lost."""
        call = CallTelemetry('s', CallLatency('s'))
        call.turn_finished('a', 9, speech_end=1.0, turn_closed=1.5, transcript=2.0, delivered=2.1)
        names = self.finished()
        self.assertIn('recognition', names)
        self.assertNotEqual(names['recognition'].context.trace_id, TRACE_ID)

    def test_the_agent_side_stages_end_on_the_reply_and_are_emitted_once(self):
        call, latency = self.call()
        latency.clock = lambda: 200.0
        latency.turn('a', 1, 'queued')
        latency.clock = lambda: 200.5
        latency.turn('a', 1, 'delivery_accepted')
        call.receipt('a', 1, 'delivered')
        latency.clock = lambda: 201.0
        latency.turn('a', 1, 'read')
        call.receipt('a', 1, 'read')
        latency.clock = lambda: 203.0
        latency.reply('u', 'a', 1)
        call.reply_received('u', 'a', 1)
        call.reply_received('u', 'a', 1)   # the room may pass this call site twice; the trace has one span
        names = [span.name for span in self.spans.get_finished_spans()]
        self.assertEqual(names.count('read_to_reply'), 1)
        self.assertEqual(names.count('delivery_to_read'), 1)
        self.assertEqual(names.count('input_queued_to_reply'), 1)
        finished = self.finished()
        self.assertEqual(self.span_ms(finished['read_to_reply']), 2000)
        self.assertEqual(self.span_ms(finished['input_queued_to_reply']), 3000)

    def test_a_shared_render_records_no_provider_request_it_never_made(self):
        call, latency = self.call()
        latency.clock = lambda: 300.0
        latency.reply('u', 'a', 1)
        latency.clock = lambda: 300.2
        latency.start_synthesis('u')
        latency.clock = lambda: 301.0
        latency.mark('u', 'audio_ready')
        call.synthesis('u', provider='elevenlabs', model='v3', shared=True,
                       provider_ms={'request_to_complete_ms': 800})
        names = self.finished()
        self.assertIn('reply_to_synthesis', names)
        self.assertNotIn('provider_synthesis', names)
        self.assertIs(names['reply_to_synthesis'].attributes['sidevoice.shared_audio'], True)

    def test_a_fresh_render_places_the_provider_request_on_the_mark_it_ends_on(self):
        call, latency = self.call()
        latency.clock = lambda: 300.0
        latency.reply('u', 'a', 1)
        latency.start_synthesis('u')
        latency.clock = lambda: 301.0
        latency.mark('u', 'audio_ready')
        call.synthesis('u', provider='elevenlabs', model='v3', shared=False,
                       provider_ms={'request_to_complete_ms': 800})
        span = self.finished()['provider_synthesis']
        self.assertEqual(self.span_ms(span), 800)
        self.assertEqual(span.context.trace_id, TRACE_ID)

    def test_audio_output_moments_are_events_on_the_call_span_not_spans_of_their_own(self):
        call, _ = self.call()
        call.audio_event('stall', {'sidevoice.audio_output': 'element', 'sidevoice.stalls': 2})
        call.call_ended('disconnected')
        span = self.finished()['voice.call']
        self.assertEqual([event.name for event in span.events], ['voice.audio.stall'])
        self.assertEqual(span.events[0].attributes['sidevoice.stalls'], 2)
        self.assertEqual(span.attributes['sidevoice.reason'], 'disconnected')


class MetricsTest(TelemetryHarness):
    def test_every_stage_has_a_histogram_and_the_counters_carry_their_status(self):
        latency = CallLatency('s')
        call = CallTelemetry('s', latency)
        call.turn_finished('a', 1, speech_end=1.0, turn_closed=1.5, transcript=2.0, delivered=2.1)
        call.playback('u', {'audio_received_to_playback_scheduled_ms': 42.5})
        call.receipt('a', 1, 'delivered')
        call.cancelled('user_speaking')
        module.redelivered('a', 'claude')
        found = self.measurements()
        self.assertEqual(self.metrics_value(found, 'sidevoice.turn.endpoint_silence'), 500.0)
        self.assertEqual(self.metrics_value(found, 'sidevoice.turn.audio_received_to_playback'), 42.5)
        self.assertEqual(found['sidevoice.input.receipts'][0].attributes['sidevoice.status'], 'delivered')
        self.assertEqual(found['sidevoice.turn.cancels'][0].value, 1)
        self.assertEqual(found['sidevoice.input.redeliveries'][0].value, 1)

    def test_every_stage_name_has_an_instrument(self):
        self.assertEqual(set(telemetry.histograms), set(STAGES))

    def test_a_turn_that_interrupted_nothing_is_not_counted_as_a_cancellation(self):
        from sidevoice.room import Room, RoomClient, Utterance
        hub = Room(MagicMock())
        call = RoomClient('s', hub, worker=AsyncMock())
        call.connected = True
        call.target = {'thread_id': 'a'}
        call.on_browser_event = lambda event: None
        call.user_started()          # the usual case: nothing was playing
        call.user_started()
        self.assertNotIn('sidevoice.turn.cancels', self.measurements())
        utterance = Utterance('u', 'texto', thread_id='a', revision=call.revision, row_id='s:voice:u')
        utterance.clients['s'] = {'status': 'playing', 'reason': None}
        hub.utterances['u'] = utterance
        call.pending.append('u')
        call.user_started()          # this one really did take a reply away from the listener
        self.assertEqual(self.measurements()['sidevoice.turn.cancels'][0].value, 1)

    @staticmethod
    def metrics_value(found, name):
        return found[name][0].sum


class PrivacyTest(TelemetryHarness):
    """The one test that must never be relaxed: a span of this room carries no content."""

    async def _one_call(self):
        from sidevoice.presentation import Speech
        from sidevoice.room import Room, RoomClient
        hub = Room(MagicMock())
        hub.journal.put.return_value = {}
        hub.journal.binding_for_thread.return_value = None
        call = RoomClient('s', hub, worker=AsyncMock())
        call.connected = True
        call.target = {'thread_id': 'a'}
        call.on_browser_event = lambda event: None
        call.telemetry.call_started(TRACEPARENT, {'sidevoice.stt_provider': 'openai',
                                                  'sidevoice.stt_model': 'gpt-4o-transcribe'})
        call.telemetry.turn_context('a', 1, TRACEPARENT)
        call.user_started()
        payload = call.enqueue_input(SECRETS[0])
        call.input_receipt(payload, 'delivered')
        call.input_receipt(payload, 'read')
        call.telemetry.turn_finished('a', 1, speech_end=1.0, turn_closed=1.5, transcript=2.0,
                                     delivered=2.1, metrics={'request_to_transcript_ms': 120})
        call.speaking = False
        choice = {'provider': 'elevenlabs', 'model': 'test', 'voice': 'test', 'speed': 1}
        audio = {'mime_type': 'audio/mpeg', 'audio_base64': 'YQ==',
                 'timings_ms': {'request_to_complete_ms': 800}}
        with patch('sidevoice.language_settings.load_settings'), \
                patch('sidevoice.language_settings.resolve_voice', return_value=choice), \
                patch('sidevoice.synthesis.synthesize', new=AsyncMock(return_value=audio)):
            await hub.publish(Speech(thread_id='a', session_id='s', revision=1,
                                     text=SECRETS[1], utterance_id='u'))
        call.telemetry.audio_event('stall', {'sidevoice.audio_output': 'element'})
        call.telemetry.call_ended('disconnected')
        return call

    def test_no_transcript_reply_or_credential_reaches_a_span(self):
        import asyncio
        asyncio.run(self._one_call())
        spans = self.spans.get_finished_spans()
        self.assertTrue(spans, 'the call under test must have produced spans')
        rendered = []
        for span in spans:
            for key, value in (span.attributes or {}).items():
                # A caller cannot widen the vocabulary: everything emitted is in the allowlist.
                self.assertIn(key, ATTRIBUTES, span.name)
                rendered.append(str(value))
            for event in span.events:
                for key, value in (event.attributes or {}).items():
                    self.assertIn(key, ATTRIBUTES, event.name)
                    rendered.append(str(value))
        haystack = ' '.join(rendered)
        for secret in SECRETS:
            self.assertNotIn(secret, haystack)
        # And nothing said it under another name either.
        self.assertNotIn(SECRETS[0], str([span.name for span in spans]))


class TelemetryEndpointTest(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        telemetry.shutdown()
        self.addCleanup(telemetry.shutdown)

    async def routes(self):
        from fastapi import FastAPI
        from sidevoice.telemetry import mount_telemetry
        app = FastAPI()
        mount_telemetry(app)
        return {route.path + ':' + sorted(route.methods - {'HEAD'})[0]: route.endpoint
                for route in app.routes if hasattr(route, 'endpoint')}

    @staticmethod
    def request(body=b'{}'):
        from starlette.requests import Request
        scope = {'type': 'http', 'method': 'POST', 'path': '/api/telemetry',
                 'headers': [(b'content-type', b'application/json')],
                 'server': ('localhost', 80), 'scheme': 'http'}

        async def receive():
            return {'type': 'http.request', 'body': body, 'more_body': False}

        return Request(scope, receive)

    async def test_an_unconfigured_room_advertises_nothing_and_drops_what_it_is_sent(self):
        module.configure(environ={})
        routes = await self.routes()
        self.assertEqual(await routes['/api/telemetry:GET'](), {'enabled': False})
        with patch('aiohttp.ClientSession') as session:
            response = await routes['/api/telemetry:POST'](self.request(b'{"resourceSpans":[]}'))
        self.assertEqual(response.status_code, 204)
        session.assert_not_called()

    async def test_a_configured_room_forwards_the_browser_batch_unread(self):
        module.configure(endpoint='http://collector:4318', span_exporter=InMemorySpanExporter(),
                         metric_reader=InMemoryMetricReader())
        routes = await self.routes()
        self.assertEqual(await routes['/api/telemetry:GET'](), {'enabled': True})
        posted = {}

        class Response:
            status = 200

            async def __aenter__(self):
                return self

            async def __aexit__(self, *error):
                return False

        class Session(Response):
            def post(self, url, data=None, headers=None):
                posted.update(url=url, data=data, headers=headers)
                return Response()

        with patch('aiohttp.ClientSession', return_value=Session()):
            response = await routes['/api/telemetry:POST'](self.request(b'{"resourceSpans":[1]}'))
        self.assertEqual(response.status_code, 204)
        self.assertEqual(posted['url'], 'http://collector:4318/v1/traces')
        self.assertEqual(posted['data'], b'{"resourceSpans":[1]}')

    async def test_an_oversized_batch_is_refused_before_anything_is_read(self):
        from fastapi import HTTPException
        module.configure(endpoint='http://collector:4318', span_exporter=InMemorySpanExporter(),
                         metric_reader=InMemoryMetricReader())
        routes = await self.routes()
        with self.assertRaises(HTTPException):
            await routes['/api/telemetry:POST'](self.request(b'x' * (module.MAX_TELEMETRY_BODY + 1)))
