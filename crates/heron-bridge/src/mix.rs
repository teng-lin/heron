//! Saturating PCM mixers — sum or average two i16 streams.
//!
//! Mixing happens whenever two PCM tracks share a sink: e.g. two
//! participants' channels collapsing into a single MeetingIn for
//! the realtime backend, or the agent's TTS playback summed with
//! a "hold music" track. The naive `a + b` would wrap on i16
//! overflow and produce a click; we clamp instead.
//!
//! ## Sum vs average
//!
//! [`mix_saturating`] / [`mix_inplace_saturating`] are *additive*
//! — two full-scale signals saturate to `i16::MAX`. That's the
//! right choice for combining independent voices (each speaker
//! should sound at their original loudness). [`average_saturating`]
//! halves the gain — useful when the caller wants the combined
//! track to fit in the same headroom as a single input (e.g. a
//! pre-mixdown for a single-channel realtime backend that doesn't
//! tolerate clipping).
//!
//! ## Length mismatch
//!
//! Both APIs zero-pad the shorter side rather than truncating to
//! the shorter length — caller almost always wants "play whatever
//! audio is available, even if one side ran out." [`mix_inplace_saturating`]
//! is the exception: it leaves the tail of `into` past `from.len()`
//! untouched, which is the same zero-pad semantic from `from`'s
//! perspective (adding zero is a no-op).

/// Sample-wise saturating sum of two PCM streams.
///
/// Output length is `max(a.len(), b.len())`; the shorter side is
/// treated as zero-padded. Two full-scale signals clamp to
/// `i16::MAX` rather than wrapping. Use this to combine independent
/// voices at their original loudness; use [`average_saturating`] if
/// the combined track must fit in single-input headroom.
///
/// ```
/// use heron_bridge::mix_saturating;
///
/// // Two participants meeting-mix: both speaking, summed into
/// // the single MeetingIn the realtime backend consumes.
/// let alice = [100, 200, 300];
/// let bob = [10, 20, 30];
/// assert_eq!(mix_saturating(&alice, &bob), vec![110, 220, 330]);
/// ```
pub fn mix_saturating(a: &[i16], b: &[i16]) -> Vec<i16> {
    let len = a.len().max(b.len());
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        // Out-of-range indices read as zero (zero-pad shorter side).
        // Adding zero is a no-op so the longer side passes through.
        let av = a.get(i).copied().unwrap_or(0);
        let bv = b.get(i).copied().unwrap_or(0);
        out.push(av.saturating_add(bv));
    }
    out
}

/// Additive saturating mix into an existing buffer.
///
/// Mixes `from[i]` into `into[i]` for `i in 0..min(into.len(), from.len())`.
/// Samples in `into` past `from.len()` are left untouched — equivalent
/// to zero-padding `from`, since adding zero is a no-op. Useful when
/// the caller already owns the output buffer (avoids the alloc that
/// [`mix_saturating`] makes).
pub fn mix_inplace_saturating(into: &mut [i16], from: &[i16]) {
    let n = into.len().min(from.len());
    for i in 0..n {
        into[i] = into[i].saturating_add(from[i]);
    }
}

