# T0 reference revisions and contract handoff

The Python Core baseline and this fixture are at
`4d6df599239602954a3c6ab503c642eeeab5ca12`.
`hola-sala-16k.wav` is the synthesized nonprivate 16 kHz mono sample documented in
`tests/fixtures/README.md`; SHA-256: `a68664544e41df96bc15d2ce5194797e53ab47be223a841639ea16064030eff0`.

Reference clients inspected at these immutable revisions; T0 does not yet claim their
interoperability, which begins in T3 and T5:

| Repository | Revision | Boundary to preserve |
|---|---|---|
| sidevoice-web | `140115fd5122c9652763b0c5e47105f21f4cee95` | Browser raw WebSocket (`/api/presentation/ws`) and device STT/TTS messages |
| sidevoice-desktop | `1b3e4946620ea02eb9a1724c19033cfae5af8d40` | Native shell and bundled web client |
| sidevoice-connector | `ddb4bd1e0c95e7997787fe4ab98e7d812fbe2390` | Connector link v2 and local JSON-RPC v3 |

T3's hosted contract uses the actual JavaScript Socket.IO v2 link from the pinned
connector revision above. The Rust v3 connector proof is pinned separately at
`8c1a3df0c8c88040d9bfeeb880d17c04a1e823f1`, and the browser contract test
comes from the pinned web revision above. `.github/workflows/rust-t3.yml` fails if
any checkout, build, or test is missing. Its Linux functional steps run inside a
network-disabled Ubuntu 24.04 image; Core and the Rust proof are built by the hosted
Ubuntu 24.04 runner. That pairing currently requires glibc 2.39 or newer in the
execution image. T7 packaging must carry this native ABI floor into its relocation
and supported Linux target checks.

`tests/rust_t3_hosted.py` covers real v2 and v3 connector handshakes, binding,
typed delivery, publication, browser playback receipts, ordered replies, a slow
host scan beside unrelated input, reconnect and an old-generation ACK. The test
uses a deterministic local queue command for the Rust proof. It establishes the
text and wire contract; the media path and audible playback belong to T5.

`rust/types.rs` carries application values, without transport or Rustvani types. A call
has `room_id`, `session_id`, and `revision`. `SpeechStage` mirrors the Python
`place/model/options/build` data; T2 owns catalogue validation and defaults.
`CallSettings` lists the Python `LanguageSettings` fields; the value comes from the
client and is call scoped, not persisted by Core. `TranscriptResult` uses the session
and request IDs to correlate device/provider transcription. `SpeechResult` carries
utterance/revision, encoded audio and optional alignment; T4/T5 decide its exact
provider/device representation. These Rust structs are internal and are not a new
wire or persistence schema.

Only `rust/pipeline.rs` may expose Rustvani types internally. T5 owns the full
frame/pipeline lifecycle, turn buffering and cancellation; the T0 detector probe
is a finite asset and inference proof, not a call implementation.
