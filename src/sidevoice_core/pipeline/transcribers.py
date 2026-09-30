"""One transcription per user turn, by whichever provider the room chose.

The pipeline decides where a turn ends (VAD plus a timer, or smart-turn); this
module only turns audio into text: a turn the pipeline closed, or a block of PCM a
browser captured while it had no socket. Two providers share one contract:
OpenAI's API, called from here, and the browser that is speaking, which already
runs Whisper locally and is asked over its own WebSocket. Both get the same WAV
and both go through the same speech gate and text filters.
"""
import asyncio
import base64
import time
import uuid
from dataclasses import dataclass, field

from loguru import logger
from pipecat.audio.utils import pcm_to_wav
from pipecat.frames.frames import InputTransportMessageFrame
from pipecat.services.settings import STTSettings
from pipecat.services.stt_service import SegmentedSTTService

from .speech_filter import SegmentSpeechGate, isolated_foreign_script, transcription_confidence, unreliable_text


@dataclass
class Transcript:
    text: str
    confidence: float | None = None
    metrics: dict = field(default_factory=dict)


class OpenAITranscriber:
    kind = 'openai'

    def __init__(self, api_key, *, model, language=None, prompt=None, client=None):
        if client is None:
            from openai import AsyncOpenAI
            client = AsyncOpenAI(api_key=api_key)
        self.client, self.model, self.language, self.prompt = client, model, language, prompt

    async def transcribe(self, wav):
        kwargs = {'file': ('audio.wav', wav, 'audio/wav'), 'model': self.model}
        if self.model.startswith('whisper'):
            kwargs['response_format'] = 'verbose_json'
        elif 'diarize' not in self.model:
            kwargs.update(response_format='json', include=['logprobs'])
        if self.language:
            kwargs['language'] = self.language
        if self.prompt:
            kwargs['prompt'] = self.prompt
        response = await self.client.audio.transcriptions.create(**kwargs)
        return Transcript(text=str(getattr(response, 'text', '') or '').strip(),
                          confidence=transcription_confidence(response))


class BrowserTranscriber:
    """The browser keeps its Whisper; the room hands it each finished turn and waits for the text."""
    kind = 'browser'
    TIMEOUT = 90.0

    def __init__(self, send, session_id, *, language=None, timeout=TIMEOUT):
        self.send, self.session_id, self.language, self.timeout = send, session_id, language, timeout
        self.pending = {}

    async def transcribe(self, wav):
        request_id = str(uuid.uuid4())
        future = asyncio.get_running_loop().create_future()
        self.pending[request_id] = future
        try:
            self.send({'type': 'voice-transcribe', 'data': {
                'session_id': self.session_id, 'request_id': request_id,
                'audio_base64': base64.b64encode(wav).decode('ascii'), 'language': self.language}})
            return await asyncio.wait_for(future, self.timeout)
        finally:
            self.pending.pop(request_id, None)

    def receive(self, message):
        """A browser message for one of our requests settles it; anything else is not ours."""
        if not isinstance(message, dict) or message.get('type') not in {'voice-transcript', 'voice-transcript-error'}:
            return False
        data = message.get('data') if isinstance(message.get('data'), dict) else {}
        future = self.pending.get(data.get('request_id'))
        if future is None or future.done():
            return True
        if message['type'] == 'voice-transcript':
            metrics = data.get('metrics') if isinstance(data.get('metrics'), dict) else {}
            future.set_result(Transcript(text=str(data.get('text') or '').strip(), metrics=metrics))
        else:
            future.set_exception(RuntimeError(str(data.get('error') or 'Transcription failed in the browser.')))
        return True

    def cancel(self):
        for future in self.pending.values():
            if not future.done():
                future.cancel()


