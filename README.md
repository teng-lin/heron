# heron

A private, on-device AI meeting agent. Two modes:

- **Note-taker (v1, in flight).** Records, transcribes, and summarizes
  meetings without joining as a visible bot.
- **Participant (v2, early implementation).** Attends meetings on
  the user's behalf — listening, taking turns, and speaking. Notes
  become a side effect of the agent's memory rather than the primary
  output. See [`docs/architecture.md`](docs/architecture.md).

In note-taker mode, heron records native meeting calls without joining
as a bot, transcribes locally, and attributes speakers using each
meeting app's own speaker signal — accessibility tree for native
clients, WebRTC track IDs for browser-based ones — so it recovers
real display names instead of voice-clustered `speaker_1` /
`speaker_2` labels. Output is a markdown summary written into an
Obsidian vault. The vault folder lives inside the user's Dropbox /
Google Drive / iCloud — heron itself never touches sync.

In participant mode, a four-layer stack — driver / bridge / policy /
realtime — joins the call through a meeting-bot driver, runs a
bidirectional realtime LLM session, and gates what the agent says
through an explicit policy contract. The first
concrete driver path is Recall.ai; the daemon and event bus are wired
far enough for health, SSE, vault reads, and manual capture lifecycle
events, while realtime speech remains behind the v2 layer contracts.
v2 ships v1's note-taker output as agent memory; the agent draws on
prior meeting transcripts as long-term context.

The product target is a business executive running many external
client meetings, plus the author. Audio never leaves the device
except to the user's chosen LLM / realtime provider.

## Scope

heron is designed to be cross-platform along two axes: operating
system and meeting app. v1 ships a focused slice of both; later
releases fill the matrix in.

### Operating systems — macOS, Windows, Linux

- **v1 (initial release): macOS only.** macOS first because Core
  Audio process taps (14.2+) give clean per-app system audio without
  driver hacks, WhisperKit + Apple Neural Engine give the best local
  STT path on Apple Silicon, and native meeting apps' macOS
  accessibility trees are the cleanest path to real speaker names.
- **Windows (v2):** WASAPI process loopback (Windows 10 build 20348+)
  is the equivalent of Core Audio process taps; UI Automation
  replaces AXObserver.
- **Linux (v2):** PipeWire per-app capture replaces Core Audio;
  AT-SPI replaces AXObserver. Lower priority than Windows because of
  the smaller Linux meeting footprint, but the architecture is
  identical.

### Meeting apps — Zoom, Google Meet, Microsoft Teams, Webex

- **v1 (initial release): Zoom only.** Zoom ships a native macOS
  client whose accessibility tree exposes per-participant mute state,
  which heron uses as the speaker signal in v1 (the §3.3 spike
  pivoted from a "currently speaking" tile to mute-state attribution
  after the AX-tree dumper showed mute state was the more reliable
  indicator across Zoom layouts).
- **Google Meet / Microsoft Teams (v1.1+):** both are browser-based,
  so attribution comes from WebRTC track interception inside an
  embedded WebView rather than an accessibility tree. Same per-
  speaker timeline contract downstream, different capture mechanism
  upstream.
- **Webex (v1.1+):** native macOS client; the AXObserver approach
  should port directly pending an accessibility-tree survey.

The Rust crate boundaries (`heron-audio`, `heron-zoom`, `heron-speech`)
are designed for this kind of extension. v1 introduces the `SttBackend`
and `AxBackend` traits as precedent; the same pattern generalizes to a
per-meeting-app capture trait in v1.1, and v2 adds an `AudioCapture`
trait so each new OS or meeting app drops in via new implementations
rather than forks.

### v2 direction — agent-as-meeting-participant

A parallel v2 track extends heron from passive note-taker into a
meeting agent that can speak. v2 is a four-layer architecture —
driver / bridge / policy / realtime — spread across `heron-bot`,
`heron-bridge`, `heron-policy`, and `heron-realtime`. Recall.ai is
the first driver path, and `heron-bot` now includes the first
`RecallDriver` implementation. v1 ships first; v2 implementation
continues behind these layer boundaries.

## Status

v1 implementation is well underway. The
desktop shell, onboarding wizard, settings pane, menubar tray, review
window with TipTap edit + transcript playback, re-summarize with diff
modal, batch purge, native notifications, pre-flight checks, crash
recovery / salvage, diagnostics tab parser, calendar wiring, and
Keychain-backed API keys have all shipped. The `heron record`,
`heron summarize`, `heron status`, `heron salvage`, and `ax-dump`
paths are wired in. Mobile (iOS / Android), other meeting apps
(Meet / Teams / Webex), other desktop operating systems (Windows /
Linux), ambient session detection, and an MCP server remain deferred
to v1.1+.

The v2 stack now has more than trait sketches: `heron-bot` includes a
`RecallDriver`, `herond` serves the localhost desktop API, and
`heron-orchestrator` publishes FSM-driven meeting lifecycle events
through the canonical event bus. The Recall.ai spike harness in
`crates/heron-bot/examples/recall-spike.rs` validated the design
against a live Zoom meeting on 2026-04-26 (see
[`docs/archives/spike-findings.md`](docs/archives/spike-findings.md)). Remaining v2
work is the realtime speech path: speech-control, policy enforcement,
audio bridge, and backend integration.

## Repository layout

