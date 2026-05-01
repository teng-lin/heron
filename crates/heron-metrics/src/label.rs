//! `RedactedLabel` — type-level enforcement of the metrics privacy
//! posture.
//!
//! See [`crate`]-level docs for the full rationale. Short version:
//! every metric label value flows through this newtype; it is **only
//! constructable** via the [`redacted!`] macro (compile-time
//! literal-only check), [`RedactedLabel::from_static`] (which only
//! accepts `&'static str` — the `'static` bound is what blocks the
//! `format!()` bypass), or [`RedactedLabel::hashed`] (which collapses
//! an arbitrary input to a stable 16-hex-char digest).
//!
//! Crucially, the inner `String` is **not pub**, and there is no
//! `From<String>`, `From<&str>` (for non-static), or `FromStr` impl.
//! A reviewer seeing
//!
//! ```ignore
//! redacted!("meeting-{id}")        // FAILS to compile (not a literal)
//! RedactedLabel::from_static(&id)  // FAILS to compile (not 'static)
//! ```
//!
//! has the foothold to reject the PR before it lands.
//!
//! The runtime checks in [`RedactedLabel::from_static`] (length cap,
//! charset whitelist) are belt-and-suspenders: even if a future
//! `&'static str` slipped past code review carrying a transcript-shaped
//! string, the constructor rejects it.

use std::fmt;

/// Maximum length of a label value. Prometheus exposition has no hard
/// cap, but a label value longer than this is almost always a sign
/// that someone is shoving a transcript / a path / a hash chain into
/// a metric — none of which should be there.
const MAX_LABEL_LEN: usize = 64;

/// Errors from [`RedactedLabel`] constructors. The runtime checks are
/// belt-and-suspenders; the primary defense is the type-level
/// constraint that you can't get into a constructor without a
/// literal or a `'static` reference.
#[derive(Debug, PartialEq, Eq)]
pub enum InvalidLabel {
    /// Label is empty (`""`).
    Empty,
    /// Label exceeds [`MAX_LABEL_LEN`] characters.
    TooLong { len: usize, max: usize },
    /// Label contains a character outside the allowed charset
    /// (`[a-zA-Z0-9_-]`). Spaces, dots, slashes, quotes, anything
    /// transcript-shaped is rejected here.
    DisallowedCharset { ch: char },
}

impl fmt::Display for InvalidLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("redacted-label is empty"),
            Self::TooLong { len, max } => {
                write!(f, "redacted-label too long ({len} > {max})")
            }
            Self::DisallowedCharset { ch } => {
                write!(
                    f,
                    "redacted-label contains disallowed character '{}' \
                     (allowed: a-z A-Z 0-9 _ -)",
                    ch.escape_debug()
                )
            }
        }
    }
}

impl std::error::Error for InvalidLabel {}

/// A metric label value that has cleared the redaction gate.
///
/// **Type invariant:** the inner string is one of:
///
/// - A `&'static str` literal that passed [`validate_charset`] +
///   [`validate_length`].
/// - A 16-hex-char digest produced by [`Self::hashed`].
///
/// There is no public `String` constructor, no `From<&str>` for
/// non-static references, and no `Display` proxy that takes
/// arbitrary input. A would-be bypasser writing
/// `RedactedLabel::from_static(format!(...).leak())` is leaking memory
/// per call AND visibly using `.leak()` — both are flags at PR
/// review.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RedactedLabel(String);

impl RedactedLabel {
    /// Construct from a `&'static str`. The `'static` bound is the
    /// key constraint — it blocks the obvious bypass:
    ///
    /// ```compile_fail
    /// # use heron_metrics::RedactedLabel;
    /// let s = format!("meeting-{}", 42);
    /// // FAILS: `&s` is not `&'static`.
    /// let _ = RedactedLabel::from_static(&s);
    /// ```
    ///
    /// At runtime the constructor still validates length + charset
    /// so a `static`-declared transcript-shaped string is also
    /// rejected.
    pub fn from_static(s: &'static str) -> Result<Self, InvalidLabel> {
        validate(s)?;
        Ok(Self(s.to_owned()))
    }

