//! macOS Keychain integration for storing API keys (PR-θ / phase 70).
//!
//! Heron's Settings pane lets the user paste an Anthropic / OpenAI API
//! key. Persisting those keys in `settings.json` would be wrong — the
//! file is plaintext under `~/Library/Application Support`, world-
//! readable by any other user on a multi-user Mac, backed up by Time
//! Machine, syncable through cloud-drive folders, etc. The macOS login
//! Keychain is the right home: scoped to the user's login session,
//! protected by the login password, ACL'd to the bundle identifier
//! that wrote the entry.
//!
//! Threat model (cliff notes — full version in the PR body):
//!
//! - secrets live in the macOS login keychain only,
//! - the service identifier scopes them to heron-desktop's bundle ID
//!   (`com.heronnote.heron`),
//! - the access-control list defaults to "applications signed by the
//!   same team", which (for a self-signed dev build) collapses to "the
//!   app that wrote it",
//! - no biometric (TouchID) gate — relies on the user's login state,
//! - secrets never cross the Rust↔JS boundary except on user-initiated
//!   `keychain_set`; the `_get` accessor is intentionally Rust-only,
//! - deletion is idempotent (a missing entry is treated as success).
//!
//! Account labels (single source of truth):
//!
//!   `KeychainAccount::AnthropicApiKey` → `"anthropic_api_key"`
//!   `KeychainAccount::OpenAiApiKey`    → `"openai_api_key"`
//!
//! Service identifier: `"com.heronnote.heron"` — hardcoded here so the
//! tests don't have to read `tauri.conf.json`. This MUST stay in lock-
//! step with the bundle identifier in `tauri.conf.json` and with
//! [`crate::default_settings_path`]'s app-id segment. A unit test pins
//! the constant so a rename-without-update gets caught at CI time.
//!
//! Test strategy
//! -------------
//! The `security-framework` crate calls into the real login keychain,
//! which on CI is locked (no UI to unlock) and which on a developer's
//! laptop would prompt every test run. We:
//!
//! 1. always run the *non-macOS stub* tests (`cfg(not(target_os =
//!    "macos"))`),
//! 2. always run the *parsing / mapping* tests on every platform
//!    (these don't touch the keychain),
//! 3. gate the real-keychain tests behind the `test_real_keychain`
//!    feature flag so they only run on a developer's machine when
//!    explicitly opted into via `cargo test --features
//!    test_real_keychain`.

use thiserror::Error;

/// Service identifier used for every Keychain entry heron writes.
///
/// Mirrors the bundle ID from `tauri.conf.json` (`com.heronnote.heron`)
/// and the app-id segment in [`crate::default_settings_path`]. We
/// hardcode it rather than read `tauri.conf.json` at compile time so
/// `cargo test` works without a Tauri context, and so the constant is
/// inspectable from one place. A unit test pins the value to catch a
/// rename-without-update.
pub const KEYCHAIN_SERVICE: &str = "com.heronnote.heron";

/// Known Keychain accounts. Adding a new variant here is the only
/// place a new secret slot needs to be declared — the Tauri-command
/// parser, the `keychain_list` enumerator, and [`KeychainAccount::all`]
/// all derive from this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeychainAccount {
    AnthropicApiKey,
    OpenAiApiKey,
}

impl KeychainAccount {
    /// Stable wire-format label used as the `account` half of the
    /// (service, account) keychain pair AND as the JSON-side string
    /// the renderer passes through `heron_keychain_*` commands.
    ///
    /// Renaming a label rotates the slot — existing entries become
    /// orphaned. Don't.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AnthropicApiKey => "anthropic_api_key",
            Self::OpenAiApiKey => "openai_api_key",
        }
    }

    /// Parse the wire-format label. Unknown values return `None` so
    /// the Tauri-command shim can reject them with a clean error
    /// instead of silently writing to a typo'd account.
    ///
    /// Named `from_label` rather than `from_str` to sidestep clippy's
    /// `should_implement_trait` lint — the signature differs from
    /// `std::str::FromStr` (returns `Option`, not `Result`), and we
    /// don't want the trait's `Err` machinery for a tiny enum.
    pub fn from_label(label: &str) -> Option<Self> {
        match label {
            "anthropic_api_key" => Some(Self::AnthropicApiKey),
            "openai_api_key" => Some(Self::OpenAiApiKey),
            _ => None,
        }
    }

    /// Every known account. `keychain_list` iterates this to discover
    /// which slots are currently populated. Adding a variant above
    /// requires adding it here.
    pub const fn all() -> &'static [KeychainAccount] {
        &[Self::AnthropicApiKey, Self::OpenAiApiKey]
    }
}

