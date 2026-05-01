//! Issue #197 regression: asserts the runtime asset-protocol scope
//! installed by `asset_scope::install_initial_scope` denies any path
//! that isn't under the configured cache + vault roots, even though
//! `tauri.conf.json`'s static `assetProtocol.scope` is empty.
//!
//! Lives as an integration test (not a unit test) so it exercises the
//! same `Manager::asset_protocol_scope` path the production setup hook
//! does — a pure-function unit test against `tauri::scope::fs::Scope`
//! could only mimic the wiring, not pin it.

#![allow(clippy::expect_used)]

use std::fs;

use heron_desktop_lib::asset_scope;
use tauri::Manager;
use tauri::test::mock_app;

#[test]
fn out_of_scope_path_is_denied_after_install() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let cache = tmp.path().join("cache");
    let vault = tmp.path().join("vault");
    let outside = tmp.path().join("outside");
    fs::create_dir_all(cache.join("sessions").join("abc")).expect("mkdir cache");
    fs::create_dir_all(vault.join("meetings")).expect("mkdir vault");
    fs::create_dir_all(&outside).expect("mkdir outside");

    let secret = outside.join("secret.txt");
    fs::write(&secret, b"do not leak").expect("seed secret");

    let app = mock_app();
    asset_scope::install_initial_scope(app.handle(), &cache, Some(&vault));
    let scope = app.handle().asset_protocol_scope();

    assert!(
        !scope.is_allowed(&secret),
        "out-of-scope path must be denied; the renderer should not be \
         able to read arbitrary files via the asset: protocol",
    );
}

#[test]
fn legitimate_cache_and_vault_paths_are_allowed_after_install() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let cache = tmp.path().join("cache");
    let vault = tmp.path().join("vault");
    fs::create_dir_all(cache.join("sessions").join("abc")).expect("mkdir cache");
    fs::create_dir_all(cache.join("daemon-audio")).expect("mkdir daemon-audio");
    fs::create_dir_all(vault.join("meetings")).expect("mkdir vault");

    // The three legitimate consumers per issue #197:
    //   1. salvage-from-cache `mic.raw`
    //   2. daemon-fetched archival m4a
    //   3. vault-archived m4a
    let mic = cache.join("sessions").join("abc").join("mic.raw");
    let daemon_m4a = cache.join("daemon-audio").join("abc.m4a");
    let archival_m4a = vault.join("meetings").join("2026-05-01-standup.m4a");
    fs::write(&mic, b"x").expect("seed mic");
    fs::write(&daemon_m4a, b"x").expect("seed daemon");
    fs::write(&archival_m4a, b"x").expect("seed archival");

    let app = mock_app();
    asset_scope::install_initial_scope(app.handle(), &cache, Some(&vault));
    let scope = app.handle().asset_protocol_scope();

    assert!(scope.is_allowed(&mic));
    assert!(scope.is_allowed(&daemon_m4a));
    assert!(scope.is_allowed(&archival_m4a));
}
