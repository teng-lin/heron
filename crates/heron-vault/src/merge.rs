//! Merge-on-write per [`docs/archives/implementation.md`](../../../docs/archives/implementation.md) §10
//! and [`docs/archives/merge-model.md`](../../../docs/archives/merge-model.md).
//!
//! The vault writer takes three inputs on every re-summarize:
//!
//! - **`base`** — the previous summary file's frontmatter+body, read
//!   from `<note>.md.bak`. Captures what the LLM previously wrote.
//! - **`ours`** — the current `<note>.md` on disk, possibly edited by
//!   the user since the last summarize.
//! - **`theirs`** — the fresh LLM output for this re-summarize.
//!
//! The merge follows the four-bucket ownership model in §10.2:
//!
//! | Bucket | Behavior |
//! |---|---|
//! | `heron_managed` | always overwritten by `theirs` |
//! | `llm_inferred`  | preserved if user edited `ours` vs `base`; else `theirs` wins |
//! | `user_owned`    | always preserved from `ours` (or `base` if `ours` is missing) |
//! | `body`          | preserved if user edited; else `theirs` wins |
//!
//! List fields (`action_items`, `attendees`) merge per stable
//! [`heron_types::ItemId`] — see [`merge_action_items`].

use std::collections::HashSet;

use heron_types::{ActionItem, Attendee, Frontmatter, ItemId};
use pulldown_cmark::{Parser, html::push_html};

/// Inputs to a single merge invocation.
///
/// Each side is borrowed; the merge produces an owned [`MergeOutcome`]
/// without mutating any input.
#[derive(Debug)]
pub struct MergeInputs<'a> {
    pub base: &'a Frontmatter,
    pub ours: &'a Frontmatter,
    pub theirs: &'a Frontmatter,
    pub base_body: &'a str,
    pub ours_body: &'a str,
    pub theirs_body: &'a str,
}

/// What the merge decided.
#[derive(Debug)]
pub struct MergeOutcome {
    pub frontmatter: Frontmatter,
    pub body: String,
}

/// Top-level merge entry point. Implements the §10.2 ownership model
/// for the full frontmatter and the §10.4 semantic-equality body
/// merge.
pub fn merge(inputs: MergeInputs<'_>) -> MergeOutcome {
    let MergeInputs {
        base,
        ours,
        theirs,
        base_body,
        ours_body,
        theirs_body,
    } = inputs;

    let action_items =
        merge_action_items(&base.action_items, &ours.action_items, &theirs.action_items);
    let attendees = merge_attendees(&base.attendees, &ours.attendees, &theirs.attendees);

    let frontmatter = Frontmatter {
        // heron_managed (always from theirs):
        date: theirs.date,
        start: theirs.start.clone(),
        duration_min: theirs.duration_min,
        source_app: theirs.source_app.clone(),
        recording: theirs.recording.clone(),
        transcript: theirs.transcript.clone(),
        diarize_source: theirs.diarize_source,
        disclosed: theirs.disclosed.clone(),
        cost: theirs.cost.clone(),

        // llm_inferred (theirs unless user edited):
        company: pick_llm_inferred(&base.company, &ours.company, &theirs.company),
        meeting_type: pick_llm_inferred_copy(
            base.meeting_type,
            ours.meeting_type,
            theirs.meeting_type,
        ),
        tags: pick_llm_inferred(&base.tags, &ours.tags, &theirs.tags),

        // list-merged via stable IDs:
        action_items,
        attendees,

        // user_owned (always preserved verbatim from ours):
        extra: ours.extra.clone(),
    };

    let body = if body_changed_semantically(base_body, ours_body) {
        ours_body.to_string()
    } else {
        theirs_body.to_string()
    };

    MergeOutcome { frontmatter, body }
}

