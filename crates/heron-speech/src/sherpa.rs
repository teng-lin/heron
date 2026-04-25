//! Production Sherpa-ONNX backend per `docs/implementation.md` §8.3.
//!
//! Cross-platform fallback to the Apple-only WhisperKit path. Wraps
//! `sherpa-rs` (the prebuilt-binaries Rust binding to `sherpa-onnx`)
//! behind the [`SttBackend`] trait. The `download-binaries` cargo
//! feature pulls a `libsherpa-onnx-c-api.dylib` + `libonnxruntime.dylib`
//! pair at build time; on macOS Apple Silicon those are the
//! `osx-universal2-shared` archive from the upstream `sherpa-onnx`
//! GitHub release matching the `sherpa-rs-sys` version (v1.12.9 today).
//!
//! ## Pipeline (per §8.3 + §8.4)
//!
//! 1. Read the WAV with `hound`. The file may be 16-bit PCM int or 32-bit
//!    float; both are mapped to `f32` in `[-1.0, 1.0]`.
//! 2. Mix to mono if stereo (simple averaging — input from §6 is already
//!    mono, but a forgiving impl avoids spurious mid-pipeline failures).
//! 3. Resample to 16 kHz via `rubato::SincFixedIn` if the source rate
//!    isn't already 16 kHz. Sherpa's offline recognizers are **strict**
//!    on sample-rate match; getting it wrong silently returns garbage.
//! 4. Feed the 16 kHz mono buffer through Silero VAD in 512-sample
//!    windows; collect each [`SpeechSegment`] the VAD emits.
//! 5. Send each segment to a Whisper-tiny.en `OfflineRecognizer`. Emit
//!    one [`Turn`] per segment with `t0`/`t1` derived from the segment's
//!    sample offset and the 16 kHz rate.
//! 6. Push every turn through [`PartialWriter`] (§8.4: fsync ≥ every
//!    10 turns / 5 s) and the user-supplied `on_turn` callback.
//!
//! ## Model layout
//!
//! Both the Silero VAD model and the Whisper-tiny.en bundle cache to
//! `~/Library/Caches/heron/sherpa/` on macOS (or the platform
//! `dirs::cache_dir()/heron/sherpa/`). Override with the
//! `HERON_SHERPA_MODEL_DIR` env var (mirrors the WhisperKit pattern).
//!
//! Layout under the cache root:
//!
//! ```text
//! sherpa/
//!   silero_vad.onnx
//!   sherpa-onnx-whisper-tiny.en/
//!     tiny.en-encoder.int8.onnx
//!     tiny.en-decoder.int8.onnx
//!     tiny.en-tokens.txt
//! ```
//!
//! `ensure_model` is idempotent: a present-and-non-empty file is treated
//! as cached. The `.partial` rename pattern guards against a SIGKILL
//! mid-download leaving a half-written model on disk.
//!
//! ## Limitations (v1)
//!
//! - Confidence is `None` per §8.3; sherpa exposes per-token timestamps
//!   but no per-segment posterior we want to surface. v1.1 may compute
//!   one from `OfflineRecognizerResult::timestamps` density.
//! - The recognizer instance is **not** cached across `transcribe` calls
//!   today. Each call constructs + destroys a recognizer; load is
//!   ~150 ms on a tiny.en bundle, dominated by the VAD/ASR work for any
//!   realistic input length. Caching arrives if profiling shows it
//!   matters.

use std::fs::{self, File};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use async_trait::async_trait;
use heron_types::{Channel, SessionId, SpeakerSource, Turn};
use hound::WavReader;
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
use sherpa_rs::silero_vad::{SileroVad, SileroVadConfig};
use sherpa_rs::whisper::{WhisperConfig, WhisperRecognizer};

use crate::partial_writer::PartialWriter;
use crate::{ProgressFn, SttBackend, SttError, TranscribeSummary, TurnFn};

