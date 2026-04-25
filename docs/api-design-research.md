# API design research — vendor survey for heron v2

Reference for [`api-design-spec.md`](./api-design-spec.md). The spec
encodes heron's invariants and state machines; this doc captures the
vendor landscape they were derived from.

Companion to [`agent-participation-research.md`](./agent-participation-research.md)
(which surveys the *product* category) and the Vexa-specific deep-read
in conversation history. This doc focuses on **API contract patterns
across vendors**, organized by the layer in heron's four-layer model
(`api-design-spec.md` §1) the vendor competes in.

The synthesis was produced via three independent agents (Claude, Codex,
Gemini) and pressure-tested by three further oracles. Findings below
are the cross-validated consensus. Where the three disagreed, both
positions are recorded.

---

## The three vendor layers

Vendors don't all operate in the same architectural slot. Conflating
them is what made the original synthesis recommend a single REST
skeleton that didn't fit anything cleanly. The clean separation:

| Layer | What it does | Example vendors |
|---|---|---|
| **Driver** | Joins meetings as a participant; lifecycle, recording, transcript artifacts | Recall.ai, Attendee, MeetingBaaS, Vexa |
| **Policy / Turn** | Decides when to speak, what's allowed, barge-in, escalation | (no shipped vendor) — built in-house in Pipecat / LiveKit / etc. apps |
| **Realtime** | Audio in/out streaming, LLM brain, TTS, VAD, turn detection | OpenAI Realtime, Gemini Live, LiveKit Agents, Pipecat |

**Key observation:** the policy/turn layer is unstaffed by vendors.
Every existing speaking-bot product builds it ad hoc. Heron's
`heron-policy` + `heron-turn` is filling a gap, not duplicating
prior work.

---

## Layer 1: Driver vendors

Capability matrix for the meeting-bot platforms heron-v2 might wrap:

| Capability | Recall.ai | Attendee | MeetingBaaS | Vexa | Native Zoom SDK |
|---|---|---|---|---|---|
| OSS / self-hostable | ❌ Commercial | ✅ AGPL | ❌ Commercial | ✅ Apache | ❌ Proprietary |
| Platforms | Zoom, Meet, Teams, Webex | Zoom, Meet, Teams | Zoom, Meet, Teams | Zoom-Web, Meet, Teams | Zoom only |
| Pricing | ~$0.69/hr/bot | self-host cost | tiered | self-host cost | per-license |
| Bot creation idempotency | partial (`Idempotency` doc page exists) | ❌ | `deduplicationKey` field | composite-key URL | N/A |
| Per-utterance ID on speak | ❌ | ❌ | ❌ | ❌ | N/A (raw audio) |
| Per-utterance cancel | ❌ (DELETE channel only) | ❌ | DELETE /speak only | DELETE /speak only | N/A |
| Live partial transcripts | WS at `wss://meeting-data.bot.recall.ai` | webhook (no live WS) | bundled in `complete` event | WS multiplex (`/ws`) | N/A |
| Webhook signing | Svix-managed HMAC | HMAC-SHA256 over canonicalized JSON | echoed API key | HMAC over `{ts}.{payload}` | N/A |
| Capacity error code | **507** Insufficient Storage | 429 | 429 | 403 | N/A |
| Graceful leave vs hard kill | **split** (`POST /leave_call` vs `DELETE`) | unified DELETE | unified DELETE | unified DELETE | N/A |
| Audio frame access | post-recording only | live PCM via WebSocket | live PCM via WebSocket | mixed PulseAudio | direct (C++) |
| Bot kick-out signal | webhook event | webhook event | terminal `failed` event | WS status channel | C++ callback |

### Recall.ai specifics

- **Resource shape**: bot-centric flat. `POST /api/v1/bot/`, then
  `POST /bot/{id}/output_audio/`, `POST /bot/{id}/send_chat_message/`,
  `POST /bot/{id}/leave_call/`, `DELETE /bot/{id}/`.
- **Lifecycle split is the gold standard**: "DELETE can only be done
  on scheduled bots that have not yet joined" — once joined, you must
  use `/leave_call`. Heron's `bot_leave()` vs `bot_terminate()`
  distinction (spec §3) borrows this directly.
- **Output Media API** accepts `{kind: "webpage", config: {url}}` —
  the agent IS a webpage. This is the cleanest unification of
  `/speak` + `/screen` + `/avatar` into one resource.
