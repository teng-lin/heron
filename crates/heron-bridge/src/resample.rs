//! Linear-interpolation PCM resampler.
//!
//! The bridge speaks 16 kHz mono i16 internally (see [`crate::pcm`]),
//! but drivers hand us audio at whatever rate the platform gives them
//! — 48 kHz from CoreAudio / WASAPI, 44.1 kHz from some browsers,
//! 24 kHz from a few realtime-backend playback paths. Resampling on
//! the way in (driver → bridge) and out (bridge → driver) is the
//! bridge's job.
//!
//! ## Why linear, not sinc
//!
//! Linear interpolation has audible alias for music; for speech in
//! the 100–4000 Hz band it's perfectly adequate, and it's branch-free,
//! allocation-free per sample, and trivially testable. The realtime
//! backends already apply their own anti-aliasing on the output side,
//! so a band-limited resampler here would be redundant cost. If the
//! product ever ships music or wideband audio, swap in `rubato` —
//! the [`resample_linear`] signature is what the rest of the bridge
//! depends on.
//!
//! ## Invariants
//!
//! - Empty input → empty output. No panic, no allocation.
//! - `from_hz == to_hz` → input is cloned, no interpolation cost.
//! - `from_hz == 0` → empty output. A degenerate caller bug, but
//!   panicking on the bridge's hot path would take down the meeting.
//! - Saturating mix on the i16 side: `[i16::MIN, i16::MAX]` neighbors
//!   produce a `b - a` that overflows i16, so the interpolation runs
//!   in i32 and clamps.

/// Canonical sample rates the bridge sees in practice. Tests
/// spot-check the resampler at these to catch regressions in the
/// integer-math path. Not a public contract — callers can pass any
/// `u32` rate.
pub const COMMON_RATES: &[u32] = &[8_000, 16_000, 24_000, 32_000, 44_100, 48_000];

