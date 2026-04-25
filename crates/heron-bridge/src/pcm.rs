//! PcmFrame helpers — sample counts, durations, silence detection.
//!
//! [`crate::PcmFrame`] is a raw `Vec<i16>` + metadata. Consumers
//! (the bridge's AEC/jitter buffer, the diagnostics tab, the
//! realtime backend's barge-in detector) all need to convert
//! between samples / milliseconds / seconds at the canonical
//! 16 kHz mono rate. Centralizing those conversions here keeps a
//! `samples / 16` factor out of every call site.
//!
//! ## Sample-rate convention
//!
//! 16 kHz mono i16 throughout. Driver / realtime-backend code
//! resamples on the way in (often from 48 kHz) and out — neither
//! direction is this module's concern. [`samples_to_ms`] /
//! [`ms_to_samples`] are the sources of truth for the conversion.
//!
//! ## Silence detection
//!
//! [`PcmFrameExt::is_silence`] uses a peak-amplitude threshold
//! (max `abs(sample)` over the frame). Peak is cheaper than RMS
//! and works for the heron use case (gating barge-in detection on
//! "is the participant audibly speaking?"); for proper VAD with
//! noise gating, the realtime backend or `webrtc-audio-processing`
//! is the right layer.

use crate::PcmFrame;

/// Canonical sample rate per the bridge's wire convention.
/// 16 kHz mono is what every realtime backend (OpenAI Realtime,
/// LiveKit, Pipecat) expects on input.
pub const SAMPLE_RATE_HZ: u32 = 16_000;

/// Default silence threshold for [`PcmFrameExt::is_silence`]. ~1%
/// of i16 max — quiet enough to consider "no speech" but loud
/// enough that ambient room noise doesn't fire constantly. Real
/// VAD lives in the realtime backend; this threshold is good
/// enough for the bridge's "detect barge-in" use case.
pub const DEFAULT_SILENCE_THRESHOLD: i16 = 320;

/// Convert sample count to milliseconds at [`SAMPLE_RATE_HZ`].
///
/// Saturates on overflow rather than wrapping — a malformed frame
/// claiming 4 billion samples shouldn't produce a negative-looking
/// duration. Caller already capped frame size at the bridge's
/// jitter-buffer limit.
pub fn samples_to_ms(samples: usize) -> u64 {
    // 16 samples per ms at 16 kHz.
    (samples as u64).saturating_mul(1_000) / SAMPLE_RATE_HZ as u64
}

/// Convert milliseconds to sample count at [`SAMPLE_RATE_HZ`].
///
/// 16 kHz / 1000 = exactly 16 samples per ms, so any integer-ms
/// input maps to an exact multiple of 16 samples — no rounding
/// needed at this entry point. The flooring case lives on the
/// inverse [`samples_to_ms`], which drops the fractional ms when
/// the sample count isn't a multiple of 16. Saturating math on
/// the multiplication so a `u64::MAX` ms input doesn't panic.
pub fn ms_to_samples(ms: u64) -> usize {
    (ms.saturating_mul(SAMPLE_RATE_HZ as u64) / 1_000) as usize
}

/// Helpers on top of [`PcmFrame`]. Implemented as an extension
/// trait so adding methods doesn't fight the existing public-fields
/// shape (the struct is `Vec<i16>` + metadata, not opaque).
pub trait PcmFrameExt {
    /// Number of samples in the frame.
    fn sample_count(&self) -> usize;

    /// Frame duration at 16 kHz mono.
    fn duration_ms(&self) -> u64;

    /// `true` when the frame's peak amplitude is at or below
    /// `threshold`. `<=` semantics so a threshold of `0` correctly
    /// reports an all-zero frame as silent. Use
    /// [`DEFAULT_SILENCE_THRESHOLD`] for the bridge default.
    fn is_silence(&self, threshold: i16) -> bool;

    /// `true` when [`Self::is_silence`] would return `true` at the
    /// default threshold. Convenience for the common-case caller.
    fn is_default_silence(&self) -> bool {
        self.is_silence(DEFAULT_SILENCE_THRESHOLD)
    }

    /// Peak absolute amplitude in the frame. `0` for an empty
    /// frame. Useful for the diagnostics tab's audio-level meter.
    fn peak_amplitude(&self) -> i16;
}

impl PcmFrameExt for PcmFrame {
    fn sample_count(&self) -> usize {
        self.samples.len()
    }

    fn duration_ms(&self) -> u64 {
        samples_to_ms(self.samples.len())
    }

    fn is_silence(&self, threshold: i16) -> bool {
        // Short-circuit on the first non-silent sample so a noisy
        // frame doesn't pay for a full-buffer scan. Equivalent to
        // `peak_amplitude() <= threshold` but linear-time worst-
        // case rather than always-linear.
        !self.samples.iter().any(|&s| s.saturating_abs() > threshold)
    }

