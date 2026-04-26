# heron v2 API design spec — invariants & state machines

Companion to [`architecture-agent-participant.md`](./architecture-agent-participant.md).
That doc proposes the v2 pivot (passive note-taker → meeting participant)
and lists the new crates. This doc encodes the **shape-determining
constraints** as state machines, invariants, and contract-level
consequences — the things that have to be pinned down *before* any
vendor (Recall / Attendee / native Zoom SDK) is chosen, otherwise the
vendor choice becomes architecture by accident.

If a question in the v2 doc could be answered "we'll figure it out
later," the answer was probably wrong, and this doc is where it gets
hardened.

The companion vendor survey is in
[`api-design-research.md`](./api-design-research.md). This doc cites
*conclusions* from the survey; it does not re-survey.

---

## 1. Architecture recap — four layers, not two

The v2 doc lists six new crates; for API-design purposes they collapse
into four design surfaces with distinct shape-and-contract properties:

| Layer | Crates | Shape | Latency | Vendor-coupled? |
|---|---|---|---|---|
| **Driver** | `heron-bot` | REST-ish, lifecycle-heavy, async | seconds | yes (Recall / Attendee / native) |
| **Bridge** | `heron-audio` (extended), maybe a new `heron-bridge` | mechanical: PCM frames, AEC, jitter, echo cancel | ms | no |
| **Policy** | `heron-policy`, `heron-turn` | decisional: should-I-speak, when, what's-allowed | ms | no |
| **Realtime** | `heron-realtime`, `heron-voice` | event-stream, low-latency, OpenAI-Realtime-modeled | sub-second | partly (model API) |

**Invariant 1.** Vendor-specific quirks live in `heron-bot` only. The
policy and realtime layers must be expressible against a vendor-neutral
trait surface. If a policy decision needs to know "are we on Recall vs
Attendee," the abstraction in `heron-bot` has leaked.

**Invariant 2.** The four layers must not share URL spaces or
type vocabularies that span more than one layer. A `BotId` belongs to
the driver layer; `UtteranceId` belongs to the realtime layer; they
cross via explicit handoff (a `bot_id` field on a session-init event),
never by import.

---

## 2. Identity model

### Canonical IDs (Stripe-style prefixed UUIDs)

| Resource | Prefix | Example |
|---|---|---|
| Bot | `bot_` | `bot_01HXG7…` |
| Meeting | `mtg_` | `mtg_01HXG8…` |
| Persona | `prs_` | `prs_01HXG9…` |
| Utterance | `utt_` | `utt_01HXGA…` |
| Session (realtime) | `sess_` | `sess_01HXGB…` |
| Webhook event | `evt_` | `evt_01HXGC…` |

UUIDv7 underneath (time-ordered, sortable in logs); base32-encoded.
Prefix is for human/log readability, not for parsing.

**Invariant 3.** Internal Rust APIs pass typed handles
(`struct BotId(Uuid)`, `struct UtteranceId(Uuid)`), never strings.
String form is a serialization concern.

### External resolvers

LLM tool callers and humans-with-URLs do not have prefixed UUIDs in
hand. They have meeting URLs, calendar event IDs, persona names. The
MCP/tool boundary exposes resolvers:

```
resolve_bot_by_meeting_url(url: &str) -> Option<BotId>
resolve_meeting_by_provider_ref(platform: Platform, native_id: &str) -> Option<MeetingId>
resolve_persona_by_name(name: &str) -> Option<PersonaId>
```

**Invariant 4.** Composite keys (`/bots/{platform}/{native_id}`) and
URLs are *resolver inputs*, never primary identity. They appear in
MCP tool argument schemas; they do not appear in REST URL paths or
database primary keys. Provider IDs are unstable and heterogeneous;
basing identity on them leaks vendor semantics into the whole system.

---

## 3. Bot lifecycle finite state machine

