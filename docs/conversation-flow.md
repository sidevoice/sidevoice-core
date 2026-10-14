# Conversation flow protocol (task T)

This is the core contract for the voice, connector, web, desktop and playground implementations of [sidevoice/.github#19](https://github.com/sidevoice/.github/issues/19). It describes the wire shape implemented by this branch. Consumers should pin a full core source SHA while developing together. These fields are part of one protocol change; no old client fallback is promised.

## Identities and ordering

- `thread_id` is the conversation. `epoch` is a `u64` local to that conversation, initially `0`. Core advances it exactly once when it accepts a qualified spoken turn start, including a turn that later ends empty. Focus, typing, skip, output settings and reconnect do not advance it.
- `revision` is the existing per-call routing boundary. It is retained on all frames. It does not authorize moving a reply to a newer epoch or session. Socket `seq` remains the resume cursor. Journal `seq` remains the history/pull cursor.
- `turn_id` identifies one device turn within `session_id`. Repeated starts do not advance the epoch again. A turn's target is captured at start; its pause and final transcription remain open until its terminal result. For offline turns, the finished frame must carry the captured `thread_id` and `binding_id`; core assigns the epoch once on acceptance.
- One `utterance_id` names one publication. A retry with the same content and context returns its original admission disposition. Reuse with different content or context is terminally refused. Publication order determines the order of `voice-reply` frames to each eligible session; same-epoch replies are not replaced.

## Device to core

A call hello may include `data.speech: {"on": true}` or `{ "on": false, "reason": "voice_off" | "agents_muted" }`. The reliable `voice-speech` frame has `data: {session_id, client_msg_id, on, reason?}` and is acknowledged by the ordinary client-message acknowledgement. Mic mute is independent of this output setting. Core accepts only `voice_off` or `agents_muted` when `on` is false.

`voice-user-turn` keeps its existing `started`, `finished`, `cancelled` phases, `session_id`, `client_msg_id`, and `turn_id`. A finished offline turn additionally supplies its captured `thread_id` and focus `binding_id` (the ID in the selected focus view, not the connector's `binding.register` ID); `merged: true` on a terminal frame transfers an empty-turn obligation to the continuing turn. Core answers a start with `voice-user-turn` carrying `revision`, `epoch`, and `thread_id`. A typed message uses `/api/presentation/text`; its receipt includes the current `epoch` without incrementing it.

`voice-playback` still supplies `utterance_id`, `status`, optional `reason`, and `heard_chars` (a Unicode scalar count, never bytes). `status: interrupted` or `unplayed` with `reason: user_skipped` means heard by choice. Output withdrawal reasons include `agents_muted` and `voice_off`. A later confirmed extent can increase the aggregate but cannot reduce it.

## Core to device

`voice-reply.data` contains `session_id`, `utterance_id`, call `revision`, `reply_revision`, conversation `epoch`, `thread_id`, `history_id`, `text`, and optional `language`/`replay`/`requested`. `voice-reply-withdrawn` remains explicit and names all affected `utterance_ids` and a reason. A client must stop a withdrawn reply and report its actual extent or never-started fate. It must not start a previously received reply after reconnect. Core does not replay missed `voice-reply` frames.

History rows include `epoch`, `audio_reason`, `heard_chars`, `globally_heard`, and `replayable`. Sidevoice-only notices have `role: note`, `text: ""`; they are system facts, not speech bubbles attributed to the person. Participant views include current `epoch`, `unheard: {count, since} | null`, and `working: bool | null`. Working is a fact only: the web applies the three second display delay.

## Core and connector

`binding.register` returns `epoch` and `initial_reply_context: {thread_id, session_id, revision, epoch}` for speak-first. In that initial context, `session_id` is `binding:<binding_id>` and `revision` is `0`; use the returned values as issued. After input arrives, use that input's session, revision and epoch. Invented session IDs are refused. `speech.publish` requires `event_id`, `utterance_id`, `binding_id`, `session_id`, `revision`, `epoch`, and `text`; `history_only: true` records a request held across a connector outage without speaking it. A current-epoch reply while any captured turn is open returns terminal `status: rejected`, `reason: user_speaking`, `error: room.user_speaking`, and the wait instruction. It has no assistant row. The V3 RPC result echoes `event_id` and `utterance_id` even on refusal, so a connector can retire its outbox item. A stale epoch is written as `text_only` with reason `newer_turn` and is never spoken. A future epoch is terminally refused. An identical retry retains the first disposition even if playback later changes. Other `text_only` reasons include `agents_muted`, `voice_off`, `inactive_conversation`, `disconnected`, `call_ended`, `connector_disconnected` and `queue_full`.

Push `input.deliver` and pull `input.pull` carry the same content fields: `message_id`, `session_id`, `revision`, `epoch`, `channel`, `text`, optional `notice`, and optional `unheard`. Push also carries `event_id`, `binding_id`, and `thread`; pull carries `arrival_time` and `cursor`. `channel` is `voice` for the person's spoken or typed words and `note` for Sidevoice-only messages (`text: ""`). A notice does not create an epoch. Pull claims a note until its `message_id` is acknowledged or the claim is released, exactly as for user words. Push retries use the same row and message ID.

Example not-played evidence:

```json
{
  "message_id": "...", "session_id": "...", "revision": 4, "epoch": 2,
  "channel": "voice", "text": "Wait, another point.",
  "notice": {"kind": "reply_not_played", "instruction": "This reply was not played. Use the current context and resend what still matters."},
  "unheard": {"count": 1, "replies": [{
    "utterance_id": "...", "history_id": "...", "session_id": "...",
    "revision": 3, "epoch": 1, "text": "The original full reply.",
    "fate": "interrupted", "reason": "newer_turn", "cut": false,
    "heard_chars": null
  }]}
}
```

Notice kinds are `reply_interrupted`, `reply_not_played`, `turn_empty`, `user_returned`, and `reply_skipped`. Skip evidence appears in `notice.skipped`, outside `unheard`. For a no-words turn with a refused speech call, `channel: note`, `text: ""`, `notice.kind: turn_empty` and the resend-if-relevant instruction are queued immediately. Return notices are queued after five seconds of eligible silence if globally unheard replies remain; no notice is queued when another device heard or skipped them. Each evidence envelope contains at most eight full replies in publication order; extra batches are correlated ordinary notices. A null `heard_chars` means no extent was confirmed. Consumers split the full text by Unicode scalar count, never by UTF-8 bytes or UTF-16 code units.

Manual replay uses `/api/presentation/replay` and a new playback ID linked to the original history row. Returning alone never requests it. Replay is refused while output is off or a user turn is open.
