//! Metric-name convention validator.
//!
//! Prometheus best-practice naming: snake_case, ASCII, with a unit
//! suffix. The validator runs at the call site (`metric_name!`) so a
//! drifted name fails to compile; it also runs at runtime in the
//! Prometheus exporter wiring as belt-and-suspenders.
//!
//! Rules:
//!
//! - Must be non-empty and ≤ 64 characters.
//! - Must match `[a-z][a-z0-9_]*` (lowercase, digits, underscores).
//! - Must end in one of:
//!   - `_total` (counters; the canonical `*_total` suffix)
//!   - `_seconds` / `_milliseconds` (latency histograms)
//!   - `_bytes` (size histograms / gauges)
//!   - `_count` (gauges holding a depth / queue length)
//!   - `_ratio` (gauges holding a 0..=1 ratio)
//!   - `_pending` (gauges holding a "things waiting for processing"
//!     count — e.g. `salvage_candidates_pending`)
//!   - `_info` (build-info and constant-1 metrics)

use std::fmt;

const MAX_METRIC_NAME_LEN: usize = 64;

/// Allowlisted unit suffixes. See module docs for rationale.
const VALID_SUFFIXES: &[&str] = &[
    "_total",
    "_seconds",
    "_milliseconds",
    "_bytes",
    "_count",
    "_ratio",
    "_pending",
    "_info",
];

/// Errors from [`validate_metric_name`]. `Display` includes the
/// metric name being rejected so a panic backtrace points at the
/// call site directly.
#[derive(Debug, PartialEq, Eq)]
pub enum InvalidMetricName {
    Empty,
    TooLong { len: usize, max: usize },
    DisallowedChar { ch: char },
    LeadsWithDigit,
    MissingUnitSuffix { name: String },
}

impl fmt::Display for InvalidMetricName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("metric name is empty"),
            Self::TooLong { len, max } => {
                write!(f, "metric name too long ({len} > {max})")
            }
            Self::DisallowedChar { ch } => {
                write!(
                    f,
                    "metric name contains disallowed character '{}' \
                     (allowed: a-z 0-9 _)",
                    ch.escape_debug()
                )
            }
            Self::LeadsWithDigit => f.write_str("metric name must start with a letter"),
            Self::MissingUnitSuffix { name } => write!(
                f,
                "metric name '{name}' is missing a recognized unit \
                 suffix (one of: _total, _seconds, _milliseconds, \
                 _bytes, _count, _ratio, _pending, _info)"
            ),
        }
    }
}

impl std::error::Error for InvalidMetricName {}

/// Validate a metric name against the convention. Used by both the
/// [`metric_name!`] macro (compile-foldable in `const fn` callers) and
/// by [`crate::recorder::register`] at runtime.
pub fn validate_metric_name(name: &str) -> Result<(), InvalidMetricName> {
    validate_charset(name)?;
    if !VALID_SUFFIXES.iter().any(|sfx| name.ends_with(sfx)) {
        return Err(InvalidMetricName::MissingUnitSuffix {
            name: name.to_owned(),
        });
    }
    Ok(())
}

/// Validate a histogram base name. Same charset / length / leading-letter
/// rules as [`validate_metric_name`] but the unit-suffix requirement is
/// dropped — Prometheus auto-appends `_bucket`, `_count`, `_sum` to the
/// base name when rendering histograms, so a name like
/// `vault_transcript_segments_count` would render as
/// `vault_transcript_segments_count_count` on the wire (the well-known
/// Prometheus naming anti-pattern). For dimensionless histograms a bare
/// plural noun is the correct shape.
///
/// Counters and gauges still go through [`validate_metric_name`] — their
/// suffix carries semantic meaning (`_total` ≠ `_pending` ≠ `_ratio`).
pub fn validate_histogram_base_name(name: &str) -> Result<(), InvalidMetricName> {
    validate_charset(name)
}

