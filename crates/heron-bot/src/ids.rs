//! Stripe-style prefixed IDs per spec §2 Invariant 4.
//!
//! Each ID kind (`BotId`, `PersonaId`, `MeetingId`, …) wraps a UUID
//! plus a compile-time prefix string. The wire form is `prefix_<uuid>`
//! (lower-case hyphenated UUID, like `bot_550e8400-e29b-41d4-a716-
//! 446655440000`). A misplaced ID fails at parse rather than flowing
//! through the system as a wrong-typed UUID.
//!
//! ## Design
//!
//! - Prefix is stored as a `&'static str` const on the type, not in
//!   the value. Two `BotId(uuid)` instances with the same UUID are
//!   equal regardless of how they were parsed; the prefix is purely
//!   for serialization + display + parse-time validation.
//! - `Serialize` writes the prefixed form. `Deserialize` rejects any
//!   string whose prefix doesn't match — so a JSON document carrying
//!   `"persona_..."` deserialized into a `BotId` field returns a
//!   serde error rather than installing a silently-wrong-typed UUID.
//! - `Display` writes the prefixed form (so `format!("{id}")` matches
//!   the wire shape). `FromStr` parses it.
//! - `Uuid::nil()` and zero-byte UUIDs are accepted at parse time —
//!   business logic rejecting them is the caller's responsibility.

use std::str::FromStr;

use thiserror::Error;
use uuid::Uuid;

/// Errors returned by [`FromStr`] for prefixed IDs. Carries the
/// expected prefix + the input (truncated for log safety) so a
/// misrouted parse surfaces a clear "this looked like a `persona_`
/// id but the receiving struct wanted a `bot_` id."
#[derive(Debug, Error, PartialEq, Eq)]
pub enum IdParseError {
    #[error("expected prefix {expected:?}_, got {actual:?} (input: {input:?})")]
    WrongPrefix {
        expected: &'static str,
        actual: String,
        input: String,
    },
    #[error("missing prefix separator '_' (input: {input:?})")]
    MissingSeparator { input: String },
    #[error("malformed UUID after prefix: {source}; input: {input:?}")]
    Uuid {
        #[source]
        source: uuid::Error,
        input: String,
    },
}

/// Maximum length we'll echo back into an [`IdParseError`]. Anything
/// over this gets truncated so a 10-MB hostile input doesn't flood
/// logs. The legitimate wire form is always under 64 bytes
/// (longest prefix ≈ 16 + `_` + 36-char UUID).
const ID_ECHO_LIMIT: usize = 80;

