//! Codex CLI summarizer per
//! [`docs/archives/implementation.md`](../../../docs/archives/implementation.md) §11.1.
//!
//! Spawns `codex exec` as a subprocess with the rendered prompt +
//! the transcript on stdin and parses the structured JSON it emits
//! on stdout. Mirrors the [`crate::claude_code`] shape — the shared
//! spawn-and-wait pipeline lives in that module so adding a third
//! subprocess backend (e.g. `gemini`) is one new file, not a
//! duplicated loop.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;

use crate::claude_code::run_cli_summarize;
use crate::transcript::{build_user_content, read_transcript_capped, strip_speaker_names};
use crate::{LlmError, Summarizer, SummarizerInput, SummarizerOutput, render_meeting_prompt};

/// Default binary name resolved on `PATH`. The user can override via
/// [`CodexClientConfig::binary`].
pub const DEFAULT_BINARY: &str = "codex";

/// Default `model` field stamped onto the `Cost` record. The CLI
/// doesn't expose token counts in non-interactive mode today; this
/// string is the breadcrumb the diagnostics tab renders.
pub const DEFAULT_MODEL: &str = "codex-cli";

/// Default subprocess timeout. Codex completion times are similar
/// to Claude Code's; 180 s is the same generous safety net.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(180);

/// Caller-supplied configuration. `Default` invokes
/// `codex exec --skip-git-repo-check --full-auto -` (read prompt
/// from stdin). Tests substitute `binary` to point at a fake script.
#[derive(Debug, Clone)]
pub struct CodexClientConfig {
    /// Path to (or name of) the `codex` binary.
    pub binary: PathBuf,
    /// Arguments passed to the binary. Defaults to the
    /// non-interactive recipe.
    pub args: Vec<String>,
    /// Model identifier stamped onto the cost record.
    pub model: String,
    /// Subprocess timeout.
    pub timeout: Duration,
}

impl Default for CodexClientConfig {
    fn default() -> Self {
        Self {
            binary: PathBuf::from(DEFAULT_BINARY),
            // `codex exec -` reads the prompt from stdin in
            // non-interactive mode. `--skip-git-repo-check` keeps
            // codex from refusing to run when heron's cache dir
            // isn't a git repo. `--full-auto` suppresses
            // prompt-for-confirmation flows we don't have a UI for.
            args: vec![
                "exec".to_owned(),
                "--skip-git-repo-check".to_owned(),
                "--full-auto".to_owned(),
                "-".to_owned(),
            ],
            model: DEFAULT_MODEL.to_owned(),
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

/// Codex CLI summarizer.
pub struct CodexClient {
    config: CodexClientConfig,
}

impl CodexClient {
    pub fn new(config: CodexClientConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &CodexClientConfig {
        &self.config
    }
}

#[async_trait]
impl Summarizer for CodexClient {
    async fn summarize(&self, input: SummarizerInput<'_>) -> Result<SummarizerOutput, LlmError> {
        let prompt = render_meeting_prompt(&input)?;
        let transcript_text = read_transcript_capped(input.transcript)?;
        // Tier 4 #21: pseudonymize speaker names for the LLM input.
        let transcript_for_llm = if input.strip_names {
            strip_speaker_names(&transcript_text)
        } else {
            transcript_text
        };
        let user_content = build_user_content(&prompt, &transcript_for_llm);
        run_cli_summarize(
            &self.config.binary,
            &self.config.args,
            &self.config.model,
            self.config.timeout,
            &user_content,
            input.meeting_type,
        )
        .await
    }
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

    fn fake_cli(payload: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("fake-codex.sh");
        let mut f = std::fs::File::create(&path).expect("create");
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

    fn cfg_with_binary(binary: PathBuf) -> CodexClientConfig {
        CodexClientConfig {
            binary,
            // Tests use no args so the fake script doesn't need to
            // ignore them.
            args: vec![],
            model: DEFAULT_MODEL.to_owned(),
            timeout: Duration::from_secs(5),
        }
    }

    #[tokio::test]
    async fn happy_path_round_trips_via_fake_cli() {
        let payload = r#"{
            "body":"summary from fake codex",
            "meeting_type":"internal",
            "tags":[]
        }"#;
        let (_d, fake) = fake_cli(payload);
        let (_t, transcript) = write_tmp_jsonl(&[r#"{"text":"hi"}"#]);
        let client = CodexClient::new(cfg_with_binary(fake));
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
        assert_eq!(out.body, "summary from fake codex");
        assert_eq!(out.meeting_type, MeetingType::Internal);
        assert_eq!(out.cost.model, DEFAULT_MODEL);
        assert_eq!(out.cost.summary_usd, 0.0);
    }

    #[tokio::test]
    async fn missing_binary_surfaces_clean_backend_error() {
        let phantom = PathBuf::from("/nonexistent/codex-binary");
        let (_t, transcript) = write_tmp_jsonl(&[r#"{"text":"hi"}"#]);
        let client = CodexClient::new(cfg_with_binary(phantom));
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
        assert!(matches!(err, LlmError::Backend(_)));
    }

    #[test]
    fn default_config_invokes_codex_exec_with_stdin_dash() {
        // Pin the args so a future "let's just call codex" change
        // doesn't silently drop the non-interactive flags.
        let cfg = CodexClientConfig::default();
        assert_eq!(cfg.binary, PathBuf::from(DEFAULT_BINARY));
        assert_eq!(cfg.args[0], "exec");
        assert!(cfg.args.contains(&"--skip-git-repo-check".to_owned()));
        assert!(cfg.args.contains(&"--full-auto".to_owned()));
        assert!(cfg.args.contains(&"-".to_owned()));
    }
}
