//! Disk-backed ringbuffer for live audio sessions.
//!
//! Per [`docs/implementation.md`](../../../docs/implementation.md) §7.2:
//! every captured frame lands in
//! `~/Library/Caches/heron/sessions/<id>/{mic,tap}.raw` (mode 0600),
//! and `session.json` is updated on every state transition so a
//! mid-session SIGKILL leaves a recoverable corpus on disk for the
//! crash-recovery scan in §14 (week 12).
//!
//! The on-disk format is intentionally trivial: little-endian `f32`
//! PCM, 48 kHz mono, one file per channel. No headers, no framing.
//! Sample count = file_size / 4. STT (week 4–5) muxes back to wav at
//! consume time; the m4a archival encode (week 9) is a separate pass
//! that reads from `mic_clean.raw` once it exists.
//!
//! This module ships the v0 surface: `Ringbuffer::new`, `push`,
//! `flush`, `snapshot`, plus `recover`. Real audio-thread integration
//! lands week 2; the unit tests below validate the disk-format
//! round-trip and the `session.json` resume contract today.

use std::fs::{File, OpenOptions, create_dir_all};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use heron_types::{Channel, SessionId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::CaptureFrame;

const MIC_FILENAME: &str = "mic.raw";
const TAP_FILENAME: &str = "tap.raw";
const SESSION_FILENAME: &str = "session.json";

#[derive(Debug, Error)]
pub enum RingbufferError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("session.json missing or unreadable at {path}")]
    SessionMetaMissing { path: PathBuf },
}

/// Snapshot of ringbuffer state, persisted to `session.json` on every
/// flush so the crash-recovery scan can pick up where the previous
/// run left off.
///
/// `mic_frames` and `tap_frames` are sample counts. `dropped_frames`
/// is the running count of frames the realtime → APM → ringbuffer
/// pipeline dropped under back-pressure (§7.4).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RingbufferState {
    pub session_id: SessionId,
    pub started_at: DateTime<Utc>,
    pub mic_frames: u64,
    pub tap_frames: u64,
    pub dropped_frames: u32,
    pub last_state_change: DateTime<Utc>,
}

/// Disk-backed buffer for one session's PCM streams.
pub struct Ringbuffer {
    session_dir: PathBuf,
    mic: BufWriter<File>,
    tap: BufWriter<File>,
    state: RingbufferState,
}

impl Ringbuffer {
    /// Open or create the session directory under `cache_root`,
    /// open `mic.raw` and `tap.raw` for appending, and write an
    /// initial `session.json`. Append-mode means re-opening an
    /// existing directory continues from where it left off — useful
    /// for the crash-recovery resume path.
    pub fn new(session_id: SessionId, cache_root: &Path) -> Result<Self, RingbufferError> {
        let session_dir = cache_root.join("sessions").join(session_id.to_string());
        create_dir_all(&session_dir)?;

        let mic_path = session_dir.join(MIC_FILENAME);
        let tap_path = session_dir.join(TAP_FILENAME);

        let mic_file = open_append(&mic_path)?;
        let tap_file = open_append(&tap_path)?;
        let mic_frames = mic_file.metadata()?.len() / 4;
        let tap_frames = tap_file.metadata()?.len() / 4;

        let now: DateTime<Utc> = SystemTime::now().into();
        let state = RingbufferState {
            session_id,
            started_at: now,
            mic_frames,
            tap_frames,
            dropped_frames: 0,
            last_state_change: now,
        };
        write_session_json(&session_dir, &state)?;

        Ok(Self {
            session_dir,
            mic: BufWriter::new(mic_file),
            tap: BufWriter::new(tap_file),
            state,
        })
    }

    /// Append a single [`CaptureFrame`] to the matching channel's
    /// `.raw` file.
    ///
    /// Updates the in-memory frame counters; `flush` persists them
    /// to `session.json`. Callers should `flush` on every state
    /// transition (mute toggle, device change, etc.) per §7.2.
    ///
    /// [`Channel::MicClean`] frames are intentionally ignored here —
    /// the disk-spill format only stores the two raw inputs (so AEC
    /// can be re-run offline against `mic.raw` + `tap.raw` if the APM
    /// config changes). The cleaned mic stream is written separately
    /// to `mic_clean.wav` at session stop by [`crate::wav_writer`].
    pub fn push(&mut self, frame: &CaptureFrame) -> Result<(), RingbufferError> {
        let writer = match frame.channel {
            Channel::Mic => &mut self.mic,
            Channel::Tap => &mut self.tap,
            // §7.2 ringbuffer is for raw inputs only; MicClean lives
            // in the broadcast → wav_writer path. Skip silently.
            Channel::MicClean => return Ok(()),
        };
        for sample in &frame.samples {
            writer.write_all(&sample.to_le_bytes())?;
        }
        match frame.channel {
            Channel::Mic => self.state.mic_frames += frame.samples.len() as u64,
            Channel::Tap => self.state.tap_frames += frame.samples.len() as u64,
            // Unreachable due to early return above, but spelled out
            // for the exhaustiveness checker.
            Channel::MicClean => {}
        }
        Ok(())
    }

