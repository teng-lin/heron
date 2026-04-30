//! Day 8-10 write-back: per-row patches against
//! `<vault>/<session_id>.md`'s `Frontmatter.action_items[].id` rows.
//!
//! Pairs with the read-render path Tier 0 #3 shipped (PR #177): the
//! Review tab's Actions list keys on `ActionItem.id` so React can flip
//! checkboxes / edit-chip state without re-rendering the surrounding
//! markdown editor. The write side preserves the same key — patches
//! address the row by `ItemId` and return the post-merge row to the
//! renderer so it can drop optimistic UI without a refetch.
//!
//! ## Why a dedicated module
//!
//! `notes.rs` is the markdown-file I/O surface (read/write the entire
//! `.md`); `meetings.rs` is the daemon HTTP proxy. Action-item writes
//! sit between the two: structured patches that target a specific
//! frontmatter row, atomically rewriting the on-disk note via
//! [`heron_vault::VaultWriter::update_action_item`]. Living next to
//! the existing files would conflate three concerns; a third module
//! keeps the Tauri command surface searchable.
//!
//! ## Wire shape
//!
//! `heron_update_action_item` mirrors the [JSON Merge Patch] (RFC 7396)
//! convention: an omitted field means "no change", `null` means "clear
//! this nullable field", and a value means "set". Frontend encodes
//! `{owner: null}` as a clear; the [`heron_vault::ActionItemPatch`]
//! double-option deserializer below distinguishes the two cases.
//!
//! Errors collapse into the existing [`String`] envelope per the rest
//! of the desktop Tauri command surface (see `notes.rs` /
//! `resummarize.rs`). The renderer surfaces the message in a Sonner
//! toast and drops the optimistic UI for that row. Concrete error
//! kinds:
//!
//! - **NotFound** — the meeting note has no row with that `ItemId`.
//!   Renders as "action item <id> not found in note frontmatter".
//! - **VaultLocked** — atomic write failed (iCloud eviction, perm
//!   error). The vault writer surfaces the OS error verbatim.
//! - **Validation** — the `meeting_id` / `item_id` failed to parse as
//!   a UUID, or the patch `due` field isn't ISO `YYYY-MM-DD`. We
//!   validate on the desktop side before reaching the writer so the
//!   wire surface is the canonical filter.
//!
//! [JSON Merge Patch]: https://datatracker.ietf.org/doc/html/rfc7396

use std::path::Path;
use std::str::FromStr;

use heron_types::ItemId;
use heron_vault::{ActionItemPatch, VaultError, VaultWriter};
use serde::{Deserialize, Serialize};

use crate::notes::resolve_note_path;

/// Wire shape for the post-patch action-item row. Mirrors the TS
/// `ActionItem` interface in `apps/desktop/src/lib/types.ts` —
/// `owner` and `due` are nullable strings (`null` for "no value"),
/// `done` is the new Day 8-10 boolean.
///
/// We don't return [`heron_types::ActionItem`] verbatim because that
/// struct has `owner: String` (empty-string-as-no-owner) — the wire
/// convention (mirrored in `heron_session::ActionItem`) is `Option<String>`
/// with `None` for "no owner". Mapping at the boundary keeps the
/// renderer's null-check single-shot and consistent with what
/// `heron_meeting_summary` already returns.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct ActionItemView {
    pub id: ItemId,
    pub text: String,
    pub owner: Option<String>,
    pub due: Option<String>,
    pub done: bool,
}

impl From<heron_types::ActionItem> for ActionItemView {
    fn from(item: heron_types::ActionItem) -> Self {
        Self {
            id: item.id,
            text: item.text,
            owner: (!item.owner.is_empty()).then_some(item.owner),
            due: item.due,
            done: item.done,
        }
    }
}

/// Validate the patch's `due` field (when present-and-set) parses as
/// ISO `YYYY-MM-DD`. We do this at the desktop boundary rather than
/// inside the vault writer because the writer is content-agnostic
/// about the string shape — any `Option<String>` round-trips through
/// the YAML cleanly. Surface the validation error here so the wire
/// envelope can render "validation: bad due format" instead of letting
/// a malformed date escape into the on-disk frontmatter.
fn validate_patch(patch: &ActionItemPatch) -> Result<(), String> {
    if let Some(Some(due)) = &patch.due
        && chrono::NaiveDate::parse_from_str(due, "%Y-%m-%d").is_err()
    {
        return Err(format!("validation: due `{due}` is not ISO YYYY-MM-DD"));
    }
    if let Some(text) = &patch.text
        && text.trim().is_empty()
    {
        return Err("validation: text must not be empty".to_string());
    }
    Ok(())
}

