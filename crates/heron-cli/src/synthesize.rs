//! `heron synthesize` — write a stub fixture directory to disk for
//! offline regression of the aligner / STT once the real backends
//! land.
//!
//! Per `docs/implementation.md` §19.5, each fixture under
//! `fixtures/<crate>/<case>/` is:
//!
//! - `mic.wav`  — 48 kHz mono, post-AEC user audio
//! - `tap.wav`  — 48 kHz mono, system-output capture
//! - `ax-events.jsonl`    — AX-event ground truth (zoom cases)
//! - `ground-truth.jsonl` — hand-labeled `Turn` records
//! - `README.md` — captured-at + hardware notes
//!
//! Real fixtures need a partner + a recording session. The
//! synthesized output here is **silent** PCM + canned events, useful
//! for exercising disk I/O paths, the §9.3 aligner, and the §8.4
//! partial-jsonl writer in unit tests without committing a real
//! recording into the repo.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use thiserror::Error;

const SAMPLE_RATE_HZ: u32 = 48_000;

/// What `synthesize_fixture` writes.
#[derive(Debug, Clone, Copy)]
pub struct SynthOptions {
    /// Length of each `.wav` in seconds. Capped at 300 (5 min) to
    /// avoid accidentally producing a multi-GB file from a typo'd
    /// flag.
    pub duration_secs: u32,
    /// How many AX speaker events to emit (linearly spaced over the
    /// duration). 0 produces an empty `ax-events.jsonl`.
    pub ax_events: u32,
    /// How many ground-truth turns to emit. 0 produces an empty
    /// `ground-truth.jsonl`.
    pub turns: u32,
}

impl Default for SynthOptions {
    fn default() -> Self {
        Self {
            duration_secs: 30,
            ax_events: 6,
            turns: 6,
        }
    }
}

#[derive(Debug, Error)]
pub enum SynthError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("duration {0}s exceeds max 300s safety cap")]
    DurationTooLarge(u32),
    #[error("duration must be > 0")]
    DurationZero,
}

const MAX_DURATION_SECS: u32 = 300;

/// Write a complete fixture directory rooted at `out_dir`. Creates
/// the directory if it doesn't exist; **refuses to overwrite an
/// existing non-empty directory** so a typo can't blow away real
/// captured fixtures.
pub fn synthesize_fixture(out_dir: &Path, opts: &SynthOptions) -> Result<(), SynthError> {
    if opts.duration_secs == 0 {
        return Err(SynthError::DurationZero);
    }
    if opts.duration_secs > MAX_DURATION_SECS {
        return Err(SynthError::DurationTooLarge(opts.duration_secs));
    }
    fs::create_dir_all(out_dir)?;
    if !is_empty_dir(out_dir)? {
        return Err(SynthError::Io(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "refusing to overwrite non-empty directory {}",
                out_dir.display()
            ),
        )));
    }

    write_silent_wav(&out_dir.join("mic.wav"), opts.duration_secs)?;
    write_silent_wav(&out_dir.join("tap.wav"), opts.duration_secs)?;
    write_ax_events_jsonl(&out_dir.join("ax-events.jsonl"), opts)?;
    write_ground_truth_jsonl(&out_dir.join("ground-truth.jsonl"), opts)?;
    write_readme(&out_dir.join("README.md"), opts)?;
    Ok(())
}

fn is_empty_dir(path: &Path) -> io::Result<bool> {
    Ok(fs::read_dir(path)?.next().is_none())
}

/// Write a silent 16-bit PCM WAV at 48 kHz mono. Header is built by
/// hand so we don't depend on the `hound` crate just for synthesis.
fn write_silent_wav(path: &Path, duration_secs: u32) -> io::Result<()> {
    let total_samples = (SAMPLE_RATE_HZ as u64) * u64::from(duration_secs);
    let total_bytes = total_samples * 2; // 16-bit mono = 2 bytes/sample
    let mut f = fs::File::create(path)?;
    write_wav_header(&mut f, total_bytes as u32)?;
    // Silence = all-zero PCM. Stream zeros in 8 KiB chunks rather than
    // allocating the full buffer (a 5 min run = 28 MiB).
    let zeros = [0u8; 8 * 1024];
    let mut remaining = total_bytes;
    while remaining > 0 {
        let take = remaining.min(zeros.len() as u64) as usize;
        f.write_all(&zeros[..take])?;
        remaining -= take as u64;
    }
    f.sync_all()?;
    Ok(())
}

fn write_wav_header(f: &mut fs::File, pcm_bytes: u32) -> io::Result<()> {
    // RIFF header.
    f.write_all(b"RIFF")?;
    f.write_all(&(pcm_bytes + 36).to_le_bytes())?;
    f.write_all(b"WAVE")?;
    // fmt chunk.
    f.write_all(b"fmt ")?;
    f.write_all(&16u32.to_le_bytes())?; // chunk size
    f.write_all(&1u16.to_le_bytes())?; // PCM
    f.write_all(&1u16.to_le_bytes())?; // mono
    f.write_all(&SAMPLE_RATE_HZ.to_le_bytes())?;
    f.write_all(&(SAMPLE_RATE_HZ * 2).to_le_bytes())?; // byte rate
    f.write_all(&2u16.to_le_bytes())?; // block align
    f.write_all(&16u16.to_le_bytes())?; // bits per sample
    // data chunk.
    f.write_all(b"data")?;
    f.write_all(&pcm_bytes.to_le_bytes())?;
    Ok(())
}

