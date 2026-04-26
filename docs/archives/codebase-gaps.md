# Codebase gap audit

_Snapshot: 2026-04-26, branch `main` at `8457d8d` (with in-flight working-tree edits for the doctor/onboarding wiring)._

A survey of the heron workspace looking for gaps that block an **alpha**
release of v2. Scope: Rust crates under `crates/`, the Tauri desktop app under
`apps/desktop`, and the Swift helpers under `swift/`. The goal is a punch list
to prioritize from, not an exhaustive TODO sweep.

## Summary

v2 is no longer trait-only. The daemon has real routes, the desktop app starts
an in-process `herond`, the event bus fans out over SSE + Tauri IPC, and every
v2 layer now has at least one concrete implementation:

- `heron_orchestrator::LocalSessionOrchestrator`
- `heron_orchestrator::live_session::LiveSessionOwner`
- `heron_bot::RecallDriver`
- `heron_realtime::OpenAiRealtime` (and `MockRealtimeBackend`)
- `heron_policy::DefaultSpeechController`
- `heron_bridge::NaiveBridge`
- `heron_doctor::Doctor::run_runtime_checks`

The single remaining shipping blocker is **composition**: `LiveSessionOwner`
exists but is only instantiated in tests. `LocalSessionOrchestrator::start_capture`
walks the meeting FSM, publishes lifecycle events, and spawns the v1
audio→STT→LLM→vault pipeline — but never opens a v2 realtime/bot/policy session.
Two adjacent gaps (no key-provisioning path, no real model-download UX) prevent
even a manual workaround.

**Alpha blockers (must land):** items 1, 2, 3, 4 below.
**GA blockers (can defer past alpha):** items 5, 6, 7.
**Polish:** items 8, 9.

## Alpha blockers — v2 cannot run a real session

### 1. `LiveSessionOwner` is never wired into `start_capture`

`crates/heron-orchestrator/src/live_session.rs:51` defines an owner that
constructs the bot, opens realtime, installs the policy controller, and tears
the stack down in dependency order. Every instantiation in the repo is in tests
(`live_session.rs:575, 610, 627, 649, 671`).

`LocalSessionOrchestrator::start_capture` (`crates/heron-orchestrator/src/lib.rs`,
roughly lines 926–1075) instead spawns the v1 `CliSessionOrchestrator` pipeline
(`lib.rs:1005–1026`). The four-layer v2 stack
(`RecallDriver` + `OpenAiRealtime` + `NaiveBridge` + `DefaultSpeechController`)
never composes on the daemon hot path. Without this wiring, the daemon cannot
join a meeting with realtime bot interaction; it can only run vault-backed
manual captures.

### 2. No `OPENAI_API_KEY` provisioning path to the daemon

`crates/heron-realtime/src/openai.rs:40` reads `OPENAI_API_KEY` from
`std::env`. There is no Tauri command, Settings UI field, or keychain entry
that propagates a user-supplied key from the desktop app into the in-process
`herond`. As soon as item 1 lands, every alpha session will fail with
`BadConfig("OPENAI_API_KEY is required")` unless the user manually exports the
variable before launching the app — not an acceptable alpha UX.

Required: settings field + secure store + injection into the daemon child env
(or, if `herond` ends up reading a runtime config file, persist it there).

### 3. Onboarding model-download step is a stub

`apps/desktop/src/pages/Onboarding.tsx:443` carries
`// TODO(phase 72+): wire heron_download_model and replace this`. The wizard's
final step does not actually deliver a model. Without a working download (or a
clearly-documented bundled-model alpha posture), users finish onboarding
without the local STT artifact and the first capture fails opaquely.

### 4. Pre-meeting context wiring still needs orchestrator hand-off

(_Was previously listed as "`attach_context` returns 501". That part is closed:
`crates/heron-orchestrator/src/lib.rs:1308` now persists context into an
in-memory map keyed by `calendar_event_id`, and `start_capture` consumes the
staged entry at `lib.rs:1047–1062`._)

What remains is the consumer side once item 1 lands: the staged context must
flow into `LiveSessionOwner` (system prompt / tool wiring / persona) so the
agent actually behaves differently when a calendar entry has been
context-loaded. Today the `Vec<MeetingContext>` is read out and dropped on the
floor in the v1 path. Tracking it as an alpha blocker because shipping #1
without #4 means context never reaches the model.

## GA blockers — defer past alpha

### 5. `heron-cli` does not delegate v2 capture to `herond`

`crates/heron-cli/src/main.rs` has no `localhost`/HTTP delegation; `cmd_record`
(`:324`) calls `heron_cli::session::Orchestrator::new()` directly and runs the
v1 pipeline in-process. The v2 escape hatch — bearer-auth + `POST /v1/meetings`,
`POST /v1/meetings/{id}/end`, `/v1/events` — is still missing. This leaves two
session-control surfaces, but the CLI is not on the alpha critical path and can
follow desktop.

### 6. Production-grade audio bridge

