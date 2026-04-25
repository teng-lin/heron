//! Per-channel WAV finalization for [`crate::AudioCaptureHandle::stop`].
//!
//! The capture pipeline emits three logical streams onto the broadcast
//! channel: raw `Channel::Mic`, raw `Channel::Tap`, and the AEC-cleaned
//! `Channel::MicClean`. At session stop we materialize each as a
//! 48 kHz mono `f32` WAV (`PCM_FLOAT`, 32-bit) under
//! `<cache_dir>/sessions/<session_id>/{mic,tap,mic_clean}.wav` so the
//! manual §6.3 AEC test rig can run on the artifacts and so the
//! week-9 archival encode pass has a stable input format.
//!
//! ## Empty-WAV contract
//!
//! Every channel always produces a path on the returned [`StopArtifacts`]
//! ([`crate::StopArtifacts`]). If a channel never emitted a frame
//! (e.g. mic capture failed → tap-only session), this module writes an
//! **empty but valid WAV header** at the standard path so downstream
//! consumers never see a missing-file error. They get a 0-sample WAV,
//! which `hound` reads back as `samples().count() == 0`.
//!
//! ## Realtime safety
//!
//! All disk I/O happens on the broadcast-consumer side of the pipeline
//! (a regular Tokio task), not on the Core Audio realtime thread. The
//! realtime path stays alloc-only / wait-free; this module is free to
//! buffer + flush as it likes.

use std::collections::HashMap;
use std::fs::{File, create_dir_all};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use heron_types::{Channel, SessionId};
use hound::{SampleFormat, WavSpec, WavWriter};

use crate::{AudioError, CaptureFrame};

/// 48 kHz mono `f32` (PCM_FLOAT). Matches the WebRTC APM config
/// (`crate::aec::APM_SAMPLE_RATE_HZ`) so APM frames pass through this
/// module untouched.
fn wav_spec() -> WavSpec {
    WavSpec {
        channels: 1,
        sample_rate: 48_000,
        bits_per_sample: 32,
        sample_format: SampleFormat::Float,
    }
}

/// Standard filename for each channel inside the session directory.
fn filename_for(channel: Channel) -> &'static str {
    match channel {
        Channel::Mic => "mic.wav",
        Channel::Tap => "tap.wav",
        Channel::MicClean => "mic_clean.wav",
    }
}

/// Lazy per-channel WAV writers. One writer per channel, opened on the
/// first frame for that channel and closed at [`finalize`].
///
/// [`finalize`]: PerChannelWavWriters::finalize
pub struct PerChannelWavWriters {
    session_dir: PathBuf,
    /// `None` until the first frame for that channel arrives. The
    /// `BufWriter` wrapping is what `hound::WavWriter::new` does
    /// internally for `&mut File` inputs anyway, but spelling it out
    /// keeps the disk-flush boundary obvious.
    mic: Option<WavWriter<BufWriter<File>>>,
    tap: Option<WavWriter<BufWriter<File>>>,
    mic_clean: Option<WavWriter<BufWriter<File>>>,
}

impl PerChannelWavWriters {
    /// Create the session directory under `cache_dir/sessions/<session_id>/`
    /// and prepare empty per-channel slots. No WAV files exist on disk
    /// yet — they're created lazily on the first frame for each channel.
    pub fn new(cache_dir: &Path, session_id: SessionId) -> Result<Self, AudioError> {
        let session_dir = cache_dir.join("sessions").join(session_id.to_string());
        create_dir_all(&session_dir)?;
        Ok(Self {
            session_dir,
            mic: None,
            tap: None,
            mic_clean: None,
        })
    }

    /// Path that the channel's WAV will (or does) live at. Useful for
    /// the empty-WAV contract — callers can pre-compute the path even
    /// before any frame has arrived for that channel.
    pub fn path_for(&self, channel: Channel) -> PathBuf {
        self.session_dir.join(filename_for(channel))
    }

