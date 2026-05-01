//! Library entry for `heron-cli` exposing the orchestrator skeleton
//! to integration tests + (eventually) the Tauri shell.
//!
//! The binary `heron` lives in `src/main.rs` and re-uses everything
//! here.
//!
//! Per issue #190 the v1 capture pipeline (`session::Orchestrator` +
//! `pipeline::run_pipeline`) lives in the `heron-pipeline` crate so
//! `heron-orchestrator` can depend on it without pulling `heron-cli`'s
//! CLI surface into the daemon runtime. The two modules are
//! re-exported below so existing callers — including
//! `heron_cli::session::Orchestrator::with_test_backends` exercised by
//! `crates/herond/tests/clio_full_pipeline.rs` — keep compiling
//! against the original module paths.

pub use heron_pipeline::{pipeline, session};

pub mod daemon;
pub mod record_delegate;
pub mod salvage;
pub mod session_log;
pub mod summarize;
pub mod synthesize;
