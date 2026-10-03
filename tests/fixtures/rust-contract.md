# T0 reference revisions and contract handoff

The Python Core baseline and this fixture are at
`4d6df599239602954a3c6ab503c642eeeab5ca12`.
`hola-sala-16k.wav` is the synthesized nonprivate 16 kHz mono sample documented in
`tests/fixtures/README.md`; SHA-256: `a68664544e41df96bc15d2ce5194797e53ab47be223a841639ea16064030eff0`.

Reference clients inspected at these immutable revisions; T0 does not yet claim their
interoperability, which begins in T3 and T5:

| Repository | Revision | Boundary to preserve |
|---|---|---|
| sidevoice-web | `140115fd5122c9652763b0c5e47105f21f4cee95` | Browser Socket.IO and device STT/TTS messages |
| sidevoice-desktop | `1b3e4946620ea02eb9a1724c19033cfae5af8d40` | Native shell and bundled web client |
| sidevoice-connector | `ddb4bd1e0c95e7997787fe4ab98e7d812fbe2390` | Connector link v2 and local JSON-RPC v3 |

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
