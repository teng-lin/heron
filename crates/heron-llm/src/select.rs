//! Backend selection per `docs/archives/plan.md` §5 weeks 7–8 + §11.1.
//!
//! Four backends are available: the Anthropic API, the OpenAI API,
//! the Claude Code CLI, and the Codex CLI. The user expresses a
//! [`Preference`]; the selector probes the local environment for an
//! [`Availability`] and returns the first viable [`Backend`] under
//! that preference, along with a [`SelectionReason`] for the
//! diagnostics tab.
//!
//! The selector is pure and synchronous so the orchestrator can call
//! it on every session start without paying for a network probe.
//! Availability is sampled at call time — a user who exports
//! `ANTHROPIC_API_KEY` or `OPENAI_API_KEY` after launching heron picks
//! up the change on the next selection.

use std::path::PathBuf;

use crate::key_resolver::{EnvKeyResolver, KeyName, KeyResolveError, KeyResolver};
use crate::{Backend, LlmError, Summarizer, build_summarizer_with_resolver};

/// User-expressed preference. Maps to `docs/archives/plan.md` §5 weeks 7–8's
/// "user can pick the cheapest viable option per session".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Preference {
    /// Pick whatever's most capable that the environment offers.
    /// Order: Anthropic API → OpenAI API → Claude Code CLI → Codex CLI.
    /// Anthropic remains first because that's the existing default; OpenAI
    /// slots above CLI fallbacks because it's a hosted backend the user
    /// explicitly configured a key for.
    #[default]
    Auto,
    /// Use only zero-billed-cost CLI backends. Skips API backends even
    /// when keys are set. Order: Claude Code CLI → Codex CLI.
    FreeOnly,
    /// Use a hosted API exclusively. Prefers Anthropic; falls through to
    /// OpenAI when only an OpenAI key is present. Errors if neither API
    /// key is configured rather than falling through to a CLI backend —
    /// the user explicitly asked for the premium path.
    PremiumOnly,
}

/// Snapshot of which backends the environment supports right now.
/// Sampled by [`Availability::detect`] each call so a user who
/// installs `claude` mid-session picks up the change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Availability {
    pub has_anthropic_key: bool,
    pub has_openai_key: bool,
    pub has_claude_cli: bool,
    pub has_codex_cli: bool,
}

impl Availability {
    /// Detect availability using the env-only resolver. Equivalent to
    /// `detect_with_resolver(&EnvKeyResolver)` and kept for callers
    /// that only need the historical CLI behaviour (the heron-cli
    /// status preflight, the test suite).
    pub fn detect() -> Self {
        Self::detect_with_resolver(&EnvKeyResolver)
    }

    /// Detect availability using a caller-supplied
    /// [`KeyResolver`]. PR-μ / phase 74: the desktop crate's
    /// `EnvThenKeychainResolver` flips `has_anthropic_key` on when the
    /// user pasted their key into Settings → Summarizer (PR-θ) but
    /// hasn't exported the env var. The CLI path keeps using
    /// [`Self::detect`] so its behaviour is unchanged.
    ///
    /// **Lossy on `Backend` errors by design.** This API returns a
    /// flat `Self`, so a `KeyResolveError::Backend` from the resolver
    /// is logged + treated as "key unavailable" rather than crashing
    /// the probe. Callers that need to distinguish a real keychain
    /// failure from a missing key MUST use
    /// [`select_summarizer_with_resolver`], which propagates Backend
    /// errors as [`LlmError::Backend`]. This split keeps the
    /// diagnostics-tab "is Anthropic configured?" probe cheap while
    /// the orchestrator's selection path still surfaces actionable
    /// errors.
    pub fn detect_with_resolver(resolver: &dyn KeyResolver) -> Self {
        let has_anthropic_key = match resolver.resolve(KeyName::AnthropicApiKey) {
            Ok(_) => true,
            // NotFound is the steady-state "key not configured" case.
            Err(KeyResolveError::NotFound(_)) => false,
            // Backend errors shouldn't crash the selector — log + treat
            // as "not available" so selection falls through to a CLI.
            // The actual error surfaces at summarize-time via
            // `from_resolver` if Anthropic ends up being chosen.
            Err(KeyResolveError::Backend(msg)) => {
                tracing::warn!("key resolver backend error during availability probe: {msg}");
                false
            }
        };
        let has_openai_key = match resolver.resolve(KeyName::OpenAiApiKey) {
            Ok(_) => true,
            Err(KeyResolveError::NotFound(_)) => false,
            Err(KeyResolveError::Backend(msg)) => {
                tracing::warn!(
                    "key resolver backend error during OpenAI availability probe: {msg}"
                );
                false
            }
        };
        Self {
            has_anthropic_key,
            has_openai_key,
            has_claude_cli: which_on_path("claude").is_some(),
            has_codex_cli: which_on_path("codex").is_some(),
        }
    }

