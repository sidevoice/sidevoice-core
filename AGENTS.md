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

Read `README.md` and `HANDOVER.md`. Refusals and errors that a person will read must be translatable: a stable key with its parameters, rendered by
the client, or whatever rubasace/sidevoice#128 settles — never a finished sentence in one language.
