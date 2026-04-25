//! Backend selection per `docs/plan.md` §5 weeks 7–8 + §11.1.
//!
//! Three backends are available: the Anthropic API, the Claude Code
//! CLI, and the Codex CLI. The user expresses a [`Preference`]; the
//! selector probes the local environment for a [`Availability`] and
//! returns the first viable [`Backend`] under that preference,
//! along with a [`SelectionReason`] for the diagnostics tab.
//!
//! The selector is pure and synchronous so the orchestrator can call
//! it on every session start without paying for a network probe.
//! Availability is sampled at call time — a user who exports
//! `ANTHROPIC_API_KEY` after launching heron picks up the change on
//! the next selection.

use std::path::PathBuf;

use crate::{Backend, LlmError, Summarizer, build_summarizer};

/// User-expressed preference. Maps to `docs/plan.md` §5 weeks 7–8's
/// "user can pick the cheapest viable option per session".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Preference {
    /// Pick whatever's most capable that the environment offers.
    /// Order: Anthropic API → Claude Code CLI → Codex CLI.
    #[default]
    Auto,
    /// Use only zero-billed-cost CLI backends. Skips the API even
    /// when `ANTHROPIC_API_KEY` is set. Order: Claude Code CLI →
    /// Codex CLI.
    FreeOnly,
    /// Use the API exclusively. Errors if `ANTHROPIC_API_KEY` is
    /// missing rather than falling through — the user explicitly
    /// asked for the premium path.
    PremiumOnly,
}

/// Snapshot of which backends the environment supports right now.
/// Sampled by [`Availability::detect`] each call so a user who
/// installs `claude` mid-session picks up the change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Availability {
    pub has_anthropic_key: bool,
    pub has_claude_cli: bool,
    pub has_codex_cli: bool,
}

impl Availability {
    pub fn detect() -> Self {
        Self {
            has_anthropic_key: std::env::var_os("ANTHROPIC_API_KEY")
                .filter(|v| !v.is_empty())
                .is_some(),
            has_claude_cli: which_on_path("claude").is_some(),
            has_codex_cli: which_on_path("codex").is_some(),
        }
    }

    /// `true` when at least one backend is viable. Lets the caller
    /// short-circuit before constructing the orchestrator.
    pub fn any_available(&self) -> bool {
        self.has_anthropic_key || self.has_claude_cli || self.has_codex_cli
    }
}

/// Why the selector picked the backend it did. Surfaced to the
/// diagnostics tab + tracing span so a `summarize` failure traces
/// back to "we picked CodexCli because the API key was missing
/// AND `claude` wasn't on PATH".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionReason {
    /// The user's first-choice backend under `Preference` was
    /// available and selected.
    PreferredBackendAvailable(Backend),
    /// The user's first-choice backend wasn't available; the
    /// selector fell through to a less-preferred one.
    FellBackTo { chose: Backend, because: String },
}

/// Errors `select_backend` can return.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SelectError {
    #[error(
        "no LLM backend is available: ANTHROPIC_API_KEY unset, \
         and neither `claude` nor `codex` is on PATH"
    )]
    NoBackendAvailable,
    #[error(
        "preference=PremiumOnly but ANTHROPIC_API_KEY is unset; \
         export the key or switch to Preference::Auto / FreeOnly"
    )]
    PremiumOnlyMissingApiKey,
}

/// Pick the first viable backend matching `pref`, given a snapshot
/// of `avail`. Pure / synchronous so the orchestrator can call it on
/// every session start.
pub fn select_backend(
    pref: Preference,
    avail: &Availability,
) -> Result<(Backend, SelectionReason), SelectError> {
    match pref {
        Preference::Auto => {
            if avail.has_anthropic_key {
                return Ok((
                    Backend::Anthropic,
                    SelectionReason::PreferredBackendAvailable(Backend::Anthropic),
                ));
            }
            if avail.has_claude_cli {
                return Ok((
                    Backend::ClaudeCodeCli,
                    SelectionReason::FellBackTo {
                        chose: Backend::ClaudeCodeCli,
                        because: "ANTHROPIC_API_KEY unset; using Claude Code CLI".to_owned(),
                    },
                ));
            }
            if avail.has_codex_cli {
                return Ok((
                    Backend::CodexCli,
                    SelectionReason::FellBackTo {
                        chose: Backend::CodexCli,
                        because:
                            "ANTHROPIC_API_KEY unset and `claude` not on PATH; using Codex CLI"
                                .to_owned(),
                    },
                ));
            }
            Err(SelectError::NoBackendAvailable)
        }
        Preference::FreeOnly => {
            if avail.has_claude_cli {
                return Ok((
                    Backend::ClaudeCodeCli,
                    SelectionReason::PreferredBackendAvailable(Backend::ClaudeCodeCli),
                ));
            }
            if avail.has_codex_cli {
                return Ok((
                    Backend::CodexCli,
                    SelectionReason::FellBackTo {
                        chose: Backend::CodexCli,
                        because: "FreeOnly preference: `claude` not on PATH; using Codex CLI"
                            .to_owned(),
                    },
                ));
            }
            Err(SelectError::NoBackendAvailable)
        }
        Preference::PremiumOnly => {
            if avail.has_anthropic_key {
                Ok((
                    Backend::Anthropic,
                    SelectionReason::PreferredBackendAvailable(Backend::Anthropic),
                ))
            } else {
                Err(SelectError::PremiumOnlyMissingApiKey)
            }
        }
    }
}

