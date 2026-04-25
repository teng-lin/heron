# heron v1 plan

A private, on-device, agent-friendly AI meeting note-taker for macOS.
Flagship: invisible capture + Fireflies-quality diarization on native Zoom,
without a bot. Output is markdown dropped into a dedicated Obsidian vault.

This plan supersedes the broader visions in `architecture.md` and `stack.md`
for the purposes of v1 scope. Those remain the long-range reference.

**Revision history.**
- v1: initial sketch.
- v2 (post-oracle review #1): WhisperKit default, Anthropic API default,
  AEC reverse-reference, WAV-before-encode, `speaker_source`/`confidence`
  schema, `heron-types` crate, 12-week timeline, permission onboarding.
- v3 (post-oracle review #2, three agents): security model for cached
  audio (purge-on-success + 0600 + explicit threat model), numeric
  pass/fail thresholds in week-0 spike, WhisperKit model download UX,
  calendar-decline contract, incremental JSONL write, `heron-audio`
  backpressure/threading contract, device-change timestamp monotonicity,
  `heron-types` enum expanded to include audio control events,
  auto-update deferred, SQLite index cut from v1, confidence-sentinel
  bug fixed, re-summarize merge-on-write rule, week-11 playback
  WAV-fallback, Keychain ACL scoped to signed bundle ID.

---

## 1. Product

**For whom.** The author (engineer, heavy Claude Code user) and business
executives who run many external client meetings.

**Problem.** Existing tools force a tradeoff:
- Fireflies et al. join as a visible bot — socially loud, clients notice.
- Granola captures invisibly but collapses remote audio to a single mixed
  track, so diarization is lossy clustering at best.
- Char / oh-my-whisper are closer to the right shape but don't solve
  per-speaker attribution for native Zoom.

**What heron does.** Runs on the user's Mac as a legitimate meeting
participant. Records mic + per-app system audio (Zoom only for v1).
Transcribes locally via WhisperKit. Attributes speakers using the Zoom
accessibility tree (real display names, no ML clustering needed in the
happy path). Summarizes via the Anthropic API (with prompt caching) into
a markdown file in a user-chosen Obsidian vault folder. The vault folder
lives inside the user's Dropbox / Google Drive / iCloud — heron never
touches sync.

**Quality promise (honest, yellow-case aware).** In the modal AXObserver
outcome (see §5 week 0), heron attributes ~70% of turns to a real name
with high confidence; the remaining ~30% are marked `them` with a visual
low-confidence indicator. That's strictly better than Granola (0%
attribution) and comparable-to-weaker than Fireflies on a well-configured
call. The pitch is **"Fireflies-quality attribution on most turns,
without a bot"**, not **"Fireflies-quality transcripts, always."**

**Threat model (explicit).** Protected: audio never leaves the device
except to the user-selected LLM provider for summarization. Out of
scope: adversary with code execution on the user's Mac running as that
user. FileVault is assumed for exec laptops. Local API keys sit in
Keychain with an ACL scoped to heron's signed bundle ID; a compromised
process running as the user can still access the vault, the cache, and
(via Keychain prompts or injection) the key. Users concerned about
local compromise should use Secure Enclave — out of scope for v1.

**Deliberate non-goals for v1.**
- Ambient session detection (`heron-session`, `heron-events`). Deferred.
- MCP server, IPC socket, or any agent-facing API beyond the vault.
  Claude Code reads the vault files directly with its built-in tools.
- iOS / Android / Windows / Linux.
- Meet / Teams / Webex. Zoom only.
- Voice-embedding enrollment / compounding identification.
- Cross-device pair listening.
- A plugin framework.
- A backend. Sync is the user's cloud folder.
- Auto-update. v1 is hand-built + notarized; v1.1 adds the Tauri updater.
- SQLite index. v1 queries are grep over the vault. v1.1 adds an
  index if performance requires.

---

## 2. Locked decisions

| # | Decision | Value |
|---|---|---|
| 1 | Target OS (v1) | macOS 14.2+ (for Core Audio process taps) |
| 2 | Primary language | Rust 2024, cargo workspace |
| 3 | Async runtime | tokio |
| 4 | System audio capture | Core Audio process taps via `cidre` |
| 5 | Microphone | `cpal` |
| 6 | AEC / noise | `webrtc-audio-processing` — mic + tap both fed to APM; tap as the reverse (far-end) reference signal |
| 7 | STT | **WhisperKit default on macOS 14+ with Apple Silicon**, sherpa-onnx on Intel or when WhisperKit unavailable; `heron-speech` trait hides which is active. Benchmark gates the default: if WhisperKit fixture WER > sherpa + 5%, fall back. |
| 8 | VAD | sherpa-onnx (Silero via ONNX) — used for chunking regardless of STT backend |
| 9 | Speaker attribution (Zoom) | `AXObserver` primary with polling fallback; channel-only degrade path always available |
| 10 | Summarizer backends | **Anthropic API (default)**, Claude Code CLI (opt-in), Codex CLI (alternate) |
| 11 | Prompt templates | Handlebars, heron owns them in-repo |
| 12 | Session detection | **Manual hotkey only in v1**; ambient deferred |
| 13 | Agent surface | **Filesystem only** (vault markdown); no MCP in v1 |
| 14 | Sync | Not heron's problem — user points at Dropbox/Drive/iCloud folder |
| 15 | Desktop shell | Tauri v2 + React + TipTap (weeks 9–12) |
| 16 | Secrets | macOS Keychain via `security-framework`, ACL scoped to heron's signed bundle ID (not "any process running as user") |
| 17 | Logging | `tracing` + JSON to `~/Library/Logs/heron/` (mode 0600) |
| 18 | Consent UX | Pre-meeting reminder + `disclosed` frontmatter block (`stated`, `when`, `how`) |
| 19 | Summary template shape | Single template with `meeting_type` branching inside |
| 20 | Event vocabulary | `heron-types` crate owns `Event` enum + all serde types from day 1. **No event types invented in other crates.** v1 internal broadcast channel carries this enum; v2 events bus is a transport wrap. |
| 21 | Canonical source of truth | **Markdown file is canonical for human-edited fields** (body, action items, company, meeting_type). Session-derived fields (times, duration, source_app, cost, disclosed) are owned by heron and overwrite on re-summarize. Merge-on-write rule in §5 weeks 7–8. |
| 22 | Crash-safety policy | Captured audio streamed to disk-backed ringbuffer during recording; WAL-style session state file updated on every state transition (not timer-based — see §5 weeks 1–2); graceful-quit trap on ⌘Q while recording. |
| 23 | Code-signing | Developer ID cert + notarization pipeline set up in week 1, not week 10. |
| 24 | At-rest security for cached audio | Ringbuffer files at mode 0600. **Purged on successful m4a encode + verification.** Stale ringbuffers (age > 24h, orphaned by a prior crash) salvaged on startup then purged. Audio encryption-at-rest is **not** implemented in v1; threat model in §1 explains why. |
| 25 | Re-summarization policy | User can re-summarize a session (different model, different template). Re-summarization preserves user edits via the **Ownership Model** documented at `implementation.md` §10: heron-managed fields overwrite; user-edited LLM-inferred fields preserved via stable item IDs (the LLM is given prior `action_items`/`attendees` with their `id`s and instructed to preserve them for unchanged items); body preserve-or-replace based on semantic equality. Rollback to prior `.md` kept as `.md.bak` for one generation. |

Note: rows removed from v2 — auto-update (deferred to v1.1), SQLite
index (cut from v1 scope; not needed since agent surface is filesystem).

---

## 3. Output contract (design this first)

Everything downstream — the desktop UI, Claude Code querying, the
`weekly-client-summary` skill — binds to the vault shape. Lock the
schema before week 1 code.

### 3.1 Vault layout

```
heron-vault/                      ← user chooses path on first run
├── meetings/
│   └── 2026-04-24-1400 Acme sync.md
├── transcripts/
│   └── 2026-04-24-1400.jsonl
└── recordings/
    └── 2026-04-24-1400.m4a
```

### 3.2 Filename

`YYYY-MM-DD-HHMM <name>.md` — `HHMM` is meeting start time, 24h, local.

Name comes from (in priority order): calendar event title (if Calendar
permission granted) → window title of foreground meeting app →
`untitled`. User can rename after the fact; heron references by path,
not title.

### 3.3 Frontmatter

```yaml
---
date: 2026-04-24
start: "14:00"
duration_min: 47
company: Acme
attendees:                     # empty array if Calendar not granted
  - name: Alice
    company: Acme
  - name: Bob
    company: Acme
meeting_type: client           # client | internal | 1:1 | other
source_app: us.zoom.xos
recording: recordings/2026-04-24-1400.m4a
transcript: transcripts/2026-04-24-1400.jsonl
diarize_source: ax             # ax | channel | hybrid
disclosed:
  stated: yes
  when: "00:14"                # mm:ss into the call (null if pre-call / written)
  how: verbal                  # verbal | written_chat | pre_email | none
cost:
  summary_usd: 0.04
  tokens_in: 14231
  tokens_out: 612
  model: claude-sonnet-4-6
action_items:
  - owner: me
    text: Send pricing deck by Friday
tags: [meeting, acme]
---
```

### 3.4 Transcript JSONL

One line per turn:
```json
{"t0": 12.4, "t1": 18.9, "speaker": "Alice", "speaker_source": "ax",      "confidence": 0.92, "text": "We need...", "channel": "tap"}
{"t0": 19.1, "t1": 21.2, "speaker": "me",    "speaker_source": "self",    "confidence": 1.00, "text": "Yeah.",       "channel": "mic"}
{"t0": 22.0, "t1": 25.4, "speaker": "them",  "speaker_source": "channel", "confidence": null, "text": "Hmm — the ", "channel": "tap"}
```

- `channel`: `"mic"` (user's own voice) or `"tap"` (Zoom output).
- `speaker_source`:
  - `"self"` — `channel == "mic"`; speaker is the user. Trivially true.
  - `"ax"` — name from `AXObserver` event with sufficient overlap (see
    §5 weeks 5–6 algorithm).
  - `"channel"` — fell back to the channel (`"them"`); AX didn't fire
    or overlap-confidence was below threshold.
  - `"cluster"` — v2 only; voice-embedding clustering result.
- `confidence`: 0.0–1.0 for `"ax"` (derived from overlap fraction ×
  decay). `1.0` for `"self"`. **`null` for `"channel"`** — "we don't
  know" rather than a sentinel 0.5.

**UI display rule: italicize speaker name when `speaker_source != "ax"
&& speaker_source != "self"`.** Keying off `speaker_source`, not
`confidence`, avoids the sentinel-comparison bug. For `"ax"` turns,
additionally italicize if `confidence < 0.7` (low-confidence AX match).

Downstream consumers (editor, weekly-summary skill) MUST treat `"ax"`
and `"channel"` turns as attributable-but-not-guaranteed and visually
differentiate low-confidence turns.

### 3.5 Transcript durability

JSONL is written incrementally during STT as `transcripts/<id>.jsonl.partial`
(append-only, one line per committed turn). On successful STT completion,
atomic rename to `transcripts/<id>.jsonl`. On crash mid-STT, the
`.partial` file retains completed turns; restart resumes from
`max(t1)` of the partial. Summarization runs against the finalized
JSONL, not the partial.

---

## 4. Crate skeleton

```
heron/
├── Cargo.toml                     # workspace
├── crates/
│   ├── heron-types/               # All serde types + the Event enum.
│   │                              # Rule: no event types invented
│   │                              # outside this crate. Any new event
│   │                              # in any other crate is a PR against
│   │                              # heron-types first.
│   ├── heron-audio/               # process tap + mic + APM (AEC with
│   │                              # tap as reverse ref) + resample +
│   │                              # disk-backed ringbuffer with
│   │                              # bounded-channel backpressure;
│   │                              # device-change handler
│   ├── heron-speech/              # WhisperKit (default) / sherpa-onnx
│   │                              # (fallback); VAD always sherpa;
│   │                              # emits Turn stream, appends to
│   │                              # .jsonl.partial
│   ├── heron-zoom/                # AXObserver primary + polling
│   │                              # fallback; emits SpeakerEvent stream
│   ├── heron-llm/                 # Summarizer trait + 3 backends +
│   │                              # handlebars templates
│   ├── heron-vault/               # filesystem writer (atomic) +
│   │                              # calendar one-shot read +
│   │                              # frontmatter merge-on-write
│   └── heron-cli/                 # `heron record | transcribe | summarize`
├── apps/
│   └── desktop/                   # Tauri v2 shell (weeks 9–12)
└── docs/
```

Seven crates + one app. `heron-types` exists now specifically so the
v2 events bus is a transport wrap, not a vocabulary refactor.

### 4.1 Data flow (v1 happy path)

```
 user hits ⌘⇧R
       │
       ▼
 heron-cli record
       │   (reads settings: vault path, target app)
       ▼
 heron-audio ─┬─► mic frames  ──┐
              └─► tap frames  ──┤  both → disk-backed ringbuffer
                                │  (0600 perms; crash-safe)
                                │
                                │  APM fed both:
                                │    mic = near-end
                                │    tap = reverse / far-end reference
                                │  → mic_clean stream (echo cancelled)
                                │
 heron-zoom ──► SpeakerEvent{t, name} stream (AX or poll)
                                │
 user hits ⌘⇧R again (or window closes, or ⌘Q trapped)
       │
       ▼
 heron-cli stop
       │   (drain ringbuffer → 2× WAV on disk, 0600)
       ▼
 heron-speech transcribe per-channel (WAV, NOT m4a)
       │   append to transcripts/<id>.jsonl.partial per committed turn
       ▼                          │
       │                          │  parallel: encode recordings/*.m4a
       │                          │  from the WAVs (0600)
       │                          │
       │  when STT done, join with SpeakerEvents (alignment §5 wks 5–6)
       │  then atomic rename .partial → .jsonl
       │
       ▼
 heron-vault calendar_read_one_shot()
       │   if Calendar not granted: returns Ok(None) immediately;
       │   meeting note falls through to window-title → "untitled";
       │   never prompts Calendar from the CLI path.
       ▼
 heron-llm summarize (default: Anthropic API, prompt-cached)
       │
       ▼
 heron-vault write meetings/*.md (atomic temp+rename)
       │   frontmatter merge-on-write (see §5 weeks 7–8)
       ▼
 heron-vault verify m4a integrity (ffprobe frame count)
       │   → purge ringbuffer + WAV on success
       │   → keep ringbuffer + surface salvage banner on failure
```

### 4.2 `heron-types`: the event contract

```rust
#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "kind")]
pub enum Event {
    // Session lifecycle
    SessionStarted   { id: SessionId, source_app: String, started_at: DateTime<Utc> },
    SessionEnded     { id: SessionId, ended_at: DateTime<Utc>, duration: Duration },

    // Audio capture
    MicMuted         { id: SessionId, at: Duration },       // mm:ss into session
    MicUnmuted       { id: SessionId, at: Duration },
    AudioDeviceChanged { id: SessionId, at: Duration, reason: DeviceChangeReason },
    CaptureDegraded  { id: SessionId, at: Duration, dropped_frames: u32, reason: String },

    // Speaker attribution
    SpeakerDetected  { id: SessionId, event: SpeakerEvent },
    AttributionDegraded { id: SessionId, at: Duration, reason: String },

    // Transcription
    TranscriptPartial { id: SessionId, turn: Turn },
    TranscriptFinal   { id: SessionId, turns_count: usize, path: PathBuf },

    // Summarization
    SummaryReady     { id: SessionId, path: PathBuf, cost: Cost },
    SummaryFailed    { id: SessionId, error: String },
}
```

**Invariant.** No crate outside `heron-types` may define an event type
carried on the internal broadcast. If a new event is needed,
PR `heron-types` first. v2 events bus serves this exact enum over a
unix socket; there is no v1 → v2 refactor.

### 4.3 `heron-audio` concurrency contract

Capture and processing across three threads:

1. **Real-time audio thread** (owned by Core Audio / cpal). Receives
   frame callbacks. MUST NOT block. Writes raw frames to a lock-free
   SPSC queue (one per source, mic and tap).
2. **APM thread** (single tokio worker, high priority). Drains the
   SPSC queues, runs AEC with tap as reverse ref, emits `CaptureFrame`
   to a **bounded** `tokio::sync::mpsc` channel (cap 500 frames =
   10s at 20ms chunks).
3. **Consumer threads**: ringbuffer-writer (always keeps up; disk I/O
   is fast), AX alignment (keeps up; rare events), STT (variable —
   can lag 0.3× realtime on Intel).

**Backpressure policy.**
- Ringbuffer writer is backpressure-free (disk spill bounded by free
  space; `StorageCritical` event + graceful session stop if <1GB free).
- STT consumes from a separate broadcast of `CaptureFrame`, with a
  per-consumer bounded lagging window of 60s. If STT falls >60s
  behind, emit `CaptureDegraded { reason: "stt lag" }`, drop oldest
  frames from STT's queue only (ringbuffer retains everything).
  Session continues; STT catches up after recording stops.
- If the realtime → APM SPSC queue fills (catastrophic — APM thread
  starved), emit `CaptureDegraded { dropped_frames: N }` and drop
  frames from the SPSC queue tail. This is the only place the pipeline
  drops data; all downstream consumers degrade from a complete
  captured timeline.

**Device-change monotonicity.** `CaptureFrame.t` is always session-wall-
time (monotonic `Instant` since `SessionStarted`), not device host
time. On device change (headphone unplug/connect): tear down capture
graph, emit `AudioDeviceChanged`, rebuild graph with new device, resume
capture. `t` never jumps backward. Core Audio host-time discontinuities
are resolved to session-wall-time at the realtime thread boundary. AX
offset estimated in the first 60s is **re-estimated over the next 30s
following a device change** (not invalidated blindly).

---

## 5. Build plan — 17 weeks (88 working days)

This section describes the **phases** of the v1 build at the
architecture level. The day-by-day execution layer — specific tasks,
acceptance criteria, file paths, code stubs — lives in
[`implementation.md`](implementation.md) (§22 has the full week-by-week
budget). The numbered "Week N" headings below are phase labels, not
calendar weeks; a single phase often spans more than one calendar
week in `implementation.md`.

The timeline grew from 12 to 17 weeks during plan revision after a
multi-agent review surfaced under-budgeted work in week 0 spike,
WhisperKit Swift bridge, fixture ground-truth labeling, merge-on-write,
permission onboarding, and dogfood weeks. The 12-week framing in the
v2 revision history reflects an earlier estimate; v3 is honestly 17.

### Week 0: AXObserver feasibility spike (3 days, numeric thresholds)

**Question.** Does `AXObserver` on Zoom's participant window emit
useful speaker events with display names, stably enough to anchor
speaker attribution for v1?

**Method.** Swift command-line binary attached to running Zoom.

Day 1 (gallery view baseline): walk the a11y tree, identify speaking
indicator element, register for `kAXValueChangedNotification`. Log
`{wall_time, host_time, participant_name, state}` for 10 minutes on
a 4-person call. Measure: event latency, name coverage, speaker
count.

Day 2 (edge cases): re-run on
- active-speaker view (not gallery),
- paginated gallery (>9 participants),
- dial-in participant (call your own cell),
- **shared-screen mode** (someone screen-shares during the call;
  participant tiles reflow),
- **tile rename mid-call** (participant edits display name live).

Day 3 (quantification): run the polling backend (50ms
`AXUIElementCopyAttributeValue`) on a 30-min call, measure sustained
CPU with and without other apps running. Run against two Zoom
versions (latest + one prior).

**Exit criteria — numeric, filled in during the spike.**

| Metric | Green if | Yellow if | Red if |
|---|---|---|---|
| AX event registration | Succeeds on gallery speaking indicator | Succeeds but flaky across sessions | Fails entirely / synthetic element not observable |
| Gallery-view name coverage | ≥95% of turns have a name | 70–95% | <70% |
| Active-speaker-view name coverage | ≥90% | 60–90% | <60% |
| Paginated gallery coverage (off-screen speakers) | N/A (not needed for green) | ≥50% of off-screen turns named | <50% |
| Dial-in participant name quality | Real name present | Present but often `""`, `"iPhone"`, `"Unknown"` | Never labeled |
| Event → speech onset latency (p95) | <250ms | 250–700ms | >700ms or unstable |
| Polling backend sustained CPU (single core) | N/A (observer works) | <5% on M-series, <8% on Intel | >8% or fan-spinning |
| Shared-screen impact | No observable impact | Events pause during share but resume | Events lost during share |
| Cross-version stability (N=2) | Both work identically | One works, one flaky | One works, one fails |

**Resolution rules.**
- All-green row or green majority with no red → **Green.**
- Any red in "registration" or "event latency" → **Red.**
- Otherwise → **Yellow.**

**Decision actions.**
- **Green (~20%).** Proceed weeks 5–6 as written.
- **Yellow (~65%, modal).** Proceed; schema already supports
  `speaker_source: "channel"` fallback. Polling backend built as
  first-class option if observer is the "flaky" one.
- **Red (~15%).** Re-plan: ship v1 with `speaker: "them"` only and
  defer name attribution to v1.1 via ECAPA-TDNN voice clustering.
  Timeline slips ~2 weeks; §1 quality promise downgrades to
  "Granola-parity speaker attribution with Fireflies-quality transcripts."

**Spike deliverables.**
- Completed table of measured values against thresholds.
- **Yellow-case fixture set** (paginated gallery, dial-in,
  active-speaker view, shared-screen) — 3–5 short recordings +
  SpeakerEvent logs committed under `crates/heron-zoom/fixtures/`.
  These anchor the alignment-algorithm regression tests in wks 5–6.
- Written go/no-go memo referencing the measured table.

**Yellow is the default assumption in the rest of this plan.**

### Week 1: foundations + AEC + code-signing

- `heron-types` crate: full `Event` enum per §4.2, `Turn`,
  `SpeakerEvent`, `Frontmatter`, `Cost`, `Duration` types. Serde
  round-trip tested. A comment at the top of `lib.rs` states the
  "no events outside this crate" invariant.
- `heron-audio` skeleton: process tap (`cidre`) + mic (`cpal`) + APM
  wired with **tap as reverse reference**. Threading per §4.3. Bounded
  channels. Done-when: short test (play YouTube in Zoom window, mic
  unmuted with speakers on) shows **no echo of the tap content in the
  mic_clean stream**.
- Code-signing: Developer ID cert installed; empty Tauri shell
  notarizes successfully on GitHub Actions. Prove the pipeline
  round-trips.
- Keychain spike: store + retrieve dummy API key with `kSecAttrAccessControl`
  setting `kSecAccessControlPrivateKeyUsage` and `kSecAccessControlApplicationPassword`
  bound to the signed bundle ID. Verify a different signed app can't
  read the key.
- `claude -p` smoke test (parallel, 30 min): send a dummy transcript,
  verify `--output-format json` output is stable and parseable.
- `~/Library/Logs/heron/` and `~/Library/Caches/heron/sessions/<id>/`
  created with mode 0700 / 0600.

**Observability from day 1.** `tracing` + a per-session
`heron_session.json` log line capturing: capture duration,
dropped-frame count, AX events received, AX→turn match rate, STT wall
time, summarize wall time, summarize tokens, cost, AX backend in use
(observer / poll). Mode 0600.

### Weeks 1–2: audio capture end-to-end (continued)

- Disk-backed ringbuffer: captured frames stream to
  `~/Library/Caches/heron/sessions/<id>/{mic,tap}.raw` at mode 0600.
  Memory cap ~60s; rest spills to disk.
- **Session state file** at
  `~/Library/Caches/heron/sessions/<id>/session.json`, updated on every
  state transition (not timer-based): `{ status, started_at, last_frame_ts,
  last_mute_ts, device_history[], backend_info }`. Mode 0600. Replaces
  the vague "checkpoint every 5 min" from v2.
- Audio device-change handler: headphones plug/unplug, Bluetooth
  connect/disconnect → graph teardown + rebuild per §4.3
  device-change-monotonicity rule. Emits `AudioDeviceChanged`.
- Mic mute/unmute: emit `MicMuted`/`MicUnmuted`. Down-stream filter:
  any STT turn on the mic channel during a muted window is dropped
  (heuristic; user mics sometimes pick up muted voice via Zoom's own
  VAD bug).
- Graceful-quit trap: intercept ⌘Q while recording, prompt
  ("Finalize and save, or discard?").
- Crash recovery: on app start, scan
  `~/Library/Caches/heron/sessions/` for orphaned sessions (status !=
  "finalized"); offer to salvage each. Age > 24h → auto-prune (user can
  opt out in settings).
- `heron-cli record --app us.zoom.xos --out recordings/test.wav`.

**Done when.** Real 30-min Zoom call produces two clean WAV files
(mic.wav, tap.wav), aligned within 10ms, AEC verifiably suppresses
far-end echo in the mic stream, device change mid-call does not corrupt
the session, ⌘Q mid-call saves cleanly, simulated crash (SIGKILL)
mid-call is salvageable on restart, simulated 0.3× realtime STT
consumer (sleep-injected) on a 60-min input triggers `CaptureDegraded`
not silent frame drop.

**Fixture capture.** End of week 2: save 2–3 reference (mic, tap) WAV
pairs + `SpeakerEvent` logs from real calls into
`crates/heron-speech/fixtures/`. These are the STT regression corpus
for the rest of the project.

### Weeks 3–4: transcription, channel-labeled

- `heron-speech` crate.
  - `SttBackend` trait. Two implementations:
    - `WhisperKitBackend` (default on macOS 14+ Apple Silicon). Swift
      helper, ANE, English.
    - `SherpaBackend` (fallback: Intel, or WhisperKit unavailable,
      or WhisperKit WER > sherpa + 5% on fixtures).
  - Runtime chooses WhisperKit if available + model present, else sherpa.
  - VAD always sherpa-onnx (Silero) regardless of STT backend.
  - **Incremental JSONL emission.** Each finalized turn is appended to
    `transcripts/<id>.jsonl.partial` (one line, fsync'd in batches of
    10 or every 5s, whichever sooner). `transcripts/<id>.jsonl` is
    produced by atomic rename at STT completion.
- Fixture regression: every PR runs fixtures through both backends,
  asserts WER ≤ previous best per backend.
- Emit `Turn { t0, t1, text, channel, speaker, speaker_source, confidence }`
  per `heron-types`. At this stage every turn is `speaker_source: "self"`
  (for mic) or `speaker_source: "channel"` (for tap, `speaker: "them"`,
  `confidence: null`).

**Done when.** Transcript of a real call is human-readable, `"me"` vs
`"them"` correct, timestamps aligned within 200ms, fixtures-as-
regression-tests pass, WhisperKit WER ≤ sherpa on the English fixtures,
simulated mid-STT SIGKILL leaves a valid `.partial` that resumes
correctly on restart.

### Weeks 5–6: AXObserver → real speaker names

- `heron-zoom` crate. Swift helper compiled as static lib +
  `swift-rs` bridge, following the `stack.md` calendar pattern.
- Two backends, chosen at runtime:
  - `AXObserverBackend` — subscribe to `kAXValueChangedNotification`;
    use when registration succeeds.
  - `AXPollingBackend` — poll `AXUIElementCopyAttributeValue` every
    50ms; fallback when synthetic elements don't support observation.
- Self-name detection: read own Zoom display name at session start;
  filter events where `name == self` (handles talking-while-muted and
  user's own tile firing).
- View-mode detection: gallery vs active-speaker; emit
  `AttributionDegraded` warning if in active-speaker view.
- Shared-screen detection (per week 0 fixture): log AX event gap
  during active screen share.
- Emit `SpeakerDetected { event: SpeakerEvent { t, name, started,
  view_mode, own_tile } }`.

**Timestamp alignment (budgeted 2–3 days).**

1. Expand each `SpeakerEvent` to `[t_event - max_lag_est,
   t_event + min_lag_est]` after offset estimation.
2. Offset estimation: first 60s of session, correlate tap audio-energy
   envelope with SpeakerEvent transitions; fit median offset.
   Re-estimate over the 30s following any `AudioDeviceChanged` event.
   If estimation fails (too few events), fall back to
   `event_lag = 350ms` prior.
3. For each `tap` `Turn`: pick `SpeakerEvent` with max overlap;
   `confidence` = overlap_fraction × exp(-|delta| / 2s).
4. If `confidence < 0.6`: `speaker = "them"`, `speaker_source =
   "channel"`, `confidence = null`. Record low-confidence attempt in
   session log.
5. If no `SpeakerEvent`s received for >30s while tap is non-silent:
   emit `AttributionDegraded`, downgrade session `diarize_source` to
   `"channel"` in frontmatter.

- Regression tests run against the week-0 yellow-case fixtures
  (paginated, dial-in, active-speaker, shared-screen).

**Done when.** On a 3-person Zoom call, ≥70% of `tap` turns get a
real name with `confidence >= 0.7`; alignment handles a deliberate
2-minute quick-fire back-and-forth without collapsing all turns to
one speaker; week-0 fixtures pass.

### Weeks 7–8: summarization + markdown writer

- `heron-llm` crate.
  - `Summarizer` trait with 3 backends:
    - `AnthropicApi` (**default**). `anthropic` crate; Sonnet 4.6 with
      prompt caching (system prompt = templates; user message =
      transcript). Opus 4.7 for long meetings (>90min). Returns token
      counts + computed USD cost.
    - `ClaudeCodeCli` (opt-in). `claude -p ... --output-format json`.
      Parse-with-fallback. UI warning: "uses your Claude Code
      subscription; rate limits apply."
    - `CodexCli` (alternate). `codex exec ...`.
  - Handlebars templates in `crates/heron-llm/templates/*.hbs`:
    - `meeting.hbs` — one template, branches on `meeting_type`.
  - Returns `Summary { frontmatter_patch, body_markdown, cost }`.
- `heron-vault` crate.
  - `calendar_read_one_shot()`: EventKit helper. **If Calendar
    permission is not granted, returns `Ok(None)` immediately, never
    prompts, never blocks.** Vault writer tolerates `None`: attendees
    becomes `[]`, meeting title falls through to window-title →
    `"untitled"`, company stays whatever the LLM inferred from the
    transcript.
  - Atomic markdown write (temp file + `rename(2)`).
  - **Merge-on-write rule** (for re-summarize):
    1. Load existing `.md` if present; parse frontmatter + body.
    2. Compute `frontmatter_merged`:
       - heron-managed fields (`date`, `start`, `duration_min`,
         `source_app`, `recording`, `transcript`, `diarize_source`,
         `disclosed`, `cost`) → new values from session/summary,
         overwrite.
       - LLM-inferred fields (`company`, `meeting_type`, `attendees`
         when Calendar didn't fill, `action_items`, `tags`) →
         3-way merge: if user edited since last summarize
         (detected by comparing to `.md.bak`), keep user edits;
         otherwise overwrite with new LLM output.
       - Unknown fields → preserve as-is (user-added).
    3. Body: if `.md.bak` exists and current body != last-written
       body, keep user's current body (user edited); otherwise
       overwrite with new LLM body.
    4. Rotate: `.md` → `.md.bak`, write new `.md`.
- **Ringbuffer purge**: after the m4a is verified (ffprobe reports
  expected frame count), purge `~/Library/Caches/heron/sessions/<id>/`
  entirely. On ffprobe mismatch, keep the ringbuffer; surface salvage
  banner.
- `heron-cli summarize transcripts/<id>.jsonl` → writes everything.

**Done when.** `weekly-client-summary` skill runs unmodified on a
week of heron output; cost block in frontmatter matches the
Anthropic API response's `usage` field exactly (the dashboard is
audit-only and lags by minutes — not used for verification);
re-summarizing a meeting after editing action items preserves the
edits via the Ownership Model; ringbuffer directory is gone after
a successful session.

### Week 9: Tauri shell + 5-step onboarding

- Tauri v2 shell, menubar-only (no dock icon by default).
- **First-run onboarding walks 5 steps:**
  1. **Microphone** (TCC prompt).
  2. **System audio** (process tap TCC prompt, 14.2+).
  3. **Accessibility** — cannot be programmatically prompted. UI
     opens System Settings → Privacy → Accessibility, shows screenshot
     with an arrow pointing at the list, provides a "Test" button that
     tries to read Zoom's AX tree and reports success/failure.
  4. **Calendar** (optional). "Enable to auto-fill attendees and
     meeting titles. You can skip and enable later." Skipping is
     first-class: skipped state persists; attendee auto-fill is off;
     no repeat prompts.
  5. **Download WhisperKit model** (~1GB). Progress bar; cancelable;
     on cancel, heron falls back to sherpa-onnx (100MB, bundled with
     the app). On slow wifi, user can choose "use sherpa instead"
     from this screen.
  - Each step has a "Test" button that confirms the permission works
     before moving to the next. Completion state persisted; onboarding
     not re-shown on restart.
- Free-disk check: refuse to record if <2GB free.
- `heron://` URL scheme: `heron://salvage/<id>` opens the salvage
  flow for a specific orphaned session.

### Week 10: recording UX + crash recovery UI

- Start/stop hotkey (default `⌘⇧R`, configurable; conflict warning
  on save).
- Status indicator in menubar: idle / recording / transcribing /
  summarizing / error.
- Consent UX: on recording start, tray icon pulses red and a
  one-time-per-call reminder banner appears ("Did you tell the
  room?") before audio begins. User clicks "Yes, go" / "Remind me
  in 30s" / "Cancel". Disposition stored in `disclosed` frontmatter.
- Crash recovery: on app start, scan
  `~/Library/Caches/heron/sessions/` for status != "finalized" dirs;
  show a salvage list, user picks which to recover and which to
  purge.
- Error states (`SummaryFailed`, `CaptureDegraded`, etc.) bubble to
  tray UI with actionable copy ("Tap lost Zoom at 00:42:15 — transcript
  may have gaps in that window").

### Week 11: post-meeting review UI

- TipTap editor inside Tauri window; opens the just-written `.md`.
- **Audio playback** bar scrubs the recording. **Until the m4a is
  encoded, playback uses the WAV files directly via a `file://`
  handler**; switches to m4a once ffprobe-verified. One-off complexity
  but eliminates the race where a user clicks play before encode
  finishes.
- Click a transcript turn → jumps playback to `t0`.
- Low-confidence speakers rendered in italics per §3.4 UI rule.
- Edits to the body write back to `.md` atomically on blur / ⌘S.
- Re-summarize button: runs summarizer again, applies merge-on-write
  per the Ownership Model. **Diff view before accepting deferred to
  v1.1** — v1 just confirms then applies; `.md.bak` is the rollback.
- Per-session diagnostics tab: AX hit rate, AEC event counts,
  summarize cost, error log. Ugly but informative — real product QA
  surface.

### Week 12: settings, polish, dogfood bake

- Settings pane: vault path, target app bundle IDs, summarizer
  backend (Anthropic API / Claude Code CLI / Codex CLI + model
  choice), API key (Keychain), hotkey, disclosure reminder on/off,
  audio retention policy, WhisperKit-vs-sherpa override, Calendar
  re-enable, free-disk threshold.
- Disk-space: show current vault size; "purge audio older than N
  days, keep transcript + md" command.
- Budget: 2 days polish + 3 days exec-friend dogfood + bug-fix
  buffer.

**Ship-or-not gate (any "yes" → don't ship v1.0; cut scope or fix):**
- Crash during normal use.
- Session lost (audio + transcript both irrecoverable).
- First-run onboarding took >20 min for an unaided non-technical user
  (relaxed from <15 min after empirical onboarding-friction
  measurement; <15 min remains the *engineer-time* target in
  `implementation.md` §13.6).
- Exec-friend couldn't complete a meeting → note flow unaided.
- AEC regression — measurable echo of tap content in `mic_clean`
  on a fresh fixture (peak normalized cross-correlation >0.15 in the
  AEC test rig).
- Cost exceeded $2 on any single meeting <60 min.

**Success signal:** ≥1 follow-up email authored from a heron-generated
note during the dogfood week.

---

## 6. Pre-code decisions needing confirmation

1. **Vault path default.** `~/Documents/heron-vault` on first run.
2. **Default hotkey.** `⌘⇧R` — configurable; warn on system-hotkey
   conflict.
3. **Audio format on disk.** `.m4a` (AAC 64 kbps, VBR) for archival;
   `.wav` intermediate, deleted only after successful m4a encode + verify.
4. **Audio retention default.** Keep all indefinitely. Settings toggle
   "purge audio older than N days" — default off, exposed in week 12.
5. **Summarizer model.** Sonnet 4.6 default; Opus 4.7 auto-selected
   for sessions >90 min. Configurable.
6. **Telemetry / analytics.** None. `~/Library/Logs/heron/` stays
   local. Revisit if we ship beyond the exec-friend dogfood.
7. **Threat model.** Host compromise is out of scope. FileVault + Keychain
   bundle-ID ACL is the v1 posture. Documented in §1 so users know.
8. **Calendar default.** Disabled by default. Step 4 of onboarding is
   optional. Keep low friction for the common case (user skips,
   filename uses window title).

---

## 7. Risks

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| AXObserver yellow-modal: flaky on paginated gallery / dial-ins / shared-screen | **High** | Medium | `speaker_source: "channel"` fallback in §3.4; `confidence` exposes gaps; yellow-case fixtures anchor §5 wks 5–6 regression |
| AXObserver red: doesn't fire on Zoom's synthetic elements | Medium | High | `AXPollingBackend` built in §5 wks 5–6; if both fail, channel-only v1 + voice clustering in v1.1 |
| AEC misconfigured silently (mic echoes tap) | Low (flagged, gated) | High | Explicit correctness test in §5 week 1 done-when |
| Timestamp alignment sloppy → wrong speaker labels | Medium | High | 2–3 day budget + algorithm in §5 wks 5–6; `confidence` surfaces bad cases; regression via yellow-fixtures |
| Device change mid-call breaks alignment | Medium | Medium | §4.3 monotonicity rule + AX offset re-estimation in the 30s following `AudioDeviceChanged` |
| TCC Accessibility cannot be programmatically prompted | Certain | Medium | Dedicated week-9 onboarding step with Test button |
| WhisperKit ~1GB download blocks first-run | **High** on exec laptops | Medium | Onboarding step 5; sherpa-onnx bundled as immediate fallback |
| `~/Library/Caches/` cache exposes client audio on laptop loss | Low (given FileVault) | **High** if exfiltrated | Mode 0600 + purge-on-m4a-verify + 24h auto-prune for orphans; threat model states host compromise is out of scope |
| Long-call memory / disk pressure (3+ hours) | Medium | Medium | Disk-backed ringbuffer; 60s memory cap; `StorageCritical` event at <1GB free |
| STT backpressure: STT lags capture → buffer fills | Medium (Intel) | Medium | §4.3 bounded channel; `CaptureDegraded` event; ringbuffer retains full audio regardless |
| Mid-call audio device change corrupts session | Medium | High | Device-change handler + monotonicity rule §4.3 |
| App crash mid-recording | Low | Medium | Ringbuffer salvageable; startup scan in §5 wk 10 |
| App crash mid-STT → lost transcript | Low | Medium | Incremental `.partial` append in §3.5 |
| User quits ⌘Q mid-recording | Medium | High | Graceful-quit trap in §5 wks 1–2 |
| Claude Code CLI `--output-format json` unstable | Medium | Low | No longer default; parse-with-fallback |
| Anthropic API cost unexpectedly high | Low | Low | Cost block in frontmatter; Opus only for >90min |
| Two-party-consent legal blowback | Low | High | §2 row 18 disclosure UX + `disclosed.{stated,when,how}` audit trail |
| Code-signing / notarization delays | Medium | Medium | Set up in week 1 (§2 row 23) |
| Dropbox/Drive partial-sync corrupts a file | Low | Medium | All writes atomic temp+rename |
| WhisperKit Apple Silicon only → Intel Mac blocked | Low | Low | Runtime fallback to sherpa |
| sherpa-onnx English quality below WhisperKit | Low | Medium | Trait abstraction; Intel path documented |
| Zoom breaks AX tree across version | Medium | Medium | Version-gated branches in `heron-zoom`; multi-version fixtures |
| Calendar denial causes hang | **Was** medium; mitigated | (n/a) | `calendar_read_one_shot()` returns `Ok(None)` immediately (§4.1, §5 wks 7–8) |
| Re-summarize wipes user edits | **Was** high; mitigated | (n/a) | Merge-on-write rule §5 wks 7–8 |
| Playback race before m4a encode finishes | **Was** medium; mitigated | (n/a) | WAV fallback in week-11 playback |
| `heron-types` drift (events invented elsewhere) | Low (gated by rule) | Medium | §2 row 20 invariant + `heron-types` PR requirement |
| Exec-laptop first-run exceeds 15-min patience | Medium | Medium | Step 5 of onboarding allows "use sherpa, skip download" escape hatch |
| Yellow-UX ships with ~30% italicized turns; feels "noisy" | Medium | Low–Medium | Acknowledged in §1 quality promise; dogfood week 12 validates; otherwise tighten threshold |

---

## 8. What's deferred to v2 (explicitly)

- Ambient session detection. `heron-session` crate.
- MCP server and `heron-events` socket API. (`heron-types` already
  owns the vocabulary — v2 is a transport wrap.)
- Windows / Linux. WASAPI / PipeWire, different process-tap story.
- iOS / Android companion (viewer). `heron-bindings` (UniFFI).
- Meet / Teams. WebRTC-track interception via embedded WebView.
- Voice-embedding enrollment / ECAPA clustering (Approach 3).
- Cross-device pair listening (Approach 4).
- Post-hoc Zoom "separate audio files" reconcile (Approach 5).
- Local LLM summarization (Ollama / LM Studio).
- Code-switch (zh-en) via SenseVoice.
- Any backend / cloud sync owned by heron.
- Telemetry / analytics.
- Beta / canary update channels.
- **Auto-update.** Tauri updater pipeline exists (week 1 notarization
  proves it), but no signed update manifest is shipped in v1. Users
  get updates by re-downloading. Adds in v1.1.
- **SQLite index.** The filesystem-as-agent-surface model means queries
  are `grep` / `rg` over the vault. An index becomes necessary only
  when query latency bites, which it won't at v1 volumes.
- **Speaker-name aliasing** (user renames "Bob" → "Robert" in one
  note; should future sessions auto-learn). Keep the explicit rename
  local to the `.md` file in v1; alias map is v1.1.
- **Ringbuffer encryption at rest.** Mode 0600 + purge-on-verify +
  explicit threat model are the v1 posture. Full encryption (per-install
  key in Secure Enclave) adds ~1 week and protects against a threat
  model that's already out of scope (local process compromise).

---

## 9. Biggest remaining uncertainties

In priority order, post-v3 revision:

1. **AXObserver green / yellow / red.** Calibrated probability:
   20 / 65 / 15 (down-shifted green after oracle review #2).
   Yellow is acceptable; schema handles it. Red drops to Granola-parity
   for v1, voice clustering queued for v1.1. Week-0 spike with the
   numeric thresholds in §5 resolves this with 3-day decisiveness.
   Ambiguity rate: expected <15% with the threshold table.

2. **Can a non-technical exec complete the 5-step onboarding unaided?**
   Step 3 (Accessibility) is the hard one — TCC can't be prompted.
   Step 5 (WhisperKit download) adds friction on slow wifi, mitigated
   by sherpa-fallback option. We learn in week 12 dogfood.

3. **Is `AXPollingBackend` CPU-acceptable on M-series?** Plan says
   <5% sustained; spike measures. If higher, the "red" bucket expands
   because observer-fallback-to-polling is the main yellow recovery.

4. **Does the yellow-UX (~30% italicized "them" turns, ~70% real
   names) feel shippable to an exec, or does it look broken?**
   Subjective; week 12 dogfood validates. §1 quality promise is
   written honestly to set expectations.

5. **Does the purge-on-verify + threat-model-disclaimer security
   posture satisfy an exec with real compliance exposure?** Likely
   yes for personal use; a corporate rollout may demand encryption-
   at-rest. v1.1 item if dogfood surfaces the need.

Claude Code CLI stability is no longer in the top 5. Auto-update
risk is out-of-scope for v1.
