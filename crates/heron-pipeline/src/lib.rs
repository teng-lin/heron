//! `heron-pipeline` — the v1 capture pipeline as a reusable library.
//!
//! Owns the [`session::Orchestrator`] FSM walk and [`pipeline::run_pipeline`]
//! audio → STT → LLM → vault data flow that `heron record` and the
//! desktop daemon both run. Extracted from `heron-cli` per issue #190
//! so `heron-orchestrator` can depend on the v1 pipeline without
//! pulling the CLI binary's dependency graph (clap, reqwest, the
//! daemon HTTP client) into the daemon's runtime graph.
//!
//! `heron-cli` re-exports `pub use heron_pipeline::{session, pipeline};`
//! so existing callers (`heron_cli::session::Orchestrator::with_test_backends`,
//! `heron_cli::session::SessionConfig`) keep compiling unchanged.

pub mod pipeline;
pub mod session;