- **Capacity = 507**, not 429. Distinct retry strategy.
- **Status delivery**: webhook-only, ~12 distinct bot states with
  `code` + `sub_code` (open enums per Recall's docs).

### Attendee specifics

- **OSS reference for self-hosting.** AGPL. Django + Docker.
- **Webhook signature is HMAC-SHA256 over canonicalized JSON** (more
  rigorous than HMAC over raw body — defends against whitespace
  re-encoding attacks). Heron's outbound webhook (spec §10) follows
  this pattern.
- **`idempotency_key` in webhook envelope** so consumers dedup across
  retries without computing their own hash. Adopted in spec §10.
- **No live WS for transcripts** — gap relative to Recall.

### MeetingBaaS specifics

- **Stripe-style structured errors**:
  `{success, data, error, message, code, statusCode, details}` with
  machine-readable codes (`FST_ERR_BOT_NOT_FOUND_BY_ID`). Heron's
  error taxonomy (spec §11) uses this shape internally.
- **`deduplicationKey`** is the body-level idempotency mechanism —
  intentional dedup, not retry-safety. Heron's spec separates these:
  `external_id` for intentional, `Idempotency-Key` header for retry.
- **Speaking bot endpoints**: `/speak`, `/chat`, `/screen`, `/avatar` —
  the most complete REST surface for active participation, but
  returns no utterance ID. This is the design failure heron's spec §9
  (`Priority::Replace` as a single primitive) explicitly avoids.

### Vexa specifics

(Detailed in conversation history; not re-covered. Key reference points:)

- **Composite-key URLs** (`/bots/{platform}/{native_id}`) — judged
  *valid as ergonomic resolvers, invalid as primary identity* (spec
  §2). Two of three oracles rejected outright; reconciliation: keep
  composite as a `resolve_*` method, not as URL path.
- **Single-DELETE-as-leave** conflates graceful and hard. Reject; use
  Recall's split.
- **Bare FastAPI `{"detail": ...}` errors.** Reject; use MeetingBaaS-shape.
- **`/speak` returns `{message: "Speak command sent"}` with no ID.**
  Reject; spec §9.

### Native Zoom SDK

- C++ Meeting SDK with Qt event loop. Highest fidelity, highest
  engineering cost, Zoom-only.
- Direct PCM frame access (vs other vendors' headless-Chromium-mediated
  streams).
- Licensing constraints: SDK is proprietary, distribution requires
  per-app approval from Zoom.
- **Trait shape consequence**: if `heron-bot` ever wraps native, the
  trait must expose raw PCM streams as a first-class capability, not
  merely "transcripts." A trait designed only against Recall would
  underspecify what native can do.

---

## Layer 2: Policy / Turn vendors

**There aren't any.** Every speaking-bot product (Vexa, MeetingBaaS,
Recall-based agents, Attendee voice-agent example) implements its own
ad-hoc policy logic in application code:

- "When should the bot speak?" — typically a name-mention regex +
  silence detector
- "What is the bot allowed to say?" — typically a system prompt
- "How does the bot escalate?" — typically not implemented at all
- "Barge-in detection" — typically VAD on incoming audio + cancel

This is a vendor gap, not a heron oversight. `heron-policy` +
`heron-turn` (spec §1, layer 3) is the load-bearing middle that
nobody ships. The spec encodes the contract; an implementation will
be heron's own contribution.

The closest reference patterns:

- **OpenAI Realtime VAD config** (`server_vad` with `interrupt_response: true`)
  — barge-in as a server-side concern.
- **LiveKit Agents `SpeechHandle`** with `wait_for_playout()` + `interrupted`
  property.
- **Pipecat `EndFrame` vs `CancelFrame`** — graceful vs hard pipeline
  termination.

These are *primitives* heron can compose. None of them are a policy
layer; they're the controls a policy layer would use.

---

## Layer 3: Realtime vendors

Capability matrix for the realtime backends heron-v2 might use:

