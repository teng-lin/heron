# Codebase gap audit

_Snapshot: 2026-04-26, branch `main` at `c265e12`._

A survey of the heron workspace looking for "big gaps" — places where the
codebase has obvious holes that would block shipping or that suggest
incomplete work. Scope: Rust crates under `crates/`, the Tauri desktop
app under `apps/desktop`, and the Swift helpers under `swift/`.

The goal is a punch list to prioritize from, not an exhaustive TODO sweep.

## Summary

v1 is still the only real note-taking pipeline. v2 is no longer just trait
surfaces: the daemon has real routes, the desktop app starts an in-process
`herond`, the event bus fans out over SSE + Tauri IPC, and the first concrete
v2 pieces now exist:

- `heron_orchestrator::LocalSessionOrchestrator`
- `heron_bot::RecallDriver`
- `heron_realtime::OpenAiRealtime`
- `heron_policy::DefaultSpeechController`
- `heron_realtime::MockRealtimeBackend`
- `heron_bridge::NaiveBridge`
- `heron_orchestrator::live_session::LiveSessionOwner`
- `heron_doctor::Doctor::run_runtime_checks`

The remaining blocker is not "make traits compile"; it is wiring a real
capture/realtime session end-to-end. `LiveSessionOwner` now composes the v2
bot/bridge/realtime/policy layers behind one lifetime owner, but
`LocalSessionOrchestrator` still only walks the meeting FSM and publishes
lifecycle events; it does not yet connect live audio, STT, LLM summarization,
bot playback, or vault writes into the daemon capture path.

**Shipping blocker:** items 1 and 4 below must land before v2 can alpha-test.
Items 5-8 block GA.

## Blockers — v2 cannot run a real session

### 1. `LocalSessionOrchestrator` has lifecycle, not a capture pipeline

`crates/heron-orchestrator/src/lib.rs:563` implements
`SessionOrchestrator::start_capture` by synchronously walking the FSM through
`Detected -> Armed -> Recording` and publishing lifecycle events. It does not
start Core Audio, launch a bot, bind a bridge, open realtime, or spawn STT/LLM
tasks.

`crates/heron-orchestrator/src/lib.rs:637` implements `end_meeting` by walking
the FSM through terminal states and publishing `meeting.ended` /
`meeting.completed`. The implementation is honest in comments: with no real
STT / LLM wired through this orchestrator, transcript and summary completion
are synthetic.

What is missing:

- Core Audio mic/process-tap startup from the daemon path.
- STT task ownership and transcript persistence.
- LLM summary generation and vault note write/finalization.
- Background task lifecycle, cancellation, and crash recovery.
- Idempotent `end_meeting` against finalized meetings once vault writes exist.

### 2. Production realtime backend exists; orchestration is still pending

`crates/heron-realtime/src/openai.rs` implements `OpenAiRealtime`, a production
`RealtimeBackend` that opens an OpenAI Realtime WebSocket session, sends
`session.update`, creates/cancels responses, forwards tool results, and maps
OpenAI server events into Heron's `RealtimeEvent` stream.

This closes the standalone backend gap. The remaining blocker is daemon
integration: `LocalSessionOrchestrator::start_capture` still needs to
instantiate the live-session owner with `OpenAiRealtime`, bind real meeting
audio through `heron-bridge`, and connect teardown to the capture lifecycle.

### 3. v2 bot + bridge + policy composition owner is in place

The concrete layer pieces exist:

- `RecallDriver` implements `MeetingBotDriver`.
- `NaiveBridge` implements `AudioBridge`.
- `DefaultSpeechController` implements `SpeechController` and invokes
  `filter::evaluate()` before every `speak()`.
- `LiveSessionOwner` creates the bot, opens realtime, installs the policy
  controller, retains the bridge for audio adapters, and tears the stack down
  in dependency order.

This closes the old "no production session owner" gap. The remaining work is
now narrower: wire this owner into `LocalSessionOrchestrator::start_capture`
once a production realtime backend and meeting-audio adapters exist.
`NaiveBridge` is still explicitly test-grade, so a production bridge still
needs real AEC/playback behavior (`WebRtcAecBridge` or equivalent), jitter
handling under real network/device conditions, and integration tests against
bot playback.

