//! Layer-2 ID-preservation fallback per §10.5.
//!
//! `merge_action_items` (§10.3) assumes the LLM honored the prompt-side
//! `RETURN THE EXACT SAME id` instruction. When it does not — empirical
//! observation in week 8 will measure this — we need a fallback so a
//! re-summarize that minted fresh UUIDs for "obviously the same item"
//! still merges cleanly with the user's edits.
//!
//! The fallback is a normalized-text similarity matcher:
//!
//! 1. Normalize both `theirs` and `base` text (lowercase, collapse
//!    whitespace, strip leading bullets / checkboxes).
//! 2. For each `theirs` item lacking a base ID match, compute
//!    Levenshtein distance against every base item and pick the
//!    closest one, *if* the similarity score clears
//!    [`MIN_SIMILARITY`].
//! 3. Rewrite the `theirs` item to carry the matched base ID. The
//!    standard §10.3 merge then resolves it normally.
//!
//! Activated only when the §10.5 layer-1 (prompt-side preservation)
//! observed rate falls below the §10.5 floor — the surrounding
//! merge code never invokes this on the happy path.

use heron_types::{ActionItem, ItemId};

/// Similarity threshold below which a candidate is *not* considered a
/// match. 0.0 = identical text, 1.0 = no overlap. The §10.5 spec
/// suggests starting around 0.30 (i.e. 70 % matching characters); we
/// pin a slightly tighter 0.25 so noise on a 4-word action item
/// doesn't mis-merge.
pub const MIN_SIMILARITY: f64 = 0.25;

/// Result of one attempted match.
#[derive(Debug, Clone, PartialEq)]
pub struct LayerTwoMatch {
    /// Index into the input `theirs` slice the match was for.
    pub theirs_index: usize,
    /// Base ID we believe `theirs[theirs_index]` is "the same item" as.
    pub matched_base_id: ItemId,
    /// Distance score in [0.0, 1.0]. Lower = closer match.
    pub distance: f64,
}

/// Try to resolve LLM-minted IDs back to base IDs by text similarity.
///
/// Walks every `theirs[i]` whose `id` is **not already present** in
/// `base`. For each, finds the best base candidate by normalized
/// Levenshtein distance. Returns one [`LayerTwoMatch`] per resolved
/// item, in `theirs` order, dropping candidates above [`MIN_SIMILARITY`].
///
/// Each base item is matched at most once — the function picks a 1:1
/// resolution greedily, so two `theirs` items can't both map to the
/// same base item (avoids a duplicate-id crash in the standard merge).
pub fn match_action_items_by_text(
    base: &[ActionItem],
    theirs: &[ActionItem],
) -> Vec<LayerTwoMatch> {
    use std::collections::HashSet;

    let base_ids: HashSet<ItemId> = base.iter().map(|a| a.id).collect();
    let mut consumed: HashSet<ItemId> = HashSet::new();
    let mut out = Vec::new();

    for (i, t) in theirs.iter().enumerate() {
        // Skip items the LLM already preserved correctly — those
        // resolve via the standard §10.3 path.
        if base_ids.contains(&t.id) {
            continue;
        }
        let t_norm = normalize(&t.text);
        let mut best: Option<(f64, &ActionItem)> = None;
        for b in base {
            if consumed.contains(&b.id) {
                continue;
            }
            let b_norm = normalize(&b.text);
            let dist = normalized_levenshtein(&t_norm, &b_norm);
            match best {
                None => best = Some((dist, b)),
                Some((cur, _)) if dist < cur => best = Some((dist, b)),
                _ => {}
            }
        }
        if let Some((d, b)) = best
            && d < MIN_SIMILARITY
        {
            consumed.insert(b.id);
            out.push(LayerTwoMatch {
                theirs_index: i,
                matched_base_id: b.id,
                distance: d,
            });
        }
    }
    out
}

/// Apply layer-2 matches in place: rewrite each matched `theirs` item's
/// `id` to the resolved base ID.
///
/// Returns the count of rewrites applied so callers can record the
/// layer-2 hit rate in the diagnostics tab.
pub fn apply_matches(theirs: &mut [ActionItem], matches: &[LayerTwoMatch]) -> usize {
    for m in matches {
        if let Some(item) = theirs.get_mut(m.theirs_index) {
            item.id = m.matched_base_id;
        }
    }
    matches.len()
}

/// Lowercase, trim, drop leading list markers / checkboxes, collapse
/// internal whitespace runs to single spaces.
fn normalize(s: &str) -> String {
    let trimmed = s.trim();
    // Strip a leading bullet ("- ", "* ") + checkbox ("[ ] ", "[x] ").
    let stripped = trim_list_marker(trimmed);
    let lowered = stripped.to_lowercase();
    collapse_whitespace(&lowered)
}

fn trim_list_marker(s: &str) -> &str {
    let s = s
        .strip_prefix("- ")
        .or_else(|| s.strip_prefix("* "))
        .unwrap_or(s);
    s.strip_prefix("[ ] ")
        .or_else(|| s.strip_prefix("[x] "))
        .or_else(|| s.strip_prefix("[X] "))
        .unwrap_or(s)
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = true;
    for c in s.chars() {
        if c.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(c);
            last_space = false;
        }
    }
    out.trim_end().to_owned()
}

