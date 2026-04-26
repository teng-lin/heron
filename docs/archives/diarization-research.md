# Invisible Meeting Capture & Speaker Diarization: Research Notes

Notes comparing Char, Granola, and Fireflies.ai — and innovative approaches to solving speaker diarization without a visible meeting bot.

---

## 1. The three approaches today

### Fireflies.ai — bot joins the call
A named bot appears in the participant list via each platform's calendar/API integration (Zoom/Meet/Teams).

- **Upside:** works cross-device, no install needed, server-side recording, gets per-participant audio streams from platform APIs.
- **Downside:** socially loud. Participants see "Fred from Fireflies" join and either get uncomfortable, decline consent, or kick it.

### Granola — local system-audio capture, no bot
Mac app records mic + system output (virtual audio device / Core Audio taps) on the user's machine. Nothing joins the call.

- **Upside:** completely invisible to other participants.
- **Downside:** single mixed remote audio track. Diarization has to be solved with voice-embedding ML.

### Char — same model as Granola, richer surface
Tauri desktop app with `local-stt`, `transcription`, `dictation`, `activity-capture`, `screen` plugins. Captures audio locally, runs STT locally, with local-LLM and TinyBase-backed sessions + TipTap editor. Closer to "notebook that listens" than "meeting recorder that transcribes."

---

## 2. Does Fireflies solve diarization?

Largely yes — it's the main architectural advantage of being a bot.

**What the bot gets for free from the platform:**
- **Zoom** — via RTMS / Meeting SDK, bots subscribe to per-participant audio streams tagged with display names. No ML needed; the mixer hasn't happened yet.
- **Google Meet** — Meet Media API (and prior Add-ons SDK) gives bots per-user audio tracks plus active-speaker events.
- **Microsoft Teams** — Graph Communications / Teams bot SDK exposes per-participant streams with participant IDs.

So "diarization" for Fireflies is largely a **metadata join**, not a voice-fingerprinting problem.

**Where it still breaks down:**
- **Phone dial-ins** collapse into a single "Caller 1" stream.
- **Shared rooms** (one laptop, multiple humans) — one participant label, many voices.
- **Platforms without per-participant APIs** — older Webex, some SIP bridges, in-person meetings.
- **Name quality** — relies on whatever display name the participant set ("iPhone", "MacBook Pro").

---

## 3. Why the SDKs don't solve "invisible + diarized"

All three platform SDKs — Zoom Meeting SDK / RTMS, Google Meet Media API, Teams Graph Communications — share one constraint by design:

**To get per-participant audio, you must be in the meeting.**

There is no "silent observer" mode, and it's not an oversight. Platforms gate per-participant streams behind participation because otherwise anyone with a meeting ID could eavesdrop. This is a **policy wall**, not a technical one.

**The reframe:** stop trying to get data from the server. The user is already a legitimate participant. Every per-participant audio stream is already on their device. The real question is how to tap it before the meeting app mixes it down.

---

## 4. Five innovative approaches

### Approach 1 — Embedded WebView for Meet/Teams, intercept WebRTC tracks

User joins the meeting inside Char's own webview (Tauri WRY = WebKit on macOS). Inject a shim that monkey-patches `RTCPeerConnection.prototype.addEventListener`:

```js
const origAdd = RTCPeerConnection.prototype.addEventListener;
RTCPeerConnection.prototype.addEventListener = function(evt, cb) {
  if (evt === 'track') {
    return origAdd.call(this, evt, (e) => {
      // e.track is a per-remote-participant MediaStreamTrack
      // e.streams[0].id correlates to a Meet/Teams participant
      routeToChar(e.track, e.streams[0].id);
      cb(e);
    });
  }
  return origAdd.call(this, evt, cb);
};
```

Each remote participant arrives as a separate `MediaStreamTrack` — that's how WebRTC works before the browser composites to speakers. Pipe each track via `MediaRecorder` or `AudioWorklet` into a WebSocket to the Tauri Rust side.

**Result:** exactly what Fireflies' bot gets, except the user is a normal participant (themselves). Zero extra attendees. Read the Meet/Teams DOM to map `stream.id → participant display name`.

**Works for:** Meet, Teams (both have full web clients). Zoom's web client is feature-thin — fall back there.

**Constraint:** users must be willing to use Char's webview instead of their native client. UX negotiation, not a technical wall.

### Approach 2 — Ride-along with the native desktop app (macOS)

For users who won't leave their native Zoom app. Two primitives:

**(a) Clean audio stream of the meeting, isolated from all other system audio.**
macOS 14.2 introduced **Core Audio process taps** (`AudioHardwareCreateProcessTap`, `CATapDescription`). Tap `us.zoom.xos` specifically — not browser, not Spotify, just Zoom's output. Windows WASAPI has per-process loopback since Windows 10 build 20348.

**(b) Per-speaker timeline to slice (a) against.**
- **macOS AXObserver on the Zoom participant window.** Zoom's accessibility tree updates the "speaking" indicator on participant tiles in real time — that's how VoiceOver announces who's talking. AXObserver gives push notifications, not OCR polling. Sub-second latency, robust across Zoom updates (accessibility is stable because Apple enforces it), and you get display names attached.
- **Lip-sync CV on gallery view** (fallback). ScreenCaptureKit captures the Zoom window, lightweight lip-movement detector scores each visible tile, correlates with audio envelope.

