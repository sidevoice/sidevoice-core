"""Bounded, in-memory latency traces. No audio, transcripts or credentials."""
from collections import OrderedDict
import math
import time


class CallLatency:
    """Server intervals use one monotonic clock; client durations stay separate."""
    BROWSER_FIELDS = {
        'audio_received_to_playback_scheduled_ms',
        'turn_finished_event_to_playback_scheduled_ms',
    }
    INPUT_FIELDS = {
        'audio_ms',
        'endpoint_silence_ms',
        'recognition_ms',
        'request_to_transcript_ms',
        'speech_end_to_transcript_ms',
        'transcript_to_delivery_ms',
    }

    def __init__(self, session_id, *, clock=time.monotonic, limit=128):
        self.session_id, self.clock, self.limit = session_id, clock, limit
        self.turns, self.replies = OrderedDict(), OrderedDict()

    def _bounded(self, records, key, initial):
        if key not in records:
            records[key] = initial
            if len(records) > self.limit:
                records.popitem(last=False)
        return records[key]

    def turn(self, thread, revision, event):
        if event not in {'queued', 'delivery_accepted', 'read'} or not thread:
            return
        marks = self._bounded(self.turns, (thread, revision), {})
        marks.setdefault(event, self.clock())

    def input(self, thread, revision, durations):
        if not thread or not isinstance(durations, dict):
            return
        marks = self._bounded(self.turns, (thread, revision), {})
        marks['input_ms'] = {**marks.get('input_ms', {}), **self._durations(durations, self.INPUT_FIELDS)}

    def reply(self, uid, thread, revision):
        self._bounded(self.replies, uid, {
            'utterance_id': uid, 'thread_id': thread, 'reply_revision': revision,
            'marks': {'received': self.clock()}, 'status': 'received',
            'provider_ms': {}, 'browser_ms': {}, 'synthesis_attempt': 0,
        })

    def start_synthesis(self, uid):
        row = self.replies.get(uid)
        if row:
            row['synthesis_attempt'] += 1
            for event in ('synthesis_started', 'audio_ready', 'audio_dispatched', 'playing_receipt'):
                row['marks'].pop(event, None)
            row['provider_ms'], row['browser_ms'] = {}, {}
            row['marks']['synthesis_started'] = self.clock()

    def mark(self, uid, event):
        row = self.replies.get(uid)
        if row and event in {'synthesis_started', 'audio_ready', 'audio_dispatched', 'playing_receipt'}:
            row['marks'].setdefault(event, self.clock())

    def status(self, uid, status):
        if uid in self.replies:
            self.replies[uid]['status'] = status

    def provider(self, uid, durations):
        if uid in self.replies and isinstance(durations, dict):
            self.replies[uid]['provider_ms'] = self._durations(
                durations, {'request_to_headers_ms', 'request_to_first_chunk_ms', 'request_to_complete_ms'})

    def browser(self, uid, durations):
        if uid in self.replies and isinstance(durations, dict):
            self.replies[uid]['browser_ms'].update(self._durations(durations, self.BROWSER_FIELDS))

    @staticmethod
    def _durations(values, allowed):
        return {key: round(value, 2) for key, value in values.items()
                if key in allowed and type(value) in (int, float)
                and 0 <= value <= 3_600_000 and math.isfinite(value)}

    @staticmethod
    def _interval(output, name, start, end):
        if start is not None and end is not None and end >= start:
            output[name] = round((end - start) * 1000, 2)

    def snapshot(self):
        rows = []
        for row in self.replies.values():
            marks = row['marks']
            turn = self.turns.get((row['thread_id'], row['reply_revision']), {})
            durations = {}
            for name, start, end in (
                ('input_queued_to_delivery_accepted_ms', turn.get('queued'), turn.get('delivery_accepted')),
                ('input_queued_to_reply_received_ms', turn.get('queued'), marks.get('received')),
                ('delivery_accepted_to_reply_received_ms', turn.get('delivery_accepted'), marks.get('received')),
                ('delivery_accepted_to_read_ms', turn.get('delivery_accepted'), turn.get('read')),
                ('input_queued_to_read_ms', turn.get('queued'), turn.get('read')),
                ('read_to_reply_received_ms', turn.get('read'), marks.get('received')),
                ('reply_received_to_synthesis_started_ms', marks.get('received'), marks.get('synthesis_started')),
                ('synthesis_started_to_audio_ready_ms', marks.get('synthesis_started'), marks.get('audio_ready')),
                ('audio_dispatched_to_playing_receipt_ms', marks.get('audio_dispatched'), marks.get('playing_receipt')),
            ):
                self._interval(durations, name, start, end)
            rows.append({key: row[key] for key in ('utterance_id', 'thread_id', 'reply_revision', 'status', 'synthesis_attempt')} |
                        {'input_ms': dict(turn.get('input_ms', {})), 'server_ms': durations,
                         'provider_ms': dict(row['provider_ms']), 'browser_ms': dict(row['browser_ms'])})
        return {'session_id': self.session_id, 'limit': self.limit, 'replies': rows,
                'notes': ['Durations only; server and browser clocks are never subtracted.',
                          'Delivery acceptance is not model execution or reading.',
                          'Playing means Web Audio scheduled playback, not Bluetooth audibility.',
                          'Input durations are measured in the browser from the last voiced microphone frame.',
                          'Missing measurements are omitted, not zero. Data expires with the call.']}
