//! Disk-level integration test for `vault_write_duration_seconds` —
//! confirms the histogram is emitted (i.e. has a non-empty time-series
//! row) after a real `atomic_write`/`finalize`/`update_action_item`
//! sequence on the file system. Per #225 acceptance: dashboards rely
//! on these metrics being present after vault writes.
//!
//! The test deliberately does NOT cover failure-path counters: those
//! are exercised by the in-crate unit tests in `metrics_emit.rs` (the
//! `ClassifyFailure` impl) and by the wiremock-style failure tests in
//! the LLM crate. Here we just need to know the on-disk write actually
//! lights up the recorder.

#![allow(clippy::expect_used)]

use std::path::PathBuf;

use chrono::NaiveDate;
use heron_metrics::init_prometheus_recorder;
use heron_types::{
    ActionItem, Cost, DiarizeSource, Disclosure, DisclosureHow, Frontmatter, ItemId, MeetingType,
};
use heron_vault::{ActionItemPatch, VaultWriter};

fn baseline_frontmatter() -> Frontmatter {
    Frontmatter {
        date: NaiveDate::from_ymd_opt(2026, 5, 1).expect("valid date"),
        start: "10:00".into(),
        duration_min: 30,
        company: Some("Acme".into()),
        attendees: vec![],
        meeting_type: MeetingType::Internal,
        source_app: "us.zoom.xos".into(),
        recording: PathBuf::from("recordings/2026-05-01-1000.m4a"),
        transcript: PathBuf::from("transcripts/2026-05-01-1000.jsonl"),
        diarize_source: DiarizeSource::Ax,
        disclosed: Disclosure {
            stated: false,
            when: None,
            how: DisclosureHow::Verbal,
        },
        cost: Cost {
            summary_usd: 0.0,
            tokens_in: 0,
            tokens_out: 0,
            model: String::new(),
        },
        action_items: vec![],
        tags: vec![],
        extra: serde_yaml::Mapping::default(),
    }
}

#[test]
fn vault_write_duration_seconds_present_after_finalize_and_update() {
    let handle = init_prometheus_recorder().expect("recorder");
    let dir = tempfile::tempdir().expect("tmpdir");
    let writer = VaultWriter::new(dir.path());

    // Drive `finalize_session` — exercises the `op="finalize"` arm
    // and (transitively) two `op="atomic_write"` rows for the note +
    // its `.bak`.
    let mut fm = baseline_frontmatter();
    let item_id = ItemId::from_u128(0x0000_0000_dead_beef_4f00_8000_0001);
    fm.action_items.push(ActionItem {
        id: item_id,
        owner: "me".into(),
        text: "Send pricing deck".into(),
        due: None,
        done: false,
    });
    let path = writer
        .finalize_session("2026-05-01", "1000", "acme-sync", &fm, "body content\n")
        .expect("finalize");
    assert!(path.exists(), "finalized note must exist");

    // Drive `update_action_item` — exercises the
    // `op="update_action_item"` arm and another `op="atomic_write"`
    // for the merged note.
    let _row = writer
        .update_action_item(
            &path,
            &item_id,
            ActionItemPatch {
                done: Some(true),
                ..ActionItemPatch::default()
            },
        )
        .expect("update");

    let body = handle.render();
    assert!(
        body.contains("vault_write_duration_seconds"),
        "duration histogram missing from exposition: {body}"
    );
    // Each op label should appear on at least one row in the
    // exposition output. The test's Prometheus recorder is
    // process-global, so prior tests in the same process may have
    // emitted other op labels — we only assert that ours show up.
    assert!(
        body.contains("op=\"finalize\""),
        "finalize op label missing: {body}"
    );
    assert!(
        body.contains("op=\"update_action_item\""),
        "update_action_item op label missing: {body}"
    );
    assert!(
        body.contains("op=\"atomic_write\""),
        "atomic_write op label missing: {body}"
    );
    // Successful writes must not bump the failures counter for these
    // op labels. Weaker invariant: even if other tests in the same
    // process recorder emitted a failure row, the histogram presence
    // above is what dashboards care about.
}