```
                     bot_create(meeting_url, persona_id, context)
                                       │
                                       ▼
                                 ┌──────────┐
                                 │   init   │
                                 └────┬─────┘
                                      │ persona load
                                      ▼
                              ┌────────────────┐
            persona_load_fail │   loading      │
            ◀─────────────────┤   persona      │
            │                 └────┬───────────┘
            │                      │ persona_loaded
            ▼                      ▼
         [failed]            ┌───────────────┐
                       ┌─────│  tts_warming  │
                       │     └────┬──────────┘
            tts_init_fail        │ tts_ready (heron-realtime initialized)
                       ▼          ▼
                   [failed]   ┌────────────┐
                              │  joining   │
                              └────┬───────┘
                            join_failed │ joined_meeting
                                  ▼     ▼
                              [failed] ┌─────────────┐
                                       │ disclosing  │
                                       └────┬────────┘
                          disclosure_decline│ disclosure_acked (Δt no objection)
                                  ▼          ▼
                              [leaving] ┌─────────────┐
                                        │ in_meeting  │
                                        └────┬────────┘
                       ┌─────────────────────┼──────────────────────────────┐
                       ▼              ▼      ▼              ▼               ▼
                   bot_leave()    ejected   host_ended  network_lost  internal_error
                       │              │      │              │               │
                       ▼              ▼      ▼              ▼               ▼
                   [leaving]    [ejected] [host_ended]  [reconnecting]  [failed]
                       │                                     │
                       ▼                              (reconnect within
                  [completed]                          T_recover or fail)
```

### Per-state contracts

- **`init` → `loading_persona`**: `bot_create()` is async, returns immediately with `BotId`. State observable via subscribe.
- **`loading_persona` → `tts_warming`**: persona loaded means voice clone reference resolved + system prompt assembled. Fast (~100ms) if cached.
- **`tts_warming` → `joining`**: this is **load-bearing** — see §4.
- **`joining` → `disclosing`**: bot is admitted to meeting (past waiting room).
- **`disclosing` → `in_meeting`**: see §4.
- **`in_meeting`** is the only state where `speak()`, `chat()`, `screen()` are valid.
- **Terminal states**: `completed`, `failed`, `ejected`, `host_ended` all finalize the vault transcript and emit `bot.completed{outcome}`.

**Invariant 5.** No state outside `in_meeting` accepts speech-control
calls. Calls in other states return `Err(NotInMeeting{current_state})`,
not 4xx.

---

## 4. Disclosure compliance

### Constraint

Two-party-consent jurisdictions (California, Florida, Illinois,
Pennsylvania, Washington in the US; most of the EU under GDPR) require
*all* parties to consent to recording. An AI agent attending in lieu
of a human introduces a *second* disclosure: the agent itself.

### Design consequences

1. **Disclosure is the bot's first audible action** in the meeting,
   before any other utterance. Not optional, not configurable to
   "off" — the configuration surface is the disclosure *text*, not
   its presence.
2. **TTS must be initialized and have a voice loaded BEFORE
   `bot_join()` returns success.** This is why `tts_warming` precedes
   `joining` in the FSM. A bot that joins a meeting and *then*
   discovers TTS is broken can't disclose, can't legally remain.
3. **The `disclosing → in_meeting` transition requires acknowledgment
   *or* a Δt timeout with no host objection.** Concretely:
   - Bot speaks: "Hi, I'm Teng's AI assistant. I'm participating on
     Teng's behalf and a recording is being created. If anyone
     objects, please say so now."
   - Heron listens for ~5s for keywords matching objection patterns
     (`object`, `stop`, `leave`, `not okay`, `please don't`).
   - On objection → `disclosing → leaving` (graceful exit with
     "Understood, I'll leave now").
   - On no objection → `disclosing → in_meeting`.
4. **Re-disclosure on participant join**: when a new participant joins
   mid-meeting (via `bot.participant_joined` event from driver), the
   bot must re-announce within ~10s of their join. Implementation:
   `heron-policy` watches the event and enqueues a disclosure utterance
   with `priority: Append`.