/// Sample-wise saturating *average* (halved-gain mix).
///
/// Output length is `max(a.len(), b.len())`; the shorter side is
/// zero-padded so the longer side gets halved against silence rather
/// than silently truncated. Computes `((a + b) / 2)` widened through
/// `i32` to avoid the `i16::MIN + i16::MIN` overflow case. Saturating
/// clamp on the cast back is defensive — `(i32 / 2)` from two i16s
/// always fits in i16, but pinning the clamp keeps the function
/// total against future widening of input types.
///
/// Note the gain difference vs [`mix_saturating`]: two `20_000`
/// inputs sum-mix to `i16::MAX` (saturated) but average to `20_000`
/// (the original loudness). Use this when downstream can't tolerate
/// clipping.
pub fn average_saturating(a: &[i16], b: &[i16]) -> Vec<i16> {
    let len = a.len().max(b.len());
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let av = a.get(i).copied().unwrap_or(0) as i32;
        let bv = b.get(i).copied().unwrap_or(0) as i32;
        // Widen to i32 so i16::MIN + i16::MIN doesn't overflow.
        // The /2 result is always in i16 range; clamp is defensive.
        let avg = ((av + bv) / 2).clamp(i16::MIN as i32, i16::MAX as i32);
        out.push(avg as i16);
    }
    out
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn mix_equal_length_sums_samples() {
        // Plain additive mix: two streams of equal length sum
        // sample-wise. No saturation in this range.
        assert_eq!(mix_saturating(&[100, 200], &[300, 400]), vec![400, 600]);
    }

    #[test]
    fn mix_saturates_at_i16_max() {
        // 20_000 + 20_000 = 40_000, which exceeds i16::MAX (32_767)
        // and would wrap to a negative on plain `+`. Pin the clamp
        // so a future "drop saturating_add for speed" surfaces here.
        assert_eq!(
            mix_saturating(&[20_000, -20_000], &[20_000, -20_000]),
            vec![i16::MAX, i16::MIN]
        );
    }

    #[test]
    fn mix_zero_extends_shorter_side() {
        // Shorter side is zero-padded; output is the longer length.
        // Caller almost always wants "play whatever's available" —
        // truncating to the shorter side would silently drop audio.
        assert_eq!(mix_saturating(&[100, 200, 300], &[10]), vec![110, 200, 300]);
    }

    #[test]
    fn mix_both_empty_returns_empty() {
        let out: Vec<i16> = mix_saturating(&[], &[]);
        assert!(out.is_empty());
    }

    #[test]
    fn mix_one_empty_returns_other() {
        // Empty + anything = anything (zero-pad makes the empty side
        // contribute nothing).
        assert_eq!(mix_saturating(&[], &[1, 2, 3]), vec![1, 2, 3]);
        assert_eq!(mix_saturating(&[1, 2, 3], &[]), vec![1, 2, 3]);
    }

    #[test]
    fn mix_inplace_only_overwrites_overlap() {
        // Past `from.len()` the buffer is left untouched — adding
        // zero would be a no-op anyway, but pin the behavior so a
        // future "fill tail with zero" rewrite surfaces here.
        let mut into = [10, 20, 30];
        mix_inplace_saturating(&mut into, &[1, 2]);
        assert_eq!(into, [11, 22, 30]);
    }

    #[test]
    fn mix_inplace_saturates() {
        // Same clamp guarantees as `mix_saturating` but on the
        // caller's own buffer.
        let mut into = [20_000, -20_000];
        mix_inplace_saturating(&mut into, &[20_000, -20_000]);
        assert_eq!(into, [i16::MAX, i16::MIN]);
    }

    #[test]
    fn average_halves_gain_relative_to_mix() {
        // The defining difference vs sum-mix: 20_000 + 20_000
        // saturates to i16::MAX, but the average is just 20_000
        // (the original loudness). This is the headroom-preserving
        // choice for downstream that can't tolerate clipping.
        assert_eq!(mix_saturating(&[20_000], &[20_000]), vec![i16::MAX]);
        assert_eq!(average_saturating(&[20_000], &[20_000]), vec![20_000]);
    }

    #[test]
    fn average_zero_extends_shorter_side() {
        // The longer side gets averaged against silence rather than
        // truncated — pin the zero-pad. (300 + 0) / 2 = 150.
        assert_eq!(
            average_saturating(&[100, 200, 300], &[10]),
            vec![55, 100, 150]
        );
    }

    #[test]
    fn average_handles_i16_min_without_overflow() {
        // i16::MIN + i16::MIN would overflow on naive i16 add.
        // Widening to i32 sidesteps it; the average is just i16::MIN.
        assert_eq!(average_saturating(&[i16::MIN], &[i16::MIN]), vec![i16::MIN]);
    }
}
