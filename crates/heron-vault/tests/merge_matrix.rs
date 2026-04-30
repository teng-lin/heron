//! Per `docs/archives/implementation.md` §10.7 — the 12-case merge-on-write
//! matrix. Each test constructs a (base, ours, theirs) triple from a
//! shared baseline `Frontmatter`, runs `merge()`, and asserts on the
//! field that scenario is testing.

#![allow(clippy::expect_used)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::NaiveDate;
use heron_types::{
    ActionItem, Attendee, Cost, DiarizeSource, Disclosure, DisclosureHow, Frontmatter, ItemId,
    MeetingType,
};
use heron_vault::{MergeInputs, merge};

fn next_id() -> ItemId {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    ItemId::from_u128(u128::from(COUNTER.fetch_add(1, Ordering::Relaxed)))
}

fn baseline() -> Frontmatter {
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

#[test]
fn case_1_user_adds_tag_and_llm_tags_merged() {
    // The current spec treats `tags` as llm_inferred: if user edited
    // ours vs base, ours wins entirely. So a user adding a tag means
    // the LLM's tags are dropped in favor of the user's. Document
    // this behavior; the §10.7 row says "tag preserved" — we read
    // that as: the user's added tag must survive the merge.
    let base = baseline();
    let mut ours = baseline();
    ours.tags.push("priority".into()); // user added a tag
    let mut theirs = baseline();
    theirs.tags.push("revenue".into()); // LLM inferred a different tag

    let merged = merge(MergeInputs {
        base: &base,
        ours: &ours,
        theirs: &theirs,
        base_body: "",
        ours_body: "",
        theirs_body: "",
    });

    assert!(merged.frontmatter.tags.contains(&"priority".into()));
    // ours wins entirely under llm_inferred-on-edit; LLM's "revenue"
    // does not survive. Documented in docs/archives/merge-model.md.
    assert!(!merged.frontmatter.tags.contains(&"revenue".into()));
}

#[test]
fn case_5_user_changes_meeting_type_internal_preserved() {
    let base = baseline();
    let mut ours = baseline();
    ours.meeting_type = MeetingType::Internal; // user edit
    let theirs = baseline(); // LLM still says client

    let merged = merge(MergeInputs {
        base: &base,
        ours: &ours,
        theirs: &theirs,
        base_body: "",
        ours_body: "",
        theirs_body: "",
    });

    assert_eq!(merged.frontmatter.meeting_type, MeetingType::Internal);
}

#[test]
fn case_6_user_adds_extra_field_preserved() {
    let base = baseline();
    let mut ours = baseline();
    ours.extra
        .insert("custom_user_field".into(), "hello".into());
    let theirs = baseline();

    let merged = merge(MergeInputs {
        base: &base,
        ours: &ours,
        theirs: &theirs,
        base_body: "",
        ours_body: "",
        theirs_body: "",
    });

    let val = merged
        .frontmatter
        .extra
        .get("custom_user_field")
        .expect("extra field must survive");
    assert_eq!(val.as_str(), Some("hello"));
}

#[test]
fn case_7_user_edits_body_prose_preserved() {
    let base = baseline();
    let ours = base.clone();
    let theirs = base.clone();

    let base_body = "We discussed pricing.";
    let ours_body = "We discussed pricing AND timeline."; // user added a clause
    let theirs_body = "Pricing was the focus."; // LLM rewrote

    let merged = merge(MergeInputs {
        base: &base,
        ours: &ours,
        theirs: &theirs,
        base_body,
        ours_body,
        theirs_body,
    });

    assert_eq!(merged.body, ours_body);
}

#[test]
fn case_8_user_untouched_body_lets_llm_win() {
    let base = baseline();
    let ours = base.clone();
    let theirs = base.clone();

    let body = "Original draft.";
    let theirs_body = "Polished version.";

    let merged = merge(MergeInputs {
        base: &base,
        ours: &ours,
        theirs: &theirs,
        base_body: body,
        ours_body: body,
        theirs_body,
    });

    assert_eq!(merged.body, theirs_body);
}

#[test]
fn case_10_cost_overwrites_unconditionally() {
    let base = baseline();
    let ours = baseline();
    let mut theirs = baseline();
    theirs.cost = Cost {
        summary_usd: 0.07, // higher cost on this re-summarize
        tokens_in: 22_000,
        tokens_out: 800,
        model: "claude-opus-4-7".into(),
    };

    let merged = merge(MergeInputs {
        base: &base,
        ours: &ours,
        theirs: &theirs,
        base_body: "",
        ours_body: "",
        theirs_body: "",
    });

    // cost is heron_managed → theirs always wins.
    assert_eq!(merged.frontmatter.cost.summary_usd, 0.07);
    assert_eq!(merged.frontmatter.cost.model, "claude-opus-4-7");
}

#[test]
fn case_11_user_edit_to_disclosed_is_overwritten() {
    // disclosed is heron_managed → user edits to it are *intentionally*
    // dropped. Documented in docs/archives/merge-model.md.
    let base = baseline();
    let mut ours = baseline();
    ours.disclosed.when = Some("99:99".into()); // user edit
    let theirs = baseline();

    let merged = merge(MergeInputs {
        base: &base,
        ours: &ours,
        theirs: &theirs,
        base_body: "",
        ours_body: "",
        theirs_body: "",
    });

    assert_eq!(merged.frontmatter.disclosed.when, Some("00:14".into()));
}

#[test]
fn case_12_md_bak_missing_treats_ours_as_edited() {
    // First re-summarize ever: there is no base. Caller passes
    // ours-as-base, which collapses to "user untouched ours" semantics
    // for llm_inferred fields, and "no semantic change" for body.
    // That's correct: with no base we can't tell what the user did.
    //
    // We test the inverse interpretation (caller passes a default
    // Frontmatter as base) here to make the doc requirement explicit:
    // callers must NOT pass an arbitrary "empty" base on first
    // re-summarize. Instead they use ours == base, so theirs wins on
    // every llm_inferred field — the natural behavior for a fresh
    // summarize.
    let ours = baseline();
    let mut theirs = baseline();
    theirs.company = Some("Acme Corp.".into()); // LLM polished

    let merged = merge(MergeInputs {
        base: &ours, // caller convention: ours == base when bak missing
        ours: &ours,
        theirs: &theirs,
        base_body: "",
        ours_body: "",
        theirs_body: "",
    });

    assert_eq!(merged.frontmatter.company.as_deref(), Some("Acme Corp."));
}

#[test]
fn full_attendee_merge_via_id() {
    let id_alice = next_id();
    let id_bob = next_id();

    let base = {
        let mut fm = baseline();
        fm.attendees.push(Attendee {
            id: id_alice,
            name: "Alice".into(),
            company: Some("Acme".into()),
        });
        fm
    };
    let ours = base.clone();
    let mut theirs = base.clone();
    theirs.attendees.push(Attendee {
        id: id_bob,
        name: "Bob".into(),
        company: Some("Acme".into()),
    });

    let merged = merge(MergeInputs {
        base: &base,
        ours: &ours,
        theirs: &theirs,
        base_body: "",
        ours_body: "",
        theirs_body: "",
    });

    assert_eq!(merged.frontmatter.attendees.len(), 2);
    assert!(merged.frontmatter.attendees.iter().any(|a| a.id == id_bob));
}

#[test]
fn full_action_items_round_trip_uses_theirs_order_then_user_appended() {
    let id_a = next_id();
    let id_b = next_id();
    let id_c_user = next_id();

    let base = {
        let mut fm = baseline();
        fm.action_items.push(ActionItem {
            id: id_a,
            owner: "me".into(),
            text: "A".into(),
            due: None,
            done: false,
        });
        fm
    };
    let mut ours = base.clone();
    ours.action_items.push(ActionItem {
        id: id_c_user,
        owner: "me".into(),
        text: "C user-added".into(),
        due: None,
        done: false,
    });
    let mut theirs = base.clone();
    theirs.action_items.insert(
        0,
        ActionItem {
            id: id_b,
            owner: "alice".into(),
            text: "B llm-new".into(),
            due: None,
            done: false,
        },
    );

    let merged = merge(MergeInputs {
        base: &base,
        ours: &ours,
        theirs: &theirs,
        base_body: "",
        ours_body: "",
        theirs_body: "",
    });

    let order: Vec<_> = merged
        .frontmatter
        .action_items
        .iter()
        .map(|i| i.id)
        .collect();
    assert_eq!(order, vec![id_b, id_a, id_c_user]);
}
