//! WebRTC APM-based Acoustic Echo Cancellation (AEC).
//!
//! Per [`docs/plan.md`] §6.3 and
//! [`docs/implementation.md`](../../../docs/implementation.md) §6.3, the
//! mic captures both the user's voice **and** speaker bleed of the
//! meeting client's audio (we run with speakers on, not headphones, so
//! the call is audible). The Core Audio process tap (`process_tap.rs`,
//! shipped in PR #52) gives us a clean reference of exactly what's
//! coming out of the meeting app — we feed that to APM as the
//! "reverse stream" / far-end, and APM subtracts the corresponding
//! echo from the mic ("capture stream" / near-end).
//!
//! The two `Processor` methods on the `webrtc-audio-processing` 2.x
//! API correspond to:
//! - [`Processor::process_render_frame`] = far-end (the tap reference,
//!   what's playing out of the speakers). Wraps APM's
//!   `ProcessReverseStream`.
//! - [`Processor::process_capture_frame`] = near-end (the mic, what
//!   we're trying to clean). Wraps APM's `ProcessStream`. Mutates the
//!   buffer in place with the cleaned audio.
//!
//! ## Frame-size assumption
//!
//! WebRTC APM works on **10 ms frames** = 480 samples at 48 kHz mono.
//! `CaptureFrame::samples` is documented in
//! [`crate::CaptureFrame`] as 48 kHz mono `f32` and "typically 10 ms =
//! 480 samples." The upstream emitters (`process_tap.rs` IO proc and
//! the cpal mic, both arriving in parallel PRs) emit 480-sample frames
//! by construction.
//!
//! Both entry points runtime-check the frame size and return
//! [`AudioError::Aborted`] on a mismatch (APM itself panics on a
//! length-mismatched frame, so we pre-check to keep the error
//! recoverable). Buffering / resampling to 480 samples is intentionally
//! out of scope for this PR — that's real DSP work and the upstream
//! pipeline already handles it.
//!
//! ## Why NS / AGC are off
//!
//! The v0 AEC config enables echo cancellation only. Noise suppression
//! (NS) and automatic gain control (AGC) are deliberately disabled:
//!
//! 1. **Minimum manipulation, maximum debuggability.** §6.3 grades this
//!    PR on a single correlation metric (`mic_clean` × `tap` < 0.15).
//!    NS and AGC introduce non-linear, content-dependent transforms
//!    that confound that metric and make it harder to tell whether a
//!    failing test is an AEC bug or an NS/AGC artifact.
//! 2. **STT prefers raw input.** Both Parakeet (week 4) and Whisper
//!    fallback (week 5) do their own VAD and gain normalization
//!    internally. Pre-applying NS/AGC strips information from the STT
//!    front-end without consistent benefit and can hurt low-volume
//!    speakers.
//! 3. **Future toggle, not future deletion.** We can flip these on in
//!    a follow-up once §6.3 passes; the config struct in `new()` is
//!    the single point of change.
//!
//! ## What this PR ships vs what's still TODO
//!
//! This PR ships the **standalone AEC processor**: build it, feed it
//! frames, get cleaned audio back. The wiring (mic capture frames →
//! `process_near_end`, tap capture frames → `process_far_end`, cleaned
//! frames → broadcast channel) lands as a follow-up once both the cpal
//! mic PR and the IO-proc-frames PR (#57) are in.

use webrtc_audio_processing::{Config, Processor, config::EchoCanceller as ApmEchoCanceller};

use crate::{AudioError, CaptureFrame};
use heron_types::Channel;

/// APM operates on 10 ms frames. At 48 kHz mono that's exactly 480
/// `f32` samples per frame. Both far-end and near-end frames must be
/// this size — APM panics or returns `BadDataLength` otherwise.
pub const APM_FRAME_SAMPLES: usize = 480;

/// APM's required sample rate for the v0 pipeline. The Core Audio
/// process tap and the cpal mic are both configured to deliver 48 kHz
/// (cidre's tap mixdown is fixed at 48 k, and `cpal` is asked for
/// 48 k). Anything else is a contract violation upstream.
pub const APM_SAMPLE_RATE_HZ: u32 = 48_000;

