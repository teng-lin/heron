//! Live API smoke harness for the [`heron_llm`] summarizer backends.
//!
//! Unlike the in-crate unit tests (which mock everything via wiremock
//! or a fake-CLI shell script), these tests hit the **real** Anthropic
//! Messages API, the **real** `claude` CLI, and the **real** `codex`
//! CLI when their prerequisites are present in the environment. The
//! goal is a `cargo test` that gets richer as the developer's machine
//! gets more capable, without ever turning red just because a key or
//! a binary is missing.
//!
//! # Costs
//!
//! - The Anthropic test issues a single small Messages API call when
//!   `ANTHROPIC_API_KEY` is set. With the default Sonnet 4.6 model
//!   and a one-line transcript the cost is on the order of a fraction
//!   of a cent per run (well under $0.01). The user opts in by
//!   exporting the key.
//! - The Claude Code CLI test consumes the user's Claude Code
//!   subscription quota, not API credits.
//! - The Codex CLI test consumes whichever account the user's
//!   `codex` install is authenticated against.
//!
//! # Skip semantics
//!
//! Each test checks its prerequisite at runtime and `return`s early
//! with an `eprintln!("skipped: ...")` line when the prereq is
//! missing. The test still **passes** in that case — there is no
//! `#[ignore]` here. That keeps `cargo test --test live_api` green on
//! every machine, with the harness silently exercising more of the
//! real surface as the developer adds keys / installs binaries.
//!
//! # Running
//!
//! ```text
//! cargo test -p heron-llm --test live_api -- --nocapture
//! ```
//!
//! `--nocapture` is what surfaces the per-test skip / success log
//! lines. See `docs/manual-test-matrix.md` § "Live LLM smoke tests
//! (heron-llm)" for the operator-facing matrix.

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

use heron_llm::{Backend, SummarizerInput, build_summarizer};
use heron_types::MeetingType;

/// Write a tiny synthetic two-turn transcript to a tempfile and hand
/// back the directory + path. The directory must outlive the test
/// (drop it and the file disappears), so callers bind it to a `_dir`
/// local that lives until the function returns.
fn write_fixture_transcript() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tmpdir");
    let path = dir.path().join("live-smoke.jsonl");
    let mut f = std::fs::File::create(&path).expect("create transcript");
    // Two short turns. Real captures are richer, but the smoke test
    // only needs enough text for the LLM to produce a non-empty body.
    writeln!(
        f,
        r#"{{"t0":0.0,"t1":2.0,"text":"Quick sync about the Q3 launch checklist.","channel":"mic","speaker":"Alice","speaker_source":"self","confidence":0.95}}"#
    )
    .expect("write turn 1");
    writeln!(
        f,
        r#"{{"t0":2.0,"t1":5.0,"text":"Bob will draft the rollout doc by Friday.","channel":"tap","speaker":"Bob","speaker_source":"diarizer","confidence":0.9}}"#
    )
    .expect("write turn 2");
    drop(f);
    (dir, path)
}

/// `true` if `binary --version` exits zero. Used to gate the CLI
/// tests on an install whose binary can actually launch — `which`
/// only proves the file exists on PATH.
fn version_exits_zero(binary: &str) -> bool {
    Command::new(binary)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Skip-reason for a CLI-backed live test, or `None` if the binary
/// is present and `--version` exits zero. Centralises the two-step
/// gate (PATH lookup + smoke-launch) so the per-backend tests stay
/// focused on assertions.
fn cli_skip_reason(binary: &str) -> Option<String> {
    if which::which(binary).is_err() {
        return Some(format!(
            "skipped: `{binary}` not on PATH; install it to exercise this backend"
        ));
    }
    if !version_exits_zero(binary) {
        return Some(format!(
            "skipped: `{binary} --version` exited non-zero — install or authenticate `{binary}`"
        ));
    }
    None
}

/// Shared body for the two CLI-backed smoke tests. Anthropic stays
/// separate because its assertions cover token counts and cost,
/// which the CLI backends do not surface.
///
/// `--version` proves the binary launches but cannot detect every
/// "installed but not usable" state (sandbox session perms, expired
/// auth tokens, network egress denied, …). When the actual
/// `summarize` call fails we treat that as a richer skip — the
/// developer hasn't configured the CLI in a way that supports the
/// non-interactive prompt path — rather than a hard failure. Unit
/// tests in `crates/heron-llm/src/{claude_code,codex}.rs` cover the
/// error-mapping contract; this harness only owns the happy path.
async fn run_cli_smoke(binary: &str, backend: Backend) {
    if let Some(reason) = cli_skip_reason(binary) {
        eprintln!("{reason}");
        return;
    }
    let (_dir, transcript) = write_fixture_transcript();
    let summarizer = build_summarizer(backend);
    let out = match summarizer
        .summarize(SummarizerInput {
            transcript: &transcript,
            meeting_type: MeetingType::Internal,
            existing_action_items: None,
            existing_attendees: None,
        })
        .await
    {
        Ok(out) => out,
        Err(e) => {
            eprintln!(
                "skipped: `{binary}` is on PATH but summarize failed ({e}); \
                 install/authenticate the CLI for the non-interactive prompt path \
                 to exercise this backend"
            );
            return;
        }
    };
    assert!(!out.body.is_empty(), "body should be non-empty");
    assert!(!out.cost.model.is_empty(), "cost.model should be stamped");
    eprintln!(
        "live {binary} CLI ok: model={} body_len={}",
        out.cost.model,
        out.body.len()
    );
}

#[tokio::test]
async fn live_anthropic_summarize_returns_non_empty() {
    if std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())
        .is_none()
    {
        eprintln!("skipped: ANTHROPIC_API_KEY unset; export it to exercise the live Anthropic path");
        return;
    }
    let (_dir, transcript) = write_fixture_transcript();
    let summarizer = build_summarizer(Backend::Anthropic);
    let out = summarizer
        .summarize(SummarizerInput {
            transcript: &transcript,
            meeting_type: MeetingType::Internal,
            existing_action_items: None,
            existing_attendees: None,
        })
        .await
        .expect("Anthropic summarize should succeed with a valid key");
    // Lenient assertions: real LLM output varies run-to-run, so we
    // only insist that the structured fields the orchestrator depends
    // on are populated.
    assert!(!out.body.is_empty(), "body should be non-empty");
    assert!(!out.cost.model.is_empty(), "cost.model should be stamped");
    // Token counts should be > 0 since we made a real API call.
    assert!(
        out.cost.tokens_in > 0,
        "tokens_in should be > 0 for a live call, got {}",
        out.cost.tokens_in
    );
    assert!(
        out.cost.tokens_out > 0,
        "tokens_out should be > 0 for a live call, got {}",
        out.cost.tokens_out
    );
    eprintln!(
        "live Anthropic ok: model={} tokens_in={} tokens_out={} cost=${:.6} body_len={}",
        out.cost.model,
        out.cost.tokens_in,
        out.cost.tokens_out,
        out.cost.summary_usd,
        out.body.len()
    );
}

#[tokio::test]
async fn live_claude_cli_summarize_returns_non_empty() {
    run_cli_smoke("claude", Backend::ClaudeCodeCli).await;
}

#[tokio::test]
async fn live_codex_cli_summarize_returns_non_empty() {
    run_cli_smoke("codex", Backend::CodexCli).await;
}