fn truncate_for_echo(s: &str) -> String {
    if s.len() <= ID_ECHO_LIMIT {
        s.to_owned()
    } else {
        let mut end = ID_ECHO_LIMIT;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

/// Generate the impl block for a Stripe-style prefixed ID type.
///
/// Each call produces:
/// - a `tuple struct $name(pub Uuid)` (the caller's existing shape)
/// - `pub const PREFIX: &str` on the type
/// - `Display`, `FromStr`, `Serialize`, `Deserialize` matching the
///   prefixed form `<prefix>_<uuid>`
/// - constructors `nil()` / `now_v7()` for ergonomic construction
///
/// Caller defines the type without serde derives. The macro stamps
/// custom serde impls so a misrouted JSON parse fails at the type.
#[macro_export]
macro_rules! prefixed_id {
    ($(#[$attr:meta])* $vis:vis $name:ident, $prefix:literal) => {
        $(#[$attr])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        $vis struct $name(pub ::uuid::Uuid);

        // Constructors below are part of the public surface; not
        // every consumer of every ID kind uses both, so suppress
        // the per-instantiation dead_code warning.
        #[allow(dead_code)]
        impl $name {
            /// Compile-time prefix for this ID kind. The wire form
            /// is `<PREFIX>_<lower-case-uuid>`.
            pub const PREFIX: &'static str = $prefix;

            /// All-zero UUID. Used as a sentinel in tests and as the
            /// default constructor for fixtures; production code
            /// should mint via [`Self::now_v7`].
            pub const fn nil() -> Self {
                Self(::uuid::Uuid::nil())
            }

            /// Mint a fresh UUIDv7. The v1 `heron_types::SessionId`
            /// is plain `uuid::Uuid`; this prefixed variant uses the
            /// same monotonic minting to keep IDs sortable.
            pub fn now_v7() -> Self {
                Self(::uuid::Uuid::now_v7())
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                write!(f, "{}_{}", Self::PREFIX, self.0.as_hyphenated())
            }
        }

        impl ::std::str::FromStr for $name {
            type Err = $crate::ids::IdParseError;
            fn from_str(s: &str) -> ::std::result::Result<Self, Self::Err> {
                $crate::ids::parse_prefixed(Self::PREFIX, s).map(Self)
            }
        }

        impl ::serde::Serialize for $name {
            fn serialize<S: ::serde::Serializer>(&self, s: S) -> ::std::result::Result<S::Ok, S::Error> {
                // Allocate once into a small stack-grown String. The
                // wire form is bounded at PREFIX_LEN + 1 + 36.
                s.serialize_str(&self.to_string())
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D: ::serde::Deserializer<'de>>(d: D) -> ::std::result::Result<Self, D::Error> {
                let s = <String as ::serde::Deserialize>::deserialize(d)?;
                <Self as ::std::str::FromStr>::from_str(&s)
                    .map_err(::serde::de::Error::custom)
            }
        }
    };
}

/// Parse `<expected_prefix>_<uuid>`. Shared core for the macro's
/// `FromStr` impls so the validation logic isn't duplicated per
/// type.
pub fn parse_prefixed(expected_prefix: &'static str, s: &str) -> Result<Uuid, IdParseError> {
    let Some((prefix, rest)) = s.split_once('_') else {
        return Err(IdParseError::MissingSeparator {
            input: truncate_for_echo(s),
        });
    };
    if prefix != expected_prefix {
        return Err(IdParseError::WrongPrefix {
            expected: expected_prefix,
            actual: truncate_for_echo(prefix),
            input: truncate_for_echo(s),
        });
    }
    Uuid::from_str(rest).map_err(|e| IdParseError::Uuid {
        source: e,
        input: truncate_for_echo(s),
    })
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    // Define a synthetic ID type local to the tests so we exercise
    // the macro itself rather than the public types.
    crate::prefixed_id!(pub TestId, "test");
    crate::prefixed_id!(pub OtherId, "other");

    #[test]
    fn display_emits_prefix_underscore_uuid() {
        let id = TestId(Uuid::from_u128(0x0123_4567_89ab_4def_8000_0000_0000_0001));
        let s = id.to_string();
        assert!(s.starts_with("test_"), "got: {s}");
        assert_eq!(
            s, "test_01234567-89ab-4def-8000-000000000001",
            "lower-case hyphenated UUID expected"
        );
    }

    #[test]
    fn from_str_round_trips_with_display() {
        let original = TestId::now_v7();
        let s = original.to_string();
        let parsed: TestId = s.parse().expect("parse round-trip");
        assert_eq!(original, parsed);
    }

    #[test]
    fn from_str_rejects_missing_underscore() {
        let err = "test1234".parse::<TestId>().expect_err("no separator");
        assert!(matches!(err, IdParseError::MissingSeparator { .. }));
    }

    #[test]
    fn from_str_rejects_wrong_prefix() {
        // Same UUID body, wrong prefix. Catches the cross-type mixup
        // bug Invariant 4 is designed to prevent.
        let other = OtherId::now_v7();
        let s = other.to_string();
        let err = s.parse::<TestId>().expect_err("cross-type misroute");
        match err {
            IdParseError::WrongPrefix {
                expected, actual, ..
            } => {
                assert_eq!(expected, "test");
                assert_eq!(actual, "other");
            }
            other => panic!("expected WrongPrefix, got {other:?}"),
        }
    }

    #[test]
    fn from_str_rejects_malformed_uuid() {
        let err = "test_not-a-real-uuid"
            .parse::<TestId>()
            .expect_err("bad uuid");
        assert!(matches!(err, IdParseError::Uuid { .. }));
    }

    #[test]
    fn serde_round_trips_via_json() {
        let id = TestId(Uuid::from_u128(0x0123_4567_89ab_4def_8000_0000_0000_0001));
        let json = serde_json::to_string(&id).expect("serialize");
        // serde_json wraps in quotes around the prefixed form.
        assert_eq!(json, r#""test_01234567-89ab-4def-8000-000000000001""#);
        let back: TestId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(id, back);
    }

    #[test]
    fn serde_rejects_wrong_prefix_with_clear_error() {
        // A JSON doc that says `"other_..."` deserialized into a
        // TestId field must fail rather than silently take the UUID.
        let bad = r#""other_01234567-89ab-4def-8000-000000000001""#;
        let err = serde_json::from_str::<TestId>(bad).expect_err("misroute");
        let msg = err.to_string();
        assert!(msg.contains("test"), "missing expected prefix: {msg}");
        assert!(msg.contains("other"), "missing actual prefix: {msg}");
    }

    #[test]
    fn nil_is_pinned_uuid_zero() {
        assert_eq!(TestId::nil().0, Uuid::nil());
    }

    #[test]
    fn now_v7_yields_distinct_ids() {
        let a = TestId::now_v7();
        let b = TestId::now_v7();
        assert_ne!(a, b, "v7 should mint distinct values");
    }

    #[test]
    fn truncate_for_echo_caps_oversize_input() {
        // A hostile 10-KB input shouldn't end up verbatim in the
        // error message. Anything past ID_ECHO_LIMIT gets truncated
        // with a `…` marker.
        let oversize = "x".repeat(ID_ECHO_LIMIT + 200);
        let err = oversize
            .parse::<TestId>()
            .expect_err("missing separator on huge input");
        let msg = err.to_string();
        assert!(msg.contains('…'), "missing truncation marker: {msg}");
        assert!(
            msg.len() < oversize.len(),
            "echoed message must be smaller than input"
        );
    }

    #[test]
    fn truncate_respects_utf8_boundaries() {
        // Multibyte char at the truncation boundary must not split
        // mid-codepoint.
        let mut payload = "x".repeat(ID_ECHO_LIMIT - 2);
        payload.push('🦀'); // 4-byte char straddling the cap
        payload.push_str("aaaa");
        let truncated = truncate_for_echo(&payload);
        // Either we caught the boundary cleanly (full crab visible)
        // or we backed off to the previous one (no crab); never a
        // half-crab.
        assert!(
            !truncated.contains('\u{FFFD}'),
            "truncation must not produce a replacement char"
        );
        assert!(truncated.ends_with('…'));
    }

    #[test]
    fn equality_ignores_prefix_only_compares_uuid() {
        // Two IDs with the same UUID body are equal regardless of
        // the type's prefix. (Documented design choice: prefix is
        // for serialization, not value-equality.)
        let body = Uuid::from_u128(42);
        assert_eq!(TestId(body), TestId(body));
        // (Different ID *types* don't even compile-compare; that's
        // the whole point of the macro.)
    }
}