    /// Hash an arbitrary input to a stable 16-hex-character digest.
    /// Use when grouping by an opaque correlation id (a session id,
    /// a request id) is genuinely needed and the raw value is
    /// user-derived.
    ///
    /// The digest is FNV-1a 64-bit. Not cryptographic — the threat
    /// model is "don't leak the meeting title into a metric label,"
    /// not "don't let an attacker invert the digest." For
    /// preimage resistance, callers should hash a salt-prefixed
    /// input or use a real KDF; for our threat model the speed
    /// (zero deps, const-foldable) and stable 64-bit output win.
    pub fn hashed(input: &str) -> Self {
        let h = fnv1a_64(input.as_bytes());
        Self(format!("{h:016x}"))
    }

    /// Internal constructor used by the [`redacted!`] macro. The
    /// macro guarantees its argument is a string literal at
    /// compile time, and the runtime checks here are
    /// belt-and-suspenders.
    ///
    /// Not part of the public API surface; the macro is the public
    /// entry point.
    #[doc(hidden)]
    pub fn __from_macro_literal(s: &'static str) -> Result<Self, InvalidLabel> {
        Self::from_static(s)
    }

    /// Borrow the inner string for use as a metric label value.
    /// Returned as `&str` rather than the wrapped `String` so callers
    /// can't append / mutate.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume into the inner `String`. Use sparingly — most metric
    /// APIs accept `&str`. Provided so the `metrics` crate's
    /// `Cow<'static, str>`-flavored label APIs can take ownership
    /// when they need to.
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl AsRef<str> for RedactedLabel {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RedactedLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Construct a [`RedactedLabel`] from a string literal.
///
/// **Compile-time guarantees:**
///
/// - The argument MUST be a string literal. `redacted!(format!(...))`,
///   `redacted!(some_var)`, and `redacted!(s.as_str())` all fail to
///   compile — the macro pattern only matches `$lit:literal`.
/// - The literal must satisfy the runtime charset / length rules
///   (validated in `RedactedLabel::__from_macro_literal`); a literal
///   like `redacted!("hello world")` panics at startup the first time
///   the call site is reached. Tests catch this — see
///   `naming::tests`.
///
/// Use for low-cardinality dimensions like enum variants:
///
/// ```ignore
/// metrics::counter!(SMOKE_CAPTURE_STARTED_TOTAL,
///     "platform" => redacted!("zoom").as_str().to_owned(),
/// ).increment(1);
/// ```
#[macro_export]
macro_rules! redacted {
    ($lit:literal) => {{
        // The `:literal` matcher rejects anything that isn't a string
        // literal at parse time. The runtime validation below is
        // belt-and-suspenders — a literal like "foo bar" fails the
        // charset check and surfaces as a panic during the first call,
        // which the unit tests in this crate exercise.
        match $crate::RedactedLabel::__from_macro_literal($lit) {
            Ok(label) => label,
            Err(e) => panic!("redacted!() literal '{}' failed validation: {}", $lit, e),
        }
    }};
}

fn validate(s: &str) -> Result<(), InvalidLabel> {
    validate_length(s)?;
    validate_charset(s)?;
    Ok(())
}

fn validate_length(s: &str) -> Result<(), InvalidLabel> {
    if s.is_empty() {
        return Err(InvalidLabel::Empty);
    }
    if s.len() > MAX_LABEL_LEN {
        return Err(InvalidLabel::TooLong {
            len: s.len(),
            max: MAX_LABEL_LEN,
        });
    }
    Ok(())
}

fn validate_charset(s: &str) -> Result<(), InvalidLabel> {
    for ch in s.chars() {
        if !is_label_char(ch) {
            return Err(InvalidLabel::DisallowedCharset { ch });
        }
    }
    Ok(())
}

fn is_label_char(ch: char) -> bool {
    matches!(ch, 'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-')
}

/// Tiny FNV-1a 64-bit hash. Inlined rather than pulling a crate so
/// `heron-metrics` stays dep-light. Stable, deterministic, and
/// good enough for "produce a 16-hex-char dimension key from an
/// opaque user-derived id."
fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn from_static_accepts_simple_lowercase() {
        let label = RedactedLabel::from_static("zoom").expect("ok");
        assert_eq!(label.as_str(), "zoom");
    }

    #[test]
    fn from_static_accepts_underscore_and_hyphen() {
        assert!(RedactedLabel::from_static("google_meet").is_ok());
        assert!(RedactedLabel::from_static("v1-stable").is_ok());
    }

    #[test]
    fn from_static_rejects_empty() {
        assert_eq!(RedactedLabel::from_static(""), Err(InvalidLabel::Empty));
    }

    #[test]
    fn from_static_rejects_overlong_input() {
        // Pad to 65 chars — one over the cap. We can't easily build a
        // `&'static str` longer than the cap dynamically, so use a
        // const that exceeds it.
        const TOO_LONG: &str =
            "0123456789012345678901234567890123456789012345678901234567890123456789";
        let err = RedactedLabel::from_static(TOO_LONG).unwrap_err();
        assert!(matches!(err, InvalidLabel::TooLong { .. }));
    }

    #[test]
    fn from_static_rejects_transcript_shaped_input() {
        // A label containing whitespace + punctuation is exactly what
        // an accidental transcript-text label looks like. Charset
        // rejects.
        let err = RedactedLabel::from_static("Hello, attendee! Welcome.").unwrap_err();
        assert!(matches!(err, InvalidLabel::DisallowedCharset { .. }));
    }

    #[test]
    fn from_static_rejects_path_like_input() {
        // `/Users/alice/...` is a smell that the caller stuffed a
        // user-path-derived string into a label.
        let err = RedactedLabel::from_static("/Users/alice/heron-vault").unwrap_err();
        assert!(matches!(err, InvalidLabel::DisallowedCharset { ch: '/' }));
    }

    #[test]
    fn from_static_rejects_dot_and_at_signs() {
        // Email-shaped (alice@example.com) and id-shaped
        // (mtg.01931e62) inputs both die at the dot / at-sign.
        assert!(matches!(
            RedactedLabel::from_static("alice@example.com").unwrap_err(),
            InvalidLabel::DisallowedCharset { ch: '@' }
        ));
        assert!(matches!(
            RedactedLabel::from_static("mtg.01931e62").unwrap_err(),
            InvalidLabel::DisallowedCharset { ch: '.' }
        ));
    }

    #[test]
    fn redacted_macro_compiles_and_returns_label() {
        let label = redacted!("zoom");
        assert_eq!(label.as_str(), "zoom");
    }

    #[test]
    fn hashed_produces_stable_16_hex_digest() {
        let a = RedactedLabel::hashed("01931e62-7a9f-7c20-bcd1-1f7e5e8a4031");
        let b = RedactedLabel::hashed("01931e62-7a9f-7c20-bcd1-1f7e5e8a4031");
        assert_eq!(a, b, "deterministic for the same input");
        assert_eq!(a.as_str().len(), 16);
        assert!(a.as_str().chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn hashed_distinguishes_different_inputs() {
        let a = RedactedLabel::hashed("alice");
        let b = RedactedLabel::hashed("bob");
        assert_ne!(a, b);
    }

    #[test]
    fn hashed_passes_charset_validation() {
        // The hashed output is a charset-safe label. If a future change
        // to `hashed` drifts (uppercase hex, base64), the assertion
        // catches it.
        let h = RedactedLabel::hashed("anything");
        assert!(super::validate(h.as_str()).is_ok());
    }

    #[test]
    fn redaction_unit_test_for_acceptance_criterion() {
        // Acceptance item: "a privacy-redaction unit test asserts that
        // a label containing transcript-shaped text is rejected or
        // redacted." This exercise is the canonical assertion the
        // sub-issues will reference.
        const TRANSCRIPT_SHAPED: &str =
            "Hi everyone, thanks for joining today's standup. Alice will go first.";
        let err = RedactedLabel::from_static(TRANSCRIPT_SHAPED).unwrap_err();
        match err {
            InvalidLabel::DisallowedCharset { .. } | InvalidLabel::TooLong { .. } => {}
            other => panic!("unexpected error variant: {other:?}"),
        }

        // The escape hatch for genuinely-needed correlation: hash it.
        // The output is a charset-safe digest, never the original
        // text.
        let hashed = RedactedLabel::hashed(TRANSCRIPT_SHAPED);
        assert_ne!(hashed.as_str(), TRANSCRIPT_SHAPED);
        assert_eq!(hashed.as_str().len(), 16);
    }
}
