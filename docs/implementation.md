# heron v1 implementation plan

Concrete execution layer. `plan.md` is "what and why" — this file is
"how, in what order, and what to verify." Written so a coding agent
(or a focused engineer) can execute weeks in sequence.

The authoritative scope is `plan.md`. If this file conflicts with
`plan.md`, `plan.md` wins and this file gets updated.

**Revision history.**
- v1: initial 12-week plan.
- v2 (post-momus 3-agent review #1): timeline 16 weeks; week 0
  expanded; WhisperKit bridge spike added; merge-on-write spec'd;
  prerequisites enumerated; live-call gates replaced with fixtures.
- v3 (post-momus 3-agent review #2): LLM ID-continuity contract added
  (prompt-side preservation); §6.3 AEC test rig fixed (partner-side
  noise playback, exact osascript); §3.3 Day 1 AX traversal recipe
  inlined as Swift code; Swift bridge reference pattern inlined with
  actual `Package.swift` / `@_cdecl` / `build.rs`; cross-references
  audited and fixed; §10.4 whitespace-in-codeblock normalization;
  partner schedule extended to weeks 2 and 7; §0.8 Accessibility
  permission and Inspector walkthrough; manual test matrix
  consolidated into `docs/manual-test-matrix.md`; ship criteria
  reconciled across plan.md and implementation.md.
- v3.1: dropped Tart VM in favor of TCC-reset workflow on the
  development laptop. Onboarding test environment is the author's
  Mac; `tccutil reset` between walkthroughs simulates first-run.
  Real "naive user" coverage moves to the week-16 exec dogfood.

---

## 0. Prerequisites

Lock these before day 1. Every failure-to-provision discovered mid-week
costs half a day. **Provisioning a missing prerequisite mid-build is
the single most common cause of slip in this plan.**

### 0.1 Toolchain

| Resource | Spec | Check command |
|---|---|---|
| macOS | 14.2+ (Sonoma or later) | `sw_vers -productVersion` |
| Architecture | Apple Silicon preferred (M-series) | `uname -m` (expect `arm64`) |
| Rust | stable 1.82+, 2024 edition | `rustc --version` |
| Xcode | 16+ with command-line tools | `xcodebuild -version && xcrun --find swiftc` |
| Node | 20 LTS (for Tauri) | `node --version` |
| pnpm | 9+ | `pnpm --version` |
| ffmpeg + ffprobe | latest | `ffprobe -version` |
| Python 3.11+ + pip | for AEC test correlation | `python3 --version && python3 -m pip --version` |
| Python deps | `scipy`, `soundfile`, `numpy` | `python3 -m pip install -r requirements-dev.txt` |
| sox | for synthetic fixture generation | `sox --version` |

### 0.2 Apple developer setup

| Resource | Notes | Check |
|---|---|---|
| Apple Developer Program | $99/year | login at developer.apple.com |
| Developer ID Application cert | for code-signing | `security find-identity -v -p codesigning \| grep "Developer ID Application"` |
| AppStoreConnect API key | for `notarytool` (preferred over ASP) | downloadable `.p8` from AppStoreConnect → Users and Access → Keys; saved at `~/.private/notarize-key.p8` |
| App-specific password | fallback only; expires annually | manual at appleid.apple.com |
| Hardened-runtime entitlements file | enumerated below; committed at `apps/desktop/src-tauri/entitlements.plist` | see §0.6 |

### 0.3 Audio routing for spike + fixture capture

The week-0 spike and week-3 fixture capture need to record *isolated
mic* and *isolated Zoom output* simultaneously, which macOS does not
provide natively. Two options:

- **BlackHole 2ch** + **Multi-Output Device** (recommended; free).
  Setup: install BlackHole, open Audio MIDI Setup, create a
  Multi-Output Device combining BlackHole + your normal speakers. Set
  Zoom's speaker output to the Multi-Output Device. QuickTime "New
  Audio Recording" selects BlackHole as input → captures Zoom-only.
  Mic captured via separate QuickTime instance with mic as input.
  **You will hear Zoom through your speakers via the Multi-Output
  passthrough.**

- **Loopback by Rogue Amoeba** ($109; faster setup; per-app routing UI).

Document the active config at `docs/spike-rigging.md` so the partner
side can replicate.

### 0.4 Onboarding test environment — TCC-reset workflow on this laptop

We do **not** use a VM. Onboarding is validated on the development
machine by resetting TCC permissions between walkthroughs to simulate
a first-run state.

```sh
# Reset all heron-relevant TCC grants:
tccutil reset Microphone com.heronnote.heron
tccutil reset AudioCapture com.heronnote.heron
tccutil reset Accessibility com.heronnote.heron
tccutil reset Calendar com.heronnote.heron

# Also clear settings + caches to simulate a fresh install:
rm -rf ~/Library/Preferences/com.heronnote.heron.plist
rm -rf ~/Library/Application\ Support/com.heronnote.heron
rm -rf ~/Library/Caches/heron
```

Wrapper script committed at `scripts/reset-onboarding.sh`.

**Coverage limits — explicitly accepted.**
- This does NOT simulate a fresh user account or a fresh Mac. The
  author's machine has dev tools, network access, hardware peripherals,
  and an existing Apple ID session.
- Real naive-user coverage (UX confusion, copy clarity, default
  behavior on a stock Mac) moves to **week 16 exec dogfood**. Bugs
  surfacing there are accepted v1.1 candidates if they're not
  blocking the §18.2 ship criteria.

### 0.5 External services / credentials

| Resource | Notes |
|---|---|
| Anthropic API key | paid tier with prompt caching; quota tested in week 1 |
| HuggingFace account | for WhisperKit model download (no auth required for openai/whisper-* but rate-limited) |
| Claude Code CLI | installed for opt-in backend test (optional but recommended) |
| Codex CLI | optional alternate backend |
| GitHub repo | `heronnote/heron` (private) with `secrets.APPLE_API_KEY_BASE64`, `APPLE_API_KEY_ID`, `APPLE_API_ISSUER` set |

### 0.6 Hardened-runtime entitlements (committed at week 1)

```xml
<!-- apps/desktop/src-tauri/entitlements.plist -->
<dict>
  <key>com.apple.security.app-sandbox</key><false/>
  <key>com.apple.security.cs.allow-jit</key><true/>
  <key>com.apple.security.cs.disable-library-validation</key><true/>
  <key>com.apple.security.device.audio-input</key><true/>
  <key>com.apple.security.temporary-exception.mach-lookup.global-name</key>
  <array>
    <string>com.apple.audio.audiohald</string>
  </array>
</dict>
```

Plus `Info.plist` usage strings in `tauri.conf.json`:
- `NSMicrophoneUsageDescription` — "heron records your voice during meetings."
- `NSCalendarsUsageDescription` — "heron auto-fills attendees from your calendar (optional)."
- `NSAudioCaptureUsageDescription` — "heron captures audio from your meeting app."
- `LSUIElement = true` — menubar-only app.

### 0.7 Test partners (scheduled BEFORE week 0)

A 17-week build with diarization + AEC fixtures requires a
**scheduled human collaborator** for the following blocks. Confirm
availability and lock calendar slots before week 0 starts.

| Phase | Time | Activity |
|---|---|---|
| Week 0 | 4–6 hours total across 5 short sessions | AX edge-case calls (gallery, active-speaker, paginated, dial-in, shared-screen, tile-rename) |
| **Week 2** | 1 hour | **AEC test rig** (§6.3) — partner is on a Zoom call with the engineer and runs an `osascript` to play the test noise from their machine. Required so `tap.wav` actually contains the noise. |
| Week 3 | 2 hours | Three reference call captures for fixture corpus |
| **Week 7** | 1 hour | **Live regression call recorded as a fixture** (§9.5). 4-person Zoom call with author + 3 partners; recorded and labeled post-hoc. |
| Week 16 | full week | Exec-friend dogfood |

**Schedule the partner blocks before week 0 begins.** A dropped slot
cascades 1–2 days of slip per occurrence.

**Partner machine prereqs for week 2 (AEC test).** The partner needs
either:
- **Zoom with "Share Computer Sound" enabled** (default Zoom feature;
  no extra software). Used in §6.3 Option A. Simplest path.
- **BlackHole 2ch + a Multi-Output Device** routed per §6.3 Option B.
  Used if the partner can't share-screen during the test for some
  reason.

Confirm with the partner which option they can run **before** the
week-2 slot. A 30-min dry-run on Mac A (per §6.3 pre-test smoke) the
day before the real test catches partner-rig errors cheaply.

### 0.8 Accessibility permission for AX probe

Before `swift/ax-probe/` can register notifications on Zoom, the
binary running it (Terminal, or the probe binary directly) must be
granted **Accessibility** in System Settings → Privacy & Security →
Accessibility. This **cannot be programmatically prompted** (Apple
gates Accessibility behind explicit user opt-in).

Day-0 step:
1. Open System Settings → Privacy & Security → Accessibility.
2. Click `+`, navigate to `/Applications/Utilities/Terminal.app` (if
   running probe from Terminal) or to `target/ax-probe` once built.
3. Enable the toggle. Re-launch the host app.
4. Verify in code: `AXIsProcessTrusted()` returns `true`.

**Recommended:** install Apple's **Accessibility Inspector** (ships
with Xcode at `Xcode → Open Developer Tool → Accessibility Inspector`).
On day 1 of the spike, point Inspector at Zoom's gallery view and
visually inspect the tile/indicator hierarchy before writing any
Swift. ~30 minutes saves ~half a day of guesswork.

### 0.9 Cross-cutting prerequisite: shared clock utility

Audio frames timestamp from `mach_absolute_time` (Core Audio host
clock). AX events timestamp from a Cocoa run-loop wall clock. The
spike (§3) and the aligner (§9) need to convert between them. Add
`crates/heron-types/src/clock.rs` in week 1 with:

```rust
pub struct SessionClock {
    pub started_at: SystemTime,
    pub mach_anchor: u64,
    pub mach_timebase: TimebaseInfo,
}

impl SessionClock {
    pub fn host_to_session_secs(&self, host_time: u64) -> f64;
    pub fn wall_to_session_secs(&self, wall: SystemTime) -> f64;
}
```

The week-0 spike binary embeds this utility; otherwise the spike
measurements are noise.

---

## 1. Day-0 bootstrap (4–6 hours)

### 1.1 Workspace layout

```
heron/
├── Cargo.toml                  (workspace)
├── rust-toolchain.toml
├── requirements-dev.txt        (scipy, soundfile, numpy)
├── .gitignore                  (already done)
├── .github/
│   └── workflows/
│       ├── rust.yml            (build + test + clippy on every PR)
│       └── notarize.yml        (notarize on tag push; week 2)
├── crates/                     (heron-types, audio, speech, zoom, llm, vault, cli)
├── apps/desktop/               (Tauri v2; week 11)
├── scripts/                    (spike-aec.sh, spike-backpressure.sh, etc.)
├── fixtures/                   (ax/, speech/, zoom/, synthetic/, manual-validation/)
├── docs/                       (plan.md, implementation.md, observability.md, etc.)
└── swift/                      (ax-probe, eventkit-helper, whisperkit-bridge, keychain-helper, zoom-ax-backend)
```

### 1.2 Root `Cargo.toml`

```toml
[workspace]
resolver = "2"
members = ["crates/*", "apps/desktop/src-tauri"]

[workspace.package]
version = "0.1.0"
edition = "2024"
rust-version = "1.82"
license = "UNLICENSED"

[workspace.dependencies]
tokio = { version = "1.40", features = ["full"] }
async-trait = "0.1"
anyhow = "1"
thiserror = "2"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["json", "env-filter"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
serde_yaml = "0.9"
schemars = "0.8"
chrono = { version = "0.4", features = ["serde"] }
uuid = { version = "1", features = ["v7", "serde"] }

# macOS bindings — VERSION-PINNED, see §1.3
cidre = "=0.5.3"
cpal = "0.15"
swift-rs = "1.0"
security-framework = "3"
core-foundation = "0.10"

# audio processing
hound = "3.5"
webrtc-audio-processing = "0.4"
rtrb = "0.3"
rubato = "0.16"
symphonia = "0.5"

# HTTP / LLM
reqwest = { version = "0.12", features = ["json", "stream", "rustls-tls"] }
eventsource-stream = "0.2"

# templating / markdown
handlebars = "6"
yaml-front-matter = "0.1"
pulldown-cmark = "0.12"

# fuzzy text match (for v1.1 ID-resolution fallback; bundled in v1
# but unused unless prompt-side ID preservation fails frequently)
strsim = "0.11"

heron-types = { path = "crates/heron-types" }
heron-audio = { path = "crates/heron-audio" }
heron-speech = { path = "crates/heron-speech" }
heron-zoom = { path = "crates/heron-zoom" }
heron-llm = { path = "crates/heron-llm" }
heron-vault = { path = "crates/heron-vault" }

[profile.dev]
opt-level = 1
```

### 1.3 Dependency risk pins

- `cidre`: pre-1.0; `AudioHardwareCreateProcessTap` API surface added
  in 0.5.x and changes between point releases. **Pin exact (=0.5.3).**
  If bumping, dedicate a half-day to re-validate.
- `webrtc-audio-processing`: known macOS arm64 build issues with
  certain `webrtc-audio-processing-sys` versions. **Build verified at
  v0.4.x in week 1**; if broken, fall back to `webrtc-audio-processing-rs`
  fork. Document in `docs/backend-evaluations.md`.
- Anthropic Rust SDK does not exist at production-grade 0.x. **Decide
  in week 9** between (a) bare `reqwest` + thin wrapper, (b) the
  `anthropic-sdk-rs` community crate, (c) `misanthropic`. Default
  plan: bare `reqwest`.

### 1.4 CI skeleton (`.github/workflows/rust.yml`)

- Runs on `push` + `pull_request`.
- Matrix: `macos-14` only.
- Steps: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings
  -D clippy::unwrap_used -D clippy::expect_used`, `cargo test --all`,
  `cargo build --release`.
- Cache: `~/.cargo/registry`, `target/` via `actions/cache`.
- Notarization workflow (`notarize.yml`) set up week 2.

### 1.5 Bootstrap acceptance

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --all -- -D warnings -D clippy::unwrap_used
python3 -m pip install -r requirements-dev.txt
python3 -c "import scipy, soundfile, numpy; print('python deps OK')"
```

Commit: `bootstrap: workspace skeleton, CI, toolchain pins`.

---

## 2. Dependency graph / critical path

```
week 0     ax-probe spike (5 days) ────┐
                                       │
week 0.5   whisperkit-bridge spike ────┤
           (3 days, parallel)          │
                                       ▼
week 1     heron-types + clock + entitlements + TCC reset workflow
                                       │
                                       ▼
week 2     heron-audio skeleton + AEC gate (with partner) + notarization
                                       │
                                       ▼
week 3     heron-audio complete + fixtures + backpressure
                                       │
                                       ▼
weeks 4–5  heron-speech (both backends)
                                       │
                                       ▼
weeks 6–7  heron-zoom + aligner ◄── needs week-0 fixtures w/ ground-truth
                                       │
week 8     merge-on-write spike (5 days, including LLM contract)
                                       │
week 9     heron-llm (with ID-preservation prompt) + m4a pipeline
                                       │
week 10    heron-vault + calendar + ringbuffer purge
                                       │
week 11    Tauri shell + 5-step onboarding (laptop TCC-reset)
                                       │
week 12    recording UX + crash recovery + WhisperKit DL UX
                                       │
week 13    review UI (TipTap + playback + diagnostics)
                                       │
week 14    settings + polish + bug-fix buffer
                                       │
week 15    personal dogfood + bug fixes
                                       │
week 16    exec-friend dogfood + ship gate
```

**Critical path is week 0 → 0.5 → 2 → 6–7.** If week 6–7 slips, week
13 has no meaningful content. Do not skip fixture capture in weeks
0/3 — they unblock fixture-based regression for every subsequent
week.

---

## 3. Week 0: AXObserver feasibility spike (5 days)

### 3.1 Goal
Produce a filled-in numeric threshold table (per `plan.md` §5 week 0),
a go/no-go memo, **and all fixture artifacts that weeks 6–7 depend
on, including turn-level ground-truth.**

### 3.2 Deliverables

- `swift/ax-probe/` — Swift CLI binary. Reusable: same binary runs
  during week-3 fixture capture to emit `ax-events.jsonl`.
- `fixtures/zoom/spike-report.md` — completed threshold table + memo.
- `fixtures/zoom/<case>/` directories — one per edge case (gallery-
  baseline, active-speaker, paginated, dial-in, shared-screen,
  tile-rename, gallery-baseline-old-zoom):
  - `mic.wav` + `tap.wav` (synced clap pulse at start; see §3.5).
  - `ax-events.jsonl` from the probe.
  - `ground-truth.jsonl` — turn-level labels (required by **§9.4
    aligner regression tests**).
  - `README.md` — meeting metadata.
- `fixtures/synthetic/` — white-noise burst + clap-impulse files used
  by the **AEC test (§6.3)** and clock-alignment verification (§7.6
  done-when).

### 3.3 Day-by-day

**Day 1 — probe scaffold + AX recipe (concrete).**

The previous review (v2) just said "walk Zoom's a11y tree." This
inlined recipe gives a developer a working starting point.

```swift
// swift/ax-probe/main.swift — Day 1 starting recipe
import ApplicationServices
import AppKit

guard AXIsProcessTrusted() else {
    print("Accessibility not granted; see implementation.md §0.8"); exit(1)
}

let zoomApps = NSWorkspace.shared.runningApplications
    .filter { $0.bundleIdentifier == "us.zoom.xos" }
guard let zoom = zoomApps.first else {
    print("Zoom not running"); exit(1)
}
let app = AXUIElementCreateApplication(zoom.processIdentifier)

// Helper: read an AX attribute as String (or "")
func attr(_ el: AXUIElement, _ name: String) -> String {
    var out: AnyObject?
    AXUIElementCopyAttributeValue(el, name as CFString, &out)
    return (out as? String) ?? ""
}

// BFS tree dump up to depth 12, printing role/subrole/title/id/desc.
func walk(_ el: AXUIElement, depth: Int, max: Int) {
    if depth > max { return }
    let role = attr(el, kAXRoleAttribute as String)
    let sub  = attr(el, kAXSubroleAttribute as String)
    let t    = attr(el, kAXTitleAttribute as String)
    let i    = attr(el, kAXIdentifierAttribute as String)
    let d    = attr(el, kAXDescriptionAttribute as String)
    print("\(String(repeating: "  ", count: depth))[\(role)/\(sub)] title='\(t)' id='\(i)' desc='\(d)'")
    var children: AnyObject?
    AXUIElementCopyAttributeValue(el, kAXChildrenAttribute as CFString, &children)
    if let kids = children as? [AXUIElement] {
        for k in kids { walk(k, depth: depth + 1, max: max) }
    }
}

walk(app, depth: 0, max: 12)
```

**Searching for the speaking indicator.** Run the dump above with
Zoom in gallery view. Inspect (or grep) the output for:

1. **Tile container.** Likely an `AXGroup` / `AXScrollArea` with
   `AXTitle` matching "Gallery" or whose immediate `AXChildren` are
   ~9 elements with `AXTitle`s matching participant display names.
2. **Speaking indicator.** A child of each tile with one of:
   - `kAXIdentifier` containing `speaker`, `active`, or `audio`
   - `kAXSubrole` `AXImage` with `kAXDescription` containing
     `speaking`
   - `kAXValueAttribute` toggling 0↔1 when the participant talks
     (use **Accessibility Inspector** to watch live values)

Try **`AXObserverAddNotification` for `kAXValueChangedNotification`**
on each candidate. The element that fires on speech is the answer.
Record the discovered `AXRole`/`AXSubrole`/`AXIdentifier` triple in
`fixtures/zoom/spike-report.md` so week 6 can re-find it.

If `AXObserverAddNotification` returns an error consistently, switch
to polling (`AXUIElementCopyAttributeValue` every 50ms) — same call,
just from a timer.

**Day 2 (gallery baseline + core fixtures).** 4-person test call,
gallery view, 15 min. Sync pulse at t=0 (§3.5). Ground-truth
labeling per §3.4 workflow.

**Day 3 (edge cases, batch 1).** Active-speaker view 5 min, paginated
gallery (>9 participants if mustered) 5 min.

**Day 4 (edge cases, batch 2 + Zoom version compare).** Dial-in
(call your cell), shared-screen, tile-rename, repeat gallery-
baseline against an older Zoom version (DMG archived at
`~/.private/zoom-prior.dmg`; pre-stage in §0 setup).

**Day 5 (polling CPU + memo).** Run probe in `--poll 50` for 30 min
under simulated CPU load. Compute every metric in `plan.md` §5
week-0 threshold table. Write `spike-report.md`.

### 3.4 Ground-truth labeling workflow

The load-bearing time sink in week 0.

**Per fixture (5–15 min audio):**
1. Run `whisper.cpp` `medium.en` on the tap channel.
2. Open the auto-transcript in `scripts/label-fixture.sh` (a TUI;
   shows audio waveform + auto-text + speaker dropdown).
3. Play, pause, correct text, assign speaker per turn.
4. Save as `ground-truth.jsonl`.

**Time per fixture: 3–4 hours for 5 minutes of audio.** First fixture
budget 6 hours (tooling friction); subsequent fixtures 3–4 hours.

**Human-in-loop tag.** Final `ground-truth.jsonl` files committed to
git as proof of completion. See `docs/manual-test-matrix.md` for the
canonical artifact list.

### 3.5 Sync pulse for clock alignment

Add `fixtures/synthetic/sync-pulse-880hz-300ms.wav` to the repo
(generated once via `sox -n -r 48000 -c 1 -b 16 sync-pulse-880hz-300ms.wav synth 0.3 sine 880`).
At t=0 of each fixture capture:
- Tap channel: play the sync pulse via Zoom's "Share Computer Sound"
  on a partner's machine (or via the Multi-Output device on your own
  machine).