### 4. Pre-meeting context storage is still 501

`crates/heron-orchestrator/src/lib.rs:837` returns
`SessionError::NotYetImplemented` from `attach_context`. Calendar reads are
available through `list_upcoming_calendar`, but the daemon still cannot persist
or apply pre-meeting context to a future capture.

This blocks the spec path where calendar/persona/context is baked into the
session before the agent joins.

## Major — blockers for GA

### 5. Onboarding has backend support, but the React wizard is still five steps

The desktop backend now starts an in-process `herond` during setup and exposes
daemon health commands:

- `daemon::install` is called from the Tauri setup hook.
- `heron_test_daemon` and `heron_daemon_status` are registered Tauri commands.

The React onboarding store still lists only five steps:

- microphone
- audio tap
- accessibility
- calendar
- model download

So users can still finish onboarding without seeing the daemon liveness check
or the richer `heron-doctor` runtime preflight results. The backend gap is
mostly closed; the user-visible wizard wiring remains.

### 6. `heron-doctor` runtime checks are not surfaced in onboarding

`heron-doctor` now has runtime preflight checks for ONNX/model artifacts, Zoom
process availability, keychain ACL on macOS, and network reachability. The
public facade is `Doctor::run_runtime_checks`.

The remaining gap is integration: neither the React onboarding flow nor a Tauri
command currently surfaces the full runtime-check set to the user. The wizard
still runs individual probes, which misses the consolidated "is this machine
ready to record?" answer the doctor now provides.

### 7. `heron-cli` does not delegate v2 capture to `herond`

`crates/heron-cli/src/main.rs` has real v1-style functionality:

- `heron record` runs the local `heron_cli::session::Orchestrator`.
- `heron summarize` re-summarizes a vault note.
- `heron status`, `salvage`, `synthesize`, and `ax-dump` are implemented.

But the v2 escape hatch is still missing: CLI capture/status commands do not
authenticate to localhost `herond` and call `POST /v1/meetings`,
`POST /v1/meetings/{id}/end`, or `/v1/events`. This leaves two session-control
surfaces instead of one.

### 8. Read-side daemon behavior depends on an existing vault snapshot

`LocalSessionOrchestrator` can list/get/read transcript/read summary/read audio
from an existing vault root, and `herond` projects those methods over HTTP.
But because the daemon capture path does not write finalized notes yet, the
read endpoints only become useful for sessions produced elsewhere.

This is less severe than the old "mostly 501" gap, but it still matters for
GA: the API surface looks complete, while the daemon cannot yet create the
durable artifacts those read endpoints are meant to serve.

## Minor — polish and post-v1

### 9. WhisperKit Swift bridge has no timeout

`swift/whisperkit-helper/Sources/WhisperKitHelper/WhisperKitHelper.swift:78`
uses a semaphore bridge to async WhisperKit. There is no `DispatchTime`
deadline on the semaphore wait, so a hung model load can block the calling
thread forever.

### 10. v2 integration test coverage is still thin

The v2 crates have many unit tests around individual invariants, and the bus
fan-out path now has integration coverage. The missing coverage is still the
hard part: production-like cross-crate tests for bot + bridge + realtime +
policy + orchestrator lifecycle.

Useful test seams to add with the remaining implementation:

- daemon `POST /meetings` starts a real session owner and publishes expected
  events;
- `end_meeting` drains tasks and persists transcript/summary/audio references;
- policy-denied speech never reaches the backend in an orchestrated session;
- bridge health degradation propagates to daemon/desktop status;
- Recall shutdown leaves no active vendor bot on graceful exit.

## Resolved or downgraded from the previous audit

### `herond` is no longer a 501 appliance

`crates/herond/src/routes/meetings.rs` now forwards meetings, transcripts,
summaries, audio, calendar, and context routes to `SessionOrchestrator`.
Some methods can still return `NotYetImplemented` depending on orchestrator
capability, but the router itself is no longer a static unimplemented surface.

