"""Frames that carry one utterance through a server-side TTS pipeline.

The room uses them only for a client whose audio is rendered by the pipeline
instead of by the browser; they mark the epoch so a queued request that went
stale never reaches synthesis, and so completion is an ordered marker rather
than a timeout.
"""
from dataclasses import dataclass

from pipecat.frames.frames import DataFrame, TTSSpeakFrame


@dataclass
class PresentationSpeech(TTSSpeakFrame):
    utterance_id: str = ''
    revision: int = 0
    language: str | None = None


@dataclass
class PresentationBoundary(DataFrame):
    utterance_id: str = ''
    revision: int = 0
    end: bool = False