/// Error surface for keychain operations.
///
/// Crucially, the `Display` impl never embeds the secret value or any
/// caller-supplied input that could carry a secret — `Backend(String)`
/// only ever receives the platform's own error description, and
/// `UnknownAccount` echoes only the label (which is non-sensitive).
#[derive(Debug, Error)]
pub enum KeychainError {
    /// Underlying platform-keychain error (macOS Security framework).
    /// The string is the platform's own description; never an echo of
    /// the secret value.
    #[error("keychain backend error: {0}")]
    Backend(String),
    /// Caller passed an account label that doesn't map to a known
    /// `KeychainAccount` variant.
    #[error("unknown keychain account: {0}")]
    UnknownAccount(String),
    /// Non-macOS platforms: the keychain surface is unavailable. The
    /// Tauri-command shims convert this to a renderer-side error so
    /// the UI can degrade gracefully (Linux dev builds hide the
    /// keychain panel rather than crashing).
    #[error("keychain is not supported on this platform")]
    Unsupported,
}

// ---- macOS implementation -----------------------------------------

#[cfg(target_os = "macos")]
mod platform {
    use super::{KEYCHAIN_SERVICE, KeychainAccount, KeychainError};
    use security_framework::base::Error as SfError;
    use security_framework::passwords::{
        delete_generic_password, get_generic_password, set_generic_password,
    };

    /// `errSecItemNotFound` is the documented "no such item" code; we
    /// map it to `Ok(None)` (or `Ok(())` for delete) instead of
    /// surfacing as a generic backend error.
    const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

    fn is_not_found(err: &SfError) -> bool {
        err.code() == ERR_SEC_ITEM_NOT_FOUND
    }

    /// Set (create-or-replace) the secret for `account`. The
    /// `security_framework` API replaces an existing entry on the same
    /// (service, account) pair — no separate delete-then-add dance.
    pub fn keychain_set(account: KeychainAccount, secret: &str) -> Result<(), KeychainError> {
        set_generic_password(KEYCHAIN_SERVICE, account.as_str(), secret.as_bytes())
            .map_err(|e| KeychainError::Backend(e.to_string()))
    }

    /// Look up the secret for `account`. Returns `Ok(None)` if the
    /// entry doesn't exist; `Err` for any other backend error.
    ///
    /// This is the only API that returns the cleartext secret — it is
    /// **never** reachable from the renderer. Callers must treat the
    /// returned `String` as sensitive (don't log it, don't bubble it
    /// through `Display`).
    pub fn keychain_get(account: KeychainAccount) -> Result<Option<String>, KeychainError> {
        match get_generic_password(KEYCHAIN_SERVICE, account.as_str()) {
            Ok(bytes) => {
                // Keychain entries are byte buffers; we only ever store
                // UTF-8 strings, but a corrupted entry could theoretically
                // contain non-UTF-8. Reject loudly rather than silently
                // returning a lossy decode — that would mask a real
                // problem (third-party tampering, encoding bug).
                let s = String::from_utf8(bytes).map_err(|_| {
                    KeychainError::Backend("stored secret is not valid UTF-8".to_owned())
                })?;
                Ok(Some(s))
            }
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(KeychainError::Backend(e.to_string())),
        }
    }

    /// Delete the entry for `account`. Idempotent: a missing entry is
    /// treated as success.
    pub fn keychain_delete(account: KeychainAccount) -> Result<(), KeychainError> {
        match delete_generic_password(KEYCHAIN_SERVICE, account.as_str()) {
            Ok(()) => Ok(()),
            Err(e) if is_not_found(&e) => Ok(()),
            Err(e) => Err(KeychainError::Backend(e.to_string())),
        }
    }
}

#[cfg(target_os = "macos")]
pub use platform::{keychain_delete, keychain_get, keychain_set};

// ---- non-macOS stub -----------------------------------------------

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::{KeychainAccount, KeychainError};

    /// Stub that compiles on Linux/Windows so workspace `cargo build`
    /// passes on every CI runner. Returns `Unsupported` for all four
    /// API entry points; the Tauri-command shims surface that as a
    /// renderer-side error.
    pub fn keychain_set(_account: KeychainAccount, _secret: &str) -> Result<(), KeychainError> {
        Err(KeychainError::Unsupported)
    }

    pub fn keychain_get(_account: KeychainAccount) -> Result<Option<String>, KeychainError> {
        Err(KeychainError::Unsupported)
    }

    pub fn keychain_delete(_account: KeychainAccount) -> Result<(), KeychainError> {
        Err(KeychainError::Unsupported)
    }
}

#[cfg(not(target_os = "macos"))]
pub use platform::{keychain_delete, keychain_get, keychain_set};