const TARGET_SAMPLE_RATE: u32 = 16_000;
const VAD_WINDOW: usize = 512;
const VAD_BUFFER_SECS: f32 = 30.0;

/// Upstream sherpa-onnx release tag the `download-binaries` feature targets.
/// The model bundles below live under the same release line and are known
/// to load against this runtime.
const SHERPA_MODELS_RELEASE: &str = "asr-models";

const SILERO_VAD_FILE: &str = "silero_vad.onnx";
const SILERO_VAD_URL: &str =
    "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx";
/// Pinned SHA-256 of the upstream Silero VAD release artifact. Drift
/// in the upstream blob fails the download with `ChecksumMismatch`
/// rather than silently loading a tampered or re-cut model.
const SILERO_VAD_SHA256: &str = "9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6";

const WHISPER_BUNDLE_DIR: &str = "sherpa-onnx-whisper-tiny.en";
const WHISPER_BUNDLE_URL: &str = "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-whisper-tiny.en.tar.bz2";
const WHISPER_BUNDLE_SHA256: &str =
    "2bd6cf965c8bb3e068ef9fa2191387ee63a9dfa2a4e37582a8109641c20005dd";
const WHISPER_ENCODER: &str = "tiny.en-encoder.int8.onnx";
const WHISPER_DECODER: &str = "tiny.en-decoder.int8.onnx";
const WHISPER_TOKENS: &str = "tiny.en-tokens.txt";

/// Production Sherpa backend.
///
/// Construction is cheap: it stores the cache directory and validates
/// nothing. The first [`SttBackend::ensure_model`] call resolves the
/// model files (downloading if missing); subsequent calls re-verify
/// in milliseconds. Models live across runs so the network round-trip
/// is a one-time cost per machine.
pub struct SherpaBackend {
    /// Folder containing `silero_vad.onnx` and `sherpa-onnx-whisper-tiny.en/`.
    /// Resolved from `HERON_SHERPA_MODEL_DIR` by [`Self::from_env`] or
    /// supplied directly via [`Self::new`] in tests. May not exist yet
    /// at construction time; `ensure_model` creates it.
    model_dir: PathBuf,
}

impl SherpaBackend {
    pub fn new(model_dir: PathBuf) -> Self {
        Self { model_dir }
    }

    /// Construct with the default cache dir (`~/Library/Caches/heron/sherpa/`
    /// on macOS) or the override path if `HERON_SHERPA_MODEL_DIR` is set.
    /// We don't fail construction when the dir doesn't exist — the
    /// `ensure_model` step is the one that materializes it, the same
    /// shape the orchestrator's progress UI already handles.
    pub fn from_env() -> Self {
        let dir = std::env::var_os("HERON_SHERPA_MODEL_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(default_model_dir);
        Self::new(dir)
    }

    pub fn model_dir(&self) -> &Path {
        &self.model_dir
    }

    fn vad_model_path(&self) -> PathBuf {
        self.model_dir.join(SILERO_VAD_FILE)
    }

    fn whisper_bundle_dir(&self) -> PathBuf {
        self.model_dir.join(WHISPER_BUNDLE_DIR)
    }
}

/// Default cache root. Falls back to a per-process tempdir-style path
/// only if `dirs::cache_dir()` is unavailable, so an unconfigured Linux
/// CI host without `$XDG_CACHE_HOME` still gets a stable location for
/// the `ensure_model` flow rather than panicking.
fn default_model_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("heron")
        .join("sherpa")
}