`heron_bridge::NaiveBridge` is documented as test-grade. Real
AEC/playback/jitter handling under live device + network conditions is GA
work. Alpha can ship on `NaiveBridge` against a small set of canary meetings.

### 7. Daemon state is in-memory only

`crates/heron-orchestrator/src/lib.rs:64–66` notes the cache,
active-meeting bookkeeping, and daemon-ID-to-note-path index are all
in-memory. Pre-meeting contexts staged via `attach_context` live in the same
process (`lib.rs:210`). A daemon restart loses in-flight captures and any
staged context. Acceptable for alpha if the release notes call it out;
needs a durable store before GA.

## Polish

### 8. v2 integration test coverage is still thin

The v2 crates have unit tests around individual invariants, and the bus
fan-out path has integration coverage. What's still missing is production-like
cross-crate lifecycle tests: daemon `POST /meetings` starts a real session
owner and publishes the expected events; `end_meeting` drains tasks and
persists transcript/summary/audio refs; policy-denied speech never reaches the
backend; bridge health degradation propagates to status; Recall shutdown
leaves no active vendor bot. Adding these alongside item 1's implementation is
the cheapest time to do it.

### 9. No crash/error reporting

`herond` has structured `tracing` logging but no Sentry/error-reporting sink.
`tauri.conf.json` has no updater plugin configured. Alpha failures will be
silent unless users dig into log files. Minimum viable: surface a log tail in
the desktop app and/or wire a lightweight error-reporting plugin before
external alpha testers come on.

## In flight (not blockers — already on the working tree)

### Doctor runtime checks surfaced in onboarding

`heron-doctor` has runtime preflight checks for ONNX/model artifacts, Zoom
process availability, keychain ACL on macOS, and network reachability via
`Doctor::run_runtime_checks`. Active uncommitted work adds the missing
glue:

- `apps/desktop/src-tauri/src/runtime_checks.rs` (new)
- `apps/desktop/src/components/RuntimeChecksPanel.tsx` (new)
- edits to `apps/desktop/src-tauri/src/lib.rs`, `apps/desktop/src/lib/invoke.ts`,
  `apps/desktop/src/pages/Onboarding.tsx`, `apps/desktop/src/store/onboarding.ts`

The onboarding store now lists six steps (microphone, audio tap, accessibility,
calendar, model download, **daemon**). Once this PR lands the wizard surfaces
the consolidated runtime-preflight answer.

## Resolved or downgraded since the previous audit

### `attach_context` is no longer 501

`crates/heron-orchestrator/src/lib.rs:1308` persists context into an in-memory
map; `start_capture` consumes staged entries (`lib.rs:1047–1062`). The
remaining work is consumer-side and is tracked as item 4 above.

### React onboarding wizard now has a daemon step

`apps/desktop/src/store/onboarding.ts:44` lists six steps including `daemon`.
The visible wiring still depends on the in-flight runtime-checks PR but the
store-level gap is closed.

### `herond` is no longer a 501 appliance

`crates/herond/src/routes/meetings.rs` forwards meetings, transcripts,
summaries, audio, calendar, and context routes to `SessionOrchestrator`. Some
methods can still return `NotYetImplemented` depending on orchestrator
capability, but the router itself is not a static unimplemented surface.

### `StubOrchestrator` is no longer the only orchestrator

`heron_orchestrator::LocalSessionOrchestrator` is wired into both the
standalone `herond` binary and the desktop in-process daemon path.
`StubOrchestrator` remains useful for tests.

### `LocalSessionOrchestrator` owns a native vault-backed capture pipeline

For vault-backed daemon sessions, `start_capture` spawns the existing
`heron-cli` audio→STT→LLM→vault pipeline with task ownership and an explicit
stop signal. `end_meeting` signals the pipeline to stop, publishes
`meeting.ended` immediately, and lets a background waiter publish
`meeting.completed` after finalization. The daemon-issued `MeetingId` remains
readable for the life of the process after the vault note is written.
Vault-less test construction still uses the synthetic FSM path.

### Daemon read-side no longer depends only on external vault artifacts

Because vault-backed daemon capture writes finalized notes through the v1
pipeline, the read endpoints serve artifacts produced by the daemon itself.
Cross-restart daemon-ID continuity is still in-memory only; after restart,
path-derived vault IDs are the source of truth.

### `MeetingBotDriver` has a concrete Recall implementation

`heron_bot::RecallDriver` implements `MeetingBotDriver` with wiremock-driven
coverage. Remaining work is orchestration and live-vendor hardening.

### `SpeechController` and policy enforcement exist

`heron_policy::DefaultSpeechController` calls `filter::evaluate()` on every
`speak()`. Production session wiring is pending under item 1.

### `OpenAiRealtime` is the first production realtime backend

`heron_realtime::OpenAiRealtime` opens a real OpenAI Realtime WebSocket
session, translates session configuration into `session.update`, and maps
response, transcript, speech, tool-call, and error events back into the
crate's backend-neutral event model. Remaining work is orchestrator
composition (item 1) and key provisioning (item 2).

