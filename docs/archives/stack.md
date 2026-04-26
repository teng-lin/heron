# heron-style meeting capture: greenfield stack

Opinionated tooling/framework recommendation for a from-scratch rewrite,
assuming the architecture in `heron-architecture-notes.md` and the product
vision: private, on-device meeting capture; desktop flagship (macOS, then
Windows/Linux); mobile viewer/editor/summary (iOS + Android); ambient
session detection; agent-friendly event surface; multilingual including
code-switch; local-first with optional cloud.

## Ground rules

- Pick **one** per layer. No "A or B, we'll decide later."
- Prefer boring, widely-deployed tools.
- Delegate anything commoditized (local model runtimes, auth, payments).
- Don't build a plugin framework until a second consumer exists.

## The stack

```
Concern                        Choice                              Why this one

CORE
Language                       Rust (2024 edition, stable)         Portable binary, no GC, mobile via UniFFI
Workspace / build              cargo + just + nextest + bacon      Standard; no custom tooling
Async runtime                  tokio                               Everyone else's choice; fits actors
Error / logging                anyhow + thiserror + tracing        Don't invent an error type system
FFI to mobile                  UniFFI                              Signal/Bitwarden/Firefox do this

AUDIO
System audio (macOS)           Core Audio Process Taps via cidre   14.4+; no BlackHole/aggregate hacks
System audio (Windows)         WASAPI loopback via cpal            Built-in, no extra driver
System audio (Linux)           PipeWire via pipewire-rs            PulseAudio is dying
Microphone                     cpal                                Same API all three OS
AEC / AGC / noise              webrtc-audio-processing             The APM everyone uses
VAD                            silero-vad via ort                  Tiny ONNX, best accuracy/ms

SPEECH
STT                            sherpa-onnx (default)               One C API covers VAD+STT+diar+speaker ID
                               + WhisperKit (macOS opt-in)         Native Apple perf when available
Diarization                    sherpa-onnx (pyannote ONNX inside)  Same lib, no second dep
Speaker ID                     sherpa-onnx embeddings              Same lib
Code-switch (zh-en etc.)       SenseVoice via ort                  The feature oh-my-whisper nailed

LLM
Local model runtime            Ollama + LM Studio (delegate)       Don't ship your own llama.cpp — too much ops
Cloud                          OpenAI-compat HTTP + Anthropic SDK  Everyone speaks /v1/chat/completions
HTTP                           reqwest + eventsource-stream        SSE streaming
Prompt management              handwritten + handlebars            No "prompt framework"; they all rot
Embeddings                     fastembed-rs (BGE-small, ONNX)      Local, fast, no Python

STORAGE
Primary DB                     SQLite via rusqlite (bundled)       One file, syncs, backs up trivially
Vector index                   sqlite-vec extension                Same DB, one file
Full-text                      SQLite FTS5                         Same DB
Migrations                     refinery                            SQL files, no magic
Reactive queries               SQLite update hooks → tokio broadcast  Roll it yourself; TinyBase is JS-only
Blob storage (audio)           flat files on disk, SHA-indexed     Don't put WAVs in SQLite
CRDT (notes body)              yrs (Yjs in Rust)                   TipTap speaks Yjs natively
Sync                           libSQL (Turso) embedded replicas    SQLite-level sync, no backend to write

SESSION / DETECTION
Calendar (macOS)               EventKit via Swift helper + swift-rs  Only supported path
Calendar (Win)                 Microsoft Graph REST                Native COM too painful
Calendar (cross)               Google Calendar REST                reqwest + oauth2
Running-app / audio activity   OS-native, thin Rust wrappers       NSWorkspace / GetForegroundWindow
State machine                  statig (typed HSM)                  Explicit states > ad-hoc flags

AGENT SURFACE (heron-events)
Local IPC                      Unix socket / named pipe + JSONL    cURL-debuggable, language-agnostic
MCP server                     rmcp                                Anthropic's Rust SDK
Schema                         serde + schemars (JSON Schema)      Self-documenting
Gateway binary                 one small axum-on-unix-socket crate Not gRPC — overkill for local

UI — DESKTOP (Win/Linux, v1 macOS)
Shell                          Tauri v2                            Ship a native app, not Electron
Frontend                       React 19 + Vite + TypeScript        Boring, maximum talent pool
Routing                        TanStack Router                     File-based, type-safe
Data fetching                  TanStack Query                      Already in AGENTS.md
Forms                          TanStack Form + Zod                 Already in AGENTS.md
UI state                       Zustand                             Keep it tiny
Components                     shadcn/ui + Radix + Tailwind        Own the code, don't lock in
Editor                         TipTap + Yjs                        Same editor on every platform
Tables/lists                   TanStack Table                      Virtualized, headless
Icons                          lucide-react                        Consistent set

UI — macOS DESKTOP (option B, if macOS is flagship)
Framework                      SwiftUI                             Shares code with iOS, best Apple feel
Editor                         TipTap-in-WKWebView                 Don't reimplement TipTap
                                                                   (Revisit after v1 ships.)

UI — iOS
Framework                      SwiftUI                             Non-negotiable for store polish
State                          Swift Observation (iOS 17+)         Built-in; skip TCA unless the team knows it
Editor                         TipTap-in-WKWebView                 Same TipTap doc as desktop

UI — ANDROID
Framework                      Jetpack Compose (Kotlin)            Non-negotiable
State                          Compose state + ViewModel           Built-in
Editor                         TipTap in WebView                   Same TipTap doc

PLATFORM / OPS
Crash reporting                Sentry                              Works for Rust + Tauri + iOS + Android
Analytics                      PostHog (self-host option)          Don't build your own
CI                             GitHub Actions + macOS-14 runners   For code-signing and notarization
Code signing / notarize        tauri-action + fastlane             Already solved
Updates                        Tauri updater + TestFlight + Play   Per-platform defaults

BACKEND (only if needed)
HTTP                           axum                                Tokio-native, great ergonomics
Auth                           Clerk or WorkOS (buy)               Do not hand-roll auth
Payments                       Stripe (buy)                        Same
DB                             Postgres + Supabase or Neon         Until you have reason to leave
Sync storage                   Turso (libSQL) or Cloudflare D1+R2  Cheap, global
```