/// Merge two list-of-action-items via stable [`ItemId`].
///
/// Rules for each id `k`:
///
/// | base | ours | theirs | result |
/// |---|---|---|---|
/// | yes | yes (= base) | yes | take `theirs` (LLM refresh, user untouched) |
/// | yes | yes (≠ base) | yes | take `ours` (user edited) |
/// | yes | yes  | no    | take `ours` (LLM dropped; keep user view) |
/// | yes | no   | yes   | drop (user deleted) |
/// | yes | no   | no    | drop (deletion converged) |
/// | no  | yes  | yes   | take `ours` (collision; user wins) |
/// | no  | yes  | no    | take `ours` (user added) |
/// | no  | no   | yes   | take `theirs` (LLM new) |
///
/// Output order: `theirs` order first (LLM-fresh items in the order
/// the LLM emitted them), then any `ours`-only items appended (user-
/// added or LLM-dropped) in their `ours` order.
pub fn merge_action_items(
    base: &[ActionItem],
    ours: &[ActionItem],
    theirs: &[ActionItem],
) -> Vec<ActionItem> {
    let base_by_id: std::collections::HashMap<ItemId, &ActionItem> =
        base.iter().map(|i| (i.id, i)).collect();
    let ours_by_id: std::collections::HashMap<ItemId, &ActionItem> =
        ours.iter().map(|i| (i.id, i)).collect();

    let mut emitted: HashSet<ItemId> = HashSet::new();
    let mut out = Vec::with_capacity(theirs.len() + ours.len());

    // First pass: walk theirs, choosing per-id source per the rules.
    //
    // `done` is a per-field exception: it always tracks `ours.done` when
    // an `ours` row exists, regardless of whether the rest of the row is
    // taken from `theirs` (LLM refresh) or `ours` (user edit). The user-
    // facing checkbox is the canonical state — re-summarize must never
    // reset a checked item just because the LLM polished its text.
    //
    // The `ours == base` user-edit detector compares text/owner/due
    // only; `done` is excluded so a user who *only* toggled the
    // checkbox still gets the LLM's text polish. See the
    // `merged_action_items_*_done` regression tests.
    for t in theirs {
        let chosen = match (base_by_id.get(&t.id), ours_by_id.get(&t.id)) {
            (Some(b), Some(o)) => {
                let mut row = if action_item_content_equal(o, b) {
                    // User untouched on text/owner/due; take theirs
                    // (LLM refresh).
                    t.clone()
                } else {
                    // User edited; ours wins on text/owner/due.
                    (*o).clone()
                };
                row.done = o.done;
                row
            }
            // collision: theirs has an id that's in ours but not base.
            // Treat as user-owned to avoid clobbering. ours.done already
            // applies because we're taking the entire `ours` row.
            (None, Some(o)) => (*o).clone(),
            // user deleted; skip.
            (Some(_), None) => continue,
            // genuinely new from LLM.
            (None, None) => t.clone(),
        };
        emitted.insert(t.id);
        out.push(chosen);
    }

    // Second pass: append items in ours that weren't already emitted
    // (LLM-dropped or user-added), preserving ours order.
    for o in ours {
        if emitted.contains(&o.id) {
            continue;
        }
        // not in theirs at all
        // - if in base: LLM dropped, user kept → preserve
        // - if not in base: user added → preserve
        out.push(o.clone());
        emitted.insert(o.id);
    }

    out
}

/// Same shape as [`merge_action_items`] for [`Attendee`] entries.
/// Attendee identity is the stable [`ItemId`]; ordering preserves the
/// `theirs` order then appends `ours`-only entries.
pub fn merge_attendees(base: &[Attendee], ours: &[Attendee], theirs: &[Attendee]) -> Vec<Attendee> {
    let base_by_id: std::collections::HashMap<ItemId, &Attendee> =
        base.iter().map(|a| (a.id, a)).collect();
    let ours_by_id: std::collections::HashMap<ItemId, &Attendee> =
        ours.iter().map(|a| (a.id, a)).collect();

    let mut emitted: HashSet<ItemId> = HashSet::new();
    let mut out = Vec::with_capacity(theirs.len() + ours.len());

    for t in theirs {
        let chosen = match (base_by_id.get(&t.id), ours_by_id.get(&t.id)) {
            (Some(b), Some(o)) => {
                if (*o) == *b {
                    t.clone()
                } else {
                    (*o).clone()
                }
            }
            (None, Some(o)) => (*o).clone(),
            (Some(_), None) => continue,
            (None, None) => t.clone(),
        };
        emitted.insert(t.id);
        out.push(chosen);
    }

    for o in ours {
        if emitted.contains(&o.id) {
            continue;
        }
        out.push(o.clone());
        emitted.insert(o.id);
    }

    out
}

/// `true` iff the user has materially edited the body (vs a `base`
/// snapshot from `.md.bak`). Per §10.4: prose-whitespace edits don't
/// count, but edits inside fenced code blocks do.
pub fn body_changed_semantically(base: &str, current: &str) -> bool {
    normalize_body(base) != normalize_body(current)
}

