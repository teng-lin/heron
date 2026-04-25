//! Incremental JSONL writer per `plan.md` §3.5 + §8.4.
//!
//! Buffers transcribed [`Turn`]s in memory and fsyncs them to disk in
//! batches so a SIGKILL during STT leaves a `.partial` file with at
//! most ~10 turns or 5 seconds of buffered output unwritten. The §14
//! crash-recovery scan picks up the `.partial` and either resumes the
//! STT pass from `t1` of the last turn or hands the truncated result
//! to the user with a "salvaged" badge.
//!
//! The writer is **not async**: the STT worker thread invokes
//! [`PartialWriter::push`] synchronously per finalized turn, and the
//! writer fsyncs from the same thread when its thresholds trip. This
//! keeps the disk-flush behavior deterministic (no executor weight)
//! and avoids cross-thread shipping of `Turn`s on the audio path.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use heron_types::Turn;
use serde_json;
use thiserror::Error;

/// Flush every N pushed turns. Per §8.4 ("fsyncs every 10 turns or 5s").
pub const FLUSH_TURNS: usize = 10;

/// Flush every D since last flush. Per §8.4.
pub const FLUSH_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Error)]
pub enum PartialWriterError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("failed to serialize turn: {0}")]
    Serialize(#[from] serde_json::Error),
}

/// Append-only `.partial` JSONL writer.
///
/// One line per [`Turn`]. The writer never rewrites or truncates; if
/// the STT pass produces a turn that supersedes an earlier partial,
/// the consumer (week 14 crash-recovery + week 13 review UI) is
/// responsible for de-duping — the on-disk file is the authoritative
/// audit log of "what STT thought at the time."
pub struct PartialWriter {
    path: PathBuf,
    file: BufWriter<File>,
    pending_turns: usize,
    last_flush: Instant,
    /// Total turns ever pushed (across all flushes). Survives a flush
    /// reset; useful for the diagnostics tab.
    total_pushed: usize,
}

impl PartialWriter {
    /// Open `path` for appending. Creates parent directories if needed
    /// so the caller doesn't have to special-case first-run state.
    pub fn create(path: PathBuf) -> Result<Self, PartialWriterError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)?;
        Ok(Self {
            path,
            file: BufWriter::new(file),
            pending_turns: 0,
            last_flush: Instant::now(),
            total_pushed: 0,
        })
    }

    /// Append one [`Turn`]. Auto-flushes when the §8.4 thresholds trip.
    ///
    /// The serialization always lands in the BufWriter; the *fsync*
    /// only fires on a flush. This means a SIGKILL between flushes
    /// loses at most `FLUSH_TURNS - 1` turns (or `FLUSH_INTERVAL` of
    /// real time), which is the documented v1 ceiling.
    pub fn push(&mut self, turn: &Turn) -> Result<(), PartialWriterError> {
        let mut json = serde_json::to_string(turn)?;
        json.push('\n');
        self.file.write_all(json.as_bytes())?;
        self.pending_turns += 1;
        self.total_pushed += 1;

        if self.pending_turns >= FLUSH_TURNS || self.last_flush.elapsed() >= FLUSH_INTERVAL {
            self.flush_now()?;
        }
        Ok(())
    }

    /// Force-flush + fsync. Called by [`PartialWriter::push`] when the
    /// thresholds trip, by [`PartialWriter::finalize`] at end-of-pass,
    /// and by the §14 recovery flow before reading back.
    pub fn flush_now(&mut self) -> Result<(), PartialWriterError> {
        self.file.flush()?;
        self.file.get_ref().sync_all()?;
        self.pending_turns = 0;
        self.last_flush = Instant::now();
        Ok(())
    }

    /// Finalize the writer. Performs a final flush; the file remains
    /// at the same path with the `.partial` extension so the §14
    /// recovery flow + the week-13 finalize-pass can find it.
    pub fn finalize(mut self) -> Result<(), PartialWriterError> {
        self.flush_now()
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn total_pushed(&self) -> usize {
        self.total_pushed
    }
}

