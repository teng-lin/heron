//! `heron summarize` core: re-summarize an existing meeting note in
//! place via a caller-supplied [`heron_llm::Summarizer`].
//!
//! The actual ID-preservation logic (§10.5 layers 1 + 2) lives on
//! [`crate::session::Orchestrator::re_summarize_note`]; this module is
//! the thin shell that resolves vault-relative paths, builds the
//! `theirs` frontmatter for the §10.3 merge, and writes the merged
//! output through [`heron_vault::VaultWriter::re_summarize`]. Splitting
//! it out of `main.rs` lets the test suite drive the full flow with a
//! stub summarizer instead of needing a live LLM backend.

use std::path::{Path, PathBuf};

use thiserror::Error;

use heron_llm::{Backend, Summarizer};
use heron_types::Frontmatter;
use heron_vault::{MergeOutcome, VaultWriter, read_note};

use crate::session;

#[derive(Debug, Error)]
pub enum SummarizeError {
    #[error("read note {path}: {source}")]
    ReadNote {
        path: PathBuf,
        #[source]
        source: heron_vault::VaultError,
    },
    #[error(
        "transcript {resolved} not found (frontmatter.transcript = {fm_value}); \
         the note's recorded transcript path no longer resolves on disk"
    )]
    TranscriptMissing {
        resolved: PathBuf,
        fm_value: PathBuf,
    },
    #[error("summarize: {0}")]
    Session(#[from] session::SessionError),
    // No `#[from]` on the merge variant: both this and `ReadNote` carry
    // a `heron_vault::VaultError`, so a blanket `From<VaultError>` would
    // silently route every `?`-propagated vault error to one variant
    // even when the failure is the other kind. We map at each call
    // site instead, with a `path` field on each so the user message
    // names the file that actually broke.
    #[error("re-summarize merge for {path}: {source}")]
    Merge {
        path: PathBuf,
        #[source]
        source: heron_vault::VaultError,
    },
}

/// Map the user-facing `--backend` string to the typed
/// [`heron_llm::Backend`].
///
/// The CLI accepts the same three identifiers the §11.1 selector
/// reports — `anthropic`, `claude-code`, `codex` — so the user can
/// re-run a session against a specific backend regardless of what
/// `Preference::Auto` would have picked at record time.
pub fn parse_backend_flag(s: &str) -> Result<Backend, ParseBackendError> {
    match s {
        "anthropic" => Ok(Backend::Anthropic),
        "claude-code" => Ok(Backend::ClaudeCodeCli),
        "codex" => Ok(Backend::CodexCli),
        other => Err(ParseBackendError(other.to_owned())),
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
#[error("unknown --backend {0:?}; expected one of `anthropic`, `claude-code`, `codex`")]
pub struct ParseBackendError(String);

/// Re-summarize the note at `note_path`, writing the merged output
/// back through [`VaultWriter::re_summarize`]. Returns the merge
/// outcome so the caller can report on what changed.
///
/// Path resolution: the note's `transcript:` frontmatter is stored
/// vault-relative so the vault is portable; this resolves it against
/// `vault_root`. Already-absolute frontmatter values are passed
/// through unchanged so a hand-edited note pointing outside the vault
/// still works.
///
/// Test seam: takes a `&dyn Summarizer` so the integration test can
/// inject a capturing stub without needing `ANTHROPIC_API_KEY` or a
/// `claude` CLI on PATH.
pub async fn re_summarize_in_vault(
    summarizer: &dyn Summarizer,
    vault_root: &Path,
    note_path: &Path,
) -> Result<MergeOutcome, SummarizeError> {
    let (fm, _body) = read_note(note_path).map_err(|source| SummarizeError::ReadNote {
        path: note_path.to_path_buf(),
        source,
    })?;

    // `Path::join` returns the pushed path unchanged when it's
    // absolute, so this single call covers both the vault-relative
    // (default) and hand-edited-absolute (escape hatch) cases.
    let transcript = vault_root.join(&fm.transcript);
    if !transcript.exists() {
        return Err(SummarizeError::TranscriptMissing {
            resolved: transcript,
            fm_value: fm.transcript.clone(),
        });
    }

    // Reuse the orchestrator's re_summarize_note for the §10.5
    // ID-preservation contract (layer 1 prompt-side + layer 2 text
    // matcher). `re_summarize_note` doesn't read any SessionConfig
    // field today, so leaving `cache_dir` / `session_id` empty is
    // accurate — fabricating a placeholder path would invite grep
    // confusion with `cmd_record`'s real cache root.
    let cfg = session::SessionConfig {
        session_id: uuid::Uuid::nil(),
        target_bundle_id: fm.source_app.clone(),
        cache_dir: PathBuf::new(),
        vault_root: vault_root.to_path_buf(),
        stt_backend_name: "sherpa".into(),
        llm_preference: heron_llm::Preference::Auto,
        // CLI re-summarize never carries pre-meeting context — that
        // surface only flows through the daemon's `attach_context`.
        pre_meeting_briefing: None,
        // Re-summarize doesn't run a live capture, so there are no AX
        // events to bridge.
        event_bus: None,
    };
    let orch = session::Orchestrator::new(cfg);
    let output = orch
        .re_summarize_note(summarizer, note_path, fm.meeting_type, &transcript)
        .await?;

    // Build `theirs_frontmatter` for the §10.3 merge: keep
    // heron-managed fields (date / start / duration / source_app /
    // recording / transcript / diarize_source / disclosed / extra)
    // intact from the current note, and overlay the fields the LLM
    // is authoritative for. The merge resolves user edits against
    // the LLM refresh; user-edited fields win, untouched fields
    // refresh.
    let theirs_frontmatter = Frontmatter {
        company: output.company,
        meeting_type: output.meeting_type,
        tags: output.tags,
        action_items: output.action_items,
        attendees: output.attendees,
        cost: output.cost,
        ..fm
    };

    let writer = VaultWriter::new(vault_root);
    let outcome = writer
        .re_summarize(note_path, &theirs_frontmatter, &output.body)
        .map_err(|source| SummarizeError::Merge {
            path: note_path.to_path_buf(),
            source,
        })?;
    Ok(outcome)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_backend_flag_known_values() {
        assert_eq!(
            parse_backend_flag("anthropic").expect("ok"),
            Backend::Anthropic
        );
        assert_eq!(
            parse_backend_flag("claude-code").expect("ok"),
            Backend::ClaudeCodeCli
        );
        assert_eq!(parse_backend_flag("codex").expect("ok"), Backend::CodexCli);
    }

    #[test]
    fn parse_backend_flag_rejects_unknown() {
        let err = parse_backend_flag("gpt-99").expect_err("unknown backend must error");
        // Don't pin the exact message; assert the shape (carries the bad input).
        assert_eq!(err, ParseBackendError("gpt-99".into()));
        let msg = err.to_string();
        assert!(
            msg.contains("gpt-99") && msg.contains("anthropic"),
            "error message must name the bad input and the valid set, got: {msg}"
        );
    }

    /// `parse_backend_flag` is exact-match — no case-folding, no
    /// whitespace trimming, no empty-string special case. Pinned
    /// because clap feeds us the raw flag value, and a future
    /// config-file path that pipes through user-typed strings should
    /// not silently coerce mismatches.
    #[test]
    fn parse_backend_flag_is_exact_match() {
        assert!(parse_backend_flag("Anthropic").is_err());
        assert!(parse_backend_flag(" anthropic").is_err());
        assert!(parse_backend_flag("anthropic ").is_err());
        assert!(parse_backend_flag("").is_err());
    }
}
