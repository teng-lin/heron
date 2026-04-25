# v2 bot driver: build vs buy decision

**Status:** Accepted (provisional — re-evaluate after spike)
**Date:** 2026-04-25
**Scope:** `heron-bot` driver implementation choice for the v2 pivot
([`architecture-agent-participant.md`](./architecture-agent-participant.md)).
**Supersedes:** N/A (first decision in this area)

This doc records the decision so it doesn't get re-litigated in 3 months.
It is intentionally short on novelty (everything here is in conversation
history) and long on consequence (what we're committing to and what
triggers re-opening).

---

## Decision

**Spike on Recall.ai for v2.0; plan to migrate to a native macOS path
(Native Zoom SDK + WKWebView for Meet/Teams) for v2.1.** Two-phase,
explicitly sequenced. The trait surface ([`api-design-spec.md`](./api-design-spec.md))
was designed so the migration is a one-crate replacement, not a
workspace-wide ripple.

If the spike reveals the *product hypothesis itself* is wrong (proxy
mode isn't the use case people want), pivot to whisper-assistant or
chat-assistant mode and skip the bot driver entirely — see §5.

## Context

The v2 architecture proposes that heron joins meetings on the user's
behalf as a participant. That requires a "bot driver" — the layer that
mechanically joins a Zoom/Meet/Teams call, captures audio, plays
synthesized speech back. Four paths exist; the choice is load-bearing
because the bot driver is the most vendor-coupled layer in the four-layer
spec, and the wrong choice locks in years of either privacy compromises
(hosted vendors) or engineering tax (rolling your own).

The decision was deferred until the API-design spec (§13 "Next steps")
named the bot driver as the gating decision before any implementation
work could begin.

## Audit summary — what heron has today

The codebase is **halfway** to a bot driver, but it has the wrong half:

| Capability | Status | Notes |
|---|---|---|
| System audio capture (Core Audio process tap) | ✅ real | `heron-audio`, 48kHz mono, 10ms frames, WebRTC AEC |
| Mic capture | ✅ real | cpal, tolerates TCC denial |
| AXObserver READ on Zoom | ✅ scaffolded | `heron-zoom`, no DRIVE primitives |
| EventKit calendar reads | ✅ real | `heron-vault::calendar` |
| HTTP client (reqwest) | ✅ production | `heron-llm::anthropic` |
| Batch STT (WhisperKit, Sherpa) | ✅ real | `heron-speech` |
| v2 trait scaffolds | ✅ scaffolded | `heron-bot`, `heron-bridge`, `heron-policy`, `heron-realtime` |
| **Audio playback into a meeting** | ❌ **missing** | No virtual audio device, no AVAudioEngine output |
| **TTS** | ❌ **missing** | Zero infrastructure |
| **Streaming STT** | ❌ **missing** | All STT today is batch |
| **Realtime LLM** | ❌ **missing** | No WebSocket client anywhere |
| **Zoom UI driving** | ❌ **missing** | AXObserver reads only |
| **Browser automation** | ❌ **missing** | Zero |

The hard half (playback, TTS, realtime LLM) is missing. The capture
half is built. Any path forward inherits this asymmetry.

## Options considered

### Path A — Recall.ai (hosted)

| Dimension | Value |
|---|---|
| Time-to-spike | 1–2 weeks |
| Time-to-ship | 4–6 weeks |
| Privacy | ❌ Recall sees every meeting that flows through |
| AGPL fit | ❌ Hosted-SaaS dependency contradicts the network-copyleft rationale that drove the relicense |
| Reuses heron-* | ~10% (only `heron-vault` for notes) |
| Cost | ~$0.69/hr/bot |
| Vendor lock | high — Recall's API surface is unique |

### Path B — Attendee (self-hosted OSS)

| Dimension | Value |
|---|---|
| Time-to-spike | 1 wk + Linux server provisioning |
| Time-to-ship | 4–8 wk + ongoing ops burden |
| Privacy | ✅ self-host |
| AGPL fit | ⚠️ License OK, but "Mac app + Linux Docker stack" is operationally awkward |
| Reuses heron-* | ~10% |
| Cost | ~self-host (idle Linux VM) |
| Vendor lock | low |

### Path C — Native Zoom SDK + WKWebView for Meet/Teams

| Dimension | Value |
|---|---|
| Time-to-spike | 4–8 weeks (Zoom SDK alone) |
| Time-to-ship | 12–20 weeks (full multi-platform) |
| Privacy | ✅✅ everything local |
| AGPL fit | ✅ true to ethos |
| Reuses heron-* | ~50% — `heron-audio` capture pipeline + `heron-zoom` AXObserver are exactly what the Zoom SDK plumbs |
| Cost | Zoom SDK license; ongoing maintenance of Meet/Teams DOM scrapers |
| Vendor lock | none (per platform) |

### Path D — WKWebView for all three (Vexa-style on Mac)

| Dimension | Value |
|---|---|
| Time-to-spike | 4–8 weeks |
| Time-to-ship | 16–24 weeks + perpetual DOM-scraping tax |
| Privacy | ✅ all local |
| AGPL fit | ✅ |
| Reuses heron-* | ~30% |
| Cost | engineering only, but ongoing |
| Vendor lock | none, but each platform breaks every few weeks |

## Why Path A first, Path C second

Three reasons drive the sequenced both:

1. **The product hypothesis isn't validated yet.** "AI agent that joins
   meetings on your behalf and speaks" is either a real product or it
   isn't. Spending 3–5 months on native Zoom SDK before knowing whether
   the feature is useful would be the worst possible sequencing.
   Recall buys a working prototype in two weeks. If the prototype reveals
   nobody wants this, no one cares about which bot driver was used.
2. **The differentiated value lives above the driver.** It's in
   `heron-policy` (when to speak, what's allowed), `heron-realtime`
   (turn-taking, barge-in), and persona/voice (the user's identity).
   Every shipped speaking-bot product (Vexa, MeetingBaaS, Attendee
   voice-agent) uses roughly the same driver pattern; the policy layer
   is what they punt on. The driver is commodity; the policy is the
   product. Building the driver first inverts the priority.
3. **The migration path is genuinely cheap if Invariant 1 holds.**
   The spec's Invariant 1 — "vendor quirks live ONLY in `heron-bot`" —
   exists for exactly this scenario. `RecallDriver` and
   `NativeZoomDriver` both implement the same `MeetingBotDriver` trait;
   swapping is a per-crate replacement, not a workspace-wide change.
   Spike + migrate is the right shape; lock in to Recall is not.

The one place this could go wrong is if the spike reveals Recall
*can't* honor a spec invariant (e.g., disclosure timing per §4 may not
be possible if Recall's `bot_create → output_audio` pipeline is too
slow). In that case, Path C becomes the only option and the spike's
budget is wasted. The risk is real but bounded — 1–2 weeks of work,
not months.

## Why not Recall permanently

Heron's whole positioning is incompatible with Recall as the long-term
spine:

- AGPL was chosen for **network copyleft** — anyone running heron as
  a hosted service must publish modifications. That choice only
  matters if heron is local-first; if Recall is the canonical bot
  driver, heron is *already* hosted-by-proxy and the AGPL choice was
  cosmetic.
- $0.69/hr/bot kills the personal-assistant use case. An enterprise
  product can absorb the cost; a personal Mac app can't, and the
  whole positioning is "private, on-device, your data on your
  machine."
- Recall sees every meeting that flows through. That's an unfixable
  privacy violation for the stated product.

Recall is a fine validation harness. It is not a product spine.

## Why not Attendee

Attendee solves the privacy problem (self-host), but introduces an
operational problem heron doesn't want: a Linux VM running a Chromium
farm, somewhere. The whole point of heron-as-Mac-app is "install via
Homebrew, runs on your laptop." A required Linux side-car contradicts
that.

If Attendee shipped a macOS-native or single-binary distribution, it
would be a contender. Today it doesn't.

## Why not Path D (WKWebView for all three)

DOM scraping is a forever tax. Vexa's `googlemeet/recording.ts` is 906
lines of Node + injected page-context JS that breaks every few weeks
when Meet ships a UI change. Heron does not have the engineering
budget to perpetually chase Meet/Teams/Zoom-Web DOM updates as a
solo/small-team project.

For Zoom specifically the native SDK avoids this entirely. For Meet
and Teams there's no equivalent native SDK, so WKWebView (or partner
with Recall/Attendee per-platform) is the only option — but **only
for those two platforms**, not all three.

## The product-shape question (not bypassed by this decision)

The above assumes the product is "AI agent attends meetings when the
user isn't present" (proxy mode). That assumption deserves to be
challenged before any spike begins. Three adjacent products don't need
a bot driver at all:

| Mode | Description | Bot driver needed? | New code estimate |
|---|---|---|---|
| Voice proxy | Agent attends in user's absence | ✅ | months |
| Voice co-pilot | User in call; agent speaks via user's mic when summoned | ✅ (audio playback only) | weeks |
| **Whisper assistant** | User in call; agent speaks to user via headphones (NOT into meeting) | ❌ | 2–4 weeks |
| **Chat assistant** | User in call; agent drafts messages user posts to chat | ❌ | 2–4 weeks |

The two no-bot-driver paths reuse 80–90% of existing heron
infrastructure (heron-audio captures, heron-speech transcribes,
heron-llm summarizes — all already built). They could ship in 2–4
weeks total. They are genuine alternative products, not consolation
prizes.

**This decision does not commit to proxy mode.** The Recall spike's
*first* job is to validate the product, not to deliver it. If the
spike result is "users actually wanted whisper-assistant," the right
follow-up is to skip the bot driver entirely, not to optimize the
choice between Recall and native.

## Reversibility — when to revisit

This decision should be re-opened if any of the following:

1. **Recall spike reveals a spec-invariant violation** (e.g., disclosure
   timing per §4 not achievable). → Skip to Path C immediately.
2. **Recall pricing changes materially.** Current ~$0.69/hr/bot is
   already too high for personal use; any increase makes Path A
   non-viable even as a spike harness.
3. **Native Zoom SDK terms change** in a way that blocks the migration
   (license cost, distribution restrictions, end-of-life).
4. **Apple ships first-party meeting integration** (e.g., a public
   AVCallKit-style API for meeting apps). Would change Path C's
   feasibility radically.
5. **A meaningful cross-platform OSS bot driver appears** (someone
   ports Vexa to macOS, or Attendee ships a single-binary Mac
   distribution).
6. **User research from the spike invalidates proxy mode** as the
   product. → Switch to whisper / chat mode; skip the bot driver
   migration entirely.

Re-evaluation cadence: after the Recall spike completes (estimated
2–3 weeks from start), and again at v2.0 → v2.1 transition.

## Migration plan (if both spike + product hypothesis succeed)

**v2.0** (Path A — Recall):
- Implement `RecallDriver: MeetingBotDriver` against the trait surface
  in `crates/heron-bot/src/lib.rs`.
- Wire through `heron-policy`, `heron-realtime`, persona/voice.
- Ship to a small alpha cohort.
- Measure: which spec invariants Recall honors; how often the bot is
  ejected; latency budget for disclosure injection; user feedback on
  proxy mode.

**v2.1** (Path C — Native Zoom SDK + WKWebView):
- Implement `NativeZoomDriver: MeetingBotDriver` (Zoom SDK + Swift
  bridge).
- Implement `WkWebViewDriver: MeetingBotDriver` for Meet and Teams.
- Add a `select_driver(platform: Platform) -> Box<dyn MeetingBotDriver>`
  router to `heron-cli`.
- Sunset `RecallDriver` once the native paths are at parity.
- Trait surface stays unchanged; this is per-crate work.

Estimated total: v2.0 in 1–2 months from spike start; v2.1 in 4–6
months after v2.0 alpha lands.

## Open follow-ups

These don't block the spike but should be answered before v2.1:

- **Native Zoom SDK licensing terms** for an AGPL'd consumer Mac app —
  is that even compatible? Need to read the SDK license carefully.
- **Meet/Teams partner option** — if WKWebView turns out to be
  intractable for Meet/Teams (DOM-scraping fragility), is there an
  Attendee-style or Recall-style "native client wrapper" we can
  embed without taking a Linux dependency?
- **Voice clone backend choice** (ElevenLabs vs Cartesia vs Piper vs
  WhisperKit-companion). Independent of the driver decision but
  blocks v2.0 ship. Probably its own decision doc.
- **Whisper-assistant vs chat-assistant prototyping** — if proxy mode
  flops in the spike, we want a whisper-assistant prototype within
  another 2–4 weeks. Worth scoping in parallel with the Recall spike.

## References

- [`docs/architecture-agent-participant.md`](./architecture-agent-participant.md) — the v2 architecture this decision serves
- [`docs/api-design-spec.md`](./api-design-spec.md) — the invariants the chosen driver must honor
- [`docs/api-design-research.md`](./api-design-research.md) — vendor capability matrices
- [`docs/agent-participation-research.md`](./agent-participation-research.md) — product-category survey
- `crates/heron-bot/src/lib.rs` — trait surface the driver implements
- Conversation thread (turn: "should we roll our own instead of using Recall.ai or attendee's API?") — full audit + tradeoffs
