# Manual test matrix

Every needs-human gate in v1 lives here. Tests tagged `[needs-human]`
are not run by CI (per [`docs/archives/implementation.md`](implementation.md)
§19.6); this file is the single source of truth for what they are,
when they happen, who runs them, and where the artifact lives.

Schema: `# | Section | Test name | Owner | When | Pass criterion | Artifact location`

| # | Section | Test name | Owner | When | Pass criterion | Artifact location |
|---|---|---|---|---|---|---|
| 1 | §3.3 | week-0 fixture capture | engineer + partner | week 0 | 5 fixture cases (gallery, active-speaker, paginated, dial-in, shared-screen, tile-rename) recorded with `mic.wav`, `tap.wav`, `ax-events.jsonl`, `ground-truth.jsonl` per case | `fixtures/zoom/<case>/` |
| 2 | §3.4 | ground-truth labeling | engineer | week 0, after #1 | each fixture case has a `ground-truth.jsonl` with one row per turn (start, end, speaker name) | `fixtures/zoom/<case>/ground-truth.jsonl` |
| 3 | §5.5 | onboarding TCC reset smoke | engineer | end of week 1 | `scripts/reset-onboarding.sh` runs without error; TCC prompts re-appear on relaunch after reset | none (binary pass/fail) |
| 4 | §5.6 | code-signing identity check | engineer | end of week 1 | `codesign --sign "Developer ID Application: …"` of a hello-world binary succeeds and `codesign --verify` returns 0 | none (binary pass/fail) |
| 5 | §6.3 | AEC test rig | engineer + partner | week 2 (1 hr) | `mic_clean.wav` correlates < 0.15 with the test noise played by partner's machine; `mic.wav` correlates ≥ 0.5 | `fixtures/manual-validation/aec/<date>/` |
| 6 | §6.4 | notarization first-cycle | engineer | week 2 | `v0.0.0-signing-test` tag → CI uploads to notarytool → `xcrun stapler validate` returns OK | `fixtures/manual-validation/notarize/v0.0.0-signing-test.dmg` |
| 7 | §7.3 | device-change validation | engineer | week 3 | mid-session AirPods connect → `AudioDeviceChanged` event fires; alignment recovers within 30s; recording continues | `fixtures/manual-validation/device-change/<date>.mov` |
| 8 | §7.5 | week-3 fixture capture | engineer + partner | week 3 (2 hrs) | 3 reference call captures with full fixture dirs | `fixtures/zoom/ref-<n>/` |
| 9 | §9.5 | week-7 regression fixture | engineer + 3 partners | week 7 (1 hr) | 4-person Zoom call recorded; aligner confidence ≥ 0.7 on ≥ 70% of turns | `fixtures/zoom/regression-week-7/` |
| 10 | §11.4 | LLM cost calibration | engineer | week 9 | summarizer cost matches API-response totals exactly across 3 reference calls | none (logs only) |
| 11 | §13.5 | onboarding walkthroughs (12) | engineer | week 11 | 5 positive paths + 5 counter-tests + 2 edge cases each ≤ 5 min, each with a screencast | `fixtures/manual-validation/onboarding/<date>-<test>.mov` |
| 12 | §15.5 | review UI playback | engineer | week 13 | edit one turn's text, save, re-open — text is preserved; playback synchronizes to the JSONL timestamps | `fixtures/manual-validation/review-ui/<date>.mov` |
| 13 | §17 | personal dogfood | engineer | week 15 | author runs heron on every meeting for the week; bug list ≤ 5 P1; no data loss | `docs/dogfood-log.md` |
| 14 | §18 | exec dogfood + ship gate | exec-friend + author | week 16 | exec uses heron unaided for 5 client calls; quality promise met (§1 plan.md); ship-criteria §18.2 all green | `docs/dogfood-log.md` |
| 15 | §3.3 | AX speaker-indicator triple spike | engineer | pre-week-15 | characterize the `(role, subrole, identifier)` triple for the active-speaker tile, OR document that no stable triple exists and update `ZoomAxHelper.swift` accordingly. **Run 2026-04-25, outcome: no AX-readable speaker indicator in Zoom 7.0.0; bridge pivoted to mute-state attribution per `fixtures/zoom/spike-triple/README.md`.** | `fixtures/zoom/spike-triple/` |

## Status legend

