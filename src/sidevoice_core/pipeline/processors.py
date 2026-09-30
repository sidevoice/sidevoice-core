"""The processors a call's pipeline carries between its input and its output.

`NoInference` swallows the context the user aggregator would hand an LLM (none lives here);
`PresentationGate` is the last epoch check before synthesis; `PresentationPlayback` turns ordered
transport output into playback transitions. The gate and the playback observer drive whatever call
they are handed through the methods `CallPort` names — never the control plane itself.
"""
from pipecat.frames.frames import LLMContextFrame, TTSAudioRawFrame, ErrorFrame
from pipecat.processors.frame_processor import FrameProcessor, FrameDirection

from .frames import PresentationBoundary, PresentationSpeech


class NoInference(FrameProcessor):
    async def process_frame(self, frame, direction):
        await super().process_frame(frame, direction)
        if not isinstance(frame, LLMContextFrame):
            await self.push_frame(frame, direction)


class PresentationGate(FrameProcessor):
    """Last epoch check before TTS; queued stale requests never synthesize."""
    client = None

    async def process_frame(self, frame, direction):
        await super().process_frame(frame, direction)
        if self.client and direction == FrameDirection.DOWNSTREAM:
            if isinstance(frame, (PresentationSpeech, PresentationBoundary)):
                if not self.client.is_current(frame.utterance_id, frame.revision):
                    return
                if isinstance(frame, PresentationSpeech):
                    if hasattr(self.client.tts, 'select_language'):
                        self.client.tts.select_language(frame.language)
                    self.client.transition(frame.utterance_id, 'synthesizing')
        if self.client and isinstance(frame, ErrorFrame):
            self.client.fail_active()
        await self.push_frame(frame, direction)


class PresentationPlayback(FrameProcessor):
    """Observe ordered transport output, never silence timeout or 'heard'."""
    client = None
    current = None

    async def process_frame(self, frame, direction):
        await super().process_frame(frame, direction)
        if self.client and direction == FrameDirection.DOWNSTREAM:
            if isinstance(frame, PresentationBoundary):
                if not frame.end:
                    self.current = (frame.utterance_id, frame.revision)
                else:
                    await self.client.playback_finished(frame.utterance_id, frame.revision)
                    if self.current == (frame.utterance_id, frame.revision):
                        self.current = None
            elif isinstance(frame, TTSAudioRawFrame) and self.current:
                uid, rev = self.current
                if self.client.is_current(uid, rev):
                    self.client.transition(uid, 'playing')
        await self.push_frame(frame, direction)
