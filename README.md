# Heron

A private, on-device meeting AI for macOS. Named for the heron — the bird sacred to Athena in Greek myth: patient, watchful, precise. Three modes carry the metaphor forward:

- **Clio** *(Muse of history)* — silent note-taker. Records natively, transcribes locally, and writes a markdown summary into your Obsidian vault. Other meeting participants see no extra invitee.
- **Athena** *(goddess of wise counsel)* — listens to your meeting and surfaces relevant facts, draft replies, and trigger flags in a heron sidebar. You stay the only voice in the room. *(Coming.)*
- **Pollux** *(the immortal twin)* — speaks in your cloned voice through a virtual microphone, attends meetings on your behalf, and hands off to you when something important happens. The double-booking solver. *(Coming.)*

In Clio mode, audio never leaves your machine — only the transcript text is sent to your chosen LLM provider for summarization. Athena and Pollux have their own data postures, called out per-mode below. See [Where your data goes](#where-your-data-goes) for the full breakdown.

## What's shipping today

**Clio is shipping today (alpha).** Athena and Pollux are tracked in [`docs/heron-implementation.md`](docs/heron-implementation.md); the per-mode roadmap and architecture rationale live in [`docs/heron-vision.md`](docs/heron-vision.md).

| | Clio | Athena | Pollux |
|---|---|---|---|
| **Operating system** | macOS 14.2+ (Apple Silicon) | (coming) | (coming) |
| **Meeting app** | Zoom (native macOS client) | Zoom + Meet + Teams | Zoom + Meet + Teams |
| **Recording / transcription** | ✅ alpha | ✓ inherits Clio | ✓ inherits Clio |
| **Summarization** | ✅ alpha | ✓ | ✓ |
| **Speaker attribution by real name** | ✅ alpha | ✓ | ✓ |
| **Realtime LLM listening** | n/a | ✓ | ✓ |
| **Suggestions surfaced in sidebar** | n/a | ✓ | ✓ |
| **Hand-off classifier (trigger flags)** | n/a | ✓ | ✓ |
| **Speaks into the meeting** | n/a | ❌ — never | ✓ (cloned voice) |
| **Multi-meeting concurrency (double-booking)** | n/a | n/a | ✓ |

If you're on Intel Mac, Windows, or Linux, the binary won't run yet — check back as the cross-platform work lands.

## What heron does

### Clio (silent note-taker)

- **Records natively, no bot in the call.** heron taps your meeting app's audio directly via Core Audio process taps. Other participants see no extra invitee. No "Otter is recording" banner.
- **Real speaker names, not "Speaker 1 / 2 / 3".** heron reads the meeting app's accessibility tree to attribute turns to the actual display names visible in the participant list — it doesn't guess speakers from voice clustering.
- **On-device transcription.** WhisperKit runs locally on Apple Neural Engine. Your voice never goes to a transcription cloud.
- **Markdown into your Obsidian vault.** Each meeting becomes one `<id>.md` file with frontmatter, transcript, and summary. The vault folder lives wherever you keep it — local, iCloud, Dropbox, Google Drive. heron itself never touches sync.

### Athena (whispered counsel) — coming

Athena does everything Clio does, **plus**:

- **Listens with you in real time** via a streaming LLM session, with your pre-meeting briefing context loaded.
- **Surfaces help in a sidebar** in heron's existing window — relevant facts from your vault, suggested replies when you're asked something, and trigger flags when your name is mentioned, a decision is requested, or a topic falls outside your briefing.
- **Never speaks into the meeting.** No virtual mic, no TTS, no impersonation. The user remains the only voice.

Legal posture: recording consent (same as Clio) plus a light AI-assistance disclosure ("I'm using an AI assistant during this call"). No voice biometrics. No deepfake.

### Pollux (your twin in the room) — coming

Pollux does everything Athena does, **plus**:

- **Speaks in your cloned voice** through a heron-shipped HAL plug-in (a virtual microphone the user selects in their meeting app once).
- **Hand-off when it matters.** When the classifier fires on a meaningful trigger, Pollux stalls with filler ("Let me think about that for a moment"), pings you with a desktop alert, and seamlessly hands the mic back to you on a keystroke.
- **Multi-meeting concurrency.** You attend meeting A normally; Pollux attends meeting B on your behalf, with isolated audio.

Legal posture: full voice biometrics consent flow (BIPA / GDPR-aware), per-meeting consent capture, two-party-consent jurisdiction auto-disclosure. Required: legal sign-off on consent text before any external launch.

## Installation

### Prerequisites

- **macOS 14.2 (Sonoma) or later** on Apple Silicon (M1/M2/M3/M4). macOS 14.2 is the floor because Core Audio process taps require it.
- **Xcode Command Line Tools** — `xcode-select --install`.
- **[Rust](https://rustup.rs/)** (rustup managed; the workspace pins its toolchain).
- **[Bun](https://bun.sh/)** for the desktop frontend.
- **ffmpeg** — `brew install ffmpeg`.
- **A Zoom client** installed.
- **An Anthropic or OpenAI API key.**

### Build from source

heron is currently distributed as source — pre-built binaries will come once the cross-platform matrix is closer to filled in.

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

The `.app` bundle lands in `target/release/bundle/macos/heron.app` (workspace target — Cargo shares the target directory across all workspace crates). Drag it into `/Applications` and launch it.

Tauri's bundler does not copy the `sherpa-rs` ONNX dylibs into the bundle, so a one-time post-build patch is needed before the `.app` will launch:

```sh
APP=target/release/bundle/macos/heron.app
mkdir -p "$APP/Contents/Frameworks"
cp -p target/release/libonnxruntime.1.17.1.dylib \
      target/release/libsherpa-onnx-c-api.dylib \
      target/release/libsherpa-onnx-cxx-api.dylib \
      "$APP/Contents/Frameworks/"
(cd "$APP/Contents/Frameworks" \
  && ln -sf libonnxruntime.1.17.1.dylib libonnxruntime.dylib)
install_name_tool -add_rpath @executable_path/../Frameworks \
  "$APP/Contents/MacOS/heron-desktop"
```

If you're running directly out of cargo via `cargo run -p heron-desktop --release` (or via `bun run tauri dev`), patch the standalone binary instead — the dylibs already sit next to it in `target/release/` (or `target/debug/`):

```sh
install_name_tool -add_rpath @executable_path/ target/release/heron-desktop
install_name_tool -add_rpath @executable_path/ target/debug/heron-desktop
```

Both patches are one-time per build; re-run after each rebuild until a Tauri post-build hook handles it automatically.

#### CLI binaries (`heron` and `herond`)

The workspace ships two binary crates: **`heron-cli`** (which produces a binary named `heron` — not `heron-cli`) and **`herond`** (the localhost daemon).

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

`sherpa-rs` drops `libonnxruntime.1.17.1.dylib` next to the binary in `target/release/`, but the binaries it produces only have `LC_RPATH=/usr/lib/swift` baked in. Without the `@executable_path/` rpath added, dyld fails to load the dylib at launch (`Library not loaded: @rpath/libonnxruntime.1.17.1.dylib`). The patch is a one-time fix per build — re-run after each `cargo build --release`. Future versions may bake this in via a post-build hook.

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

## Quick start (Clio)

When you launch heron for the first time:

1. **Run the onboarding wizard.** A few quick checks make sure your machine is ready:
   - **Microphone** — heron needs your voice.
   - **System audio** — Core Audio process tap on the Zoom native macOS client. Other meeting apps (Meet, Teams, Webex) are on the roadmap but not wired in Clio mode today.
   - **Accessibility** — lets heron read window titles and the Zoom participant list. Without it, transcripts label everyone `Speaker 1 / 2 / 3`.
   - **Calendar** — optional. Pre-fills meeting titles from Calendar.app. Off by default if you'd rather keep it offline.
   - **Speech-to-text model** — heron downloads ~1 GB of WhisperKit models on first run. Connect to Wi-Fi if you're on a metered link.
   - **Runtime checks** — environment sweep (ONNX runtime, Zoom binary, keychain ACL, network reachability).
   - **Background service** — verifies heron's local daemon is running.
   Each step has a Test button. Most are skippable; the wizard is forgiving — you can revisit later.

2. **Add an API key.** *Settings → API Keys* — paste your Anthropic or OpenAI key. heron stores it in the macOS Keychain; it never appears in plain text on disk and never crosses the IPC bridge in either direction after you save it.

3. **Pick a vault.** *Settings → Storage* — choose a folder for your meeting notes. Most people point this at an existing Obsidian vault or a folder in their cloud-synced Documents directory.

4. **Record your first meeting.** Open Zoom, join the call. In heron click **Start Recording** (or hit ⌘⇧R). The menubar tray turns red while you're recording. Click **Stop** when the meeting ends.

5. **Read the summary.** heron transcribes locally, summarizes via your LLM, and writes the markdown file into your vault. The review window opens automatically; you can also open the file in Obsidian.

## Where your data goes

**Clio (today):**

| Stays on your machine | Goes to your LLM provider |
|---|---|
| Raw audio | The transcript text (for summarization only) |
| WhisperKit transcript | The summarization prompt |
| Final markdown notes | |
| API keys (in macOS Keychain) | |

In Clio mode, audio is never uploaded to anyone. The transcript text leaves the device only when you trigger summarization, and only to the LLM provider whose key you supplied.

**Athena (coming):**

| Stays on your machine | Goes out (and where) |
|---|---|
| Speech (only the transcript text leaves) | Transcript text → realtime LLM (OpenAI today) for live suggestions |
| All audio | |
| Sidebar suggestion history | |
| Pre-meeting briefing context (sent at session start) → realtime LLM |  |

Athena uploads no audio. The realtime LLM session sees the transcript stream and your pre-meeting briefing; suggestions come back to your sidebar.

**Pollux (coming):**

| Stays on your machine | Goes out (and where) |
|---|---|
| Speech-control policy decisions | Voice sample (one-time, at onboarding) → cloning provider (e.g. ElevenLabs) for clone creation |
| Note-taker transcript + summary | Transcript text → realtime LLM (OpenAI today) for live conversation generation |
| Final markdown notes in your vault | TTS audio → cloning provider per utterance |
| Cloned voice ID (in macOS Keychain) | Pre-meeting briefing → realtime system prompt |

Pollux necessarily uploads more: the cloned voice samples once, and the realtime LLM session for conversation generation. All cloud legs use your own API keys; the orchestration layer that decides what the bot says runs locally.

heron has no analytics, no telemetry, and no first-party server.

## Common tasks

- **Re-summarize a note** — open it in heron's review window, click *Re-summarize*. heron rotates the prior body into `<id>.md.bak`, runs the summarizer again, and shows you a diff before saving.
- **Restore a backup** — same review window, *Restore* button.
- **Purge old audio** — *Settings → Audio → Audio retention*. Set a day count, or "keep all".
- **Change the recording hotkey** — *Settings → Recording*. Default ⌘⇧R; conflicts with system shortcuts surface inline.
- **Recover a crashed session** — heron scans for unfinalized sessions on launch and offers to salvage them.

## How heron is different

**As a note-taker (Clio):**

- **Fireflies / Otter** join the call as a visible bot. Clio doesn't.
- **Granola** is invisible too, but collapses everyone's audio to one mixed track and clusters speakers by voice — no real names. Clio reads the meeting app's own speaker signal so you get actual display names without ML clustering on the happy path.
- **Char / oh-my-whisper** are closer to the right shape but don't solve per-speaker attribution for native meeting clients.

In the modal outcome on a Zoom call, Clio attributes ~70% of turns to a real name with high confidence; the remaining ~30% are marked `them` with a low-confidence visual indicator. The pitch is **"Fireflies-quality attribution on most turns, without a bot"** — not "Fireflies-quality transcripts always."

**As a real-time helper (Athena):**

- **Cluely / Read AI / generic "AI assistants"** mostly land here too — coaching layers on top of your screen. Athena's differentiator is per-platform speaker attribution feeding the LLM (so suggestions are speaker-aware) and the same hand-off classifier infrastructure that powers Pollux (so the trigger flags are precise).

**As a meeting surrogate (Pollux):**

- **Nothing in market does this.** The double-booking use case — sending a cloned-voice surrogate to a meeting on your behalf, with a hand-off classifier that pings you when something important happens — is novel. Pollux is the headline mode but the smaller-audience tier; most users will land on Clio or Athena. Pollux carries voice biometrics and impersonation responsibilities that the simpler modes don't, and ships behind a stricter consent flow.

## Troubleshooting

- **"Permission denied" / TCC prompts on first launch** — macOS asks for Microphone, Accessibility, and (optionally) Calendar permissions as the wizard hits each step. Grant in *System Settings → Privacy & Security*; heron's onboarding has a Re-test button for each.
- **No audio captured from Zoom** — confirm Zoom is the bundle ID set in *Settings → Recorded apps* (default `us.zoom.xos`). Web Zoom needs `us.zoom.us` instead.
- **Speaker labels are all `them`** — accessibility permission isn't granted, or the Zoom participant list is collapsed. Open the participant list during the call.
- **Summary failed** — usually the LLM API key is missing or out of credit. Check *Settings → API Keys*.
- **Run the doctor** — `heron-doctor` walks `~/Library/Logs/heron/<date>.log` and surfaces the most recent errors with a fix suggestion.

## How it works

heron has four moving parts:

- **The desktop app** — a Tauri shell (Rust backend + React/Tailwind frontend) that owns the wizard, settings, recording controls, and review window.
- **The Clio pipeline** — Core Audio process tap, ringbuffer with backpressure, WhisperKit STT, AXObserver speaker attribution, and a markdown writer into your Obsidian vault.
- **The companion stack** *(in development for Athena and Pollux)* — `heron-companion` orchestrates a streaming LLM session over the same audio capture; `heron-handoff` runs the trigger classifier; `heron-policy` gates what the bot is allowed to say (Pollux only); `heron-virtual-mic` + the heron HAL plug-in inject TTS into the meeting (Pollux only).
- **The local daemon (`herond`)** — runs on `127.0.0.1:7384`, publishes meeting-lifecycle events on an SSE bus, and exposes a small HTTP API used by the desktop app and the `heron` CLI. Never accepts connections from anywhere but localhost.

For the full picture — process topology, crate map, data flows, the per-mode contracts — see [`docs/architecture.md`](docs/architecture.md), [`docs/heron-vision.md`](docs/heron-vision.md), and [`docs/heron-implementation.md`](docs/heron-implementation.md).

## Contributing

Bug reports and pull requests welcome. Every change goes through the same `/polish` + `/pr-workflow` pipeline (code-simplifier → multi-model review → ultrathink → CI gates → squash-merge); see [`CONTRIBUTING.md`](CONTRIBUTING.md) for the conventions and gates.

## License

[GNU AGPL-3.0-or-later](./LICENSE). Each workspace crate inherits this from `[workspace.package]` in `Cargo.toml`, and `cargo-deny` is configured to allow it.

AGPL was chosen so anyone running heron as a network service — a hosted note-taker, a hosted meeting agent — must publish their modifications under the same terms. Personal and in-company use is unrestricted.
