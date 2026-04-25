# heron

A private, on-device, agent-friendly AI meeting note-taker.

heron records native Zoom calls without joining as a bot, transcribes
locally, attributes speakers using each platform's accessibility tree
(real display names, not voice clustering), and writes a markdown
summary into an Obsidian vault. The vault folder lives inside the
user's Dropbox / Google Drive / iCloud — heron itself never touches
sync.

The product target is a business executive running many external
client meetings, plus the author. Audio never leaves the device except
to the user's chosen LLM provider for summarization.

## Platforms

heron is designed to be cross-platform: **macOS, Linux, and Windows.**

- **v1 (initial release): macOS only.** macOS first because Core
  Audio process taps (14.2+) give clean per-app system audio without
  driver hacks, WhisperKit + Apple Neural Engine give the best local
  STT path on Apple Silicon, and Zoom's macOS accessibility tree is
  the cleanest path to real speaker names.
- **Windows (v2):** WASAPI process loopback (Windows 10 build 20348+)
  is the equivalent of Core Audio process taps; UI Automation
  replaces AXObserver.
- **Linux (v2):** PipeWire per-app capture replaces Core Audio;
  AT-SPI replaces AXObserver. Lower priority than Windows because of
  Zoom's smaller Linux footprint, but the architecture is identical.

The Rust crate boundaries (`heron-audio`, `heron-zoom`, `heron-speech`)
are designed for cross-platform extensibility. v1 introduces the
`SttBackend` and `AxBackend` traits as precedent; v2 adds an
`AudioCapture` trait so platform support drops in via new
implementations rather than forks.

## Status

Pre-implementation. The repository contains only design documents.
v1 is planned as a 17-week solo build for macOS, Zoom-only, English-
only. Mobile (iOS / Android), Meet / Teams, ambient session
detection, and an MCP server are deferred to v1.1+.

## Documents

Read in this order:

| Document | What it covers |
|---|---|
| [`docs/plan.md`](docs/plan.md) | The v1 product/architecture plan. Locked decisions, output contract, build plan by week, risks, deferred-to-v2 list. Authoritative scope. |
| [`docs/implementation.md`](docs/implementation.md) | The execution layer below the plan. Prerequisites, week-by-week tasks, acceptance criteria, code stubs, test rigs. |
| [`docs/architecture.md`](docs/architecture.md) | The longer-range architectural sketch — full crate decomposition, agent surface, consumer model. v1 implements a focused subset. |
| [`docs/stack.md`](docs/stack.md) | Greenfield tooling/framework choices. Long-range reference; `plan.md` is what v1 uses. |
| [`docs/diarization-research.md`](docs/diarization-research.md) | Five approaches to invisible meeting capture + speaker attribution. Approach 2 (Core Audio process tap + AXObserver on Zoom) is the v1 flagship bet. |

If you only read one document, read `docs/plan.md`.

## How v1 differs from existing tools

- **Fireflies / Otter**: join the call as a visible bot. heron does not.
- **Granola**: invisible, but collapses remote audio to a single mixed
  track and can only cluster speakers by voice. heron uses Zoom's
  accessibility tree to recover real display names without ML
  clustering in the happy path.
- **Char / oh-my-whisper**: closer to the right shape but don't solve
  per-speaker attribution for native Zoom.

## Quality promise

In the modal AXObserver outcome, heron attributes ~70% of turns to a
real name with high confidence; the remaining ~30% are marked `them`
with a low-confidence visual indicator. This is strictly better than
Granola (0% attribution) and comparable-to-weaker than Fireflies on
a well-configured call. The pitch is **"Fireflies-quality
attribution on most turns, without a bot"**, not "Fireflies-quality
transcripts always."

## License

UNLICENSED. Private project; not yet open source.
