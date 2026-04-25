# Observability

Per [`docs/implementation.md`](implementation.md) §5.7 + §19.2.

## Logging

A single global `tracing_subscriber` is installed at process start.
JSON output, dual sink: file + stderr.

| Property | Value |
|---|---|
| File | `~/Library/Logs/heron/<YYYY-MM-DD>.log` |
| File mode | `0600` (user-only) |
| Format | JSON, one record per line |
| Stderr | enabled, INFO+ |
| Filter | `RUST_LOG`, default `heron=info,warn` |

Field names are stable; the schema is versioned in every record so
log consumers can detect drift across releases:

```json
{
  "log_version": 1,
  "ts": "2026-04-24T14:31:07.421Z",
  "level": "INFO",
  "session_id": "01931e62-7a9f-7c20-bcd1-1f7e5e8a4031",
  "module": "heron_audio::aec",
  "msg": "...",
  "fields": { "...": "..." }
}
```

`log_version` increments only on backwards-incompatible changes
(field renames, type changes). Adding new optional fields is
non-breaking.

## Per-session summary

When a session completes, heron emits one final record on
`SessionEnded` that summarizes the whole capture in one line. This
is the line a session-level log consumer (a future `heron-doctor`
CLI, week 16+) would tail to surface anomalies.

```json
{
  "log_version": 1,
  "ts": "2026-04-24T15:18:42.011Z",
  "level": "INFO",
  "session_id": "01931e62-7a9f-7c20-bcd1-1f7e5e8a4031",
  "module": "heron_session::summary",
  "msg": "session complete",
  "fields": {
    "kind": "session_summary",
    "duration_secs": 2823.4,
    "source_app": "us.zoom.xos",
    "diarize_source": "ax",
    "ax_hit_pct": 0.71,
    "channel_fallback_pct": 0.29,
    "self_pct": 0.18,
    "turns_total": 412,
    "low_conf_turns": 38,
    "audio_dropped_frames": 0,
    "aec_event_count": 2,
    "device_changes": 0,
    "summarize_cost_usd": 0.041,
    "summarize_tokens_in": 14231,
    "summarize_tokens_out": 612,
    "summarize_model": "claude-sonnet-4-6"
  }
}
```

The fields are exactly what the post-session review UI's diagnostics
tab (week 13, §15.4) reads from the JSONL.

## Field stability rules

- Fields only get **added**, never renamed or repurposed.
- Removed fields stay in place for at least one minor version with
  the value `null` so consumers don't crash on missing keys.
- A field rename is a `log_version` bump.

## What goes into events vs. logs

`heron-types::Event` (per §5.2) carries cross-crate state transitions
on the in-process broadcast channel. Logging is downstream of the
event bus: the `tracing` global subscriber owns the file/stderr
sinks, and any crate is free to emit `tracing::info!` independent
of the Event enum.

The two surfaces overlap deliberately: every `Event` variant has a
corresponding log line so a session can be reconstructed from the
log file alone (handy for crash-recovery debugging). But not every
log line corresponds to an `Event` (verbose tap-thread debug logs
don't promote to the event bus).

## What does NOT go into logs

- User audio bytes — never. Audio is on disk under
  `~/Library/Application Support/com.heronnote.heron/recordings/` and
  the only log reference is the path.
- Transcript text — only counts (`turns_total`, `low_conf_turns`).
- Calendar event titles or attendee names — only counts.
- API keys, tokens, signing secrets — assertions enforce this in
  the §11 LLM client (week 9): redact-on-debug-format wrappers.

The vault folder lives inside the user's Dropbox / Drive / iCloud;
heron's logs do not.