/// One-stop helper: detect availability, select a backend, build the
/// Summarizer, return both. Useful for the orchestrator's "give me
/// something to call" path.
pub fn select_summarizer(
    pref: Preference,
) -> Result<(Box<dyn Summarizer>, Backend, SelectionReason), LlmError> {
    let avail = Availability::detect();
    let (backend, reason) = select_backend(pref, &avail).map_err(|e| match e {
        SelectError::NoBackendAvailable => LlmError::Backend(e.to_string()),
        SelectError::PremiumOnlyMissingApiKey => LlmError::MissingApiKey,
    })?;
    Ok((build_summarizer(backend), backend, reason))
}

/// Tiny PATH-walker shared with `heron-cli`'s status preflight. Has
/// the same executable-bit check on unix so a non-executable file
/// at `PATH/binary` doesn't get reported as available.
fn which_on_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let candidate = dir.join(name);
        let Ok(meta) = std::fs::metadata(&candidate) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o111 == 0 {
                continue;
            }
        }
        return Some(candidate);
    }
    None
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn avail(api: bool, claude: bool, codex: bool) -> Availability {
        Availability {
            has_anthropic_key: api,
            has_claude_cli: claude,
            has_codex_cli: codex,
        }
    }

    #[test]
    fn auto_prefers_anthropic_when_key_present() {
        let (backend, reason) =
            select_backend(Preference::Auto, &avail(true, true, true)).expect("select");
        assert_eq!(backend, Backend::Anthropic);
        assert_eq!(
            reason,
            SelectionReason::PreferredBackendAvailable(Backend::Anthropic)
        );
    }

    #[test]
    fn auto_falls_back_to_claude_cli_when_key_missing() {
        let (backend, reason) =
            select_backend(Preference::Auto, &avail(false, true, true)).expect("select");
        assert_eq!(backend, Backend::ClaudeCodeCli);
        assert!(matches!(reason, SelectionReason::FellBackTo { .. }));
    }

    #[test]
    fn auto_falls_back_to_codex_when_only_codex_available() {
        let (backend, reason) =
            select_backend(Preference::Auto, &avail(false, false, true)).expect("select");
        assert_eq!(backend, Backend::CodexCli);
        if let SelectionReason::FellBackTo { because, .. } = reason {
            assert!(because.contains("Codex"));
            assert!(because.contains("ANTHROPIC_API_KEY"));
            assert!(because.contains("claude"));
        } else {
            panic!("expected FellBackTo");
        }
    }

    #[test]
    fn auto_errors_when_no_backend_available() {
        let err = select_backend(Preference::Auto, &avail(false, false, false)).expect_err("none");
        assert_eq!(err, SelectError::NoBackendAvailable);
    }

    #[test]
    fn free_only_skips_anthropic_even_with_key() {
        let (backend, reason) =
            select_backend(Preference::FreeOnly, &avail(true, true, true)).expect("select");
        assert_eq!(backend, Backend::ClaudeCodeCli);
        assert_eq!(
            reason,
            SelectionReason::PreferredBackendAvailable(Backend::ClaudeCodeCli)
        );
    }

    #[test]
    fn free_only_falls_through_to_codex_when_no_claude() {
        let (backend, _) =
            select_backend(Preference::FreeOnly, &avail(true, false, true)).expect("select");
        assert_eq!(backend, Backend::CodexCli);
    }

    #[test]
    fn free_only_errors_when_no_cli_available() {
        let err =
            select_backend(Preference::FreeOnly, &avail(true, false, false)).expect_err("none");
        assert_eq!(err, SelectError::NoBackendAvailable);
    }

    #[test]
    fn premium_only_uses_anthropic_when_key_present() {
        let (backend, reason) =
            select_backend(Preference::PremiumOnly, &avail(true, false, false)).expect("select");
        assert_eq!(backend, Backend::Anthropic);
        assert_eq!(
            reason,
            SelectionReason::PreferredBackendAvailable(Backend::Anthropic)
        );
    }

    #[test]
    fn premium_only_errors_when_key_missing_does_not_fall_through() {
        // A user who explicitly picked PremiumOnly wants the API or
        // nothing — DON'T silently fall through to a CLI.
        let err = select_backend(Preference::PremiumOnly, &avail(false, true, true))
            .expect_err("missing key");
        assert_eq!(err, SelectError::PremiumOnlyMissingApiKey);
    }

    #[test]
    fn availability_any_available_reports_correctly() {
        assert!(avail(true, false, false).any_available());
        assert!(avail(false, true, false).any_available());
        assert!(avail(false, false, true).any_available());
        assert!(!avail(false, false, false).any_available());
    }

    #[test]
    fn preference_default_is_auto() {
        assert_eq!(Preference::default(), Preference::Auto);
    }

    #[test]
    fn which_on_path_finds_existing_executable() {
        // `sh` is on PATH on every unix CI runner.
        #[cfg(unix)]
        {
            assert!(which_on_path("sh").is_some());
        }
    }

    #[test]
    fn which_on_path_returns_none_for_phantom_binary() {
        assert!(which_on_path("definitely-not-a-real-binary-name-xyz123").is_none());
    }
}
