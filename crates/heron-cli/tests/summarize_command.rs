//! Integration test for `heron summarize` — the orchestration shell
//! around [`heron_cli::summarize::re_summarize_in_vault`].
//!
//! Drives the full re-summarize flow with a capturing stub summarizer
//! so the test doesn't depend on `ANTHROPIC_API_KEY`, the `claude`
//! CLI, or network. What we lock down here is the wiring that
//! `cmd_summarize` provides on top of `Orchestrator::re_summarize_note`
//! + `VaultWriter::re_summarize`:
//!
//! - the vault-relative `transcript:` frontmatter resolves against the
//!   passed `vault_root` (a portability requirement — notes survive
//!   the user moving the vault),
//! - the §10.5 ID-preservation contract still holds end-to-end (the
//!   matcher rewrites LLM-minted UUIDs back to base IDs),
//! - the §10.3 merge runs, including `.md.bak` rotation that captures
//!   the **pre-merge** content (so a regression that wrote post-merge
//!   to `.bak` doesn't slip through),
//! - LLM-authoritative fields (tags, cost) reach the merged frontmatter,
//! - a user edit to the body wins over the LLM's fresh body, and
//! - LLM backend errors propagate to the user's terminal with a
//!   readable Display chain (no opaque error kind).

#![allow(clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::NaiveDate;
use heron_cli::summarize::{SummarizeError, re_summarize_in_vault};
use heron_llm::{LlmError, Summarizer, SummarizerInput, SummarizerOutput};
use heron_types::{
    ActionItem, Attendee, Cost, DiarizeSource, Disclosure, DisclosureHow, Frontmatter, ItemId,
    MeetingType,
};
use heron_vault::VaultWriter;
use tempfile::TempDir;

/// Capturing stub: records what the summarizer was called with and
/// returns a canned output. Mirrors the capture pattern used by
/// `session.rs`'s `CapturingSummarizer` test so the integration test
/// can assert on the bridge layer without a live LLM.
struct CapturingSummarizer {
    captured_transcript: Mutex<Option<PathBuf>>,
    captured_action_items: Mutex<Option<Vec<ActionItem>>>,
    captured_attendees: Mutex<Option<Vec<Attendee>>>,
    canned_output: Mutex<Option<SummarizerOutput>>,
}

#[async_trait]
impl Summarizer for CapturingSummarizer {
    async fn summarize(&self, input: SummarizerInput<'_>) -> Result<SummarizerOutput, LlmError> {
        *self.captured_transcript.lock().expect("lock") = Some(input.transcript.to_path_buf());
        *self.captured_action_items.lock().expect("lock") =
            input.existing_action_items.map(<[_]>::to_vec);
        *self.captured_attendees.lock().expect("lock") =
            input.existing_attendees.map(<[_]>::to_vec);
        self.canned_output
            .lock()
            .expect("lock")
            .take()
            .ok_or_else(|| LlmError::Backend("test fixture exhausted".into()))
    }
}

/// `<note>.md.bak` companion path: same string as the note path with
/// `.bak` appended (matches `heron_vault::writer::bak_path`'s shape).
fn bak_path_of(note_path: &Path) -> PathBuf {
    let mut s = note_path.as_os_str().to_owned();
    s.push(".bak");
    PathBuf::from(s)
}

/// Synthesize a finalized note + transcript fixture inside `vault_root`
/// and return the resulting `<note>.md` path. The transcript file is
/// written so `re_summarize_in_vault`'s `transcript.exists()`
/// preflight passes; its contents are never read by the stub
/// summarizer.
fn seed_vault_with_note(
    vault_root: &Path,
    prior_action: &ActionItem,
    prior_attendee: &Attendee,
) -> PathBuf {
    let writer = VaultWriter::new(vault_root);
    let frontmatter = Frontmatter {
        date: NaiveDate::from_ymd_opt(2026, 4, 24).expect("date"),
        start: "14:00".into(),
        duration_min: 47,
        company: Some("Acme".into()),
        attendees: vec![prior_attendee.clone()],
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
        action_items: vec![prior_action.clone()],
        tags: vec!["acme".into()],
        extra: serde_yaml::Mapping::default(),
    };
    let note_path = writer
        .finalize_session(
            "2026-04-24",
            "1400",
            "acme",
            &frontmatter,
            "Original body.\n",
        )
        .expect("finalize");

    // Materialize the transcript file so the existence preflight
    // passes. The CapturingSummarizer never reads it.
    let transcript_abs = vault_root.join(&frontmatter.transcript);
    if let Some(parent) = transcript_abs.parent() {
        std::fs::create_dir_all(parent).expect("transcript parent dir");
    }
    std::fs::write(&transcript_abs, b"").expect("transcript stub");

    note_path
}