### `StubOrchestrator` is no longer the only orchestrator

`heron_orchestrator::LocalSessionOrchestrator` exists and is wired into both
the standalone `herond` binary and the desktop in-process daemon path.
`StubOrchestrator` remains useful for tests.

### `MeetingBotDriver` has a concrete Recall implementation

`heron_bot::RecallDriver` implements `MeetingBotDriver` and has wiremock-driven
coverage. Remaining work is orchestration and live-vendor hardening, not the
absence of an implementation.

### `SpeechController` and policy enforcement exist

`heron_policy::DefaultSpeechController` implements `SpeechController` and calls
`filter::evaluate()` on every `speak()` call. The old "policy filter is never
invoked" gap is resolved at the controller layer; production session wiring is
still pending.

### `OpenAiRealtime` is the first production realtime backend

`heron_realtime::OpenAiRealtime` opens a real OpenAI Realtime WebSocket session
from `OPENAI_API_KEY`, translates session configuration into `session.update`,
and maps response, transcript, speech, tool-call, and error events back into
the crate's backend-neutral event model. Remaining work is orchestrator
composition, not backend absence.

### `AudioBridge` has a naive implementation

`heron_bridge::NaiveBridge` implements `AudioBridge` and is appropriate for
tests/prototyping. A production-grade bridge remains a blocker for GA quality.

### EventBus multi-subscriber fan-out is resolved

The bus now reaches SSE, Tauri IPC, and replay cache consumers. `LocalSessionOrchestrator`
publishes lifecycle events, and the desktop event-bus integration tests pin the
multi-subscriber behavior.

## README claims vs. reality

- **"v2 four-layer stack is currently trait surfaces only."** No longer
  accurate. Several concrete layer implementations exist, and
  `LiveSessionOwner` now composes them, but the owner is not yet connected to
  daemon capture with a production realtime backend.
- **"The desktop shell, onboarding wizard, settings pane, menubar tray have
  all shipped."** Partial. The desktop app starts the daemon and the wizard
  exists, but the wizard still lacks a user-visible daemon/runtime-preflight
  step.
- **"mobile, other meeting apps, other desktop OSes remain deferred to v1.1+."**
  Still accurate for the shipping product posture.

## Punch list — priority order

| # | Gap | File:Line | Severity | Notes |
| -- | --- | --- | --- | --- |
| 1 | Orchestrator lacks real capture/STT/LLM/vault pipeline | `crates/heron-orchestrator/src/lib.rs:563` | BLOCKER | Replace synthetic FSM-only lifecycle with real task ownership |
| 2 | Production realtime backend | `crates/heron-realtime/src/openai.rs` | RESOLVED | `OpenAiRealtime` now opens OpenAI Realtime WebSocket sessions |
| 3 | Bot + bridge + policy live-session composition owner | `crates/heron-orchestrator/src/live_session.rs` | RESOLVED | `LiveSessionOwner` now owns startup and teardown; daemon capture wiring remains under #1/#2 |
| 4 | `attach_context` unimplemented | `crates/heron-orchestrator/src/lib.rs:837` | BLOCKER | Persist/apply pre-meeting context |
| 5 | React onboarding lacks daemon/preflight step | `apps/desktop/src/store/onboarding.ts:37` | MAJOR | Backend command exists; UI still five steps |
| 6 | Doctor runtime checks not surfaced to users | `crates/heron-doctor/src/lib.rs:57` | MAJOR | Add Tauri command + onboarding/status UI |
| 7 | CLI v2 commands do not delegate to `herond` | `crates/heron-cli/src/main.rs:322` | MAJOR | Use bearer token + localhost API |
| 8 | Daemon read-side depends on external vault artifacts | `crates/heron-orchestrator/src/lib.rs:713` | MAJOR | Resolved by real daemon vault writes |
| 9 | WhisperKit semaphore timeout | `swift/whisperkit-helper/Sources/WhisperKitHelper/WhisperKitHelper.swift:78` | MINOR | Add DispatchTime deadline |
| 10 | Cross-crate v2 integration coverage | v2 crates | MINOR | Add end-to-end lifecycle suites with fakes |