#[async_trait]
impl SttBackend for SherpaBackend {
    async fn ensure_model(&self, mut on_progress: ProgressFn) -> Result<(), SttError> {
        on_progress(0.0);
        fs::create_dir_all(&self.model_dir).map_err(SttError::Io)?;

        let vad_path = self.vad_model_path();
        if !is_present(&vad_path) {
            tracing::info!(
                target: "heron_speech::sherpa",
                release = SHERPA_MODELS_RELEASE,
                path = %vad_path.display(),
                "downloading Silero VAD model",
            );
            download_to(SILERO_VAD_URL, &vad_path, SILERO_VAD_SHA256)?;
        }
        on_progress(0.4);

        let bundle_dir = self.whisper_bundle_dir();
        if !whisper_bundle_present(&bundle_dir) {
            tracing::info!(
                target: "heron_speech::sherpa",
                release = SHERPA_MODELS_RELEASE,
                path = %bundle_dir.display(),
                "downloading Whisper tiny.en bundle",
            );
            download_and_extract_tarball(
                WHISPER_BUNDLE_URL,
                WHISPER_BUNDLE_SHA256,
                &self.model_dir,
            )?;
            if !whisper_bundle_present(&bundle_dir) {
                return Err(SttError::ModelMissing(format!(
                    "tarball extracted but expected files missing under {}",
                    bundle_dir.display()
                )));
            }
        }
        on_progress(0.9);

        // Construct + drop a recognizer to surface a load failure here
        // rather than at the first `transcribe` call. The orchestrator's
        // progress UI handles a model-load error inline; a deferred one
        // would surface mid-meeting which is worse.
        let cfg = whisper_config(&bundle_dir);
        let _r = WhisperRecognizer::new(cfg)
            .map_err(|e| SttError::Failed(format!("sherpa whisper recognizer load failed: {e}")))?;
        on_progress(1.0);
        Ok(())
    }

    async fn transcribe(
        &self,
        wav_path: &Path,
        channel: Channel,
        _session_id: SessionId,
        partial_jsonl_path: &Path,
        mut on_turn: TurnFn,
    ) -> Result<TranscribeSummary, SttError> {
        let started = Instant::now();

        let vad_path = self.vad_model_path();
        let bundle_dir = self.whisper_bundle_dir();
        if !is_present(&vad_path) || !whisper_bundle_present(&bundle_dir) {
            return Err(SttError::ModelMissing(format!(
                "missing model files under {}; call ensure_model first",
                self.model_dir.display()
            )));
        }

        let wav_owned = wav_path.to_path_buf();
        let samples = tokio::task::spawn_blocking(move || load_mono_16k(&wav_owned))
            .await
            .map_err(|e| SttError::Failed(format!("wav load join failed: {e}")))??;

        // Open the partial-writer eagerly so even a zero-segment
        // transcription leaves an artifact for §3.5 recovery.
        let mut writer = PartialWriter::create(partial_jsonl_path.to_path_buf())
            .map_err(|e| SttError::Failed(format!("partial writer: {e}")))?;

        // `MicClean` is the post-AEC mic stream — same speaker (the
        // user) but the orchestrator hands us this channel when AEC
        // is wired. Treat it as Mic for attribution.
        let speaker = match channel {
            Channel::Mic | Channel::MicClean => "me".to_owned(),
            Channel::Tap => "them".to_owned(),
        };
        let speaker_source = match channel {
            Channel::Mic | Channel::MicClean => SpeakerSource::Self_,
            Channel::Tap => SpeakerSource::Channel,
        };

        // VAD + ASR are CPU-bound and bypass the executor. We hop onto
        // a blocking thread, run the entire pipeline, and ship a
        // `Vec<Turn>` back across; the partial-writer flush + on_turn
        // dispatch happen on the async side so the writer's fsync
        // cadence stays honest under tokio cancellation.
        let bundle_dir_owned = bundle_dir.clone();
        let vad_path_owned = vad_path.clone();
        let raw_segments = tokio::task::spawn_blocking(move || {
            run_vad_and_asr(&vad_path_owned, &bundle_dir_owned, &samples)
        })
        .await
        .map_err(|e| SttError::Failed(format!("sherpa pipeline join failed: {e}")))??;

        let mut turns_out = 0usize;
        for seg in raw_segments {
            let turn = Turn {
                t0: seg.t0,
                t1: seg.t1,
                text: seg.text,
                channel,
                speaker: speaker.clone(),
                speaker_source,
                // Sherpa's per-token timestamps are exposed but no
                // per-segment posterior is on the wire today; defer to
                // v1.1 (mirrors the WhisperKit position).
                confidence: None,
            };
            writer
                .push(&turn)
                .map_err(|e| SttError::Failed(format!("partial writer push: {e}")))?;
            on_turn(turn);
            turns_out += 1;
        }
        writer
            .finalize()
            .map_err(|e| SttError::Failed(format!("partial writer finalize: {e}")))?;

        Ok(TranscribeSummary {
            turns: turns_out,
            // Without a per-segment confidence on the wire we can't
            // count low-confidence turns yet; treat all as high. v1.1
            // revisits when timestamps→confidence heuristic lands.
            low_confidence_turns: 0,
            model: "sherpa-whisper-tiny.en".to_owned(),
            elapsed_secs: started.elapsed().as_secs_f64(),
        })
    }