    /// Append `frame.samples` to the channel's WAV. Opens the file
    /// lazily on the first call for that channel.
    ///
    /// # Errors
    /// Returns [`AudioError::Io`] if disk I/O fails (full disk,
    /// permission revoked mid-session) or [`AudioError::Aborted`] if
    /// `hound`'s WAV writer rejects the sample (e.g. NaN — guarded
    /// upstream by APM, but surfaced here as a recoverable error
    /// rather than a panic).
    pub fn write_frame(&mut self, frame: &CaptureFrame) -> Result<(), AudioError> {
        // Two-step borrow: open_writer needs `&self.session_dir` but
        // `writer_slot_mut` borrows `&mut self`, so they can't be live
        // at the same time. Open first if needed, then re-borrow the
        // slot for the actual write.
        if self.writer_slot_mut(frame.channel).is_none() {
            let new_writer = open_writer(&self.session_dir, frame.channel)?;
            *self.writer_slot_mut(frame.channel) = Some(new_writer);
        }
        let writer = match self.writer_slot_mut(frame.channel).as_mut() {
            Some(w) => w,
            None => {
                return Err(AudioError::Aborted(
                    "wav writer slot empty after lazy init".to_string(),
                ));
            }
        };
        for sample in &frame.samples {
            writer
                .write_sample(*sample)
                .map_err(|e| AudioError::Aborted(format!("hound write_sample failed: {e}")))?;
        }
        Ok(())
    }

    /// Flush + finalize all opened writers, and create empty WAV
    /// headers for any channel that never received a frame. Returns
    /// the set of paths, one entry per channel.
    ///
    /// Consumes `self`. After this returns, the WAV files are closed
    /// and ready for `hound::WavReader::open` against the returned
    /// paths.
    pub fn finalize(mut self) -> Result<HashMap<Channel, PathBuf>, AudioError> {
        let mut out: HashMap<Channel, PathBuf> = HashMap::new();
        for channel in [Channel::Mic, Channel::Tap, Channel::MicClean] {
            let path = self.session_dir.join(filename_for(channel));
            let writer_slot = self.writer_slot_mut(channel);
            if let Some(writer) = writer_slot.take() {
                writer
                    .finalize()
                    .map_err(|e| AudioError::Aborted(format!("hound finalize failed: {e}")))?;
            } else {
                // Channel never emitted a frame: write an empty but
                // valid WAV header so the StopArtifacts contract
                // (every path is populated) holds. `hound::WavWriter::create`
                // writes the RIFF header eagerly; finalizing without
                // any `write_sample` calls leaves a valid 0-sample
                // WAV behind.
                let writer = WavWriter::create(&path, wav_spec()).map_err(|e| {
                    AudioError::Aborted(format!(
                        "hound create empty WAV for {:?} at {}: {e}",
                        channel,
                        path.display()
                    ))
                })?;
                writer
                    .finalize()
                    .map_err(|e| AudioError::Aborted(format!("hound finalize empty WAV: {e}")))?;
            }
            out.insert(channel, path);
        }
        Ok(out)
    }

    fn writer_slot_mut(&mut self, channel: Channel) -> &mut Option<WavWriter<BufWriter<File>>> {
        match channel {
            Channel::Mic => &mut self.mic,
            Channel::Tap => &mut self.tap,
            Channel::MicClean => &mut self.mic_clean,
        }
    }
}

