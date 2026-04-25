//! Per-session summary writer per §19.2 + `docs/observability.md`.
//!
//! When a session ends, heron emits one JSON line summarizing the
//! whole capture. The session-level log consumer (a future
//! `heron-doctor` CLI, week 16+) tails this to surface anomalies, and
//! the §15.4 diagnostics tab reads it to render AX hit rate / dropped
//! frames / cost.
//!
//! Schema is locked at `log_version: 1`. Adding optional fields is
//! non-breaking; renames or type changes bump the version.
//!
//! ## Privacy
//!
//! No audio bytes, transcript text, calendar titles, or attendee names
//! ever land in this record. The matching field is *count* only —
//! e.g., `turns_total`, not `turns`. The Anthropic API key, signing
//! certs, and any other secret is excluded by construction (this
//! struct has no field for them).

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Stable schema version. Increment on rename / type change of any
/// existing field; new optional fields do *not* bump this.
pub const LOG_VERSION: u32 = 1;

/// One-line session summary record. Field set matches
/// `docs/observability.md`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionSummary {
    pub log_version: u32,
    pub ts: DateTime<Utc>,
    pub level: String,
    pub session_id: String,
    pub module: String,
    pub msg: String,
    pub fields: SessionSummaryFields,
}

/// Counts + cost only. Never holds user content.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionSummaryFields {
    pub kind: String,
    pub duration_secs: f64,
    pub source_app: String,
    pub diarize_source: String,
    pub ax_hit_pct: f64,
    pub channel_fallback_pct: f64,
    pub self_pct: f64,
    pub turns_total: u32,
    pub low_conf_turns: u32,
    pub audio_dropped_frames: u32,
    pub aec_event_count: u32,
    pub device_changes: u32,
    pub summarize_cost_usd: f64,
    pub summarize_tokens_in: u64,
    pub summarize_tokens_out: u64,
    pub summarize_model: String,
}

/// Inputs for [`write_session_summary`]. Caller supplies everything;
/// the writer is pure (no clock or env access of its own) so tests can
/// exercise it deterministically.
#[derive(Debug, Clone)]
pub struct SessionSummaryInputs {
    pub session_id: String,
    pub now: DateTime<Utc>,
    pub source_app: String,
    pub duration_secs: f64,
    pub diarize_source: String,
    pub ax_hit_pct: f64,
    pub channel_fallback_pct: f64,
    pub self_pct: f64,
    pub turns_total: u32,
    pub low_conf_turns: u32,
    pub audio_dropped_frames: u32,
    pub aec_event_count: u32,
    pub device_changes: u32,
    pub summarize_cost_usd: f64,
    pub summarize_tokens_in: u64,
    pub summarize_tokens_out: u64,
    pub summarize_model: String,
}

#[derive(Debug, Error)]
pub enum SessionLogError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("failed to serialize session summary: {0}")]
    Serialize(#[from] serde_json::Error),
}

/// Render the inputs into the wire-format [`SessionSummary`].
///
/// Pure (no I/O). Useful for the §15.4 diagnostics tab which can
/// render this without touching disk, and for tests.
pub fn build_session_summary(inputs: &SessionSummaryInputs) -> SessionSummary {
    SessionSummary {
        log_version: LOG_VERSION,
        ts: inputs.now,
        level: "INFO".to_owned(),
        session_id: inputs.session_id.clone(),
        module: "heron_session::summary".to_owned(),
        msg: "session complete".to_owned(),
        fields: SessionSummaryFields {
            kind: "session_summary".to_owned(),
            duration_secs: inputs.duration_secs,
            source_app: inputs.source_app.clone(),
            diarize_source: inputs.diarize_source.clone(),
            ax_hit_pct: inputs.ax_hit_pct,
            channel_fallback_pct: inputs.channel_fallback_pct,
            self_pct: inputs.self_pct,
            turns_total: inputs.turns_total,
            low_conf_turns: inputs.low_conf_turns,
            audio_dropped_frames: inputs.audio_dropped_frames,
            aec_event_count: inputs.aec_event_count,
            device_changes: inputs.device_changes,
            summarize_cost_usd: inputs.summarize_cost_usd,
            summarize_tokens_in: inputs.summarize_tokens_in,
            summarize_tokens_out: inputs.summarize_tokens_out,
            summarize_model: inputs.summarize_model.clone(),
        },
    }
}