```text
.
├── apps/desktop/                # Tauri v2 desktop shell
│   ├── src/                     # React frontend (Tailwind 4 + Radix)
│   └── src-tauri/               # Rust backend
├── crates/
│   ├── heron-types/             # shared serde types, SessionClock, FSM
│   ├── heron-audio/             # process tap + ringbuffer + backpressure
│   ├── heron-speech/            # SttBackend trait + WhisperKit / sherpa bridges
│   ├── heron-zoom/              # AxBackend trait + AXObserver bridge + aligner
│   ├── heron-llm/               # Summarizer trait + meeting.hbs + cost calibration
│   ├── heron-vault/             # markdown writer + merge + EventKit bridge
│   ├── heron-cli/               # `heron` CLI (record / summarize / status / …)
│   ├── heron-doctor/            # log anomalies + runtime preflight checks
│   │
│   │   # event + desktop daemon substrate
│   ├── heron-event/             # canonical event envelope + broadcast bus
│   ├── heron-event-http/        # replay cache + SSE/web transport helpers
│   ├── heron-event-tauri/       # Tauri IPC event sink
│   ├── heron-session/           # desktop SessionOrchestrator contract
│   ├── heron-orchestrator/      # local orchestrator + vault read side + FSM events
│   ├── herond/                  # localhost HTTP/SSE desktop daemon
│   │
│   │   # v2 participant layers
│   ├── heron-bot/               # Layer 1: meeting-bot driver trait + RecallDriver
│   ├── heron-bridge/            # Layer 2: PCM jitter buffer + resample + mix
│   ├── heron-policy/            # Layer 3: speech-control contract + agent policy
│   └── heron-realtime/          # Layer 4: bidirectional realtime LLM session
├── swift/
│   ├── eventkit-helper/         # @_cdecl bridge — calendar (§5.4)
│   ├── whisperkit-helper/       # @_cdecl bridge — STT (§4)
│   └── zoomax-helper/           # @_cdecl bridge — AX observer (§9)
├── docs/                        # current architecture + OpenAPI specs + archives
├── fixtures/                    # ax / speech / zoom / manual-validation
└── scripts/                     # setup-dev.sh + reset-onboarding.sh + bench-wer.sh
```

Binaries:

- **`heron`** — main CLI. Subcommands: `record`, `summarize`,
  `status`, `verify-m4a`, `synthesize`, `salvage`, `ax-dump`.
- **`heron-doctor`** — offline diagnostics over `~/Library/Logs/heron/<date>.log`.
- **`validate-vault`** — walks an Obsidian vault and reports integrity issues.
- **`herond`** — localhost-only desktop daemon on `127.0.0.1:7384`.
  It exposes `/health`, `/v1/meetings*`, `/v1/calendar/upcoming`,
  `/v1/context`, and `/events` SSE per `docs/api-desktop-openapi.yaml`.
- **`heron-desktop`** — Tauri v2 shell (in `apps/desktop`).

## Quick start

```sh
# Install toolchain + system deps (macOS only).
./scripts/setup-dev.sh

# Build everything.
cargo build --workspace

# Run the test suite.
cargo test --workspace

# Smoke the CLI.
cargo run --bin heron -- status

# Run the localhost desktop daemon.
HERON_VAULT_ROOT=/path/to/vault cargo run --bin herond

# Generate a stub fixture for offline regression.
cargo run --bin heron -- synthesize /tmp/fixture-demo

# Run the desktop frontend during development.
cd apps/desktop
bun install
bun run tauri dev
```

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the polish + pr-workflow
conventions every change goes through.

## Documents

The current architecture reference is short by design. Older planning,
research, and spike documents are archived for context under
[`docs/archives/`](docs/archives/).

| Document | What it covers |
|---|---|
| [`docs/architecture.md`](docs/architecture.md) | Current codebase architecture: process topology, crate map, data flows, and known gaps. |
| [`docs/api-desktop-openapi.yaml`](docs/api-desktop-openapi.yaml) | OpenAPI spec for the localhost desktop daemon (`herond`). |
| [`docs/api-bot-openapi.yaml`](docs/api-bot-openapi.yaml) | OpenAPI spec for the bot driver layer. |
| [`docs/archives/`](docs/archives/) | Historical plans, research, spikes, manual test notes, and earlier architecture drafts. |

If you only read one document, read [`docs/architecture.md`](docs/architecture.md).

## How v1 differs from existing tools

- **Fireflies / Otter**: join the call as a visible bot. heron does not.
- **Granola**: invisible, but collapses remote audio to a single mixed
  track and can only cluster speakers by voice. heron uses the
  meeting app's own speaker signal (Zoom's accessibility tree in v1;
  WebRTC track IDs for Meet / Teams in v1.1+) to recover real display
  names without ML clustering in the happy path.
- **Char / oh-my-whisper**: closer to the right shape but don't solve
  per-speaker attribution for native meeting clients.

## Quality promise

In the modal AXObserver outcome, heron attributes ~70% of turns to a
real name with high confidence; the remaining ~30% are marked `them`
with a low-confidence visual indicator. This is strictly better than
Granola (0% attribution) and comparable-to-weaker than Fireflies on
a well-configured call. The pitch is **"Fireflies-quality
attribution on most turns, without a bot"**, not "Fireflies-quality
transcripts always."

## License

[GNU AGPL-3.0-or-later](./LICENSE). Each workspace crate inherits
this expression from `[workspace.package]` in `Cargo.toml`, and
`cargo-deny` is configured to allow it (`deny.toml`).

AGPL was chosen so anyone running heron as a network service — a
hosted note-taker, a hosted meeting agent — must publish their
modifications under the same terms. A permissive license would
let a SaaS competitor close-source a fork; AGPL prevents that
without restricting personal or in-company use.
