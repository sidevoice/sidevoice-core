# Sidevoice voice-message contract — reviewed baseline, result timing reopened

**Status:** Sections 1–5 derive from the independently reviewed baseline, but this amended revision has not been independently re-reviewed. The newly requested Option B in §6 is documented for discussion, not chosen; tool-result timing needs the operator's clarification before the full contract is frozen. No runtime tests or code authorization. Tracking: [Core #126](https://github.com/sidevoice/sidevoice-core/issues/126) and [Connector #155](https://github.com/sidevoice/sidevoice-connector/issues/155). Baselines: [Connector 1725ad7](https://github.com/sidevoice/sidevoice-connector/tree/1725ad7) and [Core 9e513f2](https://github.com/sidevoice/sidevoice-core/tree/9e513f2). This describes intended, unshipped behavior.

## 1. Scope and invariant

The first release targets **push** in Codex, Claude Code, and the operator's Cursor bridge. Pull carries the same logical
events later; host liveness is deferred. Native chat text is outside Core. Connected replies retain full written answers and
appropriate voice, possibly several times per intervention.

The highest-priority case is causal assent. A fully played utterance `17` clearly asks “Approve X?”; the person starts saying “yes,
go ahead”; the agent submits `18`, asking “Approve Y?”, only after that acoustic start. The user's delivered words must carry the
playback state **at speech start**, including `17` as last played and `18` as later/unplayed. A bare assent naturally refers to the
clear, fully presented X question: X may proceed; Y must not. The later written presence of `18` alone is not a reason to ask for
clarification. Ask narrowly before consequential action only if the relevant heard/explicit context genuinely leaves more than one
target, such as multiple heard alternatives or an unplayed condition. “Yes, do Y” can refer explicitly to a visually read Y despite
its unplayed status. Last-played ID is evidence, not a semantic `reply_to` or approval by itself; if `17` was not a question,
interpret the actual context.

Core can enforce factual cutoff/order and no stale automatic playback. The external model chooses native tools; an MCP header
cannot guarantee its choices. Real harness action traces, including weak models, are an acceptance gate.

## 2. One logical inbound envelope

The connector renders one canonical JSON header, a blank line, then **only the person's exact words** for `channel:"voice"`. A
`channel:"note"` event is one JSON object with no user-word body. Push injects that rendering as host input; a future pull result
must expose the same logical fields and stable IDs, though model role, salience, and timing differ. No generic `[Sidevoice] If a
reply is appropriate…` or no-words sentence is appended. General reply, retry, note, and causal-assent rules live in MCP server
instructions and tool descriptions; test host exposure rather than assuming protocol priority.

```json
{
  "channel": "voice",
  "origin": "spoken_room",
  "message_id": "m-42",
  "session_id": "s-7",
  "revision": 3,
  "epoch": 8,
  "playback_at_turn_start": {
    "knowledge": "known",
    "last_played": {"utterance_id": "17", "hearing": "complete", "heard_chars": 32},
    "recent": [{"utterance_id": "17", "hearing": "complete", "heard_chars": 32}],
    "truncated": false
  },
  "not_played_at_turn_start": [{"utterance_id": "18", "hearing": "not_played", "heard_chars": 0}],
  "hearing_changes": []
}

Yes, go ahead.
```

`channel` is `voice` for room words and `note` for machine events; there is no redundant top-level `type`. `origin` is required for
`voice`: `spoken_room` or `typed_room`. Native-harness text has no Sidevoice envelope or synthetic `message_id`. Every delivered
room event has stable `message_id`, `session_id`, `revision`, and `epoch`. The same `message_id` and identical payload on redelivery
are **one** user input, not another turn or authorization. A changed payload under that ID is a conflict, not a correction; a
genuine correction is a new correlated event. Push and future pull must use the same logical identity.

