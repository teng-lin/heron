# heron v2: agent-as-meeting-participant (pivot proposal)

Companion to [`architecture.md`](./architecture.md). That doc describes a
private, on-device **note-taker** — an ambient listener that captures
meetings and writes Obsidian notes. This doc sketches what the system would
look like if we pivoted to an **agent that attends meetings on behalf of
the human** — joins the call as a participant, listens, and *speaks*.

The core inversion: meetings stop being a thing the user attends and the
software watches. Meetings become a thing the user's agent attends and the
user can either be there too or not. The note-taker becomes a side effect.

## What changes vs. v1

| concern | v1 (note-taker) | v2 (participant) |
|---|---|---|
| audio | capture-only (system tap + mic) | **bidirectional** — also play back into the call |
| STT | offline batch (WhisperKit Swift bridge) | **streaming/realtime** (Realtime API or local streaming Whisper) |
| LLM | post-hoc summarizer | **realtime conversational** (low-latency turn-taking) |
| TTS | none | **first-class** (voice cloning of user, low latency) |
| meeting access | passive: read system audio + AX | **active: join as a participant** (bot SDK or native client) |
| primary output | markdown notes in Obsidian vault | **utterances spoken in the meeting**; notes are a side effect |
| identity | the user is in the meeting | **the agent represents the user**; consent + persona become load-bearing |
| events | nice-to-have (`heron-events` planned) | **mandatory** — realtime turn-taking can't be built any other way |

## What we keep from v1

- **`heron-audio`** — still need capture. Add a playback path into the bot's virtual mic.
- **`heron-zoom`** — Zoom AXObserver attribution gets *more* valuable: the agent needs to know who is currently speaking, when it's being addressed by name, and when there's a turn-taking gap to fill.
- **`heron-vault`** — demoted from primary product to **agent memory**: prior meeting transcripts and summaries become the long-term context the agent draws on ("here's what Alice said last week about this PR").
- **`heron-types`, `heron-cli`, `heron-doctor`** — orchestrator + diagnostics carry over, scope grows.
- **Swift bridge pattern** (`docs/archives/swift-bridge-pattern.md`) — re-used for any new Apple-native capability.

## What we add

```
heron-bot          [new] meeting-bot driver: join Zoom/Meet/Teams as a
                         participant. Options (pick one): Recall.ai client
                         (commercial, fastest), Attendee self-hosted
                         (OSS, fits private-by-default ethos), or native
                         Zoom SDK (deepest integration, most work).

heron-voice        [new] TTS + voice cloning of the user. Streams synthesized
                         audio into the bot's virtual mic. Built on a
                         Pipecat-style or LiveKit-Agents-style pipeline.

heron-realtime     [new] bidirectional realtime LLM connection (OpenAI
                         Realtime, Gemini Live, or a local streaming model).
                         Owns the audio-in/audio-out + tool-call loop.

heron-turn         [new] turn-taking + VAD + barge-in policy. Decides "is
                         it my turn to speak now?" using AX speaker signal,
                         silence detection, and explicit name-mention.
                         The hardest piece by far.

heron-policy       [new] *what* the agent is allowed to say and when:
                         topic allowlist/denylist, escalation rules
                         ("punt to the human if asked about salary"),
                         confidence thresholds, hard mute on sensitive
                         meetings. Owns the consent surface.

heron-persona      [new] long-lived identity: voice clone, speaking style,
                         expertise areas, things the user has previously
                         decided/committed to. Persists across meetings.
                         Reads from heron-vault for episodic memory.

heron-events       [now mandatory] realtime event bus: turn.started,
                         turn.yielded, addressed_by_name, intent.detected,
                         agent.spoke, agent.muted, escalation.requested.
                         No realtime system can be built without this.

heron-avatar       [optional, off by default] photoreal talking-face
                         (Tavus / HeyGen / D-ID). Video output into the
                         bot's virtual camera. Off-by-default for a
                         privacy-first product.
```

## Architecture diagram

