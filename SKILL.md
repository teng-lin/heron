---
name: heron-meeting
description: Drive heron's meeting CLI to record a meeting, attend a meeting via the local daemon (with live event streaming), or re-transcribe / re-summarize an existing note. Activates when the user asks to "record a meeting", "start recording", "attend my next meeting", "transcribe", "summarize this note", or otherwise wants to operate heron from the terminal.
---

# heron-meeting

This skill composes the `heron` CLI binaries in this repo into three workflows: **record**, **attend**, **transcribe/re-summarize**. Pick the right one based on what the user asked for, run preflight first, and report the resulting vault markdown path when the session completes.

The repo ships three binaries — only `heron` and `herond` (via `heron daemon …`) are needed here:

- `heron` — CLI driver (`crates/heron-cli/src/main.rs`).
- `herond` — local daemon on `127.0.0.1:7384` (`crates/herond/src/main.rs`). Driven indirectly via `heron daemon …`.
- `heron-doctor` — log analyzer; only used if a session fails.

## Always preflight first

Before any record/attend invocation, run:

```sh
heron status
```

This checks TCC permissions (Mic / Screen Recording / Accessibility), vault path, ringbuffer, and ffmpeg. If it reports missing grants, surface them to the user — heron cannot capture audio without them, and the wizard in the desktop app is the easiest way to grant.

If the user has only just installed heron, also confirm:

- `$HERON_VAULT` is set (or pass `--vault <path>` globally).
- An LLM key is in Keychain (Anthropic or OpenAI). If summarize fails with "no key", that's the cause — check *Settings → API Keys* in the desktop app.

## Workflow 1 — Record (foreground, in-process)

Use this when the user is **already in a meeting** and wants the simplest path. The CLI runs the orchestrator in-process; one Ctrl-C ends the session and writes the markdown.

```sh
heron record --app us.zoom.xos                  # default Zoom native client
heron record --app us.zoom.xos --duration 1h    # hard cap
heron record --vault /path/to/vault             # override vault for this run
```

Notes:
- `--app` takes a macOS bundle ID. Default `us.zoom.xos` (native Zoom). Web Zoom is `us.zoom.us`.
- `--duration` accepts `30s` / `5m` / `2h`. No unit ⇒ seconds.
- `--no-op` exists but is for CI without TCC grants — do not use unless the user explicitly asks for a dry run.
- Transcription happens *inside* `record` (WhisperKit on Apple Neural Engine). There is no separate "transcribe" step for new audio.

When the session ends, look for the new `.md` in the vault and report its path to the user.

## Workflow 2 — Attend (daemon-driven, live SSE events)

Use this when the user wants progress visibility, is scripting the flow, or asked for "attend my next meeting" / "join the meeting". The local daemon (`herond`) owns the session; the CLI is a thin client over its OpenAPI surface.

```sh
# 1. Confirm the daemon is up
heron daemon status

# 2. Start the capture (returns a meeting_id)
heron daemon meeting start --platform zoom
heron daemon meeting start --platform zoom --hint "Q2 planning"
heron daemon meeting start --platform zoom --calendar-event-id <eventkit-id>

# 3. (Optional) stream live events in another terminal / background
heron daemon events                # follows until Ctrl-C
heron daemon events --once         # replay window only, then exit
heron daemon events --since-event-id evt_abc123

# 4. End the capture cleanly when the meeting wraps
heron daemon meeting end <meeting_id>

# 5. Inspect captures
heron daemon meeting list
heron daemon meeting get <meeting_id>
```

Platforms accepted by `--platform`: `zoom`, `google-meet`, `microsoft-teams`, `webex`. Only `zoom` is fully wired in v1; the others are reserved.

Daemon overrides (rarely needed):
- `--url` or `$HERON_DAEMON_URL` — defaults to `http://127.0.0.1:7384/v1`.
- `--token-file` or `$HERON_DAEMON_TOKEN_FILE` — defaults to `~/.heron/cli-token`.

If `heron daemon status` fails, the daemon isn't running. The desktop app starts it; without the GUI, the user has to launch `herond` directly (or run the wizard's "Background service" step).

## Workflow 3 — Transcribe / re-summarize an existing note

`heron` does not transcribe arbitrary audio files — transcription is part of the live `record` flow. What you *can* do is re-run the LLM summary against a note that already has a transcript section:

```sh
heron summarize /path/to/<note>.md                       # default backend: anthropic
heron summarize /path/to/<note>.md --backend claude-code
heron summarize /path/to/<note>.md --backend codex
```

Backends: `anthropic` (default), `claude-code`, `codex`. The note's previous body is rotated to `<note>.md.bak` before the new summary is written, so this is non-destructive.

If the user hands you a raw audio file and asks to transcribe it, that is **not supported by the CLI today** — say so. The transcript is produced live during `record`; there's no `heron transcribe <wav>` command.

## Recovery — orphaned sessions

If a previous run crashed or was force-killed, surface this on next launch:

```sh
heron salvage                    # human format
heron salvage --format json      # machine-parsable
```

Exit codes are meaningful: `0` clean, `3` candidates found, `2` IO error. A non-zero `3` is normal recovery, not a failure — surface the candidates to the user before running anything else.

## Diagnostics

If a session fails or the summary doesn't appear:

- `heron-doctor` walks `~/Library/Logs/heron/<date>.log` and surfaces the most recent errors with a fix suggestion.
- `heron ax-dump --bundle us.zoom.xos` dumps the accessibility tree of the running meeting app — useful if speaker labels all came back as `them` (means accessibility permission is missing or the participant list is collapsed).

## Choosing between Record and Attend

| Signal | Use |
|---|---|
| User is mid-meeting, wants the shortest path | `heron record` |
| User wants progress events, has a script in the loop, or asked to "attend" | `heron daemon meeting start` + `heron daemon events` |
| User mentioned a calendar event ID or wants pre-meeting context applied | `heron daemon meeting start --calendar-event-id …` |
| Daemon is down and user just wants to capture now | `heron record` |

Default to **Record** when ambiguous — it has fewer moving parts.

## What to report back

After the session ends, tell the user:
1. The path to the resulting `<id>.md` in the vault.
2. Which LLM backend produced the summary (only relevant for `summarize`).
3. Any TCC / permission warnings `heron status` surfaced — they affect future runs.

Do not paste the full transcript into the conversation — it is often long and the user can open the file in Obsidian.
