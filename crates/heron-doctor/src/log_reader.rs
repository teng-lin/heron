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
use std::io::{self, BufRead, BufReader, Read};
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
/// Streaming + **bounded read**: we read up to [`MAX_LINE_LEN`] + 1
/// bytes per attempted line via [`Read::take`]. A line whose
/// length exceeds the cap is consumed up to the next newline and
/// discarded *without* allocating the full content, so a multi-GB
/// pathological line cannot OOM the process. We never hold more than
/// `MAX_LINE_LEN + 1` bytes in memory at a time.
///
/// Lines whose `log_version` is not 1 are still parsed (forwards
/// compat); the caller uses [`count_unknown_versions`] to surface a
/// stderr warning.
pub fn read_session_summaries(path: &Path) -> Result<Vec<SessionSummaryRecord>, LogReadError> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(LogReadError::Io(e)),
    };
    let mut reader = BufReader::new(file);
    let mut out = Vec::new();
    loop {
        let mut buf = Vec::with_capacity(256);
        // Read up to MAX_LINE_LEN + 1 bytes so we can detect
        // overflow. `take` adapts the reader for this single read.
        let limit = (MAX_LINE_LEN as u64) + 1;
        let bytes_read = (&mut reader).take(limit).read_until(b'\n', &mut buf)?;
        if bytes_read == 0 {
            break; // EOF
        }
        // Strip trailing \n if present.
        if buf.last() == Some(&b'\n') {
            buf.pop();
        }
        if buf.len() > MAX_LINE_LEN {
            // Over-long line: drain the rest of it (up to the next \n)
            // without keeping bytes around, then move on. This bounds
            // memory at MAX_LINE_LEN + 1 even for adversarial input.
            consume_until_newline(&mut reader)?;
            continue;
        }
        if buf.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<SessionSummaryRecord>(&buf) else {
            continue;
        };
        if record.fields.as_ref().and_then(|f| f.kind.as_deref()) == Some("session_summary") {
            out.push(record);
        }
    }
    Ok(out)
}

/// Read and discard bytes from `reader` up to and including the next
/// newline. Used to skip the tail of an over-long line without
/// allocating its contents.
fn consume_until_newline<R: BufRead>(reader: &mut R) -> io::Result<()> {
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            return Ok(()); // EOF
        }
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let consume = pos + 1;
            reader.consume(consume);
            return Ok(());
        }
        let len = buf.len();
        reader.consume(len);
    }
}

/// Count records whose `log_version` is anything other than 1.
/// `0` is included (it means "field absent in source line" per the
/// `#[serde(default)]` on `SessionSummaryRecord::log_version`); a 0
/// indicates a legacy or non-heron line that snuck through.
pub fn count_unknown_versions(records: &[SessionSummaryRecord]) -> usize {
    records.iter().filter(|r| r.log_version != 1).count()
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
