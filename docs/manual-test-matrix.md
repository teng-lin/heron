# Manual test matrix

Every needs-human gate in v1 lives here. Tests tagged `[needs-human]`
are not run by CI (per [`docs/implementation.md`](implementation.md)
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

This is the live-call runbook for the §9 AXObserver bridge. It is
the human procedure that pins the placeholder
`(role, subrole, identifier)` triple in
`swift/zoomax-helper/Sources/ZoomAxHelper/ZoomAxHelper.swift` to
real values, then exercises the full Rust + Swift wiring
end-to-end against a Zoom call.

### 1. Capture the speaker-indicator triple (one-time)

Until this step is run, the placeholders in `ZoomAxHelper.swift`
(`SPEAKER_INDICATOR_ROLE`, `SPEAKER_INDICATOR_SUBROLE`,
`SPEAKER_INDICATOR_IDENTIFIER`) are guesses. They will not match
anything in the real Zoom AX tree.

1. Start a Zoom call (or join Zoom's "Test meeting" at
   <https://zoom.us/test>) in gallery view with at least 2
   participants.
2. Launch Xcode → Open Developer Tool → Accessibility Inspector.
3. In the Inspector's target picker, select the Zoom process.
4. Activate the "inspection pointer" and hover the speaker
   indicator (the colored frame around the active speaker's
   tile).
5. Read off the **Role**, **Subrole**, and **Identifier** fields
   from the Basic panel. Note them down alongside the Zoom version
   (`zoom.us → About Zoom`).
6. Repeat in active-speaker view and paginated gallery; if the
   triple changes between modes, file a follow-up — the bridge
   currently expects one stable triple.
7. Edit `swift/zoomax-helper/Sources/ZoomAxHelper/ZoomAxHelper.swift`,
   replacing the three `SPEAKER_INDICATOR_*` constants with the
   captured values, and remove the `TODO(spike-fixture)` comments.
8. Confirm the matching notification: in Accessibility Inspector
   click "Subscribe to Notifications" → toggle the speaker on/off
   → confirm `AXValueChanged` fires (or note the actual
   notification name and update `SPEAKER_INDICATOR_NOTIFICATION`).
9. Capture the artifacts under `fixtures/zoom/spike-triple/` per
   row #1 in the table above so the values are auditable.

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

With Zoom in a meeting and someone (probably you) talking:

```sh
HERON_ZOOM_RUNNING=1 cargo test -p heron-zoom \
    --test ax_observer_real -- --ignored --nocapture
```

Pass criterion: at least one `SpeakerEvent` arrives within 5
seconds, the test prints it via `--nocapture`, and `stop()`
returns cleanly. Failure modes:

- `Err(ZoomNotRunning)`: Zoom isn't running under bundle id
  `us.zoom.xos`. Check `lsappinfo list | grep -i zoom`.
- `Err(AccessibilityDenied)`: re-do step 2.
- "no SpeakerEvent received in 5s": the AX triple is wrong (most
  likely cause: step 1 hasn't been run since a Zoom update). Re-run
  step 1 against the current Zoom version.

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
