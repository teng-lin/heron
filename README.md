# heron

A private, on-device, agent-friendly AI meeting note-taker.

heron records native meeting calls without joining as a bot, transcribes
locally, attributes speakers using each meeting app's own speaker
signal — accessibility tree for native clients, WebRTC track IDs for
browser-based ones — so it recovers real display names instead of
voice-clustered `speaker_1` / `speaker_2` labels. Output is a markdown
summary written into an Obsidian vault. The vault folder lives inside
the user's Dropbox / Google Drive / iCloud — heron itself never touches
sync.

The product target is a business executive running many external
client meetings, plus the author. Audio never leaves the device except
to the user's chosen LLM provider for summarization.

## Scope

heron is designed to be cross-platform along two axes: operating
system and meeting app. v1 ships a focused slice of both; later
releases fill the matrix in.

### Operating systems — macOS, Windows, Linux

- **v1 (initial release): macOS only.** macOS first because Core
  Audio process taps (14.2+) give clean per-app system audio without
  driver hacks, WhisperKit + Apple Neural Engine give the best local
  STT path on Apple Silicon, and native meeting apps' macOS
  accessibility trees are the cleanest path to real speaker names.
- **Windows (v2):** WASAPI process loopback (Windows 10 build 20348+)
  is the equivalent of Core Audio process taps; UI Automation
  replaces AXObserver.
- **Linux (v2):** PipeWire per-app capture replaces Core Audio;
  AT-SPI replaces AXObserver. Lower priority than Windows because of
  the smaller Linux meeting footprint, but the architecture is
  identical.

### Meeting apps — Zoom, Google Meet, Microsoft Teams, Webex

- **v1 (initial release): Zoom only.** Zoom ships a native macOS
  client whose accessibility tree exposes the "currently speaking"
  tile with the real display name — the cleanest signal for the
  happy-path attribution the product is pitched on.
- **Google Meet / Microsoft Teams (v1.1+):** both are browser-based,
  so attribution comes from WebRTC track interception inside an
  embedded WebView rather than an accessibility tree. Same per-
  speaker timeline contract downstream, different capture mechanism
  upstream.
- **Webex (v1.1+):** native macOS client; the AXObserver approach
  should port directly pending an accessibility-tree survey.

The Rust crate boundaries (`heron-audio`, `heron-zoom`, `heron-speech`)
are designed for this kind of extension. v1 introduces the `SttBackend`
and `AxBackend` traits as precedent; the same pattern generalizes to a
per-meeting-app capture trait in v1.1, and v2 adds an `AudioCapture`
trait so each new OS or meeting app drops in via new implementations
rather than forks.

## Status

Pre-implementation. The repository contains only design documents.
v1 is planned as a 17-week solo build: macOS only, Zoom only, English
only. Mobile (iOS / Android), other meeting apps (Meet / Teams /
Webex), other desktop operating systems (Windows / Linux), ambient
session detection, and an MCP server are deferred to v1.1+.

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
  track and can only cluster speakers by voice. heron uses the
  meeting app's own speaker signal (Zoom's accessibility tree in v1;
  WebRTC track IDs for Meet / Teams in v1.1+) to recover real display
  names without ML clustering in the happy path.
- **Char / oh-my-whisper**: closer to the right shape but don't solve
  per-speaker attribution for native meeting clients.

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
