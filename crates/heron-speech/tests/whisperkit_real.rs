//! End-to-end smoke test for the real WhisperKit backend.
//!
//! Skipped unless `HERON_WHISPERKIT_MODEL_DIR` is set to a directory
//! with a WhisperKit-compiled model bundle (see
//! `docs/manual-test-matrix.md` "WhisperKit STT backend"). When the
//! env var is unset the test prints a notice and returns Ok — this is
//! the contract `cargo test -p heron-speech` relies on so the unit
//! suite stays clean on machines that haven't downloaded a model.
//!
//! When the env var is set:
//!   1. We synthesize a 1-second 440 Hz sine WAV in a tempdir.
//!   2. Build a `WhisperKitBackend`, call `ensure_model`, then
//!      `transcribe`.
//!   3. Assert we get either a non-empty turn list (model found speech)
//!      or a clean `Ok` with zero turns (model decided the sine was
//!      silence) — both are acceptable for a synthetic input.

#![cfg(target_vendor = "apple")]
#![allow(clippy::expect_used)]

use std::f32::consts::TAU;
use std::path::Path;
use std::sync::{Arc, Mutex};

use heron_speech::{SttBackend, WhisperKitBackend};
use heron_types::{Channel, SessionId};
use tempfile::TempDir;

const SAMPLE_RATE: u32 = 16_000;
const DURATION_SECS: u32 = 1;

fn write_sine_wav(path: &Path) {
    // 16 kHz mono 16-bit PCM is what Whisper expects natively. We
    // hand-build the WAV header instead of pulling `hound` as a
    // dev-dep just for one test.
    let n_samples = (SAMPLE_RATE * DURATION_SECS) as usize;
    let bytes_per_sample = 2u32;
    let data_size = (n_samples as u32) * bytes_per_sample;
    let chunk_size = 36 + data_size;

    let mut bytes = Vec::with_capacity(44 + data_size as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&chunk_size.to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(b"fmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
    bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM format
    bytes.extend_from_slice(&1u16.to_le_bytes()); // mono
    bytes.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    let byte_rate = SAMPLE_RATE * bytes_per_sample;
    bytes.extend_from_slice(&byte_rate.to_le_bytes());
    bytes.extend_from_slice(&(bytes_per_sample as u16).to_le_bytes()); // block align
    bytes.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_size.to_le_bytes());

    let amplitude: f32 = 0.2 * i16::MAX as f32;
    for i in 0..n_samples {
        let t = i as f32 / SAMPLE_RATE as f32;
        let sample = (amplitude * (TAU * 440.0 * t).sin()) as i16;
        bytes.extend_from_slice(&sample.to_le_bytes());
    }

    std::fs::write(path, bytes).expect("write wav fixture");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whisperkit_real_smoke() {
    let model_dir = match std::env::var_os("HERON_WHISPERKIT_MODEL_DIR") {
        Some(d) => std::path::PathBuf::from(d),
        None => {
            eprintln!(
                "skipping whisperkit_real_smoke: set HERON_WHISPERKIT_MODEL_DIR to a \
                 WhisperKit model folder to run this test (see \
                 docs/manual-test-matrix.md > 'WhisperKit STT backend')."
            );
            return;
        }
    };

    let tmp = TempDir::new().expect("tempdir");
    let wav = tmp.path().join("sine.wav");
    write_sine_wav(&wav);
    let partial = tmp.path().join("smoke.partial.jsonl");

    let backend = WhisperKitBackend::new(model_dir);

    let progress = Arc::new(Mutex::new(Vec::<f32>::new()));
    let progress_capture = Arc::clone(&progress);
    backend
        .ensure_model(Box::new(move |p| {
            progress_capture
                .lock()
                .expect("progress lock")
                .push(p);
        }))
        .await
        .expect("ensure_model");
    let recorded = progress.lock().expect("progress lock").clone();
    assert!(
        recorded.first().copied() == Some(0.0),
        "ensure_model must fire 0.0 first; got {recorded:?}"
    );
    assert!(
        recorded.last().copied() == Some(1.0),
        "ensure_model must fire 1.0 last; got {recorded:?}"
    );

    let turns = Arc::new(Mutex::new(Vec::new()));
    let turns_capture = Arc::clone(&turns);
    let summary = backend
        .transcribe(
            &wav,
            Channel::Mic,
            SessionId::nil(),
            &partial,
            Box::new(move |t| {
                turns_capture.lock().expect("turns lock").push(t);
            }),
        )
        .await
        .expect("transcribe");

    let captured = turns.lock().expect("turns lock");
    assert_eq!(
        summary.turns,
        captured.len(),
        "summary.turns must match callback count"
    );
    // Either the model heard "speech" in the sine (returns >=1 turn)
    // or it returned zero — both are acceptable for synthetic input.
    // The hard assertion is that the on-disk partial exists.
    assert!(partial.exists(), "partial JSONL must be written");
}
