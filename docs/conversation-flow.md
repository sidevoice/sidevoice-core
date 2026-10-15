# Conversation flow protocol (task T)

This is the core contract for the voice, connector, web, desktop and playground implementations of [sidevoice/.github#19](https://github.com/sidevoice/.github/issues/19). It describes the wire shape implemented by this branch. Consumers should pin a full core source SHA while developing together. These fields are part of one protocol change; no old client fallback is promised.

## Identities and ordering

- `thread_id` is the conversation. `epoch` is a `u64` local to that conversation, initially `0`. Core advances it exactly once when it accepts a qualified spoken turn start, including a turn that later ends empty. Focus, typing, skip, output settings and reconnect do not advance it.
- `revision` is the existing per-call routing boundary. It is retained on all frames. It does not authorize moving a reply to a newer epoch or session. Socket `seq` remains the resume cursor. Journal `seq` remains the history/pull cursor.
- `turn_id` identifies one device turn within `session_id`. Repeated starts do not advance the epoch again. A turn's target is captured at start; its pause and final transcription remain open until its terminal result. For offline turns, the finished frame must carry `offline: true`, the captured `thread_id`, and the focus `binding_id`; core assigns the epoch once on acceptance. An offline finish for an already open identity uses that turn's captured epoch/target; a completed or cancelled identity cannot be reopened.
- One `utterance_id` names one publication. A retry with the same content and context returns its original admission disposition. Reuse with different content or context is terminally refused. Publication order determines the order of `voice-reply` frames to each eligible session; same-epoch replies are not replaced.

## Device to core

A call hello may include `data.speech: {"on": true}` or `{ "on": false, "reason": "voice_off" | "agents_muted" }`. The reliable `voice-speech` frame has `data: {session_id, client_msg_id, on, reason?}` and is acknowledged by the ordinary client-message acknowledgement. Mic mute is independent of this output setting. Core accepts only `voice_off` or `agents_muted` when `on` is false.

`voice-user-turn` keeps its existing `started`, `finished`, `cancelled` phases, `session_id`, `client_msg_id`, and `turn_id`. A finished offline turn additionally supplies `offline: true`, its captured `thread_id`, and focus `binding_id` (the ID in the selected focus view, not the connector's `binding.register` ID). On a **cancelled** frame, `merged: true` transfers an empty-turn obligation to the continuing turn. On a **finished** frame, `merged: true` means its `text` already includes the earlier merged turns; core queues those words once and does not transfer them away. Core answers a start with `voice-user-turn` carrying `revision`, `epoch`, and `thread_id`. A typed message uses `/api/presentation/text`; its receipt includes the current `epoch` without incrementing it.

`voice-playback` still supplies `utterance_id`, `status`, optional `reason`, and `heard_chars` (a Unicode scalar count, never bytes). `status: interrupted` or `unplayed` with `reason: user_skipped` means heard by choice. Output withdrawal reasons include `agents_muted` and `voice_off`. A later confirmed extent can increase the aggregate but cannot reduce it.

### Voice #31 to core frame mapping

The voice handle ID is local to the voice engine. Each client records the association between the core `voice-reply.data.utterance_id` and the handle ID returned by `say()`, then sends the **core** ID in every `voice-playback` and `agent-interrupted` translation. Core recognizes playback `status: playing | heard | interrupted | unplayed | failed`. Unknown status values have no effect. The voice engine's `heard_chars`/`heard_until` are Unicode scalar counts; preserve them as nonnegative integers no larger than the original text's scalar length. A JavaScript UTF-16 offset must be converted before sending (for example, count code points in the prefix). A genuine played interruption may have `heard_chars: 0`; never convert it to `unplayed` merely because its confirmed prefix is zero.

Accepted `voice-playback.reason` values are `user_interrupted`, `newer_turn`, `user_skipped`, `focus_changed`, `call_ended`, `unheard`, `agents_muted`, `voice_off`, `connector_disconnected`, and `heard_elsewhere`. Unknown reasons are treated as absent. `status: failed` records `playback_failed` regardless of the supplied reason while retaining a supplied partial scalar extent.

| Voice event or handle outcome | `voice-playback` to core | Context |
| --- | --- | --- |
| `playing` event | `status: "playing"` | Send when sound starts. |
| `heard` | `status: "heard"` | Full hearing. |
| `heard-up-to`, `barge-in` | `status: "interrupted", reason: "user_interrupted", heard_chars` | The person qualified a turn while sound was playing. |
| `not-played`, `barge-in` | `status: "unplayed", reason: "newer_turn"` | Accepted reply never sounded before the qualified turn. |
| `heard-up-to` or `not-played`, `cancelled` | `status: "interrupted"` or `"unplayed"`; reason from the local action | An explicit skip maps to `user_skipped`; a core withdrawal keeps its withdrawal reason (`newer_turn` or `focus_changed`). Do not infer skip from `cancelled` alone. |
| `heard-up-to` or `not-played`, `stopped` | `status: "interrupted"` or `"unplayed"`; reason from the local action | Use `voice_off`, `agents_muted`, `call_ended`, `connector_disconnected`, or `focus_changed` as applicable. |
| `heard-up-to` or `not-played`, `failed` | `status: "failed"`, `heard_chars` when supplied | Core stores reason `playback_failed`; the voice failure `code` remains a local diagnostic. |
| `agent-interrupted {heard_until, played: true}` | `status: "interrupted", reason: "user_interrupted", heard_chars: heard_until` | Same semantic report as the handle's `heard-up-to/barge-in`; send once per core `utterance_id`, or let identical retries deduplicate. |
| `agent-interrupted {heard_until: 0, played: false}` | `status: "unplayed", reason: "newer_turn"` | Same semantic report as the handle's `not-played/barge-in`; no confirmed heard extent. |

Send the terminal playback report before the final `voice-user-turn` message in the client's ordered reliable outbox. The server's `client_msg_id` deduplication prevents the same reliable frame being applied twice; core also compares semantic evidence so the handle outcome and `agent-interrupted` event cannot create two notices. Use one stable `client_msg_id` per frame when retrying. IDs are remembered for the bounded recent-device window (256 messages); completed turn identities are retained for 4,096 turns and publication identities for 2,048 admissions. After those windows, do not replay an old action as a new request.

The fixtures in [docs/fixtures/conversation-flow.json](fixtures/conversation-flow.json) spell out the producer/consumer pairs, including played zero extent, merged phases, and the complete offline frame. Core's fixture test checks the accepted mapped receipts and outcomes.

## Core to device

`voice-reply.data` contains `session_id`, `utterance_id`, call `revision`, `reply_revision`, conversation `epoch`, `thread_id`, `history_id`, `text`, and optional `language`/`replay`/`requested`. `voice-reply-withdrawn` remains explicit and names all affected `utterance_ids` and a reason. A client must stop a withdrawn reply and report its actual extent or never-started fate. It must not start a previously received reply after reconnect. Core does not replay missed `voice-reply` frames.

History rows include `epoch`, `audio_reason`, `heard_chars`, `globally_heard`, and `replayable`. Sidevoice-only notices have `role: note`, `text: ""`; they are system facts, not speech bubbles attributed to the person. Participant views include current `epoch`, `unheard: {count, since} | null`, and `working: bool | null`. Working is a fact only: the web applies the three second display delay.

History returns the most recent 1,000 retained rows plus any older retained globally unheard assistant rows. Those older rows stay visible for explicit manual replay until the bounded room journal can safely evict them.

## Core and connector

`binding.register` returns `epoch` and `initial_reply_context: {thread_id, session_id, revision, epoch}` for speak-first. In that initial context, `session_id` is `binding:<binding_id>` and `revision` is `0`; use the returned values as issued. After input arrives, use that input's session, revision and epoch. Invented session IDs are refused. `speech.publish` requires `event_id`, `utterance_id`, `binding_id`, `session_id`, `revision`, `epoch`, and `text`; `history_only: true` records a request held across a connector outage without speaking it. A current-epoch reply while any captured turn is open returns terminal `status: rejected`, `reason: user_speaking`, `error: room.user_speaking`, and the wait instruction. It has no assistant row. The V3 RPC result echoes `event_id` and `utterance_id` even on refusal, so a connector can retire its outbox item. A stale epoch is written as `text_only` with reason `newer_turn` and is never spoken. A future epoch is terminally refused. An identical retry retains the first disposition even if playback later changes. Other `text_only` reasons include `agents_muted`, `voice_off`, `inactive_conversation`, `disconnected`, `call_ended`, `connector_disconnected` and `queue_full`.

For a `speech.publish` request whose response is uncertain, call V3 RPC `speech.recover` with **every original `speech.publish` field unchanged**, plus `recovery_binding_id` naming a currently live binding for the same connector and conversation. In particular, retain the original `binding_id`, `history_only` value, `session_id`, `revision`, `epoch`, `event_id`, `utterance_id`, `text`, and `language`; do not change `history_only` to repair uncertainty. Core checks the original fingerprint and returns the first saved or terminally refused disposition if it exists. If it does not exist, core atomically saves the text as `status: text_only`, `reason: connector_disconnected`, `text_saved: true` and never emits late `voice-reply` audio for that attempt. The response echoes the two IDs exactly as `speech.publish` does. A conflicting ID/fingerprint is terminally refused as `room.speech_conflict`; an absent or stale recovery binding returns `status: unknown_binding` and must be re-registered before retry. The connector may retire an attempted outbox item only after a saved or terminal refusal response with matching IDs; a transport failure or `unknown_binding` leaves it pending for another recovery call. Once retired, fresh-context publications use their own original fields and can play normally. This is process-local recovery within the documented publication identity retention window.

Push `input.deliver` and pull `input.pull` carry the same content fields: `message_id`, `session_id`, `revision`, `epoch`, `channel`, `text`, optional `notice`, and optional `unheard`. Push also carries `event_id`, `binding_id`, and `thread`; pull carries `arrival_time` and `cursor`. `channel` is `voice` for the person's spoken or typed words and `note` for Sidevoice-only messages (`text: ""`). A notice does not create an epoch. Pull claims a note until its `message_id` is acknowledged or the claim is released, exactly as for user words. Push retries use the same row and message ID. A user input carrying `notice` does not expire before acknowledgement. If channel closure or a terminal push result prevents its delivery, core queues the full evidence as a new `channel: note` message with `notice.related_message_id` naming the original user message. For one conversation, push holds later messages behind an earlier pending retry; pull returns journal order.

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

Notice kinds are `reply_interrupted`, `reply_not_played`, `turn_empty`, `user_returned`, `reply_skipped`, and `reply_heard`. Skip evidence appears in `notice.skipped`, outside `unheard`. An envelope becomes permanently frozen when core first offers it to push `input.deliver` or includes it in a pull `input.pull` response. A failed push acknowledgement or released pull claim returns it to the queue with the **same `message_id` and payload**; it does not unfreeze it. Before first exposure, core may reconcile its unheard evidence in place. After exposure, a later confirmed hearing extent generates a new `channel: note`, `notice.kind: reply_heard` with `notice.corrects_message_id` and `notice.heard: [{utterance_id, history_id, fate: "partial" | "heard" | "skipped", heard_chars}]`. `partial` carries the new maximum confirmed Unicode scalar prefix and leaves the remainder eligible for a fresh response; `heard` and `skipped` cover the full reply and supersede false resend advice. Further increases and a later full/skip result each generate another correction. For a no-words turn with a refused speech call, `channel: note`, `text: ""`, `notice.kind: turn_empty` and the resend-if-relevant instruction are queued immediately. Return notices are queued after five seconds of eligible silence if globally unheard replies remain; no notice is queued when another device heard or skipped them. Each evidence envelope contains at most eight full replies in publication order; extra batches are correlated ordinary notices. A null `heard_chars` means no extent was confirmed. Consumers split the full text by Unicode scalar count, never by UTF-8 bytes or UTF-16 code units.

A late receipt after an empty turn becomes an immediate ordinary note. When a newer spoken message remains in the journal, `notice.related_message_id` links its follow-up; if bounded terminal history has already aged out, the late note still carries full reply evidence with `related_message_id: null`.

The in-memory room retains at most 2,048 conversation epoch identities. After that limit, admission of a previously unseen conversation returns `room.input_backlog_full`; an existing identity never silently restarts at epoch zero. Starting a turn reserves queue headroom for its possible notice and can also return `room.input_backlog_full`. These limits protect deferred notices as well as ordinary input.

Manual replay uses `/api/presentation/replay` and a new playback ID linked to the original history row. Returning alone never requests it. Replay is refused while output is off or a user turn is open.
