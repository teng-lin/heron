//! Disk-level integration tests for the [RFC 7396] JSON Merge Patch
//! action-item write-back path
//! (`VaultWriter::update_action_item`). The unit tests next to
//! `writer.rs` exercise the per-field semantics in-process; this file
//! drives the same path through the actual file system so YAML-
//! quoting, frontmatter-fence, and trailing-newline drift surface as
//! test failures rather than ride home in production.
//!
//! Scope follows issue #188's acceptance list and the
//! deletion-semantics decision recorded in
//! [`docs/adr/0001-action-item-deletion-semantics.md`]. There is no
//! deletion verb in the IPC write-back today, so this file does not
//! contain a deletion test — that's the ADR's whole point.
//!
//! [RFC 7396]: https://datatracker.ietf.org/doc/html/rfc7396

#![allow(clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use chrono::NaiveDate;
use heron_types::{
    ActionItem, Cost, DiarizeSource, Disclosure, DisclosureHow, Frontmatter, ItemId, MeetingType,
};
use heron_vault::{ActionItemPatch, VaultError, VaultWriter, read_note};

// ---------- shared fixtures ----------

fn baseline_frontmatter() -> Frontmatter {
    Frontmatter {
        date: NaiveDate::from_ymd_opt(2026, 4, 24).expect("valid date"),
        start: "14:00".into(),
        duration_min: 47,
        company: Some("Acme".into()),
        attendees: vec![],
        meeting_type: MeetingType::Client,
        source_app: "us.zoom.xos".into(),
        recording: PathBuf::from("recordings/2026-04-24-1400.m4a"),
        transcript: PathBuf::from("transcripts/2026-04-24-1400.jsonl"),
        diarize_source: DiarizeSource::Ax,
        disclosed: Disclosure {
            stated: true,
            when: Some("00:14".into()),
            how: DisclosureHow::Verbal,
        },
        cost: Cost {
            summary_usd: 0.04,
            tokens_in: 14_231,
            tokens_out: 612,
            model: "claude-sonnet-4-6".into(),
        },
        action_items: vec![],
        tags: vec!["meeting".into(), "acme".into()],
        extra: serde_yaml::Mapping::default(),
    }
}

/// Drop two action items into a finalized note on disk and return the
/// `(note_path, id_a, id_b)` triple. Mirrors the in-module helper in
/// `writer.rs` so the integration tests can prove their per-field
/// patches reach disk identically.
fn seed_two(writer: &VaultWriter) -> (PathBuf, ItemId, ItemId) {
    let id_a = uuid::Uuid::now_v7();
    let id_b = uuid::Uuid::now_v7();
    let mut fm = baseline_frontmatter();
    fm.action_items = vec![
        ActionItem {
            id: id_a,
            owner: "alice".into(),
            text: "Send pricing deck".into(),
            due: Some("2026-05-01".into()),
            done: false,
        },
        ActionItem {
            id: id_b,
            owner: "bob".into(),
            text: "Schedule kickoff".into(),
            due: None,
            done: false,
        },
    ];
    let body = "\n## Action items\n\n- [ ] Send pricing deck\n- [ ] Schedule kickoff\n";
    let path = writer
        .finalize_session("2026-04-24", "1400", "rfc7396", &fm, body)
        .expect("finalize");
    (path, id_a, id_b)
}

/// Apply a patch synthesized from a JSON string so the test exercises
/// the same `serde_json::from_str` boundary the IPC layer crosses on
/// every renderer call.
fn apply_json_patch(
    writer: &VaultWriter,
    path: &Path,
    id: &ItemId,
    json: &str,
) -> Result<ActionItem, VaultError> {
    let patch: ActionItemPatch =
        serde_json::from_str(json).expect("patch json parses on the integration boundary");
    writer.update_action_item(path, id, patch)
}

// ---------- RFC 7396 per-field on-disk round-trip ----------