### Configuration surface

```rust
struct DisclosureProfile {
    text_template: Handlebars,         // {{user_name}}, {{meeting_title}}
    voice_override: Option<VoiceId>,   // default: persona's voice
    objection_patterns: Vec<Regex>,    // default: jurisdiction-bundled
    objection_timeout: Duration,       // default: 5s
    re_announce_on_join: bool,         // default: true
    jurisdiction: Jurisdiction,        // selects default text + patterns
}
```

**Invariant 6.** `bot_create()` rejects with `Err(NoDisclosureProfile)`
if `DisclosureProfile` is absent or has empty `text_template`. There
is no such thing as a heron bot that joins silently.

---

## 5. Multi-meeting orchestration

### v2.0 decision

**`max_concurrent_bots = 1`.** A second `bot_create()` while a bot is
not in a terminal state returns `Err(BotAlreadyActive{existing: BotId})`.

### Rationale

- Compute: WhisperKit + Realtime LLM + TTS pipeline saturates an M-series
  laptop. Two concurrent agents would degrade both.
- Audio: only one agent can hold the microphone metaphor. Mixing two
  agent voices into the user's awareness is a UX problem with no good
  answer in v2.0.
- Identity: one human = one agent persona at a time.

### Future expansion (v2.1+)

If concurrent meetings become a requirement, the expansion path is
**persona-pool-based, not bot-pool-based**: one persona, sequenced
across meetings ("the agent left Meeting A to join Meeting B"). Not
"two simultaneous agents." This is a product decision, not an
architecture decision; calling it out now prevents `heron-bot` from
being designed for plural concurrency it'll never have.

**Invariant 7.** `heron-bot` is a singleton in v2.0. The crate exposes
a single global handle, not a `Vec<Bot>`. Tests use a
`#[cfg(test)]` factory; production wires through `OnceCell`.

---

## 6. Persona / voice identity

### Resource model

`Persona` is a top-level resource, lifecycle-independent of bots.

```rust
struct Persona {
    id: PersonaId,
    name: String,
    voice: VoiceClone,           // model handle + reference samples
    system_prompt: String,       // assembled at session init
    expertise_areas: Vec<String>, // for LLM "I know about X" gating
    decisions_ref: VaultPath,    // path to prior-decisions notes
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

struct VoiceClone {
    backend: VoiceBackend,       // ElevenLabs | Cartesia | Piper | …
    backend_voice_id: String,    // backend-specific identifier
    fallback_voice_id: Option<String>,  // used if primary backend down
}
```

### Lifecycle

- Created via `persona_create()`, stored in `heron-vault` (`personas/<id>.md`
  with YAML frontmatter — keeps the user-owns-the-data ethos).
- Voice clone enrollment is out-of-band (recording samples, uploading
  to backend). `heron-doctor` validates a persona can actually speak.
- A bot is created with `persona_id`; the persona's voice is loaded
  during `loading_persona → tts_warming`.

### API consequence

```rust
async fn bot_create(
    meeting_url: Url,
    persona_id: PersonaId,
    context: PreMeetingContext,
    disclosure: DisclosureProfile,
) -> Result<BotId>;
```

**Invariant 8.** A bot without a persona is a bug. There is no anonymous
bot. The error is at create-time, not at join-time.

---

## 7. Kick-out semantics

### Constraint

The meeting platform may eject the bot, the host may end the meeting,
the network may drop, or the bot may hit an internal error. These are
*not* failures of the next API call from heron-the-app; they are
state transitions that the platform initiates.

### Event-driven, not error-driven

A kick-out manifests as a state change emitted on `heron-events`:

```rust
enum BotEvent {
    Ejected { bot_id, reason: EjectReason, at: DateTime<Utc>, details: Option<String> },
    HostEnded { bot_id, at: DateTime<Utc> },
    NetworkLost { bot_id, at: DateTime<Utc>, will_attempt_reconnect: bool },
    Reconnected { bot_id, at: DateTime<Utc> },
    InternalError { bot_id, at: DateTime<Utc>, error: String },
    // … other events
}

enum EjectReason {
    HostRemoved,
    RecordingPermissionDenied,  // host clicked "stop recording" on the bot
    AdmissionRefused,           // never made it past waiting room
    PolicyViolation,            // platform-side bot-detection
    Unknown,
}
```

### Design consequences

1. **No 4xx for ejections.** `speak()` after ejection returns
   `Err(NotInMeeting{current_state: Ejected})`, not `Err(Ejected)` —
   the latter implies the call itself caused the ejection.
2. **Cleanup is automatic.** State transition into a terminal state
   (`Ejected | HostEnded | Failed`) triggers:
   - `heron-realtime` session teardown
   - `heron-vault` transcript finalization
   - `heron-policy` flushing pending utterances
   - `bot.completed{outcome: Ejected{reason}}` emitted
3. **Reconnect is bounded.** `NetworkLost → Reconnected` allowed
   within `T_recover = 30s`. After timeout, transition to terminal
   `Failed{reason: NetworkTimeout}`.

**Invariant 9.** Every terminal-state transition emits exactly one
`bot.completed` event. Consumers must be safe to receive *only*
`bot.completed` and infer everything from its `outcome` field.

---

## 8. Pre-meeting context injection

### Constraint

The agent in the meeting needs to know what the meeting is about, who's
attending, and what was previously discussed with these people. Without
this, it's a chatty stranger; with it, it's an assistant.

### Source

Pre-meeting context is assembled by `heron-cli` from:

- Calendar event (via `heron-vault::calendar` EventKit bridge): title,
  description, attendees with emails
- Vault search: previous meeting notes with overlapping attendees
- User-supplied: per-meeting context override (`heron speak-for --note "..."`)

### Shape

```rust
struct PreMeetingContext {
    agenda: Option<String>,
    attendees_known: Vec<AttendeeContext>,
    related_notes: Vec<VaultPath>,        // paths only, content loaded lazily
    prior_decisions: Vec<DecisionRef>,
    user_briefing: Option<String>,        // free-form user note
}

struct AttendeeContext {
    name: String,
    email: Option<String>,
    last_seen_in: Option<MeetingId>,
    relationship: Option<String>,         // "manager", "client", "report"
    notes: Option<String>,                // user-curated bio
}
```

### Lifecycle integration

Context is consumed during `loading_persona → tts_warming`: the
persona's `system_prompt` template is rendered with the context as
input, producing the actual prompt sent to `heron-realtime` at session
init. Context is **not** mutable mid-meeting; if new info arrives
(e.g. host shares a doc), it goes through `heron-realtime`'s normal
turn-stream, not through context.

**Invariant 10.** Context size is capped at 16K tokens before being
passed to `heron-realtime`. Larger context is summarized (via a
non-realtime LLM call during `loading_persona`) before being injected.
A bot's context cannot mid-meeting balloon to break the realtime
session's context window.

---

## 9. Speech-control contract

### Trait