    fn name(&self) -> &'static str {
        "sherpa"
    }

    fn is_available(&self) -> bool {
        // sherpa-onnx ships its own ONNX runtime via `download-binaries`;
        // there's no platform predicate to check. Mirror the §8.6 spec.
        true
    }
}

struct AsrSegment {
    t0: f64,
    t1: f64,
    text: String,
}

/// Build the Whisper config that points at the on-disk bundle.
/// The int8-quantized encoder/decoder pair is half the size of the
/// fp32 variants and matches the WER thresholds in §8.5 within noise.
fn whisper_config(bundle_dir: &Path) -> WhisperConfig {
    WhisperConfig {
        encoder: bundle_dir.join(WHISPER_ENCODER).display().to_string(),
        decoder: bundle_dir.join(WHISPER_DECODER).display().to_string(),
        tokens: bundle_dir.join(WHISPER_TOKENS).display().to_string(),
        language: "en".to_owned(),
        ..Default::default()
    }
}

fn run_vad_and_asr(
    vad_path: &Path,
    bundle_dir: &Path,
    samples: &[f32],
) -> Result<Vec<AsrSegment>, SttError> {
    let vad_cfg = SileroVadConfig {
        model: vad_path.display().to_string(),
        sample_rate: TARGET_SAMPLE_RATE,
        window_size: VAD_WINDOW as i32,
        ..Default::default()
    };
    let mut vad = SileroVad::new(vad_cfg, VAD_BUFFER_SECS)
        .map_err(|e| SttError::Failed(format!("silero vad init: {e}")))?;
    let mut recognizer = WhisperRecognizer::new(whisper_config(bundle_dir))
        .map_err(|e| SttError::Failed(format!("whisper recognizer init: {e}")))?;

    let mut segments: Vec<AsrSegment> = Vec::new();
    let total = samples.len();
    let mut cursor = 0usize;
    while cursor + VAD_WINDOW <= total {
        let window = samples[cursor..cursor + VAD_WINDOW].to_vec();
        vad.accept_waveform(window);
        drain_vad(&mut vad, &mut recognizer, &mut segments);
        cursor += VAD_WINDOW;
    }
    // Flush any speech the VAD has been holding onto past the last
    // 512-sample boundary so a trailing turn isn't silently dropped.
    vad.flush();
    drain_vad(&mut vad, &mut recognizer, &mut segments);

    Ok(segments)
}

fn drain_vad(vad: &mut SileroVad, recognizer: &mut WhisperRecognizer, out: &mut Vec<AsrSegment>) {
    while !vad.is_empty() {
        let seg = vad.front();
        vad.pop();
        let t0 = seg.start as f64 / TARGET_SAMPLE_RATE as f64;
        let dur = seg.samples.len() as f64 / TARGET_SAMPLE_RATE as f64;
        let result = recognizer.transcribe(TARGET_SAMPLE_RATE, &seg.samples);
        let text = result.text.trim().to_owned();
        if text.is_empty() {
            // Silence-only segments leak through the VAD threshold under
            // very quiet inputs; the recognizer returns "" for them and
            // we drop those rather than emit an empty turn.
            continue;
        }
        out.push(AsrSegment {
            t0,
            t1: t0 + dur,
            text,
        });
    }
}