    /// `true` when at least one backend is viable. Lets the caller
    /// short-circuit before constructing the orchestrator.
    pub fn any_available(&self) -> bool {
        self.has_anthropic_key || self.has_openai_key || self.has_claude_cli || self.has_codex_cli
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
        "no LLM backend is available: ANTHROPIC_API_KEY and OPENAI_API_KEY unset, \
         and neither `claude` nor `codex` is on PATH"
    )]
    NoBackendAvailable,
    #[error(
        "preference=PremiumOnly but neither ANTHROPIC_API_KEY nor OPENAI_API_KEY is set; \
         export a key or switch to Preference::Auto / FreeOnly"
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
            // Order: Anthropic API → OpenAI API → Claude Code CLI → Codex CLI.
            // Anthropic remains first (existing default); OpenAI slots above
            // CLI fallbacks because it's a hosted backend the user explicitly
            // configured a key for.
            if avail.has_anthropic_key {
                return Ok((
                    Backend::Anthropic,
                    SelectionReason::PreferredBackendAvailable(Backend::Anthropic),
                ));
            }
            if avail.has_openai_key {
                return Ok((
                    Backend::OpenAI,
                    SelectionReason::FellBackTo {
                        chose: Backend::OpenAI,
                        because: "ANTHROPIC_API_KEY unset; using OpenAI API".to_owned(),
                    },
                ));
            }
            if avail.has_claude_cli {
                return Ok((
                    Backend::ClaudeCodeCli,
                    SelectionReason::FellBackTo {
                        chose: Backend::ClaudeCodeCli,
                        because: "ANTHROPIC_API_KEY and OPENAI_API_KEY unset; \
                                  using Claude Code CLI"
                            .to_owned(),
                    },
                ));
            }
            if avail.has_codex_cli {
                return Ok((
                    Backend::CodexCli,
                    SelectionReason::FellBackTo {
                        chose: Backend::CodexCli,
                        because: "ANTHROPIC_API_KEY and OPENAI_API_KEY unset, \
                                  `claude` not on PATH; using Codex CLI"
                            .to_owned(),
                    },
                ));
            }
            Err(SelectError::NoBackendAvailable)
        }
        Preference::FreeOnly => {
            // FreeOnly explicitly opts out of all billed API backends —
            // skip OpenAI even when a key is present.
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
            // PremiumOnly means "use a hosted API or error" — Anthropic
            // first, fall through to OpenAI if only that key is present.
            if avail.has_anthropic_key {
                return Ok((
                    Backend::Anthropic,
                    SelectionReason::PreferredBackendAvailable(Backend::Anthropic),
                ));
            }
            if avail.has_openai_key {
                return Ok((
                    Backend::OpenAI,
                    SelectionReason::FellBackTo {
                        chose: Backend::OpenAI,
                        because: "PremiumOnly preference: ANTHROPIC_API_KEY unset; \
                                  using OpenAI API"
                            .to_owned(),
                    },
                ));
            }
            Err(SelectError::PremiumOnlyMissingApiKey)
        }
    }
}

/// One-stop helper: detect availability, select a backend, build the
/// Summarizer, return all three. Equivalent to calling
/// [`select_summarizer_with_resolver`] with an [`EnvKeyResolver`] —
/// kept for callers (heron-cli, the test suite) that only need the
/// historical env-var behaviour.
pub fn select_summarizer(
    pref: Preference,
) -> Result<(Box<dyn Summarizer>, Backend, SelectionReason), LlmError> {
    select_summarizer_with_resolver(pref, &EnvKeyResolver)
}

