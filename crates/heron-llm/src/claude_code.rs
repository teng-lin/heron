//! Claude Code CLI summarizer per
//! [`docs/archives/implementation.md`](../../../docs/archives/implementation.md) §11.1.
//!
//! Spawns `claude` as a subprocess with the rendered prompt + the
//! transcript contents on stdin, parses the JSON it emits on stdout
//! into a [`SummarizerOutput`]. The CLI is the user's existing
//! Claude Code subscription path; cost is reported as zero from
//! heron's perspective since the user pays via Claude Code rather
//! than the API.
//!
//! The summarizer is testable by pointing `binary` at a fake
//! executable that emits the expected JSON shape — used by the unit
//! tests below to exercise the success / failure / timeout paths
//! without requiring a real Claude Code install.
//!
//! ## Why not the SDK
//!
//! The user's spec (§11.1) names the CLI explicitly. Subprocess
//! spawning isolates heron from any future Claude Code SDK churn,
//! and the shell-script test fakes are simpler to author than the
//! HTTP fixtures the API path uses.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use heron_types::{Cost, MeetingType};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::content::parse_content_json;
use crate::metrics_emit::{
    LLM_CALL_DURATION_SECONDS, LLM_CALL_FAILURES_TOTAL, record_call_success,
};
use crate::metrics_labels::{backend_label, model_label};
use crate::transcript::{build_user_content, read_transcript_capped, strip_speaker_names};
use crate::{
    Backend, LlmError, Summarizer, SummarizerInput, SummarizerOutput, render_meeting_prompt,
};

/// Default binary name resolved on `PATH`. The user can override via
/// [`ClaudeCodeClientConfig::binary`] (e.g. for a `claude` install
/// that lives outside the standard locations).
pub const DEFAULT_BINARY: &str = "claude";

/// Default `model` field stamped onto the [`Cost`] record. The CLI
/// doesn't expose token counts in `--print` mode today, so the
/// numeric fields stay zero — the model string is the only useful
/// breadcrumb for the diagnostics tab.
pub const DEFAULT_MODEL: &str = "claude-code-cli";

/// Default subprocess timeout. CLI invocations typically complete in
/// 10–60 s for our prompt sizes; 180 s is a generous safety net for
/// a long transcript plus thinking-block reasoning.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(180);

/// Caller-supplied configuration. Defaults invoke `claude --print
/// --output-format=json` and pipe the rendered prompt to stdin.
/// Tests substitute `binary` to point at a fake script.
#[derive(Debug, Clone)]
pub struct ClaudeCodeClientConfig {
    /// Path to (or name of) the `claude` binary.
    pub binary: PathBuf,
    /// Arguments passed to the binary. The default is `--print` so
    /// the CLI emits its answer without entering an interactive
    /// session. Tests can supply alternative args (e.g. flags
    /// understood by their fake script).
    pub args: Vec<String>,
    /// Model identifier stamped onto the cost record.
    pub model: String,
    /// Subprocess timeout.
    pub timeout: Duration,
}

