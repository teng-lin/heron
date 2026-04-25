//! Re-export of the shared [`heron_types::prefixed_id!`] macro.
//!
//! Phase 48 moved the macro definition into `heron-types` so every
//! v2 crate (`heron-bot`, `heron-policy`, `heron-realtime`) reaches
//! it through one source rather than instantiating a per-crate copy.
//!
//! Re-exports `IdParseError` at the pre-phase-48 path
//! `heron_bot::ids::IdParseError` so existing call sites keep
//! working. `parse_prefixed` is *not* re-exported — it's an
//! internal helper the macro reaches via `$crate::prefixed_id::`,
//! never invoked by hand.

pub use heron_types::prefixed_id::IdParseError;
