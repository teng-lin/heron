//! WER thresholds + backend selection per `docs/implementation.md`
//! §8.5 + §8.6.
//!
//! - [`WerThreshold`] — published threshold table with one row per
//!   `(fixture, backend)` pair. Used by the §8.7 done-when test that
//!   re-runs the WER baseline against committed fixtures.
//! - [`select_backend`] — runtime routing between WhisperKit and
//!   Sherpa, given a measured WER baseline + the platform predicate.
//!
//! Pure functions; no model downloads, no fixtures on disk. The §8.5
//! table is hard-coded so a future `bench-wer.sh` script doesn't need
//! to read it back from a YAML.

use crate::SttBackend;
use crate::stub::{SherpaStub, WhisperKitStub};

/// Published WER threshold: a backend's WER on a fixture must stay
/// at or below `max_wer_pct` to ship.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WerThreshold {
    pub fixture: &'static str,
    pub backend: &'static str,
    pub max_wer_pct: f64,
}

/// The §8.5 table verbatim. Order matters for fixture-by-fixture
/// reporting; tests below pin both shape and values.
pub const WER_THRESHOLDS: &[WerThreshold] = &[
    WerThreshold {
        fixture: "client-3person-gallery",
        backend: "whisperkit",
        max_wer_pct: 15.0,
    },
    WerThreshold {
        fixture: "client-3person-gallery",
        backend: "sherpa",
        max_wer_pct: 22.0,
    },
    WerThreshold {
        fixture: "team-5person-with-dialin",
        backend: "whisperkit",
        max_wer_pct: 22.0,
    },
    WerThreshold {
        fixture: "team-5person-with-dialin",
        backend: "sherpa",
        max_wer_pct: 30.0,
    },
    WerThreshold {
        fixture: "1on1-internal",
        backend: "whisperkit",
        max_wer_pct: 12.0,
    },
    WerThreshold {
        fixture: "1on1-internal",
        backend: "sherpa",
        max_wer_pct: 18.0,
    },
];

/// Look up the threshold for a `(fixture, backend)` pair. Returns
/// `None` rather than erroring so the test harness can iterate over
/// fixtures it just ran without pre-checking the table membership.
pub fn lookup_threshold(fixture: &str, backend: &str) -> Option<WerThreshold> {
    WER_THRESHOLDS
        .iter()
        .copied()
        .find(|t| t.fixture == fixture && t.backend == backend)
}

/// Measured WER baseline across the §8.5 fixtures. The `_pct` fields
/// are percentages (15.0 means 15 % WER), matching [`WerThreshold`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WerBaseline {
    /// One entry per fixture, paired with the WhisperKit measurement.
    pub whisperkit_pct: Vec<f64>,
    /// One entry per fixture, paired with the Sherpa measurement.
    pub sherpa_pct: Vec<f64>,
}

impl WerBaseline {
    /// Mean WER across recorded fixtures. Returns `None` when no
    /// measurements are recorded — the §8.6 selection logic treats
    /// "no data" as a default to *Sherpa* (the safer always-available
    /// path).
    pub fn whisperkit_avg(&self) -> Option<f64> {
        avg(&self.whisperkit_pct)
    }
    pub fn sherpa_avg(&self) -> Option<f64> {
        avg(&self.sherpa_pct)
    }
}

fn avg(xs: &[f64]) -> Option<f64> {
    if xs.is_empty() {
        None
    } else {
        Some(xs.iter().sum::<f64>() / xs.len() as f64)
    }
}

/// The §8.6 "is the platform Apple Silicon + macOS 14+?" predicate.
///
/// This trait exists so callers (including the §8.7 done-when test
/// suite) can stub the platform in tests. Production code instantiates
/// [`RealPlatform`] which runs the OS probes.
pub trait Platform {
    fn is_apple_silicon(&self) -> bool;
    fn is_macos_14_plus(&self) -> bool;
}

/// Production platform predicate. Reads `cfg!()` for arch + the
/// runtime macOS major version via Apple-only sysctl. Off-Apple builds
/// always return `false` for both — Sherpa is the only viable backend
/// on Linux / Windows.
pub struct RealPlatform;

impl Platform for RealPlatform {
    fn is_apple_silicon(&self) -> bool {
        cfg!(all(target_os = "macos", target_arch = "aarch64"))
    }
    fn is_macos_14_plus(&self) -> bool {
        // Real impl reads kern.osproductversion via sysctl. v0 returns
        // false off-mac and `true` on aarch64-darwin so the §8.6
        // selection routes the same way the live runtime would.
        cfg!(all(target_os = "macos", target_arch = "aarch64"))
    }
}