/// Enumerate the accounts that currently have entries.
///
/// Implemented as a fan-out of [`keychain_get`] over
/// [`KeychainAccount::all`] — Apple's Security framework doesn't have
/// a clean "list entries by service" API on the generic-password
/// surface, and probing N known slots is fast enough (sub-millisecond
/// per lookup) for a list with two entries. On non-macOS this returns
/// `Err(Unsupported)` because the underlying `_get` does.
///
/// Returns *labels of populated accounts*. The actual secret values
/// are dropped before this function returns — they never leave the
/// stack frame they were read into.
pub fn keychain_list() -> Result<Vec<KeychainAccount>, KeychainError> {
    let mut found = Vec::new();
    for account in KeychainAccount::all() {
        // `if let Some(_)` drops the secret immediately — only the
        // "this slot is populated" bit escapes this loop. Errors
        // (including `Unsupported` on non-macOS) bubble unchanged via
        // the `?` so callers can render a platform-appropriate
        // message instead of an empty list that masks the real
        // failure.
        if keychain_get(*account)?.is_some() {
            found.push(*account);
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_label_round_trip() {
        for account in KeychainAccount::all() {
            let label = account.as_str();
            assert_eq!(KeychainAccount::from_label(label), Some(*account));
        }
    }

    #[test]
    fn from_label_known_values() {
        // Belt-and-suspenders: explicit assertions per variant so a
        // future rename of either label can't pass the round-trip
        // test by accident (e.g. swapping both halves of the match).
        assert_eq!(
            KeychainAccount::from_label("anthropic_api_key"),
            Some(KeychainAccount::AnthropicApiKey)
        );
        assert_eq!(
            KeychainAccount::from_label("openai_api_key"),
            Some(KeychainAccount::OpenAiApiKey)
        );
    }

    #[test]
    fn unknown_account_label_rejected() {
        assert!(KeychainAccount::from_label("nope").is_none());
        assert!(KeychainAccount::from_label("").is_none());
        // A near-miss should also fail — defensive against typo
        // routes that would otherwise silently write to the wrong slot.
        assert!(KeychainAccount::from_label("anthropic_api_keys").is_none());
    }

    #[test]
    fn service_identifier_pinned_to_bundle_id() {
        // The service string MUST stay in lockstep with
        // `tauri.conf.json::identifier` and `default_settings_path`'s
        // app-id segment. If you rename the bundle, update this test.
        assert_eq!(KEYCHAIN_SERVICE, "com.heronnote.heron");
    }

    #[test]
    fn account_labels_are_distinct() {
        let labels: Vec<&str> = KeychainAccount::all().iter().map(|a| a.as_str()).collect();
        let mut sorted = labels.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(labels.len(), sorted.len(), "duplicate account labels");
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn non_macos_set_returns_unsupported() {
        let res = keychain_set(KeychainAccount::AnthropicApiKey, "ignored");
        assert!(matches!(res, Err(KeychainError::Unsupported)));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn non_macos_get_returns_unsupported() {
        let res = keychain_get(KeychainAccount::AnthropicApiKey);
        assert!(matches!(res, Err(KeychainError::Unsupported)));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn non_macos_delete_returns_unsupported() {
        let res = keychain_delete(KeychainAccount::AnthropicApiKey);
        assert!(matches!(res, Err(KeychainError::Unsupported)));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn non_macos_list_returns_unsupported() {
        let res = keychain_list();
        assert!(matches!(res, Err(KeychainError::Unsupported)));
    }

    /// Real-keychain tests, gated behind a feature flag so they don't
    /// run on CI (where the login keychain is locked) or on a
    /// developer's laptop without consent. Run manually via
    /// `cargo test -p heron-desktop --features test_real_keychain
    /// keychain::tests::real_`.
    #[cfg(all(target_os = "macos", feature = "test_real_keychain"))]
    #[allow(clippy::unwrap_used)]
    mod real_keychain {
        use super::super::*;

        // A throwaway slot — uses the OpenAI variant since we don't
        // yet ship a real OpenAI integration. The test cleans up after
        // itself, but if it crashes mid-flight, the worst case is a
        // stale entry under `com.heronnote.heron / openai_api_key`
        // that the user can delete from Keychain Access.
        const TEST_ACCOUNT: KeychainAccount = KeychainAccount::OpenAiApiKey;

        #[test]
        fn real_set_get_delete_round_trip() {
            // Clean up any leftover from a previous run.
            let _ = keychain_delete(TEST_ACCOUNT);

            keychain_set(TEST_ACCOUNT, "test-secret-value").unwrap();
            let got = keychain_get(TEST_ACCOUNT).unwrap();
            assert_eq!(got, Some("test-secret-value".to_owned()));

            keychain_delete(TEST_ACCOUNT).unwrap();
            let after = keychain_get(TEST_ACCOUNT).unwrap();
            assert_eq!(after, None);
        }

        #[test]
        fn real_delete_missing_is_idempotent() {
            // Make sure it isn't there.
            let _ = keychain_delete(TEST_ACCOUNT);
            // A second delete must succeed.
            keychain_delete(TEST_ACCOUNT).unwrap();
        }

        #[test]
        fn real_set_replaces_existing_entry() {
            let _ = keychain_delete(TEST_ACCOUNT);
            keychain_set(TEST_ACCOUNT, "first").unwrap();
            keychain_set(TEST_ACCOUNT, "second").unwrap();
            assert_eq!(
                keychain_get(TEST_ACCOUNT).unwrap(),
                Some("second".to_owned())
            );
            keychain_delete(TEST_ACCOUNT).unwrap();
        }
    }
}