```
┌──────────────────────────────────────────────────────────────────────────────┐
│                          THE MEETING (Zoom / Meet / Teams)                   │
│                                                                              │
│   Alice 🎙           Bob 🎙           [agent-of-user] 🎙 🤖                   │
│                                              ▲  │                            │
│                                              │  ▼                            │
└──────────────────────────────────────────────┼──┼────────────────────────────┘
                              audio in (to agent) │  │ audio out (from agent)
                                                  │  │
                                                  │  ▼
                              ┌───────────────────┴─────────────────────┐
                              │  heron-bot                              │
                              │  joins meeting as a participant         │
                              │  Recall.ai / Attendee / native Zoom SDK │
                              │  exposes: virtual mic, virtual cam,     │
                              │           transcript stream, AX attrib  │
                              └───────────────┬─────────────────────────┘
                                              │
            ┌─────────────────────────────────┴───────────────────────────────┐
            │                          heron-events                           │
            │   the bus everything talks through (now MANDATORY, not aspir.)  │
            │                                                                 │
            │   turn.started · turn.yielded · addressed_by_name · silence     │
            │   intent.detected · agent.spoke · agent.muted · escalation      │
            │   transcript.partial · transcript.final · summary.ready         │
            └────┬───────────┬────────────┬──────────────┬─────────────┬──────┘
                 │           │            │              │             │
                 ▼           ▼            ▼              ▼             ▼
        ┌─────────────┐ ┌──────────┐ ┌──────────┐ ┌────────────┐ ┌──────────┐
        │ heron-audio │ │ heron-   │ │ heron-   │ │ heron-turn │ │ heron-   │
        │ tap + mic + │ │ speech   │ │ zoom     │ │ VAD +      │ │ policy   │
        │ AEC +       │ │ STREAMING│ │ AXObserv │ │ name-detect│ │ allow/   │
        │ playback    │ │ STT      │ │ attrib.  │ │ + barge-in │ │ deny +   │
        │ into bot mic│ │ (Realtime│ │          │ │ + silence  │ │ escalate │
        │             │ │  or local│ │ "who is  │ │ "is it MY  │ │ "may I   │
        │             │ │  Whisper │ │  talking │ │  turn?"    │ │  speak?" │
        │             │ │  stream) │ │  now?"   │ │            │ │          │
        └─────────────┘ └────┬─────┘ └────┬─────┘ └─────┬──────┘ └────┬─────┘
                             │            │             │             │
                             └────────────┴──────┬──────┴─────────────┘
                                                 ▼
                          ┌─────────────────────────────────────────────┐
                          │  heron-realtime                             │
                          │  bidirectional LLM (OpenAI Realtime /       │
                          │  Gemini Live / local streaming)             │
                          │                                             │
                          │  inputs:  audio + persona + memory + tools  │
                          │  outputs: audio (→ heron-voice) + tool calls│
                          └─────┬────────────────────────────┬──────────┘
                                │                            │
                                ▼                            ▼
                  ┌──────────────────────────┐   ┌──────────────────────────┐
                  │  heron-voice             │   │  heron-persona           │
                  │  TTS + voice clone of    │   │  long-lived identity:    │
                  │  the user                │   │  voice clone refs,       │
                  │  streams audio into the  │   │  speaking style,         │
                  │  bot's virtual mic       │   │  expertise, prior        │
                  │  (sub-second latency)    │   │  decisions, hot topics   │
                  │                          │   │                          │
                  │  optional: heron-avatar  │   │  reads memory from ↓     │
                  └──────────────────────────┘   └────────────┬─────────────┘
                                                              │
                                                              ▼
                                                ┌───────────────────────────┐
                                                │  heron-vault              │
                                                │  (DEMOTED to agent        │
                                                │  memory; markdown notes   │
                                                │  are a side-effect)       │
                                                │                           │
                                                │  prior meetings, action   │
                                                │  items, who-said-what     │
                                                │  serve as RAG corpus      │
                                                └────────────┬──────────────┘
                                                             │
                                                             ▼
                                            ┌──────────────────────────────┐
                                            │  heron-index   [planned]     │
                                            │  semantic search over the    │
                                            │  vault for in-meeting recall │
                                            │  ("what did Alice say last   │
                                            │   sprint about retries?")    │
                                            └──────────────────────────────┘

┌──────────────────────────────────────────────────────────────────────────────┐
│                  CONSENT / DISCLOSURE / TRUST  (cross-cutting)               │
│                                                                              │
│  - The agent must announce itself on join ("Hi, I'm Teng's agent").          │
│  - heron-policy enforces "what the agent will not say" per-meeting.          │
│  - Hard kill switch: human can mute or eject the agent from any UI.          │
│  - Audit log of every agent utterance, written to heron-vault.               │
│  - Two-party-consent jurisdictions: recording disclosure + agent disclosure  │
│    are now TWO separate notices.                                             │
└──────────────────────────────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────────────────────────────┐
│                           CONSUMERS (thin clients)                           │
│                                                                              │
│   Tauri desktop          heron CLI         heron-doctor                      │
│   "send my agent to      "agent join       (diagnostics:                     │
│    this meeting"         <meeting-url>"    voice clone health,               │
│                                            realtime API quota,               │
│                                            consent prompts pass)             │
└──────────────────────────────────────────────────────────────────────────────┘
```

### Reading the diagram

- **The meeting is at the top, not the bottom.** In v1 the OS is the
  bottom layer because heron *passively observes* it. In v2 the meeting is
  the top layer because the agent is *acting in* it. Same architecture
  flipped: the agent is now a peer of Alice and Bob.