- **Scheduled** — slot booked on calendar, not yet run.
- **Run** — test executed, pass/fail recorded with artifact link.
- **Skipped** — test deliberately not run; row updated with reason
  (e.g. risk-reducer branch removed the dependency).

## Adding a row

Every PR that introduces a `[needs-human]` test must also add a row
here. CI does not enforce this — code review does. The row goes in
section order.

## Live LLM smoke tests (heron-llm)

`crates/heron-llm/tests/live_api.rs` is a live smoke harness that
exercises each summarizer backend against the real upstream when
prerequisites are present, and skips cleanly otherwise. The harness
uses **runtime skip** (early `return` with `eprintln!("skipped: …")`),
not `#[ignore]`, so `cargo test` is always green and gets richer as
the developer's machine becomes more capable.

### Prerequisites

| Test | Prereq | Skip when |
|---|---|---|
| `live_anthropic_summarize_returns_non_empty` | `ANTHROPIC_API_KEY` env var | unset or empty |
| `live_claude_cli_summarize_returns_non_empty` | `claude` on `PATH`, `claude --version` exits 0, **and** the actual `summarize` call succeeds | binary missing, `--version` fails, or `summarize` returns an error (e.g. sandbox session perms, expired auth) |
| `live_codex_cli_summarize_returns_non_empty` | `codex` on `PATH`, `codex --version` exits 0, **and** the actual `summarize` call succeeds | binary missing, `--version` fails, or `summarize` returns an error (e.g. `~/.codex/sessions` permission denied, expired auth) |

The Anthropic test is stricter: once `ANTHROPIC_API_KEY` is set the
test will fail loudly on any error from the live API, since a bad
key or network problem is a real signal the user opted in to. The
CLI tests are more forgiving because `--version` cannot detect every
"installed but not usable" state — when the actual `summarize` call
fails we log a skip line with the error and return green. Unit tests
in `crates/heron-llm/src/{claude_code,codex}.rs` cover the
error-mapping contract; this harness only owns the happy path.

### Running

```sh
cargo test -p heron-llm --test live_api -- --nocapture
```

`--nocapture` is what surfaces the per-test `skipped: …` / `live …
ok: …` log lines. Without it the tests still pass; you just don't see
which path was exercised.

### Cost note

The Anthropic test issues a single Messages API call against the
default model (`claude-sonnet-4-6`) with a two-line synthetic
transcript. Real cost is on the order of a fraction of a cent per
run (well under $0.01). The user opts in by exporting the key — the
test will not silently spend money on a machine where the key is
absent.

The CLI tests consume the user's Claude Code / Codex subscription
quota rather than API credits.

### Verifying the skip path

On a machine with no key and no CLIs:

```sh
unset ANTHROPIC_API_KEY
PATH=/usr/bin:/bin cargo test -p heron-llm --test live_api -- --nocapture
```

All three tests should print `skipped: …` and pass.

## Zoom AX observer (heron-zoom)

This is the live-call runbook for the §9 AX bridge. It exercises
the full Rust + Swift wiring end-to-end against a Zoom call.

### 1. Validate the AXDescription contract (one-time per Zoom version)