    /// Increment the dropped-frame counter. Called by [`crate::backpressure`]
    /// when the SPSC ring overflows; the running total is exposed in
    /// the next `session.json` write and ultimately on
    /// [`crate::StopArtifacts::dropped_frames`].
    pub fn record_dropped(&mut self, count: u32) {
        self.state.dropped_frames = self.state.dropped_frames.saturating_add(count);
    }

    /// Flush in-memory PCM buffers to disk and re-write
    /// `session.json` with the latest counters. Call on every state
    /// transition.
    pub fn flush(&mut self) -> Result<(), RingbufferError> {
        self.mic.flush()?;
        self.tap.flush()?;
        self.state.last_state_change = SystemTime::now().into();
        write_session_json(&self.session_dir, &self.state)?;
        Ok(())
    }

    /// Read-only snapshot of the current state.
    pub fn snapshot(&self) -> RingbufferState {
        self.state.clone()
    }

    /// The session directory on disk.
    pub fn session_dir(&self) -> &Path {
        &self.session_dir
    }
}

impl Drop for Ringbuffer {
    fn drop(&mut self) {
        // Best-effort flush so a forgotten `flush` call doesn't lose
        // the last in-memory chunk on a clean process exit. Errors
        // are intentionally swallowed: there's nothing to do with
        // them in Drop, and a half-written session is still better
        // than a zero-byte one.
        let _ = self.mic.flush();
        let _ = self.tap.flush();
        let _ = write_session_json(&self.session_dir, &self.state);
    }
}

/// Recover state from a previous session's `session.json` without
/// taking ownership of the writers. Used by the crash-recovery scan
/// (week 12, §14) to enumerate salvageable sessions before deciding
/// which to resume.
pub fn recover(
    session_id: SessionId,
    cache_root: &Path,
) -> Result<RingbufferState, RingbufferError> {
    let session_dir = cache_root.join("sessions").join(session_id.to_string());
    let path = session_dir.join(SESSION_FILENAME);
    let mut file = File::open(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            RingbufferError::SessionMetaMissing { path: path.clone() }
        } else {
            RingbufferError::Io(e)
        }
    })?;
    file.seek(SeekFrom::Start(0))?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    let state: RingbufferState = serde_json::from_str(&buf)?;
    Ok(state)
}

fn open_append(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(path)
}

/// Enumerate sessions that have a `session.json` on disk under
/// `cache_root/sessions/`. Used by the §14 crash-recovery scan
/// (week 12) at app launch — the user is shown a "salvage these
/// sessions?" list, picks which to resume.
///
/// Returns sessions in undefined order. Callers that need a stable
/// order (e.g. by `started_at`) should sort the returned `Vec`.
///
/// Sessions whose `session.json` is corrupt are silently skipped;
/// the recovery UI deliberately doesn't show un-parseable corpora
/// since there's nothing the user can do about them.
pub fn scan_recoverable_sessions(cache_root: &Path) -> Vec<RingbufferState> {
    let sessions_dir = cache_root.join("sessions");
    let entries = match std::fs::read_dir(&sessions_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(), // no sessions dir = nothing to recover
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let dir_name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n,
            None => continue,
        };
        let session_id = match SessionId::parse_str(dir_name) {
            Ok(id) => id,
            Err(_) => continue, // not a uuid-named directory; skip
        };
        if let Ok(state) = recover(session_id, cache_root) {
            out.push(state);
        }
    }
    out
}