### `AudioBridge` has a naive implementation

`heron_bridge::NaiveBridge` is appropriate for tests/prototyping. Production
quality is GA scope (item 6).

### EventBus multi-subscriber fan-out is resolved

The bus reaches SSE, Tauri IPC, and replay cache consumers.
`LocalSessionOrchestrator` publishes lifecycle events; the desktop event-bus
integration tests pin multi-subscriber behavior.

### WhisperKit Swift bridge has per-call timeouts

`swift/whisperkit-helper/Sources/WhisperKitHelper/WhisperKitHelper.swift` runs
every async→sync hop through `runWithTimeout` bounded by `WK_INIT_TIMEOUT`
(2m), `WK_FETCH_TIMEOUT` (30m), and `WK_TRANSCRIBE_TIMEOUT` (30m), and surfaces
`WK_TIMEOUT` (-4) on expiry. Resolved in PR #124 (commit `30321bc`).

### EventKit Swift bridge `ek_request_access` has a timeout

`swift/eventkit-helper/Sources/EventKitHelper/EventKitHelper.swift:36` is
bounded by `EK_REQUEST_TIMEOUT` (60s) and surfaces a recoverable
`CalendarError::Timeout`, mirroring the WhisperKit pattern from PR #124.

## README claims vs. reality

- **"v2 four-layer stack is currently trait surfaces only."** Stale. Every
  layer has a concrete impl and `LiveSessionOwner` composes them, but the
  owner is not yet on the daemon hot path (item 1).
- **"The desktop shell, onboarding wizard, settings pane, menubar tray have
  all shipped."** Partial. The wizard exists and the daemon-step + runtime
  checks are in flight; the Settings pane still lacks an OpenAI key field
  (item 2) and the model-download step is a stub (item 3).
- **"mobile, other meeting apps, other desktop OSes remain deferred to v1.1+."**
  Still accurate.

## Punch list — priority order

| # | Gap | File:Line | Severity | Notes |
| -- | --- | --- | --- | --- |
| 1 | `LiveSessionOwner` never wired into `start_capture` | `crates/heron-orchestrator/src/lib.rs` (`start_capture`) | ALPHA | Compose `RecallDriver` + `OpenAiRealtime` + `NaiveBridge` + `DefaultSpeechController` on the daemon hot path |
| 2 | `OPENAI_API_KEY` has no UI/keychain → daemon path | `crates/heron-realtime/src/openai.rs:40` | ALPHA | Settings field + secure store + injection into daemon env/config |
| 3 | Onboarding model-download step is a stub | `apps/desktop/src/pages/Onboarding.tsx:443` | ALPHA | Wire `heron_download_model` (or document bundled-model posture) |
| 4 | Pre-meeting context not consumed by `LiveSessionOwner` | `crates/heron-orchestrator/src/lib.rs:1047–1062` | ALPHA | Persist+apply landed; consumer hand-off pending with item 1 |
| 5 | `heron-cli` does not delegate v2 capture to `herond` | `crates/heron-cli/src/main.rs:324` | GA | Bearer auth + localhost API |
| 6 | Production-grade audio bridge | `crates/heron-bridge/src/naive.rs` | GA | `WebRtcAecBridge` or equivalent |
| 7 | Daemon state in-memory only | `crates/heron-orchestrator/src/lib.rs:64–66` | GA | Acceptable for alpha if release notes flag it |
| 8 | Cross-crate v2 integration coverage | v2 crates | POLISH | Add lifecycle suites alongside item 1 |
| 9 | No crash/error reporting | `apps/desktop/src-tauri/tauri.conf.json` + `crates/herond` | POLISH | Wire log tail / lightweight reporter before external alpha |
| — | Doctor runtime checks in onboarding | (in flight) | — | Working-tree edits already underway |
| — | `attach_context` returns 501 | `crates/heron-orchestrator/src/lib.rs:1308` | RESOLVED | Persists into in-memory map; consumer hand-off tracked as item 4 |
| — | React onboarding wizard 5 steps | `apps/desktop/src/store/onboarding.ts:44` | RESOLVED | Six steps including `daemon` |
| — | Production realtime backend | `crates/heron-realtime/src/openai.rs` | RESOLVED | `OpenAiRealtime` ships |
| — | Bot/bridge/policy composition owner | `crates/heron-orchestrator/src/live_session.rs` | RESOLVED | Owner exists; daemon wiring tracked as item 1 |
| — | Orchestrator vault-backed capture | `crates/heron-orchestrator/src/lib.rs` | RESOLVED | v1 pipeline delegated |
| — | WhisperKit semaphore timeout | `swift/whisperkit-helper/.../WhisperKitHelper.swift:78` | RESOLVED | PR #124 |
| — | EventKit `ek_request_access` had no timeout | `swift/eventkit-helper/.../EventKitHelper.swift:36` | RESOLVED | `EK_REQUEST_TIMEOUT` 60s |
