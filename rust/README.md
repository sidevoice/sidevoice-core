# Rust foundation layout

This is the T0 standalone Rust candidate; the selected production Core remains Python.
The root `Cargo.toml` declares one package. Rust sources live under `rust/` so the
existing Python `src/` remains intact. `main.rs` offers a finite, machine-readable
detector self-test for hosted CI. `lib.rs` exposes `types` and `pipeline`; only the
pipeline module imports Rustvani. T1/T2 can add their owned modules under `rust/`
without moving either target or introducing another package.

The self-test takes a WAV file and staged asset directory:

```text
sidevoice-core-rust --self-test path/to/16k-mono.wav path/to/models
```

The output is JSON for CI, with `error_key` on failure. It is not the future
production launch interface. `scripts/stage_rust_models.py` reads the pinned
`assets/rust-models.json` and verifies every model SHA-256 before placing assets
in Rustvani's expected cache. The self-test always passes explicit model paths,
so its inference does not trigger a first-use download.
