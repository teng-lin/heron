//! Markdown summary writer + atomic temp+rename to the Obsidian vault.
//!
//! - [`writer::VaultWriter`] — finalize a session into
//!   `<vault>/meetings/<date> <slug>.md` and re-summarize an existing
//!   note while preserving user edits via the §10 merge.
//! - [`merge`] — merge-on-write algorithm shipped in week 8 (the §10
//!   spike) ahead of the writer.
//! - [`calendar`] — EventKit Swift bridge from week 1 (§5.4); the
//!   reference shape every other Swift bridge follows.

pub mod calendar;
pub mod merge;
pub mod writer;

pub use calendar::calendar_has_access;
pub use merge::{MergeInputs, MergeOutcome, merge, merge_action_items, merge_attendees};
pub use writer::{VaultError, VaultWriter, atomic_write, read_note};
