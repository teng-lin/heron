# Backend evaluations

heron has three swappable LLM backends ([`docs/archives/implementation.md`](implementation.md)
§11.1) and two STT backends ([§8.1](implementation.md)). This document records the
evaluation criteria, the per-fixture WER / cost / latency numbers, and the
selection logic.

## STT — WhisperKit vs Sherpa

### Fixtures

The evaluation runs against the week-3 fixture corpus (§7.5). Each
fixture is a real Zoom call recorded by the engineer + a partner;
the ground truth is hand-labeled per turn.

| Fixture | Speakers | Duration | Conditions |
|---|---|---|---|
| `client-3person-gallery/` | 3 | ~25 min | Gallery view, all on wifi |
| `team-5person-with-dialin/` | 5 | ~35 min | One dial-in attendee, mixed bandwidth |
| `1on1-internal/` | 2 | ~30 min | High-quality audio both ends |

### WER thresholds (§8.5)

| Fixture | WhisperKit | Sherpa |
|---|---|---|
| `client-3person-gallery/` | ≤ 15 % | ≤ 22 % |
| `team-5person-with-dialin/` | ≤ 22 % | ≤ 30 % |
| `1on1-internal/` | ≤ 12 % | ≤ 18 % |

WER measured against ground-truth JSONL via `scripts/wer.py` (lands week 4).

### Selection (§8.6)

```rust
if !is_apple_silicon() || !is_macos_14_plus() {
    SherpaBackend
} else if fixtures_wer.whisperkit.avg() > fixtures_wer.sherpa.avg() * 1.05 {
    SherpaBackend
} else {
    WhisperKitBackend
}
```

Bias: prefer WhisperKit unless it's > 5 % worse than Sherpa on the
fixture suite; that 5 % cushion absorbs measurement noise.

### Status

**Not yet measured — fixture capture happens week 3 with a partner
session.**

## LLM — Anthropic vs Claude Code CLI vs Codex CLI

### `claude -p` smoke (§5.7)

The week-1 done-when bar for the LLM side is: pipe a 10-line fake
transcript into `claude -p` and confirm it returns a JSON object
parseable by `heron-llm::SummarizerOutput`. That smoke proves the
prompt template renders and that the user's local `claude` binary
is wired correctly; it doesn't measure quality.

**Status: not run — deferred until the user provides
ANTHROPIC_API_KEY (or has the Claude Code CLI logged in). The smoke
runs in well under a second once unblocked.**

## Cost calibration (§11.4)

Anthropic API responses include `usage.input_tokens` /
`output_tokens` and prompt-cache fields. heron computes USD per
session from current public pricing and writes it into the
`Cost` block of the frontmatter. **Source of truth: API response,
not the dashboard** (dashboard lags by minutes).

### Done-when (§11.5)

- Cost matches API-response totals exactly.
- Re-summarize integration test (using fixture from week 8)
  preserves ≥ 80 % of action item IDs (the §10.5 ID-preservation
  contract).

**Status: blocked on API key.**

## v1 default

Until the fixture suite measures otherwise:

- STT default: WhisperKit on Apple Silicon + macOS 14+, Sherpa
  fallback otherwise.
- LLM default: Anthropic API (Claude Sonnet 4.6 for ≤ 90-min
  meetings, Claude Opus 4.7 for longer per `plan.md` §5).

User can override either via the CLI / Tauri shell settings.