/// Acceptance criterion 1: a field omitted from the JSON patch leaves
/// the on-disk row unchanged. Issue #188 calls this out explicitly —
/// the merge-patch "no change" signal must survive a real
/// finalize → patch → read cycle.
#[test]
fn rfc7396_field_omitted_leaves_disk_row_unchanged() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let writer = VaultWriter::new(tmp.path());
    let (path, id_a, id_b) = seed_two(&writer);

    // Patch only `done`. Owner / due / text are *omitted* from the JSON.
    apply_json_patch(&writer, &path, &id_a, r#"{"done": true}"#).expect("patch a");

    let (fm, _) = read_note(&path).expect("read");
    let row_a = fm.action_items.iter().find(|i| i.id == id_a).expect("a");
    let row_b = fm.action_items.iter().find(|i| i.id == id_b).expect("b");

    // The field we patched flipped.
    assert!(row_a.done);

    // Every omitted field round-tripped untouched on disk.
    assert_eq!(row_a.owner, "alice");
    assert_eq!(row_a.text, "Send pricing deck");
    assert_eq!(row_a.due.as_deref(), Some("2026-05-01"));
    // The sibling row is fully untouched.
    assert_eq!(row_b.owner, "bob");
    assert_eq!(row_b.text, "Schedule kickoff");
    assert!(row_b.due.is_none());
    assert!(!row_b.done);
}

/// Acceptance criterion 2: a field set to JSON `null` clears the
/// matching nullable field on disk. RFC 7396's "clear" signal must
/// survive YAML serialize → atomic_write → re-read.
#[test]
fn rfc7396_field_null_clears_on_disk() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let writer = VaultWriter::new(tmp.path());
    let (path, id_a, _id_b) = seed_two(&writer);

    apply_json_patch(&writer, &path, &id_a, r#"{"due": null}"#).expect("patch");

    let (fm, _) = read_note(&path).expect("read");
    let row_a = fm.action_items.iter().find(|i| i.id == id_a).expect("a");
    assert!(row_a.due.is_none(), "due must clear when set to null");
    // Owner is *not* in the patch; double-option semantics: outer-None
    // means "no change", not "clear", so the seeded value stays.
    assert_eq!(row_a.owner, "alice");
}

/// Acceptance criterion 3: a field set to a value updates the on-disk
/// row. The renderer's "set" signal round-trips through serialize +
/// atomic-write + re-read.
#[test]
fn rfc7396_field_value_updates_on_disk() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let writer = VaultWriter::new(tmp.path());
    let (path, id_a, _id_b) = seed_two(&writer);

    apply_json_patch(
        &writer,
        &path,
        &id_a,
        r#"{"text": "Send the polished deck", "owner": "teng", "due": "2026-06-01"}"#,
    )
    .expect("patch");

    let (fm, body) = read_note(&path).expect("read");
    let row_a = fm.action_items.iter().find(|i| i.id == id_a).expect("a");
    assert_eq!(row_a.text, "Send the polished deck");
    assert_eq!(row_a.owner, "teng");
    assert_eq!(row_a.due.as_deref(), Some("2026-06-01"));
    // The body bullet's text portion was synced to match.
    assert!(
        body.contains("- [ ] Send the polished deck"),
        "body bullet must follow the frontmatter text update; got: {body}",
    );
}

