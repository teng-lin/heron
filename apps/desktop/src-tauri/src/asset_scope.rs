//! Runtime configuration of Tauri's `asset:` protocol scope.
//!
//! `tauri.conf.json` ships an empty `assetProtocol.scope` (issue #197):
//! the static config can't enumerate user-configurable vault paths, and a
//! permissive `["**"]` lets a compromised renderer / persistent-XSS payload
//! read every file the app process can touch. Instead we extend the scope
//! at runtime to exactly the directories the legitimate consumers
//! ([`crate::heron_resolve_recording`], [`crate::meetings::heron_meeting_audio`])
//! need:
//!
//! - `<cache_root>` (recursive) — covers `<cache_root>/sessions/<id>/{mic,tap}.raw`
//!   for the salvage fallback and `<cache_root>/daemon-audio/<id>.m4a` for
//!   `fetch_audio_at`.
//! - `<vault_root>` (recursive) — covers `<vault>/meetings/<basename>.m4a`
//!   archival recordings the playback bar plays via `convertFileSrc`.
//!
//! Because users can move their vault from Settings → Vault, we expose
//! [`extend_for_vault`] so the settings command extends scope additively
//! whenever `Settings.vault_root` changes. Scope is additive on purpose:
//! a user who moves their vault back to a previous location must still be
//! able to play those recordings without an app restart.

use std::path::Path;

use tauri::Manager;
use tauri::scope::fs::Scope;

/// Extend the asset-protocol scope to allow `<cache_root>` and (when set)
/// `<vault_root>`, both recursively. Called once from the Tauri `setup`
/// hook with the boot-time roots.
///
/// Never returns `Err`: a glob-pattern failure on a user-configured path
/// is logged and swallowed so a malformed vault path can't block app
/// launch — the rest of the app already tolerates a missing vault.
pub fn install_initial_scope<R: tauri::Runtime, M: Manager<R>>(
    manager: &M,
    cache_root: &Path,
    vault_root: Option<&Path>,
) {
    let scope = manager.asset_protocol_scope();
    allow_directory(&scope, cache_root, "cache_root");
    if let Some(vault) = vault_root {
        allow_directory(&scope, vault, "vault_root");
    }
}

/// Extend the asset-protocol scope to cover `vault_root`. Called from the
/// `heron_write_settings` Tauri command after a user picks a new vault
/// directory in Settings.
///
/// Defense-in-depth: a compromised renderer that calls `heron_write_settings`
/// with `vault_root: "/"` could otherwise turn this into the same
/// "read anything" surface the static `["**"]` scope provided. We
/// therefore reject paths that fail any of:
///
/// - empty / whitespace-only (the "unset" sentinel)
/// - does not exist on disk
/// - exists but is not a directory
/// - canonicalizes to a filesystem root with no parent
///
/// Errors are logged via `tracing::warn`, never propagated: an
/// unreadable vault surfaces downstream when the playback bar's resolver
/// fails, with a more actionable error than "scope::add::pattern".
pub fn extend_for_vault<R: tauri::Runtime, M: Manager<R>>(manager: &M, vault_root: &str) {
    let trimmed = vault_root.trim();
    if trimmed.is_empty() {
        return;
    }
    let path = Path::new(trimmed);
    let canonical = match std::fs::canonicalize(path) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                vault_root = %path.display(),
                error = %e,
                "asset_scope: vault canonicalize failed; not extending scope",
            );
            return;
        }
    };
    if !canonical.is_dir() {
        tracing::warn!(
            vault_root = %canonical.display(),
            "asset_scope: vault path is not a directory; not extending scope",
        );
        return;
    }
    if canonical.parent().is_none() {
        tracing::warn!(
            vault_root = %canonical.display(),
            "asset_scope: refusing to allow filesystem root as vault scope",
        );
        return;
    }
    let scope = manager.asset_protocol_scope();
    allow_directory(&scope, &canonical, "vault_root");
}

