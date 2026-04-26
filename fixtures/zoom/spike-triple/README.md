# Zoom AX speaker-indicator spike (§3.3 outcome)

This directory holds the artifacts from the `docs/archives/plan.md` §3.3 spike:
two `heron ax-dump` captures of the Zoom AX tree taken against a live
2-participant call (laptop + phone), with the phone toggled between
muted and unmuted between captures.

## Why these files exist

The original spike plan (per `docs/archives/manual-test-matrix.md` row #1) was
to capture a stable `(role, subrole, identifier)` triple for the
active-speaker indicator — the colored frame that highlights the
person currently speaking — so `swift/zoomax-helper/.../ZoomAxHelper.swift`
could subscribe to a single AX notification and emit a `SpeakerEvent`
each time the speaker changed.

These two captures were generated to *find* that triple via the
diff method documented in `ZoomAxHelper.swift`'s tree-dump comment:

```sh
# while phone (Blackmyth) is unmuted
heron ax-dump --bundle us.zoom.xos --out speaking.json
# while phone (Blackmyth) is muted
heron ax-dump --bundle us.zoom.xos --out muted.json

diff <(jq -S '.nodes' muted.json) <(jq -S '.nodes' speaking.json)
```

## What we found

**There is no AX-readable speaker indicator in Zoom 7.0.0.**

- Every per-participant tile (`AXTabGroup` at depth 2) has `subrole`,
  `identifier`, `value`, and `selected` all `null`.
- The yellow/colored "active speaker" frame is rendered via Metal /
  CALayer outside the AX tree.
- The class names that do differ between active-speaker view layouts
  (`ZMMTActiveVideoCellView` vs `ZMThumbnailVideoCellView`) are not
  readable through the public AX APIs Rust links against — Apple's
  Accessibility Inspector reads them via NSObject runtime
  introspection, which we cannot use.

The only signal Zoom 7.0.0 surfaces is **per-participant mute state**,
encoded in the `AXDescription` of each tile:

```
"<Name>, Computer audio (muted|unmuted)[, Video (off|on)]"
```

## What changed as a result

`ZoomAxHelper.swift` was rewritten to enumerate participant tiles by
parsing the AXDescription regex above and emit a `SpeakerEvent` on
every transition (new participant, mute toggle, participant left).
The bridge polls the AX tree at 4 Hz instead of subscribing to AX
notifications — Zoom's notification firing on these tiles is
unverified, polling is bounded CPU, and the aligner's 350 ms default
`event_lag` prior absorbs the worst-case detection latency.

The full design rationale (and the limitations — degrades on 3+
free-for-all calls; documented as the `speaker: "them"` risk-reducer
fallback per `docs/archives/implementation.md` §20) lives in the module
header comment of `swift/zoomax-helper/Sources/ZoomAxHelper/ZoomAxHelper.swift`.

## Capture environment

- macOS 26.4.1 (Darwin 25.4.0)
- Zoom 7.0.0 (build 77593), bundle id `us.zoom.xos`
- 2 participants in the meeting (laptop + phone, same Zoom account)
- Active-speaker view (split layout — `ZMMTSideBySideSplitDivider`
  visible in both dumps), not gallery

## Scrub

The captures were trimmed before commit: every `AXMenuBar`,
`AXMenuBarItem`, `AXMenu`, and `AXMenuItem` node was removed from
both files. The full `ax_dump_tree` walk also visits the macOS
menu-bar tree, which Zoom populates with the user's email address
("Switch account" / "Sign out" submenus) and recent client document
filenames ("Recent Items → Documents") — incidental capture that has
nothing to do with the speaker-attribution contract under audit.

Tile-level data (the `AXTabGroup` participant tiles whose
`AXDescription` encodes mute state) lives entirely outside those
subtrees, so the scrub preserves the spike evidence at zero loss of
relevance. Each file's top-level object carries a `"scrubbed"` key
documenting this for future readers.