```rust
#[async_trait]
pub trait SpeechController: Send + Sync {
    /// Enqueue or interrupt with new speech.
    async fn speak(
        &self,
        text: &str,
        priority: Priority,
        voice_override: Option<VoiceId>,
    ) -> Result<UtteranceId, SpeechError>;

    /// Cancel a specific utterance by ID. OK if utterance is already
    /// completed, cancelled, or unknown — returns Ok(()) idempotently.
    async fn cancel(&self, id: UtteranceId) -> Result<(), SpeechError>;

    /// Clear queue but let current utterance finish.
    async fn cancel_all_queued(&self) -> Result<(), SpeechError>;

    /// Panic-stop: cancel current + clear queue. The "barge-in by user"
    /// reflex.
    async fn cancel_current_and_clear(&self) -> Result<(), SpeechError>;

    /// Subscribe to lifecycle events for all utterances.
    fn subscribe_events(&self) -> BoxStream<'static, SpeechEvent>;
}

pub enum Priority {
    /// Append to the end of the queue.
    Append,
    /// Cancel current + clear queue + speak (single atomic operation,
    /// no audible gap). This is what avoids the cancel-then-speak race.
    Replace,
    /// Cancel current only + speak (queue stays). For corrections.
    Interrupt,
}

pub enum SpeechEvent {
    Started { id: UtteranceId, started_at: DateTime<Utc> },
    Progress { id: UtteranceId, words_spoken: u32 },
    Completed { id: UtteranceId, duration: Duration },
    Cancelled { id: UtteranceId, reason: CancelReason },
    Failed { id: UtteranceId, error: String },
}

pub enum CancelReason {
    UserRequested,
    Replaced { by: UtteranceId },     // Priority::Replace caused this
    BargedIn { by_speaker: SpeakerId },
    PolicyDenied { rule: String },
    Failed,
}
```

**Invariant 11.** `Priority::Replace` is a single primitive, not
`cancel(current)` followed by `speak(text)`. The two-call sequence has
a race producing audible silence between the cancel-completion and the
new utterance starting. This is what every existing vendor gets wrong
and is the single most product-defining speech-API decision.

### Capability degradation

Different realtime backends support different subsets of this contract.
The spec accepts this and exposes a capability matrix:

```rust
pub struct SpeechCapabilities {
    pub utterance_ids: bool,       // false → all returns share Uuid::nil()
    pub per_utterance_cancel: bool,
    pub queue: bool,               // false → Append behaves like Replace
    pub atomic_replace: bool,      // false → emulated as cancel+speak (race exists)
    pub barge_in_detect: bool,     // server-side VAD vs client-side
}
```

When the chosen backend can't honor a primitive, the implementation
returns `Err(SpeechError::CapabilityNotSupported{cap})` so policy can
fall back deliberately, rather than silently degrade.

---

## 10. heron-events bus shape

### Canonical: in-process Tokio bus

```rust
pub struct EventBus {
    sender: tokio::sync::broadcast::Sender<Event>,
}

impl EventBus {
    pub fn publish(&self, event: Event);
    pub fn subscribe(&self) -> BroadcastStream<Event>;
}
```

**Invariant 12.** All events flow through `heron-events` first. No
crate publishes events on its own private channel. This is what makes
the four-layer architecture composable; without it, every adapter has
to know every publisher.

### Adapters (transports)

The same `Event` enum is surfaced via four projections:

| Adapter | Audience | Shape |
|---|---|---|
| In-proc Rust subscriber | Other heron crates | `BroadcastStream<Event>` |
| Tauri IPC | Desktop UI | Tauri events; serde-serialized payloads |
| MCP notifications | Claude Desktop, Cursor, etc. | MCP server-to-client notifications |
| Outbound webhook (optional) | User's automation (Zapier, n8n, Home Assistant) | HTTP POST with HMAC envelope (see below) |

**Invariant 13.** The trait is canonical, transports are projections.
A new transport (gRPC, NATS, whatever) is purely additive. No
adapter-specific event types exist.

### Outbound webhook envelope

For users who opt into "POST to my URL when bot.completed fires":

```json
POST <user-configured URL>
X-Heron-Signature: sha256=<HMAC over canonicalized JSON>
X-Heron-Timestamp: <unix_secs>
Content-Type: application/webhook+json

{
  "event_id": "evt_01HXG…",
  "idempotency_key": "evt_01HXG…",
  "event_type": "bot.completed",
  "api_version": "2026-04-25",
  "created_at": "2026-04-25T12:34:56Z",
  "metadata": { /* echoed from bot_create */ },
  "data": { /* event-specific */ }
}
```

This is the *only* part of the spec that imports SaaS conventions —
because a user's outbound webhook receiver IS effectively a SaaS
integration. HMAC signing protects the loopback case (tunneled local
URLs); `idempotency_key` covers retries; `api_version` covers the
inevitable schema evolution.

