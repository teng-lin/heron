# heron-style meeting capture: architecture notes

Design exploration for decomposing a private, on-device meeting capture system
(à la `char` / `oh-my-whisper`) into reusable libraries with a clean agent
integration surface.

## Starting decomposition (as given)

1. Audio capture
2. Transcribe
3. Note organization
4. Meeting integration / contact management

## Gaps worth naming explicitly

- **Session lifecycle / detection.** The "when to start, when to stop, what
  audio belongs to which meeting" state machine. `char` buries this in the
  listener actor; `oh-my-whisper` punts it to the user
  (`whisper record --app us.zoom.xos`). This is the component that makes
  "behind the scenes" actually work — calendar + running-process +
  audio-activity fusion into a state machine. It is not part of capture, not
  part of notes.
- **Diarization / speaker attribution.** Separating mic vs. speaker channels
  (what `char` does today) gives you "me vs. them" — not "Alice vs. Bob within
  them." Different model, different library (pyannote / wespeaker), and most
  downstream features (who-said-what summaries, per-person action items)
  depend on it.
- **Summarization / LLM orchestration.** Often lumped into "note
  organization," but the LLM layer (prompt templates, model routing local ↔
  cloud, BYO-LLM, caching) is its own concern. `char`'s BYO-LLM + templates
  logic is really this layer.
- **Storage / indexing / retrieval.** Once you have 500 meetings, "AI chat
  over notes" needs a vector index and cross-meeting query. Separate from the
  editor.
- **Consent / disclosure.** Legally load-bearing in two-party-consent
  jurisdictions. TCC plumbing is technical; the disclosure / recording-notice
  UX is a product concern that belongs in a reusable library so every
  consumer handles it the same way.
- **Agent integration surface.** The events/query API that external tools
  (GUIs, CLIs, MCP servers, agent loops) consume. See below.

## Target decomposition

```
heron-types        [shipped] shared types (Event, SessionId, SpeakerEvent, …)
heron-audio        [shipped] capture (tap + mic + AEC, ringbuffer, backpressure)
heron-speech       [shipped] transcribe (pluggable: WhisperKit Swift bridge, others)
heron-zoom         [shipped] speaker attribution via Zoom AXObserver + aligner
heron-llm          [shipped] summarize / extract / template (local ↔ cloud ↔ BYO)
heron-vault        [shipped] markdown-vault writer (Obsidian-style notes,
                              EventKit calendar bridge, merge-on-write,
                              m4a encode + verify, ringbuffer purge, validator)
heron-cli          [shipped] `heron` orchestrator binary + lib (session,
                              session_log, synthesize subcommands)
heron-doctor       [shipped] offline diagnostics CLI

heron-diarize      [planned] within-channel speaker attribution
                              (Alice/Bob, beyond mic-vs-speaker)
heron-session      [planned] full lifecycle hub
                              (calendar + running app + audio fusion);
                              currently the orchestrator role is split
                              between heron-cli and heron-zoom
heron-index        [planned] semantic search across sessions
heron-events       [planned] event stream + query API; today consumers
                              wire crates directly via heron-cli
```

Everything outside the core libraries — the Tauri desktop app
(`apps/desktop`), the `heron` CLI, an MCP server, a hypothetical Raycast
extension — should eventually be a thin client over `heron-events`. Today
the desktop app and CLI both wire the crates directly; collapsing that to
a single event surface is still future work.

**Storage model deviation.** Earlier drafts of this doc named `heron-store`
as a SQLite-backed persistence layer. The shipped design replaces it with
`heron-vault` — a plain-markdown vault (YAML frontmatter + Obsidian-style
notes) with merge-on-write semantics. SQLite + a vector index (`heron-index`)
are still on the table for cross-meeting query, but the canonical store is
files on disk so the user owns the data in a tool-agnostic format.

## Ideal agent integration

As a meeting participant the human wants zero involvement. That pushes the
agent-facing API toward **events, not commands**:

1. **Ambient subscribe, not manual poll.** A local IPC / socket (or FSEvents
   over a known directory, à la `oh-my-whisper` but with a proper event fd)
   that emits: `session.detected`, `session.started`, `transcript.partial`,
   `transcript.final`, `session.ended`, `summary.ready`,
   `action_items.ready`. The agent never polls. The agent never asks "is a
   meeting happening?"
2. **Read-only query for history.** SQLite file or gRPC — "give me all action
   items assigned to me this week," "find the meeting where we discussed X."
   This is where the index earns its keep.
