//! Desktop-side [`heron_llm::KeyResolver`] that layers the macOS
//! Keychain under the env-var read (PR-μ / phase 74).
//!
//! Closes the consumer-side gap from PR-θ (#90): the Settings UI's
//! Summarizer tab can store API keys in the login keychain, but until
//! now `select_summarizer` only inspected `ANTHROPIC_API_KEY`. This
//! resolver sits between the orchestrator and the keychain reader so
//! a user who only ever pasted their key into Settings can summarize
//! without exporting an env var.
//!
//! ## Precedence
//!
//! 1. Env var (`ANTHROPIC_API_KEY` / `OPENAI_API_KEY`) wins. CI runs,
//!    docker workflows, and the live-API smoke harness in
//!    `crates/heron-llm/tests/live_api.rs` all rely on this — they
//!    want the test/CI key to take precedence over whatever might
//!    happen to be in the keychain on a developer's laptop.
//! 2. Keychain fallback. On macOS, [`crate::keychain::keychain_get`]
//!    is consulted with the matching [`KeychainAccount`]. A
//!    `KeychainError::Backend` surfaces as
//!    [`heron_llm::KeyResolveError::Backend`] so the renderer can
//!    distinguish "neither configured" from "macOS keychain returned
//!    an error".
//! 3. Both miss → [`heron_llm::KeyResolveError::NotFound`], which the
//!    Anthropic constructor maps to [`heron_llm::LlmError::MissingApiKey`].
//!    The existing renderer toast fires unchanged.
//!
//! ## Non-macOS targets
//!
//! On Linux / Windows the keychain shim returns `KeychainError::Unsupported`.
//! `EnvThenKeychainResolver` swallows that variant and reports
//! `NotFound` instead — a missing keychain on a non-macOS dev box
//! shouldn't be surfaced to the renderer as an error when the user
//! never opted into the keychain path. The behaviour collapses to
//! "env-var-only", matching what the CLI does.
//!
//! See `keychain.rs` for the on-disk format + threat model.

use heron_llm::{EnvKeyResolver, KeyName, KeyResolveError, KeyResolver};

use crate::keychain::{KeychainAccount, KeychainError, keychain_get};

/// Map a [`KeyName`] to its matching [`KeychainAccount`]. Defined as a
/// free function rather than an `impl From` so the mapping can be
/// reviewed in one place — the two enums live in different crates and
/// adding a variant in either MUST drop into a non-exhaustive match
/// here so this stays in sync.
fn key_name_to_account(name: KeyName) -> KeychainAccount {
    match name {
        KeyName::AnthropicApiKey => KeychainAccount::AnthropicApiKey,
        KeyName::OpenAiApiKey => KeychainAccount::OpenAiApiKey,
    }
}

/// Resolver that prefers the env var and falls back to the macOS
/// login keychain.
///
/// Cheap to construct (zero state); the desktop crate creates a fresh
/// instance each time it builds a summarizer. Callers don't need to
/// hold one across requests.
#[derive(Debug, Clone, Copy, Default)]
pub struct EnvThenKeychainResolver;

impl EnvThenKeychainResolver {
    /// Construct a fresh resolver. Provided so callers don't need to
    /// import `Default` to write `Box::new(EnvThenKeychainResolver::new())`.
    pub fn new() -> Self {
        Self
    }
}

