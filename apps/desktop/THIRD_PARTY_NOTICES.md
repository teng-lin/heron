# Third-Party Notices

This file lists third-party code that ships inside `heron-desktop`'s
JavaScript bundle but is not vendored as a normal npm dependency.
It satisfies the attribution requirement in Apache-2.0 §4(d).

NPM dependencies pulled by `apps/desktop/package.json` carry their own
`LICENSE` files inside `node_modules/<pkg>/` and are governed by the
license expressions in those files. This document covers code we have
**copied or adapted** into the source tree directly.

## @vexaai/transcript-rendering — Apache-2.0

- **Copyright**: Copyright (c) Vexa AI
- **License**: Apache License, Version 2.0
- **Source**: <https://github.com/Vexa-ai/vexa/tree/main/packages/transcript-rendering>
- **Used in**: `apps/desktop/src/lib/transcript.ts`

The Heron `Review` page renders meeting transcripts written into
`<vault>/<sessionId>.md` as `> HH:MM:SS Speaker: text` lines. The
speaker-grouping algorithm (walk segments in order, merge consecutive
same-speaker runs, split overlong groups at segment boundaries) is
adapted from `@vexaai/transcript-rendering`'s `groupSegments` function.

We chose to port the small grouping algorithm into a single TypeScript
file rather than vendor the full npm package because Heron writes the
transcript into a static `.md` after summarize — we never need the
package's WebSocket dedup / two-map pending/confirmed state machinery.
A header comment in `apps/desktop/src/lib/transcript.ts` repeats this
attribution so the credit is visible at the point of use.

The full Apache-2.0 license text is available at
<http://www.apache.org/licenses/LICENSE-2.0>; an unmodified copy
ships with every `@vexaai/transcript-rendering` release on npm.
