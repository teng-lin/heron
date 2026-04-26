//! API-key resolution for the summarizer backends (PR-μ / phase 74).
//!
//! Centralises "where does the Anthropic / OpenAI API key come from?"
//! behind a trait so the heron-cli orchestrator can stay env-only
//! while the desktop crate layers its macOS Keychain reader on top.
//!
//! ## Why a trait
//!
//! The `Anthropic` backend used to call [`std::env::var`] directly. That
//! works fine for the CLI (the user exports `ANTHROPIC_API_KEY`) but
//! left the desktop-shell consumer-side gap from PR-θ #90 open: the
//! Settings UI lets the user paste a key into the macOS Keychain, but
//! `select_summarizer` would still 401 because nothing was reading it
//! back. PR-μ closes that loop without dragging the `tauri` /
//! `security-framework` deps into `heron-llm`:
//!
//! - `heron-llm` defines the trait + an [`EnvKeyResolver`] default.
//! - The desktop crate at `apps/desktop/src-tauri/` provides an
//!   `EnvThenKeychainResolver` that calls `keychain::keychain_get` on
//!   miss, and threads it through to [`select_summarizer_with_resolver`].
//!
//! ## Precedence
//!
//! - **Env var wins for CI / docker / `cargo run` workflows.** A user
//!   who already exports `ANTHROPIC_API_KEY` doesn't need to know the
//!   keychain exists. This also keeps the live-API smoke harness in
//!   `tests/live_api.rs` working unchanged.
//! - **Keychain wins for desktop users** who have only ever pasted
//!   their key into the Settings → Summarizer tab. The
//!   `EnvThenKeychainResolver` (in the desktop crate) falls through to
//!   `keychain::keychain_get` when the env var is absent / empty.
//! - **Both miss → [`KeyResolveError::NotFound`].** Surfaces as
//!   [`crate::LlmError::MissingApiKey`] in the existing call chain so
//!   the renderer renders the same toast it does today
//!   ("set ANTHROPIC_API_KEY", or for desktop, "paste a key in
//!   Settings"). No silent 401s.
//!
//! Mirrors the threat-model decisions in `apps/desktop/src-tauri/src/keychain.rs`:
//! the resolver returns the cleartext secret to the caller; that
//! `String` must not be logged, must not pass through `Display`, and
//! is consumed by the Anthropic client constructor before it's
//! plausibly observable. No biometric gate, no `zeroize` (out of
//! scope for PR-μ; the keychain entry is already unlocked with the
//! user's login session).

use thiserror::Error;

/// Identifies which API key the caller wants resolved. Maps 1:1 to the
/// `KeychainAccount` enum in `apps/desktop/src-tauri/src/keychain.rs`
/// so the desktop-side resolver can do a straight match without a
/// duplicate enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyName {
    /// Used by [`crate::AnthropicClient`]. Env var: `ANTHROPIC_API_KEY`.
    /// Keychain account: `anthropic_api_key` on the desktop side.
    AnthropicApiKey,
    /// Reserved for future OpenAI / Codex hosted backends. Env var:
    /// `OPENAI_API_KEY`. Keychain account: `openai_api_key`. Not yet
    /// consumed by any backend in `heron-llm` — the Codex CLI manages
    /// its own auth — but the resolver surface accepts the variant so
    /// the desktop side's keychain plumbing can pass it through
    /// without being held back by which backends happen to need it
    /// today.
    OpenAiApiKey,
}

impl KeyName {
    /// Wire-format env-var name. Constants live here rather than in the
    /// individual backends so a future rename happens in one place.
    pub const fn env_var(self) -> &'static str {
        match self {
            Self::AnthropicApiKey => "ANTHROPIC_API_KEY",
            Self::OpenAiApiKey => "OPENAI_API_KEY",
        }
    }
}

