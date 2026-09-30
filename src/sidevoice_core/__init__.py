"""Sidevoice's core: the node's control plane and one voice pipeline per call.

`sidevoice_core.pipeline` is Pipecat and the providers, one instance per call, and imports nothing
above it. `sidevoice_core.control` is the node's conversations, history, listeners and connector
link, and creates pipelines. `sidevoice_core.server` is the thin web surface that serves both;
neither half imports a web framework (`tests/test_boundaries.py`).
"""
