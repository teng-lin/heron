# `fixtures/speech/` — STT WER regression cases

WhisperKit / sherpa transcription regression suite. Each case is one
real or rehearsed meeting with hand-labeled ground truth; the suite
runs both backends, computes WER per case, and asserts each measurement
stays at or below its §8.5 threshold.

## Required cases (§8.5)

| Case directory               | WhisperKit threshold | Sherpa threshold |
|------------------------------|----------------------|------------------|
| `client-3person-gallery/`    | ≤ 15 %               | ≤ 22 %           |
| `team-5person-with-dialin/`  | ≤ 22 %               | ≤ 30 %           |
| `1on1-internal/`             | ≤ 12 %               | ≤ 18 %           |

The thresholds are pinned in
`crates/heron-speech/src/selection.rs::WER_THRESHOLDS` and any change
must update both tables together.

## Per-case files

```text
client-3person-gallery/
├── mic.wav             # 48 kHz mono, ≥ 90 s, post-AEC
├── tap.wav             # 48 kHz mono, ≥ 90 s, system-output capture
├── ground-truth.jsonl  # one Turn per line; text is *the gold standard*
└── README.md           # date, hardware, attendees (or stand-ins)
```

`ground-truth.jsonl` follows the `heron_types::Turn` schema (see
`docs/implementation.md` §3.4 + §5.2). Text is verbatim, including
filler words; capitalization matches what the LLM sees.

## Running the regression

Once `bench-wer.sh` lands (week 4–5), per-fixture WER measurements run:

```sh
scripts/bench-wer.sh fixtures/speech/client-3person-gallery
```

The script prints a per-backend WER and exits non-zero if any
threshold is exceeded.
