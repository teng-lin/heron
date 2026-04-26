# Codebase gap audit

_Snapshot: 2026-04-26, branch `main` at phase 80 (`4181b87`)._

A survey of the heron workspace looking for "big gaps" — places where the
codebase has obvious holes that would block shipping or that suggest
incomplete work. Scope: 17 Rust crates under `crates/`, the Tauri desktop
app under `apps/desktop`, and the Swift helpers under `swift/`.

The goal is a punch list to prioritize from, not an exhaustive TODO sweep.

## Summary

v1 (phase 77) is a shipping note-taker. v2 trait surfaces are sketched
across `heron-event`, `heron-session`, `heron-bot`, `heron-realtime`,
`heron-policy`, and `heron-bridge`, but implementations are deferred —
today the v2 daemon (`herond`) is a 501 appliance behind a real
`/health` and `/events` SSE.

**Shipping blocker:** items 1–5 below (four trait impls + swapping
`StubOrchestrator` for a real one) must land before v2 can alpha-test.
Items 6–9 block GA.

## Blockers — v2 cannot run

### 1. `herond` HTTP API is mostly 501

`crates/herond/src/routes/unimpl.rs:22` — 8 of 11 endpoints return
`NotYetImplemented`:

- `POST   /meetings`
- `GET    /meetings/{id}`
- `POST   /meetings/{id}/end`
- `GET    /meetings/{id}/transcript`
- `GET    /meetings/{id}/summary`
- `GET    /meetings/{id}/audio`
- `GET    /calendar/upcoming`
- `GET    /context`

Only `/health` (`routes/health.rs`) and `/events` (`routes/events.rs`,
SSE) are real.

### 2. `StubOrchestrator` is the only `SessionOrchestrator` impl

`crates/herond/src/stub.rs:57` — every method on the orchestrator trait
returns `NotYetImplemented`. The real `LocalSessionOrchestrator`
referenced in spec docs does not exist yet.

### 3. `MeetingBotDriver` trait has zero implementations

`crates/heron-bot/src/lib.rs:194` defines the v2 driver layer.
`crates/heron-bot/examples/recall-spike.rs` is a test harness from the
2026-04-26 spike, not a production driver. No `RecallDriver`,
`AttendeeDriver`, or `NativeZoomDriver` is checked in.

### 4. `SpeechController` + `RealtimeBackend` traits, no impls

`crates/heron-policy/src/lib.rs:140` (SpeechController) and
`crates/heron-realtime/src/lib.rs:105` (RealtimeBackend) are surface
only. No OpenAI Realtime, Gemini Live, or LiveKit Agent backends exist.
Without them no realtime LLM session can run.

### 5. `AudioBridge` trait, no impls

`crates/heron-bridge/src/lib.rs:81` defines the bridge interface
(fan-out meeting audio → realtime; agent TTS → driver playback). AEC,
jitter buffer, and resample hooks are declared but no `WebRtcAecBridge`
or naive test impl is checked in.

## Major — blockers for GA

### 6. `heron-doctor` missing runtime checks

`crates/heron-doctor/src/lib.rs` implements offline log parsing for
anomaly detection but is missing the runtime preflight checks the
onboarding wizard depends on:

- ONNX runtime health check (claimed in `docs/plan.md`).
- Zoom process availability (heron-zoom wires `AXObserver`; doctor
  doesn't verify Zoom is actually running).
- Keychain ACL scope validation (`docs/security.md` §3.3 requires it).
- Network reachability for Whisper / LLM backends.

Today users can ship without required deps and discover failures at
runtime instead of at first run.

### 7. Onboarding wizard is not wired to `herond`

`apps/desktop/src-tauri/src/onboarding.rs:1` ships five Test buttons
(mic, audio-tap, accessibility, calendar, model-download) that verify
permissions in isolation. Nothing launches or validates `herond`
afterwards — onboarding succeeds, then the daemon fails silently on
first real use.

### 8. Policy filter is defined but never invoked

`crates/heron-policy/src/filter.rs:69` implements `evaluate()` (mute,
deny_topics, allow_topics) per `docs/api-design-spec.md` §3. No
caller exists today, because no `SpeechController` impl exists (see
gap 4). Effective policy enforcement is zero.

### 9. `heron-cli` v2 commands are stubs

`crates/heron-cli/src/main.rs` documents that subcommands per
`docs/implementation.md` weeks 9–13 return
`Err(anyhow::anyhow!("not yet implemented"))` until the corresponding
crate's real implementation lands. v1's `heron summarize` works; the
v2 manual-capture escape hatches (`heron record`, etc.) don't delegate
to the herond HTTP endpoints yet.

## Minor — polish and post-v1

### 10. WhisperKit Swift bridge has no timeout

`swift/whisperkit-helper/Sources/WhisperKitHelper/WhisperKitHelper.swift:78`
— sync-via-semaphore bridge to async WhisperKit. There's no
`DispatchTime` deadline on the semaphore wait, so a hung model load
will block the calling thread forever. v1 ships this; the timeout was
deferred per `docs/plan.md`.

### 11. `EventBus` has no consumers outside `herond`

`crates/heron-event/src/lib.rs` defines the bus and `heron-session`'s
Invariant 12 mandates that all events flow through it first. Today
only `heron-cli::session::Orchestrator` publishes and `herond`
subscribes via the stub. Tauri UI and external API transports are not
wired. Fine for v1; needed when v2 frontends multiply.

### 12. v2 layer test counts are thin

For a comparable LOC surface to v1 crates, v2 layers ship far fewer
tests:

| Crate            | Tests | LOC (approx) |
| ---------------- | ----- | ------------ |
| heron-bot        | 3     | ~1,600       |
| heron-realtime   | 3     | ~1,000       |
| heron-bridge     | 5     | ~1,800       |
| heron-policy     | 4     | ~1,400       |
| heron-speech (v1)| 5     | ~2,500       |
| heron-vault  (v1)| 7     | ~2,600       |

Specs are heavily documented (`api-design-spec.md` >1,000 lines) but
proof-of-concept tests only; real impls will need integration suites.

## README claims vs. reality

- **"v2 four-layer stack is currently trait surfaces only — the
  Recall.ai spike harness validated the design against a live Zoom
  meeting on 2026-04-26."** — Accurate. The spike ran; production
  driver impl is the next gate.
- **"mobile, other meeting apps, other desktop OSes remain deferred to
  v1.1+."** — Accurate. v1 ships Zoom on macOS only as promised.
- **"The desktop shell, onboarding wizard, settings pane, menubar tray
  have all shipped."** — Partial. Onboarding UI exists; daemon
  integration (gap 7) is missing. Tray exists; commands delegate to
  unimplemented herond endpoints (gap 1).

## Punch list — priority order

| #  | Gap                                 | File:Line                                                       | Severity | Notes                                       |
| -- | ----------------------------------- | --------------------------------------------------------------- | -------- | ------------------------------------------- |
| 1  | herond endpoints 501                | `crates/herond/src/routes/unimpl.rs:22`                         | BLOCKER  | swap once orchestrator real                 |
| 2  | StubOrchestrator only impl          | `crates/herond/src/stub.rs:57`                                  | BLOCKER  | build LocalSessionOrchestrator              |
| 3  | MeetingBotDriver no impl            | `crates/heron-bot/src/lib.rs:194`                               | BLOCKER  | RecallDriver next; spike findings exist     |
| 4  | SpeechController/RealtimeBackend    | `crates/heron-policy/src/lib.rs:140`, `crates/heron-realtime/src/lib.rs:105` | BLOCKER  | OpenAI Realtime first                       |
| 5  | AudioBridge no impl                 | `crates/heron-bridge/src/lib.rs:81`                             | BLOCKER  | WebRTC AEC; workspace dep exists            |
| 6  | heron-doctor runtime checks         | `crates/heron-doctor/src/lib.rs`                                | MAJOR    | ~100 LOC per check, 5–6 checks              |
| 7  | onboarding → herond wiring          | `apps/desktop/src-tauri/src/onboarding.rs:1`                    | MAJOR    | launch herond as managed Tauri service      |
| 8  | policy enforcement in speech path   | `crates/heron-policy/src/filter.rs:69`                          | MAJOR    | wire evaluate() into SpeechController impl  |
| 9  | heron-cli v2 commands               | `crates/heron-cli/src/main.rs`                                  | MAJOR    | delegate to herond HTTP                     |
| 10 | WhisperKit semaphore timeout        | `swift/whisperkit-helper/Sources/WhisperKitHelper/WhisperKitHelper.swift:78` | MINOR    | add DispatchTime deadline                   |
| 11 | EventBus subscribers                | `crates/heron-event/src/lib.rs`                                 | MINOR    | post-v1; Tauri + HTTP transports            |
| 12 | v2 layer test coverage              | (see table above)                                               | MINOR    | integration suites land with impls          |