fn alice_action(id: u128) -> ActionItem {
    ActionItem {
        id: ItemId::from_u128(id),
        owner: "alice".into(),
        text: "Send pricing deck to Acme".into(),
        due: None,
    }
}

fn alice_attendee(id: u128) -> Attendee {
    Attendee {
        id: ItemId::from_u128(id),
        name: "Alice".into(),
        company: Some("Acme".into()),
    }
}

#[tokio::test]
async fn re_summarize_threads_prior_items_and_writes_merged_frontmatter() {
    let tmp = TempDir::new().expect("tmp");
    let vault_root = tmp.path();

    let base_action_id = 0xA1;
    let prior_action = alice_action(base_action_id);
    let prior_attendee = alice_attendee(0xB1);

    let note_path = seed_vault_with_note(vault_root, &prior_action, &prior_attendee);

    // The "LLM" mints a fresh UUID for the same item — exactly the
    // failure mode §10.5's layer-2 matcher exists to fix.
    let canned = SummarizerOutput {
        body: "Polished body.\n".into(),
        company: Some("Acme".into()),
        meeting_type: MeetingType::Client,
        tags: vec!["acme".into(), "pricing".into()],
        action_items: vec![ActionItem {
            id: ItemId::from_u128(0xDEADBEEF),
            owner: "alice".into(),
            text: "Send the pricing deck to Acme".into(),
            due: None,
        }],
        attendees: vec![prior_attendee.clone()],
        cost: Cost {
            summary_usd: 0.05,
            tokens_in: 1000,
            tokens_out: 200,
            model: "claude-sonnet-4-6".into(),
        },
    };

    let summarizer = CapturingSummarizer {
        captured_transcript: Mutex::new(None),
        captured_action_items: Mutex::new(None),
        captured_attendees: Mutex::new(None),
        canned_output: Mutex::new(Some(canned)),
    };

    let outcome = re_summarize_in_vault(&summarizer, vault_root, &note_path)
        .await
        .expect("re_summarize_in_vault");

    // Assertion 1: the vault-relative `transcript:` resolved against
    // the vault root before reaching the summarizer.
    let captured_transcript = summarizer
        .captured_transcript
        .lock()
        .expect("lock")
        .clone()
        .expect("summarizer must have been invoked");
    assert_eq!(
        captured_transcript,
        vault_root.join("transcripts/2026-04-24-1400.jsonl"),
        "vault-relative frontmatter.transcript must be resolved against vault_root"
    );

    // Assertion 2: prior items reach the summarizer so §10.5 layer-1
    // (prompt-side preservation) can fire.
    let captured_actions = summarizer
        .captured_action_items
        .lock()
        .expect("lock")
        .clone()
        .expect("existing_action_items must be Some on a re-summarize");
    assert_eq!(captured_actions, vec![prior_action.clone()]);
    let captured_attendees = summarizer
        .captured_attendees
        .lock()
        .expect("lock")
        .clone()
        .expect("existing_attendees must be Some on a re-summarize");
    assert_eq!(captured_attendees, vec![prior_attendee.clone()]);

    // Assertion 3: §10.5 layer-2 matcher rewrote the LLM's minted ID
    // back to the base ID — the contract holds end-to-end through
    // the summarize wrapper, not just at the orchestrator boundary.
    assert_eq!(outcome.frontmatter.action_items.len(), 1);
    assert_eq!(
        outcome.frontmatter.action_items[0].id,
        ItemId::from_u128(base_action_id),
        "layer-2 matcher must rewrite the LLM-minted id back to the base id"
    );

    // Assertion 4: LLM-authoritative fields (tags + cost) reach the
    // merged frontmatter. A regression that drops them from the
    // `theirs_frontmatter` overlay would silently revert to the prior
    // note's values without this check.
    assert_eq!(
        outcome.frontmatter.tags,
        vec!["acme".to_string(), "pricing".to_string()],
        "LLM-refreshed tags must land in the merged frontmatter"
    );
    assert!(
        (outcome.frontmatter.cost.summary_usd - 0.05).abs() < 1e-9,
        "LLM cost must reach the merged frontmatter, got {}",
        outcome.frontmatter.cost.summary_usd
    );

    // Assertion 5: merged note + `.md.bak` rotation landed. The body
    // assertion is on the note (LLM body wins because the user did
    // not edit `Original body.` between summarizes).
    assert!(note_path.exists(), "note must still exist after merge");
    let bak = bak_path_of(&note_path);
    assert!(bak.exists(), ".md.bak rotation must have written");

    let on_disk = std::fs::read_to_string(&note_path).expect("read note");
    assert!(
        on_disk.contains("Polished body."),
        "merged note body must reflect the LLM's fresh output, got:\n{on_disk}"
    );
}

