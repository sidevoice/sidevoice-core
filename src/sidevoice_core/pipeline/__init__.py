"""The voice pipeline: Pipecat, one instance per call, and the providers it can use.

This package is the lower half of the core and imports nothing above it — not the control plane,
not a web framework (`tests/test_boundaries.py`). What it needs from the call it serves is named
once, as `call.CallPort`, and whatever object satisfies it is the control plane's business. That
is what lets a pipeline run somewhere else later: it is the part that scales.

Transcription is a provider behind one contract (`transcribers.TurnTranscriber`): OpenAI called
from here, or the client itself (`ClientTranscriber`: its own Whisper answers). Synthesis is the
client's (Kokoro) or ElevenLabs rendered here (`synthesis`).
"""
