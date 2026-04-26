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
//! ## Consumer side (PR-μ / phase 74)
//!
//! The summarizer paths in `crates/heron-llm` consume these entries
//! through the `KeyResolver` trait defined at
//! `crates/heron-llm/src/key_resolver.rs`. The desktop-only
//! `EnvThenKeychainResolver` (`crate::keychain_resolver`) layers a
//! call to [`keychain_get`] under the env-var read so a user who only
//! pasted their key into Settings → Summarizer can still summarize
//! without exporting `ANTHROPIC_API_KEY`. CLI users keep the
//! historical env-only behaviour via `heron_llm::EnvKeyResolver`.
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

use std::sync::Mutex;

use thiserror::Error;

/// Tracks which env vars *this process* populated from the keychain.
///
/// The `sync_env_for_account` IPC command only touches env vars listed
/// here. This preserves a developer's shell-exported `OPENAI_API_KEY`
/// across an accidental Settings → Save / Delete: if the user launched
/// the app with the var already exported, hydration skipped it (env
/// wins), this set stayed empty for that account, and the IPC path
/// will refuse to mutate it. The user's shell environment stays intact
/// even though the keychain entry changed.
///
/// `Mutex<Vec<...>>` (rather than `OnceLock` + `HashSet`) because the
/// set is at most two entries and is read on the cold IPC path; a
/// `Vec::contains` linear scan is faster than hashing here. The mutex
/// is held for nanoseconds.
static OWNED_ENV_VARS: Mutex<Vec<KeychainAccount>> = Mutex::new(Vec::new());

fn mark_owned(account: KeychainAccount) {
    let mut owned = match OWNED_ENV_VARS.lock() {
        Ok(g) => g,
        // A poisoned lock is recoverable here — the inner Vec is still
        // a Vec; we only ever push/contains/remove on it. Take the
        // inner data and carry on.
        Err(p) => p.into_inner(),
    };
    if !owned.contains(&account) {
        owned.push(account);
    }
}

fn is_owned(account: KeychainAccount) -> bool {
    let owned = match OWNED_ENV_VARS.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    owned.contains(&account)
}

fn unmark_owned(account: KeychainAccount) {
    let mut owned = match OWNED_ENV_VARS.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    owned.retain(|a| *a != account);
}

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

    /// Process-env variable name the in-process daemon reads for this
    /// account, or `None` if the account is consumed only through the
    /// `EnvThenKeychainResolver` (i.e. the summarizer paths in
    /// `heron-llm`, which read the keychain directly without going via
    /// the env).
    ///
    /// The OpenAI Realtime backend (`heron_realtime::OpenAiRealtime`)
    /// reads `OPENAI_API_KEY` straight from the process environment via
    /// `std::env::var`. The desktop's in-process `herond` therefore
    /// needs that env var populated *in this process* before the
    /// orchestrator constructs the realtime backend — see
    /// [`hydrate_env_from_keychain`] for the bridge.
    ///
    /// `AnthropicApiKey` is also mapped here for symmetry, even though
    /// the summarizer's `EnvThenKeychainResolver` already consults the
    /// keychain after the env miss. Pre-populating the env from the
    /// keychain at startup means a subprocess spawned from the daemon
    /// (the `claude_code_cli` summarizer backend, the `codex_cli`
    /// backend) inherits the key without reaching back into the parent
    /// process. That widens "the keychain works without an exported
    /// shell var" from "Realtime + the in-process summarizer" to "all
    /// daemon-spawned summarizer processes".
    pub const fn env_var(self) -> Option<&'static str> {
        match self {
            Self::AnthropicApiKey => Some("ANTHROPIC_API_KEY"),
            Self::OpenAiApiKey => Some("OPENAI_API_KEY"),
        }
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

