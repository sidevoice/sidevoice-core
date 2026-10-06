"""The model catalogue (sidevoice/sidevoice-core#21): which models
exist, the engines each runs on, the options each family takes, and which of them a place can run.

This package owns it. `catalog.json` (`assets/catalog/models/`, which the Rust core compiles in) is the one written by hand: clients that need it at build time carry
a copy generated from this file, never edited, and every node serves it at `GET /api/models/catalog`.
`offers.py` is the reference resolver; the clients implement the same function in their own languages and
pass the same `vectors.json`, shipped beside the catalogue.

Like the pipeline, it imports nothing above it and no web framework (`tests/test_boundaries.py`).
"""