#[tokio::test]
async fn re_summarize_writes_pre_merge_content_to_bak() {
    // Lock down the §11.2 rotation contract: `.md.bak` must capture
    // the note's content from BEFORE this re-summarize ran, not the
    // post-merge content. If the rotation order ever flipped, the
    // first re-summarize would lose the pre-merge state and the next
    // re-summarize's three-way merge would treat `theirs` as `base`.
    let tmp = TempDir::new().expect("tmp");
    let vault_root = tmp.path();

    let prior_action = alice_action(0xA2);
    let prior_attendee = alice_attendee(0xB2);
    let note_path = seed_vault_with_note(vault_root, &prior_action, &prior_attendee);

    let canned = SummarizerOutput {
        body: "Polished body.\n".into(),
        company: Some("Acme".into()),
        meeting_type: MeetingType::Client,
        tags: vec!["acme".into()],
        action_items: vec![prior_action.clone()],
        attendees: vec![prior_attendee.clone()],
        cost: Cost {
            summary_usd: 0.01,
            tokens_in: 100,
            tokens_out: 20,
            model: "stub".into(),
        },
    };
    let summarizer = CapturingSummarizer {
        captured_transcript: Mutex::new(None),
        captured_action_items: Mutex::new(None),
        captured_attendees: Mutex::new(None),
        canned_output: Mutex::new(Some(canned)),
    };

    re_summarize_in_vault(&summarizer, vault_root, &note_path)
        .await
        .expect("re_summarize_in_vault");

    let bak = bak_path_of(&note_path);
    let bak_contents = std::fs::read_to_string(&bak).expect("read .bak");
    assert!(
        bak_contents.contains("Original body."),
        ".md.bak must capture the pre-merge note body, got:\n{bak_contents}"
    );
    assert!(
        !bak_contents.contains("Polished body."),
        ".md.bak must NOT contain the post-merge body, got:\n{bak_contents}"
    );
}