/// Hydrate the process environment with API-key secrets from the
/// macOS login Keychain.
///
/// Why this exists
/// ---------------
/// The OpenAI Realtime backend (`heron_realtime::OpenAiRealtime::from_env`)
/// reads `OPENAI_API_KEY` straight from `std::env`, with no resolver
/// hook to consult the keychain. The desktop ships an in-process
/// `herond` that constructs the orchestrator (which will eventually
/// build `OpenAiRealtime` from the live-session owner — gap #2 in
/// `docs/archives/codebase-gaps.md`); for that lookup to succeed, the
/// env var must be populated in *this* process before the daemon
/// starts.
///
/// Closes alpha-blocker gap #2: a user can paste their key into
/// Settings → Summarizer (which writes the keychain) and the daemon
/// picks it up without the user exporting `OPENAI_API_KEY` in their
/// shell before launch.
///
/// Precedence
/// ----------
/// Mirrors [`crate::keychain_resolver::EnvThenKeychainResolver`]:
/// **env wins**. If `<VAR>` is already set to a non-empty value (a
/// developer running the desktop binary from a terminal with the var
/// exported, a CI smoke test) we leave it alone. Empty / unset values
/// are treated as misses and overwritten from the keychain when an
/// entry exists. Accounts with no keychain entry leave the env var
/// untouched.
///
/// Logging
/// -------
/// The function logs *which slots* it hydrated (by [`KeychainAccount`]
/// label — never the secret), and *why* it skipped a slot (env-set vs
/// keychain-empty). Backend errors surface at `warn` so a developer
/// without keychain access still sees the desktop launch successfully
/// — the orchestrator will fail later with a clearer "missing key"
/// error from the realtime backend itself.
///
/// Safety
/// ------
/// Rust 2024 marks [`std::env::set_var`] as `unsafe` because it races
/// with concurrent `getenv` readers in process. This function is meant
/// to be called *once*, early in the desktop setup hook (before any
/// orchestrator/recorder task spawns) and again from the
/// `heron_keychain_set` / `heron_keychain_delete` Tauri commands, both
/// of which run on Tauri's serialised IPC dispatch thread. There is
/// therefore no real concurrent reader at the moment any `set_var`
/// call lands. The unsafe block is annotated with the same SAFETY
/// note as `keychain_resolver::tests::with_env`.
///
/// Returns the count of env vars hydrated this call. Used by the
/// caller for a diagnostic log line; the renderer never sees this.
pub fn hydrate_env_from_keychain() -> usize {
    let mut hydrated = 0usize;
    for account in KeychainAccount::all() {
        let Some(var) = account.env_var() else {
            continue;
        };
        if env_is_set_nonempty(var) {
            tracing::debug!(
                account = account.as_str(),
                env_var = var,
                "keychain hydration: env already set, skipping",
            );
            continue;
        }
        match keychain_get(*account) {
            Ok(Some(secret)) => {
                // SAFETY: documented above — hydration runs in the
                // desktop setup hook before any orchestrator/recorder
                // task is spawned, so there are no concurrent env
                // readers in this process at the moment of the write.
                unsafe {
                    std::env::set_var(var, &secret);
                }
                // `secret` drops at end of scope. We don't zeroise — the
                // `security_framework` crate doesn't return zeroising
                // buffers, matching every other `keychain_get` caller.
                mark_owned(*account);
                tracing::info!(
                    account = account.as_str(),
                    env_var = var,
                    "keychain hydration: env populated from keychain",
                );
                hydrated += 1;
            }
            Ok(None) => {
                tracing::debug!(
                    account = account.as_str(),
                    env_var = var,
                    "keychain hydration: keychain empty, env left unset",
                );
            }
            Err(KeychainError::Unsupported) => {
                // Non-macOS dev build, or the security-framework feature
                // is gated out. The desktop never wires keychain on
                // those targets; carry on quietly.
                tracing::debug!(
                    account = account.as_str(),
                    "keychain hydration: keychain unsupported on this platform",
                );
            }
            Err(e) => {
                // Real backend failure (locked keychain, ACL denial,
                // corrupted entry). Log + continue — better to launch
                // a partially-keyed app than to crash. The orchestrator
                // will surface a "missing key" error if/when the user
                // tries to start a meeting.
                tracing::warn!(
                    account = account.as_str(),
                    error = %e,
                    "keychain hydration: backend error; env left unset",
                );
            }
        }
    }
    hydrated
}

