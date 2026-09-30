# sidevoice-core

Sidevoice's core: the part of the old room server that holds conversations and runs voice, moved
to where the agents run. One core per **node** (the machine running Claude Code, Codex…), started
and supervised by that machine's connector (`@sidevoice/uplink`); the hosted room shrinks to
rendezvous and relay. Private while it takes shape (Sidevoice client/server split, 2026-09-30).

## Provenance

Extracted from [`rubasace/sidevoice`](https://github.com/rubasace/sidevoice) at commit
`7565663` (`apps/server/sidevoice/*`, `apps/server/tests/*`, `packages/browser-audio/catalog.json`).
Commit `699ca3c` in this repository is that code copied verbatim under its new names; every change
since is in this repository's history. The design it follows is `docs/LOCAL_MODE_PLAN.md` §2.3 and
§10 steps 1–4 in rubasace/sidevoice (branch `towering-penguin`, commit `aebebf6`), with the
overnight brief of 2026-09-30 taking precedence where they differ.

## Shape

```
src/sidevoice_core/
  runtime.py     where the node keeps its files, what version it is — imports neither half
  pipeline/      Pipecat, one instance per call, and the providers it can use
  control/       the node's control plane: conversations, history, listeners, the connector link
  server/        the thin web surface (FastAPI + Socket.IO) that carries both
```

- **pipeline** — `call.CallPipeline` (transport in, Silero VAD, smart-turn v3 or a timer, one
  transcription provider, the output-side processors), `call.VoiceCall` (the turn flow: open the
  epoch, transcribe once, merge a breath, hold a resumed sentence, deliver in order), and the
  providers: the client transcribes (`transcribers.ClientTranscriber`) or OpenAI from here;
  the client synthesizes (Kokoro) or ElevenLabs from here (`synthesis`). What it needs from the
  call it serves is one protocol, `call.CallPort`. **It never imports the control plane.**
- **control** — `room.Room` / `room.RoomClient` (shared room state vs one listener's state, see
  rubasace/sidevoice `docs/MULTI_CLIENT_ROOM.md`), the journal (`history`), the delivery pump and
  speech intake for the connector (`connectors`), and `calls.run_call`, which creates one pipeline
  per call. Domain refusals are `refusal.Refusal(status, detail)`, never an HTTP exception.
- **server** — `create_app()`: the client REST surface (`/api/presentation/*`), the call socket
  (`/api/presentation/ws`) and the connector link (`/api/connectors/link`, Socket.IO, namespace
  `/connectors`) the machine's connector uses exactly as it used to use a room's; the rendezvous with
  the hosted room (`rendezvous.py`: the node dials the room, or accepts a room that dials it, and
  serves the relayed requests by making them to itself); the microphone over WebRTC
  (`webrtc.py`: `rtc/config`, `rtc/offer`, aiortc, the track fed into the same pipeline input).
  The contract for all of it is rubasace/sidevoice `docs/RENDEZVOUS.md`.

`sidevoice-core --data-dir D --port P --room-credential F --idle-exit S` is how the connector starts
it; `D/core.json` (0600) says where it listens and with which credential the connector links.

Neither `pipeline` nor `control` imports a web framework; `tests/test_boundaries.py` enforces both
rules by importing every module in a fresh interpreter and by reading every import statement.

## Run it

```
uv venv --python 3.12 && uv pip install -e '.[test]'
.venv/bin/python -m pytest -q
sidevoice-core --port 8767          # or: python -m sidevoice_core.server
```

Data (provider keys, the journal's durable state) lives in `SIDEVOICE_CORE_DATA_DIR`, else
`VOICE_RUNTIME_ROOT`, else `~/.sidevoice/core`. Tests that need a rubasace/sidevoice checkout
(the real connector, the protocol package) run when `SIDEVOICE_REPOSITORY` names one and skip
otherwise.

No PyTorch: Silero and smart-turn v3 run on onnxruntime.