#[tokio::test]
async fn re_summarize_keeps_user_body_edit_over_llm_refresh() {
    // §10.4 contract: when the user has edited the note body since
    // the last summarize, the merge keeps the user's text. Without
    // this test, a regression that swapped `ours` and `theirs` would
    // silently overwrite the user's edits with the LLM's fresh body.
    let tmp = TempDir::new().expect("tmp");
    let vault_root = tmp.path();

    let prior_action = alice_action(0xA3);
    let prior_attendee = alice_attendee(0xB3);
    let note_path = seed_vault_with_note(vault_root, &prior_action, &prior_attendee);

    // Hand-edit the note's body. We rewrite the file by re-rendering
    // its frontmatter with a new body — replacing only the body chunk
    // would require reaching into private writer helpers. This is the
    // shape a user gets in Obsidian: edit body, save, leave
    // frontmatter untouched.
    let original = std::fs::read_to_string(&note_path).expect("read note");
    let edited = original.replace("Original body.", "User-edited body.");
    assert_ne!(original, edited, "test setup must mutate the body");
    std::fs::write(&note_path, &edited).expect("write edited note");

    let canned = SummarizerOutput {
        body: "Polished body.\n".into(),
        company: Some("Acme".into()),
        meeting_type: MeetingType::Client,
        tags: vec!["acme".into()],
        action_items: vec![prior_action.clone()],
        attendees: vec![prior_attendee.clone()],
        cost: Cost {
            summary_usd: 0.02,
            tokens_in: 200,
            tokens_out: 40,
            model: "stub".into(),
        },
    };
    let summarizer = CapturingSummarizer {
        captured_transcript: Mutex::new(None),
        captured_action_items: Mutex::new(None),
        captured_attendees: Mutex::new(None),
        canned_output: Mutex::new(Some(canned)),
    };

    re_summarize_in_vault(&summarizer, vault_root, &note_path)
        .await
        .expect("re_summarize_in_vault");

    let merged = std::fs::read_to_string(&note_path).expect("read merged");
    assert!(
        merged.contains("User-edited body."),
        "user's body edit must survive the LLM refresh, got:\n{merged}"
    );
    assert!(
        !merged.contains("Polished body."),
        "LLM body must not overwrite the user's edit, got:\n{merged}"
    );
}

#[tokio::test]
async fn re_summarize_propagates_llm_backend_error() {
    // Lock down the user-facing error chain when the LLM fails. The
    // user must see the backend's actual reason ("rate limited") in
    // the Display string, not just an opaque variant name. This is
    // the contract a CLI user relies on to debug a failed summarize.
    let tmp = TempDir::new().expect("tmp");
    let vault_root = tmp.path();

    let prior_action = alice_action(0xA4);
    let prior_attendee = alice_attendee(0xB4);
    let note_path = seed_vault_with_note(vault_root, &prior_action, &prior_attendee);

    struct AlwaysFails;
    #[async_trait]
    impl Summarizer for AlwaysFails {
        async fn summarize(
            &self,
            _input: SummarizerInput<'_>,
        ) -> Result<SummarizerOutput, LlmError> {
            Err(LlmError::Backend("rate limited".into()))
        }
    }

    let result = re_summarize_in_vault(&AlwaysFails, vault_root, &note_path).await;
    let err = result.expect_err("LLM error must surface as Err");
    assert!(
        matches!(err, SummarizeError::Session(_)),
        "LLM errors must travel through the orchestrator's SessionError seam, got: {err:?}"
    );
    let display_chain = format!("{err:#}");
    assert!(
        display_chain.contains("rate limited"),
        "Display chain must surface the backend's reason, got: {display_chain}"
    );
}

#[tokio::test]
async fn re_summarize_errors_when_transcript_missing() {
    let tmp = TempDir::new().expect("tmp");
    let vault_root = tmp.path();

    let prior_action = alice_action(0xA5);
    let prior_attendee = alice_attendee(0xB5);
    let note_path = seed_vault_with_note(vault_root, &prior_action, &prior_attendee);

    // Delete the transcript so the preflight fails. This is the
    // path-too-stale case: a user moves the recording but the note
    // still references the old transcript.
    let transcript_abs = vault_root.join("transcripts/2026-04-24-1400.jsonl");
    std::fs::remove_file(&transcript_abs).expect("remove transcript");

    // The summarizer must NOT be called when the transcript is
    // missing — failing fast saves the user an LLM token bill on a
    // request that would never produce a useful answer.
    struct MustNotCall;
    #[async_trait]
    impl Summarizer for MustNotCall {
        async fn summarize(
            &self,
            _input: SummarizerInput<'_>,
        ) -> Result<SummarizerOutput, LlmError> {
            panic!("summarizer must not be called when transcript is missing");
        }
    }

    let result = re_summarize_in_vault(&MustNotCall, vault_root, &note_path).await;
    match result {
        Err(SummarizeError::TranscriptMissing { resolved, fm_value }) => {
            assert_eq!(resolved, transcript_abs);
            assert_eq!(fm_value, PathBuf::from("transcripts/2026-04-24-1400.jsonl"));
        }
        other => panic!("expected TranscriptMissing, got {other:?}"),
    }
}