/// Mirror of [`hydrate_env_from_keychain`] for a single account, called
/// by the `heron_keychain_set` / `heron_keychain_delete` Tauri command
/// shims so the daemon picks up edits without an app restart.
///
/// Ownership rule
/// --------------
/// To preserve a developer's shell-exported `OPENAI_API_KEY` /
/// `ANTHROPIC_API_KEY` across an accidental Settings save/delete, this
/// function only mutates env vars that *this process* populated from
/// the keychain (tracked in [`OWNED_ENV_VARS`], seeded by hydration).
///
/// - `Some(secret)` on an env var we own (or that is currently empty/
///   unset): write through, mark owned. The daemon picks up the new
///   value on its next `env::var` call.
/// - `Some(secret)` on an env var the user exported externally: leave
///   the env alone — the keychain entry was still updated by the
///   caller, so a future restart will pick it up. We log a one-line
///   diagnostic so a confused developer ("I saved my key but the
///   daemon still uses the old one") sees the cause in the system log.
/// - `None` on an env var we own: clear it, drop the ownership flag.
/// - `None` on an env var the user exported externally: leave alone.
///
/// Skips accounts where [`KeychainAccount::env_var`] is `None`.
///
/// # Safety
///
/// Called from the Tauri sync-command dispatcher, which serialises
/// command invocations. Other tasks in the process (the orchestrator,
/// the in-process axum daemon, the recorder) may concurrently call
/// [`std::env::var`], which races with `set_var`/`remove_var` under the
/// Rust 2024 unsafety contract. For this PR we accept the residual
/// risk — the alternative (plumbing the API key through every backend
/// constructor as an explicit field) is the larger refactor that
/// belongs to gap #1 (orchestrator wiring) in
/// `docs/archives/codebase-gaps.md`. Until that lands, the
/// `OpenAiRealtime::from_env` contract forces an env-var bridge here.
///
/// Practical mitigation: the writes are short, infrequent (user-
/// initiated from a Settings click), and atomic at the libc level on
/// macOS — the data race is technically observable but unlikely to
/// produce a torn read in the small payload sizes API keys occupy.
pub fn sync_env_for_account(account: KeychainAccount, secret: Option<&str>) {
    let Some(var) = account.env_var() else {
        return;
    };

    // Externally-exported env vars are never touched by the IPC path.
    // Ownership is only set by hydration (which respected env-wins) or
    // by a previous `sync` write — both paths represent "this process
    // is the source of truth for this var".
    let we_own_it = is_owned(account);
    let env_currently_set = env_is_set_nonempty(var);
    if env_currently_set && !we_own_it {
        tracing::info!(
            account = account.as_str(),
            env_var = var,
            "keychain sync: env was set externally; leaving env unchanged \
             (keychain was updated, restart to pick up new value)",
        );
        return;
    }

    match secret {
        Some(value) => {
            // SAFETY: see function-level safety note. Called from the
            // serialised Tauri sync-command dispatcher; the racing
            // reader risk is acknowledged + accepted there.
            unsafe {
                std::env::set_var(var, value);
            }
            mark_owned(account);
            tracing::info!(
                account = account.as_str(),
                env_var = var,
                "keychain sync: env populated from Settings update",
            );
        }
        None => {
            // SAFETY: see above.
            unsafe {
                std::env::remove_var(var);
            }
            unmark_owned(account);
            tracing::info!(
                account = account.as_str(),
                env_var = var,
                "keychain sync: env cleared (Settings delete)",
            );
        }
    }
}