fn normalize_body(s: &str) -> String {
    // pulldown-cmark round-trip strips authoring-time whitespace from
    // prose but preserves bytes inside `<pre>...</pre>` (the HTML for
    // a fenced code block). We then collapse whitespace ONLY in the
    // prose portions.
    let parser = Parser::new(s);
    let mut html = String::new();
    push_html(&mut html, parser);
    normalize_outside_code_blocks(&html)
}

/// Collapse whitespace runs to single spaces in everything that isn't
/// between `<pre>` and `</pre>`. Code blocks pass through verbatim.
fn normalize_outside_code_blocks(html: &str) -> String {
    const PRE_OPEN: &str = "<pre>";
    const PRE_CLOSE: &str = "</pre>";

    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while !rest.is_empty() {
        match rest.find(PRE_OPEN) {
            Some(start) => {
                out.push_str(&collapse_ws(&rest[..start]));
                let after_open = &rest[start..];
                match after_open.find(PRE_CLOSE) {
                    Some(end_rel) => {
                        let end = end_rel + PRE_CLOSE.len();
                        // verbatim include the <pre>...</pre> block
                        out.push_str(&after_open[..end]);
                        rest = &after_open[end..];
                    }
                    None => {
                        // unmatched <pre>: keep verbatim, stop scanning
                        out.push_str(after_open);
                        break;
                    }
                }
            }
            None => {
                out.push_str(&collapse_ws(rest));
                break;
            }
        }
    }
    out
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// User-edit detector for [`ActionItem`] that ignores the `done` flag.
///
/// The list-merge in [`merge_action_items`] needs to ask "did the user
/// edit the textual content of this row?" without the answer flipping
/// just because the user also (or only) toggled the done checkbox.
/// Comparing the full `ActionItem` (which derives `PartialEq` over every
/// field including `done`) would conflate the two — a user who *only*
/// toggled `done` would be misclassified as having "edited" the row,
/// and would then lose any LLM text polish on the next re-summarize.
fn action_item_content_equal(a: &ActionItem, b: &ActionItem) -> bool {
    a.id == b.id && a.owner == b.owner && a.text == b.text && a.due == b.due
}

/// Pick `theirs` for an `llm_inferred` field if `ours == base`
/// (user untouched); otherwise keep `ours` (user edit).
fn pick_llm_inferred<T: Clone + PartialEq>(base: &T, ours: &T, theirs: &T) -> T {
    if ours == base {
        theirs.clone()
    } else {
        ours.clone()
    }
}

/// Same as [`pick_llm_inferred`] for `Copy` types so we avoid an
/// unnecessary `clone`.
fn pick_llm_inferred_copy<T: Copy + PartialEq>(base: T, ours: T, theirs: T) -> T {
    if ours == base { theirs } else { ours }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    /// Hand out distinct deterministic [`ItemId`]s without enabling
    /// the uuid crate's `v4` feature just for tests.
    fn next_id() -> ItemId {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        ItemId::from_u128(u128::from(COUNTER.fetch_add(1, Ordering::Relaxed)))
    }

    fn ai(id: ItemId, owner: &str, text: &str) -> ActionItem {
        ActionItem {
            id,
            owner: owner.into(),
            text: text.into(),
            due: None,
            done: false,
        }
    }

    #[test]
    fn case_2_user_edits_action_text_preserved() {
        let id = next_id();
        let base = vec![ai(id, "me", "Send pricing deck Friday")];
        let ours = vec![ai(id, "me", "Send pricing deck Monday")]; // user edit
        let theirs = vec![ai(id, "me", "Send pricing deck Friday")]; // LLM same as base

        let merged = merge_action_items(&base, &ours, &theirs);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].text, "Send pricing deck Monday");
    }

    #[test]
    fn case_3_user_deletion_persists() {
        let id = next_id();
        let base = vec![ai(id, "me", "Old item")];
        let ours: Vec<ActionItem> = vec![]; // user deleted
        let theirs = vec![ai(id, "me", "Old item")]; // LLM still has it

        let merged = merge_action_items(&base, &ours, &theirs);
        assert!(merged.is_empty(), "user deletion must persist");
    }

    #[test]
    fn case_4_llm_new_item_appears() {
        let id_existing = next_id();
        let id_new = next_id();
        let base = vec![ai(id_existing, "me", "Existing")];
        let ours = vec![ai(id_existing, "me", "Existing")];
        let theirs = vec![
            ai(id_existing, "me", "Existing"),
            ai(id_new, "alice", "Brand new"),
        ];

        let merged = merge_action_items(&base, &ours, &theirs);
        assert_eq!(merged.len(), 2);
        assert!(merged.iter().any(|i| i.text == "Brand new"));
    }

    #[test]
    fn case_8_user_untouched_text_lets_llm_win() {
        let id = next_id();
        let base = vec![ai(id, "me", "Old text")];
        let ours = vec![ai(id, "me", "Old text")]; // identical to base
        let theirs = vec![ai(id, "me", "Polished new text")]; // LLM rewrote

        let merged = merge_action_items(&base, &ours, &theirs);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].text, "Polished new text");
    }

    #[test]
    fn case_9_base_theirs_only_user_deleted_drops() {
        let id_kept = next_id();
        let id_deleted = next_id();
        let base = vec![ai(id_kept, "me", "Kept"), ai(id_deleted, "me", "Goner")];
        let ours = vec![ai(id_kept, "me", "Kept")]; // user deleted Goner
        let theirs = vec![ai(id_kept, "me", "Kept"), ai(id_deleted, "me", "Goner")];

        let merged = merge_action_items(&base, &ours, &theirs);
        assert_eq!(merged.len(), 1);
        assert!(merged.iter().all(|i| i.id != id_deleted));
    }

    #[test]
    fn user_added_item_with_no_base_or_theirs_is_kept() {
        let id_user = next_id();
        let base: Vec<ActionItem> = vec![];
        let ours = vec![ai(id_user, "me", "Note from user")];
        let theirs: Vec<ActionItem> = vec![];

        let merged = merge_action_items(&base, &ours, &theirs);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].id, id_user);
    }

    #[test]
    fn item_in_ours_but_not_theirs_when_present_in_base_keeps_ours() {
        // LLM dropped an item the user kept untouched. Keep it.
        let id = next_id();
        let base = vec![ai(id, "me", "Was here")];
        let ours = vec![ai(id, "me", "Was here")]; // identical
        let theirs: Vec<ActionItem> = vec![]; // LLM dropped

        let merged = merge_action_items(&base, &ours, &theirs);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].text, "Was here");
    }

    #[test]
    fn body_whitespace_only_edits_are_not_semantic() {
        let base = "Hello   world.\n\n\nNext  paragraph.";
        let current = "Hello world.\n\nNext paragraph.";
        assert!(!body_changed_semantically(base, current));
    }

    #[test]
    fn body_real_edit_is_semantic() {
        let base = "Hello world.";
        let current = "Hello, world!";
        assert!(body_changed_semantically(base, current));
    }

    #[test]
    fn body_code_block_whitespace_is_significant() {
        // Re-indenting code in a fenced block IS a semantic change.
        let base = "```\nfn main() {\n    println!(\"hi\");\n}\n```";
        let current = "```\nfn main() {\n  println!(\"hi\");\n}\n```";
        assert!(body_changed_semantically(base, current));
    }

    #[test]
    fn body_prose_around_code_block_collapses() {
        // Whitespace-only edits to the prose surrounding a code block
        // should not register, even when a code block is present.
        let base = "Intro.\n\n```\nlet x = 1;\n```\n\nOutro.";
        let current = "Intro.\n```\nlet x = 1;\n```\nOutro.";
        assert!(!body_changed_semantically(base, current));
    }

    #[test]
    fn merged_action_items_use_theirs_order_then_append_ours_only() {
        let id_a = next_id();
        let id_b = next_id();
        let id_c = next_id();
        let base = vec![ai(id_a, "me", "A")];
        let ours = vec![ai(id_a, "me", "A"), ai(id_c, "me", "C-user-added")];
        let theirs = vec![ai(id_b, "me", "B-llm-new"), ai(id_a, "me", "A")];

        let merged = merge_action_items(&base, &ours, &theirs);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0].id, id_b); // theirs first
        assert_eq!(merged[1].id, id_a); // theirs first
        assert_eq!(merged[2].id, id_c); // user-added appended
    }

    /// Helper: build an `ActionItem` with an explicit `done` flag so the
    /// regression tests below stay readable. The `ai()` helper above
    /// always sets `done: false`; merging needs both states.
    fn ai_done(id: ItemId, owner: &str, text: &str, done: bool) -> ActionItem {
        ActionItem {
            id,
            owner: owner.into(),
            text: text.into(),
            due: None,
            done,
        }
    }

    /// Day 8-10 write-back: the merge rule for `done` must prefer the
    /// `ours` value when present so a user-checked checkbox survives a
    /// re-summarize. The LLM never emits `done` (the summarizer prompt
    /// doesn't surface a "done" concept), but a forward-compat
    /// LLM-emitted value still loses to `ours` here so the user's
    /// in-app toggle is the canonical state.
    #[test]
    fn merged_action_items_prefer_ours_done_over_theirs() {
        let id = next_id();
        // Base says not-done. User checked the box (ours.done = true).
        // The LLM re-summarized and (forward-compat path) emits
        // theirs.done = false. The merge must keep ours.done = true.
        let base = vec![ai_done(id, "me", "Send pricing deck", false)];
        let ours = vec![ai_done(id, "me", "Send pricing deck", true)];
        let theirs = vec![ai_done(id, "me", "Send pricing deck", false)];

        let merged = merge_action_items(&base, &ours, &theirs);
        assert_eq!(merged.len(), 1);
        assert!(
            merged[0].done,
            "user-checked done flag must survive re-summarize",
        );
    }

    /// Mirror: when `ours` has the item but `done` is unchanged from
    /// `base`, the merge still uses `ours.done` (which equals `base`).
    /// This is the steady-state path — the user hasn't toggled the box,
    /// re-summarize doesn't suddenly invent a checked state.
    #[test]
    fn merged_action_items_keep_ours_done_when_unchanged() {
        let id = next_id();
        let base = vec![ai_done(id, "me", "Send pricing deck", false)];
        let ours = vec![ai_done(id, "me", "Send pricing deck", false)];
        let theirs = vec![ai_done(id, "me", "Polished pricing deck", false)];

        let merged = merge_action_items(&base, &ours, &theirs);
        assert_eq!(merged.len(), 1);
        // Text refresh: ours == base on text, so theirs wins.
        assert_eq!(merged[0].text, "Polished pricing deck");
        assert!(!merged[0].done);
    }

    /// Cross path: user toggled `done = true`, LLM polished the text in
    /// `theirs`. The merge must combine BOTH refreshes — adopt the
    /// polished text from `theirs` AND keep `done = true` from `ours`.
    /// Without the per-field `done` rule, this case would either lose
    /// the user's checkbox (if theirs wins outright) or lose the LLM
    /// polish (if ours wins outright, because `done` differs from base).
    #[test]
    fn merged_action_items_combine_theirs_text_with_ours_done() {
        let id = next_id();
        let base = vec![ai_done(id, "me", "Send pricing deck", false)];
        // User checked the box but didn't edit the text.
        let ours = vec![ai_done(id, "me", "Send pricing deck", true)];
        // LLM polished the text on the next re-summarize.
        let theirs = vec![ai_done(id, "me", "Send the pricing deck to Acme", false)];

        let merged = merge_action_items(&base, &ours, &theirs);
        assert_eq!(merged.len(), 1);
        // ours == base on (text, owner, due) — only `done` differs —
        // so the row body comes from theirs (LLM refresh wins on
        // text), but `done` tracks ours.
        assert_eq!(merged[0].text, "Send the pricing deck to Acme");
        assert!(
            merged[0].done,
            "ours.done = true must survive LLM text polish"
        );
    }

    /// LLM-only path: an item appears only in `theirs` (genuinely new
    /// from the LLM, no `ours` row). The merge takes `theirs.done`
    /// verbatim — there is no `ours.done` to prefer.
    #[test]
    fn merged_action_items_take_theirs_done_when_no_ours() {
        let id = next_id();
        let base: Vec<ActionItem> = vec![];
        let ours: Vec<ActionItem> = vec![];
        let theirs = vec![ai_done(id, "alice", "Brand new item", false)];

        let merged = merge_action_items(&base, &ours, &theirs);
        assert_eq!(merged.len(), 1);
        assert!(!merged[0].done);
    }

    #[test]
    fn user_added_attendee_survives() {
        let id_calendar = next_id();
        let id_walked_in = next_id();
        let base = vec![Attendee {
            id: id_calendar,
            name: "Alice".into(),
            company: Some("Acme".into()),
        }];
        let ours = vec![
            Attendee {
                id: id_calendar,
                name: "Alice".into(),
                company: Some("Acme".into()),
            },
            Attendee {
                id: id_walked_in,
                name: "Bob (walked in)".into(),
                company: None,
            },
        ];
        let theirs = base.clone();

        let merged = merge_attendees(&base, &ours, &theirs);
        assert_eq!(merged.len(), 2);
        assert!(merged.iter().any(|a| a.name == "Bob (walked in)"));
    }
}