/// Apply [`patch`] to the action-item row identified by `item_id` in
/// `<vault>/meetings/<basename>.md`'s frontmatter (where `<basename>`
/// strips any `mtg_` wire prefix from `session_id`) and atomically
/// rewrite the note. Returns the post-merge row.
///
/// The function is the testable core; the `#[tauri::command]` shim in
/// `lib.rs` is a thin wrapper that threads renderer-supplied strings
/// into this signature.
pub async fn update_action_item(
    vault_path: &Path,
    session_id: &str,
    item_id: &str,
    patch: ActionItemPatch,
) -> Result<ActionItemView, String> {
    validate_patch(&patch)?;

    let parsed_item_id = ItemId::from_str(item_id)
        .map_err(|e| format!("validation: item_id `{item_id}` is not a UUID: {e}"))?;

    // Reuse the same path policy `notes::read_note` enforces — must
    // exist, must canonicalize inside the vault. The renderer can't
    // synthesize a path that escapes the vault.
    let note_path = resolve_note_path(vault_path, session_id, true).await?;

    // The vault writer is sync-only (it manipulates the filesystem
    // through std::fs). Hop to a blocking pool so the webview-bridge
    // thread doesn't stall on the read-mutate-write triple. The
    // writer's `vault_root` is unused by `update_action_item` — the
    // method addresses `meeting_path` directly — so we pass the
    // vault root we already have.
    let vault_root = vault_path.to_path_buf();
    let result = tokio::task::spawn_blocking(move || {
        VaultWriter::new(vault_root).update_action_item(&note_path, &parsed_item_id, patch)
    })
    .await
    .map_err(|e| format!("write task panicked: {e}"))?;

    result.map(ActionItemView::from).map_err(format_vault_error)
}