    fn peak_amplitude(&self) -> i16 {
        // `i16::abs` on `i16::MIN` would overflow; saturating_abs
        // returns `i16::MAX` for that edge case.
        self.samples
            .iter()
            .copied()
            .map(i16::saturating_abs)
            .max()
            .unwrap_or(0)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::AudioChannel;

    fn frame(samples: Vec<i16>) -> PcmFrame {
        PcmFrame {
            samples,
            captured_at_micros: 0,
            channel: AudioChannel::MeetingIn,
        }
    }

    #[test]
    fn sample_count_matches_vec_len() {
        let f = frame(vec![0; 320]);
        assert_eq!(f.sample_count(), 320);
    }

    #[test]
    fn duration_ms_at_16khz() {
        // 320 samples at 16 kHz = 20 ms (the realtime-backend
        // chunk size convention).
        assert_eq!(frame(vec![0; 320]).duration_ms(), 20);
        // 1600 samples = 100 ms.
        assert_eq!(frame(vec![0; 1_600]).duration_ms(), 100);
        // Empty frame = 0 ms.
        assert_eq!(frame(vec![]).duration_ms(), 0);
    }

    #[test]
    fn samples_to_ms_round_trip_at_msec_boundaries() {
        for ms in [0, 1, 10, 20, 100, 1_000, 60_000] {
            let s = ms_to_samples(ms);
            let back = samples_to_ms(s);
            assert_eq!(back, ms, "{ms} ms -> {s} samples -> {back} ms");
        }
    }

    #[test]
    fn ms_to_samples_is_exact_at_16khz() {
        // 16 kHz / 1000 = exactly 16 samples per ms, so every
        // integer ms input maps to an exact multiple of 16. No
        // rounding lives here — the flooring case is on the
        // inverse (`samples_to_ms`).
        assert_eq!(ms_to_samples(7), 112);
        assert_eq!(ms_to_samples(8), 128);
    }

    #[test]
    fn samples_to_ms_floors_when_not_a_multiple_of_16() {
        // 120 samples = 7.5 ms; the floor is 7. Pin so a future
        // refactor that rounds (rather than floors) surfaces here.
        assert_eq!(samples_to_ms(120), 7);
        assert_eq!(samples_to_ms(119), 7);
        // 128 samples is exactly 8 ms; no flooring needed.
        assert_eq!(samples_to_ms(128), 8);
    }

    #[test]
    fn samples_to_ms_saturates_on_huge_input() {
        // u64::MAX samples shouldn't panic. Saturating math returns
        // a finite (large) ms value.
        let result = samples_to_ms(usize::MAX);
        assert!(result > 0);
    }

    #[test]
    fn empty_frame_peak_is_zero() {
        assert_eq!(frame(vec![]).peak_amplitude(), 0);
    }

    #[test]
    fn peak_finds_max_absolute_value() {
        let f = frame(vec![100, -2_000, 50, 1_500, -500]);
        assert_eq!(f.peak_amplitude(), 2_000);
    }

    #[test]
    fn peak_handles_i16_min_without_panic() {
        // i16::MIN.abs() would overflow normally — saturating_abs
        // returns i16::MAX (32_767). Pin behavior so a corrupted
        // sample doesn't crash the bridge.
        let f = frame(vec![i16::MIN, 100]);
        assert_eq!(f.peak_amplitude(), i16::MAX);
    }

    #[test]
    fn is_silence_with_default_threshold() {
        // All samples below threshold = silence.
        let f = frame(vec![0, 50, -100, 10]);
        assert!(f.is_default_silence());

        // One sample above threshold = not silence.
        let f = frame(vec![0, 50, -1_000, 10]);
        assert!(!f.is_default_silence());
    }

    #[test]
    fn is_silence_with_custom_threshold() {
        let f = frame(vec![0, 200, -150]);
        // 200 < 500 → silence at threshold 500.
        assert!(f.is_silence(500));
        // 200 >= 100 → not silence at threshold 100.
        assert!(!f.is_silence(100));
    }

    #[test]
    fn is_silence_empty_frame_is_silent() {
        // No samples → no audio → silent. The barge-in detector
        // shouldn't fire on a zero-length frame.
        let f = frame(vec![]);
        assert!(f.is_silence(0));
        assert!(f.is_default_silence());
    }

    #[test]
    fn is_silence_threshold_zero_treats_any_sample_as_speech() {
        // Threshold = 0 means "any non-zero sample is speech."
        let f = frame(vec![0, 0, 1, 0]);
        assert!(!f.is_silence(0));
        let f = frame(vec![0, 0, 0]);
        assert!(f.is_silence(0));
    }

    #[test]
    fn duration_ms_is_deterministic() {
        let f = frame(vec![0; 320]);
        assert_eq!(f.duration_ms(), f.duration_ms());
    }
}