For `spoken_room`, `playback_at_turn_start` is required. The app captures a local playback snapshot at acoustic onset; Core
associates it with the resulting words, not with the later transcript submission time. `knowledge:"known"` plus `last_played:null`
means no publication had actually played; `knowledge:"unknown"` means evidence was unavailable, not confirmed silence. `recent`
carries the latest **up to eight** per-utterance states in original publication order, with `truncated:true` if older states exist;
it preserves holes such as A heard, B missed, C heard. `last_played` identifies the most recently actually played original, not the
newest submitted or merely queued publication. `hearing` is `complete`, `partial`, `not_played`, or `unknown`. `heard_chars` is the
confirmed prefix length in Unicode scalar values for complete/partial/not-played and is `null` for unknown. A partial prefix is
conservative device evidence, not proof of human comprehension. Never convert unknown to zero. Later receipts may refine only
playback **before** the immutable acoustic cutoff; playback after onset cannot be backdated.

`not_played_at_turn_start` lists publications known by delivery whose **absence from playback at the local acoustic cutoff is
certified by the app**, whether Core received their admission before or after its delayed turn-start event. It is separate from the
immutable snapshot so `18` cannot change what the person had available when saying “yes.” Core arrival order alone cannot prove this
fact; if the app cannot certify it, report `unknown` rather than `not_played`. An open spoken turn bars later playback. If there are
more than eight, include the latest eight and `not_played_at_turn_start_truncated:true`; this does not erase older publication state
in Core. `hearing_changes` is a compact, ID-correlated delta of physical facts newly relevant at this natural input. Each entry has
`utterance_id`, `attempt_id` (or `null`), `hearing`, and `heard_chars` (or `null` for unknown); the enclosing `message_id`
identifies the logical delivery, while later corrections use new correlated events. Do not reprint full original replies or a
perpetual unresolved ledger. If device evidence at onset cannot be established, mark it unknown and permit natural
clarification or conservative recovery later.

For `typed_room`, omit `playback_at_turn_start` and `not_played_at_turn_start`: there is no acoustic onset. Keep `origin`,
identity/context fields, exact typed words, and any compact `hearing_changes` relevant to that input. Typed room text neither
interrupts active audio nor advances the spoken epoch. It does invalidate automatic playback of an older not-yet-started pending
publication via the fresh-input fence; the agent may later authorize replay if relevant. A future observable typing-start cutoff
could be added explicitly; submission time must not masquerade as acoustic onset.

