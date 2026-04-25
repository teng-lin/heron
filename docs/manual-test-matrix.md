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
