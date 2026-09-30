"""Wire format between a client and its call: binary frames carry microphone PCM up,
text frames carry JSON app messages both ways. No audio flows down; the browser synthesizes.

The microphone may also arrive another way — a WebRTC track, when the client negotiated one — and the
client says which with `voice-media` (`socket` or `webrtc`). This serializer owns that switch because
it owns the socket's frames: while the client says `webrtc`, PCM on the socket is not audio any more
(it still proves the client is there), so one voice can never be heard twice."""
import json
import time

from pipecat.frames.frames import (Frame, InputAudioRawFrame, InputTransportMessageFrame,
                                   OutputTransportMessageFrame, OutputTransportMessageUrgentFrame)
from pipecat.serializers.base_serializer import FrameSerializer

MIC_SAMPLE_RATE = 16000
MIC_CHANNELS = 1


class BrowserFrameSerializer(FrameSerializer):
    """Binary in: 16-bit little-endian PCM at the declared format. Text: one JSON object per frame."""

    def __init__(self, sample_rate=MIC_SAMPLE_RATE, channels=MIC_CHANNELS):
        # RTVI events (transcriptions, speaking state) are exactly what the page listens for.
        super().__init__(FrameSerializer.InputParams(ignore_rtvi_messages=False))
        self.sample_rate, self.channels = sample_rate, channels
        # When this socket last carried anything at all, audio or message: one browser's whole
        # evidence of being alive, and what the call's keepalive reads (`browser_heartbeat.py`).
        self.last_frame_at = time.monotonic()
        # What the microphone actually delivered, so a silent call can be told from a broken one.
        self.audio_frames = self.audio_bytes = 0
        self.last_audio_at = None
        self.last_audio_gap_ms = self.max_audio_gap_ms = 0
        self.audio_gap_count = 0
        self.media_path = 'socket'   # where this client's microphone comes from, as the client last said

    def heard(self, size, now=None):
        """Microphone audio arrived, by whatever path: the evidence of life and the gap statistics."""
        now = self.last_frame_at = time.monotonic() if now is None else now
        if self.last_audio_at is not None:
            gap_ms = max(0, round((now - self.last_audio_at) * 1000))
            self.last_audio_gap_ms = gap_ms
            self.max_audio_gap_ms = max(self.max_audio_gap_ms, gap_ms)
            if gap_ms > 250:
                self.audio_gap_count += 1
        self.last_audio_at = now
        self.audio_frames += 1
        self.audio_bytes += size

    async def serialize(self, frame: Frame):
        if isinstance(frame, (OutputTransportMessageFrame, OutputTransportMessageUrgentFrame)):
            return json.dumps(frame.message)
        return None  # Nothing else crosses to the browser: no audio, no pipeline control.

    async def deserialize(self, data):
        self.last_frame_at = time.monotonic()
        if isinstance(data, (bytes, bytearray)):
            usable = len(data) - len(data) % (2 * self.channels)
            if not usable or self.media_path != 'socket':
                return None
            self.heard(usable, self.last_frame_at)
            return InputAudioRawFrame(audio=bytes(data[:usable]), sample_rate=self.sample_rate,
                                      num_channels=self.channels)
        try:
            message = json.loads(data)
        except ValueError:
            return None
        if not isinstance(message, dict):
            return None
        if message.get('type') == 'voice-media' and isinstance(message.get('data'), dict):
            if message['data'].get('path') in {'socket', 'webrtc'}:
                self.media_path = message['data']['path']
        return InputTransportMessageFrame(message=message)


def session_message(session_id, serializer):
    """First text frame of a call: the id the room minted, the PCM format it expects, and what the room is."""
    from ..runtime import build_info
    return {'type': 'voice-session', 'data': {'session_id': session_id,
                                              'sample_rate': serializer.sample_rate,
                                              'channels': serializer.channels,
                                              'room': build_info()}}