- Mic channel: simultaneously clap (audible in mic).

In post-processing (`scripts/measure-sync-offset.py`), cross-correlate
the two streams against the known impulse to derive the inter-channel
offset. **This is the reference clock for §7.6 "aligned within 10ms"
verification** and the §9 alignment offset estimation.

### 3.6 Gate

| Outcome | Action |
|---|---|
| Green | Proceed to week 1 as planned. |
| Yellow (modal) | Proceed. Aligner builds against yellow-case fixtures. Polling backend escalated to first-class. |
| Red | Branch to `v1-channel-only` plan: drop `heron-zoom` from v1, ship with `speaker: "them"` only, defer clustering to v1.1. Update `plan.md` §1 quality promise. Schedule slips ~2 weeks. |

Commit: `spike: AXObserver feasibility report; fixtures with ground-truth labeled`.

---

## 4. Week 0.5: WhisperKit Swift bridge spike (3 days)

Run in parallel with week 0 if possible; otherwise immediately after.

### 4.1 Goal
Determine whether a production WhisperKit↔Rust bridge is feasible.
Produces either a working PoC or a "use sherpa-only in v1" decision.

### 4.2 Method

**Day 1 (Swift wrapper).** `swift/whisperkit-bridge/Package.swift`
declares dependency on `WhisperKit` (argmaxinc). Write
`WhisperKitBridge.swift` exposing C-callable functions:

```swift
@_cdecl("whisperkit_create")
public func create() -> OpaquePointer? { /* WhisperKit.init() */ }

@_cdecl("whisperkit_transcribe_chunked")
public func transcribeChunked(
    _ ptr: OpaquePointer,
    _ samples: UnsafePointer<Float>,
    _ count: Int,
    _ callback: @convention(c) (UnsafePointer<CChar>, Float, Float) -> Void
) -> Int32 { ... }
```

**Async-to-sync bridging gotcha.** WhisperKit's `transcribe` is async.
Bridging it to a sync C ABI via `DispatchSemaphore` works **only if
the awaited continuation runs on a different dispatch queue from the
one blocked by `sem.wait()`.** Calling `sem.wait()` on the same queue
the continuation is scheduled on **deadlocks silently**. Rule:
- Run the bridge call from a background `DispatchQueue` (or from
  Rust's tokio blocking pool, which does this automatically).
- Never call the bridge from Swift code already on the main thread
  if the awaited callback also targets `.main`.

Acceptable pattern:
```swift
let sem = DispatchSemaphore(value: 0)
Task.detached {                  // ← detached: own queue
    let result = try await whisperKit.transcribe(samples)
    /* fill out-params */
    sem.signal()
}
sem.wait()                       // safe: caller is on a worker queue
```

**Day 2 (Rust binding).** `crates/heron-speech/build.rs` invokes
`swift build` and links the static lib via `swift-rs`. Rust
`WhisperKitBackend` calls the C ABI with a known WAV.

**Day 3 (model lifecycle).** Bundled `tiny.en` model at
`resources/whisperkit-tiny-en.mlmodelc/`. Test model download
progress callback.

### 4.3 Gate

- **Pass:** PoC transcribes 1-min audio with reasonable WER. Proceed.
- **Fail (build doesn't link, async bridge deadlocks, WER >50% on
  tiny, model load >10s):** flip `plan.md` §2 row 7 to "sherpa-only
  in v1." Drop ~3 days from week 4.

Commit: `spike: whisperkit bridge feasibility result`.

---

## 5. Week 1: foundations + clock + entitlements + TCC reset workflow

### 5.1 Goals
1. `heron-types` v0 locked (vocabulary).
2. Shared `SessionClock` utility usable by all crates.
3. Hardened-runtime entitlements committed.
4. **TCC-reset onboarding workflow validated** on this laptop (so
   week 11 can rely on it).
5. Code-signing identity verified.
6. **EventKit reference Swift bridge** with actual files (canonical
   pattern for all later bridges).
7. `claude -p` smoke + observability spec.

### 5.2 `heron-types` public surface

```rust
// crates/heron-types/src/lib.rs

pub type SessionId = uuid::Uuid;
pub type ItemId = uuid::Uuid;           // v7 — for action_items, attendees

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    pub t0: f64,
    pub t1: f64,
    pub text: String,
    pub channel: Channel,
    pub speaker: SpeakerLabel,
    pub speaker_source: SpeakerSource,
    pub confidence: Option<f64>,
}

#[derive(Serialize, Deserialize)] #[serde(rename_all = "snake_case")]
pub enum Channel { Mic, Tap }

#[derive(Serialize, Deserialize)] #[serde(rename_all = "snake_case")]
pub enum SpeakerSource { Self_, Ax, Channel, Cluster }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionItem {
    pub id: ItemId,             // load-bearing for §10.3 merge
    pub owner: String,
    pub text: String,
    pub due: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attendee {
    pub id: ItemId,             // load-bearing for §10.3 merge
    pub name: String,
    pub company: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frontmatter {
    pub date: chrono::NaiveDate,
    pub start: String,
    pub duration_min: u32,
    pub company: Option<String>,
    pub attendees: Vec<Attendee>,
    pub meeting_type: MeetingType,
    pub source_app: String,
    pub recording: PathBuf,
    pub transcript: PathBuf,
    pub diarize_source: DiarizeSource,
    pub disclosed: Disclosure,
    pub cost: Cost,
    pub action_items: Vec<ActionItem>,
    pub tags: Vec<String>,
    #[serde(flatten)]
    pub extra: serde_yaml::Mapping,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    SessionStarted   { id: SessionId, source_app: String, started_at: DateTime<Utc> },
    SessionEnded     { id: SessionId, ended_at: DateTime<Utc>, duration: Duration },
    MicMuted         { id: SessionId, at: Duration },
    MicUnmuted       { id: SessionId, at: Duration },
    AudioDeviceChanged { id: SessionId, at: Duration, reason: DeviceChangeReason },
    CaptureDegraded  { id: SessionId, at: Duration, dropped_frames: u32, reason: String },
    SpeakerDetected  { id: SessionId, event: SpeakerEvent },
    AttributionDegraded { id: SessionId, at: Duration, reason: String },
    TranscriptPartial { id: SessionId, turn: Turn },
    TranscriptFinal   { id: SessionId, turns_count: usize, path: PathBuf },
    SummaryReady     { id: SessionId, path: PathBuf, cost: Cost },
    SummaryFailed    { id: SessionId, error: String },
    StorageCritical  { id: SessionId, free_bytes: u64 },
}
```

Top-of-`lib.rs` invariant comment: "no event types invented outside
this crate."

### 5.3 `SessionClock` (§0.9)

Implementation in `crates/heron-types/src/clock.rs`. Used by ax-probe
(week 0; rebuilt against this in week 1), heron-audio (week 2),
heron-zoom (week 6), aligner (week 7). Unit tests round-trip
wall-time → session-secs within 1ms.

### 5.4 EventKit reference bridge — actual files

This **is** the reference implementation for every other Swift bridge
(`whisperkit-bridge`, `zoom-ax-backend`, `keychain-helper`). The
files are real here, not described.

**Layout.**
```
swift/eventkit-helper/
├── Package.swift
└── Sources/EventKitHelper/
    └── EventKitHelper.swift
```

**`Package.swift`:**
```swift
// swift-tools-version:5.9
import PackageDescription
let package = Package(
    name: "EventKitHelper",
    platforms: [.macOS(.v14)],
    products: [
        .library(name: "EventKitHelper", type: .static, targets: ["EventKitHelper"]),
    ],
    targets: [.target(name: "EventKitHelper")]
)
```

**`EventKitHelper.swift`:**
```swift
import EventKit
import Foundation

private let store = EKEventStore()

// Returns 1 if granted, 0 if denied.
@_cdecl("ek_request_access")
public func requestAccess() -> Int32 {
    var result: Int32 = 0
    let sem = DispatchSemaphore(value: 0)
    // Detached task: continuation runs on its own queue, so sem.wait()
    // on the caller's queue cannot deadlock. See §4.2 deadlock note.
    Task.detached {
        do {
            let granted = try await store.requestFullAccessToEvents()
            result = granted ? 1 : 0
        } catch { result = 0 }
        sem.signal()
    }
    sem.wait()
    return result
}

@_cdecl("ek_read_window_json")
public func readWindowJSON(
    _ start_unix: Int64,
    _ end_unix: Int64,
    _ out: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>
) -> Int32 {
    let s = Date(timeIntervalSince1970: TimeInterval(start_unix))
    let e = Date(timeIntervalSince1970: TimeInterval(end_unix))
    let predicate = store.predicateForEvents(withStart: s, end: e, calendars: nil)
    let events = store.events(matching: predicate)
    let serialized = events.map { event -> [String: Any] in
        ["title": event.title as Any,
         "start": event.startDate.timeIntervalSince1970,
         "end":   event.endDate.timeIntervalSince1970,
         "attendees": (event.attendees ?? []).map { p -> [String: Any] in
             ["name": p.name as Any, "email": (p.url.absoluteString)] }]
    }
    let json = (try? JSONSerialization.data(withJSONObject: serialized)) ?? Data()
    json.withUnsafeBytes { bp in
        let cstr = strndup(bp.baseAddress!.assumingMemoryBound(to: CChar.self),
                           json.count)
        out.pointee = cstr
    }
    return Int32(events.count)
}

@_cdecl("ek_free_string")
public func freeString(_ p: UnsafeMutablePointer<CChar>?) {
    if let p = p { free(p) }
}
```

**Rust side — `crates/heron-vault/build.rs`:**
```rust
fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let swift_dir = std::path::Path::new(&manifest).join("../../swift/eventkit-helper");
    let status = std::process::Command::new("swift")
        .args(["build", "-c", "release", "--arch", "arm64"])
        .current_dir(&swift_dir)
        .status().expect("swift build");
    assert!(status.success(), "swift build failed for eventkit-helper");
    println!("cargo:rustc-link-search=native={}",
             swift_dir.join(".build/arm64-apple-macosx/release").display());
    println!("cargo:rustc-link-lib=static=EventKitHelper");
    println!("cargo:rustc-link-lib=framework=EventKit");
    println!("cargo:rustc-link-lib=framework=Foundation");
    println!("cargo:rerun-if-changed={}", swift_dir.display());
}
```

**Rust verification call — `crates/heron-vault/src/calendar.rs`:**
```rust
use std::os::raw::{c_char, c_longlong};

extern "C" {
    fn ek_request_access() -> i32;
    fn ek_read_window_json(start: c_longlong, end: c_longlong,
                           out: *mut *mut c_char) -> i32;
    fn ek_free_string(s: *mut c_char);
}

pub fn calendar_has_access() -> bool {
    unsafe { ek_request_access() == 1 }
}
```

**Boundary verification command** (run end of week 1):
```sh
cargo test -p heron-vault calendar_smoke -- --ignored
# Test prints "calendar access: <bool>" — proves Rust → Swift FFI works.
```

**Bridge pattern, condensed.** Other Swift bridges follow this exact
shape: a `Package.swift` with `type: .static`, `@_cdecl` exports
that own all string allocations and provide a `_free_string` to
return ownership, a `build.rs` that runs `swift build` and links
both the static lib and the relevant Apple frameworks, and a Rust-
side `extern "C"` block.

**Exception: single-file bridges.** `swift/ax-probe/main.swift` is
not a Package — it's a single-file binary built directly with
`swiftc`. This is fine for **executable spike binaries**, not for
bridges that link into Rust code. Use this carve-out only for
exploratory tools, never for v1 production bridges.

Document the pattern at `docs/swift-bridge-pattern.md` (~50 lines,
referencing this section as canonical).

### 5.5 Onboarding test workflow (laptop, TCC reset)

We do not provision a VM. Onboarding tests run on the development
machine; `scripts/reset-onboarding.sh` (per §0.4) drops TCC grants
plus heron's local state to simulate a first-run experience.

**Smoke test end of week 1** — proves the reset workflow works
before week 11 depends on it:

```sh
# 1. Confirm `tccutil reset` clears each TCC bucket (no error):
scripts/reset-onboarding.sh

# 2. Open the empty Tauri shell built in §6.4. Click "Test microphone."
#    Expect: TCC prompt for microphone (because we just reset it).

# 3. Grant. Re-run the script. Re-launch the app. Expect prompt again.
```

If `tccutil reset` produces an error or the TCC prompt fails to
re-appear after reset, fix in week 1 — week 11 cannot validate
onboarding without this.

Document the reset script + smoke test at `docs/onboarding-tests.md`.

### 5.6 Code-signing identity check (no notarization yet)

```sh
echo 'fn main() { println!("hello"); }' > /tmp/hello.rs
rustc /tmp/hello.rs -o /tmp/hello
codesign --sign "Developer ID Application: ..." /tmp/hello
codesign --verify --verbose=4 /tmp/hello
```

If this fails, fix before week 2 notarization work.

### 5.7 `claude -p` smoke + observability spec

- Run smoke per `plan.md` §5 week 1. Document at
  `docs/backend-evaluations.md`.
- Commit `docs/observability.md` with the per-session log line schema.

### 5.8 Week 1 done-when

- `cargo test --all` passes.
- `SessionClock` round-trips within 1ms.
- `scripts/reset-onboarding.sh` smoke test passes (per §5.5): TCC
  prompts re-appear after reset.
- Hello-world signs with the real Developer ID.
- EventKit bridge: `cargo test -p heron-vault calendar_smoke` proves
  Rust→Swift FFI works.
- `docs/observability.md`, `docs/security.md`,
  `docs/backend-evaluations.md`, `docs/swift-bridge-pattern.md`,
  `docs/onboarding-tests.md`, `docs/manual-test-matrix.md` (initial)
  committed.

Commit: `week 1: heron-types + clock + entitlements + TCC reset workflow + EventKit bridge`.

---

## 6. Week 2: heron-audio skeleton + AEC gate + notarization

### 6.1 Goals
1. `heron-audio` end-to-end at the **single-session, in-process,
   no-disk-spill** level. (Disk ringbuffer comes in week 3.)
2. AEC correctness gate **passes** (rig spec'd in §6.3).
3. Notarization pipeline: tag a `v0.0.0-signing-test`, get a stapled
   `.dmg` from CI.
4. Keychain bundle-ID ACL verified.

### 6.2 `heron-audio` v0 surface

```rust
pub struct AudioCapture { /* ... */ }

impl AudioCapture {
    pub async fn start(
        session_id: SessionId,
        target_bundle_id: &str,     // "us.zoom.xos"
        cache_dir: &Path,           // for week-3 ringbuffer
    ) -> Result<AudioCaptureHandle>;
}

pub struct AudioCaptureHandle {
    pub frames: broadcast::Receiver<CaptureFrame>,
    pub events: broadcast::Receiver<Event>,
    pub clock: SessionClock,
}

impl AudioCaptureHandle {
    pub async fn stop(self) -> Result<StopArtifacts>;
}
```

Threading: realtime → SPSC → APM thread → bounded broadcast. No disk
spill yet (week 3 adds it).

### 6.3 AEC correctness test — full topology

The v2 plan's "share-system-sound is enabled, osascript triggers
playback" was incoherent (Zoom's share-sound broadcasts to remote;
the engineer's own tap can't capture it). Corrected rig:

**Topology.**
- **Mac A (engineer's machine)** — runs `heron-audio` recording
  `(mic, tap, mic_clean)`. On a Zoom call. Speakers play Zoom's
  output (Multi-Output passthrough). Mic unmuted; engineer doesn't
  speak during the test.
- **Mac B (partner's machine, scheduled per §0.7 week 2)** — joined
  to the same Zoom call. Sends the test noise into the call uplink
  via one of two routes (pick whichever the partner can configure
  most reliably). **Mac B uses headphones, OR mutes its own mic
  during playback**, to prevent the noise from being re-captured
  by Mac B's mic and Zoom-NS-suppressed before it reaches Mac A.

**Mac B injection — Option A (simpler, recommended).** Use Zoom's
**Share Screen → Audio only** with "Share computer sound" enabled.
Zoom uplinks the system audio mix while screen-sharing. Mac B plays
the noise file via QuickTime; the audio goes up the Zoom wire to
Mac A as if it were the partner's voice.

**Mac B injection — Option B (no screen-share required).** Mac B
installs BlackHole 2ch + a Multi-Output Device that sends to
BlackHole + their normal speakers. **Set Mac B's *system audio
output* to the Multi-Output Device** so QuickTime audio reaches
BlackHole. **Set Zoom's *microphone input* to BlackHole 2ch** so
Zoom captures the system audio as its mic source. (Their actual
mic is unused during the test.)

If the partner can't configure Option B in their available time,
fall back to Option A.

**Result on Mac A.** `tap.wav` captures the noise (Zoom delivered it
via the call). `mic.wav` captures speaker bleed of the same noise.
`mic_clean.wav` should suppress the bleed if APM is wired correctly.

**Pre-test smoke check (run on Mac A before the real test).** A
30-second dry run while partner plays the noise: confirm `tap.wav`
contains audible noise (peak amplitude > 0.05). If silent, the
partner-side injection is broken — fix before running the real test.

**Test signal.** `fixtures/synthetic/aec-test-noise-10s.wav`
(committed, generated once via
`sox -n -r 48000 -c 1 -b 16 aec-test-noise-10s.wav synth 10 noise`).

**Exact osascript (run on Mac B, partner's machine):**
```sh
osascript -e 'tell application "QuickTime Player"
  open POSIX file "/path/to/aec-test-noise-10s.wav"
  play document 1
end tell'
```

**Recording window on Mac A.** `heron-audio` records for 12 s; the
test drops 1 s lead/trail (transient suppression).

**Pass criterion** — `scripts/aec-correlation.py`:
```python
import sys
import numpy as np, soundfile as sf
from scipy.signal import correlate
mic_clean, sr = sf.read("mic_clean.wav")
tap, _       = sf.read("tap.wav")
window = slice(int(1.0*sr), int(9.0*sr))   # central 8s
m, t = mic_clean[window], tap[window]
m = m / (np.linalg.norm(m) + 1e-9)
t = t / (np.linalg.norm(t) + 1e-9)
xc = correlate(m, t, mode="same")
mid = len(xc)//2
lag_50ms = int(0.05 * sr)
peak = float(np.max(np.abs(xc[mid - lag_50ms : mid + lag_50ms])))
print(f"correlation peak: {peak}")
sys.exit(0 if peak < 0.15 else 1)
```

Pass: `peak < 0.15`. Fail: investigate APM `process_reverse_stream`
wiring before proceeding.

**Human-in-loop tag — needs partner.** Save `(mic_clean.wav, tap.wav,
correlation.txt)` as artifacts at
`fixtures/manual-validation/aec-test/<date>/`. CI does not run this
test; the fixture is the gate.

### 6.4 Notarization pipeline (`.github/workflows/notarize.yml`)

Uses AppStoreConnect API key (preferred over ASP):

```yaml
- uses: tauri-apps/tauri-action@v0
  env:
    APPLE_API_KEY:        ${{ secrets.APPLE_API_KEY_BASE64 }}
    APPLE_API_KEY_ID:     ${{ secrets.APPLE_API_KEY_ID }}
    APPLE_API_ISSUER:     ${{ secrets.APPLE_API_ISSUER }}
    APPLE_SIGNING_IDENTITY: ${{ secrets.APPLE_SIGNING_IDENTITY }}
```

Tag `v0.0.0-signing-test`. Workflow runs. Verify with
`scripts/verify-notarization.sh`:

```sh
#!/bin/bash
set -euo pipefail
DMG="$1"
spctl --assess --type execute "$DMG" || { echo "spctl failed"; exit 1; }
xcrun stapler validate "$DMG" || { echo "stapler failed"; exit 1; }
echo "notarization verified"
```

**Plan for first-cycle failure.** Common: missing entitlement,
hardened-runtime not enabled, `Info.plist` missing usage string.
Budget 1 day. Common log: `xcrun notarytool log <submission-id>`.

### 6.5 Keychain ACL test (with two signed binaries)

`swift/keychain-helper/main.swift` follows the §5.4 bridge pattern.
Build twice with different bundle IDs (`com.heronnote.heron`,
`com.heronnote.test-foreign`); confirm cross-bundle reads fail.

Document in `docs/security.md`.

### 6.6 Week 2 done-when

- AEC test artifact (`mic_clean.wav` + `tap.wav` + correlation.txt
  with peak < 0.15) committed at
  `fixtures/manual-validation/aec-test/`.
- `verify-notarization.sh` returns "notarization verified" against
  the `v0.0.0-signing-test` artifact.
- Keychain ACL test logs ACL denial for the foreign binary.
- `cargo test --all` passes.

Commit: `week 2: heron-audio + AEC gate + notarization + keychain ACL`.

---

## 7. Week 3: heron-audio complete + fixtures

### 7.1 Goals
1. Disk-backed ringbuffer.
2. Device-change handler tested with real BT/headphones.
3. Mute/unmute tracking + ⌘Q trap + crash-recovery scan.
4. Backpressure spike passes.
5. **Fixture corpus committed at `fixtures/speech/`.**

### 7.2 Ringbuffer + recovery

Per `plan.md` §5 weeks 1–2 + §4.3 concurrency contract. Files at
`~/Library/Caches/heron/sessions/<id>/{mic,tap}.raw` (mode 0600);
`session.json` updated on every state transition.

### 7.3 Device-change validation (needs-human)

Manual test, save the screencast as a CI artifact at
`fixtures/manual-validation/device-change/<date>/`:
1. Start a session.
2. After 10s, plug in wired headphones.
3. After 30s, unplug them.
4. After 50s, connect Bluetooth headphones.
5. Stop session.

Verify: 4 `AudioDeviceChanged` events; mic.raw + tap.raw remain
contiguous (no t-skips); resampling handles BT 16kHz vs builtin
48kHz transitions.

### 7.4 Backpressure spike

`scripts/spike-backpressure.sh`:
```sh
cargo run -p heron-cli -- record \
  --app us.zoom.xos --out /tmp/test \
  --fake-stt-lag 3.0 --duration 3600
```

Verifier: `mic.raw` and `tap.raw` sample-counts each ≥
`60min × sample_rate × 0.99`.

### 7.5 Fixture capture (with parallel ax-probe)

Three real Zoom calls, each 20–40 min, diverse:
1. 1:1 internal (engineer with manager).
2. 3-person client call (gallery view).
3. 5-person team meeting with 1 dial-in.

**ax-probe binary from week 0 runs in a separate terminal during
each capture, emitting `ax-events.jsonl`.** This is the load-bearing
dependency for week 7 alignment regression (§9.4).

Each fixture saved at `fixtures/speech/<case>/`:
- `mic.wav` + `tap.wav` (synced to clap pulse at t=0)
- `ax-events.jsonl` (from probe)
- `ground-truth.jsonl` — turn-level (3–4 hours labeling each)

### 7.6 Week 3 done-when

- All week-2 tests + ringbuffer test passing.
- Real 30-min Zoom call: mic.wav and tap.wav each within ±1% of
  expected sample count; cross-correlated clap impulse aligns within
  10ms (per §3.5 method).
- Simulated SIGKILL mid-call salvageable: ≥99% of frames written
  before SIGKILL appear in salvaged session.
- Backpressure spike: 60-min fake-STT run keeps ≥99% of frames in
  ringbuffer; emits `CaptureDegraded` once STT queue saturates.
- 3 fixture calls committed with all four artifacts each.

Commit: `week 3: heron-audio complete + fixtures with ground-truth`.

---

## 8. Weeks 4–5: heron-speech (both backends)

### 8.1 Public API

```rust
#[async_trait]
pub trait SttBackend: Send + Sync {
    async fn ensure_model(&self, on_progress: impl FnMut(f32) + Send) -> Result<()>;

    async fn transcribe(
        &self,
        wav_path: &Path,
        channel: Channel,
        session_id: SessionId,
        partial_jsonl_path: &Path,
        on_turn: impl FnMut(Turn) + Send,
    ) -> Result<TranscribeSummary>;

    fn name(&self) -> &'static str;
    fn is_available(&self) -> bool;
}
```

### 8.2 WhisperKit backend (productionizing the §4 spike)

If §4 spike passed: build streaming chunked transcription, real model
lifecycle (tiny.en bundled + base.en downloadable), progress callback
wired through.

If §4 spike failed: skip; sherpa is the only backend.

### 8.3 Sherpa backend

Wraps `sherpa-onnx` C API. Bundled `parakeet-tdt-0.6b-en` (~200MB).
Always available.

### 8.4 Incremental JSONL

Per `plan.md` §3.5. `crates/heron-speech/src/partial_writer.rs`
buffers turns, fsyncs every 10 turns or 5s.

### 8.5 WER thresholds

| Fixture | Backend | Threshold |
|---|---|---|
| `client-3person-gallery/` | WhisperKit | ≤15% WER |
| `client-3person-gallery/` | Sherpa | ≤22% WER |
| `team-5person-with-dialin/` | WhisperKit | ≤22% WER |
| `team-5person-with-dialin/` | Sherpa | ≤30% WER |
| `1on1-internal/` | WhisperKit | ≤12% WER |
| `1on1-internal/` | Sherpa | ≤18% WER |

### 8.6 Backend selection

```rust
pub fn select_backend(fixtures_wer: &WerBaseline) -> Box<dyn SttBackend> {
    if !is_apple_silicon() || !is_macos_14_plus() {
        return Box::new(SherpaBackend::new());
    }
    if fixtures_wer.whisperkit.avg() > fixtures_wer.sherpa.avg() * 1.05 {
        return Box::new(SherpaBackend::new());
    }
    Box::new(WhisperKitBackend::new())
}
```

### 8.7 Weeks 4–5 done-when

- Both backends transcribe all 3 fixtures.
- WER thresholds met.
- WhisperKit chosen as default (or sherpa if WhisperKit underperforms).
- SIGKILL mid-STT → `.partial` resumes correctly.
- Bundled tiny.en model loads without HuggingFace network.

Commit: `weeks 4-5: heron-speech with WhisperKit + sherpa + incremental jsonl`.

---

## 9. Weeks 6–7: heron-zoom + aligner

### 9.1 Public API

```rust
#[async_trait]
pub trait AxBackend: Send + Sync {
    async fn start(
        &self,
        session_id: SessionId,
        clock: SessionClock,
        out: mpsc::Sender<SpeakerEvent>,
        events: mpsc::Sender<Event>,
    ) -> Result<AxHandle>;
}

pub struct Aligner { /* ... */ }
```

### 9.2 Backend selection

```rust
pub fn select_ax_backend() -> Box<dyn AxBackend> {
    match try_observer_registration_on_zoom() {
        Ok(()) => Box::new(AxObserverBackend::new()),
        Err(_) => Box::new(AxPollingBackend::new(Duration::from_millis(50))),
    }
}
```

The Swift bridge follows the §5.4 reference pattern. The
`{role, subrole, identifier}` triple for the speaking indicator was
recorded in `fixtures/zoom/spike-report.md` during week 0 — re-find
it from there.

### 9.3 Aligner algorithm

Per `plan.md` §5 weeks 5–6 (5-step algorithm). Implementation in
`crates/heron-zoom/src/aligner.rs`. Uses `SessionClock` for
host_time→session_secs.

### 9.4 Regression tests against fixtures

```rust
#[test]
fn aligner_handles_paginated_gallery() {
    let events = load_fixture("fixtures/zoom/paginated/ax-events.jsonl");
    let truth  = load_fixture("fixtures/zoom/paginated/ground-truth.jsonl");
    let mut a = Aligner::new();
    events.iter().for_each(|e| a.ingest_event(e.clone()));
    let aligned: Vec<_> = truth.iter().map(|t| a.ingest_turn(t.clone())).collect();
    assert!(name_accuracy(&aligned, &truth) >= 0.50);
}
// + active-speaker, dial-in, shared-screen, tile-rename, gallery-baseline-old-zoom
```

### 9.5 Live regression — recorded fixture, not live call

Schedule a 10-min 4-person gallery call with the partner (per §0.7
week 7). Record (mic.wav, tap.wav, ax-events.jsonl) + label
ground-truth post-hoc → commit as
`fixtures/zoom/week7-regression/`. Aligner gate: ≥70% of `tap` turns
get a real name with confidence ≥0.7 against this fixture's ground
truth.

### 9.6 Weeks 6–7 done-when

- Both AX backends selectable; observer-registration succeeds (if
  yellow per week 0) or polling-fallback works.
- Alignment regression tests pass for all 6 week-0 fixtures.
- `week7-regression` fixture: ≥70% accuracy.
- `AudioDeviceChanged` triggers re-estimation (synthetic event
  injection at t=120s in a fixture).
- Polling backend CPU verified: <5% sustained on M-series.

Commit: `weeks 6-7: heron-zoom backends + aligner + regression suite`.

---

## 10. Week 8: merge-on-write spike (5 days)

### 10.1 Goal
Implement `crates/heron-vault/src/merge.rs` with a documented
ownership model, an LLM ID-preservation contract (week 8 day 3),
and a 12-case test matrix.

### 10.2 Ownership model

```rust
// Field ownership:
//   heron_managed = always overwritten by heron on re-summarize:
//     date, start, duration_min, source_app, recording, transcript,
//     diarize_source, disclosed, cost
//   llm_inferred = overwritten if user hasn't edited; preserved if user has:
//     company, meeting_type, action_items, attendees (when calendar empty), tags
//   user_owned = always preserved:
//     extra (any non-schema field)
//   body = preserved if user has edited; else replaced
```

### 10.3 List-item merge via stable IDs

`ActionItem` and `Attendee` carry stable `id: ItemId` (week 1, §5.2).
**This only works if the LLM preserves IDs across re-summarize calls
— see §10.5.**

```rust
fn merge_action_items(
    base: &[ActionItem],     // .md.bak
    ours: &[ActionItem],     // current .md (potentially user-edited)
    theirs: &[ActionItem],   // new LLM output WITH PRESERVED IDS (§10.5)
) -> Vec<ActionItem> {
    // For each id:
    //   - present in base, ours, theirs:
    //       keep ours if changed vs base; else keep theirs
    //   - present in ours but not theirs:
    //       keep ours (user added; or LLM dropped — keep regardless)
    //   - present in theirs but not ours/base:
    //       append theirs (new from LLM)
    //   - present in base+theirs only (deleted in ours):
    //       drop (respect user deletion)
    //   - present in base+ours only (LLM dropped):
    //       keep ours
    //   - present in base only (LLM dropped, user deleted):
    //       drop (deletion converged)
}
```

### 10.4 Body merge: semantic equality with whitespace preservation

```rust
fn body_changed_semantically(base: &str, current: &str) -> bool {
    let normalize = |s: &str| -> String {
        // 1. pulldown-cmark round-trip: strips formatting whitespace
        //    BUT preserves whitespace inside <pre>/<code>.
        // 2. Then normalize ONLY non-code whitespace (collapse runs,
        //    trim trailing space) — leave code blocks byte-exact.
        let parser = pulldown_cmark::Parser::new(s);
        let mut html = String::new();
        pulldown_cmark::html::push_html(&mut html, parser);
        normalize_outside_code_blocks(&html)
    };
    normalize(base) != normalize(current)
}
```

The earlier v2 spec used `pulldown_cmark::html::push_html` directly
which preserves whitespace inside `<pre>`/`<code>`. That's correct
for code blocks (a user re-indenting code in a fenced block means
something) but wrong for prose (whitespace edits not semantic). The
`normalize_outside_code_blocks` helper splits on `<pre>...</pre>`,
collapses whitespace in the prose portions only, and rejoins.

### 10.5 LLM ID-preservation contract (Day 3 work)

The merge logic in §10.3 assumed `theirs[i].id == base[i].id` for
items the LLM "kept the same." This requires the LLM to be told
about prior IDs. Two layers:

**Layer 1 — Prompt-side preservation (primary).**

The summarizer template (§11.2) accepts an `existing_action_items`
variable when re-summarizing. Template fragment:

```handlebars
{{#if existing_action_items}}
The following action items were generated from a prior summary of
this meeting. Each has a stable `id`. **For items that you would
output again with the same meaning, RETURN THE EXACT SAME `id`.**
Mint a new `id` only for genuinely new items not in this list.

{{#each existing_action_items}}
- id: "{{id}}" | owner: {{owner}} | text: {{text}}
{{/each}}
{{/if}}
```

The output schema (still JSON) includes `id` per item. heron-llm
parses the JSON and validates: if a returned `id` doesn't match any
known ID and isn't a fresh UUIDv7, treat the item as new (mint a
fresh UUID server-side).

**Layer 2 — Text-similarity matcher (fallback, v1.1).**

If empirical observation in week 8 shows the LLM ignores the
preserve instruction frequently (>20% of items), fall back to a
text-similarity matcher (`strsim::levenshtein`) that resolves new
LLM items to base items by normalized-text distance. v1 ships
without this; week-8 day-3 work tests the prompt-side approach
against a 10-call fixture corpus to decide.

### 10.6 Day-by-day

- Day 1: type definitions, `MergePolicy` API, scaffolding.
- Day 2: list-item merge for `action_items` + `attendees` (assumes
  IDs preserved); unit tests using hand-crafted fixtures with
  matching IDs.
- **Day 3: LLM ID-preservation contract.** Update `heron-llm`
  template + parser. Run integration test on 10 fixture re-
  summarizes; measure `id_preservation_rate`. If <80%, escalate to
  text-similarity matcher (Layer 2).
- Day 4: frontmatter-level merge + extra pass-through; body semantic
  comparison with whitespace-in-code-blocks fix; `.md.bak` rotation.
- Day 5: full test-matrix (§10.7) passing; code review against
  ownership doc.

### 10.7 Test matrix

`crates/heron-vault/tests/merge_matrix.rs` — 12 cases:

| # | Scenario | Expected |
|---|---|---|
| 1 | User adds tag, re-summarize | tag preserved; LLM tags merged |
| 2 | User edits action item text, re-summarize | edit preserved (id match) |
| 3 | User deletes action item, re-summarize | item stays deleted |
| 4 | LLM adds new action item, user hasn't touched | new item appears |
| 5 | User changes meeting_type to "internal" | preserved |
| 6 | User adds custom frontmatter field | preserved (extra) |
| 7 | User edits body prose, re-summarize | body preserved |
| 8 | User hasn't touched body, re-summarize | LLM body wins |
| 9 | Both base+theirs have item; user deleted ours | deletion wins |
| 10 | New summarize generates different cost | cost overwrites |
| 11 | User edited disclosed.when | user value lost (heron-managed; documented) |
| 12 | `.md.bak` missing (first re-summarize) | treat as "user edited" |

### 10.8 Week 8 done-when

- All 12 matrix tests pass.
- ID-preservation rate ≥80% on the 10-fixture re-summarize corpus.
- `docs/merge-model.md` committed with the ownership model.
- Re-summarize round-trips a non-trivial fixture without data loss.

Commit: `week 8: merge-on-write spike with ownership model + LLM ID contract + 12-case matrix`.

---

## 11. Week 9: heron-llm + m4a pipeline

### 11.1 `heron-llm` API

Per `plan.md` §5 weeks 7–8. Three backends. Bare `reqwest` wrapper
for Anthropic.

### 11.2 Templates with ID preservation

`crates/heron-llm/templates/meeting.hbs` — single template,
branches on `meeting_type`. Includes the `existing_action_items` /
`existing_attendees` blocks from §10.5 when the caller passes prior
items (re-summarize path). On first summarize, the blocks are
empty and the LLM mints fresh UUIDs.

The `heron-llm::Summarizer` API:
```rust
pub struct SummarizerInput<'a> {
    pub transcript: &'a Path,
    pub meeting_type: MeetingType,
    pub existing_action_items: Option<&'a [ActionItem]>,
    pub existing_attendees: Option<&'a [Attendee]>,
}
```

**Source of `existing_action_items` / `existing_attendees`.** When
re-summarizing, `heron-vault::re_summarize` reads these fields from
the **current `.md`** (i.e., `ours` in the §10.3 merge — the file
with any user edits applied), **not** from `.md.bak`. Reasoning:
showing the LLM the user's edited text is a stronger preserve-the-ID
signal than showing it the prior summarize's stale text. The merge
logic in §10.3 still uses `base = .md.bak`, `ours = current .md`,
`theirs = LLM output`; the LLM sees only `ours` because that's the
current truth.

### 11.3 m4a encode pipeline (explicit build step)

`crates/heron-vault/src/encode.rs`:

```rust
pub async fn encode_to_m4a(
    wav_mic: &Path, wav_tap: &Path, out_m4a: &Path,
) -> Result<()> {
    // ffmpeg subprocess; stereo (L=mic, R=tap); AAC 64kbps VBR.
}

pub fn verify_m4a(path: &Path, expected_duration_sec: f64) -> Result<bool> {
    // ffprobe -v error -show_entries stream=nb_frames,duration
}
```

### 11.4 Cost calibration

Anthropic API responses include `usage.input_tokens` /
`output_tokens` and prompt-cache fields. Compute USD from current
public pricing; include in `Cost`. **Source of truth: API response,
not the dashboard** (dashboard lags by minutes; matched plan.md
§5 weeks 7–8 done-when).

### 11.5 Week 9 done-when

- Anthropic backend summarizes a real fixture transcript.
- Claude Code CLI backend summarizes; warning surface emits in CLI.
- Cost matches API-response totals exactly.
- m4a encode + verify works on a 30-min WAV pair.
- Re-summarize integration test (using fixture from week 8) preserves
  ≥80% of action item IDs.

Commit: `week 9: heron-llm + ID preservation + m4a encode pipeline`.

---

## 12. Week 10: heron-vault + calendar + ringbuffer purge

### 12.1 `heron-vault` API
Per `plan.md` §5 weeks 7–8. `finalize_session`, `re_summarize`,
`calendar_read_one_shot`.

### 12.2 Calendar one-shot

EventKit Swift bridge under `swift/eventkit-helper/` (already exists
from week 1, §5.4). New `--read-window <start> <end>` flag exposed
via `ek_read_window_json`.

**Denial contract.** Per `plan.md`. If `requestFullAccessToEvents`
returns denied, the helper returns 0 from `ek_request_access`. Rust
caller maps to `Ok(None)`. **Caller never blocks on a prompt; the
prompt happens during onboarding (week 11) and is remembered.**

### 12.3 Ringbuffer purge with verification

```rust
if encode::verify_m4a(&m4a_path, expected_sec)? {
    fs::remove_dir_all(cache_dir)?;
} else {
    warn!("m4a verification failed; ringbuffer retained");
    surface_salvage_banner(id);
}
```

### 12.4 Week 10 done-when

- `weekly-client-summary` skill runs unmodified on 1 week of heron
  output.
- Calendar-denied path returns in <100ms.
- Re-summarize path passes the §10.7 matrix end-to-end.
- Ringbuffer dir gone after a successful session.

Commit: `week 10: heron-vault + calendar + ringbuffer purge`.

---

## 13. Week 11: Tauri shell + 5-step onboarding

### 13.1 Goals
1. Empty Tauri shell from week 2 grows into a real app.
2. 5-step onboarding flow per `plan.md` §5 week 9.
3. **All 5 steps validated on this laptop via `scripts/reset-onboarding.sh`
   between walkthroughs** (per §5.5). Real naive-user coverage is
   week-16 exec dogfood.

### 13.2 Onboarding routes
Per `plan.md`: `/onboarding/{mic,audio,ax,calendar,model}`. State
persists to `~/Library/Preferences/com.heronnote.heron.plist`.

### 13.3 Per-step Test buttons (with counter-tests)

| Step | Test (positive) | Counter-test (negative) |
|---|---|---|
| 1 Microphone | Record 1s; level meter > -60dB during speech | After deny: level meter never moves |
| 2 System audio | Tap any open app; record 1s; non-silent waveform | After deny: tap fails with clear error |
| 3 Accessibility | `ax-probe` returns any AX element | Without grant: re-shows instructions |
| 4 Calendar | `requestFullAccess`; read next event | Decline: `calendar_read_one_shot` returns Ok(None) within 100ms |
| 5 Model download | Progress reaches 100% OR cancel → sherpa | Cancel mid-download: rollback; sherpa selected |

### 13.4 macOS Settings screenshots

`apps/desktop/public/onboarding/accessibility-sonoma.png` and
`-sequoia.png`. Half-day budget.

### 13.5 Onboarding validation on this laptop

Per §5.5 (laptop TCC-reset workflow). Loop:

```sh
# For each walkthrough:
scripts/reset-onboarding.sh    # clears TCC + heron state
open /Applications/heron.app   # launches as if first run
# Walk through onboarding manually, recording the screen.
# Save screencast → fixtures/manual-validation/onboarding/<date>-<n>.mov
```

12 walkthroughs total (5 positive paths + 5 counter-tests + 2 edge
cases — paginated gallery, slow-network model download). Each
~3 min + ~1 min reset overhead = ~1 day.

**Coverage caveat (recorded in `docs/manual-test-matrix.md`):** the
laptop has dev tools, hardware peripherals, and existing user state.
Onboarding paper-cuts that only surface on a stock Mac (default
fonts, no Xcode-installed certificates, slow public wifi) are
deferred to week-16 exec dogfood. Bugs found there during dogfood
are accepted as v1.1 candidates if not blocking §18.2 ship criteria.

### 13.6 Week 11 done-when

- Engineer completes onboarding on this laptop (post-reset) in
  <15 min.
- All 5 positive tests pass.
- All 5 counter-tests behave gracefully (no crash, clear error,
  recoverable state).
- Skipping Calendar: completes; `calendar_read_one_shot` returns
  `Ok(None)`.
- Cancel model download: heron records using sherpa.

Commit: `week 11: Tauri shell + 5-step onboarding + counter-tests`.

---

## 14. Week 12: recording UX + crash recovery + WhisperKit DL UX

### 14.1 Scope
- Hotkey, status indicator, consent banner: per `plan.md` §5 week 10.
- Salvage flow + error toasts.
- WhisperKit re-download from settings.
- Disk-space gate (<2GB free → record disabled).

### 14.2 Disclosure-banner state machine

```
idle ──(hotkey)──► armed
armed ──(yes)──► recording
armed ──(remind 30s)──► armed-cooldown ──(30s tick)──► armed
armed ──(cancel)──► idle
recording ──(hotkey or window close)──► transcribing
transcribing ──(done)──► summarizing
summarizing ──(done|fail)──► idle
```

### 14.3 Week 12 done-when

- Start→record→stop→summarize completes via menubar; resulting `.md`
  has non-empty body (asserted by integration test).
- Simulated SIGKILL during recording → salvage list on next launch.
- WhisperKit re-download from settings works.
- Disk-space gate fires at <2GB.

Commit: `week 12: recording UX + crash recovery + DL UX`.

---

## 15. Week 13: review UI

### 15.1 Scope
TipTap editor + audio playback + transcript jump-to-time per
`plan.md` §5 week 11.

### 15.2 Asset-protocol fallback

Tauri custom protocol handler resolves `heron://recording/<id>`:
- If m4a verified → serve it.
- Else → mixed-down WAV from cache.

### 15.3 Re-summarize UI

Button → confirms → runs backend → applies merge per §10.7 → renders
updated `.md`. **No diff modal in v1** (matched to plan.md §5 week 11
revision). Diff modal is a v1.1 enhancement; `.md.bak` is the v1
rollback.

### 15.4 Diagnostics tab

Reads `heron_session.json`. Renders AX hit rate, dropped frames, STT
wall time, cost, error log.

### 15.5 Week 13 done-when

- Open any completed session; play audio; click transcript lines to
  scrub.
- Open before m4a finished → playback works via WAV fallback.
- Re-summarize: user-edited action items preserved per §10.7.
- Diagnostics tab shows real values.

Commit: `week 13: review UI`.

---

## 16. Week 14: settings + polish + buffer

### 16.1 Settings pane
Per `plan.md` §5 week 12. ~10 settings.

### 16.2 Bug-fix buffer

Half of week 14 reserved for whatever surfaces during weeks 11–13.

### 16.3 Week 14 done-when

- Settings persisted across restart.
- Bug-fix list from weeks 11–13 closed.

Commit: `week 14: settings + polish`.

---

## 17. Week 15: personal dogfood

Engineer dogfoods every meeting taken during week 15. Track every
paper cut at `docs/dogfood-log.md`. Triage at end of week.

End-of-week ship-or-not gate per `plan.md` §5 week 12 ship criteria.

Commit: `week 15: personal dogfood + fixes`.

---

## 18. Week 16: exec dogfood + ship gate

### 18.1 Schedule
- Day 1: exec-friend onboarding, screen-shared. Author observes;
  logs every confusion.
- Days 2–4: exec uses heron unaided. Author monitors logs; doesn't
  intervene unless heron breaks.
- Day 5: retro + decision.

### 18.2 Ship criteria

Sourced from `plan.md` §5 week 12 (kept in sync). Any "yes" → don't
ship v1.0; cut scope or fix:
- Crash during normal use.
- Session lost (audio + transcript both irrecoverable).
- First-run onboarding took >20 min for an unaided non-technical user.
- Exec couldn't complete a meeting → note flow unaided.
- AEC regression (peak normalized cross-correlation >0.15 on the
  fixture from §6.3).
- Cost exceeded $2 on any single meeting <60 min.

**Success signal:** ≥1 follow-up email authored from a heron-
generated note during the dogfood week.

If all criteria green: tag `v1.0`, build notarized DMG.

Commit: `week 16: exec dogfood; v1.0 tag`.

---

## 19. Cross-cutting conventions

### 19.1 Error handling
`anyhow::Result` at binary / async-task boundaries.
`thiserror::Error` on typed errors crossing crate boundaries. No
`unwrap()` / `expect()` in non-test code; CI lints (§1.4).

### 19.2 Logging
- Single global subscriber, JSON, file + stderr.
- `~/Library/Logs/heron/<date>.log` (mode 0600).
- Per-session summary line on `SessionEnded`.
- Field names stable; schema versioned `log_version: 1`.

### 19.3 Swift bridge pattern
Reference: `swift/eventkit-helper/` (see §5.4 for actual files).
All bridges (whisperkit, zoom-ax, keychain) follow that exact shape.
Single-file `swiftc` binaries (`ax-probe`) are for spike tools only.
Pattern documented at `docs/swift-bridge-pattern.md`.

### 19.4 File atomicity
`heron-vault::atomic_write` (UUID-named temp + rename, mode 0600).
Used for every file in the vault and partial-jsonl finalization.

### 19.5 Fixture testing
Each fixture is a directory under `fixtures/<crate>/<case>/` with
`mic.wav`, `tap.wav`, `ax-events.jsonl`, `ground-truth.jsonl`,
`README.md`. Tests glob over fixtures.

### 19.6 Human-in-loop tests
Tagged `// [needs-human]` and require a recorded artifact at
`fixtures/manual-validation/<test-name>/<date>.{mov,wav,png}`. CI
does not run these tests; they are manual gates per relevant week's
done-when.

### 19.7 `docs/manual-test-matrix.md`

Single source of truth for every needs-human gate. Committed end of
week 1 with a row per test. Schema:

| # | Section | Test name | Owner | When | Pass criterion | Artifact location |
|---|---|---|---|---|---|---|

Initial rows (filled in week 1):
1. §3.3 fixture capture (week 0) — engineer + partner — completed fixture dirs at `fixtures/zoom/<case>/`
2. §3.4 ground-truth labeling (week 0) — engineer — `ground-truth.jsonl` files
3. §6.3 AEC test rig (week 2) — engineer + partner — correlation < 0.15 + recorded artifacts
4. §7.3 device-change validation (week 3) — engineer — screencast at `fixtures/manual-validation/device-change/`
5. §7.5 fixture capture (week 3) — engineer + partner — full fixtures committed
6. §9.5 week-7-regression fixture (week 7) — engineer + partner — fixture committed
7. §13.5 onboarding walkthroughs on this laptop after `scripts/reset-onboarding.sh` (week 11) — engineer — 12 screencasts at `fixtures/manual-validation/onboarding/`
8. §18 exec dogfood (week 16) — exec + author observation — `docs/dogfood-log.md` entries

---

## 20. Risk-reducer branching

| Trigger | Effect |
|---|---|
| Week 0 spike Red (AXObserver unusable) | Delete `heron-zoom`; ship v1 with `speaker: "them"`. Update `plan.md` §1 quality promise. Schedule slips ~2 weeks. |
| Week 0.5 WhisperKit bridge fail | Flip `plan.md` §2 row 7 to "sherpa-only v1." Drop ~3 days from week 4. |
| AEC test fails week 2 | Stop. Debug `process_reverse_stream` wiring. Do not write week-3 code until correlation < 0.15. |
| Notarization first-cycle failure week 2 | Spend 1 day debugging; if not green by EOW, escalate to a focused 2-day work block at start of week 3. |
| `cidre` API breaks on next point release | Stay pinned at `=0.5.3`. Re-evaluate v1.1. |
| `webrtc-audio-processing` arm64 build fails | Fall back to community fork; document. |
| `tccutil reset` doesn't re-prompt TCC | Investigate week 1; may need full app re-install or stale Info.plist usage strings. Block week 11 until fixed. |
| Polling CPU >8% week 6 | Emit `AttributionDegraded`; consider 100ms poll interval. |
| Claude Code CLI JSON unstable | Degrade `ClaudeCodeCli` to "experimental, opt-in" with prominent warning. |
| Anthropic API outage week 9 | Summarization is async; queue. Manual `retry` button in review UI. |
| Live week-7 partner unavailable | Reschedule; do not skip the recorded-fixture step. |
| **LLM ID-preservation rate <80% in week 8** | Activate Layer-2 text-similarity matcher (§10.5) before week 9. |
| Merge matrix exposes ambiguity | Surface via diagnostics; defer in-app conflict UI to v1.1. |

---

## 21. Go/no-go per week

Each week's commit is the gate. If week-N done-when fails:
1. Do not start week N+1.
2. Open a `blocker/<topic>` branch.
3. Fix or cut scope. If cutting scope, update `plan.md` + this file.
4. Re-run done-when. Proceed only when green.

The dependency graph (§2) is load-bearing.

---

## 22. Time budget summary

| Week | Description | Days | Cumulative |
|---|---|---|---|
| 0 | AX spike (incl. labeling) | 5 | 5 |
| 0.5 | WhisperKit bridge spike | 3 | 8 |
| 1 | Foundations + clock + entitlements + EventKit bridge + TCC reset | 5 | 13 |
| 2 | heron-audio + AEC + notarization | 5 | 18 |
| 3 | heron-audio complete + fixtures | 5 | 23 |
| 4 | heron-speech (WhisperKit) | 5 | 28 |
| 5 | heron-speech (sherpa + WER) | 5 | 33 |
| 6 | heron-zoom backends | 5 | 38 |
| 7 | aligner + week7-regression fixture | 5 | 43 |
| 8 | merge-on-write spike (incl. LLM ID contract) | 5 | 48 |
| 9 | heron-llm + m4a | 5 | 53 |
| 10 | heron-vault + calendar | 5 | 58 |
| 11 | Tauri shell + onboarding | 5 | 63 |
| 12 | recording UX + crash recovery | 5 | 68 |
| 13 | review UI | 5 | 73 |
| 14 | settings + polish + buffer | 5 | 78 |
| 15 | personal dogfood | 5 | 83 |
| 16 | exec dogfood + ship | 5 | 88 |

**Total: 88 working days = ~17.6 weeks** at 5 days/week, 17 weeks
calendar with ~4–5% slip absorbed by week 14 buffer + week 16.5 if
needed.

This budget assumes ~7 hours of focused work per working day.