fn allow_directory(scope: &Scope, path: &Path, label: &'static str) {
    if let Err(e) = scope.allow_directory(path, true) {
        tracing::warn!(
            kind = label,
            path = %path.display(),
            error = %e,
            "asset_scope: allow_directory failed",
        );
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use std::fs;
    use tauri::test::{MockRuntime, mock_app};

    fn fresh_app() -> tauri::App<MockRuntime> {
        mock_app()
    }

    #[test]
    fn cache_and_vault_dirs_are_allowed_recursively() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let cache = tmp.path().join("cache");
        let vault = tmp.path().join("vault");
        fs::create_dir_all(cache.join("sessions").join("abc")).expect("mkdir cache");
        fs::create_dir_all(vault.join("meetings")).expect("mkdir vault");
        let mic = cache.join("sessions").join("abc").join("mic.raw");
        let m4a = vault.join("meetings").join("note.m4a");
        fs::write(&mic, b"x").expect("seed mic");
        fs::write(&m4a, b"x").expect("seed m4a");

        let app = fresh_app();
        install_initial_scope(app.handle(), &cache, Some(&vault));
        let scope = app.handle().asset_protocol_scope();

        assert!(scope.is_allowed(&mic), "cache descendant must be allowed");
        assert!(scope.is_allowed(&m4a), "vault descendant must be allowed");
    }

    #[test]
    fn out_of_scope_path_is_denied() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let cache = tmp.path().join("cache");
        let vault = tmp.path().join("vault");
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&cache).expect("mkdir cache");
        fs::create_dir_all(&vault).expect("mkdir vault");
        fs::create_dir_all(&outside).expect("mkdir outside");
        let secret = outside.join("secret.txt");
        fs::write(&secret, b"nope").expect("seed secret");

        let app = fresh_app();
        install_initial_scope(app.handle(), &cache, Some(&vault));
        let scope = app.handle().asset_protocol_scope();

        assert!(
            !scope.is_allowed(&secret),
            "path outside cache+vault must be denied",
        );
    }

    #[test]
    fn extend_for_vault_adds_after_boot() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let cache = tmp.path().join("cache");
        let vault_a = tmp.path().join("vault-a");
        let vault_b = tmp.path().join("vault-b");
        fs::create_dir_all(cache.join("sessions")).expect("mkdir cache");
        fs::create_dir_all(vault_a.join("meetings")).expect("mkdir vault_a");
        fs::create_dir_all(vault_b.join("meetings")).expect("mkdir vault_b");
        let m4a_b = vault_b.join("meetings").join("note.m4a");
        fs::write(&m4a_b, b"x").expect("seed m4a_b");

        let app = fresh_app();
        install_initial_scope(app.handle(), &cache, Some(&vault_a));
        let scope = app.handle().asset_protocol_scope();
        assert!(
            !scope.is_allowed(&m4a_b),
            "before extend_for_vault, vault_b must be out of scope",
        );

        extend_for_vault(app.handle(), &vault_b.to_string_lossy());
        let scope = app.handle().asset_protocol_scope();
        assert!(
            scope.is_allowed(&m4a_b),
            "after extend_for_vault, vault_b must be in scope",
        );
    }

    #[test]
    fn extend_for_vault_ignores_empty_string() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let cache = tmp.path().join("cache");
        fs::create_dir_all(&cache).expect("mkdir cache");

        let app = fresh_app();
        install_initial_scope(app.handle(), &cache, None);
        // Should not panic / error. There's no observable assertion
        // beyond "did not blow up" — the no-op nature is the contract.
        extend_for_vault(app.handle(), "");
        extend_for_vault(app.handle(), "   ");
    }

    #[test]
    fn extend_for_vault_rejects_filesystem_root() {
        // Defense-in-depth: a compromised renderer must not be able to
        // turn `heron_write_settings({vault_root: "/"})` into a
        // re-introduction of the `["**"]` scope.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let cache = tmp.path().join("cache");
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&cache).expect("mkdir cache");
        fs::create_dir_all(&outside).expect("mkdir outside");
        let secret = outside.join("secret.txt");
        fs::write(&secret, b"nope").expect("seed secret");

        let app = fresh_app();
        install_initial_scope(app.handle(), &cache, None);
        extend_for_vault(app.handle(), "/");
        let scope = app.handle().asset_protocol_scope();
        assert!(
            !scope.is_allowed(&secret),
            "extend_for_vault(\"/\") must be a no-op",
        );
    }

    #[test]
    fn extend_for_vault_rejects_nonexistent_path() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let cache = tmp.path().join("cache");
        fs::create_dir_all(&cache).expect("mkdir cache");

        let app = fresh_app();
        install_initial_scope(app.handle(), &cache, None);
        // The path doesn't exist; canonicalize fails; scope unchanged.
        extend_for_vault(
            app.handle(),
            "/nonexistent/heron/vault/cannot-exist-7c3a91d2",
        );
        // Indirect: a freshly-resolved path under that nonexistent
        // directory must still be denied.
        let probe = Path::new("/nonexistent/heron/vault/cannot-exist-7c3a91d2/meetings/x.m4a");
        let scope = app.handle().asset_protocol_scope();
        assert!(!scope.is_allowed(probe));
    }

    #[test]
    fn extend_for_vault_rejects_file_path() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let cache = tmp.path().join("cache");
        fs::create_dir_all(&cache).expect("mkdir cache");
        let f = tmp.path().join("not-a-dir.txt");
        fs::write(&f, b"x").expect("seed file");

        let app = fresh_app();
        install_initial_scope(app.handle(), &cache, None);
        extend_for_vault(app.handle(), &f.to_string_lossy());
        let scope = app.handle().asset_protocol_scope();
        assert!(!scope.is_allowed(&f), "file path must not be allowed as a directory scope");
    }
}