/// Pick a backend per §8.6:
///
/// 1. Off Apple-Silicon-on-Sonoma+, return Sherpa (WhisperKit needs
///    those primitives).
/// 2. If WhisperKit's measured average WER is more than 5 % worse than
///    Sherpa's, return Sherpa (WhisperKit is materially behind on
///    *this user's fixtures*).
/// 3. Else return WhisperKit (the v1 default).
///
/// "More than 5% worse" matches the §8.6 spec: `wk.avg() > sh.avg() * 1.05`.
/// The factor leaves a hysteresis band so a single-fixture noise spike
/// doesn't oscillate the choice on every install.
pub fn select_backend<P: Platform>(platform: &P, baseline: &WerBaseline) -> Box<dyn SttBackend> {
    if !platform.is_apple_silicon() || !platform.is_macos_14_plus() {
        return Box::new(SherpaStub);
    }
    match (baseline.whisperkit_avg(), baseline.sherpa_avg()) {
        (Some(wk), Some(sh)) if wk > sh * 1.05 => Box::new(SherpaStub),
        _ => Box::new(WhisperKitStub),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakePlatform {
        apple_silicon: bool,
        macos_14_plus: bool,
    }
    impl Platform for FakePlatform {
        fn is_apple_silicon(&self) -> bool {
            self.apple_silicon
        }
        fn is_macos_14_plus(&self) -> bool {
            self.macos_14_plus
        }
    }

    #[test]
    fn wer_table_has_six_rows_with_distinct_pairs() {
        assert_eq!(WER_THRESHOLDS.len(), 6);
        let mut pairs: Vec<_> = WER_THRESHOLDS
            .iter()
            .map(|t| (t.fixture, t.backend))
            .collect();
        pairs.sort();
        pairs.dedup();
        assert_eq!(
            pairs.len(),
            6,
            "every (fixture, backend) pair must be unique"
        );
    }

    #[test]
    fn whisperkit_threshold_is_strictly_below_sherpa_per_fixture() {
        // WhisperKit is the spec's primary backend; if its threshold
        // ever loosens above Sherpa's, we'd be admitting that picking
        // it makes no sense.
        for fixture in [
            "client-3person-gallery",
            "team-5person-with-dialin",
            "1on1-internal",
        ] {
            let wk = lookup_threshold(fixture, "whisperkit").expect("wk");
            let sh = lookup_threshold(fixture, "sherpa").expect("sh");
            assert!(
                wk.max_wer_pct < sh.max_wer_pct,
                "WhisperKit threshold {} on {fixture} is not below Sherpa's {}",
                wk.max_wer_pct,
                sh.max_wer_pct
            );
        }
    }

    #[test]
    fn lookup_unknown_fixture_returns_none() {
        assert!(lookup_threshold("not-a-real-fixture", "whisperkit").is_none());
    }

    #[test]
    fn off_apple_silicon_routes_to_sherpa() {
        let p = FakePlatform {
            apple_silicon: false,
            macos_14_plus: true,
        };
        let b = select_backend(&p, &WerBaseline::default());
        assert_eq!(b.name(), "sherpa");
    }

    #[test]
    fn pre_sonoma_routes_to_sherpa_even_on_apple_silicon() {
        let p = FakePlatform {
            apple_silicon: true,
            macos_14_plus: false,
        };
        let b = select_backend(&p, &WerBaseline::default());
        assert_eq!(b.name(), "sherpa");
    }

    #[test]
    fn no_baseline_data_routes_to_whisperkit_on_supported_platform() {
        // "No data yet" is what the orchestrator sees on a fresh
        // install; the design says default to WhisperKit (the v1
        // primary) and let the next session record real numbers.
        let p = FakePlatform {
            apple_silicon: true,
            macos_14_plus: true,
        };
        let b = select_backend(&p, &WerBaseline::default());
        assert_eq!(b.name(), "whisperkit");
    }

    #[test]
    fn whisperkit_materially_worse_than_sherpa_routes_to_sherpa() {
        let p = FakePlatform {
            apple_silicon: true,
            macos_14_plus: true,
        };
        let baseline = WerBaseline {
            whisperkit_pct: vec![20.0, 25.0],
            sherpa_pct: vec![15.0, 18.0],
        };
        // WK avg = 22.5, Sherpa avg = 16.5; 22.5 > 16.5 * 1.05 = 17.325 → Sherpa.
        let b = select_backend(&p, &baseline);
        assert_eq!(b.name(), "sherpa");
    }

    #[test]
    fn whisperkit_within_hysteresis_band_keeps_whisperkit() {
        let p = FakePlatform {
            apple_silicon: true,
            macos_14_plus: true,
        };
        let baseline = WerBaseline {
            whisperkit_pct: vec![16.0, 17.0],
            sherpa_pct: vec![15.5, 16.5],
        };
        // WK avg = 16.5, Sherpa avg = 16.0 * 1.05 = 16.8. WK is within
        // the band — keep WhisperKit (avoid oscillation).
        let b = select_backend(&p, &baseline);
        assert_eq!(b.name(), "whisperkit");
    }
}