/// WebRTC APM-backed echo canceller.
///
/// Holds an inner `webrtc_audio_processing::Processor` configured for
/// 48 kHz mono with AEC3 enabled. Use [`process_far_end`] to feed the
/// per-app tap reference and [`process_near_end`] to clean a mic frame
/// in place.
///
/// Drop releases the underlying APM allocations cleanly via the inner
/// `Processor`'s Drop impl — no explicit teardown needed.
///
/// [`process_far_end`]: Self::process_far_end
/// [`process_near_end`]: Self::process_near_end
pub struct EchoCanceller {
    inner: Processor,
    /// Reusable scratch buffer for the far-end path. APM's
    /// `process_render_frame` takes `AsMut<[f32]>` even though it
    /// doesn't actually mutate the buffer (the in-place contract is
    /// only for symmetry with `process_capture_frame` — the upstream
    /// `examples/simple.rs` asserts that the render buffer is
    /// unchanged after the call). Owning a single pre-allocated
    /// 480-sample `Vec` lets `process_far_end` `copy_from_slice` into
    /// it instead of allocating a fresh `Vec` per 10 ms call. At
    /// 100 calls/s that's a hot allocator path even though APM runs
    /// off the realtime callback (per `docs/implementation.md` §6.2's
    /// "realtime → SPSC → APM thread" topology).
    far_end_scratch: Vec<f32>,
}

impl EchoCanceller {
    /// Construct a new APM-backed echo canceller configured for 48 kHz
    /// mono input with AEC3 enabled and automatic delay estimation.
    ///
    /// NS and AGC are intentionally disabled; see the module-level
    /// docs for the rationale.
    ///
    /// # Errors
    /// Returns [`AudioError::Aborted`] if the underlying WebRTC library
    /// fails to initialize (e.g. unsupported sample rate, allocation
    /// failure). In practice this should be infallible for the pinned
    /// 48 kHz mono config.
    pub fn new() -> Result<Self, AudioError> {
        let inner = Processor::new(APM_SAMPLE_RATE_HZ)
            .map_err(|e| AudioError::Aborted(format!("WebRTC APM init failed: {e}")))?;

        // AEC3 with `stream_delay_ms: None` lets APM estimate the
        // mic↔reference delay automatically, which is what we want —
        // we don't have a hardware-known delay between the per-app
        // tap (ring 1) and the mic (ring 2); they enter APM via two
        // independent SPSC queues whose relative latency depends on
        // scheduler jitter and Core Audio buffer sizes.
        //
        // NS and AGC are spelled out as `None` instead of relying on
        // `Default::default()` so a future patch-version bump of
        // `webrtc-audio-processing` (which the upstream README warns
        // can carry breaking changes inside the 2.x major) cannot
        // silently turn either of them on. The high-pass filter,
        // capture amplifier, and pipeline still pick up WebRTC's
        // out-of-the-box defaults via `..Default::default()`.
        let config = Config {
            echo_canceller: Some(ApmEchoCanceller::Full {
                stream_delay_ms: None,
            }),
            noise_suppression: None,
            gain_controller: None,
            ..Default::default()
        };
        inner.set_config(config);

        Ok(Self {
            inner,
            far_end_scratch: vec![0.0; APM_FRAME_SAMPLES],
        })
    }

    /// Feed a far-end (tap / reference) frame to APM via
    /// `process_render_frame`. The frame is **not** mutated — APM only
    /// uses it to model the echo path.
    ///
    /// # Errors
    /// Returns [`AudioError::Aborted`] if:
    /// - `frame.channel != Channel::Tap` — feeding the wrong stream into
    ///   APM is a wiring bug, fail loudly rather than silently degrade
    ///   AEC quality.
    /// - `frame.samples.len() != APM_FRAME_SAMPLES` — APM's
    ///   `process_render_frame` panics on a length mismatch; we
    ///   convert that into a recoverable `Aborted` so the capture
    ///   pipeline can degrade gracefully.
    /// - APM itself returns an error (e.g. internal allocation failure).
    pub fn process_far_end(&mut self, frame: &CaptureFrame) -> Result<(), AudioError> {
        if frame.channel != Channel::Tap {
            return Err(AudioError::Aborted(format!(
                "process_far_end requires Channel::Tap, got {:?} (channel mis-routed)",
                frame.channel
            )));
        }
        if frame.samples.len() != APM_FRAME_SAMPLES {
            return Err(AudioError::Aborted(format!(
                "process_far_end frame size: expected {} samples (10 ms @ 48 kHz mono), got {}",
                APM_FRAME_SAMPLES,
                frame.samples.len()
            )));
        }

        // APM expects non-interleaved per-channel slices. For mono
        // that is a single inner slice. `copy_from_slice` into the
        // pre-allocated `far_end_scratch` rather than allocating a
        // fresh `Vec` every call — see the field-level docs on
        // `far_end_scratch` for the rationale. The `Vec` is sized at
        // construction to exactly `APM_FRAME_SAMPLES`; the runtime
        // length check above guarantees the source slice matches, so
        // `copy_from_slice` will not panic.
        self.far_end_scratch.copy_from_slice(&frame.samples);
        self.inner
            .process_render_frame([self.far_end_scratch.as_mut_slice()])
            .map_err(|e| AudioError::Aborted(format!("APM process_render_frame failed: {e}")))?;
        Ok(())
    }

