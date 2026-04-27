# Companion-mode bot — implementation plan

> Engineering plan for shipping the companion-mode product direction defined in [`heron-vision.md`](heron-vision.md). Concrete crate layout, phase sequencing, PR slicing, and explicit acceptance criteria.
>
> **Plan revision: 2026-04-27 (v4).** Reframed around the three-mode taxonomy (Clio / Athena / Pollux) and incremental shipping. v3 collapsed all modes into a single 22-week release; v4 ships Clio at week ~6, Athena at week ~11, Pollux at week ~22–26. The phase-by-phase architecture, file paths, and acceptance criteria carry over from v3 with mode tags added.

## TL;DR

- **Three modes ship incrementally**, not as one big release:
  - **Mode-1 Clio** (silent note-taker, mostly v1 hardening): GA at **~week 6**.
  - **Mode-2 Athena** (listens + suggests in sidebar, never speaks in meeting): GA at **~week 11**.
  - **Mode-3 Pollux** (full cyborg — virtual mic, voice cloning, double-booking): GA at **~week 22–26**.
- **Phase 0 spikes (2 wk)** kick off all three modes' tracks in parallel. Different spikes are required for different modes; some run in parallel with early shipping work.
- **Each phase is mode-tagged** below: which modes need it, which phases each mode requires.
- **Per-PR workflow** unchanged: `/polish` → `/pr-workflow` → squash-merge only when CI green and `mergeStateStatus: CLEAN` (per [`CLAUDE.md`](../CLAUDE.md)).

## Mode → phase requirement matrix

| Phase | Effort | Clio | Athena | Pollux |
|---|---|:---:|:---:|:---:|
| **P0.1** TTS provider eval (S0.1) | 1–2 d | — | — | ✓ |
| **P0.2** HAL plug-in MVP (S0.2) | 5–7 d | — | — | ✓ |
| **P0.3** Classifier corpus (S0.3) | 1 wk | — | ✓ | ✓ |
| **P0.4** HAL install UX (S0.4) | 3–4 d | — | — | ✓ |
| **P0.5** Type ownership audit (S0.5) | 1–2 d | ✓ | ✓ | ✓ |
| **P1** Audio plumbing MVP (HAL + virtual mic) | 3 wk | — | — | ✓ |
| **P2** Realtime conversation | 3 wk | — | ✓ | ✓ |
| **P2.5** Per-platform AX adapters (Meet/Teams) | 3 wk | ✓ | ✓ | ✓ |
| **P3a** Hand-off classifier (suggests-to-UI for Athena) | 2 wk | — | ✓ | ✓ |
| **P3b** Hand-off classifier (drives source-swap for Pollux) | 2 wk | — | — | ✓ |
| **P4** Multi-meeting concurrency | 4 wk | — | — | ✓ |
| **P5a** Disclosure UX (light: Clio + Athena) | 1 wk | ✓ | ✓ | ✓ |
| **P5b** Guardrails + voice biometrics consent (Pollux) | 1 wk | — | — | ✓ |
| **P6** Voice cloning | 2.5 wk | — | — | ✓ |
| **P7** Vendor-SDK layer cleanup | 1 wk | ✓ | ✓ | ✓ |
| **P8** Beta (continuous) | — | ✓ | ✓ | ✓ |
| **NEW: P-ATHENA** Athena UI surface (sidebar + alerts) | 1.5 wk | — | ✓ | ✓ (re-uses) |
| **NEW: P-CLIO** Clio hardening (UI rename + extended AX) | 1 wk | ✓ | — | — |

## Mode release tracks

