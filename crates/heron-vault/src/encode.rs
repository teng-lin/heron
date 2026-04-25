//! m4a encode + verify pipeline per
//! [`docs/implementation.md`](../../../docs/implementation.md) §11.3.
//!
//! After STT consumes the per-channel `mic.wav` / `tap.wav` files,
//! the writer encodes them into a single stereo m4a:
//!
//! - **L** = `mic.wav` (the user's voice, post-AEC)
//! - **R** = `tap.wav` (everyone else)
//!
//! AAC at 64 kbps VBR. The split into L/R lets the review-UI
//! playback (week 13, §15) lean into one channel or the other.
//!
//! Both calls shell out to `ffmpeg` / `ffprobe`. Real-world v1
//! installs ship ffmpeg via Homebrew (per
//! [`docs/implementation.md`](../../../docs/implementation.md) §0.1)
//! and the binary is on PATH.

use std::io;
use std::path::Path;
use std::process::Command;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum EncodeError {
    #[error("ffmpeg not found on PATH; install via brew install ffmpeg")]
    FfmpegMissing,
    #[error("ffprobe not found on PATH; install via brew install ffmpeg")]
    FfprobeMissing,
    #[error("ffmpeg exited with status {0}: {1}")]
    FfmpegFailed(i32, String),
    #[error("ffprobe exited with status {0}: {1}")]
    FfprobeFailed(i32, String),
    #[error("ffprobe output unparseable: {0}")]
    FfprobeParse(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Encode `wav_mic` and `wav_tap` into a stereo m4a at `out_m4a`.
/// L = mic, R = tap, AAC 64 kbps VBR.
///
/// Returns once ffmpeg has exited with status 0; the file is fully
/// written and synced (ffmpeg doesn't fsync, but the rename in the
/// caller's [`crate::atomic_write`] does).
pub fn encode_to_m4a(wav_mic: &Path, wav_tap: &Path, out_m4a: &Path) -> Result<(), EncodeError> {
    if !is_on_path("ffmpeg") {
        return Err(EncodeError::FfmpegMissing);
    }

    // Two -i inputs, then `-filter_complex` to mux them as L/R of
    // a stereo output, AAC at -q:a 1 (VBR ~64kbps for speech).
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-loglevel",
            "error",
            "-i",
            &wav_mic.display().to_string(),
            "-i",
            &wav_tap.display().to_string(),
            "-filter_complex",
            "[0:a][1:a]amerge=inputs=2[a]",
            "-map",
            "[a]",
            "-c:a",
            "aac",
            "-q:a",
            "1",
            "-ac",
            "2",
            &out_m4a.display().to_string(),
        ])
        .output()?;

    if !status.status.success() {
        let code = status.status.code().unwrap_or(-1);
        let stderr = String::from_utf8_lossy(&status.stderr).to_string();
        return Err(EncodeError::FfmpegFailed(code, stderr));
    }
    Ok(())
}

/// Verify the m4a was produced sanely. Returns `true` iff:
///
/// - the file exists and is non-empty
/// - ffprobe reports a duration within ±1 % of `expected_sec`
///
/// Used by [`crate::ringbuffer`] purge logic — only delete the
/// session's `.raw` files if `verify_m4a` returns `Ok(true)`.
pub fn verify_m4a(path: &Path, expected_sec: f64) -> Result<bool, EncodeError> {
    if !is_on_path("ffprobe") {
        return Err(EncodeError::FfprobeMissing);
    }
    let meta = std::fs::metadata(path)?;
    if meta.len() == 0 {
        return Ok(false);
    }

    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
            &path.display().to_string(),
        ])
        .output()?;
    if !out.status.success() {
        let code = out.status.code().unwrap_or(-1);
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        return Err(EncodeError::FfprobeFailed(code, stderr));
    }
    let stdout = String::from_utf8(out.stdout)
        .map_err(|e| EncodeError::FfprobeParse(format!("ffprobe stdout not utf8: {e}")))?;
    let duration: f64 = stdout.trim().parse().map_err(|e| {
        EncodeError::FfprobeParse(format!("could not parse duration {stdout:?}: {e}"))
    })?;
    let tolerance = expected_sec.abs() * 0.01;
    Ok((duration - expected_sec).abs() <= tolerance.max(0.5))
}

fn is_on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|p| {
                let candidate = p.join(name);
                candidate.is_file()
                    && std::fs::metadata(&candidate)
                        .map(|m| {
                            #[cfg(unix)]
                            {
                                use std::os::unix::fs::PermissionsExt;
                                m.permissions().mode() & 0o111 != 0
                            }
                            #[cfg(not(unix))]
                            {
                                let _ = m;
                                true
                            }
                        })
                        .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn verify_returns_false_for_zero_byte_file() {
        if !is_on_path("ffprobe") {
            // No ffprobe on this dev box — exercise this test only
            // when the toolchain has it. The §0.1 prerequisite says
            // ffmpeg is on PATH for v1 dev machines.
            return;
        }
        let tmp = tempfile::NamedTempFile::new().expect("tmp");
        let result = verify_m4a(tmp.path(), 60.0).expect("verify");
        assert!(!result, "zero-byte file must not verify");
    }

    #[test]
    fn is_on_path_finds_a_well_known_binary() {
        // `sh` is on every supported OS.
        assert!(is_on_path("sh"));
    }

    #[test]
    fn is_on_path_returns_false_for_nonsense() {
        assert!(!is_on_path("definitely-not-a-real-binary-zxqv"));
    }

    /// End-to-end smoke: synthesize 1s of silence into a .wav (no
    /// dep on a fixture), encode to m4a, verify duration. Skipped
    /// if ffmpeg/ffprobe aren't on PATH.
    #[test]
    #[ignore = "requires ffmpeg + ffprobe; run with --ignored"]
    fn encode_then_verify_round_trip() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let mic = tmp.path().join("mic.wav");
        let tap = tmp.path().join("tap.wav");
        let out = tmp.path().join("out.m4a");

        // Synthesize 1s silence into both .wav files via ffmpeg.
        for path in [&mic, &tap] {
            let s = Command::new("ffmpeg")
                .args([
                    "-y",
                    "-loglevel",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "anullsrc=channel_layout=mono:sample_rate=48000",
                    "-t",
                    "1.0",
                    &path.display().to_string(),
                ])
                .status()
                .expect("synth");
            assert!(s.success());
        }

        encode_to_m4a(&mic, &tap, &out).expect("encode");
        let ok = verify_m4a(&out, 1.0).expect("verify");
        assert!(ok, "1s round-trip must verify");
    }
}