/// Resolver-aware variant: detect availability, pick a backend, build
/// the Summarizer, all driven through `resolver`.
///
/// PR-μ / phase 74 hook for the desktop crate. With an
/// `EnvThenKeychainResolver` plugged in, a user who only ever pasted
/// their key into Settings → Summarizer (PR-θ) gets
/// `Backend::Anthropic` selected automatically, with the same fallback
/// chain to the CLI backends as the env-only path. The CLI binary
/// keeps using [`select_summarizer`] so its behaviour is unchanged.
///
/// **Resolver Backend errors are propagated, not masked.** A real
/// keychain failure (corrupted UTF-8, Security framework returning
/// non-`errSecItemNotFound`) surfaces as [`LlmError::Backend`] so the
/// Review-UI toast can distinguish "macOS keychain returned an error"
/// from "no key configured" / "fall back to CLI". This is the
/// contract `apps/desktop/src-tauri/src/keychain_resolver.rs`
/// documents under "Precedence #2". `Availability::detect_with_resolver`
/// (used directly by callers that want a cheap probe — the
/// diagnostics tab) keeps the lossy "treat Backend as unavailable"
/// behaviour because there's no `Result` channel to surface the
/// error through.
pub fn select_summarizer_with_resolver(
    pref: Preference,
    resolver: &dyn KeyResolver,
) -> Result<(Box<dyn Summarizer>, Backend, SelectionReason), LlmError> {
    // Skip the resolver probe entirely under `FreeOnly` — that
    // preference says "use a CLI backend, never the API". A broken
    // keychain shouldn't block the user from selecting `claude` /
    // `codex` when they've explicitly opted out of API backends.
    let (has_anthropic_key, has_openai_key) = match pref {
        Preference::FreeOnly => (false, false),
        Preference::Auto | Preference::PremiumOnly => {
            // Probe explicitly so a `Backend` error surfaces as
            // `LlmError::Backend(...)` instead of being swallowed by
            // `detect_with_resolver` and masquerading as either
            // `NoBackendAvailable` (Auto fall-through) or
            // `PremiumOnlyMissingApiKey` (PremiumOnly). This is the
            // path the desktop crate's `EnvThenKeychainResolver`
            // relies on so a corrupted keychain entry produces an
            // actionable renderer toast distinct from "paste a key
            // in Settings".
            let has_anthropic = match resolver.resolve(KeyName::AnthropicApiKey) {
                Ok(_) => true,
                Err(KeyResolveError::NotFound(_)) => false,
                Err(KeyResolveError::Backend(msg)) => {
                    return Err(LlmError::Backend(format!("api key resolver: {msg}")));
                }
            };
            let has_openai = match resolver.resolve(KeyName::OpenAiApiKey) {
                Ok(_) => true,
                Err(KeyResolveError::NotFound(_)) => false,
                Err(KeyResolveError::Backend(msg)) => {
                    return Err(LlmError::Backend(format!("api key resolver: {msg}")));
                }
            };
            (has_anthropic, has_openai)
        }
    };
    let avail = Availability {
        has_anthropic_key,
        has_openai_key,
        has_claude_cli: which_on_path("claude").is_some(),
        has_codex_cli: which_on_path("codex").is_some(),
    };
    let (backend, reason) = select_backend(pref, &avail).map_err(|e| match e {
        SelectError::NoBackendAvailable => LlmError::Backend(e.to_string()),
        SelectError::PremiumOnlyMissingApiKey => LlmError::MissingApiKey,
    })?;
    Ok((
        build_summarizer_with_resolver(backend, resolver),
        backend,
        reason,
    ))
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

    /// Helper: construct an `Availability` snapshot.
    /// `(anthropic_key, openai_key, claude_cli, codex_cli)`.
    fn avail(anthropic: bool, openai: bool, claude: bool, codex: bool) -> Availability {
        Availability {
            has_anthropic_key: anthropic,
            has_openai_key: openai,
            has_claude_cli: claude,
            has_codex_cli: codex,
        }
    }

    #[test]
    fn auto_prefers_anthropic_when_key_present() {
        let (backend, reason) =
            select_backend(Preference::Auto, &avail(true, true, true, true)).expect("select");
        assert_eq!(backend, Backend::Anthropic);
        assert_eq!(
            reason,
            SelectionReason::PreferredBackendAvailable(Backend::Anthropic)
        );
    }

    #[test]
    fn auto_falls_back_to_openai_when_only_openai_available() {
        // Anthropic key absent; OpenAI key present; no CLIs.
        let (backend, reason) =
            select_backend(Preference::Auto, &avail(false, true, false, false)).expect("select");
        assert_eq!(backend, Backend::OpenAI);
        if let SelectionReason::FellBackTo { because, .. } = reason {
            assert!(
                because.contains("ANTHROPIC_API_KEY"),
                "missing key name: {because}"
            );
            assert!(
                because.contains("OpenAI"),
                "missing backend name: {because}"
            );
        } else {
            panic!("expected FellBackTo");
        }
    }

    #[test]
    fn auto_falls_back_to_claude_cli_when_api_keys_missing() {
        let (backend, reason) =
            select_backend(Preference::Auto, &avail(false, false, true, true)).expect("select");
        assert_eq!(backend, Backend::ClaudeCodeCli);
        assert!(matches!(reason, SelectionReason::FellBackTo { .. }));
    }

    #[test]
    fn auto_falls_back_to_codex_when_only_codex_available() {
        let (backend, reason) =
            select_backend(Preference::Auto, &avail(false, false, false, true)).expect("select");
        assert_eq!(backend, Backend::CodexCli);
        if let SelectionReason::FellBackTo { because, .. } = reason {
            assert!(because.contains("Codex"));
            assert!(because.contains("ANTHROPIC_API_KEY"));
        } else {
            panic!("expected FellBackTo");
        }
    }

    #[test]
    fn auto_errors_when_no_backend_available() {
        let err =
            select_backend(Preference::Auto, &avail(false, false, false, false)).expect_err("none");
        assert_eq!(err, SelectError::NoBackendAvailable);
    }

    #[test]
    fn free_only_skips_anthropic_even_with_key() {
        let (backend, reason) =
            select_backend(Preference::FreeOnly, &avail(true, true, true, true)).expect("select");
        assert_eq!(backend, Backend::ClaudeCodeCli);
        assert_eq!(
            reason,
            SelectionReason::PreferredBackendAvailable(Backend::ClaudeCodeCli)
        );
    }

    #[test]
    fn free_only_skips_openai_even_with_key() {
        // FreeOnly must skip OpenAI even when the key is present.
        let (backend, _) =
            select_backend(Preference::FreeOnly, &avail(false, true, true, false)).expect("select");
        assert_eq!(backend, Backend::ClaudeCodeCli);
    }

    #[test]
    fn free_only_falls_through_to_codex_when_no_claude() {
        let (backend, _) =
            select_backend(Preference::FreeOnly, &avail(true, false, false, true)).expect("select");
        assert_eq!(backend, Backend::CodexCli);
    }

    #[test]
    fn free_only_errors_when_no_cli_available() {
        let err = select_backend(Preference::FreeOnly, &avail(true, true, false, false))
            .expect_err("none");
        assert_eq!(err, SelectError::NoBackendAvailable);
    }

    #[test]
    fn premium_only_uses_anthropic_when_key_present() {
        let (backend, reason) =
            select_backend(Preference::PremiumOnly, &avail(true, false, false, false))
                .expect("select");
        assert_eq!(backend, Backend::Anthropic);
        assert_eq!(
            reason,
            SelectionReason::PreferredBackendAvailable(Backend::Anthropic)
        );
    }

    #[test]
    fn premium_only_uses_openai_when_only_openai_key() {
        // PremiumOnly with only an OpenAI key must use OpenAI, not error.
        let (backend, reason) =
            select_backend(Preference::PremiumOnly, &avail(false, true, false, false))
                .expect("select");
        assert_eq!(backend, Backend::OpenAI);
        if let SelectionReason::FellBackTo { because, .. } = reason {
            assert!(
                because.contains("ANTHROPIC_API_KEY"),
                "missing key name: {because}"
            );
            assert!(
                because.contains("OpenAI"),
                "missing backend name: {because}"
            );
        } else {
            panic!("expected FellBackTo");
        }
    }

    #[test]
    fn premium_only_errors_when_no_api_key_does_not_fall_through_to_cli() {
        // A user who explicitly picked PremiumOnly wants an API backend or
        // nothing — DON'T silently fall through to a CLI.
        let err = select_backend(Preference::PremiumOnly, &avail(false, false, true, true))
            .expect_err("missing key");
        assert_eq!(err, SelectError::PremiumOnlyMissingApiKey);
    }

    #[test]
    fn availability_any_available_reports_correctly() {
        assert!(avail(true, false, false, false).any_available());
        assert!(avail(false, true, false, false).any_available());
        assert!(avail(false, false, true, false).any_available());
        assert!(avail(false, false, false, true).any_available());
        assert!(!avail(false, false, false, false).any_available());
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

    /// Stub resolver: hand back a fixed Ok(_) so `detect_with_resolver`
    /// flips `has_anthropic_key` on without touching the env. Anchors
    /// the desktop crate's `EnvThenKeychainResolver` integration: a
    /// user who only pasted their key into the keychain still gets
    /// the API path picked.
    struct StubResolverYes;
    impl KeyResolver for StubResolverYes {
        fn resolve(&self, _name: KeyName) -> Result<String, KeyResolveError> {
            Ok("test-key".to_owned())
        }
    }

    /// Stub resolver: NotFound for every probe. Used to assert
    /// `detect_with_resolver` flips `has_anthropic_key` off without
    /// reading the env var.
    struct StubResolverNo;
    impl KeyResolver for StubResolverNo {
        fn resolve(&self, name: KeyName) -> Result<String, KeyResolveError> {
            Err(KeyResolveError::NotFound(name))
        }
    }

    #[test]
    fn detect_with_resolver_yes_flips_has_anthropic_key_on() {
        let avail = Availability::detect_with_resolver(&StubResolverYes);
        assert!(
            avail.has_anthropic_key,
            "stub resolver returned Ok; has_anthropic_key must be true"
        );
        assert!(
            avail.has_openai_key,
            "stub resolver returned Ok for all keys; has_openai_key must be true"
        );
    }

    #[test]
    fn detect_with_resolver_no_flips_has_anthropic_key_off() {
        let avail = Availability::detect_with_resolver(&StubResolverNo);
        assert!(
            !avail.has_anthropic_key,
            "stub resolver returned NotFound; has_anthropic_key must be false"
        );
        assert!(
            !avail.has_openai_key,
            "stub resolver returned NotFound; has_openai_key must be false"
        );
    }

    /// Round-trip through the resolver-aware selector: a stub resolver
    /// returning Ok must drive `Preference::Auto` to pick Anthropic
    /// regardless of what the env var contains. End-to-end shape of
    /// the desktop-side keychain integration.
    #[test]
    fn select_summarizer_with_resolver_picks_anthropic_when_resolver_yields_key() {
        let (_summarizer, backend, _reason) =
            select_summarizer_with_resolver(Preference::Auto, &StubResolverYes).expect("select");
        assert_eq!(backend, Backend::Anthropic);
    }

    /// Stub resolver that always returns a `Backend` error. Used to
    /// pin the contract that real keychain failures get propagated by
    /// [`select_summarizer_with_resolver`] rather than being silently
    /// downgraded to "no key configured".
    struct StubResolverErr;
    impl KeyResolver for StubResolverErr {
        fn resolve(&self, _name: KeyName) -> Result<String, KeyResolveError> {
            Err(KeyResolveError::Backend(
                "simulated keychain failure".into(),
            ))
        }
    }

    /// `detect_with_resolver` is a flat `Self`-returning probe; it
    /// has no `Result` channel to surface a `Backend` error through.
    /// We accept the lossy "treat as unavailable" behaviour for THAT
    /// API alone — diagnostics-tab callers only need a bool. The
    /// `select_summarizer_with_resolver` path below pins the
    /// orchestrator-side contract that Backend errors DO propagate.
    #[test]
    fn detect_with_resolver_lossily_treats_backend_error_as_unavailable() {
        let avail = Availability::detect_with_resolver(&StubResolverErr);
        assert!(
            !avail.has_anthropic_key,
            "detect_with_resolver is a flat probe; Backend errors must collapse to has_anthropic_key=false"
        );
    }

    /// Per the `keychain_resolver.rs` contract: a real keychain
    /// failure (corrupted UTF-8, Security-framework non-not-found
    /// error) MUST surface as [`LlmError::Backend`] from
    /// `select_summarizer_with_resolver` so the renderer can render
    /// a distinct "macOS keychain returned an error" toast — not
    /// fall through to a CLI backend or report `MissingApiKey`.
    /// PR-μ review (codex) caught the regression where Backend
    /// errors were being swallowed; this test guards the fix.
    #[test]
    fn select_summarizer_with_resolver_propagates_backend_error_as_llm_backend() {
        // `Box<dyn Summarizer>` doesn't implement `Debug` so we can't
        // use `expect_err`; pattern-match on the result instead.
        match select_summarizer_with_resolver(Preference::Auto, &StubResolverErr) {
            Ok(_) => panic!(
                "Backend error must propagate; silent fall-through to a CLI \
                 backend would mask a corrupted keychain entry from the user",
            ),
            Err(LlmError::Backend(msg)) => {
                assert!(
                    msg.contains("simulated keychain failure"),
                    "expected resolver error to be wrapped, got: {msg}"
                );
                assert!(
                    msg.contains("api key resolver"),
                    "expected resolver-prefixed error, got: {msg}"
                );
            }
            Err(other) => panic!(
                "Backend errors must surface as LlmError::Backend, not {other:?}; \
                 silent fall-through to CLI or MissingApiKey would mask a corrupted \
                 keychain entry from the user"
            ),
        }
    }

    /// FreeOnly + resolver Backend error: the keychain probe must be
    /// SKIPPED. The user opted out of Anthropic; a broken keychain
    /// entry has nothing to do with whether `claude` / `codex` is on
    /// PATH, so it must not block selection of a CLI backend. The
    /// outcome depends on which CLIs are on PATH at test time
    /// (different in CI vs a dev box) — what we pin is "the resolver
    /// error did NOT propagate" by asserting the result is anything
    /// other than `LlmError::Backend(api key resolver: ...)`.
    #[test]
    fn select_summarizer_with_resolver_skips_resolver_probe_under_free_only() {
        match select_summarizer_with_resolver(Preference::FreeOnly, &StubResolverErr) {
            // CLI on PATH: success. Resolver was never asked. ✓
            Ok((_, backend, _)) => assert_ne!(
                backend,
                Backend::Anthropic,
                "FreeOnly must never pick Anthropic"
            ),
            // No CLI on PATH: NoBackendAvailable, NOT a resolver-prefixed error.
            Err(LlmError::Backend(msg)) => {
                assert!(
                    !msg.contains("api key resolver"),
                    "FreeOnly must skip the resolver probe entirely; got {msg}"
                );
            }
            Err(other) => {
                panic!("unexpected error variant under FreeOnly with broken resolver: {other:?}")
            }
        }
    }

    /// PremiumOnly + resolver Backend error: must surface as Backend,
    /// not as `MissingApiKey`. Without the propagation, a corrupted
    /// keychain entry under PremiumOnly would tell the user "export
    /// the key" — a confusing toast when the key IS configured but
    /// the keychain itself is broken.
    #[test]
    fn select_summarizer_with_resolver_propagates_backend_error_under_premium_only() {
        match select_summarizer_with_resolver(Preference::PremiumOnly, &StubResolverErr) {
            Ok(_) => panic!("Backend error must propagate even under PremiumOnly"),
            Err(LlmError::Backend(_)) => {}
            Err(other) => panic!(
                "PremiumOnly + Backend error must surface as Backend, not {other:?}; \
                 a 'export the key' toast is misleading when the key IS configured \
                 but the keychain itself is broken"
            ),
        }
    }

    /// A resolver that returns Ok for OpenAI but NotFound for Anthropic.
    /// Anchors the Auto fallback path: when only OpenAI key is present,
    /// `select_summarizer_with_resolver` must pick `Backend::OpenAI`.
    struct StubResolverOpenAIOnly;
    impl KeyResolver for StubResolverOpenAIOnly {
        fn resolve(&self, name: KeyName) -> Result<String, KeyResolveError> {
            match name {
                KeyName::OpenAiApiKey => Ok("sk-test-openai".to_owned()),
                _ => Err(KeyResolveError::NotFound(name)),
            }
        }
    }

    #[test]
    fn select_summarizer_with_resolver_picks_openai_when_only_openai_key() {
        let (_summarizer, backend, _reason) =
            select_summarizer_with_resolver(Preference::Auto, &StubResolverOpenAIOnly)
                .expect("select");
        assert_eq!(backend, Backend::OpenAI);
    }

    #[test]
    fn select_summarizer_with_resolver_premium_only_picks_openai_when_only_openai_key() {
        let (_summarizer, backend, _reason) =
            select_summarizer_with_resolver(Preference::PremiumOnly, &StubResolverOpenAIOnly)
                .expect("select");
        assert_eq!(backend, Backend::OpenAI);
    }

    #[test]
    fn free_only_skips_openai_even_with_key_via_resolver() {
        // FreeOnly + only OpenAI key available: result depends on CLIs.
        // What we pin is "OpenAI must never be picked under FreeOnly".
        match select_summarizer_with_resolver(Preference::FreeOnly, &StubResolverOpenAIOnly) {
            Ok((_, backend, _)) => {
                assert_ne!(backend, Backend::OpenAI, "FreeOnly must never pick OpenAI")
            }
            // No CLI on PATH: NoBackendAvailable is expected and fine.
            Err(LlmError::Backend(_)) => {}
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
}