/// Read every [`Turn`] back from a `.partial` JSONL file.
///
/// Tolerates a truncated final line (a SIGKILL mid-write) by silently
/// dropping it. Any earlier line that fails to parse, however, is
/// surfaced as an error — partial-line tolerance is for the *tail*
/// only.
pub fn read_partial_jsonl(path: &std::path::Path) -> Result<Vec<Turn>, PartialWriterError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let text = String::from_utf8_lossy(&bytes);
    let mut out = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Turn>(line) {
            Ok(turn) => out.push(turn),
            Err(e) => {
                // Truncated final line is a SIGKILL signature: only
                // the *last* line gets the benefit of the doubt.
                if lines.peek().is_none() {
                    break;
                }
                return Err(PartialWriterError::Serialize(e));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use heron_types::{Channel, SpeakerSource};

    fn fixture(t0: f64, text: &str) -> Turn {
        Turn {
            t0,
            t1: t0 + 1.0,
            text: text.to_owned(),
            channel: Channel::Mic,
            speaker: "me".to_owned(),
            speaker_source: SpeakerSource::Self_,
            confidence: Some(0.9),
        }
    }

    #[test]
    fn push_then_flush_writes_one_line_per_turn() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let path = tmp.path().join("a.partial");
        let mut w = PartialWriter::create(path.clone()).expect("create");
        for i in 0..3 {
            w.push(&fixture(i as f64, &format!("line {i}")))
                .expect("push");
        }
        w.finalize().expect("finalize");

        let lines: Vec<_> = std::fs::read_to_string(&path)
            .expect("read")
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(lines.len(), 3);
        for (i, line) in lines.iter().enumerate() {
            let parsed: Turn = serde_json::from_str(line).expect("parse");
            assert_eq!(parsed.text, format!("line {i}"));
        }
    }

    #[test]
    fn auto_flushes_after_flush_turns_threshold() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let path = tmp.path().join("b.partial");
        let mut w = PartialWriter::create(path.clone()).expect("create");
        // Push exactly FLUSH_TURNS turns: the FLUSH_TURNS-th push must
        // flush, so the file is fully synced even if the writer is
        // dropped without finalize().
        for i in 0..FLUSH_TURNS {
            w.push(&fixture(i as f64, "x")).expect("push");
        }
        // Don't finalize — the auto-flush must already have synced.
        let on_disk = std::fs::read_to_string(&path).expect("read");
        assert_eq!(on_disk.lines().count(), FLUSH_TURNS);
        assert_eq!(w.total_pushed(), FLUSH_TURNS);
    }

    #[test]
    fn read_round_trips_pushed_turns() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let path = tmp.path().join("c.partial");
        let mut w = PartialWriter::create(path.clone()).expect("create");
        for i in 0..5 {
            w.push(&fixture(i as f64, &format!("t{i}"))).expect("push");
        }
        w.finalize().expect("finalize");

        let read_back = read_partial_jsonl(&path).expect("read");
        assert_eq!(read_back.len(), 5);
        for (i, t) in read_back.iter().enumerate() {
            assert_eq!(t.text, format!("t{i}"));
        }
    }

    #[test]
    fn read_tolerates_truncated_final_line() {
        // Simulate a SIGKILL mid-write: append a partial JSON object
        // after the proper lines. read_partial_jsonl must drop the
        // tail rather than erroring — that's the SIGKILL contract.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let path = tmp.path().join("d.partial");

        let good = serde_json::to_string(&fixture(0.0, "good")).expect("ser");
        let truncated = "{\"t0\":1.0,\"t1\":2.0,\"text\":\"";
        std::fs::write(&path, format!("{good}\n{truncated}")).expect("seed");

        let read = read_partial_jsonl(&path).expect("read");
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].text, "good");
    }

    #[test]
    fn read_errors_on_corrupt_non_final_line() {
        // Earlier lines have no SIGKILL excuse — their corruption is
        // a real bug and must be loud.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let path = tmp.path().join("e.partial");

        let good = serde_json::to_string(&fixture(0.0, "good")).expect("ser");
        let bad_then_good = format!("{{not json\n{good}\n");
        std::fs::write(&path, bad_then_good).expect("seed");

        let result = read_partial_jsonl(&path);
        assert!(matches!(result, Err(PartialWriterError::Serialize(_))));
    }

    #[test]
    fn read_returns_empty_when_file_missing() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let read = read_partial_jsonl(&tmp.path().join("never.partial")).expect("read");
        assert!(read.is_empty());
    }

    #[test]
    fn create_makes_parent_dirs() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let nested = tmp.path().join("a/b/c/x.partial");
        let _ = PartialWriter::create(nested.clone()).expect("create");
        assert!(nested.exists());
    }
}