3. **Context injection back in.** Before the meeting, push: "here's the
   agenda, here's the relevant PR, here's what this person said last time."
   `heron-session` should accept pre-meeting context keyed to the calendar
   event so summaries come out better.
4. **Post-meeting hooks.** `on(summary.ready) → draft follow-up email`,
   `on(action_items.ready) → create Linear tickets`. The service should not
   ship those integrations; it should expose the event so the agent can.
5. **No `start` / `stop` API in the happy path.** If the agent ever has to
   call `record_start()`, session detection failed. Manual trigger exists as
   an escape hatch, not the primary interface.

**Tradeoff.** Making `heron-session` this smart is the hardest piece by far
(calendar ACLs, per-app audio heuristics, false-positive handling when you
play a YouTube video during lunch), and it is the piece that most determines
whether the product feels ambient or feels like yet another "hit record"
app. Build everything else to be trivially replaceable and sink the design
effort there.

## Architecture diagram

```
┌──────────────────────────────────────────────────────────────────────────────┐
│                        CONSUMERS (thin clients)                              │
│                                                                              │
│   Tauri desktop      heron CLI         heron-doctor      MCP server          │
│   (apps/desktop)     (heron-cli)       (diagnostics)     (planned)           │
└────────┬─────────────────┬──────────────────┬──────────────────┬─────────────┘
         │                 │                  │                  │
         ▼                 ▼                  ▼                  ▼
┌──────────────────────────────────────────────────────────────────────────────┐
│                  heron-events  [planned: see §"invariant"]                   │
│  subscribe()  query()  inject_context()  on(summary.ready) …                 │
│                                                                              │
│  session.detected ─ started ─ transcript.partial ─ transcript.final          │
│                ─ ended ─ summary.ready ─ action_items.ready                  │
│                                                                              │
│  Today: consumers wire heron-cli + crates directly. The event surface is     │
│  the next consolidation.                                                     │
└──────────────────────────────────────────────────────────────────────────────┘
                                     ▲
          ┌──────────────────────────┼──────────────────────────┐
          │                          │                          │
          │        ┌─────────────────┴──────────────────┐       │
          │        │   heron-session  [planned hub]     │       │
          │        │   today: orchestration split       │       │
          │        │   between heron-cli (sessions,     │       │
          │        │   session_log) + heron-zoom        │       │
          │        │   (AXObserver-driven detection)    │       │
          │        │                                    │       │
          │        │  calendar + running app + audio    │       │
          │        │  activity  →  DETECTED → ARMED     │       │
          │        │  → RECORDING → ENDED → DONE        │       │
          │        └──┬──────────┬─────────────┬────────┘       │
          │           │arms      │triggers     │emits events    │
          │           ▼          ▼             │                │
          │  ┌──────────────┐  ┌─────────────────────────────┐  │
          │  │ heron-audio  │─▶│ heron-speech                │  │
          │  │              │  │ (WhisperKit Swift bridge,   │  │
          │  │ tap + mic    │  │  others pluggable)          │  │
          │  │ + AEC +      │  └──────────────┬──────────────┘  │
          │  │ ringbuffer   │                 │                 │
          │  └──────────────┘                 ▼                 │
          │                   ┌──────────────────────────────┐  │
          │                   │  heron-zoom                  │  │
          │                   │  (Zoom AXObserver speaker    │  │
          │                   │   attribution + aligner)     │  │
          │                   │                              │  │
          │                   │  heron-diarize  [planned]    │  │
          │                   │  (within-channel: Alice/Bob) │  │
          │                   └──────────────┬───────────────┘  │
          │                                  ▼                  │
          │                   ┌──────────────────────────────┐  │
          │                   │  heron-llm                   │  │
          │                   │  summarize · extract actions │  │
          │                   │  · templates · routing       │  │
          │                   │  (local ↔ cloud ↔ BYO)       │  │
          │                   └──────────────┬───────────────┘  │
          │                                  ▼                  │
          │                   ┌──────────────────────────────┐  │
          │                   │  heron-vault                 │  │
          │                   │  markdown vault writer       │  │
          │                   │  (Obsidian-style notes,      │  │
          │                   │   merge-on-write,            │  │
          │                   │   m4a encode + verify,       │  │
          │                   │   ringbuffer purge,          │  │
          │                   │   EventKit Swift bridge,     │  │
          │                   │   vault validator)           │  │
          │                   └──────────────┬───────────────┘  │
          │                                  │                  │
          │                                  ▼                  │
          │                   ┌──────────────────────────────┐  │
          │                   │  heron-index   [planned]     │  │
          │                   │  semantic search across      │  │
          │                   │  meetings                    │  │
          │                   └──────────────────────────────┘  │
          │                                                     │
          └────────────── consent / TCC / disclosure ───────────┘
                          (cross-cutting; will live with heron-session)

┌──────────────────────────────────────────────────────────────────────────────┐
│                       Swift bridge layer (swift/)                            │
│                                                                              │
│  whisperkit-helper       eventkit-helper        zoomax-helper                │
│  (heron-speech)          (heron-vault)          (heron-zoom)                 │
│                                                                              │
│  Each crate links a static library produced by a sibling Swift package via   │
│  swift-rs + a build.rs `links =` directive. See docs/archives/swift-bridge-pattern.md.│
└──────────────────────────────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────────────────────────────┐
│                       macOS primitives                                       │
│                                                                              │
│  Core Audio         CPAL /          EventKit         AXObserver /            │
│  Process Taps       AVAudioEngine   (Calendar)       NSWorkspace             │
│  (system audio)     (mic)                            (running apps + UI)     │
│                                                                              │
│  TCC: kTCCServiceAudioCapture · kTCCServiceMicrophone · Calendar             │
│       · Accessibility (AXObserver)                                           │
└──────────────────────────────────────────────────────────────────────────────┘
```

