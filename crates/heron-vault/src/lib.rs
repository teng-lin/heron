//! Markdown summary writer + atomic temp+rename to the Obsidian vault.
//!
//! Real summary-writer implementation arrives week 10 per
//! `docs/implementation.md` §12. The calendar bridge ships in week 1
//! per §5.4 because it doubles as the reference Swift bridge for every
//! other helper (`whisperkit-bridge`, `zoom-ax-backend`,
//! `keychain-helper`).
//!
//! The [`merge`] module ships in week 8 (the merge-on-write spike,
//! §10) ahead of the writer so the LLM template work in §11.2 can
//! integrate against a stable type signature.

pub mod calendar;
pub mod merge;

pub use calendar::calendar_has_access;
pub use merge::{MergeInputs, MergeOutcome, merge, merge_action_items, merge_attendees};
