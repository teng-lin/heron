# heron

A private, on-device meeting AI for macOS. Two modes:

- **Note-taker** — sits beside your meeting app (no bot in the call),
  records natively, transcribes locally, and writes a markdown
  summary into your Obsidian vault.
- **Meeting agent** — joins the call as a participant on your behalf
  through a meeting-bot driver, listens, takes turns, and speaks.
  The notes the agent leaves behind become long-term memory it draws
  on for future meetings.

In note-taker mode, audio never leaves your machine — only the
transcript text is sent to your chosen LLM provider for
summarization. In agent mode, audio necessarily travels to your
meeting-bot driver (Recall.ai today) and the realtime LLM session,
in both cases via API keys you supply. See [Where your data
goes](#where-your-data-goes) for the per-mode breakdown.

## What heron does

### Note-taker

- **Records natively, no bot in the call.** heron taps your meeting
  app's audio directly via Core Audio process taps. Other participants
  see no extra invitee. No "Otter is recording" banner.
- **Real speaker names, not "Speaker 1 / 2 / 3".** heron reads the
  meeting app's accessibility tree to attribute turns to the actual
  display names visible in the participant list — it doesn't guess
  speakers from voice clustering.
- **On-device transcription.** WhisperKit runs locally on Apple Neural
  Engine. Your voice never goes to a transcription cloud.
- **Markdown into your Obsidian vault.** Each meeting becomes one
  `<id>.md` file with frontmatter, transcript, and summary. The vault
  folder lives wherever you keep it — local, iCloud, Dropbox, Google
  Drive. heron itself never touches sync.

### Meeting agent

- **Attends meetings as a participant.** A four-layer
  driver / bridge / policy / realtime stack joins the call through a
  meeting-bot driver (Recall.ai is the first one wired), hears the
  room, and speaks back through OpenAI's realtime API.
- **Pre-meeting context from turn one.** Calendar agenda, attendee
  notes, and briefing material staged via `attach_context` flow into
  the bot's persona and the realtime session's system prompt — the
  agent walks in informed, not cold.
- **Explicit speech-control policy.** What the agent says is gated
  by a policy contract — muted profile, topic allow/deny lists,
  "only speak when addressed", escalation hooks for sensitive
  topics — not free-form improvisation. You stay in control of when
  the agent talks.
- **Long-term memory across meetings.** Notes from the note-taker
  side feed back into the agent as memory, so it can ground its
  answers in what was actually said in earlier meetings instead of
  guessing.
- **You bring the LLM.** Summarization and the realtime session both
  use your Anthropic or OpenAI API key, stored in the macOS Keychain.
  Pick the provider you trust; rotate the key whenever.

The four-layer composition is wired end-to-end as of the alpha shape:
the daemon installs a `LiveSessionFactory` at boot when both
`OPENAI_API_KEY` and a Recall key are available, and `start_capture`
opens a real bot + realtime + bridge + policy session alongside the
note-taker pipeline. Audio handling uses a naive bridge (no AEC, no
production-grade jitter handling) and daemon state is in-memory —
both are explicitly alpha-shaped, slated for hardening before GA.
See [`docs/architecture.md`](docs/architecture.md) for the layered
contracts.

## What's supported today

| | Note-taker | Meeting agent |
|---|---|---|
| **Operating system** | macOS 14.2+ (Apple Silicon) | macOS 14.2+ (Apple Silicon) |
| **Meeting app** | Zoom (native macOS client) | Anything Recall.ai supports (Zoom, Meet, Teams, Webex) |
| **Recording / transcription** | ✅ alpha | ✅ alpha |
| **Summarization** | ✅ alpha | ✅ alpha |
| **Speaker attribution by real name** | ✅ alpha | — (uses Recall's per-participant audio) |
| **Realtime speech (agent talks back)** | n/a | ✅ alpha (OpenAI realtime backend) |
| **Pre-meeting context → agent persona** | n/a | ✅ alpha |
| **Speech-control policy gating** | n/a | ✅ alpha |
| **Production-grade audio bridge (AEC + jitter)** | n/a | 🚧 GA — alpha ships on a naive bridge |
| **Durable daemon state across restarts** | (planned) | 🚧 GA — alpha is in-memory only |
| **Other operating systems** | Windows, Linux on the roadmap | Windows, Linux on the roadmap |

If you're on Intel Mac, Windows, or Linux, the binary won't run
yet — check back as the cross-platform work lands.

## Installation

### Prerequisites

- **macOS 14.2 (Sonoma) or later** on Apple Silicon (M1/M2/M3/M4).
  macOS 14.2 is the floor because Core Audio process taps require
  it.
- **Xcode Command Line Tools** — `xcode-select --install`.
- **[Rust](https://rustup.rs/)** (rustup managed; the workspace pins
  its toolchain).
- **[Bun](https://bun.sh/)** for the desktop frontend.
- **ffmpeg** — `brew install ffmpeg`.
- **A Zoom client** installed.
- **An Anthropic or OpenAI API key.**

### Build from source

heron is currently distributed as source — pre-built binaries will
come once the cross-platform matrix is closer to filled in.

Clone and run the pinned-toolchain bootstrap once:

```sh
git clone https://github.com/teng-lin/heron.git
cd heron

# Install pinned Rust toolchain + checks system deps. Idempotent —
# safe to re-run after upgrades.
./scripts/setup-dev.sh
```

Then pick what you want to build.

#### Desktop app (full UI)

```sh
cd apps/desktop
bun install
bun run tauri build           # release .app
# or:
bun run tauri dev             # iterative dev loop
```

The signed `.app` bundle lands in
`apps/desktop/src-tauri/target/release/bundle/macos/heron.app`. Drag
it into `/Applications` and launch it.

#### CLI binaries (`heron` and `herond`)

The workspace ships two binary crates: **`heron-cli`** (which
produces a binary named `heron` — not `heron-cli`) and **`herond`**
(the localhost daemon).

```sh
# From the workspace root.
cargo build -p heron-cli -p herond --release
```

That writes:

```text
target/release/heron        # main CLI: record / summarize / status / salvage / …
target/release/herond       # localhost daemon on 127.0.0.1:7384
```

Patch the rpath so the binaries can find the bundled ONNX runtime:

```sh
install_name_tool -add_rpath @executable_path/ target/release/heron
install_name_tool -add_rpath @executable_path/ target/release/herond
```

`sherpa-rs` drops `libonnxruntime.1.17.1.dylib` next to the
binary in `target/release/`, but the binaries it produces only
have `LC_RPATH=/usr/lib/swift` baked in. Without the
`@executable_path/` rpath added, dyld fails to load the dylib at
launch (`Library not loaded: @rpath/libonnxruntime.1.17.1.dylib`).
The patch is a one-time fix per build — re-run after each
`cargo build --release`. Future versions may bake this in via a
post-build hook.

Smoke check:

```sh
./target/release/heron --version
./target/release/heron status
./target/release/herond --version
```

If you'd rather run via cargo without picking the path manually:

```sh
cargo run --release --bin heron -- status
cargo run --release --bin herond
```

##### Put them on `$PATH`

```sh
# Option A — symlink into ~/.local/bin (assuming it's on PATH)
ln -sf "$(pwd)/target/release/heron"  ~/.local/bin/heron
ln -sf "$(pwd)/target/release/herond" ~/.local/bin/herond

# Option B — install via cargo (always release, copies into ~/.cargo/bin)
cargo install --path crates/heron-cli
cargo install --path crates/herond
```

#### Other useful binaries in the workspace

```sh
cargo build -p heron-doctor --release   # offline log-anomaly analyzer
cargo build -p heron-vault --bin validate-vault --release   # vault integrity check
```

Both end up in `target/release/` next to `heron` and `herond`.

## Quick start

### Note-taker

When you launch heron for the first time:

1. **Run the onboarding wizard.** A few quick checks make sure your
   machine is ready:
   - **Microphone** — heron needs your voice.
   - **System audio** — Core Audio process tap on the Zoom native
     macOS client. Other meeting apps (Meet, Teams, Webex) are on
     the roadmap but not wired in note-taker mode today.
   - **Accessibility** — lets heron read window titles and the Zoom
     participant list. Without it, transcripts label everyone
     `Speaker 1 / 2 / 3`.
   - **Calendar** — optional. Pre-fills meeting titles from
     Calendar.app. Off by default if you'd rather keep it offline.
   - **Speech-to-text model** — heron downloads ~1 GB of WhisperKit
     models on first run. Connect to Wi-Fi if you're on a metered
     link.
   - **Runtime checks** — environment sweep (ONNX runtime, Zoom
     binary, keychain ACL, network reachability).
   - **Background service** — verifies heron's local daemon is
     running.
   Each step has a Test button. Most are skippable; the wizard is
   forgiving — you can revisit later.

2. **Add an API key.** *Settings → API Keys* — paste your Anthropic or
   OpenAI key. heron stores it in the macOS Keychain; it never
   appears in plain text on disk and never crosses the IPC bridge in
   either direction after you save it.

3. **Pick a vault.** *Settings → Storage* — choose a folder for your
   meeting notes. Most people point this at an existing Obsidian
   vault or a folder in their cloud-synced Documents directory.

4. **Record your first meeting.** Open Zoom, join the call. In heron
   click **Start Recording** (or hit ⌘⇧R). The menubar tray turns red
   while you're recording. Click **Stop** when the meeting ends.

5. **Read the summary.** heron transcribes locally, summarizes via
   your LLM, and writes the markdown file into your vault. The
   review window opens automatically; you can also open the file in
   Obsidian.

### Meeting agent

To turn on the agent path so heron joins as a participant and
speaks back, in addition to the note-taker setup above:

1. **Add an OpenAI API key** in *Settings → API Keys*. The realtime
   session needs OpenAI specifically (Anthropic alone won't enable
   the agent mode yet — the realtime backend trait has only an
   OpenAI implementation today).

2. **Set a Recall.ai API key.** Today this is read from the
   `RECALL_API_KEY` environment variable when the daemon starts —
   the Settings-UI field is a known follow-up. Easiest path: launch
   the app from a terminal that has the variable exported, or set
   it in your shell profile:

   ```sh
   export RECALL_API_KEY=...
   open -a heron
   ```

3. **(Optional) Stage pre-meeting context.** Use the `attach_context`
   API on the daemon (`POST /v1/context`, see
   [`docs/api-desktop-openapi.yaml`](docs/api-desktop-openapi.yaml))
   to attach an agenda, attendee list, or briefing keyed by your
   calendar event id. The orchestrator forwards it into the bot's
   persona and the realtime session's system prompt at session
   start, so the agent knows the room from turn one.

4. **Start a meeting normally.** When both keys are present at
   daemon boot, the orchestrator installs a `LiveSessionFactory` and
   every `start_capture` opens the four-layer agent stack alongside
   the note-taker pipeline. Recall joins the meeting on your behalf;
   the realtime session listens and speaks per your speech-control
   policy. End the meeting and the agent leaves cleanly; the
   note-taker output ends up in your vault as usual.

If either key is missing the orchestrator falls back to note-taker
only — capture is never blocked by realtime unavailability.

## Where your data goes

**Note-taker only:**

| Stays on your machine | Goes to your LLM provider |
|---|---|
| Raw audio | The transcript text (for summarization only) |
| WhisperKit transcript | The summarization prompt |
| Final markdown notes | |
| API keys (in macOS Keychain) | |

In note-taker mode, audio is never uploaded to anyone. The
transcript text leaves the device only when you trigger
summarization, and only to the LLM provider whose key you supplied.

**Meeting agent:**

| Stays on your machine | Goes out (and where) |
|---|---|
| Speech-control policy decisions | Audio of the meeting → Recall.ai (the bot driver — Recall hears the call so it can mix the agent's voice in) |
| Note-taker transcript + summary | Realtime audio + transcript ↔ OpenAI (the realtime backend — bidirectional voice session) |
| Final markdown notes in your vault | Pre-meeting context (agenda / attendees / briefing) → bot persona + realtime system prompt |

Agent mode necessarily uploads audio: a meeting agent that cannot
hear the room cannot participate in it. Today's pipeline routes
audio through Recall (bot driver) and through OpenAI (realtime
session); both legs use your own API keys, and the orchestration
layer that decides what the agent says runs locally.

heron has no analytics, no telemetry, and no first-party server.

## Common tasks

- **Re-summarize a note** — open it in heron's review window, click
  *Re-summarize*. heron rotates the prior body into `<id>.md.bak`,
  runs the summarizer again, and shows you a diff before saving.
- **Restore a backup** — same review window, *Restore* button.
- **Purge old audio** — *Settings → Audio → Audio retention*. Set a
  day count, or "keep all".
- **Change the recording hotkey** — *Settings → Recording*. Default
  ⌘⇧R; conflicts with system shortcuts surface inline.
- **Recover a crashed session** — heron scans for unfinalized
  sessions on launch and offers to salvage them.

## How heron is different

**As a note-taker:**

- **Fireflies / Otter** join the call as a visible bot. heron's
  note-taker doesn't.
- **Granola** is invisible too, but collapses everyone's audio to one
  mixed track and clusters speakers by voice — no real names. heron
  reads the meeting app's own speaker signal so you get actual
  display names without ML clustering on the happy path.
- **Char / oh-my-whisper** are closer to the right shape but don't
  solve per-speaker attribution for native meeting clients.

In the modal outcome on a Zoom call, heron's note-taker attributes
~70% of turns to a real name with high confidence; the remaining
~30% are marked `them` with a low-confidence visual indicator. The
pitch is **"Fireflies-quality attribution on most turns, without a
bot"** — not "Fireflies-quality transcripts always."

**As a meeting agent:**

- **Cluely / Read AI / generic "AI assistants"** are coaching layers
  on top of your screen — they observe, they don't participate.
  heron's agent mode joins the call and speaks when it should.
- **Recall.ai-built "agent" demos** typically pipe everything through
  a hosted backend. heron runs the orchestrator and policy locally;
  only the realtime LLM session leaves your machine, and only to the
  provider whose key you supplied. The bot driver is pluggable —
  Recall today, others later.
- **Speech control is explicit**, not vibes. The policy layer gates
  every utterance: muted profile, topic allow/deny, per-meeting
  silence. The agent doesn't free-form on a hot mic.

## Troubleshooting

- **"Permission denied" / TCC prompts on first launch** — macOS asks
  for Microphone, Accessibility, and (optionally) Calendar permissions
  as the wizard hits each step. Grant in *System Settings → Privacy &
  Security*; heron's onboarding has a Re-test button for each.
- **No audio captured from Zoom** — confirm Zoom is the bundle ID set
  in *Settings → Recorded apps* (default `us.zoom.xos`). Web Zoom
  needs `us.zoom.us` instead.
- **Speaker labels are all `them`** — accessibility permission isn't
  granted, or the Zoom participant list is collapsed. Open the
  participant list during the call.
- **Summary failed** — usually the LLM API key is missing or out of
  credit. Check *Settings → API Keys*.
- **Run the doctor** — `heron-doctor` walks
  `~/Library/Logs/heron/<date>.log` and surfaces the most recent
  errors with a fix suggestion.

## How it works

heron has four moving parts:

- **The desktop app** — a Tauri shell (Rust backend + React/Tailwind
  frontend) that owns the wizard, settings, recording controls, and
  review window.
- **The note-taker pipeline** — Core Audio process tap, ringbuffer
  with backpressure, WhisperKit STT, AXObserver speaker attribution,
  and a markdown writer into your Obsidian vault.
- **The agent stack** — four layers spread across `heron-bot`
  (driver: joins the call), `heron-bridge` (PCM jitter buffer +
  resample + mix), `heron-policy` (speech-control contract: when /
  what the agent is allowed to say), and `heron-realtime`
  (bidirectional realtime LLM session).
- **The local daemon (`herond`)** — runs on `127.0.0.1:7384`,
  publishes meeting-lifecycle events on an SSE bus, and exposes a
  small HTTP API used by the desktop app and the `heron` CLI. Never
  accepts connections from anywhere but localhost.

For the full picture — process topology, crate map, data flows,
the four-layer agent contract — see
[`docs/architecture.md`](docs/architecture.md).

## Contributing

Bug reports and pull requests welcome. Every change goes through the
same `/polish` + `/pr-workflow` pipeline (code-simplifier →
multi-model review → ultrathink → CI gates → squash-merge); see
[`CONTRIBUTING.md`](CONTRIBUTING.md) for the conventions and
gates.

## License

[GNU AGPL-3.0-or-later](./LICENSE). Each workspace crate inherits
this from `[workspace.package]` in `Cargo.toml`, and `cargo-deny`
is configured to allow it.

AGPL was chosen so anyone running heron as a network service — a
hosted note-taker, a hosted meeting agent — must publish their
modifications under the same terms. Personal and in-company use is
unrestricted.