/// Resample mono i16 PCM from `from_hz` to `to_hz` via linear
/// interpolation.
///
/// Output length is `input.len() * to_hz / from_hz` (integer math,
/// floored). Output sample `i` is interpolated between input indices
/// `floor(i * from_hz / to_hz)` and the next sample, by the
/// fractional remainder. The upper index is clamped to
/// `input.len() - 1` so the tail doesn't read out of bounds.
///
/// `from_hz == 0` or `to_hz == 0` returns an empty `Vec` rather than
/// panicking — a driver feeding the bridge a zero rate is a bug, but
/// taking down the meeting on a divide-by-zero is worse than a silent
/// frame.
///
/// ```
/// use heron_bridge::resample_linear;
///
/// // Driver hands us 48 kHz; the bridge speaks 16 kHz internally.
/// let driver_frame = vec![0i16; 480]; // 10 ms at 48 kHz
/// let bridge_frame = resample_linear(&driver_frame, 48_000, 16_000);
/// assert_eq!(bridge_frame.len(), 160); // 10 ms at 16 kHz
/// ```
pub fn resample_linear(input: &[i16], from_hz: u32, to_hz: u32) -> Vec<i16> {
    if input.is_empty() || from_hz == 0 || to_hz == 0 {
        return Vec::new();
    }
    if from_hz == to_hz {
        return input.to_vec();
    }

    // Output length comes from the rate ratio. u64 mul before div so
    // the intermediate doesn't overflow on a long input at a high
    // upsample ratio (e.g. a 1-second buffer at 48 kHz → 144 kHz).
    let out_len = (input.len() as u64 * to_hz as u64 / from_hz as u64) as usize;
    let mut out = Vec::with_capacity(out_len);

    let from = from_hz as u64;
    let to = to_hz as u64;
    // Safe because input is non-empty (early-returned above).
    let last_idx = input.len() - 1;

    for i in 0..out_len {
        // Position in the input expressed as `pos_num / to`. Keep it
        // integer: fractional floats here would drift on long buffers.
        let pos_num = i as u64 * from;
        let floor_idx = (pos_num / to) as usize;
        let rem = (pos_num % to) as i64;

        // Clamp the upper neighbor so the last output sample doesn't
        // index past the input. This is the standard linear-resampler
        // tail handling — the alternative (allocating a phantom sample)
        // would cost an alloc for the common case.
        let lo = input[floor_idx.min(last_idx)] as i64;
        let hi = input[(floor_idx + 1).min(last_idx)] as i64;

        // i64 math throughout. The product `rem * (hi - lo)` overflows
        // i32 in normal use: at to_hz = 48000 with `[i16::MIN, i16::MAX]`
        // neighbors, `47_999 * 65_535 ≈ 3.15e9` exceeds i32::MAX
        // (2.15e9). Final clamp into i16 guards against any future
        // change to the formula that could push the result outside
        // i16's range.
        let to_i64 = to as i64;
        let mixed = lo + (rem * (hi - lo)) / to_i64;
        out.push(mixed.clamp(i16::MIN as i64, i16::MAX as i64) as i16);
    }

    out
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn identity_when_rates_equal() {
        // Same rate is the fast path — input cloned verbatim, no
        // interpolation drift. Pin so a future refactor doesn't
        // accidentally route 16k→16k through the math path.
        let input = vec![1, 2, 3, -100, i16::MIN, i16::MAX];
        let output = resample_linear(&input, 16_000, 16_000);
        assert_eq!(output, input);
    }

    #[test]
    fn empty_input_returns_empty() {
        assert!(resample_linear(&[], 48_000, 16_000).is_empty());
        assert!(resample_linear(&[], 16_000, 48_000).is_empty());
        assert!(resample_linear(&[], 16_000, 16_000).is_empty());
    }

    #[test]
    fn from_hz_zero_returns_empty_no_panic() {
        // Degenerate caller bug (driver reporting 0 Hz). Must not
        // panic — the bridge runs on the meeting's hot path.
        let result = resample_linear(&[1, 2, 3], 0, 16_000);
        assert!(result.is_empty());
    }

    #[test]
    fn to_hz_zero_returns_empty_no_panic() {
        // Symmetric to `from_hz == 0`: a driver/backend asking for
        // 0 Hz output is a bug; degrade gracefully rather than
        // panic on the bridge's hot path.
        let result = resample_linear(&[1, 2, 3], 16_000, 0);
        assert!(result.is_empty());
    }

    #[test]
    fn downsample_48k_to_16k_preserves_dc() {
        // A constant-200 input has no frequency content to alias;
        // the resampler should pass it through unchanged. ±1
        // tolerance for any integer-rounding artifact (in practice
        // the 3:1 ratio is exact, but pin loosely so a future
        // formula tweak doesn't fail by an off-by-one rounding).
        let input = vec![200i16; 480];
        let output = resample_linear(&input, 48_000, 16_000);
        for s in &output {
            assert!((s - 200).abs() <= 1, "expected ~200, got {s}");
        }
    }

    #[test]
    fn upsample_16k_to_48k_length_triples() {
        // 1:3 ratio → output length is exactly 3x input length.
        let input = vec![0i16; 160];
        let output = resample_linear(&input, 16_000, 48_000);
        assert_eq!(output.len(), 480);
    }

    #[test]
    fn downsample_48k_to_16k_length_thirds() {
        // 3:1 ratio → output length is exactly input/3.
        let input = vec![0i16; 480];
        let output = resample_linear(&input, 48_000, 16_000);
        assert_eq!(output.len(), 160);
    }

    #[test]
    fn extrema_dont_overflow_when_clamping() {
        // `[i16::MIN, i16::MAX]` neighbors give `hi - lo == 65_535`,
        // which overflows i16. The interpolation runs in i64 then
        // clamps; pin that the upsample doesn't panic and produces
        // a monotonically increasing ramp from MIN to MAX (which is
        // what linear interpolation between those endpoints means).
        let input = [i16::MIN, i16::MAX];
        let output = resample_linear(&input, 1, 8);
        assert!(!output.is_empty());
        // Endpoints land on the input samples exactly — pos 0 is MIN
        // and pos floor(7/8 * 1) = 0 with rem 7 still interpolates,
        // but the first output is exactly input[0].
        assert_eq!(output[0], i16::MIN);
        // Monotonic non-decreasing across the ramp.
        for pair in output.windows(2) {
            assert!(pair[1] >= pair[0], "non-monotonic: {:?}", pair);
        }
    }

    #[test]
    fn extrema_at_high_to_hz_does_not_overflow() {
        // Regression for the i32 overflow in `rem * (hi - lo)`. With
        // `to_hz = 48_000` and extrema neighbors, the product reaches
        // `47_999 * 65_535 ≈ 3.15e9`, which exceeds i32::MAX. Earlier
        // code in i32 panicked under overflow checks (debug) or
        // wrapped to a corrupt sample (release). Pin in i64.
        let input = [i16::MIN, i16::MAX];
        let output = resample_linear(&input, 1, 48_000);
        assert_eq!(output.len(), 96_000);
        assert_eq!(output[0], i16::MIN);
        // Final sample either lands on or near i16::MAX (depends on
        // exactly where the floor lands at the tail clamp).
        let last = *output.last().expect("non-empty");
        assert!(last > 0, "expected positive tail near MAX, got {last}");
        // No panics occurred — that's the load-bearing check.
    }

    #[test]
    fn forty_four_one_to_sixteen_preserves_dc() {
        // 44.1k → 16k is the rate pair where rounding/truncation
        // drift surfaces (irrational ratio in audio terms). DC
        // input has no frequency content to alias; output should
        // be ±1 of input.
        let input = vec![1234i16; 4_410];
        let output = resample_linear(&input, 44_100, 16_000);
        assert_eq!(output.len(), 1_600);
        for s in &output {
            assert!(
                (s - 1_234).abs() <= 1,
                "expected ~1234 at 44.1k→16k, got {s}"
            );
        }
    }

    #[test]
    fn forty_four_one_to_forty_eight_preserves_dc() {
        // 44.1k → 48k is upsample-with-irrational-ratio. Same
        // tolerance as above. Catches a future formula tweak that
        // accidentally biases on upsample.
        let input = vec![-2_000i16; 4_410];
        let output = resample_linear(&input, 44_100, 48_000);
        assert_eq!(output.len(), 4_800);
        for s in &output {
            assert!(
                (s - (-2_000)).abs() <= 1,
                "expected ~-2000 at 44.1k→48k, got {s}"
            );
        }
    }

    #[test]
    fn single_sample_input() {
        // A 1-sample input has no neighbor to interpolate against;
        // the upper-index clamp pins both `lo` and `hi` to the same
        // sample, so every output sample equals it. Output length
        // depends on the ratio — at minimum ≥ 1 sample for any
        // upsample, 0 for a 2:1 downsample (1*1/2 = 0). Test only
        // the upsample case where the spec requires ≥ 1.
        let input = [1234i16];
        let output = resample_linear(&input, 16_000, 48_000);
        assert!(!output.is_empty());
        for s in &output {
            assert_eq!(*s, 1234);
        }
    }

    #[test]
    fn linear_interpolation_at_2x_upsample() {
        // Input `[0, 1000]` at 2x upsample. Output length = 4.
        // Sample 0: pos 0.0 → 0
        // Sample 1: pos 0.5 → 500 (linear midpoint)
        // Sample 2: pos 1.0 → 1000
        // Sample 3: pos 1.5 → upper-clamped → 1000
        // Pin the exact 500 so a future formula change (e.g.
        // rounding instead of flooring the rem*delta term) surfaces.
        let output = resample_linear(&[0, 1000], 1, 2);
        assert_eq!(output, vec![0, 500, 1000, 1000]);
    }

    #[test]
    fn common_rates_pairs_dont_panic() {
        // Spot-check every (from, to) pair across COMMON_RATES with
        // a small buffer. Catches regressions in the integer-math
        // path (e.g. a from/to swap, or a u32 overflow at 44_100 *
        // input.len()) without enumerating every rate manually.
        let input = vec![100i16; 64];
        for &from in COMMON_RATES {
            for &to in COMMON_RATES {
                let _ = resample_linear(&input, from, to);
            }
        }
    }
}