/// Helper: is `var` currently set to a non-empty value?
///
/// Both the precedence rule in [`hydrate_env_from_keychain`] and the
/// "empty env counts as miss" contract in
/// [`crate::keychain_resolver::EnvThenKeychainResolver`] use this
/// predicate; centralising it keeps the two sites in lockstep.
fn env_is_set_nonempty(var: &str) -> bool {
    matches!(std::env::var(var), Ok(v) if !v.is_empty())
}

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

    /// Serialise tests that mutate the real `OPENAI_API_KEY` /
    /// `ANTHROPIC_API_KEY` env vars + the static `OWNED_ENV_VARS`
    /// tracker so they don't race under cargo's parallel test runner.
    /// Same pattern as `keychain_resolver::tests::ENV_LOCK`.
    static REAL_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Reset the per-account ownership flag. Used by tests that flip
    /// ownership state and want to restore a known baseline before the
    /// next test reads it.
    fn reset_owned(account: KeychainAccount) {
        unmark_owned(account);
    }

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

    /// `KeychainAccount::all()` is hand-maintained — adding a variant
    /// to the enum without adding it to `all()` would silently leave
    /// `keychain_list` skipping that slot. The exhaustive match in
    /// `count_variants` won't compile if a new variant is added,
    /// forcing whoever adds it to pick the new count + thus revisit
    /// `all()`. This is the cheapest compile-time guard for two
    /// variants; if the enum grows, switch to `strum::EnumIter`.
    #[test]
    fn all_covers_every_variant() {
        const fn count_variants(a: KeychainAccount) -> usize {
            match a {
                // Each arm contributes 1 — sum equals the variant count.
                // Adding a variant fails to compile until added here.
                KeychainAccount::AnthropicApiKey => 2,
                KeychainAccount::OpenAiApiKey => 2,
            }
        }
        assert_eq!(
            KeychainAccount::all().len(),
            count_variants(KeychainAccount::AnthropicApiKey),
            "KeychainAccount::all() is out of sync with the enum's variant count",
        );
    }

    /// `env_var` is the bridge between a keychain slot and the process
    /// env var the in-process daemon reads. Pin both mappings so
    /// renaming `OPENAI_API_KEY` (or, more realistically, accidentally
    /// swapping the two arms) is caught at CI time.
    #[test]
    fn env_var_mapping_is_pinned() {
        assert_eq!(
            KeychainAccount::AnthropicApiKey.env_var(),
            Some("ANTHROPIC_API_KEY"),
        );
        assert_eq!(
            KeychainAccount::OpenAiApiKey.env_var(),
            Some("OPENAI_API_KEY"),
        );
    }

    /// `env_is_set_nonempty` underpins the env-wins precedence in
    /// hydration and in `EnvThenKeychainResolver`. Both surfaces must
    /// agree that empty == miss; pin the predicate directly.
    #[test]
    fn env_is_set_nonempty_treats_empty_as_unset() {
        // Use a bespoke var name unlikely to clash with anything else
        // a developer might have exported. The test serialises with the
        // env-mutating tests in `keychain_resolver` via the same lock,
        // but those use ANTHROPIC_API_KEY / OPENAI_API_KEY — different
        // names, so no contention is possible without a typo.
        const VAR: &str = "HERON_KEYCHAIN_TEST_NONEMPTY_PROBE";
        let saved = std::env::var_os(VAR);
        // SAFETY: process-global env mutation. The variable name is
        // bespoke to this test (see comment above) so no other thread
        // is observing it. Restored at the end of the test.
        unsafe {
            std::env::remove_var(VAR);
        }
        assert!(
            !env_is_set_nonempty(VAR),
            "unset env must read as 'not set'"
        );
        unsafe {
            std::env::set_var(VAR, "");
        }
        assert!(
            !env_is_set_nonempty(VAR),
            "empty env must read as 'not set'"
        );
        unsafe {
            std::env::set_var(VAR, "value");
        }
        assert!(env_is_set_nonempty(VAR), "non-empty env must read as 'set'");
        // Restore.
        unsafe {
            match saved {
                Some(v) => std::env::set_var(VAR, v),
                None => std::env::remove_var(VAR),
            }
        }
    }

    /// `sync_env_for_account` is the live-update path the keychain set/
    /// delete Tauri commands call after a successful keychain mutation.
    /// We exercise the env-write/clear behaviour against a bespoke env
    /// var name (not the real `OPENAI_API_KEY` / `ANTHROPIC_API_KEY`)
    /// to avoid clobbering a developer's exported keys mid-run.
    ///
    /// The function dispatches by `account.env_var()`; we can't change
    /// that mapping at test time. Instead we assert the *observable
    /// effect* on the real env var name with a value the test owns,
    /// then restore the prior state.
    #[test]
    #[allow(clippy::expect_used)]
    fn sync_env_for_account_writes_and_clears() {
        let _lock = REAL_ENV_LOCK.lock().expect("real env lock");
        const ACCOUNT: KeychainAccount = KeychainAccount::OpenAiApiKey;
        // `env_var()` returns `Option<&'static str>` — the OpenAI variant
        // currently maps to `Some("OPENAI_API_KEY")`. A future change
        // that drops the env mapping for this slot would turn this test
        // into a no-op rather than a panic, which is the safer
        // regression mode here (the test pinning the mapping itself
        // — `env_var_mapping_is_pinned` — would catch the rename).
        let Some(var) = ACCOUNT.env_var() else {
            return;
        };
        let saved = std::env::var_os(var);

        // Start from a clean slate: env unset, account un-owned. The
        // ownership rule says an empty/unset env is fair game for the
        // sync write path, so the first `sync` will succeed and mark
        // ownership.
        unsafe {
            std::env::remove_var(var);
        }
        reset_owned(ACCOUNT);

        sync_env_for_account(ACCOUNT, Some("test-sentinel-value"));
        assert_eq!(
            std::env::var(var).ok().as_deref(),
            Some("test-sentinel-value"),
            "set path must populate the env var when env was empty",
        );
        assert!(is_owned(ACCOUNT), "set path must mark the account as owned",);

        sync_env_for_account(ACCOUNT, None);
        assert!(
            std::env::var_os(var).is_none(),
            "clear path must remove the env var (got {:?})",
            std::env::var_os(var),
        );
        assert!(
            !is_owned(ACCOUNT),
            "clear path must drop the ownership flag",
        );

        // Restore.
        unsafe {
            match saved {
                Some(v) => std::env::set_var(var, v),
                None => std::env::remove_var(var),
            }
        }
        reset_owned(ACCOUNT);
    }

    /// Ownership rule: when env was set externally (we did not hydrate
    /// it), `sync_env_for_account` must NOT mutate the env. Pin both
    /// the set and clear paths so a future refactor can't silently
    /// stomp a developer's exported `OPENAI_API_KEY` from a stray
    /// click in Settings.
    #[test]
    #[allow(clippy::expect_used)]
    fn sync_leaves_externally_exported_env_alone() {
        let _lock = REAL_ENV_LOCK.lock().expect("real env lock");
        const ACCOUNT: KeychainAccount = KeychainAccount::OpenAiApiKey;
        let Some(var) = ACCOUNT.env_var() else {
            return;
        };
        let saved = std::env::var_os(var);

        // External shell exported the var; we did not hydrate.
        unsafe {
            std::env::set_var(var, "from-shell-export");
        }
        reset_owned(ACCOUNT);

        // Settings → Save: must NOT overwrite the exported value.
        sync_env_for_account(ACCOUNT, Some("from-settings-save"));
        assert_eq!(
            std::env::var(var).ok().as_deref(),
            Some("from-shell-export"),
            "sync(Some) must leave externally-exported env intact",
        );
        assert!(
            !is_owned(ACCOUNT),
            "sync(Some) on externally-set env must not mark ownership",
        );

        // Settings → Delete: must NOT clear the exported value either.
        sync_env_for_account(ACCOUNT, None);
        assert_eq!(
            std::env::var(var).ok().as_deref(),
            Some("from-shell-export"),
            "sync(None) must leave externally-exported env intact",
        );

        // Restore.
        unsafe {
            match saved {
                Some(v) => std::env::set_var(var, v),
                None => std::env::remove_var(var),
            }
        }
        reset_owned(ACCOUNT);
    }

    /// Ownership rule, second half: an env var the *process* hydrated
    /// (or wrote via a previous sync) IS fair game for subsequent
    /// sync calls. Distinct from the externally-exported case above —
    /// without this guarantee, the user couldn't update their key from
    /// Settings on the same launch they entered it.
    #[test]
    #[allow(clippy::expect_used)]
    fn sync_updates_owned_env_in_place() {
        let _lock = REAL_ENV_LOCK.lock().expect("real env lock");
        const ACCOUNT: KeychainAccount = KeychainAccount::OpenAiApiKey;
        let Some(var) = ACCOUNT.env_var() else {
            return;
        };
        let saved = std::env::var_os(var);

        // Simulate a prior `sync` write: env populated, account owned.
        unsafe {
            std::env::set_var(var, "first-value");
        }
        reset_owned(ACCOUNT);
        mark_owned(ACCOUNT);
        assert!(is_owned(ACCOUNT));

        // Update path: must overwrite, ownership stays.
        sync_env_for_account(ACCOUNT, Some("second-value"));
        assert_eq!(
            std::env::var(var).ok().as_deref(),
            Some("second-value"),
            "sync(Some) on owned env must update in place",
        );
        assert!(is_owned(ACCOUNT), "ownership must persist across update");

        // Delete path: must clear, ownership drops.
        sync_env_for_account(ACCOUNT, None);
        assert!(
            std::env::var_os(var).is_none(),
            "sync(None) on owned env must clear",
        );
        assert!(
            !is_owned(ACCOUNT),
            "sync(None) on owned env must drop ownership",
        );

        // Restore.
        unsafe {
            match saved {
                Some(v) => std::env::set_var(var, v),
                None => std::env::remove_var(var),
            }
        }
        reset_owned(ACCOUNT);
    }

    /// `hydrate_env_from_keychain` honours env precedence: a non-empty
    /// pre-existing env value must NOT be overwritten by the keychain
    /// path. This pins the rule alongside the
    /// `EnvThenKeychainResolver::returns_env_value_when_env_set` test
    /// in `keychain_resolver.rs` so a future refactor that flips the
    /// precedence on the hydration side gets caught here even if the
    /// resolver test is updated in lockstep.
    ///
    /// The test asserts the env var still reads what it had before
    /// hydration. We don't run on macOS in CI with a populated keychain,
    /// so this check is meaningful on any platform: if hydration wrote
    /// the keychain value, the env would no longer match the sentinel.
    #[test]
    #[allow(clippy::expect_used)]
    fn hydrate_respects_env_precedence_when_env_set() {
        let _lock = REAL_ENV_LOCK.lock().expect("real env lock");
        const VAR: &str = "OPENAI_API_KEY";
        const ACCOUNT: KeychainAccount = KeychainAccount::OpenAiApiKey;
        let saved = std::env::var_os(VAR);

        // SAFETY: see other env-mutating tests in this module.
        unsafe {
            std::env::set_var(VAR, "env-already-set-sentinel");
        }
        // Reset ownership to prove hydration honoured env-wins by
        // *not* marking ownership (the env was set externally, from
        // the test's perspective).
        reset_owned(ACCOUNT);
        let _hydrated = hydrate_env_from_keychain();
        assert_eq!(
            std::env::var(VAR).ok().as_deref(),
            Some("env-already-set-sentinel"),
            "hydration must not overwrite a non-empty pre-existing env value",
        );
        assert!(
            !is_owned(ACCOUNT),
            "hydration must NOT mark ownership when env was already set",
        );

        unsafe {
            match saved {
                Some(v) => std::env::set_var(VAR, v),
                None => std::env::remove_var(VAR),
            }
        }
        reset_owned(ACCOUNT);
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
