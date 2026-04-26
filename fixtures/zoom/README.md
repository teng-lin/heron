# `fixtures/zoom/` — aligner regression cases

Zoom-app AX events + audio captures consumed by the §9.3 aligner.
Each case under `fixtures/zoom/<case>/` follows the layout in the
parent `fixtures/README.md`.

## Required cases (§9.4 + §9.6)

| Case directory              | What it exercises                                    | Status            |
|-----------------------------|------------------------------------------------------|-------------------|
| `gallery-3person/`          | Three-person gallery view, normal cadence            | needs capture     |
| `paginated/`                | Paginated gallery — speaker on a non-visible page    | needs capture     |
| `dial-in-mixed/`            | One dial-in attendee + 2 video attendees             | needs capture     |
| `screen-share/`             | Speaker hidden behind a shared screen                | needs capture     |
| `host-mute-toggle/`         | Speaker indicator transitions while host mutes/un-   | needs capture     |
|                             | mutes — exercises §9.3 step 4 (gap threshold)        |                   |
| `week7-regression/`         | Locked regression case (§9.5)                        | needs capture     |

## Capture procedure

The §3.3 spike workflow applies:

1. Open Zoom Test Meeting (or rehearsed call with a trusted partner).
2. Run `cargo run --bin ax-probe -- --bundle us.zoom.xos > ax-events.jsonl`.
3. Capture `mic.wav` + `tap.wav` via `cargo run --bin heron capture` (when it lands).
4. Hand-label `ground-truth.jsonl` per `docs/archives/implementation.md` §3.4.

## Aligner regression invariants

For every case, the §9.3 aligner output must:

- **AX-hit-rate ≥ 70 %** for cases without dial-in.
- **AX-hit-rate ≥ 50 %** for cases that include a dial-in attendee.
- **No turn attributed across a > 30 s gap** (per `ATTRIBUTION_GAP_THRESHOLD`).
- **Confidence floor `>= 0.6`** for every attributed turn (else `SpeakerSource::Channel`).

These thresholds match the constants in `crates/heron-zoom/src/aligner.rs`.
