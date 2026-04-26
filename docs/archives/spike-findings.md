# Recall.ai spike findings — first live run

Companion to [`build-vs-buy-decision.md`](./build-vs-buy-decision.md) and
[`api-design-spec.md`](./api-design-spec.md). Captures what the live
run of `crates/heron-bot/examples/recall-spike.rs` discovered when
exercised against a real Zoom meeting on 2026-04-26.

The spike was the gate condition for committing to the Recall.ai
driver path per the build-vs-buy doc's "v2.0 (Path A — Recall)" plan.
This doc records the data so the build-vs-buy decision can be
re-evaluated against actual evidence rather than vendor research.

## Run metadata

| Field | Value |
|---|---|
| Date | 2026-04-26 (UTC) |
| Region | `us-west-2` (`https://us-west-2.recall.ai`) |
| Meeting platform | Zoom (`us06web.zoom.us`) — personal-use room |
| Meeting type | Open (no waiting room enforced once host joined) |
| API key auth | `Authorization: Token <key>` — confirmed working |
| Disclosure audio | 8s mono MP3 @ 64kbps (synthesized via `say` + `ffmpeg`) |
| Bot name | "Teng's AI assistant" |
| Findings file | `spike-findings.jsonl` (gitignored) — 13 entries |

## Latency table (Recall-side acceptance)

All measurements are wall-clock from operation issue to API ack
(NOT audibility — see "What we couldn't measure" below).

| Operation | Latency | HTTP | Outcome |
|---|---|---|---|
| `disclosure-inject` total | **37,766ms** | 200 | Inconclusive |
| ↳ `create_bot` | 657ms | 200 | Success |
| ↳ wait for `in_call_recording` | 36,574ms | — | (polling) |
| ↳ `output_audio` | 533ms | 200 | Success |
| `speak` (isolated) | 786ms | 200 | Inconclusive |
| `replace-test` (concurrent via `tokio::join!`) | 675ms | 200/200 | Inconclusive |
| `interrupt` | 338ms | 204 | Success |
| `leave_call` | 454ms | 200 | Success |
| `disclosure-inject-cleanup` (first run) | 449ms | 200 | Success |

