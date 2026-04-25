//! JSONL session-summary reader for `~/Library/Logs/heron/`.
//!
//! The schema mirrors `docs/observability.md` `log_version: 1`. We
//! redefine the field set locally rather than depending on
//! `heron-cli` to avoid pulling the audio / LLM / Tauri stack into a
//! diagnostics binary that just reads JSON.
//!
//! The reader is **permissive**: every field is `Option`, unknown
//! fields are ignored, and a malformed line is silently skipped.
//! The whole point of this tool is to surface anomalies on
//! partially-broken corpora; failing the entire read on the first
//! bad line would defeat that.
//!
//! The reader is also **streaming**: lines beyond [`MAX_LINE_LEN`]
//! are dropped before any `serde_json::from_slice` allocation, so a
//! malicious or accidentally-huge log file can't OOM the process.

use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Largest line we'll attempt to parse. Real session-summary lines
/// are < 1 KiB; anything orders of magnitude bigger is a sign of
/// corruption (or a different log format) and should be ignored.
pub const MAX_LINE_LEN: usize = 64 * 1024;

#[derive(Debug, Error)]
pub enum LogReadError {
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// One parsed session-summary line. Keep the field set in sync with
/// `docs/observability.md` and `heron_cli::session_log::SessionSummary`.
///
/// `serde` already treats `Option<T>` as default-`None` when the key
/// is absent, so most fields don't carry an explicit `#[serde(default)]`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionSummaryRecord {
    /// Stable schema marker per `docs/observability.md`. `0` = the
    /// field was absent in the source line; treat as legacy.
    #[serde(default)]
    pub log_version: u32,
    pub ts: Option<DateTime<Utc>>,
    pub level: Option<String>,
    pub session_id: Option<String>,
    pub module: Option<String>,
    pub msg: Option<String>,
    pub fields: Option<SessionSummaryFields>,
}

/// Counts + cost only — never user content (per `docs/observability.md`
/// privacy invariant). All fields are optional so a partial record
/// from a future schema version still parses.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SessionSummaryFields {
    pub kind: Option<String>,
    pub duration_secs: Option<f64>,
    pub source_app: Option<String>,
    pub diarize_source: Option<String>,
    pub ax_hit_pct: Option<f64>,
    pub channel_fallback_pct: Option<f64>,
    pub self_pct: Option<f64>,
    pub turns_total: Option<u32>,
    pub low_conf_turns: Option<u32>,
    pub audio_dropped_frames: Option<u32>,
    pub aec_event_count: Option<u32>,
    pub device_changes: Option<u32>,
    pub summarize_cost_usd: Option<f64>,
    pub summarize_tokens_in: Option<u64>,
    pub summarize_tokens_out: Option<u64>,
    pub summarize_model: Option<String>,
}

/// Read every `kind: "session_summary"` line from `path`.
///
/// Lines that fail to parse OR that have a `kind` other than
/// `session_summary` are skipped (the heron `tracing` subscriber
/// emits other levels into the same file). A missing file resolves
/// to an empty Vec — first-run state is "no logs yet."
///
/// Streaming via [`BufReader::lines`]: we never hold more than one
/// line in memory at a time, and lines longer than [`MAX_LINE_LEN`]
/// are skipped before deserialization. A multi-GB log file is safe
/// to point this at.
///
/// Lines whose `log_version` is not 1 are still parsed (forwards
/// compat), but the count of mismatched versions is reported via
/// [`UnknownVersionCount`] so the caller can surface a warning.
pub fn read_session_summaries(path: &Path) -> Result<Vec<SessionSummaryRecord>, LogReadError> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(LogReadError::Io(e)),
    };
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for line_result in reader.lines() {
        let line = match line_result {
            Ok(l) => l,
            // A read error mid-file (e.g. the file got truncated)
            // surfaces as an IO error; let the caller decide.
            Err(e) => return Err(LogReadError::Io(e)),
        };
        if line.is_empty() || line.len() > MAX_LINE_LEN {
            continue;
        }
        let Ok(record) = serde_json::from_str::<SessionSummaryRecord>(&line) else {
            continue;
        };
        if record.fields.as_ref().and_then(|f| f.kind.as_deref()) == Some("session_summary") {
            out.push(record);
        }
    }
    Ok(out)
}