impl KeyResolver for EnvThenKeychainResolver {
    fn resolve(&self, name: KeyName) -> Result<String, KeyResolveError> {
        // Env first — delegated to `EnvKeyResolver` so the empty-string
        // / "VAR exported but unset" handling lives in exactly one
        // place (heron-llm). Falling through on `NotFound` here means
        // the desktop crate doesn't have to track changes to the
        // env-precedence rules across crates.
        match EnvKeyResolver.resolve(name) {
            Ok(value) => return Ok(value),
            // NotFound = env unset / empty → consult the keychain below.
            Err(KeyResolveError::NotFound(_)) => {}
            // EnvKeyResolver doesn't currently produce Backend errors
            // (it only reads env vars), but the trait permits them, so
            // surface them rather than falling through to a keychain
            // probe whose answer would be ambiguous.
            Err(other) => return Err(other),
        }

        // Keychain fallback.
        let account = key_name_to_account(name);
        match keychain_get(account) {
            Ok(Some(secret)) => Ok(secret),
            Ok(None) => Err(KeyResolveError::NotFound(name)),
            // On non-macOS the keychain stub returns `Unsupported`. The
            // user has not opted into a keychain path on those targets;
            // collapse to NotFound rather than surfacing a confusing
            // "keychain unsupported" error from a renderer that may
            // not even render a keychain-related toast.
            Err(KeychainError::Unsupported) => Err(KeyResolveError::NotFound(name)),
            // Any other keychain error (e.g. macOS Security framework
            // failure, corrupted UTF-8 in the stored value) is real —
            // surface as Backend so the renderer can render a distinct
            // toast that points at the keychain rather than the
            // env var.
            Err(other) => Err(KeyResolveError::Backend(other.to_string())),
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialize tests that mutate process-global env so they don't
    /// race under cargo's parallel test runner. Same pattern the
    /// `anthropic` module uses.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Run `body` with `var` set to `value` (or removed if `None`),
    /// restoring the prior value on exit.
    fn with_env<R>(var: &str, value: Option<&str>, body: impl FnOnce() -> R) -> R {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let saved = std::env::var_os(var);
        // SAFETY: process-global env mutation is unsafe under Rust 2024
        // edition. The lock serializes env-touching tests; the restore
        // keeps post-test state matching pre-test state.
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
    fn returns_env_value_when_env_set() {
        let resolver = EnvThenKeychainResolver;
        with_env("ANTHROPIC_API_KEY", Some("env-wins-value"), || {
            // We don't care what's in the keychain on the host — env
            // takes precedence, so the keychain probe is never run.
            let got = resolver
                .resolve(KeyName::AnthropicApiKey)
                .expect("env wins");
            assert_eq!(got, "env-wins-value");
        });
    }

    #[test]
    fn empty_env_falls_through_past_env() {
        // An empty env var must NOT be returned as a successful
        // resolution — that path 401s downstream. The resolver should
        // treat empty exactly like unset and fall through to the
        // keychain. On a CI runner with no keychain entry this surfaces
        // as `NotFound` (or `Unsupported` -> NotFound off-Apple).
        let resolver = EnvThenKeychainResolver;
        with_env("ANTHROPIC_API_KEY", Some(""), || {
            let result = resolver.resolve(KeyName::AnthropicApiKey);
            // The exact result depends on whether the host has a real
            // keychain entry. CI machines don't (login keychain is
            // locked); we assert the resolver did NOT return Ok("")
            // — that's the regression we're guarding against.
            match result {
                Ok(v) => assert!(
                    !v.is_empty(),
                    "empty env must not surface as a successful empty resolve"
                ),
                Err(KeyResolveError::NotFound(_)) => {}
                Err(KeyResolveError::Backend(_)) => {
                    // Acceptable on a host where the macOS Security
                    // framework returns a non-not-found error during
                    // probe; we still didn't return Ok("").
                }
            }
        });
    }

    /// On non-macOS hosts (and on macOS hosts without a keychain
    /// entry), the resolver must report `NotFound` when env is unset.
    /// This is the contract the Anthropic constructor maps to
    /// `LlmError::MissingApiKey` so the renderer renders the same
    /// toast it does today.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn missing_env_and_unsupported_keychain_surfaces_not_found() {
        let resolver = EnvThenKeychainResolver;
        with_env("ANTHROPIC_API_KEY", None, || {
            let err = resolver
                .resolve(KeyName::AnthropicApiKey)
                .expect_err("nothing configured");
            assert!(
                matches!(err, KeyResolveError::NotFound(KeyName::AnthropicApiKey)),
                "expected NotFound on non-macOS with no env, got {err:?}"
            );
        });
    }

    /// `key_name_to_account` is the single mapping point between the
    /// two enums. Pin both directions so adding a variant in either
    /// (KeyName / KeychainAccount) without updating the other gets
    /// caught at CI time.
    #[test]
    fn key_name_to_account_round_trip() {
        assert_eq!(
            key_name_to_account(KeyName::AnthropicApiKey),
            KeychainAccount::AnthropicApiKey
        );
        assert_eq!(
            key_name_to_account(KeyName::OpenAiApiKey),
            KeychainAccount::OpenAiApiKey
        );
    }

    /// The resolver is `Send + Sync` so it can be parked behind
    /// `Arc<dyn KeyResolver>` once a future phase wires it through
    /// `Orchestrator::run`. Compile-time bound check — if a field is
    /// added that violates Send/Sync, this test stops compiling.
    #[test]
    fn resolver_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<EnvThenKeychainResolver>();
    }

    /// Real-keychain integration: with the env unset and a real entry
    /// in the macOS keychain, the resolver returns the keychain value.
    /// Gated behind the same feature flag the `keychain` module uses
    /// so CI / routine local runs don't hit the real keychain.
    #[cfg(all(target_os = "macos", feature = "test_real_keychain"))]
    #[allow(clippy::unwrap_used)]
    mod real_keychain {
        use super::*;
        use crate::keychain::{keychain_delete, keychain_set};

        // Borrow the same throwaway slot the `keychain` module uses so
        // a stale entry from a crashed test of either module gets
        // cleaned up by the next run of either suite.
        const TEST_ACCOUNT: KeychainAccount = KeychainAccount::OpenAiApiKey;
        const TEST_KEY: KeyName = KeyName::OpenAiApiKey;

        #[test]
        fn keychain_fallback_returns_stored_secret_when_env_unset() {
            let _ = keychain_delete(TEST_ACCOUNT);
            keychain_set(TEST_ACCOUNT, "from-keychain-real").unwrap();

            let resolver = EnvThenKeychainResolver;
            with_env("OPENAI_API_KEY", None, || {
                let got = resolver.resolve(TEST_KEY).expect("resolved from keychain");
                assert_eq!(got, "from-keychain-real");
            });

            keychain_delete(TEST_ACCOUNT).unwrap();
        }

        #[test]
        fn env_wins_over_keychain_when_both_set() {
            let _ = keychain_delete(TEST_ACCOUNT);
            keychain_set(TEST_ACCOUNT, "from-keychain").unwrap();

            let resolver = EnvThenKeychainResolver;
            with_env("OPENAI_API_KEY", Some("from-env"), || {
                let got = resolver.resolve(TEST_KEY).expect("env wins");
                assert_eq!(got, "from-env");
            });

            keychain_delete(TEST_ACCOUNT).unwrap();
        }

        #[test]
        fn returns_not_found_when_neither_env_nor_keychain_has_it() {
            let _ = keychain_delete(TEST_ACCOUNT);

            let resolver = EnvThenKeychainResolver;
            with_env("OPENAI_API_KEY", None, || {
                let err = resolver.resolve(TEST_KEY).expect_err("nothing configured");
                assert!(matches!(err, KeyResolveError::NotFound(TEST_KEY)));
            });
        }
    }
}
