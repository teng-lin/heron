//! Markdown summary writer + atomic temp+rename to the Obsidian vault.
//!
//! Real summary-writer implementation arrives week 10 per
//! `docs/implementation.md` §12. The calendar bridge ships in week 1
//! per §5.4 because it doubles as the reference Swift bridge for every
//! other helper (`whisperkit-bridge`, `zoom-ax-backend`,
//! `keychain-helper`).

pub mod calendar;

pub use calendar::calendar_has_access;
