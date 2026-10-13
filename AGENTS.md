# AGENTS.md

Rules for any coding agent (and person) working in this repository.

## Language of the code and of the product

- Code, identifiers, comments, commit messages and docs are in **English**.
- **Every user-facing text goes through i18n**: a key looked up in per-language message bundles (one file per
  language), with **English as the fallback** when a key is missing. No literal sentence in any language in
  components, server refusals and errors shown to people, native UI (tray, windows, notifications, installer or CLI
  output meant for people). Adding a text means adding its key to the English bundle; other languages may lag.
- Language defaults: the device's system language when we support it, otherwise English. Never Spanish, or any
  other language, as a hard-coded default.
- What reaches the agent (MCP instructions, tool results, the `[Sidevoice]` trailer) is English; it is not UI.

## Before changing things

Read `README.md`. Refusals and errors that a person will read must be translatable: a stable key with its parameters, rendered by
the client, or whatever sidevoice/sidevoice-web#17 settles — never a finished sentence in one language.

## Protocol compatibility

The app or web page, the core and the connector (with its MCP side) may run different versions: a remote machine
installed with npx, an agent session keeping an older `sidevoice mcp`. Every protocol between them — HTTP requests and
responses, query parameters, WebSocket messages, connector JSON-RPC frames — evolves like a production API
(sidevoice/sidevoice-core#112).

- **Allowed without a version bump:** new optional fields in requests, responses, WebSocket messages and connector
  frames; new routes, methods and message types; new enum values, as long as a reader that does not know them has a
  defined default behaviour.
- **Never:** removing or renaming a field or a route, changing its meaning or its type, or making an optional field
  required. To change something, add the new one, mark the old one deprecated, and remove it only once no supported
  version uses it.
- **Mandatory tolerance in every reader:** unknown fields are ignored, at the top level and nested (no
  `#[serde(deny_unknown_fields)]` on wire types, no check that an object's keys are within an allowed list, no size
  bound tighter than the frame's own). An unknown enum value gets a defined default: ignore the message or the field,
  or read it as the safest known value (`#[serde(other)]`, a default arm). A message is never refused because one
  optional part of it is unknown. Required fields and security-relevant values are still validated: tolerance means
  ignoring what is unknown, not accepting bad required data.
- **A protocol version number** (in `GET /api/host/status` and the connector/MCP handshake) changes only for a
  genuinely breaking change, so that the app can say "update this machine" instead of failing.

The contract tests that send each boundary's messages with fields and values the core does not know are in
`tests/connector_link.rs` (connector ↔ core) and `tests/room_frames.rs` (page ↔ core); a new message or reader gets
its case there.

## Public product information

For changes to user-visible behavior, supported platforms/models, setup, security/privacy practices, availability, limitations, or release/download details, follow the shared [public-information process](https://github.com/sidevoice/landing/blob/main/AGENTS.md#keep-public-product-information-current). Record the landing change/PR or a linked `sidevoice/landing` issue in the PR checklist. Landing issues are the follow-up triage queue.