/// Map [`VaultError`] to the wire envelope strings the React tree
/// already keys on (Sonner toasts pattern-match on these prefixes).
/// Mirrors the `NotFound 404 / VaultLocked 423 / Validation 422` table
/// in the IPC contract spec — the prefixes are stable enough that the
/// renderer can branch without an enum.
fn format_vault_error(e: VaultError) -> String {
    match e {
        VaultError::ActionItemNotFound { id } => {
            format!("not_found: action item {id} not found in note frontmatter")
        }
        VaultError::Io(io) => format!("vault_locked: {io}"),
        VaultError::Yaml(y) => format!("validation: frontmatter yaml: {y}"),
        VaultError::MissingFrontmatter { path } => {
            format!(
                "validation: note has no frontmatter fence at {}",
                path.display()
            )
        }
        VaultError::UnterminatedFrontmatter { path } => format!(
            "validation: note has unterminated frontmatter at {}",
            path.display()
        ),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use heron_types::{
        ActionItem, Attendee, Cost, DiarizeSource, Disclosure, DisclosureHow, Frontmatter,
        MeetingType,
    };
    use heron_vault::VaultWriter;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn baseline_frontmatter() -> Frontmatter {
        Frontmatter {
            date: chrono::NaiveDate::from_ymd_opt(2026, 4, 24).expect("date"),
            start: "14:00".into(),
            duration_min: 47,
            company: Some("Acme".into()),
            attendees: vec![Attendee {
                id: uuid::Uuid::nil(),
                name: "Alice".into(),
                company: Some("Acme".into()),
            }],
            meeting_type: MeetingType::Client,
            source_app: "us.zoom.xos".into(),
            recording: PathBuf::from("recordings/x.m4a"),
            transcript: PathBuf::from("transcripts/x.jsonl"),
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
            tags: vec![],
            extra: serde_yaml::Mapping::default(),
        }
    }

    /// Lay down a finalized note with two action items at known IDs.
    fn seed_vault_note(tmp: &TempDir, session_id: &str) -> (PathBuf, ItemId, ItemId) {
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
                owner: "".into(),
                text: "Schedule kickoff".into(),
                due: None,
                done: false,
            },
        ];

        // Seed at `<vault>/meetings/<session_id>.md` to match the
        // shape `heron_vault::VaultWriter::finalize_with_pattern`
        // writes (and the shape `notes::resolve_note_path` resolves).
        let body = "\n## Action items\n\n- [ ] Send pricing deck\n- [ ] Schedule kickoff\n";
        let rendered = heron_vault::render_note(&fm, body).expect("render");
        let meetings = crate::notes::meetings_dir(tmp.path());
        std::fs::create_dir_all(&meetings).expect("mkdir meetings");
        let note_path = meetings.join(format!("{session_id}.md"));
        heron_vault::atomic_write(&note_path, rendered.as_bytes()).expect("seed note");
        let _ = VaultWriter::new(tmp.path()); // pin the import shape
        (note_path, id_a, id_b)
    }

    #[tokio::test]
    async fn update_action_item_done_round_trips_through_command() {
        let tmp = TempDir::new().expect("tmp");
        let (_path, id_a, id_b) = seed_vault_note(&tmp, "note");

        let view = update_action_item(
            tmp.path(),
            "note",
            &id_a.to_string(),
            ActionItemPatch {
                done: Some(true),
                ..ActionItemPatch::default()
            },
        )
        .await
        .expect("patch");

        assert_eq!(view.id, id_a);
        assert!(view.done);
        // Owner mapping: `alice` → Some("alice"). Empty string would
        // map to `None`; this row had a real owner so we expect Some.
        assert_eq!(view.owner.as_deref(), Some("alice"));
        assert_eq!(view.due.as_deref(), Some("2026-05-01"));

        // Sibling row untouched — read back via vault directly.
        let note_path = crate::notes::meetings_dir(tmp.path()).join("note.md");
        let (fm, _body) = heron_vault::read_note(&note_path).expect("read");
        let row_b = fm.action_items.iter().find(|i| i.id == id_b).expect("b");
        assert!(!row_b.done);
        assert_eq!(row_b.text, "Schedule kickoff");
    }

    #[tokio::test]
    async fn update_action_item_owner_clear_maps_empty_to_none_in_view() {
        let tmp = TempDir::new().expect("tmp");
        let (_path, id_a, _id_b) = seed_vault_note(&tmp, "note");

        let view = update_action_item(
            tmp.path(),
            "note",
            &id_a.to_string(),
            ActionItemPatch {
                owner: Some(None),
                ..ActionItemPatch::default()
            },
        )
        .await
        .expect("patch");

        // ActionItemView.owner is None when the on-disk owner is empty.
        assert!(
            view.owner.is_none(),
            "cleared owner must surface as null on the wire"
        );
    }

    #[tokio::test]
    async fn update_action_item_not_found_returns_not_found_envelope() {
        let tmp = TempDir::new().expect("tmp");
        let (_path, _id_a, _id_b) = seed_vault_note(&tmp, "note");

        let bogus = uuid::Uuid::now_v7();
        let err = update_action_item(
            tmp.path(),
            "note",
            &bogus.to_string(),
            ActionItemPatch {
                done: Some(true),
                ..ActionItemPatch::default()
            },
        )
        .await
        .expect_err("must error on missing id");
        assert!(
            err.starts_with("not_found: "),
            "wire envelope should prefix `not_found:`, got {err}",
        );
    }

    #[tokio::test]
    async fn update_action_item_validates_due_format() {
        let tmp = TempDir::new().expect("tmp");
        let (_path, id_a, _id_b) = seed_vault_note(&tmp, "note");

        let err = update_action_item(
            tmp.path(),
            "note",
            &id_a.to_string(),
            ActionItemPatch {
                due: Some(Some("tomorrow".into())),
                ..ActionItemPatch::default()
            },
        )
        .await
        .expect_err("must reject non-ISO due");
        assert!(err.starts_with("validation: "), "got {err}");
    }

    #[tokio::test]
    async fn update_action_item_validates_item_id_uuid() {
        let tmp = TempDir::new().expect("tmp");
        let (_path, _id_a, _id_b) = seed_vault_note(&tmp, "note");

        let err = update_action_item(tmp.path(), "note", "not-a-uuid", ActionItemPatch::default())
            .await
            .expect_err("must reject malformed item_id");
        assert!(err.starts_with("validation: "), "got {err}");
    }

    #[tokio::test]
    async fn update_action_item_rejects_empty_text() {
        let tmp = TempDir::new().expect("tmp");
        let (_path, id_a, _id_b) = seed_vault_note(&tmp, "note");

        let err = update_action_item(
            tmp.path(),
            "note",
            &id_a.to_string(),
            ActionItemPatch {
                text: Some("   ".into()),
                ..ActionItemPatch::default()
            },
        )
        .await
        .expect_err("must reject whitespace-only text");
        assert!(err.starts_with("validation: "), "got {err}");
    }
}