---

## 11. Inbound network discipline (consume-side)

When `heron-bot` wraps Recall.ai or Attendee, heron is a **client of**
those APIs. The synthesis vocabulary that the pushback dismissed as
"SaaS ceremony" is exactly what's needed here, just inverted from
publisher to consumer:

- **Idempotency-Key on outbound `POST /bots`** to Recall: a network
  hiccup retry must not double-join the meeting. `heron-bot`
  generates a UUIDv7 per intent; if Recall returns success after a
  retry, the second 200 is the *same* bot, not a duplicate.
- **HMAC signature verification on inbound webhooks** from Recall to
  our local tunnel/proxy: the synthesis's webhook patterns become
  validation logic, not publishing logic.
- **Structured error parsing**: Recall's `sub_code` and MeetingBaaS's
  `FST_ERR_*` codes get mapped to a heron-internal error taxonomy.
  The taxonomy is heron's; the parsing is per-vendor.
- **Rate-limit handling**: respect `Retry-After` headers; back off on
  429; surface 507 (Recall capacity) as a user-visible "the bot
  service is full, try again in N minutes" rather than a generic error.

**Invariant 14.** Vendor-API discipline lives entirely in `heron-bot`.
Higher layers see `Result<T, BotError>` with a heron-internal taxonomy.
A change of vendor (Recall → Attendee → native) is a re-implementation
of `heron-bot`, not a ripple through the whole codebase.

---

## 12. Invariants summary

| # | Invariant |
|---|---|
| 1 | Vendor quirks live only in `heron-bot`. |
| 2 | The four layers don't share URL spaces or types across more than one layer. |
| 3 | Internal Rust APIs use typed handles, never strings. |
| 4 | Composite keys / URLs are resolver inputs, never primary identity. |
| 5 | No state outside `in_meeting` accepts speech-control calls. |
| 6 | A bot without a `DisclosureProfile` is rejected at create time. |
| 7 | `heron-bot` is a singleton in v2.0. |
| 8 | A bot without a persona is a bug, rejected at create time. |
| 9 | Every terminal state emits exactly one `bot.completed` event. |
| 10 | Pre-meeting context is capped at 16K tokens; over-cap goes through summarization. |
| 11 | `Priority::Replace` is a single primitive, not cancel-then-speak. |
| 12 | All events flow through `heron-events` first. |
| 13 | The trait is canonical; transports are projections. |
| 14 | Vendor-API discipline lives entirely in `heron-bot`. |

These are the constraints the bot-driver choice (Recall vs Attendee
vs native) gets evaluated against. A driver that can't honor an
invariant either gets adapted or rejected; we don't bend the
invariants to fit the driver.

## 13. What's intentionally NOT specced here

- **Specific bot-driver choice.** Resolved by the spike (next step).
- **Specific realtime backend choice** (OpenAI Realtime vs Gemini Live
  vs local). Resolved by latency benchmarking.
- **Voice clone backend choice** (ElevenLabs vs Cartesia vs Piper).
  Resolved by quality + privacy tradeoff analysis.
- **Specific MCP tool surface.** Downstream of the trait files; design
  in the trait sketches, not here.
- **Avatar / video output.** Out of scope for v2.0 per
  `architecture-agent-participant.md`.
- **Pricing / quota model.** Heron is a Mac app, not a hosted product;
  pricing applies to the upstream APIs (Recall, OpenAI), not heron.

## Next steps

1. Sketch the four traits as Rust files: `heron-bot`, `heron-policy`,
   `heron-realtime`, `heron-bridge`. ~100 lines each, with the error
   enums and capability matrices from §9 wired in.
2. Spike `heron-bot` against Recall.ai with the contract: `join`,
   `listen`, `speak`, `interrupt`, `eject-detect`, `disclosure-inject`.
   Single goal: discover which invariants Recall can honor.
3. Refine traits per spike findings.
4. Commit to driver. Then build the rest.