The note form is `{"channel":"note","event":"user_returned"|"recovery_needed","message_id":…, "session_id":…, "revision":…,
"epoch":…, "hearing_changes":[…]}`. It has no `origin`, no `text`, and no fabricated acoustic-start field. `user_returned`
includes a listener effectively refocusing **this same bound conversation** after `focus_changed`, as well as a relevant
network/device return; it is never broadcast to other conversations or agents. Emit it only when that conversation has
model-relevant, still-available unseen reply IDs and hearing facts; a no-change return updates presence silently.
For example, if `voice_say("The contract is ready")` became `text_only` while the listener focused another conversation,
refocusing the original conversation **must** deliver one `user_returned` note identifying that available, unplayed original.
For this event, `hearing_changes` includes current compact states for relevant available unseen originals even if the physical
receipt predates the return; it is a recovery snapshot, not a claim that hearing just changed. The note does **not**
authorize automatic audio; the agent uses its new note's `message_id` as `context_message_id` to decide whether to call
`voice_replay`. Current [Core focus selection](https://github.com/sidevoice/sidevoice-core/blob/9e513f2/rust/control/room/focus.rs#L29-L46)
offers a note, but its existing unheard predicate does not cover every `text_only` focus-loss publication; this
notification is new contract behavior, not a claim that the baseline already does it.
`recovery_needed` is emitted only if Sidevoice has handed recovery ownership to the agent after its own playback/resume cannot
complete; app-owned successful resume must not also ask the agent to repeat. Both carry physical facts rather than imperative prose.
Preserve the current #152/#153 hearing/recovery behavior until the new path is validated.

## 3. Reply context, `voice_say`, and ordering

`voice_connect` returns `initial_reply_context:{session_id,revision,epoch}` for the active binding. After receiving a room event,
the agent uses its complete context triple and `message_id` as `context_message_id` in ordinary `voice_say`, including for answers
initiated by native-harness typed chat. Before any received room event it may omit the triple and `context_message_id` to use
`initial_reply_context`; supplying a partial triple is invalid. Core resolves `context_message_id` in the owned binding journal and
fences against any later model-relevant room words, including `typed_room`. It never silently refreshes an older context to the
latest room input. There is no separate native-text speech tool, synthetic room user turn, or portable ordering between native chat
IDs and Core IDs.

```json
{"text":"I created the issue.","event_id":"e-19","utterance_id":"u-19",
 "session_id":"s-7","revision":3,"epoch":8,"context_message_id":"m-42"}
```

`event_id` and `utterance_id` are required nonempty stable caller IDs. Retrying the **same call** uses both same IDs and identical
text, language, binding, and original context; Connector and Core return the same disposition within the same Core incarnation.
Reusing either ID for different content/context returns `identity_conflict`. The connector's existing same-key retry and Core
fingerprint protections must remain. Connector scopes the request to its owned binding; IDs are not access credentials.

In the reviewed **Option A admission baseline**, Core atomically validates arguments, ownership, context, capacity, and turn
state; then stores the immutable original and returns promptly, including while the person speaks:

```json
{"status":"accepted","utterance_id":"u-19","event_id":"e-19",
 "playback":"pending","attempt_id":"a-1"}
```

`playback` is `pending` (accepted but blocked by an open spoken turn), `queued` (eligible and scheduled, not yet heard), or
`text_only` (retained as text under the existing no-listener or `focus_changed` baseline, not scheduled). All are distinct from `playing`,
`completed`, and confirmed hearing receipts. No accepted/queued result is `OK`, “played,” or “heard.” A rejected call returns
`{"status":"rejected","reason":…}` and was not stored/scheduled. Reasons are `invalid_request`, `binding_inactive`, `context_stale`,
`identity_conflict`, or `capacity_full`; `context_stale` means Core cannot prove a current or legitimate pre-turn/empty-turn
lineage, not merely that a spoken turn incremented the browser revision. A transport timeout is **indeterminate**, so retry with the
same IDs rather than inventing a second publication. `capacity_full` rejects the new call after normal cleanup, without evicting an
accepted pending publication or causing blind agent retry. Existing `text_only` no-listener behavior stays unchanged; the different
absent-listener optimization belongs to [connector
#156](https://github.com/sidevoice/sidevoice-connector/issues/156).

The implementation defaults for initial review are `MAX_PENDING=8` per conversation, `AUTO_TTL=5 minutes`, `ORIGINAL_TTL=30
minutes`, and a replayable-original cache capped at 128 records or 2 MiB (evict oldest non-pending first). These are bounded
engineering defaults to support several ordered acknowledgements/checkpoints/finals without an indefinite archive, **not
operator-ratified service guarantees**. Configurability and exact cleanup mechanics may be adjusted by review or measured tests.
While the process remains alive, an accepted pending record cannot be silently evicted before its automatic-play deadline merely to
admit a new one; after expiry it is not auto-eligible. Read/replay is best effort until original text expires or is evicted. No Core
disk archive or restart survival is required; restart/leave must invalidate automatic eligibility, never revive stale speech. Loss
is reported as unavailable, never inferred as heard. Core reloads with fresh bindings and an empty reply journal; an old
binding/session or `context_message_id` is stale after restart, so an old retry is rejected rather than re-admitted as audio. The
old exact disposition may be unrecoverable without an archive, so no cross-restart same-result promise is made. Existing connector
outbox and web transcript storage still require privacy review; this contract does not claim a RAM-only system.

Admission with an open spoken turn may hold a reply from its **pre-turn context** as pending. At terminal empty/cancelled
settlement, Core may rebase that specific pending record only if the **same binding and thread** remain owned, Core proves this
publication's pre-turn epoch and turn lineage through terminally empty/cancelled turns, no user words (spoken or typed) or
conflicting notice intervened, no other spoken turn remains open, and the auto deadline is live. Opening a turn may advance the
browser revision; equality to the original revision is **not** required for this internal eligibility rebase. The original
session/revision/epoch remains in the immutable idempotency fingerprint; Core updates only effective playback eligibility, never
rewrites what context the agent supplied. New words instead revoke automatic eligibility for older pending audio and enter the
normal inbound event once with correlated physical hearing/not-played facts. A later unrelated request does not by itself make the
original text semantically obsolete, but only explicit agent replay may offer it again. Core must reject a context with unread room
words (including typed words), changed binding, or a stale pre-restart binding/session; it must not stamp the latest epoch onto the
agent's old context. A chain of empty turns may be crossed only when Core proves every intervening turn terminally empty/cancelled
and no model-relevant input was missed. Native-harness text invisible to Core limits this validation; the agent's use of the latest
**received** context and real-host tests are required.

Core serializes publication, turn opening/settlement, and playback eligibility. Do not start sound while **any** relevant spoken
turn is open. Wait for terminal outcome, not silence alone; merged/overlapping turns cannot cause a transient empty event to start
stale audio. Preserve several accepted publications in admission order, including replay followed by a new acknowledgement and
final. A new spoken turn interrupts active audio; if it ends with words, stop automatic continuation; if it ends empty/cancelled, an
otherwise eligible original may resume under §4. A late arrival from an old playback attempt never overwrites a newer confirmed
hearing prefix.

## 4. Hearing, automatic resume, lookup, and explicit replay

Core records hearing per original `utterance_id` **and listener session**, with each physical playback `attempt_id` linked to the
original and its listener. An aggregate room row may remain useful history but its maximum heard prefix must never drive resume for
a listener that heard less. Unknown receipt is `unknown`, never zero. The app reports conservative completed playback chunks and
captures the local onset snapshot before stopping playback; Core validates monotonic per-listener confirmed-prefix updates and
retains correction provenance. A single global last-heard cursor is insufficient when A heard, B missed, and C heard, or when D1
heard 80 characters and D2 heard none. Normal model-facing updates occur with the next user words or relevant reconnect/recovery
note; no routine receipt-only turn.

When an in-progress publication is interrupted by a spoken turn that terminally ends empty/cancelled, Sidevoice is the recovery
owner if it can resume automatically. It chooses `replay_start_chars` at the start of the sentence containing or preceding that
**listener's** `confirmed_heard_chars`, using a valid preceding `.`, `?`, `!`, `…`, or `...` separator; absent one, start at
original character zero. Avoid false separators in decimal numbers and abbreviations, and handle newline/quotes in deterministic
fixtures. Repeating a few confirmed words for coherence does **not** decrease `confirmed_heard_chars`. If that listener's hearing is
unknown, use its last confirmed prefix; with none, start from zero and accept possible repetition. The new attempt is linked to the
same immutable original. Successful app resume emits no agent re-say note. If Sidevoice abandons resume and hands off recovery, it
must atomically end automatic ownership before emitting one correlated `recovery_needed` event. Subsequent late receipts update
physical truth and replay checks, not a routine new model turn.

`voice_get_reply({"utterance_id":"u-19"})` is a read-only MCP operation. The connector identifies the owning live
conversation/binding; Core returns exactly that authorized immutable original and a **conservative current-audience** hearing
summary, or `{"status":"unavailable"}` for expired, evicted, restarted, missing, or unauthorized data without revealing another
conversation's existence. Available result:
`{"status":"available","utterance_id":"u-19","text":"…","audience_hearing":{"confirmed_for_all_chars":0,"complete_for_all":false,"uncertain":false,"listener_count":2}}`.
`confirmed_for_all_chars` is the minimum confirmed prefix across eligible target listeners, **not** a claim that none heard more;
`uncertain` is true if any target's receipt is unknown. With no eligible listener, return `listener_count:0`, `uncertain:true`, and
zero as a conservative lower bound, not a device hearing claim. It does not acknowledge inbound messages, mark a reply heard, extend
TTL, authorize playback, or change notice state. A caller-supplied ID is a reference, never authorization. If unavailable, the agent
may ask or make a **new** context-based summary, not claim verbatim replay or invent the original/heard portion. No
list-by-conversation lookup is required initially.

`voice_replay({"utterance_id":"u-19","replay_id":"r-6","mode":"resume","session_id":"s-7","revision":3,"epoch":9,"context_message_id":"m-43"})`
is the sole agent-initiated reauthorization. `mode` defaults to `resume`; `restart` explicitly repeats the full original. Core
resolves the original ID, checks the owning live binding, **current focus and eligible listener for that same conversation**,
the context from the latest note/input rather than a stale pre-leave reference, available text, **per-listener** hearing,
turn state, expiry, and ordering **at admission and immediately before sound**. A rapid focus switch before sound rejects or
cancels that attempt; a switch during sound stops the old-conversation audio and records only the confirmed heard prefix.
For a shared replay to several listeners, `resume` uses
the sentence boundary at or before the **minimum confirmed prefix of every target**; this may repeat text for D1 so that D2 misses
none. Unknown target hearing falls back to that target's last confirmed prefix (zero if none). Return `{"status":"already_heard"}`
only if **all** targets are confirmed complete, never because one device or a room aggregate reached the end. `restart` starts at
zero even when fully heard. A missing/expired original returns explicit failure `{"status":"failed","reason":"unavailable"}`;
not found or unable to reproduce is never silent success. A stale/currently unsafe authorization
returns `{"status":"rejected","reason":"context_stale"|"binding_inactive"|"no_listener"|"capacity_full"}`. `no_listener` means text
may still be available but there is no eligible playback target; it must not be reported as missing content. Successful
authorization returns
`{"status":"accepted","utterance_id":"u-19","replay_id":"r-6","attempt_id":"a-2","playback":"queued"|"pending"}` in Option A;
accepted still does not mean started, completed, or heard. If an accepted attempt later cannot play, Core records a
correlated terminal failure rather than reporting success; how the agent receives that later result is part of §6's
unsettled timing contract.

Retrying the same `replay_id` with identical original/mode/context returns the same outcome and cannot create another playback;
changing its fingerprint returns `identity_conflict`. A deliberate new replay uses a new `replay_id` and attempt linked to the
original. New spoken or typed room words before sound revoke that attempt's automatic eligibility and require a fresh relevance
choice. During sound, spoken input interrupts under the turn rules; typed room input does not stop already playing audio. The agent
chooses relevance after reading the newest user words: additive input may justify replaying an older useful answer, while
cancellation or corrected facts require omission or revision. The agent should use hearing metadata internally and speak naturally,
without unsolicited ID/count/interruption narration. Replaying stored text may require new TTS synthesis and does not promise audio
sample reuse.

## 5. Ownership and verification

**Web/app:** capture acoustic onset and device playback state before later publications can affect it; stop audio on barge-in; send
original IDs, attempt IDs, conservative scalar hearing receipts, and the immutable onset snapshot with the eventual words. Ordinary
room typing supplies provenance but no fake acoustic snapshot. Do only the UI changes necessary for this provenance/snapshot; no
unrelated landing or profile work.

**Core:** own turn/epoch ordering, canonical immutable originals while available, bounded admission, idempotency, per-attempt
hearing, one recovery owner, sentence offset, atomic stale checks, and accurate envelope facts. Never infer hearing from acceptance,
model delivery, lookup, or absence of a receipt. Core's lock orders arrival, not acoustic onset: the app's captured state is the
causal input, and insufficient evidence is unknown.

**Connector:** bind MCP calls and lookup/replay to the owned conversation, preserve stable IDs through push redelivery and future
pull, render the common envelope without repeated generic prose, expose central MCP instructions/tool schemas, and return honest
accepted/rejected/unavailable results. Keep the current no-listener behavior pending
[#156](https://github.com/sidevoice/sidevoice-connector/issues/156). No duplicate user words in a `voice_say` result and a pushed
message.

**Deterministic gates:** Core+connector fixtures cover open/empty/merged turns, words winning over pending autoplay, context
rebasing only across proved empty turns, leave/restart with no stale revival, capacity/retry/timeouts, multiple ordered
publications, `unknown` versus zero, late receipts, scalar offsets, sentence separators, lookup isolation/expiry, replay idempotency
and races. Include A→B→A focus return with an available `text_only` A original, A-only note and fresh context, no automatic audio,
no note without relevant content, and another switch before replay sound. Exercise the real spoken-turn/playback/hearing path, not typed presentation alone. An MCP conformance check is separate
from behavior and does not substitute for these gates.

**Real-harness release gate:** Script push conversations with actual Codex, Claude Code, and the operator's Cursor bridge; include
small supported models and long/compacted contexts. Use harmless instrumented file-delete/database-update stand-ins and inspect
**tool calls**, not prose alone. Clear fully heard A17 + B18 submitted after acoustic onset + bare “yes” permits X, forbids Y, and
should not cause reflexive clarification. Separate cases with two genuinely plausible heard targets or a partly unheard condition
must make **no** consequential tool call before targeted clarification. Explicit “yes, do Y” may authorize visually read Y despite
B's unplayed status. Also test A-specific focus-return note and explicit replay, note versus words, duplicate delivery, native-chat dual output, recovery with unavailable original,
and several ordered speech publications. Record failures by model/harness; no fixed pass rate or claim of deterministic model
obedience is assumed before tests. A scripted fake agent validates plumbing but not a real host's instruction exposure or model
decisions.

## 6. Playback-anchored tool result (Option B — alternative, not selected)

Sections 3–4 describe **Option A**: `voice_say` or `voice_replay` returns promptly after validated admission, with a truthful
`accepted`/`pending`/`queued`/`text_only` result; a later playback outcome is separate. The operator has asked us to document
**Option B**, in which the tool call waits for a factual playback milestone. This is distinct from the older proposal to wait for
the *person's spoken turn* to settle before accepting speech. Option B is not adopted by this draft; in particular, a pending
publication while the person speaks cannot be reported as `OK` merely because Core accepted it.

There are two possible wait boundaries, still requiring the operator's choice:

- **Start boundary:** wait until the target app confirms that sound actually began, then return `started` (or `OK` defined
  strictly as started). If the attempt cannot start, return an explicit `not_started`/`failed` result and reason. This says
  nothing about later completion or hearing.
- **Terminal boundary:** wait for the attempt's terminal playback outcome and report `completed`, `partial`, `cancelled`,
  `not_started`, or `failed`, linked to the original and attempt IDs. Any confirmed hearing extent is per target listener;
  missing receipts remain `unknown`. Device completion does not prove human comprehension. A completion result cannot promise
  that no new user turn will start immediately afterward.

Under either boundary, missing/expired originals, stale authorization, loss of focus/listener, or inability to reproduce must
produce explicit failure rather than a success-shaped response. A pre-admission rejection remains a rejection. An admitted
attempt whose outcome is still pending at a bounded deadline must return an **indeterminate timeout**, not `OK` or fabricated
`completed`; same-key retries must not start a duplicate attempt. MCP cancellation, leave, focus switch, and host tool deadlines
need explicit bounded handling. Neither Core's room lock nor the connector's serial admission path may be held while awaiting
playback. Such a call may block this model's next serial step; it need not block independent app/Core work, other workers, or a
host's own user input, but those concurrency properties must be verified per harness.

After any factual `started` or `completed` result, the person may begin speaking. The app still captures the acoustic-start
snapshot and the next **ordinary words event** reports the correct played/partial/not-played evidence under §2. The tool result
never embeds those new user words or substitutes for that event. A later interruption or receipt may refine physical hearing;
it must not retroactively turn admission into hearing or misbind assent to a later unplayed question. Validation must compare
Option A with both B boundaries on overlap, cancellation, focus/leave, timeout, idempotent retry, and real-harness model latency
and action traces before replacing the reviewed result timing. No implementation or runtime validation of Option B is claimed.

## Deferred follow-ups

Optimizing optional speech while the listener is absent is tracked separately in
[connector#156](https://github.com/sidevoice/sidevoice-connector/issues/156). Spoken verbosity and progress/heartbeat cadence
remain separate profile work in [connector#2](https://github.com/sidevoice/sidevoice-connector/issues/2). Neither changes the
scope of this contract or implies a promise to suppress spoken output or add progress signals.
