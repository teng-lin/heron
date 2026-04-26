//! Library entry for `heron-cli` exposing the orchestrator skeleton
//! to integration tests + (eventually) the Tauri shell.
//!
//! The binary `heron` lives in `src/main.rs` and re-uses everything
//! here.

pub mod daemon;
pub mod pipeline;
pub mod salvage;
pub mod session;
pub mod session_log;
pub mod summarize;
pub mod synthesize;