impl Default for ClaudeCodeClientConfig {
    fn default() -> Self {
        Self {
            binary: PathBuf::from(DEFAULT_BINARY),
            args: vec!["--print".to_owned()],
            model: DEFAULT_MODEL.to_owned(),
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

/// Claude Code CLI summarizer.
///
/// One config per session. Constructing the client doesn't probe the
/// binary; missing-binary surfaces at first `summarize` call as a
/// [`LlmError::Backend`] with the spawn error.
pub struct ClaudeCodeClient {
    config: ClaudeCodeClientConfig,
}

impl ClaudeCodeClient {
    pub fn new(config: ClaudeCodeClientConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &ClaudeCodeClientConfig {
        &self.config
    }
}

#[async_trait]
impl Summarizer for ClaudeCodeClient {
    async fn summarize(&self, input: SummarizerInput<'_>) -> Result<SummarizerOutput, LlmError> {
        heron_metrics::timed_io_async(
            LLM_CALL_DURATION_SECONDS,
            LLM_CALL_FAILURES_TOTAL,
            ("backend", backend_label(Backend::ClaudeCodeCli)),
            self.summarize_inner(input),
        )
        .await
    }
}

impl ClaudeCodeClient {
    async fn summarize_inner(
        &self,
        input: SummarizerInput<'_>,
    ) -> Result<SummarizerOutput, LlmError> {
        let prompt = render_meeting_prompt(&input)?;
        let transcript_text = read_transcript_capped(input.transcript)?;
        // Tier 4 #21: pseudonymize speaker names for the LLM input.
        let transcript_for_llm = if input.strip_names {
            strip_speaker_names(&transcript_text)
        } else {
            transcript_text
        };
        let user_content = build_user_content(&prompt, &transcript_for_llm);
        let output = run_cli_summarize(
            &self.config.binary,
            &self.config.args,
            &self.config.model,
            self.config.timeout,
            &user_content,
            input.meeting_type,
        )
        .await?;
        // The CLI cost record stamps a model breadcrumb but no token
        // counts; emit zero-token counters for shape parity with the
        // API backends — dashboards aggregating "total LLM tokens"
        // across backends pick up the CLI rows as 0 contributions.
        record_call_success(
            backend_label(Backend::ClaudeCodeCli),
            model_label(&output.cost.model),
            output.cost.tokens_in,
            output.cost.tokens_out,
            output.cost.summary_usd,
        );
        Ok(output)
    }
}

/// Spawn the CLI subprocess with `args`, write `user_content` to
/// stdin, wait for stdout, parse the structured JSON, and return a
/// [`SummarizerOutput`] with a zero-cost record stamped with `model`.
///
/// Pulled out of the trait impl so the upcoming `codex.rs` backend
/// (phase 39) reuses it without duplicating the spawn / wait /
/// parse pipeline.
pub(crate) async fn run_cli_summarize(
    binary: &Path,
    args: &[String],
    model: &str,
    timeout: Duration,
    user_content: &str,
    fallback_meeting_type: MeetingType,
) -> Result<SummarizerOutput, LlmError> {
    let mut cmd = Command::new(binary);
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Don't inherit the controlling tty — `claude` defaults to
        // interactive when stdin is a tty even with --print, and we
        // explicitly want the non-interactive path.
        .kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|e| {
        LlmError::Backend(format!(
            "spawn {bin:?}: {e}; install Claude Code or pass --binary",
            bin = binary
        ))
    })?;

    // Feed the prompt via stdin so we don't run into argv length
    // limits on long transcripts.
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| LlmError::Backend("failed to capture child stdin pipe".to_owned()))?;
        stdin
            .write_all(user_content.as_bytes())
            .await
            .map_err(LlmError::Io)?;
        // Dropping stdin sends EOF; some CLIs wait for that before
        // emitting their answer.
    }

    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            return Err(LlmError::Backend(format!(
                "wait_with_output failed for {bin:?}: {e}",
                bin = binary
            )));
        }
        Err(_) => {
            return Err(LlmError::Backend(format!(
                "{bin:?} did not exit within {secs}s; killed",
                bin = binary,
                secs = timeout.as_secs()
            )));
        }
    };

    if !output.status.success() {
        let code = output
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "<signal>".to_owned());
        let stderr_snippet = String::from_utf8_lossy(&output.stderr);
        let truncated: String = stderr_snippet.chars().take(2_048).collect();
        return Err(LlmError::Backend(format!(
            "{bin:?} exited {code}: {truncated}",
            bin = binary
        )));
    }

    let stdout = std::str::from_utf8(&output.stdout).map_err(|e| {
        LlmError::Backend(format!("{bin:?} stdout was not UTF-8: {e}", bin = binary))
    })?;
    let body = parse_content_json(stdout.trim())?;
    Ok(SummarizerOutput {
        body: body.body,
        company: body.company,
        meeting_type: body.meeting_type.unwrap_or(fallback_meeting_type),
        tags: body.tags.unwrap_or_default(),
        action_items: body.action_items.unwrap_or_default(),
        attendees: body.attendees.unwrap_or_default(),
        cost: Cost {
            // The CLI doesn't expose per-call token counts; the user
            // pays through their Claude Code subscription, not via a
            // billed API key. Stamp the model breadcrumb so the
            // diagnostics tab can render which backend produced the
            // summary.
            summary_usd: 0.0,
            tokens_in: 0,
            tokens_out: 0,
            model: model.to_owned(),
        },
    })
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::io::Write;
    use std::path::PathBuf;

    use heron_types::MeetingType;

    use super::*;

    fn write_tmp_jsonl(lines: &[&str]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("transcript.jsonl");
        let mut f = std::fs::File::create(&path).expect("create");
        for line in lines {
            writeln!(f, "{line}").expect("write");
        }
        (dir, path)
    }

    /// Write a fake-CLI shell script that emits `payload` on stdout.
    /// Used to exercise the summarizer pipeline without a real
    /// `claude` install.
    fn fake_cli(payload: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("fake-claude.sh");
        let mut f = std::fs::File::create(&path).expect("create");
        // Drain stdin so the parent's pipe close doesn't EPIPE; cat
        // > /dev/null is a portable spelling.
        writeln!(
            f,
            "#!/bin/sh\ncat > /dev/null\ncat <<'EOF'\n{payload}\nEOF\n"
        )
        .expect("write");
        drop(f);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).expect("meta").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).expect("chmod");
        }
        (dir, path)
    }

    /// Same shape as `fake_cli` but exits with the supplied non-zero
    /// status after emitting `stderr_msg` on stderr. Used to cover
    /// the failure path.
    fn fake_cli_failure(exit_code: i32, stderr_msg: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("fake-claude-fail.sh");
        let mut f = std::fs::File::create(&path).expect("create");
        writeln!(
            f,
            "#!/bin/sh\ncat > /dev/null\nprintf '%s' '{stderr_msg}' >&2\nexit {exit_code}\n"
        )
        .expect("write");
        drop(f);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).expect("meta").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).expect("chmod");
        }
        (dir, path)
    }

    fn cfg_with_binary(binary: PathBuf) -> ClaudeCodeClientConfig {
        ClaudeCodeClientConfig {
            binary,
            args: vec![],
            model: DEFAULT_MODEL.to_owned(),
            timeout: Duration::from_secs(5),
        }
    }

    #[tokio::test]
    async fn happy_path_round_trips_via_fake_cli() {
        let payload = r#"{
            "body":"summary from fake CLI",
            "meeting_type":"client",
            "tags":["acme"],
            "action_items":[],
            "attendees":[]
        }"#;
        let (_d, fake) = fake_cli(payload);
        let (_t, transcript) = write_tmp_jsonl(&[r#"{"text":"hi"}"#]);
        let client = ClaudeCodeClient::new(cfg_with_binary(fake));
        let out = client
            .summarize(SummarizerInput {
                transcript: &transcript,
                meeting_type: MeetingType::Other,
                existing_action_items: None,
                existing_attendees: None,
                pre_meeting_briefing: None,
                persona: None,
                strip_names: false,
            })
            .await
            .expect("summarize");
        assert_eq!(out.body, "summary from fake CLI");
        assert_eq!(out.tags, vec!["acme".to_owned()]);
        assert_eq!(out.meeting_type, MeetingType::Client);
        assert_eq!(out.cost.model, DEFAULT_MODEL);
        assert_eq!(out.cost.summary_usd, 0.0);
    }

    #[tokio::test]
    async fn falls_back_to_caller_meeting_type_when_omitted() {
        let payload = r#"{"body":"no type specified"}"#;
        let (_d, fake) = fake_cli(payload);
        let (_t, transcript) = write_tmp_jsonl(&[r#"{"text":"hi"}"#]);
        let client = ClaudeCodeClient::new(cfg_with_binary(fake));
        let out = client
            .summarize(SummarizerInput {
                transcript: &transcript,
                meeting_type: MeetingType::Internal,
                existing_action_items: None,
                existing_attendees: None,
                pre_meeting_briefing: None,
                persona: None,
                strip_names: false,
            })
            .await
            .expect("ok");
        assert_eq!(out.meeting_type, MeetingType::Internal);
    }

    #[tokio::test]
    async fn missing_binary_surfaces_clean_backend_error() {
        let phantom = PathBuf::from("/nonexistent/path/to/claude-binary");
        let (_t, transcript) = write_tmp_jsonl(&[r#"{"text":"hi"}"#]);
        let client = ClaudeCodeClient::new(cfg_with_binary(phantom));
        let err = client
            .summarize(SummarizerInput {
                transcript: &transcript,
                meeting_type: MeetingType::Client,
                existing_action_items: None,
                existing_attendees: None,
                pre_meeting_briefing: None,
                persona: None,
                strip_names: false,
            })
            .await
            .expect_err("missing binary");
        match err {
            LlmError::Backend(msg) => {
                assert!(msg.contains("spawn"), "missing spawn marker in: {msg}")
            }
            other => panic!("expected Backend, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn non_zero_exit_propagates_status_and_stderr() {
        let (_d, fake) = fake_cli_failure(7, "nope");
        let (_t, transcript) = write_tmp_jsonl(&[r#"{"text":"hi"}"#]);
        let client = ClaudeCodeClient::new(cfg_with_binary(fake));
        let err = client
            .summarize(SummarizerInput {
                transcript: &transcript,
                meeting_type: MeetingType::Client,
                existing_action_items: None,
                existing_attendees: None,
                pre_meeting_briefing: None,
                persona: None,
                strip_names: false,
            })
            .await
            .expect_err("non-zero");
        match err {
            LlmError::Backend(msg) => {
                assert!(msg.contains("7"), "missing status: {msg}");
                assert!(msg.contains("nope"), "missing stderr: {msg}");
            }
            other => panic!("expected Backend, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn malformed_stdout_surfaces_parse_error() {
        let (_d, fake) = fake_cli("not the JSON shape we asked for");
        let (_t, transcript) = write_tmp_jsonl(&[r#"{"text":"hi"}"#]);
        let client = ClaudeCodeClient::new(cfg_with_binary(fake));
        let err = client
            .summarize(SummarizerInput {
                transcript: &transcript,
                meeting_type: MeetingType::Client,
                existing_action_items: None,
                existing_attendees: None,
                pre_meeting_briefing: None,
                persona: None,
                strip_names: false,
            })
            .await
            .expect_err("malformed");
        assert!(matches!(err, LlmError::Parse(_)));
    }

    #[tokio::test]
    async fn timeout_is_surfaced_as_backend_error() {
        // Fake script that sleeps longer than the configured timeout.
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("fake-slow.sh");
        let mut f = std::fs::File::create(&path).expect("create");
        writeln!(f, "#!/bin/sh\ncat > /dev/null\nsleep 30\necho '{{}}'").expect("write");
        drop(f);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).expect("meta").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).expect("chmod");
        }

        let (_t, transcript) = write_tmp_jsonl(&[r#"{"text":"hi"}"#]);
        let mut cfg = cfg_with_binary(path);
        cfg.timeout = Duration::from_millis(200);
        let client = ClaudeCodeClient::new(cfg);
        let err = client
            .summarize(SummarizerInput {
                transcript: &transcript,
                meeting_type: MeetingType::Client,
                existing_action_items: None,
                existing_attendees: None,
                pre_meeting_briefing: None,
                persona: None,
                strip_names: false,
            })
            .await
            .expect_err("should time out");
        match err {
            LlmError::Backend(msg) => assert!(
                msg.contains("did not exit within"),
                "missing timeout marker: {msg}"
            ),
            other => panic!("expected Backend, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn happy_path_pipes_user_content_via_stdin() {
        // Verify the prompt + transcript actually reaches the
        // subprocess via stdin: write a script that echoes its
        // stdin to a tempfile, then assert the file contents
        // contain the transcript marker.
        let dir = tempfile::tempdir().expect("tmpdir");
        let echo_path = dir.path().join("input.txt");
        let script = dir.path().join("fake-echo.sh");
        let mut f = std::fs::File::create(&script).expect("create");
        // The script reads stdin to a file, then emits a minimal
        // valid JSON answer so the parser is happy.
        writeln!(
            f,
            "#!/bin/sh\ncat > '{}'\ncat <<'EOF'\n{{\"body\":\"ok\"}}\nEOF\n",
            echo_path.display()
        )
        .expect("write");
        drop(f);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).expect("meta").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).expect("chmod");
        }

        let (_t, transcript) =
            write_tmp_jsonl(&[r#"{"t0":0,"t1":1,"text":"distinctive-marker-aaaa"}"#]);
        let client = ClaudeCodeClient::new(cfg_with_binary(script));
        let _out = client
            .summarize(SummarizerInput {
                transcript: &transcript,
                meeting_type: MeetingType::Client,
                existing_action_items: None,
                existing_attendees: None,
                pre_meeting_briefing: None,
                persona: None,
                strip_names: false,
            })
            .await
            .expect("ok");
        let stdin_seen = std::fs::read_to_string(&echo_path).expect("read input");
        assert!(
            stdin_seen.contains("distinctive-marker-aaaa"),
            "transcript not piped to subprocess stdin: {stdin_seen:.200?}"
        );
        assert!(stdin_seen.contains("Transcript JSONL"));
    }
}
