# Merge-on-write model

heron's vault writer is **merge-on-write**: every re-summarize takes
the user's current `<note>.md` (which may contain edits) and the
LLM's fresh output, and produces a new `<note>.md` that respects user
edits while pulling in new LLM-derived content.

This document is the single source of truth for which fields belong
to which side. Source: [`docs/archives/implementation.md`](implementation.md)
§10. Implementation: [`crates/heron-vault/src/merge.rs`](../../crates/heron-vault/src/merge.rs).

## Inputs

| Name | What it is |
|---|---|
| `base` | the previous summary's frontmatter+body, read from `<note>.md.bak` (rotated on every successful write) |
| `ours` | the current `<note>.md` on disk, possibly edited by the user since the last summarize |
| `theirs` | the fresh LLM output for this re-summarize |

A re-summarize call without a `.md.bak` (the very first re-summarize
on a freshly-shipped note, or after the file was rolled back from a
backup) sets `base = ours`, which collapses every llm_inferred-field
decision to "ours is unedited, theirs wins" — the natural behavior
for a re-run when we don't know what the user did. Body decisions
collapse the same way (no semantic delta means theirs wins).

## Field ownership

Every frontmatter field falls into one of four buckets.

### `heron_managed` — always overwritten by `theirs`

These come from the session machinery, not the LLM, and the writer
has the canonical value at re-summarize time:

- `date`, `start`, `duration_min`
- `source_app`
- `recording`, `transcript` (file paths)
- `diarize_source`
- `disclosed` (the {stated, when, how} block)
- `cost` (the {summary_usd, tokens_in, tokens_out, model} block)

User edits to these fields are intentionally **not preserved.** This
is documented behavior, not a bug: they reflect what heron did during
the session, and the writer's view is authoritative. (If you want a
note about it, write it in the body.)

### `llm_inferred` — preserved if user edited; else `theirs` wins

The LLM mints these from the transcript and they're commonly subject
to user correction:

- `company`
- `meeting_type`
- `tags`

The merge decision is: if `ours[field] == base[field]`, the user
didn't edit it, so let `theirs` overwrite. Otherwise, keep `ours`.

### `user_owned` — always preserved verbatim from `ours`

- `extra` — every field in the YAML frontmatter that heron's schema
  doesn't model. Custom user tags, plugin metadata, anything else.

### List fields — merged via stable IDs

`action_items` and `attendees` carry stable [`ItemId`](../../crates/heron-types/src/lib.rs)
values. Per-id rules:

| `base` | `ours` | `theirs` | result |
|---|---|---|---|
| has | has (= base) | has | **theirs wins** — LLM refresh on user-untouched item |
| has | has (≠ base) | has | **ours wins** — user edited |
| has | has | missing | **ours wins** — LLM dropped, user kept it |
| has | missing | has | **drop** — user deleted |
| has | missing | missing | **drop** — deletion converged |
| missing | has | has | **ours wins** — id collision; user version trumps |
| missing | has | missing | **ours wins** — user added |
| missing | missing | has | **theirs wins** — LLM produced a new item |

Output order: items in `theirs` order first (LLM-fresh items appear
in the order the LLM emitted them, typically meeting-flow), then
items present only in `ours` appended in their `ours` order.

## Body merge

The body is preserved if the user has made a **semantic** edit since
`base`. "Semantic" excludes whitespace-only edits in prose: re-flowing
paragraphs or collapsing double-spaces doesn't count. Edits inside
fenced code blocks, however, **always** count — re-indenting code is
intentional.

The check is in
[`merge::body_changed_semantically`](../../crates/heron-vault/src/merge.rs):

1. Run both sides through `pulldown_cmark::html::push_html`. This
   strips authoring-time whitespace from prose, but preserves bytes
   inside `<pre>...</pre>` (the HTML for fenced code blocks).
2. Collapse whitespace runs to single spaces in everything **except**
   `<pre>...</pre>` blocks (handled by
   `merge::normalize_outside_code_blocks`). Prose normalization,
   code-block-verbatim.
3. Compare the two normalized strings.

If they differ, the user has edited; keep `ours_body`. Otherwise,
take `theirs_body`.

## LLM ID-preservation contract

The list-merge logic above only behaves correctly when `theirs[i].id
== base[i].id` for items the LLM "kept the same." That's not free —
the LLM has to be told about prior IDs and asked to preserve them.

The summarizer template (`crates/heron-llm/templates/meeting.hbs`,
arrives week 9 per §11.2) embeds an `existing_action_items` block when
re-summarizing:

```handlebars
{{#if existing_action_items}}
The following action items were generated from a prior summary of
this meeting. Each has a stable `id`. **For items that you would
output again with the same meaning, RETURN THE EXACT SAME `id`.**
Mint a new `id` only for genuinely new items not in this list.

{{#each existing_action_items}}
- id: "{{id}}" | owner: {{owner}} | text: {{text}}
{{/each}}
{{/if}}
```

`heron-llm`'s parser validates: if a returned `id` doesn't match any
known ID and isn't a fresh UUIDv7, the item is treated as new and
gets a fresh server-side UUID.

**Empirical gate (week 8 day 3 work):** measure ID-preservation rate
against a 10-call fixture corpus. The §10.8 done-when bar is ≥80%. If
the rate is materially lower, fall back to a text-similarity matcher
(`strsim::levenshtein`) that resolves new LLM items to base items by
normalized-text distance — this is the v1.1 layer-2 fallback.

## What this model does NOT do

- **No three-way text merge of action item text.** If the user edits
  text and the LLM also rewrites the same item's text, ours wins.
  We don't try to reconcile mid-text edits.
- **No body sub-section merge.** Body is all-or-nothing: either ours
  or theirs. A future v1.1 may split the body into LLM-generated
  sections (summary, action items, decisions) and merge per-section,
  but v1 keeps it monolithic to avoid dragging in a structured-markdown
  layer.
- **No move detection.** If the user reorders action items, the new
  order is preserved (because ours wins on edited items), but the
  merge doesn't try to detect "this is the same item moved."
