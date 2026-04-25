//! `heron-doctor` — offline diagnosis of session log files.
//!
//! Reads `~/Library/Logs/heron/<YYYY-MM-DD>.log` (one daily JSONL
//! file per `docs/observability.md`), parses each `kind:
//! "session_summary"` record, and reports anomalies against the v1
//! ship-criteria thresholds in `docs/implementation.md` §18.2.
//!
//! Pure offline: no network, no model loads, no auth. The §15.4
//! diagnostics tab consumes a single session's record; this binary
//! is the *cross-session* counterpart for the user (and for the
//! eventual `heron-doctor` automation hook in §16).

pub mod anomalies;
pub mod log_reader;

pub use anomalies::{Anomaly, AnomalyKind, Thresholds, detect_anomalies};
pub use log_reader::{
    LogReadError, MAX_LINE_LEN, SessionSummaryFields, SessionSummaryRecord, count_unknown_versions,
    read_session_summaries,
};