fn write_session_json(dir: &Path, state: &RingbufferState) -> Result<(), RingbufferError> {
    let path = dir.join(SESSION_FILENAME);
    let tmp = dir.join(format!("{SESSION_FILENAME}.tmp"));
    let mut f = File::create(&tmp)?;
    serde_json::to_writer_pretty(&mut f, state)?;
    f.flush()?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use heron_types::Channel;
    use tempfile::TempDir;

    fn frame(channel: Channel, samples: &[f32]) -> CaptureFrame {
        CaptureFrame {
            channel,
            host_time: 0,
            session_secs: 0.0,
            samples: samples.to_vec(),
        }
    }

    #[test]
    fn round_trip_preserves_pcm_bytes() {
        let tmp = TempDir::new().expect("tmpdir");
        let session = SessionId::nil();
        let mut rb = Ringbuffer::new(session, tmp.path()).expect("new");

        let mic_samples = vec![0.1f32, -0.2, 0.3, -0.4];
        let tap_samples = vec![0.5f32, 0.6];
        rb.push(&frame(Channel::Mic, &mic_samples))
            .expect("mic push");
        rb.push(&frame(Channel::Tap, &tap_samples))
            .expect("tap push");
        rb.flush().expect("flush");

        let session_dir = tmp.path().join("sessions").join(session.to_string());
        let mic_bytes = std::fs::read(session_dir.join(MIC_FILENAME)).expect("read mic");
        assert_eq!(mic_bytes.len(), mic_samples.len() * 4);

        let mut roundtripped = Vec::new();
        for chunk in mic_bytes.chunks_exact(4) {
            let bytes: [u8; 4] = chunk.try_into().expect("chunk");
            roundtripped.push(f32::from_le_bytes(bytes));
        }
        assert_eq!(roundtripped, mic_samples);
    }

    #[test]
    fn snapshot_reports_frame_counts() {
        let tmp = TempDir::new().expect("tmpdir");
        let session = SessionId::nil();
        let mut rb = Ringbuffer::new(session, tmp.path()).expect("new");

        rb.push(&frame(Channel::Mic, &[0.0; 480])).expect("mic");
        rb.push(&frame(Channel::Mic, &[0.0; 480])).expect("mic");
        rb.push(&frame(Channel::Tap, &[0.0; 480])).expect("tap");

        let snap = rb.snapshot();
        assert_eq!(snap.mic_frames, 960);
        assert_eq!(snap.tap_frames, 480);
        assert_eq!(snap.dropped_frames, 0);
    }

    #[test]
    fn dropped_frames_saturating_add() {
        let tmp = TempDir::new().expect("tmpdir");
        let mut rb = Ringbuffer::new(SessionId::nil(), tmp.path()).expect("new");
        rb.record_dropped(u32::MAX - 5);
        rb.record_dropped(100); // would overflow without saturating_add
        assert_eq!(rb.snapshot().dropped_frames, u32::MAX);
    }

    #[test]
    fn recover_reads_session_json_after_flush() {
        let tmp = TempDir::new().expect("tmpdir");
        let session = SessionId::nil();
        {
            let mut rb = Ringbuffer::new(session, tmp.path()).expect("new");
            rb.push(&frame(Channel::Mic, &[0.0; 240])).expect("push");
            rb.record_dropped(7);
            rb.flush().expect("flush");
        }
        let state = recover(session, tmp.path()).expect("recover");
        assert_eq!(state.mic_frames, 240);
        assert_eq!(state.dropped_frames, 7);
        assert_eq!(state.session_id, session);
    }

    #[test]
    fn recover_errors_clearly_when_session_missing() {
        let tmp = TempDir::new().expect("tmpdir");
        let result = recover(SessionId::nil(), tmp.path());
        match result {
            Err(RingbufferError::SessionMetaMissing { .. }) => {}
            other => panic!("expected SessionMetaMissing, got {other:?}"),
        }
    }

    #[test]
    fn reopen_existing_session_resumes_frame_counts() {
        let tmp = TempDir::new().expect("tmpdir");
        let session = SessionId::nil();
        {
            let mut rb = Ringbuffer::new(session, tmp.path()).expect("new");
            rb.push(&frame(Channel::Mic, &[0.5; 480])).expect("push");
            rb.flush().expect("flush");
        }
        // re-open: append mode keeps existing file, frame counts
        // recompute from on-disk file size
        let rb2 = Ringbuffer::new(session, tmp.path()).expect("re-open");
        assert_eq!(rb2.snapshot().mic_frames, 480);
    }

    #[test]
    fn scan_returns_empty_when_no_sessions_dir() {
        let tmp = TempDir::new().expect("tmpdir");
        let states = scan_recoverable_sessions(tmp.path());
        assert!(states.is_empty());
    }

    #[test]
    fn scan_finds_committed_session() {
        let tmp = TempDir::new().expect("tmpdir");
        let session = SessionId::from_u128(0x1234_5678);
        {
            let mut rb = Ringbuffer::new(session, tmp.path()).expect("new");
            rb.push(&frame(Channel::Mic, &[0.0; 100])).expect("push");
            rb.flush().expect("flush");
        }
        let states = scan_recoverable_sessions(tmp.path());
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].session_id, session);
        assert_eq!(states[0].mic_frames, 100);
    }

    #[test]
    fn scan_skips_non_uuid_directories() {
        let tmp = TempDir::new().expect("tmpdir");
        std::fs::create_dir_all(tmp.path().join("sessions").join("not-a-uuid")).expect("mkdir");
        // Add one real session alongside.
        let session = SessionId::from_u128(0xABCD);
        {
            let _ = Ringbuffer::new(session, tmp.path()).expect("new");
        }
        let states = scan_recoverable_sessions(tmp.path());
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].session_id, session);
    }

    #[test]
    fn scan_skips_corrupt_session_json() {
        let tmp = TempDir::new().expect("tmpdir");
        let bad_id = SessionId::from_u128(0xBAD);
        let bad_dir = tmp.path().join("sessions").join(bad_id.to_string());
        std::fs::create_dir_all(&bad_dir).expect("mkdir");
        std::fs::write(bad_dir.join(SESSION_FILENAME), b"{not json}").expect("write bad");

        let states = scan_recoverable_sessions(tmp.path());
        assert!(states.is_empty(), "corrupt session must not surface");
    }
}