/// Normalized Levenshtein distance in `[0.0, 1.0]`. 0.0 = identical,
/// 1.0 = nothing in common. Implemented with a single-row buffer, no
/// allocations beyond the buffer itself, so the §10.5 fallback path
/// stays cheap even on hundreds of action items.
fn normalized_levenshtein(a: &str, b: &str) -> f64 {
    if a == b {
        return 0.0;
    }
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let max_len = a_chars.len().max(b_chars.len());
    if max_len == 0 {
        return 0.0;
    }
    let dist = levenshtein_chars(&a_chars, &b_chars);
    dist as f64 / max_len as f64
}

fn levenshtein_chars(a: &[char], b: &[char]) -> usize {
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let m = b.len();
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut curr = vec![0usize; m + 1];
    for (i, ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            curr[j + 1] = (curr[j] + 1).min(prev[j + 1] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[m]
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use heron_types::ActionItem;

    fn item(id_seed: u128, text: &str) -> ActionItem {
        ActionItem {
            id: ItemId::from_u128(id_seed),
            owner: "alice".to_owned(),
            text: text.to_owned(),
            due: None,
        }
    }

    #[test]
    fn identical_text_distance_is_zero() {
        assert!(normalized_levenshtein("hello", "hello") < 1e-9);
    }

    #[test]
    fn disjoint_text_distance_is_one() {
        let d = normalized_levenshtein("abc", "xyz");
        assert!((d - 1.0).abs() < 1e-9);
    }

    #[test]
    fn normalize_collapses_whitespace_and_lowercases() {
        assert_eq!(normalize("  Hello   World  "), "hello world");
        assert_eq!(normalize("- [ ] Send PRICING deck"), "send pricing deck");
        assert_eq!(normalize("* [x] FOLLOW UP with bob"), "follow up with bob");
    }

    #[test]
    fn match_by_text_resolves_minted_id_to_base() {
        let base_id = ItemId::from_u128(1);
        let base = vec![item(1, "Send pricing deck to Acme")];
        // LLM regenerated the same item with a fresh UUID.
        let theirs = vec![item(99, "Send the pricing deck to Acme")];

        let matches = match_action_items_by_text(&base, &theirs);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].theirs_index, 0);
        assert_eq!(matches[0].matched_base_id, base_id);
    }

    #[test]
    fn match_skips_items_with_already_matching_id() {
        let base = vec![item(1, "X"), item(2, "Y")];
        let theirs = vec![item(2, "Y'")]; // id matches; layer-1 handled
        let matches = match_action_items_by_text(&base, &theirs);
        assert!(matches.is_empty(), "layer-2 must not re-match layer-1 hits");
    }

    #[test]
    fn match_drops_too_distant_candidates() {
        let base = vec![item(1, "Send pricing deck")];
        let theirs = vec![item(99, "Schedule a follow-up demo for next quarter")];
        let matches = match_action_items_by_text(&base, &theirs);
        assert!(
            matches.is_empty(),
            "items above MIN_SIMILARITY must not match: got {matches:?}"
        );
    }

    #[test]
    fn match_picks_best_unconsumed_candidate() {
        let base = vec![
            item(1, "Send pricing deck"),
            item(2, "Schedule onboarding call"),
        ];
        let theirs = vec![
            item(99, "Send pricing deck v2"),
            item(98, "Schedule onboarding call now"),
        ];
        let matches = match_action_items_by_text(&base, &theirs);
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].theirs_index, 0);
        assert_eq!(matches[0].matched_base_id, ItemId::from_u128(1));
        assert_eq!(matches[1].theirs_index, 1);
        assert_eq!(matches[1].matched_base_id, ItemId::from_u128(2));
    }

    #[test]
    fn match_does_not_double_consume_a_base_item() {
        // Two near-identical theirs items; only one should map to the
        // single base item — the other gets nothing.
        let base = vec![item(1, "Follow up")];
        let theirs = vec![item(99, "Follow up"), item(98, "Follow up please")];
        let matches = match_action_items_by_text(&base, &theirs);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].theirs_index, 0);
    }

    #[test]
    fn apply_matches_rewrites_ids_in_place() {
        let base_id = ItemId::from_u128(1);
        let mut theirs = vec![item(99, "Send pricing deck")];
        let matches = vec![LayerTwoMatch {
            theirs_index: 0,
            matched_base_id: base_id,
            distance: 0.05,
        }];
        let count = apply_matches(&mut theirs, &matches);
        assert_eq!(count, 1);
        assert_eq!(theirs[0].id, base_id);
    }

    #[test]
    fn apply_matches_silently_skips_out_of_bounds_indices() {
        let mut theirs = vec![item(99, "x")];
        let matches = vec![LayerTwoMatch {
            theirs_index: 42,
            matched_base_id: ItemId::from_u128(1),
            distance: 0.0,
        }];
        // No panic; nothing happens.
        let count = apply_matches(&mut theirs, &matches);
        assert_eq!(count, 1);
        assert_eq!(theirs[0].id, ItemId::from_u128(99));
    }
}
