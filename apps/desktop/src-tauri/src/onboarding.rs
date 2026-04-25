//! Per-step Test-button backends per `docs/implementation.md` §13.3.
//!
//! Each onboarding step (mic, audio-tap, accessibility, calendar,
//! model-download) ships a Tauri command the frontend can call to
//! verify the step completed. v0 returns structured [`TestOutcome`]
//! values shaped to the §13.3 acceptance table; the real probes plug
//! into the same surface in week 11.
//!
//! All five commands return [`TestOutcome`] rather than a typed
//! error, because the onboarding UI's job is to *render* the failure
//! mode (positive vs. counter-test), not propagate it. The
//! `details` field carries human-facing copy.
//!
//! v0 ↔ production transition: the §13.3 counter-tests verify the
//! "after deny" state. The stubs here always report `Skipped` with a
//! reason, so a UI exercise today proves the wiring; a real
//! exercise (week 11) flips the body to a probe call.

use serde::Serialize;

/// Result of one Test-button click.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TestOutcome {
    /// Probe ran and the step's positive criterion was met. `details`
    /// carries copy for the success badge.
    Pass { details: String },
    /// Probe ran and the step's positive criterion was *not* met.
    /// `details` carries copy for the inline error toast.
    Fail { details: String },
    /// Probe is not yet wired up. `details` documents what the real
    /// implementation will do. Distinct from `Fail` so the UI shows
    /// a "TODO" badge in dev builds rather than a hard error.
    Skipped { details: String },
}

impl TestOutcome {
    pub fn pass(details: impl Into<String>) -> Self {
        Self::Pass {
            details: details.into(),
        }
    }
    pub fn fail(details: impl Into<String>) -> Self {
        Self::Fail {
            details: details.into(),
        }
    }
    pub fn skipped(details: impl Into<String>) -> Self {
        Self::Skipped {
            details: details.into(),
        }
    }
    pub fn is_pass(&self) -> bool {
        matches!(self, Self::Pass { .. })
    }
}

/// **§13.3 step 1 — Microphone.** Real impl records 1 s and asserts
/// the level meter ever crossed -60 dB. v0 reports the §6.2
/// `AudioCapture` stub state.
pub fn test_microphone() -> TestOutcome {
    TestOutcome::skipped(
        "wires through heron_audio::AudioCapture once §6.2 ships; \
         today returns NotYetImplemented from the stub",
    )
}

/// Maximum length we'll accept for a frontend-supplied identifier.
/// Apple's bundle-id format itself caps far below this; the cap is to
/// stop a compromised renderer from echoing megabytes back at us.
const MAX_IDENT_LEN: usize = 255;

/// **§13.3 step 2 — System audio.** Real impl creates a process tap
/// against any open app for 1 s and asserts a non-silent waveform.
///
/// `target_bundle_id` is validated against the Apple bundle-id charset
/// (`[A-Za-z0-9._-]`, no spaces) so a compromised renderer can't echo
/// arbitrary text back into log lines or the UI.
pub fn test_audio_tap(target_bundle_id: &str) -> TestOutcome {
    let id = target_bundle_id.trim();
    if id.is_empty() {
        return TestOutcome::fail("target bundle id is empty");
    }
    if id.len() > MAX_IDENT_LEN {
        return TestOutcome::fail(format!("bundle id exceeds {MAX_IDENT_LEN} chars"));
    }
    if !id.chars().all(is_bundle_id_char) {
        return TestOutcome::fail("bundle id contains invalid characters; expected [A-Za-z0-9._-]");
    }
    TestOutcome::skipped(format!(
        "would tap bundle {id} via heron_audio once §6.2 ships"
    ))
}

/// **§13.3 step 3 — Accessibility.** Real impl invokes ax-probe and
/// asserts at least one AX element is returned. Backend selection is
/// internal to the Rust side (per §9.2 `select_ax_backend`); the
/// command takes no frontend-supplied parameter.
pub fn test_accessibility() -> TestOutcome {
    // Real impl: let backend = heron_zoom::select_ax_backend(); probe()
    // The Rust side authoritatively picks the AX backend; the frontend
    // doesn't get to name one.
    TestOutcome::skipped("would call select_ax_backend() and probe once §9 ships")
}

/// **§13.3 step 4 — Calendar.** Real impl probes EventKit's current
/// authorization status (no prompt) and reads a small window if
/// granted. The §12.2 denial contract says a denied user gets
/// `Ok(None)` within 100 ms — *not* an error — so the deny path is
/// also a `Pass` for onboarding purposes.
///
/// The function takes no parameter: an earlier draft accepted a
/// `has_access: bool` from the frontend, which inverted the trust
/// direction (only macOS authoritatively knows the access state) and
/// turned the test into a pure function of its input. v0 reports
/// `Skipped` until the §12.2 wires land.
pub fn test_calendar() -> TestOutcome {
    // Real impl: match heron_vault::calendar_read_one_shot(now, now + 24h) {
    //     Ok(Some(_)) => Pass("granted"),
    //     Ok(None)    => Pass("denied per §12.2 contract"),
    //     Err(e)      => Fail(e.to_string()),
    // }
    TestOutcome::skipped(
        "would probe EventKit authorization via heron_vault::calendar_read_one_shot \
         once §12.2 wires the no-prompt path",
    )
}

