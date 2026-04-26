//! Keychain ACL scope check (macOS-only).
//!
//! `docs/security.md` §3.3 ("Keychain ACL — `swift/keychain-helper`")
//! requires that secrets stored in the login keychain by heron are
//! scoped to the heron bundle ID (`com.heronnote.heron`) — i.e. no
//! arbitrary other signed app on the user's machine can read the
//! Anthropic API key. The §6.5 keychain ACL **release test** is the
//! authoritative end-to-end check (it round-trips a fake key between
//! two signed binaries with different bundle IDs); this preflight
//! check is the **runtime** complement: at first run, can heron
//! actually find a keychain item for its own bundle ID?
//!
//! The check is a positive existence probe rather than an ACL
//! introspection probe because the macOS Security framework does not
//! expose a stable API to enumerate the ACL of a generic-password
//! item without prompting the user. So we ask the cheaper question:
//!
//! > Does a keychain entry exist under `(service =
//! > com.heronnote.heron, account = anthropic_api_key)`, and can we
//! > read it without prompting?
//!
//! That gives us three meaningful signals:
//!
//! - **Pass** — an entry exists at the heron `(service, account)`
//!   pair AND this process can read it without prompting. **This is
//!   a NECESSARY but NOT SUFFICIENT** condition for "the ACL is
//!   correctly scoped to heron." A successful read also matches the
//!   case where a previously-approved external app has been
//!   added to the entry's ACL — the §6.5 keychain ACL release test
//!   (which round-trips between two signed binaries with different
//!   bundle IDs) is the authoritative answer; this preflight is the
//!   first-run "is the entry there at all?" complement. The probe
//!   uses `ItemSearchOptions::load_data(false)` so the secret bytes
//!   are never materialised into the doctor's address space — only
//!   the existence + readability bits are observable.
//! - **Warn** — no entry under the heron service. Either the user
//!   hasn't pasted a key yet (fresh install, expected — onboarding
//!   wizard handles this) or someone deleted it. Surface as a
//!   non-blocking warning.
//! - **Fail** — keychain backend returned something other than
//!   "not found" (locked keychain, hardware error). Block onboarding
//!   so the user can unlock the login keychain manually.
//!
//! ## What this does NOT verify
//!
//! - We don't audit the ACL list itself; that's §6.5's job.
//! - We don't write a probe entry. Mutating the keychain at preflight
//!   time would surprise the user and require code-signing parity
//!   between the test and prod binaries to avoid an "always-succeeds"
//!   false positive on `cargo run`.
//! - We don't prompt. If the OS ever tries to prompt, the
//!   `security_framework` API returns an error rather than blocking
//!   the calling thread — which we map to `Fail`.

use security_framework::base::Error as SfError;
use security_framework::item::{ItemClass, ItemSearchOptions};

use super::{CheckSeverity, RuntimeCheck, RuntimeCheckOptions, RuntimeCheckResult};

const NAME: &str = "keychain_acl";

/// `errSecItemNotFound`. Mirrors the constant in
/// `apps/desktop/src-tauri/src/keychain.rs::ERR_SEC_ITEM_NOT_FOUND`.
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

/// Keychain service identifier heron writes API keys under. Mirrors
/// `apps/desktop/src-tauri/src/keychain.rs::KEYCHAIN_SERVICE`. Pinned
/// here rather than imported because pulling the Tauri app crate in
/// as a dep on heron-doctor would invert the layering (doctor is a
/// leaf crate; the desktop app already depends on doctor for §16
/// automation hooks). A unit test pins both constants stay in sync.
pub(crate) const KEYCHAIN_SERVICE: &str = "com.heronnote.heron";

/// Account label probed for existence. Matches
/// `KeychainAccount::AnthropicApiKey::as_str()` from the desktop app.
pub(crate) const PROBE_ACCOUNT: &str = "anthropic_api_key";

/// Outcome of a single keychain probe. `EntryFound` does NOT carry
/// the secret — the cleartext is dropped inside the probe.
///
/// `#[non_exhaustive]` so a future variant (e.g. `EntryAclTooBroad`
/// once we have a way to introspect the ACL) is non-breaking.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum KeychainProbeOutcome {
    /// Entry exists under (service, account) and was read without
    /// prompting. Implies our code-signing identity is on the ACL.
    EntryFound,
    /// Service / account pair has no entry. Onboarding wizard step
    /// for "paste API key" hasn't run yet.
    EntryMissing,
    /// Backend returned an unexpected error (locked keychain, hardware
    /// issue). Carries the platform's own error description.
    BackendError { reason: String },
}

/// Trait the [`KeychainAclCheck`] uses to ask "is the heron-scoped
/// keychain entry present and readable?" Indirected so tests can
/// stub all three outcomes without touching the real login keychain.
pub trait KeychainProbe: Send + Sync {
    fn probe(&self) -> KeychainProbeOutcome;
}

/// Real-world probe via `security-framework`. Reads under the
/// hardcoded heron `(service, account)` pair and discards the
/// returned bytes immediately.
pub fn real_probe() -> Box<dyn KeychainProbe> {
    Box::new(SecurityFrameworkProbe)
}

struct SecurityFrameworkProbe;

