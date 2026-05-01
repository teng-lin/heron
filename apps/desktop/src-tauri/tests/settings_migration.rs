//! Schema-migration + round-trip tests for `Settings` (issue #187).
//!
//! Tier 1 added nine `#[serde(default)]` fields onto the on-disk
//! `Settings` shape (`apps/desktop/src-tauri/src/settings.rs`) without
//! fixture-driven coverage. The unit tests next to the module already
//! pin the per-field migration story (e.g.
//! `read_pre_tier1_settings_fills_new_field_defaults`), but they
//! seed JSON inline as raw string literals — a future field rename or
//! schema cleanup can edit the literal at the same time it edits the
//! struct, defeating the migration check.
//!
//! The fixtures here live as committed `.json` files in
//! `tests/fixtures/`. A migration regression has to *also* edit the
//! fixture, which is the whole point — the fixture is the captured
//! shape of an older release, not an artifact regenerated from the
//! current code.

#![allow(clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use heron_desktop_lib::settings::{
    ActiveMode, FileNamingPattern, Persona, Settings, read_settings, write_settings,
};
use serde_json::Value;

/// Resolve a path under `apps/desktop/src-tauri/tests/` from
/// `CARGO_MANIFEST_DIR` (= `apps/desktop/src-tauri`). Same recipe the
/// IPC contract test uses; cargo guarantees the env var at test build
/// time.
fn fixture_path(name: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests");
    p.push("fixtures");
    p.push(name);
    p
}

fn read_fixture_value(name: &str) -> Value {
    let path = fixture_path(name);
    let bytes =
        std::fs::read(&path).unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()));
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|e| panic!("parse fixture {} as JSON: {e}", path.display()))
}

/// The exact set of field keys Tier 1 added on top of the pre-Tier-1
/// schema. Hard-coded here so the test itself documents the migration
/// surface; a future field add must update this set, which forces
/// reviewer attention on the migration story.
fn tier1_added_keys() -> BTreeSet<&'static str> {
    BTreeSet::from([
        "hotwords",
        "persona",
        "file_naming_pattern",
        "summary_retention_days",
        "strip_names_before_summarization",
        "show_tray_indicator",
        "auto_detect_meeting_app",
        "openai_model",
        "shortcuts",
    ])
}