/// Shared charset / length / leading-letter checks. Both
/// [`validate_metric_name`] and [`validate_histogram_base_name`]
/// run this first.
fn validate_charset(name: &str) -> Result<(), InvalidMetricName> {
    if name.len() > MAX_METRIC_NAME_LEN {
        return Err(InvalidMetricName::TooLong {
            len: name.len(),
            max: MAX_METRIC_NAME_LEN,
        });
    }
    let mut chars = name.chars();
    // The `None` arm handles the empty-name case; no separate
    // `is_empty()` short-circuit needed.
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        Some(c) if c.is_ascii_digit() => return Err(InvalidMetricName::LeadsWithDigit),
        Some(c) => return Err(InvalidMetricName::DisallowedChar { ch: c }),
        None => return Err(InvalidMetricName::Empty),
    }
    for ch in chars {
        if !is_metric_name_char(ch) {
            return Err(InvalidMetricName::DisallowedChar { ch });
        }
    }
    Ok(())
}

fn is_metric_name_char(ch: char) -> bool {
    matches!(ch, 'a'..='z' | '0'..='9' | '_')
}

/// First-call-validated metric name. Wraps a string literal and
/// runs [`validate_metric_name`] the first time the call site is
/// reached, panicking on failure. A drifted name (`capture_started`,
/// `latency`, `LLMCallsTotal`) flunks the first unit test that
/// exercises the call site.
///
/// `validate_metric_name` is not `const fn` (the iterator chain
/// inside isn't const-stable on the workspace MSRV), so this macro
/// validates at first call rather than at compilation. Because
/// every metric is exercised by at least one unit test
/// (`tests::smoke_metric_name_passes_validation` in this module is
/// the canonical pattern), drift is caught in CI before reaching
/// production.
///
/// Bind the result to a `let` or `static`, NOT a `const`:
///
/// ```
/// # use heron_metrics::metric_name;
/// let name: &str = metric_name!("capture_started_total");
/// assert_eq!(name, "capture_started_total");
/// ```
#[macro_export]
macro_rules! metric_name {
    ($lit:literal) => {{
        // The `:literal` matcher rejects anything that isn't a
        // string literal at parse time — `metric_name!(some_var)`
        // and `metric_name!(format!(...))` both fail to compile.
        // The runtime validation below catches drifted literals.
        match $crate::validate_metric_name($lit) {
            Ok(()) => $lit as &str,
            Err(e) => panic!(
                "metric_name!() literal '{}' violates naming convention: {}",
                $lit, e
            ),
        }
    }};
}