impl KeychainProbe for SecurityFrameworkProbe {
    fn probe(&self) -> KeychainProbeOutcome {
        // Use `ItemSearchOptions` with `load_data(false)` so the
        // probe asks the keychain "does this item exist and am I
        // allowed to read it?" without ever copying the secret bytes
        // into our address space. The previous impl used
        // `get_generic_password`, which returns the cleartext — even
        // though we dropped it immediately, materializing it was
        // unnecessary secret exposure for a diagnostic probe (Codex
        // CK1 review note).
        match ItemSearchOptions::new()
            .class(ItemClass::generic_password())
            .service(KEYCHAIN_SERVICE)
            .account(PROBE_ACCOUNT)
            .load_data(false)
            .load_attributes(false)
            .load_refs(false)
            .search()
        {
            Ok(_) => KeychainProbeOutcome::EntryFound,
            Err(e) if is_not_found(&e) => KeychainProbeOutcome::EntryMissing,
            Err(e) => KeychainProbeOutcome::BackendError {
                reason: e.to_string(),
            },
        }
    }
}

fn is_not_found(err: &SfError) -> bool {
    err.code() == ERR_SEC_ITEM_NOT_FOUND
}

/// Keychain ACL scope check. Construct with [`real_probe`] for
/// production or with a stub for tests.
pub struct KeychainAclCheck {
    probe: Box<dyn KeychainProbe>,
}

impl KeychainAclCheck {
    pub fn new(probe: Box<dyn KeychainProbe>) -> Self {
        Self { probe }
    }
}

impl RuntimeCheck for KeychainAclCheck {
    fn name(&self) -> &'static str {
        NAME
    }

    fn run(&self, _opts: &RuntimeCheckOptions) -> RuntimeCheckResult {
        match self.probe.probe() {
            KeychainProbeOutcome::EntryFound => RuntimeCheckResult::pass(
                NAME,
                "keychain entry exists and is readable (note: ACL scope \
                 not audited — see §6.5 release test)",
            ),
            KeychainProbeOutcome::EntryMissing => RuntimeCheckResult {
                name: NAME,
                severity: CheckSeverity::Warn,
                summary: "no API key in keychain yet".to_owned(),
                detail: format!(
                    "no entry found at (service: {KEYCHAIN_SERVICE}, account: \
                     {PROBE_ACCOUNT}). Paste your Anthropic key into Settings \
                     → Summarizer or export ANTHROPIC_API_KEY for env-only \
                     mode (CLI / docker)."
                ),
            },
            KeychainProbeOutcome::BackendError { reason } => RuntimeCheckResult::fail(
                NAME,
                "keychain backend returned an error",
                format!(
                    "Security framework error: {reason}. Most often the login \
                     keychain is locked — open Keychain Access, double-click \
                     'login', and unlock with your account password."
                ),
            ),
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    struct StubProbe(KeychainProbeOutcome);
    impl KeychainProbe for StubProbe {
        fn probe(&self) -> KeychainProbeOutcome {
            self.0.clone()
        }
    }

    fn check(outcome: KeychainProbeOutcome) -> RuntimeCheckResult {
        KeychainAclCheck::new(Box::new(StubProbe(outcome))).run(&RuntimeCheckOptions::default())
    }

    #[test]
    fn entry_found_yields_pass() {
        let r = check(KeychainProbeOutcome::EntryFound);
        assert_eq!(r.severity, CheckSeverity::Pass);
        assert_eq!(r.name, NAME);
    }

    #[test]
    fn entry_missing_yields_warn() {
        let r = check(KeychainProbeOutcome::EntryMissing);
        assert_eq!(r.severity, CheckSeverity::Warn);
        assert!(r.detail.contains("com.heronnote.heron"));
        assert!(r.detail.contains("anthropic_api_key"));
    }

    #[test]
    fn backend_error_yields_fail() {
        let r = check(KeychainProbeOutcome::BackendError {
            reason: "errSecAuthFailed".to_owned(),
        });
        assert_eq!(r.severity, CheckSeverity::Fail);
        assert!(r.detail.contains("errSecAuthFailed"));
    }

    #[test]
    fn name_is_stable() {
        let c = KeychainAclCheck::new(Box::new(StubProbe(KeychainProbeOutcome::EntryFound)));
        assert_eq!(c.name(), "keychain_acl");
    }

    #[test]
    fn service_constant_matches_desktop_keychain() {
        // Mirrors `apps/desktop/src-tauri/src/keychain.rs::KEYCHAIN_SERVICE`.
        // If the desktop crate ever renames the bundle ID this assertion
        // is the canary that says "update both places."
        assert_eq!(KEYCHAIN_SERVICE, "com.heronnote.heron");
        assert_eq!(PROBE_ACCOUNT, "anthropic_api_key");
    }

    /// Smoke test against the **real** login keychain. Gated behind
    /// `#[ignore]` because routine `cargo test` should not touch the
    /// user's keychain (`heron-desktop`'s `keychain.rs` does the same
    /// thing). Run on demand with
    /// `cargo test -p heron-doctor real_probe_does_not_panic -- --ignored`.
    #[test]
    #[ignore]
    fn real_probe_does_not_panic() {
        let p = real_probe();
        match p.probe() {
            KeychainProbeOutcome::EntryFound
            | KeychainProbeOutcome::EntryMissing
            | KeychainProbeOutcome::BackendError { .. } => {}
        }
    }
}