The first run hit the 180s in-call timeout because the harness was
checking the wrong status-code prefix (see "Major API-shape
discovery" below); cleanup-on-failure fired correctly and avoided
the orphan. Second run completed end-to-end after the prefix fix
(PR #99).

## Per-invariant analysis

Cross-referenced against the 14 invariants in
[`api-design-spec.md`](./api-design-spec.md) §12.

| # | Invariant | Verdict | Notes |
|---|---|---|---|
| 1 | Vendor quirks live only in `heron-bot` | ✅ holds | Spike harness is vendor-coupled by design; the eventual `RecallDriver: MeetingBotDriver` impl honors this |
| 2 | Layers don't share types across more than one | ✅ holds | n/a in spike |
| 3 | Internal Rust APIs use typed handles, never strings | ⚠️ TBD | Spike uses raw `String` for `bot_id`; `RecallDriver` impl will introduce `BotId(Uuid)` |
| 4 | Composite keys / URLs are resolver inputs only | ✅ holds | Spike takes meeting URL as a CLI arg, returns a `bot_id` string per Recall's REST shape |
| 5 | No state outside `in_meeting` accepts speech-control | ⚠️ Recall-enforced | Recall's `output_audio` requires the bot to have been created with `automatic_audio_output` AND to be in-call. Wrong state returns 4xx |
| 6 | A bot without `DisclosureProfile` is rejected at create time | ⚠️ heron-side | Recall doesn't enforce; heron's `RecallDriver` will need to refuse `bot_create()` without disclosure |
| 7 | `heron-bot` is a singleton in v2.0 | ✅ holds | Spike singleton-by-default; `--max-concurrent-bots` not exposed |
| 8 | A bot without a persona is rejected at create time | ⚠️ heron-side | Recall accepts bare `bot_name`; heron must wrap |
| 9 | Every terminal state emits exactly one `bot.completed` event | ✅ verified | Live run emitted `recording_done` then `done` (terminal); only `done`/`fatal` are spec-terminal per Recall docs (`call_ended` is NOT terminal — `done` follows) |
| 10 | Pre-meeting context capped at 16K tokens | ⚠️ TBD | Spike doesn't exercise context injection |
| 11 | `Priority::Replace` is a single primitive | ❌ unsupported by Recall | Recall has no Replace primitive. Two concurrent `output_audio` calls were both accepted (HTTP 200 each); audibility-confirmed semantic still TBD |
| 12 | All events flow through `heron-events` first | ⚠️ TBD | Spike bypasses event bus; `RecallDriver` impl will publish |
| 13 | Trait canonical, transports are projections | ✅ holds | n/a in spike |
| 14 | Vendor-API discipline lives entirely in `heron-bot` | ✅ verified | Polish pipeline added: real `http_status` capture, error-body capture, distinct `RateLimit (429)` vs `CapacityExhausted (507)` variants, retry semantics |

## Major API-shape discovery

**Recall's REST `GET /bot/{id}/` returns `status_changes` codes
WITHOUT the `bot.` prefix that the webhook docs show.**

Doc research before the spike (Codex, verified against
[bot-status-change-events](https://docs.recall.ai/docs/bot-status-change-events))
gave us the prefixed form: `bot.in_call_recording`, `bot.done`,
`bot.fatal`. Live REST returns:

```
joining_call
in_waiting_room
in_call_not_recording
in_call_recording
call_ended
recording_done
done
```

The harness's `wait_for_in_call` checked
`code.starts_with("bot.in_call")` which never matched against the
REST shape. The first live run dispatched a bot, the bot reached
`in_call_recording` at +6.5s, but the harness gave up at +180s
believing the bot never made it in-call. Cleanup-on-failure fired
correctly (no orphan), but the disclosure never played.

PR #99 fixes this by accepting both the prefixed (webhook) and
unprefixed (REST) forms in `BotState::is_terminal`,
`wait_for_in_call`, and `cmd_watch_eject`. Three new tests pin
the discovery.

**Implication for `RecallDriver` impl**: any code that consumes
status_changes must handle both forms, OR normalize at the
boundary. The trait surface in `heron-bot` should normalize to a
single form internally.

## Sub-codes are populated and useful

Spec §7's `EjectReason` enum was speculative — earlier vendor
research suggested most platforms collapse to "ejected, reason
unknown." Recall does NOT collapse; `sub_code` is populated:

| Code | Sub-code observed | Meaning |
|---|---|---|
| `in_waiting_room` | `meeting_not_started` | Bot is admitted but host hasn't started the meeting yet |
| `call_ended` | `bot_received_leave_call` | Our `leave_call` POST triggered the end |

These sub_codes map cleanly to spec §7's `EjectReason` variants.
The full sub_code list is at [docs.recall.ai/docs/sub-codes](https://docs.recall.ai/docs/sub-codes)
and includes platform-specific variants prefixed `zoom_`, `google_meet_`,
`microsoft_teams_`, `webex_`.

**Implication for `RecallDriver` impl**: implement the full
`EjectReason` mapping table from Recall's documented sub_codes;
don't accept "unknown reason" as the default unless heron sees a
sub_code Recall didn't document.

## Disclosure-ordering reality check (spec §4 Invariant 6)

The spec requires "TTS must be initialized and have a voice loaded
BEFORE `bot_join()` returns success." Recall's flow makes this
*technically* honorable — the bot is in-call before `output_audio`
fires — but at a real cost:

- **Time from `bot_create()` ack to `in_call_recording`**: ~37s on
  the second run, ~6.5s on the first run (different waiting-room
  hold times)
- **Time from `in_call_recording` to `output_audio` ack**: 533ms

So total time from "user clicks join" to "bot first speaks" is
~7s (best case) to ~37s (worst case observed) on Zoom personal
rooms. For meetings with strict waiting rooms, this could be
unbounded.

**Implication for `RecallDriver` impl + spec §4**: the disclosure
guarantee in the spec is satisfiable on Recall but the user-visible
latency is non-trivial. The spec already accounts for this by
making `bot_create()` async and observing transition events, so
no spec change is needed — but the product UX needs to handle
the multi-second silent-bot-in-meeting window (e.g. show the user
"agent joining…" rather than implying instant disclosure).

## Spec §9 Replace semantics — INCONCLUSIVE

Two concurrent `output_audio` calls (via `tokio::join!`) both
returned HTTP 200 in 675ms. We do NOT yet know whether Recall:
- **Queued** them (both played in submission order)
- **Replaced** (only the latter played)
- **Played both** simultaneously (overlapping audio)
- **Rejected** one silently (only one was actually accepted)

Closing this requires audibility confirmation from a human in the
meeting. The spike harness records "both calls accepted by API"
which validates spec Invariant 11's stated finding (Recall has no
documented Replace primitive); the actual audible semantic is
still open.

**Action item**: re-run `replace-test` with a longer audio file
(~10s) so any overlap is audible, and document the observed
playback semantic.

## What we couldn't measure (needs human attestation)

The harness records Recall-side API acceptance, NOT actual
audibility. Three operations from the live run remain
**Inconclusive** in the JSONL until a human in the meeting confirms:

1. **`disclosure-inject`**: did the disclosure audio actually play
   in the meeting?
2. **`speak`** (isolated call): did the second standalone speak
   play after the disclosure-inject sequence?
3. **`replace-test`**: how did the two concurrent calls sound — one
   playback, two sequential, two overlapping, or one silent?

Closing these requires either:
- The user from the original run attesting (preferred)
- A re-run with a recording made of the meeting audio (Zoom can do this)

## Operations confirmed working end-to-end

- `bot_create` (with `automatic_audio_output.in_call_recording.data`
  populated to enable later `output_audio`)
- `get_bot` status polling
- Status-change history retrieval
- `output_audio` API acceptance
- `DELETE /output_audio` channel cancellation
- `leave_call` graceful exit
- Sub_code-rich state transitions
- Cleanup-on-failure (the polish-v2 guard prevented an orphaned
  bot on the first run's prefix-bug timeout)

## Recommendations for the eventual `RecallDriver` impl

Based on the live evidence:

1. **Normalize Recall status codes at the boundary.** Strip an
   optional `bot.` prefix on read so the rest of the code only
   sees one form. Round-trip mapping:
   `in_call_recording` → heron `BotState::InMeeting`,
   `done | bot.done` → terminal `BotState::Completed{outcome}`.
2. **Map sub_codes to `EjectReason` with a documented table.**
   Don't fall back to `Unknown` unless the sub_code is genuinely
   not in Recall's documented list.
3. **Surface the join-to-in-call latency as a metric.** Spec §4's
   guarantee is honored by waiting for `in_call_recording`; the
   product UX above this layer needs to show progress during the
   wait.
4. **`Priority::Replace` requires emulation.** Recall's
   `output_audio` has no Replace primitive — the impl must call
   `DELETE /output_audio` then `POST /output_audio` and accept the
   small audio gap. Spec Invariant 11 says this is wrong — but
   Recall doesn't give us a single-primitive option, so the
   `RecallDriver` either degrades transparently or we add a
   `SpeechCapabilities { atomic_replace: false }` and let policy
   degrade.
5. **Carry the placeholder MP3 into the impl.** Recall requires
   `automatic_audio_output.in_call_recording.data` at create time
   for `output_audio` to work later. The `RecallDriver` impl
   should embed a small silent MP3 (or take one from disclosure
   config) so callers don't trip on this gotcha.
6. **Capture HTTP status + error body in every API result** —
   the polish-v1 fix (`ApiOk { status, body }` wrapper +
   `ApiError` variants) is the right shape; carry into the impl.
7. **Distinct error variants for 429 vs 507** — Recall reserves
   507 for "warm-bot pool depleted on Create Bot" only. Other ops
   use 429. The impl should retry both but with different
   strategies (429: respect `Retry-After`; 507: poll every 30s).

## Re-evaluation against build-vs-buy gates

Per [`build-vs-buy-decision.md`](./build-vs-buy-decision.md)
"Reversibility — when to revisit", these are the trigger conditions
that would re-open the Path A (Recall) decision:

| Trigger | Status |
|---|---|
| Spike reveals spec-invariant violation | **None observed.** Invariant 11 (atomic Replace) was already known unsupported; everything else is honorable on Recall |
| Recall pricing changes materially | n/a (no change) |
| Native Zoom SDK terms change | n/a |
| Apple ships first-party meeting integration | n/a |
| Cross-platform OSS bot driver appears | n/a |
| User research invalidates proxy mode | **TBD** — depends on user feedback once disclosure audibility is confirmed |

**Conclusion**: the spike does not invalidate Path A. The next
gate is the `RecallDriver: MeetingBotDriver` implementation
itself, which should be its own follow-on PR per the migration plan.

## Outstanding action items

- [ ] Confirm disclosure / speak / replace-test audibility from
      the original run (or re-run with recording enabled)
- [ ] Re-run `replace-test` with a 10s audio file to make
      overlap audible
- [ ] Begin `RecallDriver: MeetingBotDriver` impl per the
      recommendations above
- [ ] Update `crates/heron-bot/Cargo.toml` to depend on `reqwest`
      as a primary dep (not just dev-dep) when the impl lands

## File references

- `crates/heron-bot/examples/recall-spike.rs` — the harness
- `spike-findings.jsonl` — gitignored, this run's output
- `disclosure.mp3` — gitignored, the disclosure audio
- [`docs/archives/api-design-spec.md`](./api-design-spec.md) — the
  invariants this spike validates against
- [`docs/archives/build-vs-buy-decision.md`](./build-vs-buy-decision.md) —
  the decision this spike informs
- PR #85 — original spike harness
- PR #99 — prefix-fix from this spike's first run