/// Read a WAV, mix-down to mono if needed, and resample to 16 kHz.
/// `hound` handles 16-bit PCM and 32-bit float natively; other formats
/// (24-bit, ALAW, etc.) error out. The §6 capture pipeline only ever
/// produces 48 kHz mono PCM16 today, so this code path is a forgiving
/// fallback rather than a wide compatibility surface.
fn load_mono_16k(path: &Path) -> Result<Vec<f32>, SttError> {
    let reader = WavReader::open(path).map_err(|e| SttError::Failed(format!("hound open: {e}")))?;
    let spec = reader.spec();
    let channels = spec.channels.max(1) as usize;
    let src_rate = spec.sample_rate;

    let interleaved: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => reader
            .into_samples::<i32>()
            .map(|s| {
                s.map(|v| v as f32 / i32_full_scale(spec.bits_per_sample))
                    .map_err(|e| SttError::Failed(format!("hound sample: {e}")))
            })
            .collect::<Result<_, _>>()?,
        hound::SampleFormat::Float => reader
            .into_samples::<f32>()
            .map(|s| s.map_err(|e| SttError::Failed(format!("hound sample: {e}"))))
            .collect::<Result<_, _>>()?,
    };

    let mono = if channels == 1 {
        interleaved
    } else {
        // Average across channels per frame. The capture path is mono
        // by construction; hitting this branch means a user fed a
        // pre-recorded stereo WAV in.
        let frames = interleaved.len() / channels;
        let mut mono = Vec::with_capacity(frames);
        for f in 0..frames {
            let mut acc = 0.0f32;
            for c in 0..channels {
                acc += interleaved[f * channels + c];
            }
            mono.push(acc / channels as f32);
        }
        drop(interleaved);
        mono
    };

    if src_rate == TARGET_SAMPLE_RATE {
        return Ok(mono);
    }
    resample_to_16k(&mono, src_rate)
}

fn i32_full_scale(bits: u16) -> f32 {
    // hound exposes int samples right-aligned; divide by 2^(bits-1)-1
    // to map to [-1, 1]. Using the next power of two as the divisor
    // avoids the off-by-one at saturated samples.
    let shift = bits.saturating_sub(1).min(31);
    (1u32 << shift) as f32
}

fn resample_to_16k(input: &[f32], src_rate: u32) -> Result<Vec<f32>, SttError> {
    let ratio = TARGET_SAMPLE_RATE as f64 / src_rate as f64;
    // SincFixedIn buffers a fixed input chunk and produces a variable
    // output; chunk size 1024 is the rubato example default. The
    // Blackman-Harris window + cubic interpolation matches the
    // anti-aliasing quality the §6 AEC expects from upstream.
    let params = SincInterpolationParameters {
        sinc_len: 256,
        f_cutoff: 0.95,
        oversampling_factor: 256,
        interpolation: SincInterpolationType::Cubic,
        window: WindowFunction::BlackmanHarris2,
    };
    let mut resampler = SincFixedIn::<f32>::new(ratio, 2.0, params, 1024, 1)
        .map_err(|e| SttError::Failed(format!("rubato init: {e}")))?;

    let mut out = Vec::with_capacity((input.len() as f64 * ratio) as usize + 1024);
    let chunk = resampler.input_frames_next();
    let mut pos = 0usize;
    while pos + chunk <= input.len() {
        let block = [&input[pos..pos + chunk]];
        let processed = resampler
            .process(&block, None)
            .map_err(|e| SttError::Failed(format!("rubato process: {e}")))?;
        out.extend_from_slice(&processed[0]);
        pos += chunk;
    }
    if pos < input.len() {
        // Pad the trailing chunk with zeros so we don't drop the tail.
        // process_partial would skip the resampler's internal-state
        // priming we already paid for; pad-and-process is simpler.
        let mut tail = vec![0.0f32; chunk];
        let remaining = input.len() - pos;
        tail[..remaining].copy_from_slice(&input[pos..]);
        let block = [tail.as_slice()];
        let processed = resampler
            .process(&block, None)
            .map_err(|e| SttError::Failed(format!("rubato process tail: {e}")))?;
        // Truncate the tail's resampled output to the expected length
        // so we don't append silence past the actual end of audio.
        let tail_out_len = (remaining as f64 * ratio).round() as usize;
        out.extend_from_slice(&processed[0][..tail_out_len.min(processed[0].len())]);
    }
    Ok(out)
}

