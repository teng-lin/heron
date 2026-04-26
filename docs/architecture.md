# Heron Architecture

This document describes the current codebase, not the archived build
plans. Historical planning, research, and spike notes live in
[`docs/archives/`](archives/). The active wire contracts that remain at
the root of `docs/` are:

- [`api-desktop-openapi.yaml`](api-desktop-openapi.yaml) for the local
  desktop daemon.
- [`api-bot-openapi.yaml`](api-bot-openapi.yaml) for the meeting-bot
  driver boundary.

## Product Shape

Heron has two related product paths.

The v1 path is a private meeting note-taker for macOS and Zoom. It
captures the native meeting app without joining as a visible bot,
transcribes locally where possible, uses meeting-app signals for
speaker attribution, summarizes through the selected LLM backend, and
writes Markdown notes plus audio sidecars into a user-owned vault.

The v2 path turns Heron into an agent participant. It joins a meeting
through a bot driver, receives realtime meeting state, gates speech
through policy, and sends speech/audio through a realtime backend. The
v2 code exists as explicit layers today, with Recall.ai as the first
concrete bot driver. The realtime speech path is still incomplete.

## Process Topology

```text
                         +----------------------+
                         |      User / UI       |
                         +----------+-----------+
                                    |
                                    v
                 +------------------+-------------------+
                 | apps/desktop React renderer          |
                 | onboarding, recording, review,       |
                 | salvage, settings                    |
                 +------------------+-------------------+
                                    |
                         Tauri invoke + events
                                    |
                                    v
                 +------------------+-------------------+
                 | apps/desktop/src-tauri               |
                 | settings, keychain, tray, notes,     |
                 | probes, daemon startup, IPC events   |
                 +------------+----------------+--------+
                              |                |
                              | shared         | Tauri IPC
                              | orchestrator   v
                              |       +--------+---------+
                              |       | WebView listeners|
                              |       +------------------+
                              v
        +---------------------+----------------------+
        | crates/heron-orchestrator                  |
        | SessionOrchestrator impl, event bus,       |
        | replay cache, vault reads, calendar reads, |
        | manual capture FSM events                  |
        +----------+----------------------+----------+
                   |                      |
                   | HTTP projection      | v1 pipeline helpers
                   v                      v
        +----------+-----------+   +------+----------------------+
        | crates/herond        |   | capture / speech / LLM /    |
        | localhost HTTP + SSE |   | vault crates                |
        +----------+-----------+   +-----------------------------+
                   |
                   v
        +----------+-----------+
        | CLI / local clients  |
        | bearer auth + SSE    |
        +----------------------+
```

The desktop shell and `herond` share the same domain model through
`heron-session`. `herond` is intentionally localhost-only. Browser-origin
requests are rejected, `/health` is unauthenticated, and other endpoints
use a bearer token loaded from `~/.heron/cli-token`.

The CLI is a separate entry point. `crates/heron-cli` wires direct v1
recording and summarization paths and exposes operational subcommands
such as `status`, `salvage`, `synthesize`, `verify-m4a`, and `ax-dump`.

## Crate Map

### Shared Domain

`heron-types` contains shared IDs, recording FSM types, recovery state,
session clocks, channels, turns, and typed prefixes such as `mtg_*` and
`evt_*`.

`heron-session` defines the desktop daemon's domain model:
`Meeting`, `Transcript`, `Summary`, `CalendarEvent`, health types,
event payloads, and the `SessionOrchestrator` trait. The OpenAPI file is
a projection of this contract.

### Eventing And Transports

`heron-event` owns the canonical event envelope, `EventBus`, `EventSink`,
and `ReplayCache` traits. Domain payloads do not live here; the crate is
transport-agnostic.

`heron-event-http` provides HTTP/SSE support pieces: replay cache,
topic filtering, replay-window headers, and SSE formatting helpers.

`heron-event-tauri` projects event envelopes into Tauri IPC events for
the desktop WebView.

### Desktop API

`herond` is the Axum daemon. It serves:

- `GET /v1/health`
- `GET /v1/events` as Server-Sent Events with heartbeat and resume
- `/v1/meetings*`
- `/v1/calendar/upcoming`
- `/v1/context`

Handlers are thin projections over `SessionOrchestrator`.

`heron-orchestrator` is the current local implementation. It has a live
event bus, in-memory replay cache, vault-backed read paths, EventKit
calendar reads, and manual capture lifecycle events. Capture lifecycle
methods currently drive a recording FSM and publish meeting events; they
do not yet run the full live audio -> STT -> LLM pipeline behind the
daemon.

### v1 Capture And Notes

`heron-audio` captures audio on macOS. It owns the Core Audio process tap,
mic capture, WebRTC APM/AEC hooks, WAV writing, disk ringbuffer,
backpressure tracking, and recovery scanning.

