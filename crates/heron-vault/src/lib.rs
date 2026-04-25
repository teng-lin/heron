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
pub mod encode;
pub mod id_match;
pub mod merge;
pub mod purge;
pub mod validate;
pub mod writer;

pub use calendar::{
    CalendarAttendee, CalendarError, CalendarEvent, calendar_has_access, calendar_read_one_shot,
    epoch_seconds_to_utc,
};
pub use encode::{EncodeError, encode_to_m4a, verify_m4a};
pub use id_match::{LayerTwoMatch, MIN_SIMILARITY, apply_matches, match_action_items_by_text};
pub use merge::{MergeInputs, MergeOutcome, merge, merge_action_items, merge_attendees};
pub use purge::{PurgeOutcome, purge_after_verify};
pub use validate::{Issue, validate_vault};
pub use writer::{VaultError, VaultWriter, atomic_write, read_note};