| Capability | OpenAI Realtime | Gemini Live | LiveKit Agents | Pipecat | WhisperKit (local) |
|---|---|---|---|---|---|
| Bidirectional audio streaming | ✅ WS | ✅ WS | ✅ in-process | ✅ in-process | ❌ batch only today |
| Per-utterance ID | ✅ `response_id`, `item_id` | ✅ similar | ✅ `SpeechHandle` | ✅ frame-IDs | N/A |
| Per-utterance cancel | ✅ `response.cancel` | ✅ | ✅ `SpeechHandle.interrupt()` | ✅ `InterruptionFrame` | N/A |
| Speech queue (Append) | ❌ | ❌ | ✅ `session.say()` | ✅ frames | N/A |
| Atomic Replace | partial | partial | ❌ (cancel + speak) | partial | N/A |
| Server-side VAD / barge-in | ✅ `server_vad` config | ✅ | ✅ adaptive | ✅ | ❌ |
| Latency (audio in → audio out) | <500ms | <500ms | depends | depends | N/A (batch) |
| Tool / function calling | ✅ | ✅ | ✅ | ✅ | N/A |
| Privacy (data leaves device?) | yes | yes | depends on hosting | depends | **no** |

### Why `replace_current` is novel

Reading the table: **no vendor cleanly supports atomic Replace.**
Cancel + speak is a two-call sequence with a race. Spec §9 makes
`Priority::Replace` a single primitive; the implementation will have
to *emulate* it on top of every existing backend by careful event
ordering. This is heron's contribution to the realtime API category.

### OpenAI Realtime as the cleanest contract reference

Per Codex's analysis, OpenAI Realtime is the cleanest vocabulary in
the category for utterance lifecycle:

```json
// Cancel current response (utterance) by ID
{ "type": "response.cancel", "event_id": "<response_id>" }

// Truncate current item mid-speech
{ "type": "conversation.item.truncate", "item_id": "<item_id>", ... }

// Configure barge-in via server-side VAD
{
  "type": "session.update",
  "session": {
    "turn_detection": {
      "type": "server_vad",
      "threshold": 0.5,
      "interrupt_response": true,
      "create_response": true
    }
  }
}
```

`heron-realtime`'s trait should mirror this vocabulary one-to-one
where possible. When wrapping LiveKit or Pipecat, the trait
implementation translates; when wrapping OpenAI directly, it's a
near-pass-through.

### Local WhisperKit gap

WhisperKit is heron's existing STT (`heron-speech` Swift bridge). It is
**batch-mode** today; the v2 pivot needs **streaming** STT. Either:

- (a) Wait for WhisperKit streaming (uncertain timeline)
- (b) Build a sliding-window prefix-confirmation submitter on top of
  WhisperKit batch — exactly the pattern Vexa uses
  ([`speaker-streams.ts`](https://github.com/Vexa-ai/vexa/blob/5b48da2/services/vexa-bot/core/src/services/speaker-streams.ts#L1-L60)).
  This is the most transferable single idea from the Vexa read.
- (c) Accept that v2 hands STT to OpenAI Realtime / Gemini Live in
  exchange for sub-second latency, and WhisperKit remains for offline
  post-processing.

The spec doesn't pick. The trait surface accommodates either.

---

## Cross-cutting findings

### Identity model — three positions

| Position | Argument | Vendors |
|---|---|---|
| Surrogate UUIDs only (Stripe orthodoxy) | Stable, opaque, vendor-neutral | Recall, MeetingBaaS, Attendee |
| Composite keys (`/bots/{platform}/{native_id}`) | LLM-friendly: model has the URL not a UUID | Vexa |
| Both / hybrid | UUIDs canonical, composite as resolver | (heron's choice) |

The pushback on the synthesis defended composite keys as primary
identity. Two of three oracles rejected: composite keys leak provider
semantics into the system of record. The reconciliation: composite
keys are *resolver inputs* at the MCP/tool boundary
(`resolve_bot_by_meeting_url`), never URL path identifiers. Spec §2.

### Webhook envelope — converged design

All three sources agree on the canonical shape:

```json
{
  "event_id": "evt_<uuid>",
  "idempotency_key": "<for consumer dedup>",
  "event_type": "bot.completed",
  "api_version": "2026-04-25",
  "created_at": "...",
  "metadata": { /* echoed from create */ },
  "data": { /* event-specific */ }
}
```

Header: `X-Heron-Signature: sha256=<HMAC over canonicalized JSON>` +
`X-Heron-Timestamp: <unix_secs>` + `Content-Type: application/webhook+json`.

This is also the *only* SaaS-style surface heron exposes outward — for
users opting into "POST events to my Zapier/n8n endpoint." The
synthesis was right that the canonical pattern is well-known and
should be adopted; the pushback was wrong to dismiss it as "ceremony."
Spec §10.

### Status code conventions

Distilled from the survey:

| Status | Use |
|---|---|
| 202 Accepted | Async command accepted (not yet executed) |
| 204 No Content | Successful DELETE / Cancel |
| 400 | Malformed JSON |
| 401 | Missing / invalid auth |
| 403 | Plan / quota / scope denial (not retryable) |
| 409 | Duplicate / state conflict (with body indicating existing resource) |
| 422 | Validation error (well-formed but invalid) |
| 424 | Upstream dependency failed (Codex's contribution) |
| 429 | Rate limit (with `Retry-After`) |
| 503 | Platform-wide outage |
| 507 | Capacity exhausted (Recall's pattern; distinct retry strategy) |

The distinction between 429 (back off and try again soon) and 507
(provisioning capacity, not throughput; user-actionable) is worth
preserving.

### MCP surface — composite tools, not REST mirror

MeetingBaaS's `meeting-mcp` is the reference for production meeting
MCP servers. Notable patterns:

- **Composite tools**: `findKeyMoments(botId, topics, maxMoments)`
  returns a markdown list — single LLM call, no client-side loop.
  `shareMeetingSegments(botId, segments[])` builds a multi-link share
  doc atomically.
- **Workflow chaining via prose-tools**: `oauthGuidance` returns
  *prose instructions* for the model to follow. Replaces docs the
  model never reads.
- **Auth translation**: Bearer token at MCP layer translates to the
  vendor's `X-API-Key` at the REST layer.

For heron's MCP surface (separate from this spec), the pattern is
"read-heavy + composite," not "1:1 with REST." Heron's bot is launched
out-of-band by the desktop app, so MCP doesn't need provisioning
tools — only reading + influence (`speak`, `mute`, `leave`).

---

## Open vendor questions (unresolved)

These wait on the spike:

1. **Recall vs Attendee on disclosure injection.** Spec §4 requires
   that the bot speak before doing anything else. Does Recall's
   `output_audio` fire fast enough after `bot_create` to meet a
   reasonable disclosure-window UX? Does Attendee?
2. **Live PCM latency budget.** Spec §1's `heron-bridge` needs to
   route audio at <50ms total bridge latency for natural turn-taking.
   What does Recall's WebSocket actually deliver in practice on a
   trans-Pacific link?
3. **Kick-out granularity.** Spec §7's `EjectReason` enum is rich
   (HostRemoved / RecordingDenied / AdmissionRefused / etc.). Do
   vendors actually distinguish, or do they all collapse to
   "ejected, reason unknown"?
4. **Voice-clone backend privacy.** ElevenLabs / Cartesia are
   network-only; Piper is local. The persona model (spec §6) supports
   either, but the privacy posture argument for AGPL'd heron favors
   local-first if quality is acceptable.

Each of these becomes a single bullet in the spike report.

## Sources

Original three-agent synthesis: see conversation history (turn:
"Multi-model API design synthesis"). Key citations from that work:

- [Recall.ai docs](https://docs.recall.ai/)
- [Attendee on GitHub](https://github.com/attendee-labs/attendee)
- [MeetingBaaS API v2 announcement](https://www.meetingbaas.com/en/api/introducing-meeting-baas-v2)
- [MeetingBaaS meeting-mcp](https://github.com/Meeting-Baas/meeting-mcp)
- [Vexa](https://github.com/Vexa-ai/vexa) — see also conversation deep-read
- [LiveKit Agents](https://docs.livekit.io/agents/)
- [Pipecat](https://docs.pipecat.ai/)
- [OpenAI Realtime API](https://platform.openai.com/docs/guides/realtime)
- [OpenAI Realtime VAD guide](https://platform.openai.com/docs/guides/realtime-vad)
- [Stripe API versioning](https://docs.stripe.com/api/versioning)
- [Stripe idempotent requests](https://docs.stripe.com/api/idempotent_requests)
- [Microsoft Graph onlineMeeting](https://learn.microsoft.com/en-us/graph/api/resources/onlinemeeting)
- [Google Meet REST API](https://developers.google.com/workspace/meet/api/reference/rest)