    /// Feed a near-end (mic) frame to APM via `process_capture_frame`,
    /// **mutating `frame.samples` in place** with the AEC-cleaned
    /// audio. After this returns, `frame.samples` is what STT should
    /// see.
    ///
    /// Call order matters: for any given 10 ms window, feed the
    /// matching far-end frame *first* (or as close in time as the
    /// upstream queues allow); APM's automatic delay estimator will
    /// align them, but it can only align frames it has actually been
    /// shown.
    ///
    /// # Errors
    /// Returns [`AudioError::Aborted`] if:
    /// - `frame.channel != Channel::Mic`.
    /// - `frame.samples.len() != APM_FRAME_SAMPLES`.
    /// - APM itself returns an error.
    pub fn process_near_end(&mut self, frame: &mut CaptureFrame) -> Result<(), AudioError> {
        if frame.channel != Channel::Mic {
            return Err(AudioError::Aborted(format!(
                "process_near_end requires Channel::Mic, got {:?} (channel mis-routed)",
                frame.channel
            )));
        }
        if frame.samples.len() != APM_FRAME_SAMPLES {
            return Err(AudioError::Aborted(format!(
                "process_near_end frame size: expected {} samples (10 ms @ 48 kHz mono), got {}",
                APM_FRAME_SAMPLES,
                frame.samples.len()
            )));
        }

        self.inner
            .process_capture_frame([frame.samples.as_mut_slice()])
            .map_err(|e| AudioError::Aborted(format!("APM process_capture_frame failed: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use heron_types::SessionClock;

    /// Build a mono `CaptureFrame` of the given sample length and
    /// channel, with the per-sample data from `gen`. `host_time` and
    /// `session_secs` are filled with placeholder values from a fresh
    /// `SessionClock` — the AEC processor doesn't read them.
    fn frame_with<F: FnMut(usize) -> f32>(
        channel: Channel,
        len: usize,
        mut sample_fn: F,
    ) -> CaptureFrame {
        let clock = SessionClock::new();
        let samples: Vec<f32> = (0..len).map(&mut sample_fn).collect();
        CaptureFrame {
            channel,
            host_time: clock.mach_anchor,
            session_secs: 0.0,
            samples,
        }
    }

    fn rms(samples: &[f32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
        (sum_sq / samples.len() as f32).sqrt()
    }

    /// Assert that `err` is `AudioError::Aborted` and its message
    /// (case-insensitive) contains at least one of `needles`. Folds the
    /// repeated `match err { Aborted(msg) => assert!(msg.contains(...)) }`
    /// pattern in the rejection tests below into one place.
    fn assert_aborted_contains(err: AudioError, needles: &[&str]) {
        match err {
            AudioError::Aborted(msg) => {
                let lower = msg.to_lowercase();
                assert!(
                    needles.iter().any(|n| lower.contains(*n)),
                    "Aborted message must mention one of {needles:?}, got: {msg}"
                );
            }
            other => panic!("expected AudioError::Aborted, got {other:?}"),
        }
    }

    /// Sanity: APM constructs and drops without panicking. This catches
    /// link failures (missing meson/ninja in CI) and config validation
    /// regressions earlier than any of the heavier tests below.
    #[test]
    fn construct_and_drop() {
        let aec = EchoCanceller::new().expect("APM must initialize at 48 kHz mono");
        drop(aec);
    }

    /// Echo-suppression smoke test: simulate "speaker bleed of a tone
    /// into the mic" by feeding APM a 1 kHz tone as the far-end and an
    /// attenuated copy of the same tone as the near-end, repeated for
    /// long enough that APM's adaptive filter converges.
    ///
    /// We assert the **direction** (RMS shrinks), not a specific dB
    /// figure — APM's exact suppression depends on a delay estimator,
    /// HPF, and AEC3 internals that are not contractually stable
    /// across patch versions. The full §6.3 acceptance criterion
    /// (correlation < 0.15) lives in the manual test rig, not in unit
    /// tests; this is just a "did we wire reverse stream at all?"
    /// regression.
    #[test]
    fn near_end_rms_shrinks_after_aec_with_matching_far_end() {
        let mut aec = EchoCanceller::new().expect("APM must initialize");

        // 1 kHz sine at 48 kHz: phase increments by 2π * 1000 / 48000
        // per sample. Amplitude 0.5 stays well below clipping.
        let omega = 2.0 * std::f32::consts::PI * 1000.0 / APM_SAMPLE_RATE_HZ as f32;
        let attenuation = 0.3_f32;

        let mut total_input_rms = 0.0_f32;
        let mut total_output_rms = 0.0_f32;
        let mut measured_frames = 0_u32;

        // 200 frames = 2 s of synthetic audio. AEC3's adaptive filter
        // typically converges within ~500 ms; we measure RMS only
        // over the second half of the run so the warm-up doesn't bias
        // the comparison.
        const WARMUP_FRAMES: usize = 100;
        const TOTAL_FRAMES: usize = 200;

        for frame_index in 0..TOTAL_FRAMES {
            let far = frame_with(Channel::Tap, APM_FRAME_SAMPLES, |i| {
                let t = (frame_index * APM_FRAME_SAMPLES + i) as f32;
                (omega * t).sin() * 0.5
            });
            let mut near = frame_with(Channel::Mic, APM_FRAME_SAMPLES, |i| {
                let t = (frame_index * APM_FRAME_SAMPLES + i) as f32;
                // Near-end is just an attenuated copy of the same
                // tone — i.e. the mic only "hears" speaker bleed, no
                // user voice. APM should learn the echo path and
                // drive the output toward zero.
                (omega * t).sin() * 0.5 * attenuation
            });

            let input_rms = rms(&near.samples);
            aec.process_far_end(&far)
                .expect("far-end frame must process");
            aec.process_near_end(&mut near)
                .expect("near-end frame must process");
            let output_rms = rms(&near.samples);

            if frame_index >= WARMUP_FRAMES {
                total_input_rms += input_rms;
                total_output_rms += output_rms;
                measured_frames += 1;
            }
        }

        assert!(
            measured_frames > 0,
            "should have measured at least one frame"
        );
        let mean_input = total_input_rms / measured_frames as f32;
        let mean_output = total_output_rms / measured_frames as f32;
        assert!(
            mean_output < mean_input,
            "after AEC convergence, mean near-end RMS should shrink \
             (input mean RMS={mean_input}, output mean RMS={mean_output})"
        );
    }

    /// Wrong-channel rejection: feeding a Mic frame into the far-end
    /// path is a wiring bug we want to surface loudly, not absorb
    /// silently. If this assertion ever fires in production it means
    /// the broadcast plumbing got reshuffled.
    #[test]
    fn process_far_end_rejects_mic_channel() {
        let mut aec = EchoCanceller::new().expect("APM must initialize");
        let mic = frame_with(Channel::Mic, APM_FRAME_SAMPLES, |_| 0.0);
        let err = aec
            .process_far_end(&mic)
            .expect_err("Mic into far-end must be rejected");
        assert_aborted_contains(err, &["channel"]);
    }

    /// Symmetric guard: a Tap frame fed into the near-end path is also
    /// a wiring bug. APM has no notion of "this is the wrong stream";
    /// we have to enforce it ourselves.
    #[test]
    fn process_near_end_rejects_tap_channel() {
        let mut aec = EchoCanceller::new().expect("APM must initialize");
        let mut tap = frame_with(Channel::Tap, APM_FRAME_SAMPLES, |_| 0.0);
        let err = aec
            .process_near_end(&mut tap)
            .expect_err("Tap into near-end must be rejected");
        assert_aborted_contains(err, &["channel"]);
    }

    /// Wrong-frame-size rejection on the far-end path. APM's
    /// `process_render_frame` panics on a length mismatch; we
    /// pre-check so the caller gets a clean Result.
    #[test]
    fn process_far_end_rejects_short_frame() {
        let mut aec = EchoCanceller::new().expect("APM must initialize");
        let short = frame_with(Channel::Tap, 240, |_| 0.0);
        let err = aec
            .process_far_end(&short)
            .expect_err("short far-end frame must be rejected");
        assert_aborted_contains(err, &["frame size", "480"]);
    }

    /// Wrong-frame-size rejection on the near-end path. Symmetric to
    /// the far-end check above.
    #[test]
    fn process_near_end_rejects_short_frame() {
        let mut aec = EchoCanceller::new().expect("APM must initialize");
        let mut short = frame_with(Channel::Mic, 240, |_| 0.0);
        let err = aec
            .process_near_end(&mut short)
            .expect_err("short near-end frame must be rejected");
        assert_aborted_contains(err, &["frame size", "480"]);
    }
}
