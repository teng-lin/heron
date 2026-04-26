# heron

A private, on-device AI meeting agent. Two modes:

- **Note-taker (v1, in flight).** Records, transcribes, and summarizes
  meetings without joining as a visible bot.
- **Participant (v2, trait surfaces in place).** Attends meetings on
  the user's behalf — listening, taking turns, and speaking. Notes
  become a side effect of the agent's memory rather than the primary
  output. See [`docs/architecture-agent-participant.md`](docs/architecture-agent-participant.md).

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
through an explicit policy contract (full contract in
[`docs/api-design-spec.md`](docs/api-design-spec.md)). v2 ships v1's
note-taker output as agent memory; the agent draws on prior meeting
transcripts as long-term context.

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
- **Tencent Meeting / Feishu (Lark) Meeting (v2.2+):** outside the
  Recall / Attendee / MeetingBaaS coverage set, so each gets its own
  native driver (TRTC SDK and Lark Open Platform respectively) gated
  on a concrete CN distribution thesis. Tradeoff and architectural
  fit in
  [`docs/build-vs-buy-decision.md`](docs/build-vs-buy-decision.md#regional-platforms-tencent-meeting-feishu--lark-meeting).

The Rust crate boundaries (`heron-audio`, `heron-zoom`, `heron-speech`)
are designed for this kind of extension. v1 introduces the `SttBackend`
and `AxBackend` traits as precedent; the same pattern generalizes to a
per-meeting-app capture trait in v1.1, and v2 adds an `AudioCapture`
trait so each new OS or meeting app drops in via new implementations
rather than forks.

### v2 direction — agent-as-meeting-participant

A parallel v2 track is in flight (proposal in
[`docs/architecture-agent-participant.md`](docs/architecture-agent-participant.md))
that extends heron from passive note-taker into a meeting agent that
can speak. v2 is a four-layer architecture — driver / bridge / policy
/ realtime — captured as trait surfaces in `heron-bot`,
`heron-bridge`, `heron-policy`, and `heron-realtime`. The full
contract is [`docs/api-design-spec.md`](docs/api-design-spec.md);
build-vs-buy ([`docs/build-vs-buy-decision.md`](docs/build-vs-buy-decision.md))
selected Recall.ai as Path A, and the live spike findings are in
[`docs/spike-findings.md`](docs/spike-findings.md). v1 ships first;
v2 implementations land behind these traits.

## Status

v1 implementation is well underway (currently at phase 77). The
desktop shell, onboarding wizard, settings pane, menubar tray, review
window with TipTap edit + transcript playback, re-summarize with diff
modal, batch purge, native notifications, pre-flight checks, crash
recovery / salvage, calendar wiring, and Keychain-backed API keys
have all shipped. The `heron summarize` subcommand and the `ax-dump`
diagnostic are wired in. Mobile (iOS / Android), other meeting apps
(Meet / Teams / Webex), other desktop operating systems (Windows /
Linux), ambient session detection, and an MCP server remain deferred
to v1.1+.

The v2 four-layer stack is currently trait surfaces only — the
Recall.ai spike harness in `crates/heron-bot/examples/recall-spike.rs`
validated the design against a live Zoom meeting on 2026-04-26
(see [`docs/spike-findings.md`](docs/spike-findings.md)); the
`RecallDriver: MeetingBotDriver` impl is the next gate.

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
│   ├── heron-doctor/            # `heron-doctor` log-anomaly CLI
│   │
│   │   # v2 trait surfaces (api-design-spec.md §1) — implementations deferred
│   ├── heron-bot/               # Layer 1: meeting-bot driver trait + Recall spike
│   ├── heron-bridge/            # Layer 2: PCM jitter buffer + resample + mix
│   ├── heron-policy/            # Layer 3: speech-control contract + agent policy
│   └── heron-realtime/          # Layer 4: bidirectional realtime LLM session
├── swift/
│   ├── eventkit-helper/         # @_cdecl bridge — calendar (§5.4)
│   ├── whisperkit-helper/       # @_cdecl bridge — STT (§4)
│   └── zoomax-helper/           # @_cdecl bridge — AX observer (§9)
├── docs/                        # plan + implementation + architecture + v2 specs
├── fixtures/                    # ax / speech / zoom / manual-validation
└── scripts/                     # setup-dev.sh + reset-onboarding.sh + bench-wer.sh
```

Binaries:

- **`heron`** — main CLI. Subcommands: `record`, `summarize`,
  `status`, `verify-m4a`, `synthesize`, `salvage`, `ax-dump`.
- **`heron-doctor`** — offline diagnostics over `~/Library/Logs/heron/<date>.log`.
- **`validate-vault`** — walks an Obsidian vault and reports integrity issues.
- **`heron-desktop`** — Tauri v2 shell (in `apps/desktop`).

## Quick start

```sh
# Install toolchain + system deps (macOS only).
./scripts/setup-dev.sh

# Build everything.
cargo build --workspace

# Run the test suite.
cargo test --workspace

# Smoke the CLI scaffold.
cargo run --bin heron -- status

# Generate a stub fixture for offline regression.
cargo run --bin heron -- synthesize /tmp/fixture-demo
```

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the polish + pr-workflow
conventions every change goes through.

## Documents

The plan and implementation docs are the day-to-day reference. The
v2 specs run alongside them as a parallel track.

### v1 — product, plan, execution

| Document | What it covers |
|---|---|
| [`docs/plan.md`](docs/plan.md) | The v1 product/architecture plan. Locked decisions, output contract, build plan by week, risks, deferred-to-v2 list. Authoritative scope. |
| [`docs/implementation.md`](docs/implementation.md) | Execution layer below the plan. Prerequisites, week-by-week tasks, acceptance criteria, code stubs, test rigs. |
| [`docs/architecture.md`](docs/architecture.md) | Long-range architectural sketch — full crate decomposition, agent surface, consumer model. v1 implements a focused subset. |
| [`docs/stack.md`](docs/stack.md) | Greenfield tooling/framework choices. Long-range reference; `plan.md` is what v1 uses. |
| [`docs/diarization-research.md`](docs/diarization-research.md) | Five approaches to invisible meeting capture + speaker attribution. Approach 2 (Core Audio process tap + AXObserver on Zoom) is the v1 flagship bet. |
| [`docs/merge-model.md`](docs/merge-model.md) | The re-summarize merge contract — which fields are user-owned, which are LLM-owned, and how diffs reconcile. |
| [`docs/security.md`](docs/security.md) | Threat model + cached-audio handling (purge-on-success, 0600 perms, Keychain ACL scoping). |
| [`docs/observability.md`](docs/observability.md) | Log schema, metric names, and what `heron-doctor` looks for. |
| [`docs/onboarding-tests.md`](docs/onboarding-tests.md) | Acceptance tests for the first-run wizard. |
| [`docs/manual-test-matrix.md`](docs/manual-test-matrix.md) | Manual-validation matrix run before each release. |
| [`docs/swift-bridge-pattern.md`](docs/swift-bridge-pattern.md) | Canonical `@_cdecl` shape every Swift helper follows. |

### v2 — agent-as-meeting-participant

| Document | What it covers |
|---|---|
| [`docs/architecture-agent-participant.md`](docs/architecture-agent-participant.md) | The pivot proposal: what changes vs. v1, what stays, the four-layer architecture. |
| [`docs/api-design-spec.md`](docs/api-design-spec.md) | The trait contract for the four layers + the 14 invariants. Authoritative for v2 surfaces. |
| [`docs/api-design-research.md`](docs/api-design-research.md) | Background research that shaped the layering — vendor capability matrix, where each draws the line. |
| [`docs/agent-participation-research.md`](docs/agent-participation-research.md) | Research into proxy-mode UX and disclosure handling. |
| [`docs/build-vs-buy-decision.md`](docs/build-vs-buy-decision.md) | Why Path A (Recall.ai) first, Path C second, with reversibility triggers. |
| [`docs/backend-evaluations.md`](docs/backend-evaluations.md) | Realtime-backend evaluation (OpenAI Realtime / Gemini Live / LiveKit / Pipecat). |
| [`docs/spike-findings.md`](docs/spike-findings.md) | Live Recall.ai spike results + per-invariant verdicts + recommendations for the `RecallDriver` impl. |
| [`docs/api-bot-openapi.yaml`](docs/api-bot-openapi.yaml) | OpenAPI spec for the bot driver layer. |
| [`docs/api-desktop-openapi.yaml`](docs/api-desktop-openapi.yaml) | OpenAPI spec for the `herond` desktop daemon. |

If you only read one document, read [`docs/plan.md`](docs/plan.md).
For v2 context, [`docs/api-design-spec.md`](docs/api-design-spec.md)
is the load-bearing one.

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
