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
heron-audio        capture (tap + mic + AEC)
heron-stt          transcribe (pluggable: whisperkit, sensevoice, cloud)
heron-diarize      speaker attribution
heron-session      lifecycle: calendar + app + audio → state machine
heron-store        persistence + query (SQLite)
heron-llm          summarize / extract / template
heron-index        semantic search across sessions
heron-events       event stream + query API (the agent surface)
```

Everything else — Tauri GUI, `whisper` CLI, MCP server, a hypothetical
Raycast extension — is a thin client over `heron-events`. That is the only
decomposition under which `oh-my-whisper`'s "files on disk" model and
`char`'s GUI model both fall out as 100-line adapters.

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
│   Tauri GUI       CLI (whisper)     MCP server     Agent loop    Raycast     │
│   (char desktop)  (oh-my-whisper)   (Claude/IDE)   (you)         ext.        │
└────────┬─────────────┬──────────────────┬──────────────┬────────────┬────────┘
         │             │                  │              │            │
         ▼             ▼                  ▼              ▼            ▼
┌──────────────────────────────────────────────────────────────────────────────┐
│                            heron-events                                       │
│  subscribe()  query()  inject_context()  on(summary.ready) ...               │
│                                                                              │
│  session.detected ─ started ─ transcript.partial ─ transcript.final          │
│                ─ ended ─ summary.ready ─ action_items.ready                  │
└──────────────────────────────────────────────────────────────────────────────┘
                                     ▲
          ┌──────────────────────────┼──────────────────────────┐
          │                          │                          │
          │        ┌─────────────────┴──────────────────┐       │
          │        │          heron-session              │       │
          │        │  (lifecycle state machine: the     │       │
          │        │   piece that makes it "ambient")   │       │
          │        │                                    │       │
          │        │  calendar + running app + audio    │       │
          │        │  activity  →  DETECTED → ARMED     │       │
          │        │  → RECORDING → ENDED → DONE        │       │
          │        └──┬──────────┬─────────────┬────────┘       │
          │           │arms      │triggers     │emits events    │
          │           ▼          ▼             │                │
          │  ┌──────────────┐  ┌─────────────────────────────┐  │
          │  │ heron-audio   │─▶│ heron-stt                    │  │
          │  │              │  │ (whisperkit / sensevoice /  │  │
          │  │ tap + mic    │  │  cloud, pluggable)          │  │
          │  │ + AEC        │  └──────────────┬──────────────┘  │
          │  └──────────────┘                 │                 │
          │                                   ▼                 │
          │                   ┌──────────────────────────────┐  │
          │                   │  heron-diarize                │  │
          │                   │  (speaker attribution:       │  │
          │                   │   Alice/Bob, not just        │  │
          │                   │   mic-vs-speaker)            │  │
          │                   └──────────────┬───────────────┘  │
          │                                  │                  │
          │                                  ▼                  │
          │                   ┌──────────────────────────────┐  │
          │                   │  heron-llm                    │  │
          │                   │  summarize · extract actions │  │
          │                   │  · templates · routing       │  │
          │                   │  (local ↔ cloud ↔ BYO)       │  │
          │                   └──────────────┬───────────────┘  │
          │                                  │                  │
          │           ┌──────────────────────┴──────┐           │
          │           ▼                             ▼           │
          │  ┌──────────────────┐        ┌────────────────────┐ │
          │  │  heron-store      │◀──────▶│  heron-index        │ │
          │  │  SQLite          │        │  vector search     │ │
          │  │  sessions, notes │        │  across meetings   │ │
          │  │  transcripts     │        │                    │ │
          │  └──────────────────┘        └────────────────────┘ │
          │                                                     │
          └────────────── consent / TCC / disclosure ───────────┘
                          (cross-cutting, owned by heron-session)

┌──────────────────────────────────────────────────────────────────────────────┐
│                       macOS primitives                                       │
│                                                                              │
│  Core Audio         CPAL /          EventKit         FSEvents /              │
│  Process Taps       AVAudioEngine   (Calendar)       NSWorkspace             │
│  (system audio)     (mic)                            (running apps)          │
│                                                                              │
│  TCC: kTCCServiceAudioCapture · kTCCServiceMicrophone · Calendar             │
└──────────────────────────────────────────────────────────────────────────────┘
```

### Reading the diagram

- **Vertical axis is trust / abstraction.** OS primitives at the bottom,
  reusable libraries in the middle, consumer UIs at the top. Every consumer
  goes through `heron-events` — no consumer pokes `heron-audio` or `heron-stt`
  directly.
- **`heron-session` is the hub, not `heron-audio`.** It is the only component
  wired to all three OS signal sources (calendar, running apps, audio
  activity). It *arms* the audio pipeline rather than being driven by it —
  that is the "ambient" property.
- **Horizontal flow is the data pipeline.** `audio → stt → diarize → llm →
  store + index`. Each stage is pluggable: swap WhisperKit for SenseVoice,
  swap local LLM for Claude, swap SQLite for something else — nothing else
  changes because they all talk through `heron-events`.
- **`heron-events` is the agent surface.** It carries both the realtime event
  stream (subscribe) and the historical query API (query). An agent never
  touches anything else. `oh-my-whisper`'s `~/whisper/` files + `--json` is
  one implementation of this surface; an MCP server is another; a gRPC
  socket is a third.
- **Consent lives on the session boundary** because that is where "a
  recording is about to start" is a meaningful moment — not at the audio
  layer, which is too low, and not in the UI, which is too client-specific.

### The one invariant to defend

**Consumers talk to `heron-events`, never to internal libraries.** The moment
the GUI reaches directly into `heron-store`, or the CLI imports `heron-audio`,
the decomposition is lost — every future consumer has to re-learn the whole
system.