/// Append one JSON line to the daily log file at `log_path`.
///
/// Creates parent directories on demand. The line ends with `\n` so
/// `tail -F` works as expected. The file is opened with `O_APPEND` so
/// concurrent writers from background tasks (the session-end summary
/// + a late `tracing` event) interleave at line boundaries.
pub fn write_session_summary(
    log_path: &Path,
    inputs: &SessionSummaryInputs,
) -> Result<(), SessionLogError> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let summary = build_session_summary(inputs);
    let mut json = serde_json::to_string(&summary)?;
    json.push('\n');

    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .open(log_path)?;
    set_user_only_perms(log_path)?;
    file.write_all(json.as_bytes())?;
    Ok(())
}

#[cfg(unix)]
fn set_user_only_perms(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms)
}

#[cfg(not(unix))]
fn set_user_only_perms(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn fixture_inputs() -> SessionSummaryInputs {
        SessionSummaryInputs {
            session_id: "01931e62-7a9f-7c20-bcd1-1f7e5e8a4031".to_owned(),
            now: chrono::DateTime::parse_from_rfc3339("2026-04-24T15:18:42.011Z")
                .expect("rfc3339")
                .with_timezone(&Utc),
            source_app: "us.zoom.xos".to_owned(),
            duration_secs: 2_823.4,
            diarize_source: "ax".to_owned(),
            ax_hit_pct: 0.71,
            channel_fallback_pct: 0.29,
            self_pct: 0.18,
            turns_total: 412,
            low_conf_turns: 38,
            audio_dropped_frames: 0,
            aec_event_count: 2,
            device_changes: 0,
            summarize_cost_usd: 0.041,
            summarize_tokens_in: 14_231,
            summarize_tokens_out: 612,
            summarize_model: "claude-sonnet-4-6".to_owned(),
        }
    }

    #[test]
    fn build_emits_log_version_1_with_expected_field_set() {
        let summary = build_session_summary(&fixture_inputs());
        assert_eq!(summary.log_version, LOG_VERSION);
        assert_eq!(summary.level, "INFO");
        assert_eq!(summary.fields.kind, "session_summary");
        assert_eq!(summary.fields.turns_total, 412);
        assert_eq!(summary.fields.summarize_model, "claude-sonnet-4-6");
    }

    #[test]
    fn write_appends_one_jsonl_line() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let log = tmp.path().join("2026-04-24.log");

        write_session_summary(&log, &fixture_inputs()).expect("write");
        write_session_summary(&log, &fixture_inputs()).expect("write 2");

        let contents = std::fs::read_to_string(&log).expect("read");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2, "two writes must produce two JSONL lines");

        for line in &lines {
            let parsed: SessionSummary = serde_json::from_str(line).expect("parse");
            assert_eq!(parsed.log_version, LOG_VERSION);
        }
    }

    #[test]
    fn no_pii_field_names_appear() {
        // Privacy invariant from docs/observability.md: counts only,
        // never raw text. Any future drift that adds a `transcript` or
        // `attendees` field should fail this test loudly.
        // Test against full field names rather than short prefixes
        // like "token" — the legit `summarize_tokens_in/_out` *count*
        // fields contain that substring and are explicitly allowed.
        let summary = build_session_summary(&fixture_inputs());
        let json = serde_json::to_string(&summary).expect("serialize");
        for forbidden in [
            "transcript",
            "attendees",
            "calendar_title",
            "api_key",
            "audio_bytes",
            "secret",
        ] {
            assert!(
                !json.contains(forbidden),
                "session summary leaked banned field: {forbidden}"
            );
        }
    }

    #[test]
    fn write_creates_missing_parent_dir() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let nested = tmp.path().join("a/b/c/2026-04-24.log");
        write_session_summary(&nested, &fixture_inputs()).expect("nested write");
        assert!(nested.exists());
    }

    #[cfg(unix)]
    #[test]
    fn written_log_has_user_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().expect("tmp");
        let log = tmp.path().join("2026-04-24.log");
        write_session_summary(&log, &fixture_inputs()).expect("write");
        let mode = std::fs::metadata(&log).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn round_trip_through_serde() {
        let summary = build_session_summary(&fixture_inputs());
        let json = serde_json::to_string(&summary).expect("serialize");
        let parsed: SessionSummary = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, summary);
    }
}
