# ADR 0001: Action-item deletion semantics in the IPC write-back path

- Status: Accepted
- Date: 2026-05-01
- Decider: Teng Lin
- Related: PR #180 (`feat(desktop): action-item write-back path`),
  issue #188 (`test(vault): RFC 7396 action-item integration tests`),
  `crates/heron-vault/src/writer.rs::update_action_item`,
  `crates/heron-vault/src/merge.rs::merge_action_items`,
  [merge model](../archives/merge-model.md)

## Context

The Day 8-10 write-back path (`heron_update_action_item`) takes an
[RFC 7396] JSON Merge Patch keyed by stable `ItemId`. RFC 7396 is
defined for JSON objects: "no change" is a missing field, "clear" is
`null`, "set" is the value. The spec is explicit that **arrays are
replaced wholesale** — there is no merge-patch primitive for "remove
this element of an array."

`Frontmatter.action_items` is an array of `ActionItem`. The wire shape
today (`ActionItemPatch`) addresses **one** row by id and patches its
fields. There is no "delete this whole row" verb.

A separate path already deletes rows by happenstance: `merge_action_items`
treats "row in `base`, not in `ours`" as user-deleted and drops the
LLM's `theirs` copy on next re-summarize. So a hand-edit that removes
a row from the YAML frontmatter does delete it. The open question is
specifically about **the IPC write-back path** — should the renderer
have a verb to delete from the editable list shown on the Review page?

The blocker on issue #188's integration test work is choosing one
shape so the deletion-related test cases can be written (or
deliberately not written) without ambiguity.

## Options

### A — Separate `heron_delete_action_item` RPC

Add a new Tauri command alongside `heron_update_action_item`:

```rust
#[tauri::command]
async fn heron_delete_action_item(
    meeting_id: MeetingId,
    item_id: ItemId,
) -> Result<(), String>;
```

Vault gets a sibling `VaultWriter::delete_action_item` that
read-modify-writes the frontmatter with the row spliced out, plus
best-effort body-bullet removal mirroring the existing text/done sync.

Pros:
- Clean verb separation; no semantic overload of "patch."
- Stays inside RFC 7396's letter (no array surgery in the patch shape).
- Pairs cleanly with the merge-side "row missing from `ours`" path.

Cons:
- Two IPC commands, two error envelopes, two optimistic-UI paths to
  keep in sync.
- The merge path already drops rows the user removes from `ours`, so
  the delete RPC would race against `re_summarize` the same way
  `update_action_item` does (last-writer-wins per `atomic_write`).

### B — No deletion verb. UI filters on `done == true`

Action items are append-only on the wire. Users mark items "done"
(the existing `ActionItemPatch.done = Some(true)`) and the renderer
filters them out of the active view. A "Show completed" toggle keeps
them visible for audit / undo.

Pros:
- One IPC command. One write path. No new vault API.
- Matches the lightweight task-tracker UX (Things, Reminders) where
  completed items archive rather than vanish.
- Aligns with the existing merge rule that `done` is per-row sticky:
  re-summarize never unchecks a user-checked item.
- Re-summarize self-heals the body bullet (`[ ]` → `[x]`) anyway, so
  the disk artifact stays meaningful even after the row scrolls out
  of the renderer.

Cons:
- The frontmatter array grows unbounded over the meeting's lifetime
  (the audit trail). A 50-bullet brainstorm meeting with most items
  closed still ships every row on every re-summarize prompt.
- Power users who want a clean list have to hand-edit the YAML, which
  triggers the merge path's user-delete branch. That's a feature, not
  a bug — but it's not discoverable.

### C — Tombstone field on the row (`deleted: true`)

Add `deleted: bool #[serde(default)]` to `ActionItem`. The patch shape
gains `deleted: Option<bool>`. Renderer hides tombstoned rows.
`merge_action_items` treats `deleted` as a per-row sticky field
identical to `done` (ours wins).

Pros:
- Reversible (set `deleted: false` to undelete) without consulting
  `<note>.md.bak`.
- Keeps the disk record for audit / litigation-hold.
- Patch-shape symmetry with `done`.

Cons:
- Two boolean closure states (`done`, `deleted`) on every row, with
  unclear precedence in the renderer.
- Frontmatter grows unbounded same as B, plus an extra field per row.
- `Frontmatter` is a wire type; adding a field is a (compatible)
  schema change every consumer has to grok.

## Decision

**B — No deletion verb in the IPC write-back path. The UI filters on
`done == true`.** A "Show completed" toggle keeps the audit trail
reachable.

The hand-edit deletion path (remove a row from the YAML, re-summarize
drops the LLM's copy) stays as the escape hatch for power users who
want a clean frontmatter — it's the same path the merge already
implements and tests. We do not add a `heron_delete_action_item` RPC,
and we do not add a tombstone field to `ActionItem`.

## Consequences

- `ActionItemPatch` stays at four fields (`text`, `owner`, `due`,
  `done`). No new `deleted` patch field, no new RPC.
- The integration tests added under
  `crates/heron-vault/tests/action_item_patch.rs` cover the three
  RFC 7396 per-field cases (omitted / null / value) plus
  hostile-YAML round-trip and concurrent writes. They do **not**
  cover deletion — there is no deletion API to test, and the merge-
  side user-delete path is already covered in
  `crates/heron-vault/src/merge.rs`'s unit tests.
- The Review page renderer (`apps/desktop/src/components/ActionItemsEditor.tsx`)
  filters out `done == true` rows by default in a follow-up PR. This
  ADR does not block the test PR; the renderer change ships
  separately so the failure mode of either side regressing is
  isolated.
- Frontmatter `action_items` arrays grow with meeting tenure. Re-
  summarize prompts include every row including completed ones.
  Acceptable for v1: meeting-scoped notes rarely accrue >20 rows.
  Revisit if a real meeting hits 100+ items and prompt cost matters.
- If a future product need (multi-meeting "Tasks" view, GDPR
  delete-on-request, litigation hold filter) makes B inadequate, the
  next ADR supersedes this one and adds the verb. Choosing B today
  does not foreclose A or C — it just defers them until they earn
  their complexity.

[RFC 7396]: https://datatracker.ietf.org/doc/html/rfc7396
