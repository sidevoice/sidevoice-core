# Rust core layout

The root `Cargo.toml` declares one package; its sources live under `rust/`. `main.rs` starts the core.

## Tests

`cargo test --locked` runs the unit tests and the integration tests in `tests/`, most of which drive the built
binary as a process. Every peer is a local fake written in Rust: a browser's call socket, a connector on the v2
(Socket.IO) and v3 (JSON-RPC) links, the room on the rendezvous link. Nothing reaches the network, and no other
repository is checked out.

Compatibility with the latest published releases of the connector and the web client is
`cargo xtask compat`, run weekly by `.github/workflows/compat.yml`.
