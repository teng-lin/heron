# Research notes: AI agents that actively participate in meetings

State-of-the-art survey (compiled 2026-04-25) for projects that let an AI
agent **attend a meeting on behalf of a human** — i.e., join Zoom/Meet/Teams
as a participant, *listen and speak in real time*, not just transcribe
after the fact. Source material for the v2 pivot proposal in
[`architecture-agent-participant.md`](./architecture-agent-participant.md).

## TL;DR

**Yes — speaking meeting agents are real, shipping technology as of 2025–2026**,
not just demos. Both commercial APIs and open-source stacks let a bot join
Zoom/Meet/Teams as a participant, listen, and speak in real time.

The realistic state of the art is **production-capable for narrow roles**
(sales coach, interviewer, CEO avatar reading scripted remarks) but **not
yet a believable open-ended replacement for a human in a normal team
meeting** — latency, turn-taking, and persona fidelity are the remaining
gaps. Zoom's own roadmap puts true agent delegation ~12 months out from
late 2025.

## Category 1 — Active participation agents (the real answer)

Ranked by maturity / relevance. These bots speak AND listen in the meeting.

### 1. Recall.ai — Output Media API (commercial)

- **URL:** <https://docs.recall.ai/docs/stream-media>
- **YC launch:** <https://www.ycombinator.com/launches/M9k-recall-ai-output-media-api-ai-agents-that-talk-in-meetings>
- **Sample repo:** <https://github.com/recallai/meeting-bot>
- **Speaks:** ✅ + listens.
- **How:** streams audio/video from a web app you control into the bot's
  virtual mic and camera; provides a virtual mic for receiving meeting audio.
- **Platforms:** Zoom, Google Meet, Teams, Webex.
- **Maturity:** Shipped product. Production usage for AI sales agents,
  recruiters, interviewers.
- **License/cost:** Commercial API. Per-minute pricing; vendor sees every
  meeting that flows through (privacy implication for `heron`'s ethos).
- **Confidence:** High (official docs + YC launch + sample repo).

### 2. Attendee (open source)

- **Repo:** <https://github.com/attendee-labs/attendee> (557★, active
  through April 2026)
- **Voice agent example:** <https://github.com/attendee-labs/voice-agent-example> (MIT)
- **Speaks:** ✅ + listens (voice-agent example wires to Deepgram Voice Agent).
- **How:** self-hostable Django/Docker meeting-bot API.
- **Platforms:** Zoom, Meet, Teams.
- **Maturity:** Beta-quality OSS. The voice-agent example is reference
  code, not turnkey.
- **License:** AGPL-style ("Other" on GitHub).
- **Why it matters for heron:** self-hostable + OSS = aligns with
  "private, on-device" ethos in a way Recall.ai does not.
- **Confidence:** High.

### 3. MeetingBaaS — speaking-meeting-bot (open source, MIT)

- **Repo:** <https://github.com/Meeting-Baas/speaking-meeting-bot> (47★)
- **MCP server:** <https://github.com/Meeting-Baas/speaking-bots-mcp>
- **Speaks:** ✅ "fully autonomous speaking bots."
- **Stack:** MeetingBaaS API + Pipecat (Cartesia TTS, Deepgram/Gladia STT,
  GPT-4). Personas defined in Markdown.
- **Platforms:** Google Meet, Microsoft Teams (Zoom via MeetingBaaS API).
- **Maturity:** Working OSS demo, also exposed via MCP server for LLM agents.
- **Confidence:** High.

### 4. Zoom AI Companion + custom AI Companion / digital twin (commercial)

- **URL:** <https://www.zoom.com/en/blog/agentic-ai-next-evolution-zoom/>
- **Press:** Eric Yuan used his own AI avatar on the Zoom Q1 2025 earnings
  call (May 2025) — <https://techcrunch.com/2025/05/22/after-klarna-zooms-ceo-also-uses-an-ai-avatar-on-quarterly-call/>
- **Speaks:** ✅ avatar + voice; reading prepared content is shipped, true
  autonomous "send my twin to a meeting" is on the stated 12-month roadmap
  (per Yuan, late 2025).
- **Platforms:** Native Zoom; AI Companion can also attend Teams/Meet for
  *notes only* (no speaking on those platforms yet).
- **Pricing:** $12/user/mo add-on.
- **Confidence:** High for avatar/voice; Medium for true delegation.

### 5. Vexa (open source, Apache 2.0)

- **Repo:** <https://github.com/Vexa-ai/vexa> (1,909★, very active)
- **Speaks:** ❌ **listen-only** today (real-time transcripts + MCP for AI
  agents). No native speaking output.
- **Platforms:** Meet, Teams, Zoom.
- **Note:** can be paired with Pipecat/LiveKit to add talkback, but that's
  not what ships out of the box.