- **`heron-bot` is the new boundary.** Everything below it is heron;
  everything above it is the meeting platform. Choosing the bot driver
  (Recall.ai vs. Attendee vs. native Zoom SDK) is the most consequential
  decision in v2 — it determines latency, cost, supported platforms, and
  whether the system can be self-hosted at all.
- **`heron-events` is no longer optional.** Realtime turn-taking can't be
  built by direct crate-to-crate wiring; it requires a bus everyone
  subscribes to. The v1 doc treated `heron-events` as an aspirational
  consolidation layer. Here it's the spine.
- **`heron-realtime` is the brain, but `heron-turn` and `heron-policy`
  are what make it *socially viable*.** The realtime LLM can generate
  a response to anything — the question is *should it speak now*, and
  *is it allowed to say this*. Those two crates are the difference between
  "a bot that constantly talks over people" and "an agent you'd actually
  send to a meeting."
- **`heron-vault` is demoted from product to memory.** The markdown notes
  still get written (and arguably matter more — they're the auditable
  record of what the agent did on your behalf), but they are no longer
  the thing the user pays for. The thing the user pays for is "a coherent
  voice in a meeting you didn't have to attend."
- **The Swift bridge layer (omitted from this diagram)** still exists
  unchanged: `whisperkit-helper`, `eventkit-helper`, `zoomax-helper`.
  v2 likely adds at most a `coreaudio-loopback-helper` for low-latency
  bidirectional audio if the CPAL path proves insufficient.

## What this pivot is *not*

- Not "Otter that can speak." The whole point is that the agent is a
  participant with intent, not a recording bot with a voice.
- Not an avatar product. `heron-avatar` is optional and off by default.
  The product is a voice in the meeting; the face is a separate decision.
- Not a Zoom-only play. `heron-bot`'s driver abstraction means Meet and
  Teams come along as long as the chosen driver supports them.
- Not a replacement for the user in *every* meeting. The realistic
  initial scope is meetings where the user can't attend (timezone
  conflict, double-booked, sick) or low-stakes meetings where being
  represented is acceptable (status updates, info shares).

## The hardest open questions

1. **Latency budget.** End-to-end audio-in → LLM → TTS → audio-out under
   ~700ms is the floor for natural turn-taking. Realtime APIs (OpenAI,
   Gemini Live) get there; local stacks currently don't.
2. **Voice cloning ethics + consent.** The user must consent to their
   own voice being cloned, *and* the meeting must be told it is hearing
   a clone. Two consent surfaces, both legally load-bearing.
3. **"When to speak."** `heron-turn` is the equivalent of v1's
   `heron-session` — the load-bearing piece that determines whether the
   product feels natural or feels like a rude bot. Concretely:
   addressed-by-name detection, silence-after-question detection,
   speaker-attribution-driven yield.
4. **What the agent knows about the user.** `heron-persona` plus
   `heron-vault`-backed memory is a non-trivial RAG problem with strong
   privacy constraints — none of this should leave the device unless
   the user opts in.
5. **Build-vs-buy for the bot driver.** Recall.ai is the fastest path
   (commercial API, all platforms supported) but is a dependency on a
   third party that sees every meeting. Attendee is OSS and self-hosted
   and aligns with heron's privacy ethos but is more work and less
   battle-tested. Native Zoom SDK is deepest integration but Zoom-only.
6. **Does the existing `heron-vault` markdown model survive?** It
   probably does — it becomes the agent's long-term memory and the
   audit trail of every utterance. But the writer's "merge-on-write"
   semantics may need to evolve to handle realtime updates rather than
   end-of-meeting writes.

## Migration sketch (rough sequencing if we commit)

1. Land `heron-events` as a real bus (week 1–2). This was already on
   the v1 roadmap; v2 needs it sooner.
2. Pick a `heron-bot` driver and ship a "join + listen" prototype that
   re-uses `heron-speech` for STT and writes to `heron-vault` (week 3–4).
   This is essentially v1's pipeline running through a meeting-bot
   instead of a system audio tap. Validates the bot integration in
   isolation.
3. Add `heron-voice` (TTS + voice clone) and a stub `heron-realtime`
   that just echoes a canned phrase when addressed by name (week 5).
   First end-to-end "agent speaks in a real meeting" demo.
4. Replace stub with real `heron-realtime` + minimal `heron-turn` (VAD +
   addressed-by-name only) + minimal `heron-policy` (allowlist of
   meeting types) (week 6–8).
5. `heron-persona` + vault-backed memory (week 9+).
6. `heron-avatar` is post-v2.0 if at all.

## The one invariant to defend (revised for v2)

**The human must always be able to mute or eject the agent in under one
second from any consumer surface.** v1's invariant ("consumers talk to
`heron-events` only") still holds and is stricter now — but the v2
invariant is about safety, not architecture. An agent the user can't
silence instantly is a liability, not a product.