/// Combined RFC 7396 round-trip: one patch exercises every shape
/// (omitted / null / value) in the same call. The on-disk state must
/// reflect the union of all three semantics.
#[test]
fn rfc7396_combined_patch_round_trips_through_disk() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let writer = VaultWriter::new(tmp.path());
    let (path, id_a, _id_b) = seed_two(&writer);

    // owner: set, due: clear, text: omit, done: set.
    apply_json_patch(
        &writer,
        &path,
        &id_a,
        r#"{"owner": "teng", "due": null, "done": true}"#,
    )
    .expect("patch");

    let (fm, body) = read_note(&path).expect("read");
    let row_a = fm.action_items.iter().find(|i| i.id == id_a).expect("a");
    assert_eq!(row_a.owner, "teng", "owner: set survived");
    assert!(row_a.due.is_none(), "due: null cleared");
    assert_eq!(row_a.text, "Send pricing deck", "text: omitted untouched");
    assert!(row_a.done, "done: set survived");
    // Body bullet flips for `done`. Text didn't change so the bullet
    // text portion stays.
    assert!(
        body.contains("- [x] Send pricing deck"),
        "body bullet should reflect done=true; got: {body}",
    );
}

// ---------- Concurrency: promote the unit-test invariant to disk ----------

/// `writer.rs` already pins the concurrent-write invariant in-process.
/// The issue asks us to prove the same property end-to-end through the
/// real filesystem so any future regression in `atomic_write`'s
/// rename semantics surfaces here too. We don't pin a winner — the
/// race is resolved by the OS rename — but both threads must succeed
/// AND the post-race note must parse cleanly (no half-written YAML).
#[test]
fn concurrent_disk_writes_observe_atomic_renames() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let writer = Arc::new(VaultWriter::new(tmp.path()));
    let (path, id_a, id_b) = seed_two(&writer);

    // Spawn 4 threads (2 per row) so the rename order is genuinely
    // unstable on a typical CI scheduler. The unit-test version uses
    // 2 threads; bumping to 4 here is the cheapest way to make the
    // race actually race on a single-core runner.
    let mut handles = Vec::new();
    for n in 0..4u32 {
        let writer = Arc::clone(&writer);
        let path = path.clone();
        let target = if n.is_multiple_of(2) { id_a } else { id_b };
        handles.push(thread::spawn(move || {
            let patch: ActionItemPatch =
                serde_json::from_str(&format!(r#"{{"text": "writer {n} touched the row"}}"#,))
                    .expect("patch json parses");
            writer.update_action_item(&path, &target, patch)
        }));
    }
    for h in handles {
        h.join().expect("join").expect("patch ok");
    }

    // Post-race: note still parses cleanly and both rows are present.
    // Last-writer-wins on each row's text — we don't assert which
    // winner — but the note must not be a half-written mix.
    let (fm, _body) = read_note(&path).expect("post-race note must parse");
    assert_eq!(fm.action_items.len(), 2);
    assert!(fm.action_items.iter().any(|i| i.id == id_a));
    assert!(fm.action_items.iter().any(|i| i.id == id_b));
}

// ---------- Hostile YAML round-trip ----------

/// Stress-test the YAML serializer + frontmatter renderer with values
/// the wild internet (and a global user base) actually produces.
/// Each survives a finalize → patch → read round-trip without quoting
/// drift, dropped graphemes, or fence corruption.
#[test]
fn hostile_yaml_round_trips_through_patch_path() {
    let tmp = tempfile::TempDir::new().expect("tmp");
    let writer = VaultWriter::new(tmp.path());

    // Each row is its own torture case. Sharing one note proves the
    // serializer doesn't cross-contaminate rows when several pathological
    // values sit next to each other in the same YAML sequence.
    let id_emoji = uuid::Uuid::now_v7();
    let id_rtl = uuid::Uuid::now_v7();
    let id_quotes = uuid::Uuid::now_v7();
    let id_multiline = uuid::Uuid::now_v7();
    let id_tz_edge = uuid::Uuid::now_v7();
    let id_null_due = uuid::Uuid::now_v7();

    // Hostile inputs:
    // - Emoji owner including a ZWJ sequence ("woman technologist").
    let emoji_owner = "alice \u{1F469}\u{200D}\u{1F4BB}";
    // - RTL Arabic with an embedded LTR digit.
    let rtl_text = "اجتماع 2026 — مراجعة";
    // - Embedded ASCII double-quote *and* a YAML-significant colon.
    let quoted_text = r#"He said: "ship it" — then we shipped"#;
    // - Multiline note (literal newline mid-string).
    let multiline_text = "line one\nline two\n  indented line three";
    // - TZ-edge calendar date (Pacific/Kiritimati keeps UTC+14; this
    //   is a real ISO date the renderer must not mangle).
    let tz_edge_due = "2026-12-31";

    let mut fm = baseline_frontmatter();
    fm.action_items = vec![
        ActionItem {
            id: id_emoji,
            owner: emoji_owner.into(),
            text: "Polish slide deck".into(),
            due: None,
            done: false,
        },
        ActionItem {
            id: id_rtl,
            owner: "team".into(),
            text: rtl_text.into(),
            due: None,
            done: false,
        },
        ActionItem {
            id: id_quotes,
            owner: "ceo".into(),
            text: quoted_text.into(),
            due: None,
            done: false,
        },
        ActionItem {
            id: id_multiline,
            owner: "writer".into(),
            text: multiline_text.into(),
            due: None,
            done: false,
        },
        ActionItem {
            id: id_tz_edge,
            owner: "ops".into(),
            text: "Year-end rollover check".into(),
            due: Some(tz_edge_due.into()),
            done: false,
        },
        ActionItem {
            id: id_null_due,
            owner: String::new(),
            text: "Owner-less, due-less item".into(),
            due: None,
            done: false,
        },
    ];

    let body = "\n## Action items\n\n- [ ] (see frontmatter)\n";
    let path = writer
        .finalize_session("2026-04-24", "1400", "hostile", &fm, body)
        .expect("finalize");

    // Read it back BEFORE any patch — the serializer survives every
    // input verbatim.
    let (fm_pre, _) = read_note(&path).expect("read pre");
    let row = |id: ItemId| -> ActionItem {
        fm_pre
            .action_items
            .iter()
            .find(|i| i.id == id)
            .cloned()
            .expect("row exists")
    };
    assert_eq!(row(id_emoji).owner, emoji_owner);
    assert_eq!(row(id_rtl).text, rtl_text);
    assert_eq!(row(id_quotes).text, quoted_text);
    assert_eq!(row(id_multiline).text, multiline_text);
    assert_eq!(row(id_tz_edge).due.as_deref(), Some(tz_edge_due));
    assert!(row(id_null_due).due.is_none());
    assert_eq!(row(id_null_due).owner, "");

    // Now patch each row through the integration boundary. After each
    // patch, every OTHER row must still round-trip its hostile value
    // verbatim — the serializer cannot smear quoting decisions across
    // rows.
    apply_json_patch(&writer, &path, &id_quotes, r#"{"done": true}"#).expect("patch quotes");
    apply_json_patch(&writer, &path, &id_tz_edge, r#"{"due": null}"#).expect("clear tz_edge due");
    apply_json_patch(
        &writer,
        &path,
        &id_null_due,
        r#"{"owner": "now-set", "due": "2026-07-04"}"#,
    )
    .expect("set null_due fields");

    let (fm_post, _) = read_note(&path).expect("read post");
    let row_post = |id: ItemId| -> ActionItem {
        fm_post
            .action_items
            .iter()
            .find(|i| i.id == id)
            .cloned()
            .expect("row exists")
    };
    // Untouched rows: every hostile value is byte-identical.
    assert_eq!(row_post(id_emoji).owner, emoji_owner);
    assert_eq!(row_post(id_rtl).text, rtl_text);
    assert_eq!(
        row_post(id_quotes).text,
        quoted_text,
        "embedded-quote text must survive a sibling row's patch",
    );
    assert!(row_post(id_quotes).done, "patched done flipped");
    assert_eq!(row_post(id_multiline).text, multiline_text);
    // Patched rows: signals applied.
    assert!(row_post(id_tz_edge).due.is_none(), "due cleared via null");
    assert_eq!(row_post(id_null_due).owner, "now-set");
    assert_eq!(row_post(id_null_due).due.as_deref(), Some("2026-07-04"));
}

// ---------- Vault-lock failure surface ----------

/// RAII guard that restores a path's `Permissions` on `Drop`. Used by
/// the vault-lock test below so a panic mid-test still leaves the
/// tempdir cleanable. Mirrors the pattern recommended by
/// gemini-code-assist on PR #203.
#[cfg(unix)]
struct PermsGuard {
    path: PathBuf,
    original: std::fs::Permissions,
}

#[cfg(unix)]
impl Drop for PermsGuard {
    fn drop(&mut self) {
        // Best-effort: if a panic is already unwinding, swallow the
        // restore error rather than double-panicking. The tempdir
        // cleanup will still run (and succeed once perms are back).
        let _ = std::fs::set_permissions(&self.path, self.original.clone());
    }
}

/// `update_action_item` does NOT retry today — an `EAGAIN`/`EACCES`
/// from iCloud's lock surfaces immediately as `VaultError::Io` to the
/// caller. This test pins the *contract* (error type + on-disk
/// invariance), not the timing — the latter would be flake-prone on a
/// throttled CI runner per `CONTRIBUTING.md` ("prefer lower bounds
/// over upper bounds"). When iCloud lock retry lands, the test gets a
/// new sibling that asserts elapsed >= some_lower_bound; this one's
/// invariants stay valid for the no-retry path that the renderer's
/// optimistic-rollback envelope still depends on.
///
/// Approach: chmod the meeting file's parent directory to deny writes
/// (via a `Drop` guard so a panic restores perms before tempdir
/// cleanup), attempt the patch, observe the IO failure, confirm the
/// on-disk note is the pre-patch content (no half-write).
///
/// Unix-only: Windows + Tauri's mobile targets don't ship in v1, and
/// the chmod technique relies on POSIX semantics.
#[cfg(unix)]
#[test]
fn vault_lock_surface_propagates_io_error_without_retry() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::TempDir::new().expect("tmp");
    let writer = VaultWriter::new(tmp.path());
    let (path, id_a, _id_b) = seed_two(&writer);

    let pre = fs::read_to_string(&path).expect("read pre");
    let parent = path.parent().expect("note parent dir").to_path_buf();
    let original_perms = fs::metadata(&parent).expect("perms").permissions();

    // 0o500 = read+execute, no write. atomic_write's rename into the
    // dir fails with PermissionDenied (mapped to VaultError::Io). The
    // guard's `Drop` restores perms whether the test panics or not, so
    // the tempdir's cleanup never trips on a locked-down parent.
    let perms_guard = PermsGuard {
        path: parent.clone(),
        original: original_perms,
    };
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o500)).expect("clamp perms");

    let result = writer.update_action_item(
        &path,
        &id_a,
        ActionItemPatch {
            done: Some(true),
            ..ActionItemPatch::default()
        },
    );

    let err = result.expect_err("write must fail when parent dir is read-only");
    match err {
        VaultError::Io(_) => {}
        other => panic!("expected VaultError::Io for EACCES, got {other:?}"),
    }

    // The on-disk note is the pre-patch content — no half-written
    // frontmatter from a partial atomic_write. The Drop guard restores
    // perms first; we read after that. We can't read while the dir is
    // 0o500 from within this test because the read path needs traverse
    // perms (which 0o500 retains) but the post-test fs::read also
    // benefits from perms being restored, so we drop the guard first.
    drop(perms_guard);
    let post = fs::read_to_string(&path).expect("read post");
    assert_eq!(post, pre, "failed write must not mutate the note on disk");
    let (fm, _) = read_note(&path).expect("re-parse");
    let row_a = fm.action_items.iter().find(|i| i.id == id_a).expect("a");
    assert!(!row_a.done, "frontmatter row must remain pre-patch");
}