## What to explicitly reject

- **Electron.** Lose every performance and distribution advantage.
- **Flutter / React Native / KMP for the core.** You'll add them *and*
  still write Rust for audio. Net negative.
- **A custom ORM / query builder.** SQL is the contract. Migrations too.
- **A plugin framework in v1.** The current repo has ~50 Tauri plugins;
  most of that fan-out is premature. Start with one crate exposing one
  `heron-events` trait and add plugins when a second consumer appears.
- **Your own llama.cpp wrapper.** Ollama and LM Studio are free
  infrastructure; use them until you have a specific reason not to.
- **Microservices / gRPC / Kafka.** This is a desktop app. One binary,
  one socket.
- **Kubernetes / Docker for the app.** Likewise.
- **NIH prompt framework / agent framework.** handlebars + the Anthropic
  SDK is enough.
- **Multiple editors across platforms.** TipTap + Yjs everywhere via
  WebView. Revisit only if a customer complaint forces it.
- **RxJS-style reactive layers in Rust.** `tokio::sync::broadcast` off
  SQLite update hooks is 40 lines and outperforms anything ceremonial.

## The v1 crate skeleton (8 crates, not 190)

```
char/
├── crates/
│   ├── heron-audio/      # tap + mic + APM + resample → CaptureFrame stream
│   ├── heron-speech/     # sherpa-onnx wrapper: VAD + STT + diarize
│   ├── heron-llm/        # ollama/lmstudio/openai/anthropic + templates
│   ├── heron-store/      # rusqlite + sqlite-vec + FTS5 + yrs
│   ├── heron-session/    # detection state machine + consent gate
│   ├── heron-events/     # public API: subscribe() query() inject_context()
│   ├── heron-bindings/   # UniFFI surface (Swift + Kotlin)
│   └── heron-cli/        # the whisper-style CLI over heron-events
├── apps/
│   ├── desktop/         # Tauri v2 + React
│   ├── ios/             # SwiftUI, links heron-bindings.xcframework
│   └── android/         # Compose, links heron-bindings.aar
└── gateway/             # the local IPC / MCP binary
```

Nine artifacts. Ship v1 from this and *only* this. Every new crate after
that needs to justify its existence with a second consumer.

## The one tradeoff worth flagging

**sherpa-onnx vs. whisper.cpp + separate diarization.** Sherpa gives a
single, opinionated speech pipeline (VAD + STT + diarization + speaker ID)
in one C lib. whisper.cpp is more widely benchmarked but you'd bolt on
pyannote-onnx yourself and wire up diarization logic. Sherpa is the faster
path to a working product; whisper.cpp is the safer path if you expect to
swap STT backends often.

Recommendation: start with sherpa-onnx and make `heron-speech` the
abstraction boundary so swapping later is a day's work, not a rewrite.
