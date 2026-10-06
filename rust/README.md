# Rust core layout

The root `Cargo.toml` declares one package. Rust sources live under `rust/` so the
existing Python `src/` remains intact. `main.rs` starts the core, and offers a finite,
machine-readable detector self-test. Only the pipeline module imports Rustvani.

The self-test takes a WAV file and staged asset directory:

```text
sidevoice-core-rust --self-test path/to/16k-mono.wav path/to/models
```

The output is JSON, with `error_key` on failure. It is not the production launch
interface: `cargo xtask dist` runs it on every packaged archive. `cargo xtask models`
reads the pinned `assets/rust-models.json` and verifies every model SHA-256 before
placing assets in Rustvani's expected cache. The self-test always passes explicit model
paths, so its inference does not trigger a first-use download.

## Tests

`cargo test --locked` runs the unit tests and the integration tests in `rust/tests/`, which
drive the built binary as a process. Every peer is a local fake written in Rust: a browser's
call socket and WebRTC track, a connector on the v2 (Socket.IO) and v3 (JSON-RPC) links, the
room on the rendezvous link, the cloud providers. Nothing reaches the network, and no other
repository is checked out. The tests that run the voice detectors need the models staged in
`RUSTVANI_CACHE_DIR` (`cargo xtask models`).

`cargo test --locked --all-features --test call_media --test provider_routes` runs the tests that
point the core at fake providers through the `hosted-fixtures` overrides. CI runs both steps: the rest
of the suite stays on the binary as shipped, because `hosted-fixtures` also turns off mDNS host
candidates, which `webrtc_answer` checks.

Compatibility with the latest published releases of the connector and the web client is
`cargo xtask compat`, run weekly by `.github/workflows/compat.yml`.