`heron-speech` owns speech-to-text backends. WhisperKit is the Apple
primary path through a Swift bridge, and Sherpa/ONNX is the bundled
fallback path. The common interface is `SttBackend`.

`heron-zoom` owns Zoom-specific speaker attribution. It uses the macOS
accessibility tree through a Swift helper and aligns speaker events with
transcript turns.

`heron-llm` owns summary generation. It contains transcript/content
conversion, provider selection, Anthropic, Claude Code, Codex, cost
tracking, and the meeting summary template.

`heron-vault` is the storage boundary. It writes Obsidian-style Markdown
notes, preserves user edits during re-summarization, reads EventKit
calendar data through a Swift bridge, encodes and verifies audio
sidecars, purges cached audio after verification, and validates vault
integrity.

### Desktop Shell

`apps/desktop/src` is the React renderer. It contains the onboarding,
home, recording, review, salvage, and settings pages plus shared stores
and UI components.

`apps/desktop/src-tauri` is the Rust side of the Tauri app. It provides
commands for settings, keychain access, onboarding probes, diagnostics,
note reads/writes, re-summarization, salvage, disk checks, tray
navigation, asset resolution, the in-process event bus, and daemon
startup.

### Diagnostics And Operations

`heron-doctor` has two roles:

- offline log parsing and anomaly detection for session summaries
- runtime preflight checks for ONNX, Zoom process availability,
  Keychain ACLs, and network reachability

`validate-vault` is a binary from `heron-vault` that walks a vault and
reports note integrity issues.

### v2 Participant Layers

The participant stack is split into four crates:

- `heron-bot`: meeting-bot driver boundary and first Recall.ai driver
- `heron-bridge`: PCM, jitter buffer, resampling, mixing, and bridge
  health primitives
- `heron-policy`: speech-control contract, filtering, queues,
  controller state, validation, and escalation handling
- `heron-realtime`: realtime LLM session boundary with mock and fallback
  implementations

The intended flow is:

```text
Meeting platform
  -> heron-bot
  -> heron-bridge
  -> heron-realtime
  -> heron-policy
  -> heron-bot
  -> Meeting platform
```

The Recall driver can create and track bot lifecycle state against the
Recall API. Real TTS output, webhook ingestion, and the end-to-end
speech loop still need to be completed.

## v1 Data Flow

```text
Zoom on macOS
  -> heron-audio
       tap.wav, mic.wav, mic_clean.wav, capture events
  -> heron-speech
       partial JSONL and final turns
  -> heron-zoom
       speaker attribution and turn alignment
  -> heron-llm
       Markdown summary and structured frontmatter
  -> heron-vault
       note, transcript sidecar, audio sidecar, merge/recovery metadata
```

Crash recovery is file-based. Session state is written under the cache
root, `heron salvage` discovers unfinished sessions, and the desktop
salvage UI can finalize or purge candidates.

## Desktop API Flow

```text
Client
  -> herond HTTP route
  -> SessionOrchestrator method
  -> LocalSessionOrchestrator
  -> vault/calendar/FSM/event bus
  -> HTTP JSON response or SSE event
```

For live event consumption:

```text
Publisher
  -> heron_event::EventBus
  -> InMemoryReplayCache
  -> herond /v1/events
  -> SSE client with Last-Event-ID resume
```

The Tauri path uses the same bus concept but projects events into WebView
IPC rather than SSE. Replay is available on the HTTP path; Tauri IPC
listeners should treat missed events as lost.

## Swift Bridge Pattern

Three macOS capabilities are implemented through Swift packages:

- `swift/whisperkit-helper` for WhisperKit transcription
- `swift/eventkit-helper` for EventKit calendar access
- `swift/zoomax-helper` for Zoom accessibility observation

Rust crates link these helpers through `swift-rs` and `build.rs`.
Swift-facing code should stay isolated behind the owning crate's Rust
API so non-Apple builds can compile stubs or return platform-specific
unavailable errors.

## Storage Model

The canonical user store is a Markdown vault on disk, not a service
database. Notes use YAML frontmatter and Markdown bodies. Audio sidecars
are encoded to m4a and verified before cached WAVs are purged. User edits
are preserved by merge-on-write rather than overwriting the note with a
fresh LLM response.

The daemon read side can derive meeting resources from vault files.
In-memory state is used for active captures, event replay, and daemon
runtime coordination.

## Current Boundaries And Gaps

The current codebase has a real desktop shell, local daemon, event bus,
vault read side, v1 CLI recording path, v1 summary path, and the first v2
bot driver.

The main unfinished areas are:

- full daemon-backed live capture through `LocalSessionOrchestrator`
- end-to-end v2 speech loop across bridge, realtime, policy, and bot
- production TTS and webhook handling for Recall
- cross-platform capture backends beyond macOS
- meeting app support beyond Zoom
- long-term semantic indexing across meetings