/// Count records whose `log_version` is not 1. The CLI uses this to
/// emit a one-line stderr warning so a future schema bump doesn't
/// silently mis-report.
pub fn count_unknown_versions(records: &[SessionSummaryRecord]) -> usize {
    records
        .iter()
        .filter(|r| r.log_version != 0 && r.log_version != 1)
        .count()
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture_record(session_id: &str, cost: f64) -> String {
        format!(
            r#"{{"log_version":1,"ts":"2026-04-24T15:18:42.011Z","level":"INFO",
            "session_id":"{session_id}","module":"heron_session::summary",
            "msg":"session complete","fields":{{
              "kind":"session_summary","duration_secs":1800.0,
              "source_app":"us.zoom.xos","diarize_source":"ax",
              "ax_hit_pct":0.71,"channel_fallback_pct":0.29,"self_pct":0.18,
              "turns_total":300,"low_conf_turns":20,"audio_dropped_frames":0,
              "aec_event_count":1,"device_changes":0,
              "summarize_cost_usd":{cost},"summarize_tokens_in":10000,
              "summarize_tokens_out":500,"summarize_model":"claude-sonnet-4-6"
            }}}}"#
        )
        .replace('\n', "")
        .replace("            ", "")
    }

    #[test]
    fn missing_file_returns_empty() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let path = tmp.path().join("never.log");
        let read = read_session_summaries(&path).expect("read");
        assert!(read.is_empty());
    }

    #[test]
    fn reads_one_session_summary_line() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let path = tmp.path().join("today.log");
        fs::write(&path, fixture_record("abc", 0.04)).expect("seed");

        let read = read_session_summaries(&path).expect("read");
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].session_id.as_deref(), Some("abc"));
        let f = read[0].fields.as_ref().expect("fields");
        assert_eq!(f.summarize_cost_usd, Some(0.04));
    }

    #[test]
    fn ignores_non_summary_records() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let path = tmp.path().join("mixed.log");
        let lines = format!(
            "{{\"level\":\"INFO\",\"msg\":\"audio thread started\"}}\n{}\n{{\"foo\":\"bar\"}}\n",
            fixture_record("xyz", 0.02)
        );
        fs::write(&path, lines).expect("seed");
        let read = read_session_summaries(&path).expect("read");
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].session_id.as_deref(), Some("xyz"));
    }

    #[test]
    fn skips_malformed_lines() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let path = tmp.path().join("bad.log");
        let lines = format!("not json {{\n{}\n", fixture_record("ok", 0.01));
        fs::write(&path, lines).expect("seed");
        let read = read_session_summaries(&path).expect("read");
        assert_eq!(read.len(), 1);
    }

    #[test]
    fn empty_lines_are_ignored() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let path = tmp.path().join("blank.log");
        let lines = format!("\n\n{}\n\n", fixture_record("ok", 0.01));
        fs::write(&path, lines).expect("seed");
        let read = read_session_summaries(&path).expect("read");
        assert_eq!(read.len(), 1);
    }

    #[test]
    fn unknown_fields_are_tolerated() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let path = tmp.path().join("future.log");
        let line = r#"{"log_version":2,"session_id":"v2","module":"heron_session::summary",
            "fields":{"kind":"session_summary","new_field_v2":"hi"}}"#
            .replace('\n', "")
            .replace("            ", "");
        fs::write(&path, line).expect("seed");
        let read = read_session_summaries(&path).expect("read");
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].log_version, 2);
        assert_eq!(count_unknown_versions(&read), 1);
    }

    #[test]
    fn lines_above_max_length_are_skipped() {
        // A 200 KiB line: bigger than MAX_LINE_LEN, should silently
        // be dropped (no parse, no allocation of the parsed record).
        let tmp = tempfile::TempDir::new().expect("tmp");
        let path = tmp.path().join("huge.log");
        let huge = "x".repeat(200 * 1024);
        let lines = format!("{huge}\n{}\n", fixture_record("ok", 0.01));
        fs::write(&path, lines).expect("seed");
        let read = read_session_summaries(&path).expect("read");
        assert_eq!(read.len(), 1, "the over-long line must not parse");
    }

    #[test]
    fn count_unknown_versions_zero_when_all_v1() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let path = tmp.path().join("all-v1.log");
        let lines = format!(
            "{}\n{}\n",
            fixture_record("a", 0.01),
            fixture_record("b", 0.02)
        );
        fs::write(&path, lines).expect("seed");
        let read = read_session_summaries(&path).expect("read");
        assert_eq!(count_unknown_versions(&read), 0);
    }
}
