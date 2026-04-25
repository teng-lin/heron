//! Diagnostics tab backend per §15.4.
//!
//! Reads `heron_session.json` (the per-session log line schema in
//! `docs/observability.md`) and renders AX hit rate, dropped frames,
//! STT wall time, cost, and the error log.
//!
//! Today this module ships the parser + summary computation with
//! permissive deserialization so future schema bumps stay readable.
//! The Tauri command wraps `read_diagnostics` and exposes it to the
//! frontend.
//!
//! The wire shape matches what `docs/observability.md` calls
//! `log_version: 1` and is intentionally tolerant: unknown fields are
//! accepted (and ignored) rather than causing the diagnostics tab to
//! fail to render — a missing-field bug should surface as a "—" in
//! the UI, not a parser error.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DiagnosticsError {
    #[error("session.json not found at {path}")]
    NotFound { path: String },
    #[error("session.json is not valid utf-8: {0}")]
    InvalidUtf8(#[from] std::string::FromUtf8Error),
    #[error("session.json could not be parsed: {0}")]
    Parse(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Permissive view of `heron_session.json`. Every field is optional;
/// the diagnostics tab renders "—" for anything that didn't make it
/// onto disk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SessionLog {
    /// Stable schema marker. Bump on breaking field changes.
    #[serde(default)]
    pub log_version: u32,
    pub session_id: Option<String>,
    /// AX-attribution hit rate over the live window, [0.0, 1.0].
    pub ax_hit_rate: Option<f64>,
    /// Frames the realtime → APM → ringbuffer pipeline dropped under
    /// back-pressure (§7.4).
    pub dropped_frames: Option<u32>,
    /// Wall time the STT backend spent producing the final transcript.
    pub stt_wall_time_secs: Option<f64>,
    /// USD cost of the LLM call that produced the summary, if any.
    pub llm_cost_usd: Option<f64>,
    /// Error log for the session — one entry per surfaced error.
    #[serde(default)]
    pub errors: Vec<SessionLogError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionLogError {
    pub at: Option<String>,
    pub kind: String,
    pub message: String,
}

/// What the diagnostics tab renders. Same shape regardless of which
/// fields are populated; missing fields surface as `None` and the
/// frontend renders "—".
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DiagnosticsView {
    pub session_id: Option<String>,
    pub ax_hit_rate: Option<f64>,
    pub dropped_frames: Option<u32>,
    pub stt_wall_time_secs: Option<f64>,
    pub llm_cost_usd: Option<f64>,
    pub error_count: usize,
    pub errors: Vec<SessionLogError>,
}

impl From<SessionLog> for DiagnosticsView {
    fn from(log: SessionLog) -> Self {
        let error_count = log.errors.len();
        DiagnosticsView {
            session_id: log.session_id,
            ax_hit_rate: log.ax_hit_rate,
            dropped_frames: log.dropped_frames,
            stt_wall_time_secs: log.stt_wall_time_secs,
            llm_cost_usd: log.llm_cost_usd,
            error_count,
            errors: log.errors,
        }
    }
}

/// Read `heron_session.json` at `path` and return its diagnostics view.
///
/// Single `fs::read`; we map ENOENT to [`DiagnosticsError::NotFound`]
/// with the path in the message so a TOCTOU race between an `exists()`
/// pre-check and the read can't drop us into an opaque
/// [`DiagnosticsError::Io`]. Bytes are parsed via
/// [`serde_json::from_slice`] to skip the intermediate `String` copy.
pub fn read_diagnostics(path: &Path) -> Result<DiagnosticsView, DiagnosticsError> {
    let bytes = fs::read(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            DiagnosticsError::NotFound {
                path: path.display().to_string(),
            }
        } else {
            DiagnosticsError::Io(e)
        }
    })?;
    let log: SessionLog = serde_json::from_slice(&bytes)?;
    Ok(DiagnosticsView::from(log))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use std::fs;

    fn write_session(path: &Path, json: &str) {
        fs::write(path, json).expect("seed session.json");
    }

    #[test]
    fn read_full_log_round_trips() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let session = tmp.path().join("heron_session.json");
        write_session(
            &session,
            r#"{
              "log_version": 1,
              "session_id": "abc-123",
              "ax_hit_rate": 0.92,
              "dropped_frames": 7,
              "stt_wall_time_secs": 12.5,
              "llm_cost_usd": 0.0123,
              "errors": [
                {"at": "2026-04-01T12:00:00Z", "kind": "stt", "message": "model load slow"}
              ]
            }"#,
        );
        let view = read_diagnostics(&session).expect("read");
        assert_eq!(view.session_id.as_deref(), Some("abc-123"));
        assert_eq!(view.ax_hit_rate, Some(0.92));
        assert_eq!(view.dropped_frames, Some(7));
        assert_eq!(view.stt_wall_time_secs, Some(12.5));
        assert_eq!(view.llm_cost_usd, Some(0.0123));
        assert_eq!(view.error_count, 1);
        assert_eq!(view.errors.first().map(|e| e.kind.as_str()), Some("stt"));
    }

    #[test]
    fn unknown_fields_are_tolerated() {
        // Forward-compat: a future version that adds new fields must
        // not break the diagnostics tab on older builds.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let session = tmp.path().join("heron_session.json");
        write_session(
            &session,
            r#"{"session_id":"abc","brand_new_field":42,"ax_hit_rate":0.5}"#,
        );
        let view = read_diagnostics(&session).expect("read");
        assert_eq!(view.ax_hit_rate, Some(0.5));
    }

    #[test]
    fn missing_optional_fields_render_as_none() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let session = tmp.path().join("heron_session.json");
        write_session(&session, r#"{"session_id":"sparse"}"#);
        let view = read_diagnostics(&session).expect("read");
        assert_eq!(view.session_id.as_deref(), Some("sparse"));
        assert!(view.ax_hit_rate.is_none());
        assert!(view.dropped_frames.is_none());
        assert!(view.stt_wall_time_secs.is_none());
        assert!(view.llm_cost_usd.is_none());
        assert_eq!(view.error_count, 0);
        assert!(view.errors.is_empty());
    }

    #[test]
    fn missing_file_errors_with_path() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let path = tmp.path().join("does_not_exist.json");
        let err = read_diagnostics(&path).expect_err("missing");
        match err {
            DiagnosticsError::NotFound { path: p } => assert!(p.contains("does_not_exist.json")),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn malformed_json_errors_clearly() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let session = tmp.path().join("heron_session.json");
        write_session(&session, "not json {");
        let err = read_diagnostics(&session).expect_err("malformed");
        assert!(matches!(err, DiagnosticsError::Parse(_)));
    }
}