```
Wk:   1   2   3   4   5   6   7   8   9   10  11  12  13  14  15  16  17  18  19  20  21  22  23
      ─────────────────────────────────────────────────────────────────────────────────────────────
P0    ████████  (S0.1, S0.2, S0.3, S0.4, S0.5 — different spikes for different modes)

OBSERVER track (silent note-taker, ships at wk 6):
P-CLIO         ████          ▼ Mode-1 Clio GA
P5a            ██          (lightweight disclosure)
P7                ██        (cleanup of v2 driver layer)

CO-PILOT track (suggests in sidebar, ships at wk 11):
P2.5      ████████████████████        (per-platform AX in parallel)
P2            ████████████
P3a                         ████████        ▼ Mode-2 Athena GA
P-ATHENA                                ██████

SURROGATE track (cyborg, ships at wk 22-26):
P1            ████████████
P3b                                   ████████
P4                                            ████████████████
P5b                                                       ██████
P6                                                              ██████████
                                                                          ▼ Mode-3 Pollux GA

P8 Beta:    Clio beta from wk 6;  Athena beta from wk 11;  Pollux beta from wk 22.
```

Critical paths:
- **Clio**: P0.5 → P-CLIO → P5a → P7 → GA. ~6 weeks.
- **Athena**: P0.3 + P0.5 → P2 → P3a → P-ATHENA → P5a → GA. ~11 weeks.
- **Pollux**: All P0 → P1 → P2 → P3a → P3b → P4 → P5b → P6 → GA. ~22 weeks + 2–4 wk buffer.

The plan retains the v3 architectural fixes (`current_reader_pid` verifier, room-tone injection, `PolicyDecision::Stall`, type migration in P0.5 etc.) — the change in v4 is the *taxonomy and shipping cadence*, not the engineering content.

---

## Phase 0 — Spikes (2 weeks, five parallel tracks)

(Carried from v3. Mode tags added to each.)

### S0.1 — TTS provider evaluation [Pollux-only]
**Goal**: pick TTS provider for voice cloning. **Compare**: ElevenLabs, OpenAI Realtime preset (no clone yet), local XTTS/Coqui. **Output**: `docs/archives/spike-s0-1-tts-eval-YYYY-MM-DD.md`. **Effort**: 1–2 d.

### S0.2 — HAL plug-in MVP scope [Pollux-only]
**Goal**: confirm we can ship a heron-branded AudioServerPlugIn with the v3-required APIs (`current_reader_pid`, source switch, room-tone injection, codesigned + notarized).

**Measure**: HAL gap ≤ 30 ms, perceptual swap test, AEC reconverge ≤ 1.5 s, `current_reader_pid` correctness, N=8 concurrent devices feasible, TCC permission audit (Process Tap vs. ScreenCaptureKit).

