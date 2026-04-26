# `fixtures/manual-validation/` — `[needs-human]` test artifacts

Per `docs/archives/implementation.md` §19.6: every `// [needs-human]` test
records a single `.mov` / `.wav` / `.png` artifact under this
directory so a reviewer can verify the gate held without re-running it.

## Convention

```text
fixtures/manual-validation/<test-name>/<YYYY-MM-DD>.{mov,wav,png}
```

`<test-name>` is the slugified Rust function name (e.g. the §13.5
laptop-onboarding screencasts live at
`fixtures/manual-validation/onboarding/`).

## Index

| Test | Section | Artifact format | Cadence       |
|------|---------|-----------------|---------------|
| `device-change` | §7.3 | `.mov` | Once per release |
| `aec-test-rig` | §6.3 | `.wav` correlation report | Once per release |
| `onboarding/<n>` | §13.5 | `.mov` x12 | Per release-candidate |
| `exec-dogfood` | §18 | `docs/dogfood-log.md` | One week, week 16 |

The full list lives in `docs/archives/manual-test-matrix.md`.