With (a) + (b), diarization becomes a **timeline join**, not an ML problem: clean per-app audio + millisecond-accurate "speaker X started/stopped" event stream with real names. Slice audio by speaker changes, run ASR per segment.

**Result:** Fireflies-quality transcripts from a Granola-invisible setup.

### Approach 3 — Compounding voice enrollment across meetings

Neither Fireflies nor Granola does this today — and it gets *better* over time:

1. First meeting: mixed remote audio + calendar attendees.
2. Run voice-embedding clustering (pyannote / ECAPA-TDNN). Get N clusters but don't know which cluster = which person. User labels once at the end ("cluster A = Sarah").
3. Save embeddings keyed by person.
4. Second meeting with Sarah: don't diarize, **identify**. Cluster mixed audio, match each cluster to a known embedding by cosine similarity. Zero user input.
5. Over time, recurring colleagues' embeddings become near-perfect identification.

Fireflies doesn't do this because the platform hands them labels. But they pay for that in social cost. Char pays an ML cost once and it compounds. After 3 meetings with the same 5 coworkers, diarization is **better than Fireflies'** because voice texture beats display names (which can be "iPhone 14").

### Approach 4 — Cross-device pair listening (network effect)

When two Char users are in the same meeting, each device has:
- Their own mic: clean, ground-truth-labeled stream of that specific person's voice.
- The mixed remote stream: everyone else.

If two Char clients talk to each other (LAN discovery or relayed), each contributes their own mic as "this is definitely Alex" / "this is definitely Bin." For everyone else, fall back to clustering.

**Flywheel:** more Char users per org → more meetings with ≥2 Char users → more clean voice labels → diarization approaches free. Only approach that gets more accurate the more it's adopted.

Doubles as **corroboration**: two clients independently transcribe the same remote stream → ensemble ASR outputs. Two mid-quality local STTs can beat one cloud STT.

### Approach 5 — Use platform-native legitimate features, silently

Platforms already offer per-speaker data through sanctioned, host-controlled features — just post-hoc:

- **Zoom "record separate audio files"** — if host enables this, Zoom's own local recording produces one M4A per participant at meeting end. Char detects these files in `~/Documents/Zoom/<meeting>/` and ingests.
- **Zoom cloud recording transcript** — accessible via Zoom API if user is host.
- **Teams meeting transcript API** — Graph API exposes it with speaker attribution for organized meetings.
- **Meet artifacts API** — similar, for Workspace users.

Combine with a live rough-pass transcript (mixed audio + rough diarization) during the meeting, then silently **reconcile** with authoritative per-speaker post-meeting data and upgrade the record in place. User sees notes during, gets perfect speaker labels minutes after hanging up.

---

## 5. Layered fallback strategy

Don't pick one — build a layered fallback keyed on what's available per meeting:

| Meeting type | Primary mechanism |
|---|---|
| Meet / Teams in Char webview | WebRTC track interception (Approach 1) |
| Native Zoom + macOS | Process audio tap + AXObserver timeline (Approach 2) |
| Native Zoom, user is host | Post-hoc reconcile with separate-audio-files or cloud transcript (Approach 5) |
| Any meeting, recurring team | Voice-embedding identification (Approach 3), always on |
| Any meeting with ≥2 Char users | Cross-device pair listening (Approach 4), always on |

**Shared insight across all five:** the user is a legitimate participant and all the signal is already on their device or in their account. No new bot, no new server — just plumb data the user already has rightful access to.

- Defensible vs. **Fireflies** (which pays a social tax for per-participant audio).
- Novel vs. **Granola** (which hasn't gone past "single mixed track + cluster").

---

## 6. Highest-leverage first build

**Approach 2 (AXObserver + per-process audio tap on macOS).**

- Unlocks full Fireflies-quality experience for native Zoom users without touching their workflow.
- Uses only first-party Apple APIs (Core Audio process taps, Accessibility).
- Accessibility tree is stable because Apple enforces it across Zoom's version bumps.

Rest are roadmap.

---

## 7. Open questions / risks

- **macOS process audio tap permissions** — requires user grant; UX for first-run permission flow needs care.
- **AXObserver on Zoom** — needs accessibility permission; also, Electron-based meeting apps may have thinner a11y trees than native ones. Validate Zoom, Teams, Slack Huddles separately.
- **Embedded webview for Meet/Teams** — login flows (SSO, SAML, 2FA) inside a custom webview may trip fraud/bot detection. Needs Safari-like UA and persistent WebKit data store.
- **Voice enrollment privacy** — storing voice embeddings keyed by person is sensitive. Must be local-only, per-user-toggleable, easy to purge.
- **Cross-device pair listening discovery** — mDNS on corporate networks often blocked. Need a relay fallback and E2E encryption so Char servers never see audio.
- **Legal / consent** — even invisible capture of someone's voice has jurisdictional rules (two-party-consent US states, GDPR). Product needs clear "Char is recording" affordances for the user that they can surface to the room verbally.