/// 1) A pre-Tier-1 `settings.json` deserializes via the
///    container-level `#[serde(default)]`, and re-serializing the
///    resulting struct yields a JSON object whose key set differs from
///    the original by exactly the Tier-1 additions — no renames, no
///    field drops, no shape drift. Field *values* present in the
///    original are preserved untouched.
#[test]
fn pre_tier1_fixture_deserializes_and_diffs_only_by_new_keys() {
    let original = read_fixture_value("settings_pre_tier1.json");
    let original_obj = original.as_object().expect("fixture must be a JSON object");

    // Deserialize the pre-Tier-1 fixture into a current-shape `Settings`.
    let parsed: Settings =
        serde_json::from_value(original.clone()).expect("pre-Tier-1 fixture must deserialize");

    // Spot-check that the values from the file survived the partial
    // deserialize untouched — belt-and-suspenders against a regression
    // that confuses container-level `default` with field-level reset.
    // Every value here is deliberately non-default so a regression that
    // resets a pre-Tier-1 field to its default would fail loudly.
    assert_eq!(parsed.stt_backend, "sherpa");
    assert_eq!(parsed.llm_backend, "claude_code_cli");
    assert!(!parsed.auto_summarize);
    assert_eq!(parsed.vault_root, "/Users/example/Vault");
    assert_eq!(parsed.record_hotkey, "F12");
    assert_eq!(parsed.remind_interval_secs, 60);
    assert!(!parsed.recover_on_launch);
    assert_eq!(parsed.min_free_disk_mib, 1024);
    assert!(!parsed.session_logging);
    assert!(parsed.crash_telemetry);
    assert_eq!(parsed.audio_retention_days, Some(30));
    assert!(parsed.onboarded);
    assert_eq!(
        parsed.target_bundle_ids,
        vec!["us.zoom.xos".to_owned(), "com.microsoft.teams2".to_owned()],
    );
    assert_eq!(parsed.active_mode, ActiveMode::Clio);

    // The Tier-1 fields the file does not carry must come from
    // `Settings::default()` rather than ::default()-of-each-type — the
    // distinction matters for fields with non-trivial defaults (e.g.
    // `openai_model` defaults to `"gpt-4o-mini"`, not `String::new()`).
    let defaults = Settings::default();
    assert_eq!(parsed.hotwords, defaults.hotwords);
    assert_eq!(parsed.persona, defaults.persona);
    assert_eq!(parsed.file_naming_pattern, defaults.file_naming_pattern);
    assert_eq!(
        parsed.summary_retention_days,
        defaults.summary_retention_days
    );
    assert_eq!(
        parsed.strip_names_before_summarization,
        defaults.strip_names_before_summarization
    );
    assert_eq!(parsed.show_tray_indicator, defaults.show_tray_indicator);
    assert_eq!(
        parsed.auto_detect_meeting_app,
        defaults.auto_detect_meeting_app
    );
    assert_eq!(parsed.openai_model, defaults.openai_model);
    assert_eq!(parsed.shortcuts, defaults.shortcuts);

    // Re-serialize and diff. Only the Tier-1 added keys may appear in
    // the round-tripped object's key set on top of the original's; no
    // pre-Tier-1 key may disappear.
    let round_tripped = serde_json::to_value(&parsed).expect("serialize parsed Settings");
    let round_tripped_obj = round_tripped
        .as_object()
        .expect("Settings serializes to a JSON object");

    let original_keys: BTreeSet<&str> = original_obj.keys().map(String::as_str).collect();
    let round_tripped_keys: BTreeSet<&str> = round_tripped_obj.keys().map(String::as_str).collect();

    let only_in_original: BTreeSet<&str> = original_keys
        .difference(&round_tripped_keys)
        .copied()
        .collect();
    assert!(
        only_in_original.is_empty(),
        "pre-Tier-1 keys must survive the round trip; missing on re-serialize: {only_in_original:?}",
    );

    let added_keys: BTreeSet<&str> = round_tripped_keys
        .difference(&original_keys)
        .copied()
        .collect();
    let expected_added = tier1_added_keys();
    assert_eq!(
        added_keys, expected_added,
        "diff must be exactly the Tier-1 added keys; got {added_keys:?}, expected {expected_added:?}",
    );

    // Every key the original *did* carry must round-trip with the same
    // JSON value — no silent re-encode (e.g. integer → float, bool →
    // string) under the partial-deserialize path.
    for (key, original_value) in original_obj {
        let round_tripped_value = round_tripped_obj
            .get(key)
            .unwrap_or_else(|| panic!("key {key:?} missing from re-serialized Settings"));
        assert_eq!(
            round_tripped_value, original_value,
            "value for key {key:?} drifted across the migration round trip",
        );
    }
}

/// 2) A `settings.json` carrying every Tier-1 field round-trips through
///    `write_settings` / `read_settings` with structural equality —
///    JSON object key order is not guaranteed by serde, so the test
///    compares parsed `serde_json::Value`s rather than raw bytes.
///
/// Acts as the canonical "current schema fully populated" fixture: a
/// future field add must update `settings_full.json` to keep this green,
/// which is the desired forcing function — the fixture is the captured
/// shape of the current release, not an artifact regenerated from the
/// struct's `Default`.
#[test]
fn full_fixture_round_trips_with_structural_equality() {
    let original = read_fixture_value("settings_full.json");

    let parsed: Settings =
        serde_json::from_value(original.clone()).expect("full fixture must deserialize");

    // Sanity-check the parsed struct carries every Tier-1 field with
    // its non-default fixture value — this is what makes the round-trip
    // assertion below load-bearing.
    assert_eq!(parsed.hotwords, vec!["heron", "Anthropic", "Tauri"]);
    assert_eq!(
        parsed.persona,
        Persona {
            name: "Alice Example".to_owned(),
            role: "Product Manager".to_owned(),
            working_on: "Q2 launch plan".to_owned(),
        }
    );
    assert_eq!(parsed.file_naming_pattern, FileNamingPattern::DateSlug);
    assert_eq!(parsed.summary_retention_days, Some(90));
    assert!(parsed.strip_names_before_summarization);
    assert!(!parsed.show_tray_indicator);
    assert!(!parsed.auto_detect_meeting_app);
    assert_eq!(parsed.openai_model, "gpt-4o");
    assert_eq!(parsed.shortcuts.len(), 2);
    assert_eq!(
        parsed.shortcuts.get("toggle_recording").map(String::as_str),
        Some("F12"),
    );
    assert_eq!(parsed.active_mode, ActiveMode::Athena);

    // Drive the on-disk path: write the parsed struct via the real
    // atomic-rename writer, read it back via the real reader, and
    // assert structural equality with the original fixture JSON. This
    // exercises the same code path the Settings pane uses at runtime,
    // not just the in-memory serde round trip.
    let tmp = tempfile::TempDir::new().expect("tmp");
    let path = tmp.path().join("settings.json");
    write_settings(&path, &parsed).expect("write");

    let written_bytes = std::fs::read(&path).expect("read written file");
    let written: Value =
        serde_json::from_slice(&written_bytes).expect("parse written file as JSON");
    // `serde_json::Value`'s `PartialEq` for `Object(_)` already compares
    // by key set + per-key value, independent of insertion order, so a
    // direct equality is the structural-equality check the issue spec
    // asks for. No need for a hand-rolled walker.
    assert_eq!(
        written, original,
        "fixture must round-trip through write_settings/read_settings without shape drift",
    );

    let reread = read_settings(&path).expect("re-read parsed settings");
    assert_eq!(reread, parsed, "Settings round-trips structurally");
}