/// Errors a [`KeyResolver`] implementation can surface.
///
/// `NotFound` is the common case (CI without exported keys, fresh
/// desktop install before the user pastes a key) — callers map it to
/// [`crate::LlmError::MissingApiKey`] so the existing error surface
/// stays unchanged.
#[derive(Debug, Error)]
pub enum KeyResolveError {
    /// Neither the env var nor the keychain (if probed) returned a
    /// value. The variant carries the [`KeyName`] so the renderer can
    /// render a key-specific toast.
    #[error("api key {0:?} not found in env or keychain")]
    NotFound(KeyName),
    /// Backend-specific failure (e.g. macOS keychain returned an error
    /// other than "not found"). The string is the platform's own
    /// description; it never embeds the secret value.
    #[error("backend error: {0}")]
    Backend(String),
}

/// Resolve an API key by name.
///
/// Implementations are expected to be cheap (one env-var read; at most
/// one keychain probe) so the orchestrator can call them every session
/// start without a measurable latency hit. `Send + Sync` so the
/// resolver can be parked behind `Arc<dyn KeyResolver>` and shared
/// across async tasks once we wire one through `Orchestrator::run`.
pub trait KeyResolver: Send + Sync {
    /// Return the cleartext value for `name`, or [`KeyResolveError`]
    /// if it can't be found. Implementations MUST treat the returned
    /// `String` as sensitive — no logging, no `Display` round-trips.
    fn resolve(&self, name: KeyName) -> Result<String, KeyResolveError>;
}

/// Default resolver: read env vars only. Used by the CLI binary and by
/// the historical `from_env` constructors that PR-μ kept for
/// backward-compat.
///
/// Matches the historical [`std::env::var`] read exactly: an empty
/// string is treated as "not set" so a shell that did `export VAR=`
/// without a value doesn't silently submit an empty Authorization
/// header.
#[derive(Debug, Clone, Copy, Default)]
pub struct EnvKeyResolver;

impl EnvKeyResolver {
    /// Construct a fresh resolver. Trivial constructor; provided so
    /// callers can write `Box::new(EnvKeyResolver::new())` without
    /// importing `Default`.
    pub fn new() -> Self {
        Self
    }
}

impl KeyResolver for EnvKeyResolver {
    fn resolve(&self, name: KeyName) -> Result<String, KeyResolveError> {
        std::env::var(name.env_var())
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or(KeyResolveError::NotFound(name))
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    // Single crate-wide ENV_LOCK shared with `anthropic::tests` — two
    // module-private mutexes would race each other on the same env
    // var (`ANTHROPIC_API_KEY` is touched by both modules' tests).
    // See `crate::test_env`.
    use crate::test_env::ENV_LOCK;

    /// Helper: run `body` with `var` temporarily set to `value`,
    /// restoring (or removing) the prior value on exit.
    fn with_env<R>(var: &str, value: Option<&str>, body: impl FnOnce() -> R) -> R {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let saved = std::env::var_os(var);
        // SAFETY: process-global env mutation is unsafe under Rust 2024.
        // ENV_LOCK serializes env-touching tests; restore on exit keeps
        // post-test state matching pre-test state.
        unsafe {
            match value {
                Some(v) => std::env::set_var(var, v),
                None => std::env::remove_var(var),
            }
        }
        let result = body();
        unsafe {
            match saved {
                Some(v) => std::env::set_var(var, v),
                None => std::env::remove_var(var),
            }
        }
        result
    }

    #[test]
    fn env_resolver_returns_value_when_env_set() {
        let resolver = EnvKeyResolver;
        with_env("ANTHROPIC_API_KEY", Some("test-key-aaa"), || {
            let got = resolver
                .resolve(KeyName::AnthropicApiKey)
                .expect("resolved");
            assert_eq!(got, "test-key-aaa");
        });
    }

    #[test]
    fn env_resolver_returns_not_found_when_env_unset() {
        let resolver = EnvKeyResolver;
        with_env("ANTHROPIC_API_KEY", None, || {
            let err = resolver
                .resolve(KeyName::AnthropicApiKey)
                .expect_err("should miss");
            assert!(matches!(
                err,
                KeyResolveError::NotFound(KeyName::AnthropicApiKey)
            ));
        });
    }

    #[test]
    fn env_resolver_treats_empty_string_as_not_found() {
        // Matches the historical `from_env` semantics: a shell that
        // ran `export VAR=` without a value should NOT submit an empty
        // Authorization header — that path 401s with a confusing error.
        let resolver = EnvKeyResolver;
        with_env("ANTHROPIC_API_KEY", Some(""), || {
            let err = resolver
                .resolve(KeyName::AnthropicApiKey)
                .expect_err("should miss");
            assert!(matches!(
                err,
                KeyResolveError::NotFound(KeyName::AnthropicApiKey)
            ));
        });
    }

    #[test]
    fn env_resolver_resolves_openai_key_independently() {
        // Test under a single lock acquisition so we don't deadlock by
        // recursively calling `with_env` (Rust's Mutex isn't reentrant).
        let _guard = ENV_LOCK.lock().expect("env lock");
        let saved_openai = std::env::var_os("OPENAI_API_KEY");
        let saved_anthropic = std::env::var_os("ANTHROPIC_API_KEY");
        // SAFETY: process-global env mutation is unsafe under Rust 2024;
        // ENV_LOCK serializes env-touching tests, restore-on-exit keeps
        // post-test state matching pre-test state.
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "sk-test-openai");
            std::env::remove_var("ANTHROPIC_API_KEY");
        }

