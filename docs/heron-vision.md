# Companion-mode bot — product vision

> Canonical reference for heron v2 product direction. Supersedes the v2 participant-bot work tracked in `docs/archives/api-design-spec.md`, `docs/archives/spike-findings.md`, and `docs/archives/build-vs-buy-decision.md`. Implementation plan lives in [`heron-implementation.md`](heron-implementation.md).
>
> **Revision 2026-04-27**: companion mode is now a *taxonomy* of three operation modes — Clio, Athena, Pollux — each with its own job-to-be-done, legal posture, and shipping timeline. Earlier revisions treated companion as one thing with optional toggles; that conflated three distinct products and forced the most-exposed mode's legal apparatus on the simplest user.

## Core hypothesis

> Most meetings are dull. Some need help. A few would happen without you if they could.

heron meets users where they are along that spectrum, with three operation modes that share a substrate but ship as distinct products:

| Mode | Job-to-be-done | What the user does | When it ships |
|---|---|---|---|
| **1. Clio** | "I want a record of what was said." | Joins their meeting normally; heron transcribes + summarizes silently. | Hardening of v1; ~6 weeks. |
| **2. Athena** | "I want help thinking in real time, while I stay in control." | Joins their meeting normally; heron listens, surfaces facts / draft replies / fact-checks in a sidebar. User speaks every word. | ~11 weeks. |
| **3. Pollux** | "I can't be in two places. Be me in meeting B while I'm in A." | Configures heron once per meeting app; heron speaks in their cloned voice; escalates on triggers. | ~22–26 weeks. |

Each mode is a complete product. Most users will land on Clio or Athena. Pollux is the headline but the smaller-audience tier — the double-booking power user.

## Why this direction

Comparative research (2026-04-26) on the meeting-bot landscape established four facts:

1. Recall.ai itself defaults to Chromium-in-Bubblewrap rather than the native Zoom Meeting SDK; the SDK is opt-in for customers who explicitly need per-participant raw audio.
2. Nobody publicly runs the Linux Meeting SDK on a user's Mac; the macOS Meeting SDK variant is Cocoa-runtime / UI-coupled and not designed for headless use.
3. Zoom's Realtime Media Streams (GA June 2025) plus the OBF token requirement (effective March 2026) signal Zoom is herding the ecosystem off bot-based integration.
4. Granola, Cluely, Krisp, and Circleback (Circleback's [public engineering post](https://circleback.ai/blog/how-we-rebuilt-our-electron-recording-engine-in-swift) is the canonical testimony) all converged on local audio capture and won.

Companion mode (in any of the three flavors) is the path of least resistance and highest user value. The three-mode split lets us ship the simpler modes faster while the harder mode bakes.

---

## Mode 1 — Clio

> "I want a record of what was said."

The original heron note-taker, renamed and made first-class.

### What it does
- Captures system audio of the meeting via the existing Core Audio process tap (macOS 14.2+).
- Transcribes locally via WhisperKit.
- Identifies speakers via per-platform Accessibility-tree adapters (Zoom today; Meet/Teams next).
- Summarizes after the meeting.
- **Silent.** No participation. No persona.

### What ships
Mostly v1 hardening: UI rename ("Note-taker" → "Clio"), Meet/Teams AX adapters (graduated to first-class — were planned for v2 anyway), lightweight disclosure UX (toggle: "let other participants know I'm transcribing").

### Legal posture
Recording-consent only. Two-party-consent jurisdictions get an automatic disclosure prompt. No voice biometrics, no impersonation.

### When
~6 weeks (mostly hardening + Meet/Teams AX).

---

## Mode 2 — Athena

> "I want help thinking in real time, while I stay in control."

Bot listens; bot suggests; user speaks. The bot never makes a sound in the meeting.

### What it does
Everything Clio does, **plus**:
- A live LLM session via `heron-realtime` consumes the transcript and the user's pre-meeting briefing context.
- A heron sidebar (in the existing desktop window) surfaces:
  - **Facts**: relevant info from the user's notes / vault, when topic matches.
  - **Drafts**: a suggested reply, when the user is asked something and has hesitated.
  - **Trigger flags**: "your name was mentioned," "decision needed," "topic outside your briefing" — same classifier as Pollux's hand-off, but the user is the one who acts on it.
- An optional macOS Critical Alert when a trigger fires and the heron window isn't focused.

### What it does NOT do
- Speak into the meeting. No virtual mic. No TTS into the meeting audio.
- Clone the user's voice.
- Take over for the user.

### Legal posture
Recording consent + AI-assist disclosure (light — "I'm using an AI assistant during this call"). No voice biometrics. No impersonation. The user remains the only voice on the call.

### Why this exists as a separate mode
Many users want help but won't accept the legal/ethical surface of voice cloning. Athena is the largest target market in this space (Granola, Cluely, Krisp, etc. all live here). Shipping it as a distinct mode lets it stand on its own without the cyborg's legal apparatus.

### When
~11 weeks (P0 spikes that co-pilot needs + P2 realtime + P3-suggest classifier + co-pilot UI surface + P5-light disclosure).

### UX surface
Default: a sidebar in heron's existing desktop window. Floating overlay (Cluely-style) and Apple Watch glanceable feed are post-MVP options.

---

## Mode 3 — Pollux

> "I can't be in two places. Be me in meeting B while I'm in A."

The full cyborg. Bot speaks via cloned voice through a virtual microphone; classifies escalation triggers; hands off to the user mid-conversation. The double-booking solver.

### What it does
Everything Athena does, **plus**:
- Speaks into the meeting via heron's HAL plug-in virtual microphone, using the user's cloned voice (TTS).
- Hand-off classifier triggers a fast in-plug-in source swap from TTS → real-mic-passthrough; user takes over with a keystroke.
- Multi-meeting concurrency: heron can attend meeting B while the user attends A.
- Pre-meeting policy + guardrails (no commitments to dates / dollars / hires / scope without escalation).

### Legal posture
Most exposed of the three. Two-party-consent jurisdictions auto-enable disclosure. Voice biometrics consent flow (BIPA / GDPR-aware). Per-meeting consent capture. Required: legal sign-off on consent text before any external launch.

### When
~22–26 weeks (full plan — P1 HAL plug-in + P3 hand-off SLOs + P4 multi-meeting + P6 voice cloning + P5 full guardrails).

### Why retain this mode at all
Double-booking is a real, painful, currently-unsolved problem. Most "AI meeting assistant" products land in the Athena tier and stop. Pollux is the differentiator — but it carries 2-3× the engineering and 5× the legal exposure of Athena, so it must be a deliberate, separate tier.

---

## Locked product positions (per mode)

### Disclosure default
- **Clio**: ON when at least one participant is in a two-party-consent jurisdiction; user-toggleable.
- **Athena**: ON by default (light disclosure: "I'm using AI assistance"); user can opt out per meeting.
- **Pollux**: OFF by default per the locked decision earlier in this conversation; **automatically ON in two-party-consent jurisdictions**; per-meeting confirmation when geo signals disagree. Reviewers preferred default-ON globally; the locked OFF-with-TPC-override stands until revisited.

### Voice
- **Clio**: N/A (silent).
- **Athena**: N/A (silent in the meeting; only writes to the heron sidebar).
- **Pollux**: cloned from user's own voice; provider TBD per spike S0.1.

### Hand-off mechanics
- **Clio**: N/A (no participation).
- **Athena**: classifier fires → trigger flag in sidebar + optional Critical Alert. User acts manually.
- **Pollux**: classifier fires → bot stalls with filler (≥ 1.5 s for AEC reconverge) → push notification → 10 s grace → in-plug-in source swap (TTS → real-mic with continuous −60 dB room-tone injection so noise floors match across the swap).

### Guardrails
- **Clio**: N/A.
- **Athena**: surfaces violations in sidebar; user decides.
- **Pollux**: hard gate via `heron-policy::PolicyDecision::Stall { reason }`; violations become escalation triggers.

---

## Shared substrate

Modes share crates and infrastructure where it's safe to. Mode-specific code is named per-mode.

| Subsystem | Crate | Clio | Athena | Pollux |
|---|---|---|---|---|
| Audio capture (Core Audio process tap) | `heron-audio` | ✓ | ✓ | ✓ |
| Transcription (WhisperKit) | `heron-speech` | ✓ | ✓ | ✓ |
| Per-platform speaker attribution (AX) | `heron-zoom` (becomes per-platform) | ✓ | ✓ | ✓ |
| Vault writer | `heron-vault` | ✓ | ✓ | ✓ |
| Realtime LLM | `heron-realtime` | — | ✓ | ✓ |
| Hand-off classifier | `heron-handoff` | — | ✓ (suggest) | ✓ (swap) |
| Policy / guardrails | `heron-policy` | partial | partial | full |
| Disclosure UX | `apps/desktop/src-tauri` | light | light+ | full |
| **HAL plug-in (virtual mic)** | `heron-virtual-mic` + `helpers/heron-virtual-audio/` | — | — | required |
| **Voice cloning** | `heron-voice-clone` | — | — | required |
| **Multi-meeting routing** | `heron-meeting-router` | — | — | required |

The HAL plug-in, voice cloning, and multi-meeting machinery are **only required for Pollux**. Clio and Athena ship without any of them, which is why those modes ship 10+ weeks earlier.

---

## Shared architecture sketch

```
┌─────────────────────────────────────────────────────────────────────┐
│                        herond (heron daemon)                        │
│                                                                     │
│  ┌─────────────┐    ┌────────────┐    ┌────────────┐                │
│  │ Audio in    │───▶│ Transcribe │───▶│ Vault      │ ALL MODES      │
│  │ (Process    │    │ (Whisper)  │    │ writer     │                │
│  │  Tap)       │    └─────┬──────┘    └────────────┘                │
│  └─────────────┘          │                                         │
│                           ▼                                         │
│                    ┌────────────┐                                   │
│                    │ Realtime   │ Athena + Pollux              │
│                    │ LLM        │                                   │
│                    └─────┬──────┘                                   │
│                          │                                          │
│           ┌──────────────┼──────────────┐                           │
│           ▼              ▼              ▼                           │
│     ┌──────────┐  ┌────────────┐  ┌──────────┐                      │
│     │ Suggest  │  │ Classifier │  │ TTS      │ Pollux only      │
│     │ to UI    │  │ (handoff)  │  │ (cloned  │                      │
│     │ (sidebar)│  │            │  │  voice)  │                      │
│     └──────────┘  └────────────┘  └────┬─────┘                      │
│      (Athena)         │              │                            │
│                         │              ▼                            │
│                         │       ┌──────────────┐                    │
│                         │       │ HAL plug-in  │ Pollux only     │
│                         │       │ (virtual mic │                    │
│                         │       │  with source │                    │
│                         │       │  swap)       │                    │
│                         │       └──────┬───────┘                    │
│                         │              │                            │
│                         ▼              ▼                            │
│                   ┌──────────────────────────┐                      │
│                   │ Notify (sidebar / alert) │                      │
│                   └──────────────────────────┘                      │
└─────────────────────────────────────────────────────────────────────┘

Clio:  Audio in → Transcribe → Vault. UI shows transcript + post-meeting summary.
Athena:  + Realtime LLM + Suggest-to-UI sidebar + Classifier (suggests, not acts).
Pollux: + Classifier-acts + TTS + HAL plug-in + multi-meeting routing.
```

---

## Legal / compliance

Risk increases sharply across the modes. Each mode has its own consent flow and onboarding step.

| Surface | Clio | Athena | Pollux |
|---|---|---|---|
| Recording consent (any jurisdiction) | required | required | required |
| Two-party-consent disclosure (CA, FL, IL, MD, MA, MT, NH, PA, WA, EU) | auto-on | auto-on | auto-on (overrides default-OFF) |
| AI-assistance notice | optional | required | required |
| Voice biometrics (BIPA, GDPR Art. 9) | — | — | required, multi-step |
| Right-to-be-forgotten | transcript delete | + sidebar history | + voice ID delete (cascades to provider) |
| Legal sign-off as launch gate | recording-consent text only | + AI notice | + voice biometrics + impersonation |

Engineering ships the technical primitives. Launch readiness for each mode includes its own legal sign-off.

---

## Open questions

### Cross-mode
1. **Mode picker UX**: per-meeting selector? Per-persona default? Onboarding selects a default mode? Resolve before P5.
2. **Mode-switch within a session**: can a user start in Clio and "promote" to Athena mid-meeting? Likely yes; needs UX. Resolve in P3.
3. **Tencent Meeting / WeMeet support**: works for Clio (capture is platform-agnostic); per-platform AX work needed for speaker attribution. Defer to post-beta.
4. **Cross-platform Windows/Linux**: Clio and Athena are most portable (no virtual mic needed); Pollux requires platform-specific routing (Windows: `IAudioPolicyConfigFactory` private API; Linux: PipeWire native). Out of scope for v1.

### Pollux-specific
5. **Disclosure default direction** (locked OFF with TPC override; reviewers preferred ON): user owns this call. Re-evaluate after Athena ships and we have real adoption data.
6. **TTS provider** (S0.1): ElevenLabs, OpenAI, or local model.
7. **HAL plug-in distribution** (S0.4): admin install + `coreaudiod` restart UX flow.
8. **Notification fan-out**: desktop only vs. watch + phone. Defer to P3 retrospective.
9. **AX-driving Zoom's mic picker**: brittle but eliminates the user-side device-selection step. Investigate post-P4 if friction warrants.

### Athena-specific
10. **Sidebar surface**: in heron's existing window vs. floating overlay (Cluely) vs. Watch. v1 ships sidebar; overlay + Watch are post-MVP.
11. **Suggestion density**: how often does the LLM speak up? Tunable; defaults to "high signal only" (≤ 1 suggestion per 30 s).
