"""A call port for a short echo diagnostic, deliberately outside room state."""
import asyncio
import uuid

from ..i18n import keyed_message, translate
from ..pipeline import synthesis
from ..pipeline.settings import resolve_voice

MAX_SECONDS = 60
SPEECH_ACK_SECONDS = 15


class EchoLatency:
    def input(self, thread_id, revision, metrics):
        return None


class EchoTelemetry:
    """Echo diagnostics do not publish room or external telemetry records."""
    def call_started(self, *args, **kwargs):
        return None

    def call_ended(self, *args, **kwargs):
        return None

    def turn_context(self, *args, **kwargs):
        return None

    def audio_event(self, *args, **kwargs):
        return None

    def turn_finished(self, *args, **kwargs):
        return None


class EchoCall:
    """The `VoiceCall` contract for echo mode. It has no room, conversation or journal reference."""

    echo_mode = True

    def __init__(self, session_id, *, settings, config, send):
        self.id = session_id
        self.settings = settings
        self.config = config
        self.send = send
        self.connected = False
        self.closed = False
        self.speaking = False
        self.error = None
        self.target = {}
        self.turn_target = {}
        self.turn_revision = 0
        self.cancelled_turn = None
        self.audio_grace_seconds = settings.audio_grace_seconds
        self.input_stats = None
        self.stt = None
        self.voice = None
        self.tts = None
        self.mic = None
        self.media_peer = None
        self.latency = EchoLatency()
        self.telemetry = EchoTelemetry()
        self.transcripts = 0
        self.spoken = 0
        self.pending_speech = {}
        self.speech_tasks = set()

    def user_started(self):
        self.speaking = True
        self.turn_revision += 1
        self.turn_target = {}
        self.cancelled_turn = None
        for request_id, future in tuple(self.pending_speech.items()):
            self.send({'type': 'echo.cancel', 'data': {'session_id': self.id, 'request_id': request_id}})
            if not future.done():
                future.set_result('cancelled')
        for task in tuple(self.speech_tasks):
            task.cancel()

    async def finish_user_turn(self):
        self.speaking = False

    def enqueue_input(self, text, *, target=None, revision=None, message_id=None, history_id=None,
                      offline=None, at=None):
        """Expose this final transcript to the page and ask the selected voice stage to speak it."""
        text = text.strip() if isinstance(text, str) else ''
        if not text or not self.connected or self.cancelled_turn == self.turn_revision:
            return None
        revision = self.turn_revision if revision is None else revision
        self.transcripts += 1
        self.send({'type': 'echo.transcript', 'data': {
            'session_id': self.id, 'revision': revision, 'text': text}})
        task = asyncio.create_task(self._speak(text, revision))
        self.speech_tasks.add(task)
        task.add_done_callback(self.speech_tasks.discard)
        return None

    async def _speak(self, transcript, revision):
        try:
            choice = resolve_voice(self.settings, self.settings.ui_language)
        except ValueError:
            self.report_error('echo.voice-unavailable', stage='voice', provider=self.settings.tts.place,
                              model=self.settings.tts.model)
            return
        response = translate('echo.prefix', self.settings.ui_language, text=transcript)
        request_id = str(uuid.uuid4())
        future = asyncio.get_running_loop().create_future()
        self.pending_speech[request_id] = future
        payload = {'session_id': self.id, 'request_id': request_id, 'revision': revision,
                   'text': response, 'voice': choice}
        try:
            if choice['place'] == 'device':
                self.send({'type': 'echo.speech', 'data': payload})
            else:
                audio = await synthesis.synthesize(response, model=choice['model'], voice=choice['voice'],
                                                   speed=choice['speed'], config=self.config)
                if future.done() and future.result() == 'cancelled':
                    return
                self.send({'type': 'echo.speech', 'data': {**payload, **audio}})
            status = await asyncio.wait_for(future, SPEECH_ACK_SECONDS)
            if status == 'played':
                self.spoken += 1
            elif status != 'cancelled':
                self.report_error('echo.voice-failed', stage='voice', provider=choice['place'], model=choice['model'])
        except asyncio.TimeoutError:
            self.report_error('echo.voice-timeout', stage='voice', provider=choice['place'], model=choice['model'])
        except asyncio.CancelledError:
            raise
        except Exception:
            self.report_error('echo.voice-failed', stage='voice', provider=choice['place'], model=choice['model'])
        finally:
            self.pending_speech.pop(request_id, None)

    def receive_speech_result(self, data):
        if not isinstance(data, dict) or data.get('session_id') != self.id:
            return False
        future = self.pending_speech.get(data.get('request_id'))
        if future is None or future.done():
            return True
        future.set_result('played' if data.get('status') == 'played' else 'failed')
        return True

    def report_error(self, key, **params):
        self.error = key
        self.send({'type': 'echo.error', 'data': keyed_message(key, self.settings.ui_language, **params)})

    def deadline_error(self, audio_frames):
        if not audio_frames:
            return keyed_message('echo.microphone-unavailable', self.settings.ui_language, stage='microphone')
        if not self.transcripts:
            return keyed_message('echo.silence-timeout', self.settings.ui_language, stage='microphone')
        return keyed_message('echo.deadline', self.settings.ui_language, seconds=MAX_SECONDS)

    def is_current(self, uid, revision):
        return False

    def transition(self, uid, status, reason=None):
        return None

    def fail_active(self):
        return None

    async def playback_finished(self, uid, revision):
        return None

    def report_client_error(self, data):
        return None

    def report_audio_health(self, health):
        return None

    def disconnect(self):
        if self.closed:
            return
        self.connected, self.closed = False, True
        for future in self.pending_speech.values():
            if not future.done():
                future.cancel()
        for task in tuple(self.speech_tasks):
            task.cancel()
        self.telemetry.call_ended('disconnected')