class TurnTranscriber(SegmentedSTTService):
    """Buffers every VAD fragment of a turn and transcribes the turn once, when the room says it ended.

    Pipecat's segmented service would upload each VAD fragment on its own; a short
    hesitation then deprived the provider of the rest of the sentence and made
    language detection unstable. Here fragments accumulate and `transcribe_turn`
    is called by the user-turn handler, whatever strategy closed the turn.
    """

    def __init__(self, provider, *, speech_gate=None, language=None, **kwargs):
        kwargs.setdefault('settings', STTSettings(model=getattr(provider, 'kind', 'turn'), language=None))
        super().__init__(**kwargs)
        self.provider = provider
        self.language = language
        self.speech_gate = speech_gate or SegmentSpeechGate()
        self.filter_stats = {'audio_rejected': 0, 'confidence_rejected': 0, 'submitted': 0, 'script_rejected': 0}
        self.on_message = None   # browser messages that are not a transcription answer go here
        self.vad_stopped_at = None  # when the detector last reported the pause that may end this turn
        self._turn_audio = bytearray()

    async def run_stt(self, audio):
        # Segments are never transcribed one by one; see transcribe_turn.
        return
        yield  # pragma: no cover

    async def process_frame(self, frame, direction):
        await super().process_frame(frame, direction)
        if isinstance(frame, InputTransportMessageFrame):
            handled = hasattr(self.provider, 'receive') and self.provider.receive(frame.message)
            if not handled and self.on_message is not None:
                self.on_message(frame.message)

    async def _handle_user_stopped_speaking(self, frame):
        self._user_speaking = False
        self.vad_stopped_at = time.monotonic()
        self._turn_audio.extend(self._audio_buffer)
        self._audio_buffer.clear()

    async def cancel(self, frame):
        if hasattr(self.provider, 'cancel'):
            self.provider.cancel()
        self._turn_audio.clear()
        await super().cancel(frame)

    def take_turn_audio(self):
        """Everything this turn has heard so far, handed over now: what arrives next is another turn's."""
        pcm = bytearray(self._turn_audio)
        self._turn_audio.clear()
        if self._user_speaking:
            # The turn closed without a VAD stop (audio went idle): the speech is still in the live buffer.
            pcm.extend(self._audio_buffer)
            self._audio_buffer.clear()
        return bytes(pcm)

    async def transcribe_turn(self, pcm=None):
        """The whole turn's audio to text, or None when there was nothing to transcribe."""
        pcm = self.take_turn_audio() if pcm is None else pcm
        if not pcm:
            return None
        return await self.recognise(bytes(pcm) + self._trailing_silence(), self.sample_rate)

    async def transcribe_audio(self, pcm, sample_rate=None):
        """Audio this room never heard as it happened — what a browser captured while its socket was
        down — through the same gate, the same provider and the same filters as a turn.

        It reaches recognition and nothing else. The detector, the turn strategy and the aggregator
        never see it, so audio spoken to a session that no longer exists cannot open a turn here.
        """
        if not pcm:
            return None
        return await self.recognise(bytes(pcm), sample_rate or self.sample_rate)

    async def recognise(self, pcm, sample_rate):
        """One block of PCM to text: usage, the speech gate, the provider and the text filters."""
        self._record_stt_audio_usage(pcm)
        await self.emit_stt_usage_metrics()
        wav = pcm_to_wav(pcm, sample_rate)
        evidence = await asyncio.to_thread(self.speech_gate.assess, wav)
        # Only acoustic/decoder measurements: no waveform or transcript archive.
        measurement = {'speech_ms': evidence.speech_ms, 'peak_probability': round(evidence.peak_probability, 4),
                       'mean_logprob': None, 'decision': evidence.reason}
        history = self.filter_stats.setdefault('recent_segments', [])
        history.append(measurement)
        del history[:-20]
        if not evidence.accepted:
            self.filter_stats['audio_rejected'] += 1
            logger.info('STT ignored non-speech turn: speech_ms={} peak_probability={:.3f}',
                        evidence.speech_ms, evidence.peak_probability)
            return Transcript('')
        self.filter_stats['submitted'] += 1
        result = await self.provider.transcribe(wav)
        measurement['mean_logprob'] = result.confidence
        measurement['decision'] = 'accepted'
        if isolated_foreign_script(result.text) and self.language != 'hi':
            measurement['decision'] = 'isolated_foreign_script'
            self.filter_stats['script_rejected'] += 1
            logger.info('STT ignored isolated non-Latin text: {}', measurement)
            return Transcript('', result.confidence, result.metrics)
        if unreliable_text(result.text, result.confidence):
            measurement['decision'] = 'low_confidence'
            self.filter_stats['confidence_rejected'] += 1
            logger.info('STT ignored a low-confidence transcript')
            return Transcript('', result.confidence, result.metrics)
        logger.info('STT turn evidence: {}', measurement)
        return result