fn is_bundle_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

/// **§13.3 step 5 — Model download.** Real impl streams progress
/// from WhisperKit's downloader; on cancel mid-download the runtime
/// rolls back to sherpa per §13.3 counter-test. v0 reports the
/// step's stub state.
pub fn test_model_download(progress: f32) -> TestOutcome {
    if progress.is_nan() {
        return TestOutcome::fail("progress is NaN — caller bug");
    }
    if !(0.0..=1.0).contains(&progress) {
        return TestOutcome::fail(format!(
            "progress {progress} out of [0.0, 1.0] — caller bug"
        ));
    }
    if progress >= 1.0 {
        TestOutcome::pass("model download complete; WhisperKit will be selected next session")
    } else {
        TestOutcome::skipped(format!(
            "model download at {pct:.0}%; ensure_model() not yet wired (§8.2 stub)",
            pct = progress * 100.0
        ))
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn microphone_test_is_skipped_until_audio_lands() {
        let r = test_microphone();
        assert!(matches!(r, TestOutcome::Skipped { .. }));
    }

    #[test]
    fn audio_tap_rejects_empty_bundle() {
        let r = test_audio_tap("");
        assert!(matches!(r, TestOutcome::Fail { .. }));
    }

    #[test]
    fn audio_tap_rejects_whitespace_only_bundle() {
        // Trim before the empty-check so "   " doesn't slip through.
        let r = test_audio_tap("   ");
        assert!(matches!(r, TestOutcome::Fail { .. }));
    }

    #[test]
    fn audio_tap_with_bundle_skips_with_id_in_message() {
        let r = test_audio_tap("us.zoom.xos");
        match r {
            TestOutcome::Skipped { details } => assert!(details.contains("us.zoom.xos")),
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[test]
    fn audio_tap_rejects_invalid_chars() {
        // Apple bundle ids are ASCII alphanumerics + . _ -. Anything
        // else is a sign the renderer is feeding us junk.
        for bad in ["us zoom xos", "<script>", "bundle/with/slash", "你好"] {
            assert!(
                matches!(test_audio_tap(bad), TestOutcome::Fail { .. }),
                "expected Fail for {bad:?}"
            );
        }
    }

    #[test]
    fn audio_tap_rejects_overlong_bundle() {
        let huge = "a".repeat(MAX_IDENT_LEN + 1);
        assert!(matches!(test_audio_tap(&huge), TestOutcome::Fail { .. }));
    }

    #[test]
    fn accessibility_takes_no_arguments_and_skips() {
        // The frontend can't name the AX backend; the Rust side picks.
        let r = test_accessibility();
        assert!(matches!(r, TestOutcome::Skipped { .. }));
    }

    #[test]
    fn calendar_takes_no_arguments_and_skips() {
        // EventKit authorization state is only known to macOS, not the
        // frontend. The earlier draft accepted a bool from the renderer
        // and inverted the trust direction.
        let r = test_calendar();
        assert!(matches!(r, TestOutcome::Skipped { .. }));
    }

    #[test]
    fn model_download_complete_is_pass() {
        assert!(test_model_download(1.0).is_pass());
    }

    #[test]
    fn model_download_in_progress_is_skipped_with_pct() {
        match test_model_download(0.5) {
            TestOutcome::Skipped { details } => assert!(details.contains("50%")),
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[test]
    fn model_download_negative_is_fail() {
        assert!(matches!(
            test_model_download(-0.1),
            TestOutcome::Fail { .. }
        ));
    }

    #[test]
    fn model_download_over_one_is_fail() {
        assert!(matches!(test_model_download(1.1), TestOutcome::Fail { .. }));
    }

    #[test]
    fn model_download_nan_is_fail_with_clear_message() {
        match test_model_download(f32::NAN) {
            TestOutcome::Fail { details } => assert!(details.contains("NaN")),
            other => panic!("expected Fail with NaN message, got {other:?}"),
        }
    }

    #[test]
    fn model_download_infinity_is_fail() {
        assert!(matches!(
            test_model_download(f32::INFINITY),
            TestOutcome::Fail { .. }
        ));
    }

    #[test]
    fn pass_outcome_serializes_with_status_tag() {
        let s = serde_json::to_string(&TestOutcome::pass("ok")).expect("ser");
        assert!(s.contains(r#""status":"pass""#));
        assert!(s.contains(r#""details":"ok""#));
    }

    #[test]
    fn fail_outcome_serializes_with_status_tag() {
        // Lock the wire tag for every variant the frontend matches on.
        let s = serde_json::to_string(&TestOutcome::fail("nope")).expect("ser");
        assert!(s.contains(r#""status":"fail""#));
    }

    #[test]
    fn skipped_outcome_serializes_with_status_tag() {
        let s = serde_json::to_string(&TestOutcome::skipped("later")).expect("ser");
        assert!(s.contains(r#""status":"skipped""#));
    }
}
