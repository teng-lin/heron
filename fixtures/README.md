# Fixtures

Recorded artifacts that test cases consume. Most fixtures are gated on
`#[cfg(feature = "fixture-tests")]` or `#[ignore]` so the default
`cargo test` runs without requiring the on-disk corpus.

## Layout

Per `docs/implementation.md` §19.5:

```text
fixtures/
├── ax/                   # AX-event JSONL captures (week 0 spike, §3.3)
├── manual-validation/    # Screencasts + WAVs from #[needs-human] tests (§19.6)
├── speech/               # WhisperKit / sherpa STT regression cases (§8.5)
├── synthetic/            # Generated/simulated inputs for fast unit tests
└── zoom/                 # Zoom-app AX + audio fixtures (§9.3 aligner)
```

## Per-fixture format

Each leaf fixture directory contains:

```text
<crate>/<case>/
├── mic.wav            # 48 kHz mono, post-AEC user audio
├── tap.wav            # 48 kHz mono, system-output capture (everyone-but-user)
├── ax-events.jsonl    # ground-truth AX events (zoom + ax cases only)
├── ground-truth.jsonl # human-labeled `Turn` records (text + speaker)
└── README.md          # captured-at, hardware, anything case-specific
```

## Adding a new fixture

1. Capture `mic.wav` + `tap.wav` from a real or fixture session.
2. Run `ax-probe` if zoom/ax case → emit `ax-events.jsonl`.
3. Hand-label `ground-truth.jsonl` per `docs/implementation.md` §3.4.
4. Drop a `README.md` documenting the case (date, devices, edge cases).
5. Commit; CI's WER + aligner regression suites pick it up
   automatically once they iterate over the directory.

## Privacy

Real meetings only commit if every participant has signed off — see
`docs/manual-test-matrix.md` for the consent gate. The default
fallback is **synthesized** input (`fixtures/synthetic/`) which has no
PII.