/// First-call-validated histogram base name. Same first-call pattern
/// as [`metric_name!`] but routes through [`validate_histogram_base_name`]
/// — drops the unit-suffix requirement so dimensionless histogram
/// base names (e.g. `vault_transcript_segments`) are accepted.
///
/// Use this for histograms whose base name describes WHAT is measured
/// (a count, an event tally) rather than its unit. Histograms with a
/// natural unit (`*_seconds`, `*_bytes`, `*_milliseconds`) can use
/// either macro — both validators accept them.
///
/// Counters and gauges should NOT use this macro — their suffix
/// (`_total`, `_pending`, `_ratio`, etc.) is semantically required.
///
/// ```
/// # use heron_metrics::histogram_name;
/// let name: &str = histogram_name!("vault_transcript_segments");
/// assert_eq!(name, "vault_transcript_segments");
/// ```
#[macro_export]
macro_rules! histogram_name {
    ($lit:literal) => {{
        match $crate::validate_histogram_base_name($lit) {
            Ok(()) => $lit as &str,
            Err(e) => panic!(
                "histogram_name!() literal '{}' violates naming convention: {}",
                $lit, e
            ),
        }
    }};
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn accepts_canonical_counter_name() {
        assert!(validate_metric_name("capture_started_total").is_ok());
    }

    #[test]
    fn accepts_histogram_with_seconds_unit() {
        assert!(validate_metric_name("llm_call_duration_seconds").is_ok());
    }

    #[test]
    fn accepts_gauge_with_pending_suffix() {
        assert!(validate_metric_name("salvage_candidates_pending").is_ok());
    }

    #[test]
    fn rejects_empty_name() {
        assert_eq!(validate_metric_name(""), Err(InvalidMetricName::Empty));
    }

    #[test]
    fn rejects_uppercase() {
        assert!(matches!(
            validate_metric_name("CaptureStartedTotal"),
            Err(InvalidMetricName::DisallowedChar { ch: 'C' })
        ));
    }

    #[test]
    fn rejects_hyphen() {
        assert!(matches!(
            validate_metric_name("capture-started_total"),
            Err(InvalidMetricName::DisallowedChar { ch: '-' })
        ));
    }

    #[test]
    fn rejects_leading_digit() {
        assert_eq!(
            validate_metric_name("1_capture_started_total"),
            Err(InvalidMetricName::LeadsWithDigit)
        );
    }

    #[test]
    fn rejects_missing_unit_suffix() {
        match validate_metric_name("capture_started") {
            Err(InvalidMetricName::MissingUnitSuffix { name }) => {
                assert_eq!(name, "capture_started");
            }
            other => panic!("expected MissingUnitSuffix, got {other:?}"),
        }
    }

    #[test]
    fn rejects_dotted_name() {
        // Prometheus allows `.` in OTLP-bridged names but our
        // convention is snake_case ASCII; reject so we don't
        // produce mixed conventions.
        assert!(matches!(
            validate_metric_name("capture.started_total"),
            Err(InvalidMetricName::DisallowedChar { ch: '.' })
        ));
    }

    #[test]
    fn metric_name_macro_returns_validated_str() {
        let name: &str = metric_name!("capture_started_total");
        assert_eq!(name, "capture_started_total");
    }

    #[test]
    #[should_panic(expected = "violates naming convention")]
    fn metric_name_macro_panics_on_drifted_literal() {
        // A literal missing the unit suffix panics at first call.
        // The unit-test surface in the workspace is what catches
        // drifted call sites before they ship.
        let _ = metric_name!("capture_started");
    }

    #[test]
    fn smoke_metric_name_passes_validation() {
        // Cross-check: the `SMOKE_CAPTURE_STARTED_TOTAL` const in
        // `lib.rs` is the value sub-issues copy. If a future rename
        // breaks the convention, this test catches it.
        assert!(validate_metric_name(crate::SMOKE_CAPTURE_STARTED_TOTAL).is_ok());
    }

    #[test]
    fn histogram_base_name_accepts_unit_less_plural_noun() {
        // Issue #237: histograms whose base name describes what's
        // measured (not its unit) need a separate validator. Counters
        // and gauges still require a suffix via `validate_metric_name`.
        assert!(validate_histogram_base_name("vault_transcript_segments").is_ok());
    }

    #[test]
    fn histogram_base_name_still_enforces_charset() {
        assert!(matches!(
            validate_histogram_base_name("VaultTranscriptSegments"),
            Err(InvalidMetricName::DisallowedChar { ch: 'V' })
        ));
        assert_eq!(
            validate_histogram_base_name(""),
            Err(InvalidMetricName::Empty)
        );
        assert_eq!(
            validate_histogram_base_name("1_segments"),
            Err(InvalidMetricName::LeadsWithDigit)
        );
    }

    #[test]
    fn histogram_base_name_does_not_accept_counter_only_suffix() {
        // Negative-shape: `_total` is reserved for counters; a
        // histogram should not be named `*_total`. The validator
        // doesn't enforce this directly (it only rejects bad
        // charsets), but documenting via test that the function
        // accepts `*_total` (because charset is fine) — the
        // *call-site discipline* is what keeps counters and
        // histograms separate, not this validator.
        assert!(validate_histogram_base_name("capture_started_total").is_ok());
    }

    #[test]
    fn validate_metric_name_still_rejects_unit_less_name() {
        // Regression guard: the histogram-base-name carve-out must
        // NOT loosen the strict counter/gauge validator.
        match validate_metric_name("vault_transcript_segments") {
            Err(InvalidMetricName::MissingUnitSuffix { .. }) => {}
            other => panic!("expected MissingUnitSuffix, got {other:?}"),
        }
    }

    #[test]
    fn histogram_name_macro_returns_validated_str() {
        let name: &str = histogram_name!("vault_transcript_segments");
        assert_eq!(name, "vault_transcript_segments");
    }

    #[test]
    #[should_panic(expected = "violates naming convention")]
    fn histogram_name_macro_panics_on_drifted_literal() {
        // Charset rule still applies — capital letters are rejected.
        let _ = histogram_name!("VaultTranscriptSegments");
    }
}