        let resolver = EnvKeyResolver;
        let got = resolver.resolve(KeyName::OpenAiApiKey).expect("resolved");
        assert_eq!(got, "sk-test-openai");
        // Anthropic key still missing — the keys are independent.
        let err = resolver
            .resolve(KeyName::AnthropicApiKey)
            .expect_err("should miss");
        assert!(matches!(
            err,
            KeyResolveError::NotFound(KeyName::AnthropicApiKey)
        ));

        // SAFETY: restoring prior values, see above.
        unsafe {
            match saved_openai {
                Some(v) => std::env::set_var("OPENAI_API_KEY", v),
                None => std::env::remove_var("OPENAI_API_KEY"),
            }
            match saved_anthropic {
                Some(v) => std::env::set_var("ANTHROPIC_API_KEY", v),
                None => std::env::remove_var("ANTHROPIC_API_KEY"),
            }
        }
    }

    #[test]
    fn key_name_env_var_is_pinned() {
        // Renaming the env vars is a breaking change for every existing
        // user shell config + every CI runner. Pin the constants here.
        assert_eq!(KeyName::AnthropicApiKey.env_var(), "ANTHROPIC_API_KEY");
        assert_eq!(KeyName::OpenAiApiKey.env_var(), "OPENAI_API_KEY");
    }

    /// A stub resolver that always returns a fixed key. Shape-pins the
    /// trait so a downstream impl (the desktop crate's
    /// `EnvThenKeychainResolver`) can swap in without touching
    /// `select_summarizer_with_resolver`.
    struct StubResolver(&'static str);

    impl KeyResolver for StubResolver {
        fn resolve(&self, _name: KeyName) -> Result<String, KeyResolveError> {
            Ok(self.0.to_owned())
        }
    }

    #[test]
    fn stub_resolver_satisfies_trait_object() {
        // Object-safety check + tiny smoke test that a custom resolver
        // can satisfy the bound. If `KeyResolver` ever stops being
        // object-safe (e.g. a generic method sneaks in), this will
        // refuse to compile.
        let resolver: Box<dyn KeyResolver> = Box::new(StubResolver("canned"));
        let got = resolver.resolve(KeyName::AnthropicApiKey).expect("ok");
        assert_eq!(got, "canned");
    }
}