**Reference reading**: [BlackHole](https://github.com/ExistentialAudio/BlackHole), [BackgroundMusic DEVELOPING.md](https://github.com/kyleneideck/BackgroundMusic/blob/master/DEVELOPING.md), [libASPL](https://github.com/gavv/libASPL).

**Output**: `docs/archives/spike-s0-2-hal-plugin-YYYY-MM-DD.md`. **Effort**: 5–7 d. Highest-risk spike.

### S0.3 — Hand-off classifier corpus collection [Athena + Pollux]
**Goal**: collect labeled corpus before P3a. 20 internal meetings, ≥ 1500 windows, ≥ 100 ESCALATE positives. **Output**: `docs/archives/spike-s0-3-corpus-YYYY-MM-DD.md` + `fixtures/handoff-corpus.jsonl`. **Effort**: 1 wk.

### S0.4 — HAL plug-in distribution + install UX [Pollux-only]
**Goal**: design first-install / upgrade / uninstall / denial / `coreaudiod`-restart flow. CI strategy decision (self-hosted vs `#[ignore]` on hosted CI). **Output**: `docs/archives/spike-s0-4-install-ux-YYYY-MM-DD.md`. **Effort**: 3–4 d.

### S0.5 — Type ownership audit + migration plan [ALL modes]
**Goal**: settle where cross-cutting types live before any consumer phase imports them. Codex's catch: `PreMeetingContext` already in `crates/heron-session/src/lib.rs:272`. Grep workspace, decide canonical home per type, decide `BotState` destination, decide `PolicyDecision::Stall` extension (NOT new `Decision` enum).

**Output**: `docs/archives/spike-s0-5-type-audit-YYYY-MM-DD.md` with call-site inventory + migration sequence. **Effort**: 1–2 d.

---

## Mode-1 Clio track (~6 weeks total)

### P-CLIO — Clio hardening (1 week) [Clio-only]

**Goal**: rename v1 note-taker as "Clio mode," extend AX to non-Zoom platforms (gated to P2.5 if Meet/Teams polish needed; Zoom-only acceptable for first GA), add lightweight disclosure UX.

**Modified crates**:
- `crates/heron-zoom/` — split into per-platform AX adapters (or rename to `heron-ax/` per S0.5 outcome).
- `apps/desktop/src-tauri/` — UI rename ("Note-taker" → "Clio"); mode picker scaffolding (one mode visible today, more coming).

**PR slicing**:
1. `feat(desktop): rename note-taker UI to "Clio mode" + mode picker scaffold`
2. `feat(ax): finalize Zoom AX adapter (replaces stub at heron-zoom/src/ax_bridge.rs:72)`
3. `feat(observer): light disclosure UX (per P5a)`

**Acceptance**:
- Existing v1 note-taker users see no functional regression.
- New onboarding flow names "Clio" and previews future modes.
- `heron-doctor --probe clio` green.

**Effort**: 1 wk. Requires S0.5 first (so AX-adapter rename uses settled type homes).

### P5a — Lightweight disclosure UX (1 week) [Clio + Athena]

**Goal**: ship the disclosure layer that Clio and Athena need; defer voice-biometrics consent to P5b (Pollux).

**Modified crates**:
- `crates/heron-policy/` — extend `PolicyDecision` with `Stall { reason }` (canonical extension per S0.5; used by Athena's classifier).
- `apps/desktop/src-tauri/` — per-meeting disclosure toggle (geo-aware default per the conservative jurisdiction handling: declared + calendar + IP).
- New: per-meeting consent capture persisted in vault.

**PR slicing**:
1. `feat(policy): extend PolicyDecision with Stall + can_speak entry point`
2. `feat(desktop): per-meeting disclosure toggle (Clio + Athena defaults)`
3. `feat(desktop): conservative jurisdiction handling (declared + calendar + IP)`

**Acceptance**:
- Two-party-consent simulation: 0/N false-negatives over 50 trials.
- **Legal sign-off** on Clio + Athena consent text — required before merge of PR-2 + PR-3.

**Effort**: 1 wk (was 1.5 in v3; v4 splits voice-biometrics into P5b).

### P7 — Vendor-SDK cleanup (1 week) [ALL modes]

(Carried from v3.)

**Removed**:
- `crates/heron-bot/src/recall/` — entire Recall integration.
- `crates/heron-bot/src/lib.rs` — `MeetingBotDriver` trait, `BotState` (whichever variants haven't already migrated per S0.5), `BotCreateArgs`, `BotError`, `EjectReason` enum.
- `crates/heron-orchestrator/src/live_session.rs` — `LiveSessionFactory`, `LiveSessionOwner`, all stub drivers.
- `apps/desktop/src-tauri/src/` — Recall API key provisioning.

**Documentation**: SUPERSEDED banners on v2 spec docs; update README; update `docs/architecture.md`.

**Acceptance — expanded grep**:
- `! grep -r "MeetingBotDriver\|RecallDriver\|LiveSessionFactory\|BotState\|BotId\|EjectReason\|heron_bot::\|RECALL_API_KEY" crates/ apps/`
- `cargo test --workspace` + `cargo clippy --workspace --all-targets -- -D warnings` green.

**Effort**: 1 wk.

### Clio GA gate (~week 6)
- P-CLIO, P5a, P7 all merged.
- Internal beta active for ≥ 2 weeks.
- Recording-consent text legally signed off.
- No regression vs. v1 note-taker on existing Zoom flows.

---

## Mode-2 Athena track (~11 weeks total)

### P2 — Realtime conversation foundation (3 weeks) [Athena + Pollux]

**Goal**: bot has a real conversation. Wires existing `heron-realtime` (already has `openai.rs`) to the audio pipeline. **For Athena**: outputs go to the heron sidebar, NOT to a virtual mic. **For Pollux**: same pipeline, output route flips to virtual mic at P3b.

(See the v3 file for the full P2 detail. Carried over verbatim except for noting the dual mode usage.)

**New crates**: `crates/heron-companion/` — top-level orchestrator. Pipeline: process-tap-in → transcription → realtime LLM → policy-gate → **mode-specific output adapter**.

**Modified crates**: `crates/heron-realtime/`, `crates/heron-bridge/`, `crates/heron-zoom/` (build the AX adapter; Zoom-only for now).

**PR slicing**:
1. `feat(companion): scaffold heron-companion crate with mode-specific output adapter trait`
2. `feat(realtime): close streaming-speech-path gaps`
3. `feat(companion): wire process-tap → realtime → adapter (AthenaSidebarAdapter as first impl)`
4. `feat(zoom): build Zoom AX adapter (replaces stub)`
5. `feat(desktop): minimal companion-mode UI scaffold (mode picker, transcript view)`

**Acceptance**: 5-min smoke with Athena adapter — bot writes ≥ 8/10 briefing-content suggestions to sidebar correctly.

**Effort**: 3 wk.

### P2.5 — Per-platform AX adapters (3 weeks, parallel) [ALL modes]

**Goal**: speaker attribution for Meet, Teams. Webex stays out of v1.

(Carried from v3 verbatim; serves all three modes' speaker attribution needs.)

**PR slicing**: rename + per-platform AX modules.

**Effort**: 3 wk. Runs in parallel with P2 + P3.

### P3a — Hand-off classifier (suggests-to-UI) (2 weeks) [Athena + Pollux]

**Goal**: classifier identifies trigger windows; for Athena, results surface to the sidebar; for Pollux (later, in P3b), the same classifier drives the source-swap.

**New crates**: `crates/heron-handoff/` — trigger classifier.

**Classifier model decision**: OpenAI function-calling on existing realtime session. Honest p99 latency budget: ≤ 1.5 s. `confidence` field removed (LLMs don't expose calibrated values). Few-shot prompt as `const` string in `heron-handoff/src/prompt.rs`.

**Triggers**: `NameSpoken`, `DirectQuestion`, `DecisionRequest`, `NamedOutsideBriefing`, `EmotionalShift`, `GuardrailViolation`.

**PR slicing**:
1. `feat(handoff): scaffold heron-handoff crate + classifier prompt + S0.3 corpus eval`
2. `feat(companion): integrate classifier with AthenaSidebarAdapter (suggests-to-UI)`

**Acceptance**: S0.3 corpus eval — precision ≥ 0.85, recall ≥ 0.7. Manual: confederate-triggered escalations all surface in sidebar within 2 s.

**Effort**: 2 wk.

### P-ATHENA — Athena UI surface (1.5 weeks) [Athena-specific]

**Goal**: build the sidebar where Athena's suggestions, fact-checks, and trigger flags appear.

**Tauri / desktop changes**:
- New sidebar pane in heron's existing window. Live-updates from the companion daemon via the existing event channel.
- Three sections, each collapsible: **Triggers** (red flags from classifier), **Drafts** (suggested replies), **Facts** (vault content the LLM thinks is relevant).
- Suggestion density default: ≤ 1 surfaced item per 30 s ("high signal only"). User-tunable.
- Optional macOS Critical Alert when a high-priority trigger fires and the heron window is unfocused.
- Keyboard shortcut to dismiss a suggestion (default ⌘.).

**Acceptance**:
- 5-min meeting smoke: confederate-triggered escalations appear in sidebar within 2 s; user can read aloud the suggested reply naturally.
- Sidebar respects the suggestion-density throttle (no more than 10 items per 5 min by default).

**Effort**: 1.5 wk.

### Athena GA gate (~week 11)
- P0.3 + P0.5 + P2 + P2.5 (or at least Zoom AX) + P3a + P-ATHENA + P5a + P7 merged.
- Internal beta ≥ 2 weeks.
- Classifier hits acceptance thresholds.
- AI-assistance disclosure text legally signed off.

---

## Mode-3 Pollux track (~22–26 weeks total)

### P1 — Audio plumbing MVP (3 weeks) [Pollux-only]

**Goal**: end-to-end audio in/out works. Heron's HAL plug-in + Process Tap + a static test audio file is heard cleanly in a real Zoom meeting; meeting audio flows back into transcription. No LLM, no TTS, no hand-off.

**OS floor**: macOS 14.2+.

(Full detail carried from v3. Mode-tag: Pollux-only.)

**New crates**: `crates/heron-virtual-mic/` (with `current_reader_pid()` verifier, source switch, room-tone push, `VirtualMicAllocator`). `helpers/heron-virtual-audio/` (the AudioServerPlugIn — xcodebuild, codesigned, notarized).

**Modified crates**: `crates/heron-audio/` (wire `process_tap.rs`); `crates/heron-doctor/` (probes); `apps/desktop/src-tauri/` (S0.4 install flow).

**Mic Armed state**: real mic captures only when bot session is active or take-over UI is open. Visible indicator in menubar.

**PR slicing**:
1. `feat(virtual-audio): scaffold HAL plug-in (one device, source switch, current_reader_pid, room-tone)`
2. `feat(virtual-mic): scaffold heron-virtual-mic + VirtualMicAllocator`
3. `feat(audio): Mic Armed state + process_tap.rs → transcription wiring`
4. `feat(doctor): probes for HAL plug-in + TCC + coreaudiod + real-mic perm`
5. `feat(desktop): onboarding install flow per S0.4`
6. `feat(desktop): pre-meeting wizard verifying Zoom uses heron Mic via current_reader_pid match`

**Acceptance** (carried from v3):
- `cargo test -p heron-virtual-mic` green on hosted CI; self-hosted CI green on real plug-in.
- Manual smoke: confederate hears tone clearly; reply transcribes WER ≤ 10 %.
- `heron-doctor --probe pollux` reports `Pass`.

**Effort**: 3 wk. Runs in parallel with P2/P2.5.

### P3b — Hand-off source-swap (2 weeks) [Pollux-only]

**Goal**: extend the P3a classifier output from "surfaces to sidebar" to "drives the in-plug-in source swap." Bot stalls with filler ≥ 1.5 s, fires push notification, awaits user-takeover, swaps source from TTS to RealMicPassthrough.

**Modified crates**:
- `crates/heron-companion/` — new `PolluxAdapter` impl of the output adapter trait. Same classifier feed; different action.
- `crates/heron-virtual-mic/` — fast (`< 5 ms`) source-swap API used by the swap.

**Hand-off SLOs** (carried from v3):
- Classifier latency p99 ≤ 1.5 s.
- Filler-start latency p99 ≤ 800 ms.
- Filler duration ≥ 1.5 s (lets Zoom AEC reconverge).
- Source-swap acceptance: HAL gap ≤ 30 ms + perceptual test (confederate identifies swap moment ≤ 5/10 trials with room-tone injection) + AEC stability (no audible howl ≥ 5 s post-swap).
- User-take-over grace 10 s; on timeout: "I'll need to come back to that."

**Tauri / desktop**: notification (Tauri + Critical Alert escalation); take-over UI (default ⌘⇧Space); post-meeting escalation review.

**PR slicing**:
1. `feat(companion): PolluxAdapter with filler + grace timer`
2. `feat(virtual-mic): fast source-swap + room-tone integration`
3. `feat(desktop): notification path + take-over UI + Mic Armed indicator`
4. `feat(desktop): post-meeting escalation review`

**Acceptance**: manual swap-quality test passes thresholds (perceptual ≥ 4/5 average across 8/10 trials).

**Effort**: 2 wk.

### P4 — Multi-meeting concurrency (4 weeks) [Pollux-only]

(Carried from v3. Includes concrete `MeetingRoute` data model, `MeetingIdentity` enum for native-app-vs-browser-tab, persistence, crash recovery.)

**New crates**: `crates/heron-meeting-router/`.

**Effort**: 4 wk.

### P5b — Pollux guardrails + voice biometrics consent (1 week) [Pollux-only]

**Goal**: extend P5a's lightweight disclosure with the full Pollux apparatus — voice biometrics consent, multi-step BIPA/GDPR-aware consent capture, full guardrail policy (not commitments to dates/dollars/hires/scope).

**Modified crates**: `crates/heron-policy/` — full per-persona + per-meeting policy composition.

**PR slicing**:
1. `feat(policy): full guardrail policy composition (most-restrictive-wins)`
2. `feat(desktop): voice-biometrics consent flow (multi-step, jurisdiction-aware)`
3. `feat(desktop): per-meeting policy editor + blocklists`

**Acceptance**: legal sign-off on voice-biometrics consent text — required before merge.

**Effort**: 1 wk.

### P6 — Voice cloning (2.5 weeks) [Pollux-only]

(Carried from v3. Multi-step consent flow + biometric privacy notice + retention disclosure already in P5b; P6 builds the actual capture + provider integration + Keychain storage + local-TTS fallback.)

**New crates**: `crates/heron-voice-clone/`.

**PR slicing** (carried from v3):
1. `feat(voice-clone): scaffold + provider-specific backend (per S0.1)`
2. `feat(desktop): voice-sample capture + quality gates`
3. `feat(companion): switch PolluxAdapter TTS to cloned voice ID with local-TTS fallback`
4. `feat(desktop): voice deletion flow`

**Effort**: 2.5 wk.

### Pollux GA gate (~week 22)
- All of P0 + P1 + P2 + P3a + P3b + P4 + P5b + P6 merged.
- Internal beta ≥ 4 weeks.
- All P3b SLOs met on real meetings.
- Voice biometrics consent legally signed off.

---

## Phase 8 — Beta (continuous, per mode)

Each mode enters internal beta when its GA gate clears, then external beta after its mode-specific exit criteria are met.

### Exit criteria per mode (internal → external)

**Clio**:
- ≥ 50 real meetings across ≥ 5 users.
- No regression vs. v1 note-taker on transcription accuracy or summary quality.
- Recording-consent flow green in TPC simulation.

**Athena**:
- ≥ 50 real meetings across ≥ 5 users.
- Classifier precision ≥ 0.85, recall ≥ 0.7.
- Suggestion-relevance survey ≥ 80 % "useful" rating.
- AI-assistance disclosure flow green.

**Pollux**:
- ≥ 50 real meetings across ≥ 5 users.
- Classifier precision ≥ 0.85, recall ≥ 0.7.
- Source-swap perceptual rating ≥ 4/5 average (100+ swaps).
- AEC stability: no audible echo > 1 s in ≥ 95 % of swaps.
- Crash rate ≤ 1 per 50 meetings.
- "Would you let the bot run again?" ≥ 80 % yes.
- Voice biometrics consent legally signed off.

---

## Failure mode matrix

(Carried from v3.)

| Failure | Detection | UX | Recovery |
|---|---|---|---|
| `herond` panic | systemd / launchd respawn | Bot session paused; HAL plug-in sources go to silence | Restore routes from vault; user prompted to resume |
| HAL plug-in eviction | doctor probe + plug-in heartbeat | Pollux sessions end | User re-installs via onboarding |
| Provider outage (OpenAI, ElevenLabs) | HTTP error/timeout | Pollux falls back to local TTS; classifier falls back to "always escalate" | Auto-recover when provider returns |
| OpenAI Realtime WS disconnect | WS event | Bot pauses; filler emitted; user notified | Auto-reconnect with backoff |
| Real mic permission revoked | TCC check + capture failure | Bot can't take over; user notified | User grants in System Settings |
| Meeting app crash / restart | NSWorkspace notification | Route → `Reconnecting`; bot pauses | App relaunches → re-resolve PID |
| Network loss | request failure | Bot pauses; classifier stops; filler emitted | Resume on reconnect |

## Per-platform support tiers

| Platform | Clio | Athena | Pollux | Phase shipped |
|---|---|---|---|---|
| Zoom | ✓ today | v1 (P2) | v1 (P3b) | per mode |
| Google Meet | v1 (P2.5) | v1 (P2.5) | v1 (P2.5) | P2.5 |
| Microsoft Teams | v1 (P2.5) | v1 (P2.5) | v1 (P2.5) | P2.5 |
| Webex | post-beta | post-beta | post-beta | TBD |
| Tencent / WeMeet | post-beta | post-beta | post-beta | TBD |

## Data retention table

(Carried from v3.)

| Artifact | Where | Retention | Deletion path |
|---|---|---|---|
| Meeting transcripts | vault (local) | User-controlled (default 90 d) | Settings → "delete meeting" |
| Voice samples (Pollux only) | `~/.heron/voice-sample-tmp.wav` | 24 h max | Auto-deleted post-upload + on success |
| Cloned voice ID (Pollux) | macOS Keychain + provider | Until user-initiated deletion | Settings → "delete my cloned voice" |
| Consent records | vault | Permanent (audit trail) | Cascade-delete with associated meeting |
| Classifier corpus | `fixtures/handoff-corpus.jsonl` | Permanent | Manual deletion + repo update |
| Telemetry | local-only (P8); upload TBD | Local: rolling 30 d | Settings → "clear telemetry" |

## Verification commands

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# Mode-specific (after the relevant phases have merged):
# Clio
cargo test -p heron-zoom -p heron-policy -p heron-speech
heron-doctor --probe clio

# Athena
cargo test -p heron-companion -p heron-realtime -p heron-handoff
heron-doctor --probe athena

# Pollux
cargo test -p heron-virtual-mic -p heron-meeting-router -p heron-voice-clone
helpers/heron-virtual-audio/build.sh
heron-doctor --probe pollux

# P7 — expanded grep
! grep -r "MeetingBotDriver\|RecallDriver\|LiveSessionFactory\|BotState\|BotId\|EjectReason\|heron_bot::\|RECALL_API_KEY" crates/ apps/
```

## Definition of done (v1.0)

A v1.0 release means **all three modes shipped** and met their respective GA gates + external-beta exit criteria. Subgoals:

1. All phases shipped, each merged via the per-PR workflow in `CLAUDE.md`.
2. **Clio** in external beta from ~week 6.
3. **Athena** in external beta from ~week 11.
4. **Pollux** end-to-end smoke: real double-booked meeting; bot represents user in B; escalates correctly twice in 30 min; user takes over with perceptual rating ≥ 4/5.
5. Per-mode legal sign-offs in place.
6. `docs/architecture.md` reflects companion-mode taxonomy; v2 spec docs banner-archived.
7. Workspace clippy + fmt + test all green.
8. heron HAL plug-in (Pollux-only) signed, notarized, shipping with the desktop installer.
9. Failure-mode matrix entries each have a manual repro + recovery validated.

## Open questions deferred to product

- **Mode picker UX**: per-meeting selector? Per-persona default? Onboarding-time default? Resolve before P5a.
- **Mode-switch within a session**: can a user start in Clio and "promote" to Athena mid-meeting? Likely yes; needs UX. Resolve in P3a retrospective.
- **Athena sidebar surface alternatives**: floating overlay (Cluely-style), Apple Watch glanceable feed. Post-MVP.
- **v1 note-taker compatibility**: Clio mode IS the v1 note-taker, renamed. No separate "no virtual mic" mode needed since Clio never has one.
- **AX-driving Zoom's mic picker** (Pollux): brittle but eliminates user-side device-selection. Investigate post-P4.
- **Cross-platform Windows/Linux ports**: Clio/Athena are most portable; Pollux requires platform-specific routing. Out of scope for v1.
- **Disclosure default direction (Pollux)**: locked OFF; reviewers prefer ON. User reaffirms or revisits after Athena ships and we have adoption data.