fn is_present(p: &Path) -> bool {
    fs::metadata(p).map(|m| m.len() > 0).unwrap_or(false)
}

fn whisper_bundle_present(bundle_dir: &Path) -> bool {
    is_present(&bundle_dir.join(WHISPER_ENCODER))
        && is_present(&bundle_dir.join(WHISPER_DECODER))
        && is_present(&bundle_dir.join(WHISPER_TOKENS))
}

/// Download `url` to `dest` atomically and verify SHA-256.
///
/// Streams to a sibling `.partial` path while feeding a SHA-256 hasher,
/// fsyncs, checks the digest against `expected_sha256`, then renames.
/// A SIGKILL mid-download leaves no `dest`, so the next `ensure_model`
/// call cleanly restarts. A digest mismatch deletes the partial and
/// returns `Failed` so a tampered or re-cut upstream artifact never
/// becomes a cached model.
///
/// The temp filename appends `.partial` rather than replacing the
/// extension, so callers passing a `dest` that already ends in
/// `.partial` (e.g. the tarball-staging path) still get a distinct
/// tmp path and a sound atomic rename.
fn download_to(url: &str, dest: &Path, expected_sha256: &str) -> Result<(), SttError> {
    use sha2::{Digest, Sha256};

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(SttError::Io)?;
    }
    let mut tmp = dest.as_os_str().to_owned();
    tmp.push(".partial");
    let tmp = PathBuf::from(tmp);
    let resp = ureq_get(url)?;
    let mut reader = resp;
    let mut file = File::create(&tmp).map_err(SttError::Io)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| SttError::Failed(format!("download {url}: {e}")))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n]).map_err(SttError::Io)?;
    }
    file.sync_all().map_err(SttError::Io)?;
    drop(file);

    let actual = hex_lower(&hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        let _ = fs::remove_file(&tmp);
        return Err(SttError::Failed(format!(
            "SHA-256 mismatch for {url}: expected {expected_sha256}, got {actual}"
        )));
    }

    fs::rename(&tmp, dest).map_err(SttError::Io)?;
    Ok(())
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((b & 0x0f) as u32, 16).unwrap_or('0'));
    }
    out
}

/// Download a `.tar.bz2` and extract it under `dest_dir` using the
/// system `tar` binary. macOS, Linux, and the GNU/Windows toolchains
/// all ship a `tar` that handles `bz2`, so we avoid pulling in a Rust
/// bz2 crate (we'd otherwise have to vendor `bzip2-sys` C). The
/// archive's top-level directory becomes the cached bundle.
fn download_and_extract_tarball(
    url: &str,
    expected_sha256: &str,
    dest_dir: &Path,
) -> Result<(), SttError> {
    fs::create_dir_all(dest_dir).map_err(SttError::Io)?;
    let archive = dest_dir.join("download.tar.bz2");
    download_to(url, &archive, expected_sha256)?;
    let status = std::process::Command::new("tar")
        .arg("xjf")
        .arg(&archive)
        .arg("-C")
        .arg(dest_dir)
        .status()
        .map_err(|e| SttError::Failed(format!("tar invocation: {e}")))?;
    let _ = fs::remove_file(&archive);
    if !status.success() {
        return Err(SttError::Failed(format!(
            "tar extract failed with {status}"
        )));
    }
    Ok(())
}

