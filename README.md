# sidevoice-core

Sidevoice lets you talk with your coding agents instead of reading them: you speak, the agent hears you as a
message in its conversation, and its answers come back as speech. **sidevoice-core** is the part that runs on the
machine where the agents run (the **node**): it holds that machine's conversations, runs one voice pipeline per
call (voice activity, turn detection, transcription, speech), and decides which devices may use it.

It is started and supervised by that machine's Sidevoice connector. A device (the web page, the desktop app)
talks to the node directly, or through a **room**: a hosted rendezvous and relay for nodes that are not reachable
from where the device is.

## Run it

Python 3.12, with [uv](https://docs.astral.sh/uv/):

```
uv venv --python 3.12 && uv pip install -e '.[test]'
.venv/bin/python -m pytest -q
.venv/bin/sidevoice-core --port 8767          # or: python -m sidevoice_core.server
```

It listens on loopback. `sidevoice-core --help` lists its options; the connector starts it as
`sidevoice-core --data-dir D --port P --room-credential F --idle-exit S`, and `D/core.json` (mode 0600) then says
where it listens and with which credential the connector links.

No PyTorch: Silero VAD and smart-turn v3 run on onnxruntime.

## Configuration

Data — provider keys in `integrations.json`, the paired devices, the node's identity key, the conversation
journal — lives in `SIDEVOICE_CORE_DATA_DIR`, else `~/.sidevoice/core`. Every secret file there is written
0600 from creation.

| Variable | What it does |
|---|---|
| `VOICE_STT_API_KEY`, `VOICE_ELEVENLABS_API_KEY` | Provider keys for OpenAI transcription and ElevenLabs speech, for a headless host. A key saved from a paired device wins over them. |
| `SIDEVOICE_PUBLIC_URLS` | Comma-separated addresses where devices can reach this node directly, put into pairing codes. Each must be `https://`; a plaintext one is left out of codes (see below). |
| `SIDEVOICE_TRUSTED_CLUSTER_HOSTS` | Comma-separated hosts that may receive credentials over plaintext HTTP. Empty by default. See below. |
| `SIDEVOICE_ALLOWED_HOSTS`, `SIDEVOICE_ALLOWED_ORIGINS` | Host names and page origins this node answers besides loopback, for a node someone made reachable. |
| `SIDEVOICE_CORE_HOST`, `SIDEVOICE_CORE_PORT` | Where to listen (default `127.0.0.1:8768`). |

## Security model

- **Devices.** Every route except discovery, the identity proof and redeeming a pairing code needs a device
  token. A device gets one by redeeming a one-time code the node issues (valid ten minutes); the node keeps only
  its hash, and revoking a device ends its open calls at once. Every paired device has the machine's full
  authority: there are no roles.
- **Discovery** (`GET /api/rendezvous`, `GET /api/device/identity`) answers without a token and says only what
  pairing needs: that this is a node, its fingerprint and public key, and a signature over the caller's nonce.
  The machine's name is given only to a device that redeemed a code.
- **Transport.** A pairing secret, device token or connector credential travels only over HTTPS, except to
  loopback and to the hosts in `SIDEVOICE_TRUSTED_CLUSTER_HOSTS`. An entry starting with a dot is a suffix
  (`.svc.cluster.local`), any other entry one exact host. Listing a host there states that the network between
  this node and that host is yours (a Kubernetes cluster's pod network, say) and that whatever can read it may
  read those credentials: a host name proves nothing about where it resolves, so none is trusted by its spelling.
- **Relays are trusted.** A room relaying a device's traffic terminates TLS on both sides. It sees the pairing
  secret redeemed through it, every device token, and all the traffic — conversations, voice, provider keys
  saved from a device — and could use them. The node's signed identity proves which node answered; it encrypts
  nothing. Use only a room you trust, or reach the node directly. End-to-end encryption between a device and the
  node's pinned identity is planned and is not there yet.

## Shape

```
src/sidevoice_core/
  runtime.py     where the node keeps its files, what version it is — imports neither half
  pipeline/      Pipecat, one instance per call, and the providers it can use
  control/       the node's control plane: conversations, history, listeners, the connector link, devices
  server/        the thin web surface (FastAPI + Socket.IO) that carries both
```

- **pipeline** — `call.CallPipeline` (transport in, Silero VAD, smart-turn v3 or a timer, one transcription
  provider, the output-side processors), `call.VoiceCall` (the turn flow), and the providers: the client
  transcribes or OpenAI does it from here; the client synthesizes (Kokoro) or ElevenLabs does it from here. The
  keys those providers are called with are the node's integrations (`integrations`): one per provider, in one
  0600 file, written from any paired device and never read back. What it needs from the call it serves is one
  protocol, `call.CallPort`. **It never imports the control plane.**
- **control** — the shared room state and each listener's (`room`), the journal (`history`), the delivery pump
  and speech intake for the connector (`connectors`), device pairing (`devices`), and `calls.run_call`, which
  creates one pipeline per call. Domain refusals are `refusal.Refusal(status, detail)`, never an HTTP exception.
- **server** — `create_app()`: the client REST surface (`/api/presentation/*`), the call socket
  (`/api/presentation/ws`), the connector link (`/api/connectors/link`, Socket.IO), the rendezvous with a room
  (`rendezvous.py`: the node dials the room, or accepts a room that dials it, and serves the relayed requests by
  making them to itself), the microphone over WebRTC (`webrtc.py`), device pairing (`devices.py`) and the
  transport rule above (`transport.py`).

Neither `pipeline` nor `control` imports a web framework; `tests/test_boundaries.py` enforces both rules.

Two test modules exercise the real connector and the browser's catalogue code; they run when
`SIDEVOICE_REPOSITORY` names a checkout of the connector and web source tree, and skip otherwise.

## Licence

MIT — see [`LICENSE`](LICENSE).