The §3.3 spike (artifacts at `fixtures/zoom/spike-triple/`,
outcome at that directory's `README.md`) established that Zoom does
not surface an active-speaker indicator via Accessibility. The
bridge `swift/zoomax-helper/Sources/ZoomAxHelper/ZoomAxHelper.swift`
instead parses each participant tile's `AXDescription`:

```
"<Name>, Computer audio (muted|unmuted)[, Video (off|on)]"
```

A new Zoom release could break this format. Before relying on the
bridge against an unfamiliar Zoom version, re-run the spike capture
to confirm the contract still holds:

1. Join a Zoom call with ≥ 2 participants (laptop + phone, or a
   real partner). zoom.us/test by itself is single-participant and
   won't surface remote tiles.
2. Run the dump (build heron first if needed via
   `cargo build -p heron-cli`):
   ```sh
   DYLD_LIBRARY_PATH="$(pwd)/target/debug" \
     ./target/debug/heron ax-dump --bundle us.zoom.xos --out /tmp/ax.json
   jq -r '.nodes[] | select(.role == "AXTabGroup" and .depth == 2) | .description' /tmp/ax.json
   ```
3. Confirm the output looks like
   `<Name>, Computer audio (muted|unmuted)…` — one line per
   participant. If the format has changed, update
   `tileDescriptionRegex` in `ZoomAxHelper.swift` and re-archive a
   new fixture pair under `fixtures/zoom/spike-triple/`.

If you don't already have artifacts on disk, the existing fixtures
(`muted.json` / `speaking.json`) document what the spike found
against Zoom 7.0.0.

### 2. Grant Accessibility to the test binary (one-time)

`AXIsProcessTrustedWithOptions` returns false until the binary is
explicitly listed under System Settings → Privacy & Security →
Accessibility.

1. Build the test binary first so the path exists:
   `cargo test -p heron-zoom --test ax_observer_real --no-run`.
2. The binary lives at
   `target/debug/deps/ax_observer_real-<hash>` — find the latest:
   `ls -t target/debug/deps/ax_observer_real-*  | head -1`.
3. System Settings → Privacy & Security → Accessibility →
   `+` → navigate to that binary path → Enable.
4. Re-run the test. Each `cargo test` rebuild produces a new hash;
   you may need to re-grant or use a stable wrapper script.

### 3. Run the live test

With Zoom in a meeting (≥ 2 participants visible — see step 1):

```sh
HERON_ZOOM_RUNNING=1 cargo test -p heron-zoom \
    --test ax_observer_real -- --ignored --nocapture
```

Pass criterion: at least one `SpeakerEvent` arrives within 5
seconds, the test prints it via `--nocapture`, and `stop()`
returns cleanly. (The polling thread emits an event on its very
first walk, so being in a 2-participant meeting is enough — no
need to toggle anything.)

Failure modes:

- `Err(ZoomNotRunning)`: Zoom isn't running under bundle id
  `us.zoom.xos`. Check `lsappinfo list | grep -i zoom`.
- `Err(AccessibilityDenied)`: re-do step 2.
- "no SpeakerEvent received in 5s": the most likely cause is a
  single-participant meeting (no remote tiles to enumerate) or a
  Zoom version change that broke the AXDescription contract. Re-run
  step 1 to diagnose; if the contract has changed, update
  `tileDescriptionRegex` in `ZoomAxHelper.swift`. The fallback when
  the regex stops parsing is `Event::AttributionDegraded` from the
  aligner after `ATTRIBUTION_GAP_THRESHOLD` (30s) of silence — the
  recording still completes, but speaker attribution drops to
  channel-only.

## WhisperKit STT backend

The real WhisperKit `SttBackend` (see `crates/heron-speech/src/lib.rs`)
loads a CoreML-compiled Whisper model at runtime; the model itself is
not vendored in the repo. To run the smoke test:

1. **Download a model.** WhisperKit publishes pre-converted models on
   the [argmaxinc/whisperkit-coreml](https://huggingface.co/argmaxinc/whisperkit-coreml)
   HuggingFace repo. See the WhisperKit README's
   [Model Selection](https://github.com/argmaxinc/WhisperKit#model-selection)
   section for a chooser. A reasonable default for development is
   `openai_whisper-base.en`; for end-to-end accuracy work pick
   `openai_whisper-large-v3-turbo` or similar. Place the unpacked
   bundle on disk at any stable path, e.g.
   `~/Library/Application Support/heron/models/whisperkit/openai_whisper-base.en`.
2. **Point the env var.** Export
   `HERON_WHISPERKIT_MODEL_DIR=<that path>`. The path is the *folder*
   containing the `*.mlmodelc` bundles WhisperKit expects, not a
   parent directory.
3. **Run the test.**
   ```sh
   cargo test -p heron-speech --test whisperkit_real -- --nocapture
   ```
   When the env var is unset the test prints a notice and skips.

### Build-time network requirement

`swift build` for `swift/whisperkit-helper/` resolves the
`argmaxinc/WhisperKit` Swift package (pinned at `v0.18.0`,
commit `e2adabbe`) on first run. Network access to github.com (or a
configured Swift Package Registry mirror) is required at *build*
time. CI must be allowed network egress, or the resolved `.build/`
checkout must be vendored — vendoring is out of scope for the
scaffolding PR that introduced this section.

## Process-tap real-device runbook (§6.2)

The integration test
`crates/heron-audio/tests/process_tap_real.rs` exercises the real
Core Audio process tap against a live meeting client. It is
`#[ignore]`d by default because it needs hardware + TCC grants that
no CI runner has.

**Preconditions on the test machine.**
1. macOS 14.2 or newer (process tap API was added in 14.2).
2. The target meeting app installed and running. Default bundle id
   is `us.zoom.xos`; override via `HERON_PROCESS_TAP_BUNDLE_ID`
   (e.g. `HERON_PROCESS_TAP_BUNDLE_ID=us.zoom.xos.ZoomClips` for the
   helper, or `com.microsoft.teams2` for Teams).
3. Join a Zoom meeting that is actually playing audio — the test
   only asserts that the tap pipeline builds without TCC failure
   today (the IO-proc → broadcast pipe lands week 3), but a real
   meeting is what catches "tap built but produces silence" before
   it ships.
4. TCC grants:
   - **System Settings → Privacy & Security → Microphone** —
     allow the test runner's parent (Terminal / iTerm / VS Code).
   - **Privacy & Security → System Audio Recording** (14.2+) —
     same. If the section is missing, the laptop is < 14.2.
   - If running from a fresh checkout, the first invocation will
     pop the system-audio prompt; click Allow then re-run.

**Running the test.**
```sh
HERON_PROCESS_TAP_REAL=1 \
  cargo test -p heron-audio --test process_tap_real -- --ignored --nocapture
```

Without `HERON_PROCESS_TAP_REAL=1` the test prints a SKIPPED line
and exits 0; that's the CI behavior. With the env var set:
- Pass: prints "process_tap_emits_at_least_one_frame" with no
  panic. Today this means "tap + aggregate device built without
  TCC failure"; once the week-3 IO proc lands, it means "≥ 1
  CaptureFrame arrived in 2 s".
- Fail (`PermissionDenied`): TCC not granted — see preconditions.
- Fail (`ProcessNotFound`): meeting app not running. Launch it
  (or override bundle id) and re-run.

**Resetting TCC** for a clean re-run (e.g. after revoking via the
Settings UI to test the prompt path):
```sh
tccutil reset Microphone $(id -un)
tccutil reset SystemAudioRecording $(id -un)   # 14.2+
```
The next test run will re-prompt.

**When to promote this from `[needs-human]` to CI.** Once the §7
ringbuffer + IO proc patch lands and we have a synthetic Zoom-side
fixture that produces deterministic audio (the AEC test rig in §6.3
plays a similar role for AEC), this test gets a non-ignored
counterpart. Until then it stays here.

## Mic capture (heron-audio) real-device runbook (§6)

The integration test
`crates/heron-audio/tests/mic_capture_real.rs` exercises the real
cpal mic-input pipeline alongside (but independent of) the process
tap. It is `#[ignore]`d by default — it needs a real default input
device and TCC microphone permission.

**Preconditions on the test machine.**
1. macOS 14.2 or newer (matches v0's supported floor; cpal itself
   works on earlier versions, but the rest of `heron-audio` is
   pinned to 14.2+).
2. A default input device selected in System Settings → Sound →
   Input. The internal Apple Silicon mic is fine; external USB
   headsets and audio interfaces also work.
3. The default input device must support 48 kHz f32 input. This is
   the modern-Apple default; if the test fails with `Aborted: cpal
   build_input_stream failed`, switch inputs.
4. TCC grant:
   - **System Settings → Privacy & Security → Microphone** — allow
     the test runner's parent (Terminal / iTerm / VS Code).
   - First invocation may pop the system microphone prompt; click
     Allow then re-run.

**Running the test.**
```sh
HERON_MIC_CAPTURE_REAL=1 \
  cargo test -p heron-audio --test mic_capture_real -- --ignored --nocapture
```

Without `HERON_MIC_CAPTURE_REAL=1` the test prints a SKIPPED line
and exits 0; that's the CI behavior. With the env var set:
- Pass: at least one `Channel::Mic` `CaptureFrame` arrives within 5 s.
- Fail (`PermissionDenied`): TCC not granted — see preconditions.
- Fail (`Aborted: no default input device`): no input selected, or
  Bluetooth headset disconnected mid-test. Reconnect or switch
  inputs.

**Resetting TCC** for a clean re-run (e.g. after revoking via the
Settings UI to test the prompt path):
```sh
tccutil reset Microphone $(id -un)
```
The next test run will re-prompt.

**Mic-failure-doesn't-fail-the-session policy.** Note that
`AudioCapture::start` swallows mic failures (logs + emits
`Event::CaptureDegraded`, returns the handle with `_mic = None`).
This test calls `mic_capture::start_mic` directly so a TCC denial
or device error is observable as a panicking test rather than a
silent tap-only session.

**When to promote this from `[needs-human]` to CI.** Once a CI
runner with a virtual audio loopback (BlackHole or equivalent) is
available, this test gets a non-ignored counterpart that pumps a
known sine wave through the loopback and asserts on FFT energy.
Until then it stays here.

## AEC processor smoke (heron-audio)

Row #5 in the table above tracks the **end-to-end** AEC correctness
gate per [`docs/archives/implementation.md`](implementation.md) §6.3 — engineer
on Mac A, partner on Mac B, real Zoom call, correlation < 0.15
between `mic_clean.wav` and `tap.wav`. That test rig is intentionally
unchanged.

Independent of that, the standalone AEC processor itself
(`crates/heron-audio/src/aec.rs`) is exercised by **CI-resident unit
tests** that don't need a partner machine or a Zoom call:

```sh
cargo test -p heron-audio aec
```

These tests cover (a) APM construction succeeds, (b) RMS shrinks
after AEC convergence on synthetic 1 kHz speaker bleed, and (c) the
wrong-channel / wrong-frame-size guards reject mis-routed input
loudly. They link against the bundled WebRTC C++ source (built once
per CI run via `meson + ninja`, see `.github/workflows/rust.yml`).

**End-to-end gate is still §6.3** — the unit tests cannot replace it
because (a) APM's adaptive filter behaves differently against
synthetic tones than real speech, and (b) the §6.3 metric is the
contract with downstream STT. With the live-pipeline wiring in
place (mic + tap → APM → cleaned broadcast → per-channel WAVs at
stop), the §6.3 test rig is now runnable end-to-end. The unit
tests catch wiring regressions in the AEC processor without
blocking on partner availability.

### §6.3 AEC test rig — end-to-end runbook

The matrix row above (#5) is the engineer + partner exercise. With
the wire-up + WAV finalization landed, the procedure is now:

1. **Both machines.** macOS 14.2+, latest Zoom, TCC microphone +
   system-audio-recording granted to the test runner's parent
   process. Headphones **off** on Mac A (the mic must hear speaker
   bleed for AEC to have anything to subtract).
2. **Mac B (partner).** Joins a private Zoom meeting with Mac A and
   plays a known reference signal — the canonical choice is a
   30-second clip of pink noise at conversational volume, but a
   spoken-word podcast clip works too (it stresses the adaptive
   filter more than synthetic noise does). Stays muted otherwise so
   the only audio Mac A hears via the call is the reference signal.
3. **Mac A (engineer).** Runs the end-to-end harness against the
   live call:
   ```sh
   HERON_PROCESS_TAP_REAL=1 \
     cargo test -p heron-audio --test end_to_end_real -- --ignored --nocapture
   ```
   The harness drives a 2-second session, calls `stop()`, and
   asserts that `mic.wav`, `tap.wav`, and `mic_clean.wav` all
   exist with consistent frame counts.
4. **Locate the artifacts.** The harness prints the session
   directory under a `tempfile` path; copy the three WAVs out into
   `fixtures/manual-validation/aec/<YYYY-MM-DD>/` for the matrix
   archive. The engineer should also run a longer 30-second
   capture (modify the `tokio::time::sleep` line locally; do not
   commit the change) for the correlation metric below.
5. **Compute the §6.3 metric.** With the 30 s capture in
   `fixtures/manual-validation/aec/<date>/`, run any signal-
   correlation tool the engineer has handy (e.g. `numpy.corrcoef`
   on the time-aligned mono samples). Pass criterion per
   §6.3: `mic_clean × tap < 0.15` (echo successfully removed) and
   `mic × tap >= 0.5` (the raw mic *was* correlated, so the AEC
   actually did work — not just a silent input).
6. **Record the row.** Update the matrix's row #5 status to "Run"
   with the date, the two correlation values, and the artifact
   directory link.

The `end_to_end_real.rs` harness above is the lightweight
sanity-check used during development; it does not compute the
correlation metric. The full §6.3 row remains a `[needs-human]`
gate because it requires two machines and partner coordination.
