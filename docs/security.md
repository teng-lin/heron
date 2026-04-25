# Security model

heron is an on-device meeting note-taker. Audio never leaves the
machine except to the user's chosen LLM provider for summarization.
This document records the concrete security assumptions, threat
model, and the mechanisms we rely on to keep them.

## Trust boundaries

| Boundary | Crosses | Notes |
|---|---|---|
| user → heron | TCC prompts | Microphone, AudioCapture, Accessibility, Calendar are all opt-in. Failed grants disable the corresponding feature without crashing the session. |
| heron → meeting-app | Core Audio process tap; AXObserver | Read-only on Zoom's process; we never inject. |
| heron → LLM provider | HTTPS | The user picks their backend (Anthropic API, Claude Code CLI, Codex CLI). Per-session redaction lives in the prompt template, not after-the-fact. |
| heron → vault | atomic write to filesystem | The vault folder is the user's responsibility (Obsidian inside Dropbox / iCloud / Google Drive). We don't sync; we write `0600` files. |

## Hardened-runtime entitlements

Committed at `apps/desktop/src-tauri/entitlements.plist` per §0.6.
The five entitlements heron requests:

- `com.apple.security.app-sandbox = false` — Core Audio process taps
  need unsandboxed mach access to AudioHALD.
- `com.apple.security.cs.allow-jit = true` — WhisperKit + the Tauri
  WebView use JIT.
- `com.apple.security.cs.disable-library-validation = true` —
  needed for loading WhisperKit's CoreML model bundles + Tauri
  sidecar binaries.
- `com.apple.security.device.audio-input = true` — microphone
  access (paired with `NSMicrophoneUsageDescription`).
- `com.apple.security.temporary-exception.mach-lookup.global-name`
  array containing `com.apple.audio.audiohald` — Core Audio process
  taps via mach lookup.

## Keychain ACL — `swift/keychain-helper`

**Status: not yet implemented (deferred per user guidance).**

The §6.5 keychain ACL test ships in week 2 alongside the
notarization pipeline. The test builds the Swift helper twice with
different bundle IDs (`com.heronnote.heron`,
`com.heronnote.test-foreign`) and confirms that an item written by
one cannot be read by the other. That gives the user confidence
that secrets stored in Keychain (the Anthropic API key, when used)
are accessible only to a signed heron binary, not arbitrary other
apps the user happens to install.

This document will be updated with the test methodology + the
verification command once §6.5 ships. The blocker is the paid
Apple Developer ID required for `codesign --sign "Developer ID
Application: …"` of the two test binaries.

## Logs and PII

Per [`docs/observability.md`](observability.md):

- No audio bytes in logs.
- No transcript text in logs.
- No calendar event titles or attendee names in logs.
- API keys / signing secrets / tokens are wrapped in
  redact-on-debug-format types in `heron-llm` (assertion enforced
  in §11 wiring, week 9).

## Vault location

The vault folder lives **inside** the user's chosen sync provider.
heron writes `0600` files via atomic temp + rename per §19.4.
Sync-provider security (server-side encryption, sharing settings)
is the user's responsibility; heron does not assume the vault is
private if it's in a publicly-shared folder.

## What heron deliberately does NOT do

- **No bot in the meeting.** Unlike Fireflies / Otter, heron never
  joins the call as a participant.
- **No central server.** No backend-side aggregation. Sync is the
  user's cloud folder.
- **No background uploads.** The only network egress is the
  per-session summarize call to the user's chosen LLM provider.
- **No telemetry.** Logs stay on disk. v1 ships without crash
  reporting; v1.1 may add an opt-in dialog.

## Threat model — out of scope

Recorded for honesty:

- **Compromised macOS user account** — an attacker with shell
  access can read the vault and the cache directory. We don't
  encrypt at rest; that's FileVault's job.
- **Malicious LLM provider** — the user's chosen LLM sees the
  transcript. Pick one you trust. v1.1 will add an offline-only
  summarizer toggle.
- **Compromised meeting-app process** — Zoom could in theory feed
  AX names that don't match the actual speakers. We treat AX as
  trusted because the alternative (voice clustering) is worse on
  every empirical axis.