- **Why it's listed here:** active OSS, Apache-licensed, MCP-native — most
  obvious base if `heron-bot` chooses self-hosted listening only.
- **Confidence:** High (verified listen-only).

## Category 2 — Passive note-takers (for contrast, NOT the answer)

These are what `heron` v1 looks like today. They join meetings but **do not
speak**:

- Otter, Fireflies, Granola, Read.ai, tl;dv, Avoma
- Zoom AI Companion (default mode), Microsoft Copilot
- Meetily (OSS, local-first)

## Category 3 — Adjacent / building blocks

These aren't full meeting-participant products, but every speaking bot
above is built on some combination of them. They are the parts list for a
self-built `heron-bot` + `heron-voice` + `heron-realtime` stack.

### Voice agent runtimes (the orchestration layer)

- **Pipecat** (BSD-2, 11.5k★) — <https://github.com/pipecat-ai/pipecat>
  — the de-facto OSS voice-agent pipeline behind most speaking bots above.
- **LiveKit Agents** (Apache, 10.2k★) — <https://github.com/livekit/agents>
  — same role; powers OpenAI's Realtime demos.

### Realtime LLM APIs (the brain)

- **OpenAI Realtime API** — bidirectional speech-to-speech.
- **Gemini Live** — bidirectional, low-latency.
- **Vapi** — wraps realtime LLMs for telephony/voice agents.

### Avatars (the face, optional)

- **Tavus CVI** — <https://www.tavus.io/cvi> — sub-1s utterance-to-utterance
  photoreal avatars, embeddable in Daily-powered rooms; current leader.
- **HeyGen Interactive Avatar / D-ID Agents** — same role, different vendors.

### "Digital twin" / AI representative products (commercial)

- **Delphi.ai** — voice/video "Digital Mind" clones. Deepak Chopra has
  used his Delphi to attend Zoom calls in his absence
  (<https://www.technologyreview.com/2025/09/02/1122856/can-an-ai-doppelganger-help-me-do-my-job/>).
- **Personal.ai** — similar AI-twin angle.

## What this means for heron

If `heron` pivots from passive note-taker to active participant, the
build-vs-buy decision tree shrinks to roughly three paths:

| path | bot driver | voice runtime | tradeoff |
|---|---|---|---|
| **fastest** | Recall.ai | OpenAI Realtime | weeks to a working demo; vendor sees every meeting; not aligned with privacy ethos |
| **most aligned with heron** | Attendee (self-hosted) | Pipecat + local TTS where possible | months to ship; OSS top-to-bottom; privacy-defensible |
| **deepest** | Native Zoom SDK | custom on top of LiveKit Agents | longest path; Zoom-only; best latency and integration |

The architecture in [`architecture-agent-participant.md`](./architecture-agent-participant.md)
is intentionally driver-agnostic — `heron-bot` is the abstraction that
lets us choose a path later.

## Open questions the research did not resolve

- Real-world end-to-end latency numbers for each path (Recall.ai +
  Realtime vs. Attendee + Pipecat) — needs benchmarking, not desk research.
- Voice-cloning consent UX patterns that have actually shipped at
  consumer scale — Delphi and Zoom's avatar program are the closest
  reference points but neither publishes their flow.
- Two-party-consent legal status of an AI agent attending in lieu of a
  human in jurisdictions that already require recording disclosure —
  may require an additional disclosure category.

## Sources

- [Recall.ai Output Media API](https://docs.recall.ai/docs/stream-media)
- [Recall.ai YC launch — AI agents that talk in meetings](https://www.ycombinator.com/launches/M9k-recall-ai-output-media-api-ai-agents-that-talk-in-meetings)
- [Attendee on GitHub](https://github.com/attendee-labs/attendee)
- [Attendee voice-agent example](https://github.com/attendee-labs/voice-agent-example)
- [MeetingBaaS speaking-meeting-bot](https://github.com/Meeting-Baas/speaking-meeting-bot)
- [Vexa](https://github.com/Vexa-ai/vexa)
- [Zoom agentic AI blog](https://www.zoom.com/en/blog/agentic-ai-next-evolution-zoom/)
- [TechCrunch: Zoom CEO uses AI avatar on earnings call](https://techcrunch.com/2025/05/22/after-klarna-zooms-ceo-also-uses-an-ai-avatar-on-quarterly-call/)
- [Tavus CVI](https://www.tavus.io/cvi)
- [Pipecat](https://github.com/pipecat-ai/pipecat)
- [LiveKit Agents](https://github.com/livekit/agents)
- [Delphi.ai — MIT Tech Review](https://www.technologyreview.com/2025/09/02/1122856/can-an-ai-doppelganger-help-me-do-my-job/)