fn write_ax_events_jsonl(path: &Path, opts: &SynthOptions) -> io::Result<()> {
    let mut f = fs::File::create(path)?;
    if opts.ax_events == 0 {
        return Ok(());
    }
    let step = opts.duration_secs as f64 / opts.ax_events as f64;
    for i in 0..opts.ax_events {
        let at = (i as f64) * step;
        // SpeakerEvent in heron-types is the canonical wire shape.
        // We emit a minimal schema-compatible JSON line; the real
        // structure is exercised by tests in the consuming crate.
        writeln!(
            f,
            r#"{{"kind":"ax_speaker_changed","at":{at:.3},"speaker":"speaker-{i}"}}"#
        )?;
    }
    f.sync_all()?;
    Ok(())
}

fn write_ground_truth_jsonl(path: &Path, opts: &SynthOptions) -> io::Result<()> {
    let mut f = fs::File::create(path)?;
    if opts.turns == 0 {
        return Ok(());
    }
    let step = opts.duration_secs as f64 / opts.turns as f64;
    for i in 0..opts.turns {
        let t0 = (i as f64) * step;
        let t1 = t0 + step;
        let speaker = if i % 2 == 0 { "me" } else { "them" };
        let speaker_source = if i % 2 == 0 { "self" } else { "channel" };
        let channel = if i % 2 == 0 { "mic" } else { "tap" };
        writeln!(
            f,
            r#"{{"t0":{t0:.3},"t1":{t1:.3},"text":"synthetic turn {i}","channel":"{channel}","speaker":"{speaker}","speaker_source":"{speaker_source}","confidence":0.95}}"#
        )?;
    }
    f.sync_all()?;
    Ok(())
}

fn write_readme(path: &Path, opts: &SynthOptions) -> io::Result<()> {
    let body = format!(
        "# Synthetic fixture\n\n\
         Generated by `heron synthesize`. **Silent PCM + canned events** —\n\
         not a real recording. Useful for offline regression of disk-I/O\n\
         paths, §9.3 aligner code, and the §8.4 partial-jsonl writer.\n\n\
         Replace with real captures (per the §19.5 capture procedure)\n\
         before relying on this for WER / aligner accuracy.\n\n\
         | Field | Value |\n\
         |---|---|\n\
         | duration_secs | {dur} |\n\
         | ax_events | {events} |\n\
         | turns | {turns} |\n\
         | sample_rate_hz | {rate} |\n",
        dur = opts.duration_secs,
        events = opts.ax_events,
        turns = opts.turns,
        rate = SAMPLE_RATE_HZ,
    );
    fs::write(path, body)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn synthesize_writes_all_five_files() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let out = tmp.path().join("synth-1");
        synthesize_fixture(&out, &SynthOptions::default()).expect("synth");
        for f in [
            "mic.wav",
            "tap.wav",
            "ax-events.jsonl",
            "ground-truth.jsonl",
            "README.md",
        ] {
            assert!(out.join(f).exists(), "expected {f} in fixture");
        }
    }

    #[test]
    fn wav_header_round_trips_via_riff_size() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let out = tmp.path().join("synth-wav");
        synthesize_fixture(
            &out,
            &SynthOptions {
                duration_secs: 1,
                ax_events: 0,
                turns: 0,
            },
        )
        .expect("synth");
        let mic = std::fs::read(out.join("mic.wav")).expect("read");
        // RIFF + 4 bytes size + WAVE + 16-byte fmt + data hdr (8) +
        // 1 sec mono 16-bit @ 48k = 96000 bytes PCM = 96044 total.
        assert_eq!(mic.len(), 96044);
        assert_eq!(&mic[..4], b"RIFF");
        assert_eq!(&mic[8..12], b"WAVE");
    }

    #[test]
    fn jsonl_event_count_matches_options() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let out = tmp.path().join("synth-events");
        synthesize_fixture(
            &out,
            &SynthOptions {
                duration_secs: 10,
                ax_events: 4,
                turns: 5,
            },
        )
        .expect("synth");
        let ax = std::fs::read_to_string(out.join("ax-events.jsonl")).expect("ax");
        assert_eq!(ax.lines().count(), 4);
        let gt = std::fs::read_to_string(out.join("ground-truth.jsonl")).expect("gt");
        assert_eq!(gt.lines().count(), 5);
    }

    #[test]
    fn refuses_to_overwrite_non_empty_dir() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let out = tmp.path().join("existing");
        std::fs::create_dir_all(&out).expect("mkdir");
        std::fs::write(out.join("preexisting.txt"), b"do not delete me").expect("seed");
        let err = synthesize_fixture(&out, &SynthOptions::default()).expect_err("must refuse");
        match err {
            SynthError::Io(e) => assert_eq!(e.kind(), io::ErrorKind::AlreadyExists),
            other => panic!("expected Io::AlreadyExists, got {other:?}"),
        }
        assert!(
            out.join("preexisting.txt").exists(),
            "preexisting file must survive"
        );
    }

    #[test]
    fn duration_zero_is_rejected() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let out = tmp.path().join("zero");
        let err = synthesize_fixture(
            &out,
            &SynthOptions {
                duration_secs: 0,
                ..SynthOptions::default()
            },
        )
        .expect_err("zero must error");
        assert!(matches!(err, SynthError::DurationZero));
    }

    #[test]
    fn duration_above_cap_is_rejected() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let out = tmp.path().join("huge");
        let err = synthesize_fixture(
            &out,
            &SynthOptions {
                duration_secs: 9999,
                ..SynthOptions::default()
            },
        )
        .expect_err("over-cap must error");
        assert!(matches!(err, SynthError::DurationTooLarge(9999)));
    }

    #[test]
    fn empty_existing_dir_is_acceptable() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let out = tmp.path().join("empty");
        std::fs::create_dir_all(&out).expect("mkdir");
        synthesize_fixture(&out, &SynthOptions::default()).expect("synth");
        assert!(out.join("README.md").exists());
    }
}