fn open_writer(
    session_dir: &Path,
    channel: Channel,
) -> Result<WavWriter<BufWriter<File>>, AudioError> {
    let path = session_dir.join(filename_for(channel));
    WavWriter::create(&path, wav_spec()).map_err(|e| {
        AudioError::Aborted(format!(
            "hound WavWriter::create({}) failed: {e}",
            path.display()
        ))
    })
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use hound::WavReader;

    fn frame(channel: Channel, samples: Vec<f32>) -> CaptureFrame {
        CaptureFrame {
            channel,
            host_time: 0,
            session_secs: 0.0,
            samples,
        }
    }

    /// A single frame round-trips through `hound`: same sample count,
    /// same first/last sample, same sample rate. Locks down the spec
    /// (48 kHz mono f32) so a future patch can't silently change the
    /// on-disk format without breaking a test.
    #[test]
    fn known_frame_round_trips_through_hound() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = SessionId::nil();
        let mut writers = PerChannelWavWriters::new(dir.path(), session).expect("create writers");

        // 1 kHz sine, 480 samples = 10 ms at 48 kHz. Same shape as a
        // real APM frame.
        let omega = 2.0 * std::f32::consts::PI * 1000.0 / 48_000.0;
        let samples: Vec<f32> = (0..480).map(|i| (omega * i as f32).sin() * 0.5).collect();
        let first = samples[0];
        let last = samples[479];

        writers
            .write_frame(&frame(Channel::Mic, samples.clone()))
            .expect("write");

        let paths = writers.finalize().expect("finalize");
        let mic_path = paths.get(&Channel::Mic).expect("mic path present");

        let mut reader = WavReader::open(mic_path).expect("open mic.wav");
        let spec = reader.spec();
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, 48_000);
        assert_eq!(spec.bits_per_sample, 32);
        assert_eq!(spec.sample_format, SampleFormat::Float);

        let read: Vec<f32> = reader
            .samples::<f32>()
            .collect::<Result<_, _>>()
            .expect("samples decode");
        assert_eq!(read.len(), 480, "sample count survives round-trip");
        assert!(
            (read[0] - first).abs() < 1e-6,
            "first sample preserved: wrote {first}, read {}",
            read[0]
        );
        assert!(
            (read[479] - last).abs() < 1e-6,
            "last sample preserved: wrote {last}, read {}",
            read[479]
        );
    }

    /// Channels that never emitted still produce a valid 0-sample WAV
    /// at the standard path. Exercises the empty-WAV contract called
    /// out in the module docs and on `StopArtifacts`.
    #[test]
    fn finalize_writes_empty_wav_for_silent_channels() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = SessionId::nil();
        let writers = PerChannelWavWriters::new(dir.path(), session).expect("create writers");

        let paths = writers.finalize().expect("finalize on empty session");

        for channel in [Channel::Mic, Channel::Tap, Channel::MicClean] {
            let path = paths
                .get(&channel)
                .unwrap_or_else(|| panic!("path for {channel:?} present"));
            assert!(
                path.exists(),
                "empty WAV must exist on disk for {channel:?} at {}",
                path.display()
            );
            let reader = WavReader::open(path)
                .unwrap_or_else(|e| panic!("open empty WAV for {channel:?}: {e}"));
            assert_eq!(
                reader.duration(),
                0,
                "empty WAV should have 0 sample frames for {channel:?}"
            );
            let spec = reader.spec();
            assert_eq!(spec.channels, 1);
            assert_eq!(spec.sample_rate, 48_000);
            assert_eq!(spec.sample_format, SampleFormat::Float);
        }
    }

    /// Mixed write: one channel with frames, one channel silent. The
    /// silent channel still gets an empty WAV; the live channel keeps
    /// its samples. This is the exact shape of a tap-only session
    /// (mic capture failed, tap proceeds, MicClean falls back to
    /// passthrough = empty if upstream never emits).
    #[test]
    fn mixed_session_finalizes_both_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = SessionId::nil();
        let mut writers = PerChannelWavWriters::new(dir.path(), session).expect("create writers");

        let samples = vec![0.25_f32; 480];
        writers
            .write_frame(&frame(Channel::Tap, samples.clone()))
            .expect("write tap");

        let paths = writers.finalize().expect("finalize");
        let tap_path = paths.get(&Channel::Tap).expect("tap path");
        let mic_path = paths.get(&Channel::Mic).expect("mic path");

        let tap_reader = WavReader::open(tap_path).expect("open tap.wav");
        assert_eq!(tap_reader.duration(), 480, "tap got the samples");

        let mic_reader = WavReader::open(mic_path).expect("open empty mic.wav");
        assert_eq!(mic_reader.duration(), 0, "mic stayed empty");
    }
}