### Reading the diagram

- **Vertical axis is trust / abstraction.** OS primitives at the bottom,
  Swift bridges and reusable Rust libraries in the middle, consumer UIs at
  the top. The eventual rule is that every consumer goes through
  `heron-events` — no consumer pokes `heron-audio` or `heron-speech` directly.
  This is not yet true: today the Tauri shell and `heron-cli` wire crates
  directly. See "the one invariant to defend" below.
- **Swift bridges are first-class, not an implementation detail.** Three
  capabilities — WhisperKit transcription, EventKit calendar reads, and
  Zoom AX observation — only have stable Apple-native APIs in Swift, so each
  consuming crate (`heron-speech`, `heron-vault`, `heron-zoom`) compiles a
  Swift package into a static library and links it via `swift-rs` plus a
  `build.rs` with `links = "…Helper"`. The pattern is documented once in
  `docs/archives/swift-bridge-pattern.md` and copy-pasted with deliberate uniformity.
- **`heron-session` is the hub, not `heron-audio`** — but it does not exist
  as its own crate yet. Today the orchestration role is split:
  `heron-cli::session` owns the `DETECTED → ARMED → RECORDING → ENDED → DONE`
  state machine and `heron-zoom` owns the AXObserver-driven attribution
  signal. The intent is still to consolidate into a `heron-session` hub
  wired to calendar + running apps + audio activity.
- **Horizontal flow is the data pipeline.**
  `audio → speech → zoom (attribution) → llm → vault (+ planned index)`.
  Each stage is pluggable: swap WhisperKit for SenseVoice, swap local LLM
  for Claude, swap the markdown vault for something else — nothing else
  should change because all stages will talk through `heron-events`.
- **`heron-vault` is files on disk, not a database.** Sessions become
  Obsidian-style markdown notes with YAML frontmatter, merged into existing
  notes via `heron-vault::merge`, audio sidecars are encoded to m4a +
  verified, and a validator (`validate-vault` binary) sanity-checks the tree.
  The user owns the data in a tool-agnostic format. A SQLite-backed
  `heron-index` for cross-meeting query is still planned but is additive,
  not a replacement.
- **`heron-events` is the agent surface.** Once it lands it will carry both
  the realtime event stream (subscribe) and the historical query API
  (query). An agent should never touch anything else. The CLI's
  `session_log` writer is the closest thing today; an MCP server and a
  proper subscribe socket are the next steps.
- **Consent lives on the session boundary** because that is where "a
  recording is about to start" is a meaningful moment — not at the audio
  layer, which is too low, and not in the UI, which is too client-specific.

### The one invariant to defend

**Consumers must eventually talk to `heron-events`, never to internal
libraries.** Today the Tauri desktop shell and the `heron` CLI both reach
directly into `heron-audio`, `heron-speech`, `heron-vault`, etc. — that is
expected for a pre-`heron-events` codebase, but every additional consumer
that takes the shortcut increases the cost of introducing the event layer
later. New consumers (MCP server, additional UIs) should be designed
against the planned event surface, not against the current crate graph.