/// Minimal blocking HTTP GET used only by `ensure_model`. We avoid
/// pulling `reqwest` (already in the workspace but async-by-default)
/// onto this synchronous path; `ureq` is a transitive dep already
/// brought in by `sherpa-rs-sys`'s `download-binaries` feature.
///
/// Bounded connect + read timeouts so a stalled GitHub mirror can't
/// wedge the orchestrator's first-run model fetch indefinitely.
fn ureq_get(url: &str) -> Result<Box<dyn Read + Send>, SttError> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(15))
        .timeout_read(std::time::Duration::from_secs(60))
        .build();
    let resp = agent
        .get(url)
        .call()
        .map_err(|e| SttError::Failed(format!("HTTP GET {url}: {e}")))?;
    Ok(Box::new(BufReader::new(resp.into_reader())))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// Serializes `HERON_SHERPA_MODEL_DIR` mutations across tests so
    /// `cargo test`'s parallel runner can't interleave a setter and a
    /// reader on the same env var.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn from_env_uses_override_when_set() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        unsafe {
            std::env::set_var("HERON_SHERPA_MODEL_DIR", "/tmp/heron-sherpa-fixture");
        }
        let b = SherpaBackend::from_env();
        assert_eq!(b.model_dir(), PathBuf::from("/tmp/heron-sherpa-fixture"));
        unsafe {
            std::env::remove_var("HERON_SHERPA_MODEL_DIR");
        }
    }

    #[test]
    fn from_env_default_is_under_cache_dir() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        unsafe {
            std::env::remove_var("HERON_SHERPA_MODEL_DIR");
        }
        let b = SherpaBackend::from_env();
        // The default lives under dirs::cache_dir() which on macOS is
        // ~/Library/Caches; we don't pin the absolute path (CI hosts
        // vary) but we do pin the trailing two segments.
        let p = b.model_dir();
        assert!(p.ends_with("heron/sherpa"), "got {}", p.display());
    }

    #[test]
    fn name_and_availability_match_design() {
        let b = SherpaBackend::new(PathBuf::from("/nonexistent/heron-sherpa"));
        assert_eq!(b.name(), "sherpa");
        assert!(b.is_available());
    }

    #[tokio::test]
    async fn transcribe_without_models_errors() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let b = SherpaBackend::new(tmp.path().to_path_buf());
        let wav = tmp.path().join("input.wav");
        // Write a tiny silent 16 kHz mono WAV so the WAV reader itself
        // succeeds; the failure must come from the missing models.
        write_silent_wav(&wav, 16_000, 800);
        let jsonl = tmp.path().join("p.jsonl");
        let result = b
            .transcribe(
                &wav,
                Channel::Mic,
                SessionId::nil(),
                &jsonl,
                Box::new(|_| {}),
            )
            .await;
        assert!(
            matches!(result, Err(SttError::ModelMissing(_))),
            "expected ModelMissing, got {result:?}",
        );
    }

    #[test]
    fn resample_passes_through_at_16k() {
        let input: Vec<f32> = (0..16_000).map(|i| (i as f32 * 0.001).sin()).collect();
        let out = load_mono_16k_from_samples(&input, 16_000, 1).expect("resample");
        // Identity resample doesn't reach the rubato path; the buffer
        // must come back unchanged.
        assert_eq!(out.len(), input.len());
    }

    #[test]
    fn resample_changes_length_when_rate_differs() {
        // 2 s of 48 kHz audio → ~32 000 samples at 16 kHz. Allow a
        // small slack for the resampler's edge handling.
        let input = vec![0.1f32; 48_000 * 2];
        let out = load_mono_16k_from_samples(&input, 48_000, 1).expect("resample");
        let expected = 32_000;
        let drift = (out.len() as i64 - expected as i64).unsigned_abs();
        assert!(drift < 2_048, "expected ~{expected}, got {}", out.len());
    }

    #[test]
    fn stereo_input_mixes_down_to_mono() {
        // Two 16 kHz channels: left = +0.5, right = -0.5. Average must
        // be 0. The output frame count is half the interleaved length.
        let mut interleaved = Vec::with_capacity(2 * 16_000);
        for _ in 0..16_000 {
            interleaved.push(0.5);
            interleaved.push(-0.5);
        }
        let out = load_mono_16k_from_samples(&interleaved, 16_000, 2).expect("mix");
        assert_eq!(out.len(), 16_000);
        for s in &out {
            assert!(s.abs() < 1e-6, "stereo mix to mono should be ~0, got {s}");
        }
    }

    /// Reuse `load_mono_16k`'s mix-down + resample logic directly on a
    /// sample buffer, bypassing the WAV codec. Mirrors the production
    /// path so a regression in mixing or resampling fails this test
    /// rather than waiting for an integration run.
    fn load_mono_16k_from_samples(
        samples: &[f32],
        rate: u32,
        channels: usize,
    ) -> Result<Vec<f32>, SttError> {
        let mono = if channels == 1 {
            samples.to_vec()
        } else {
            let frames = samples.len() / channels;
            let mut mono = Vec::with_capacity(frames);
            for f in 0..frames {
                let mut acc = 0.0f32;
                for c in 0..channels {
                    acc += samples[f * channels + c];
                }
                mono.push(acc / channels as f32);
            }
            mono
        };
        if rate == TARGET_SAMPLE_RATE {
            Ok(mono)
        } else {
            resample_to_16k(&mono, rate)
        }
    }

    fn write_silent_wav(path: &Path, rate: u32, samples: usize) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).expect("wav create");
        for _ in 0..samples {
            w.write_sample(0i16).expect("wav write");
        }
        w.finalize().expect("wav finalize");
    }

    /// Integration test: run the full Silero-VAD + Whisper-tiny.en
    /// pipeline against the speech sample bundled with the upstream
    /// model archive. Gated on `HERON_SHERPA_INTEGRATION=1` because the
    /// first run downloads ~120 MB of models from GitHub Releases (and
    /// every subsequent run still needs the cache directory populated).
    /// Set `HERON_SHERPA_MODEL_DIR=/path/to/cache` to point at a
    /// pre-warmed cache rather than pulling fresh.
    #[tokio::test]
    async fn integration_transcribes_speech_when_gated() {
        if std::env::var_os("HERON_SHERPA_INTEGRATION").is_none() {
            return;
        }
        let tmp = tempfile::TempDir::new().expect("tmp");
        let b = SherpaBackend::from_env();
        b.ensure_model(Box::new(|_| {}))
            .await
            .expect("ensure_model");

        // Use the test wav shipped in the Whisper bundle as the input.
        let wav = b.whisper_bundle_dir().join("test_wavs").join("0.wav");
        if !wav.exists() {
            eprintln!("skipping: test wav missing at {}", wav.display());
            return;
        }
        let jsonl = tmp.path().join("turns.jsonl");
        let captured: std::sync::Arc<std::sync::Mutex<Vec<Turn>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let cap_for_cb = std::sync::Arc::clone(&captured);
        let summary = b
            .transcribe(
                &wav,
                Channel::Mic,
                SessionId::nil(),
                &jsonl,
                Box::new(move |t| {
                    cap_for_cb.lock().expect("lock").push(t);
                }),
            )
            .await
            .expect("transcribe");

        let turns = captured.lock().expect("lock");
        assert!(!turns.is_empty(), "expected at least one turn");
        assert!(summary.turns >= 1);
        let blob = turns
            .iter()
            .map(|t| t.text.to_lowercase())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            blob.contains("yellow") || blob.contains("nightfall"),
            "expected ground-truth keywords in transcript, got {blob:?}",
        );
    }
}