/// 3) Per-field stress: every settings field must survive a write→read
///    cycle carrying values from the torture classes the issue spec
///    enumerates — empty string, max int, unicode, RTL text, embedded
///    backslash. String-class values are spread across every
///    string-bearing field (including the `shortcuts` BTreeMap keys);
///    `u32::MAX` is exercised on every numeric field plus the optional
///    retention-day fields.
#[test]
fn tier1_fields_round_trip_under_per_field_stress() {
    use std::collections::BTreeMap;

    // Each (label, torture-string) pair is exercised against every
    // string-bearing field. The label surfaces in panic messages so a
    // failure points at the offending class without re-reading the
    // test source.
    let torture_strings: &[(&str, &str)] = &[
        ("empty", ""),
        // Mix of CJK + emoji-with-ZWJ + combining diacritic.
        ("unicode", "héron — \u{1f426}\u{200d}\u{2b1c} 鷺"),
        // Right-to-left Arabic + Hebrew. RTL text canonically round-
        // trips byte-for-byte in JSON; pinning it here catches a future
        // serializer that accidentally normalizes/escapes RTL chars.
        ("rtl", "اجتماع עברית"),
        // Backslashes are JSON-escaped on the wire (`\\`) and have to
        // survive the decode unmodified — Windows-style paths users
        // hand-edit into the file are the realistic source.
        ("backslash", r"C:\Users\Alice\Documents\Vault"),
    ];

    for (label, value) in torture_strings {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let path = tmp.path().join("settings.json");

        let s_in = Settings {
            // Pre-Tier-1 string-bearing fields: cover them too so the
            // stress isn't artificially scoped to Tier-1 additions.
            vault_root: (*value).to_owned(),
            record_hotkey: (*value).to_owned(),
            // Tier-1 string-bearing fields.
            openai_model: (*value).to_owned(),
            hotwords: vec![(*value).to_owned()],
            persona: Persona {
                name: (*value).to_owned(),
                role: (*value).to_owned(),
                working_on: (*value).to_owned(),
            },
            // The `shortcuts` map keys *and* values both flow into JSON;
            // exercising both pins that the BTreeMap key path doesn't
            // mishandle unicode under serde's default `Map` encoding.
            shortcuts: BTreeMap::from([((*value).to_owned(), (*value).to_owned())]),
            ..Default::default()
        };

        write_settings(&path, &s_in).unwrap_or_else(|e| panic!("[{label}] write: {e}"));
        let s_out = read_settings(&path).unwrap_or_else(|e| panic!("[{label}] read: {e}"));
        assert_eq!(s_out, s_in, "[{label}] string fields must round-trip");
    }

    // Numeric-boundary stress (the issue spec's "max int" class): every
    // `u32` field at `u32::MAX`, plus the optional retention-day fields
    // carrying `Some(u32::MAX)`. The `i64` JSON number range comfortably
    // fits `u32::MAX`, but a future refactor that retypes a counter to
    // `i32` would silently overflow — pin the upper bound.
    let tmp = tempfile::TempDir::new().expect("tmp");
    let path = tmp.path().join("settings.json");
    let s_in = Settings {
        remind_interval_secs: u32::MAX,
        min_free_disk_mib: u32::MAX,
        audio_retention_days: Some(u32::MAX),
        summary_retention_days: Some(u32::MAX),
        ..Default::default()
    };
    write_settings(&path, &s_in).expect("write max-int settings");
    let s_out = read_settings(&path).expect("read max-int settings");
    assert_eq!(s_out, s_in, "[max-int] u32::MAX values must round-trip");
}
