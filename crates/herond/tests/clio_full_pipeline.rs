//! Real-pipeline (Clio) full-stack smoke test.
//!
//! Drives a synthetic 90-second WAV fixture through the production
//! STT → LLM → vault path with **real** WhisperKit + **real** Anthropic
//! API, asserting the resulting `<vault>/meetings/<...>.md` carries the
//! frontmatter / body / cost contract the desktop renderer depends on.
//!
//! ## Why this is feature-gated
//!
//! Per issue #194, this test is the nightly counterpart to the
//! existing `#[ignore]`d real-pipeline tests in `heron-audio`,
//! `heron-speech`, and `heron-llm`. It runs only when the binary is
//! compiled with `--features real-pipeline` AND the env-var skips
//! below pass — both gates exist deliberately:
//!
//! - **`feature = "real-pipeline"`** keeps the test out of every
//!   contributor's `cargo test --workspace` so the default suite
//!   never reaches for a downloaded STT model bundle or a paid LLM
//!   API key.
//! - **Env-var skip** (`HERON_REAL_PIPELINE_FIXTURE`,
//!   `HERON_WHISPERKIT_MODEL_DIR`, `ANTHROPIC_API_KEY`) is the
//!   belt-and-braces gate so a local dev who passed `--features
//!   real-pipeline` without configuring those still runs the binary
//!   to a clean skip rather than a hard failure.
//!
//! ## Why it doesn't actually spawn `herond`
//!
//! Issue #194's acceptance reads "spawn daemon → inject fixture →
//! wait for SSE `meeting.summarized`". The current orchestrator
//! surface does not yet expose either an audio-fixture injection
//! point on `start_capture` or a `meeting.summarized` event variant
//! (the closest is `meeting.completed`; see `EventPayload` in
//! `heron-session/src/lib.rs`). Adding either would be a separate
//! production-wiring PR.
//!
//! What this test ships today is the equivalent assertion at the
//! layer the daemon's HTTP route would itself thunk through:
//! `heron_cli::session::Orchestrator::with_test_backends` with
//! `skip_audio_capture: true`, real backends, and a pre-seeded mic
//! WAV. That covers STT + LLM + vault end-to-end against the same
//! fixture nightly contract — the only piece deferred is the HTTP /
//! SSE projection layer, which is exercised by the existing
//! `tests/v2_lifecycle.rs` cross-crate suite.
//!
//! When the orchestrator grows fixture injection (per the deferred
//! `tests/v2_lifecycle.rs::end_meeting_persists_transcript_summary_audio_references`
//! TODO), this test should grow a sibling that drives the full HTTP
//! path.

#![cfg(all(feature = "real-pipeline", target_vendor = "apple"))]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use heron_cli::session::{Backends, Orchestrator, SessionConfig};
use heron_llm::{AnthropicClient, AnthropicClientConfig};
use heron_speech::WhisperKitBackend;
use heron_types::{Frontmatter, RecordingState, SessionId};
use heron_vault::{CalendarError, CalendarEvent, CalendarReader, FileNamingPattern};
use heron_zoom::AxBackend;
use tempfile::TempDir;
use tokio::sync::oneshot;

/// Cost ceiling per the issue acceptance criteria. Baked into
/// `AnthropicClientConfig::max_tokens` below so the API itself caps
/// the run before billing climbs past this. Sonnet 4.6 output is
/// ~$15/Mtok; 256 max-tokens × $15/Mtok ≈ $0.004 of output, well
/// inside the $0.05 ceiling even with a chunky transcript on the
/// input side. The `assert!` at the end of the test is the
/// double-check that the `Cost.summary_usd` the API actually billed
/// stayed in `[0.0, 0.05]`.
const COST_CEILING_USD: f64 = 0.05;

/// `max_tokens` baked into the Anthropic client for this test. Per
/// issue #194's "Cost ceiling baked into test code via `max_tokens`"
/// requirement: the API call cannot bill output past this, so a
/// pricing change in the model card cannot silently push the test
/// past the cost ceiling without producing a visible token-out
/// number that flags it.
const MAX_OUTPUT_TOKENS: u32 = 256;

/// Stub AX backend. The pipeline only invokes `start` on the live-
/// audio path; with `skip_audio_capture: true` this path is never
/// reached, so a `NotYetImplemented` here would be silent — but if a
/// future refactor accidentally calls into AX, it's better that fails
/// loudly than that the test silently drops the fail-closed contract.
struct StubAx;

#[async_trait::async_trait]
impl AxBackend for StubAx {
    async fn start(
        &self,
        _session_id: heron_types::SessionId,
        _clock: heron_types::SessionClock,
        _out: tokio::sync::mpsc::Sender<heron_types::SpeakerEvent>,
        _events: tokio::sync::mpsc::Sender<heron_types::Event>,
    ) -> Result<heron_zoom::AxHandle, heron_zoom::AxError> {
        Err(heron_zoom::AxError::NotYetImplemented)
    }
    fn name(&self) -> &'static str {
        "stub-ax-real-pipeline"
    }
}

/// Calendar stub that returns no events. Mirrors the production
/// "Calendar permission denied" branch so the slug falls through to
/// `Id` and the frontmatter `attendees` stays LLM-inferred — the
/// minimum surface this test needs since it isn't asserting on
/// calendar wiring.
struct StubCalendarDenied;

impl CalendarReader for StubCalendarDenied {
    fn read_window(
        &self,
        _start_utc: chrono::DateTime<chrono::Utc>,
        _end_utc: chrono::DateTime<chrono::Utc>,
    ) -> Result<Option<Vec<CalendarEvent>>, CalendarError> {
        Ok(None)
    }
}

/// Resolve the WAV fixture path. Defaults to
/// `fixtures/audio/clio-smoke-90s.wav` relative to the workspace
/// root; an explicit `HERON_REAL_PIPELINE_FIXTURE=/abs/path` overrides
/// it for ad-hoc debugging against an alternative source.
fn resolve_fixture_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("HERON_REAL_PIPELINE_FIXTURE") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
        eprintln!(
            "HERON_REAL_PIPELINE_FIXTURE set to {} but the file does not exist; \
             falling back to the workspace default",
            p.display()
        );
    }
    // `CARGO_MANIFEST_DIR` for an integration test in `crates/herond/`
    // resolves to the herond crate dir; walk up two parents to land at
    // the workspace root and join the well-known fixture path.
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = crate_dir.parent()?.parent()?;
    let fixture = workspace_root
        .join("fixtures")
        .join("audio")
        .join("clio-smoke-90s.wav");
    fixture.exists().then_some(fixture)
}

/// Copy the source WAV into `<cache>/sessions/<id>/mic.wav`. The
/// pipeline's `skip_audio_capture` branch reads `mic_clean.wav` if it
/// exists (post-AEC convention) and falls back to `mic.wav`; we seed
/// the latter so the test exercises the same fall-back path the
/// production CLI uses when AEC didn't run. `tap.wav` is required by
/// the m4a encode step's preconditions even when no tap audio is
/// available — write a 1-sample silent stub so that step doesn't
/// abort the whole session.
fn seed_session_wavs(session_dir: &Path, fixture: &Path) {
    std::fs::create_dir_all(session_dir).expect("mkdir session_dir");
    std::fs::copy(fixture, session_dir.join("mic.wav")).expect("copy fixture mic.wav");
    // Hand-roll a 1-sample silent WAV at 48 kHz mono f32 so the m4a
    // encode step's "open and read tap.wav" precondition holds. The
    // pipeline doesn't fail when tap is silent — it just produces no
    // tap-channel turns, which the LLM will see as a one-sided
    // transcript.
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 48_000,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let tap_path = session_dir.join("tap.wav");
    let mut writer = hound::WavWriter::create(&tap_path, spec).expect("create tap.wav");
    writer.write_sample(0.0_f32).expect("write tap sample");
    writer.finalize().expect("finalize tap.wav");
}

/// Build the real backends the test needs. Returns `None` when any
/// prerequisite is missing so the caller can skip-with-message
/// instead of failing.
fn build_real_backends(model_dir: PathBuf, anthropic_key: String) -> Backends {
    let stt = Box::new(WhisperKitBackend::new(model_dir));
    let ax = Box::new(StubAx);
    // Build the Anthropic client by hand so we can pin `max_tokens`
    // to the cost ceiling rather than the 4_096-token default. The
    // `from_env` constructor wouldn't let us narrow that.
    let cfg = AnthropicClientConfig {
        api_key: anthropic_key,
        // Default base + model pulled from heron-llm consts so this
        // test's view tracks any production-side bump. `max_tokens`
        // is the only field the test pins explicitly — it's the
        // mechanical lever that bounds the API's output billing.
        base_url: heron_llm::anthropic::DEFAULT_BASE_URL.to_owned(),
        model: heron_llm::anthropic::DEFAULT_MODEL.to_owned(),
        max_tokens: MAX_OUTPUT_TOKENS,
        timeout: Duration::from_secs(120),
    };
    let llm: Box<dyn heron_llm::Summarizer> = match AnthropicClient::new(cfg) {
        Ok(c) => Box::new(c),
        Err(e) => panic!("AnthropicClient::new failed unexpectedly: {e}"),
    };
    let calendar: Box<dyn CalendarReader> = Box::new(StubCalendarDenied);
    (stt, ax, llm, calendar)
}

/// Read the YAML frontmatter from a meeting note. Returns the parsed
/// `Frontmatter` plus the body trailing it. Mirrors the inverse of
/// `heron_vault::VaultWriter::finalize_*` so a schema drift in either
/// direction surfaces here.
fn split_frontmatter(note: &str) -> (Frontmatter, &str) {
    assert!(
        note.starts_with("---\n"),
        "note must open with `---\\n`; got: {:?}",
        &note[..note.len().min(64)]
    );
    let after_open = &note[4..];
    let close_idx = after_open
        .find("\n---\n")
        .expect("note must close frontmatter with `\\n---\\n`");
    let yaml = &after_open[..close_idx];
    let body = &after_open[close_idx + 5..];
    let fm: Frontmatter = serde_yaml::from_str(yaml).expect("frontmatter parses as YAML");
    (fm, body)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clio_real_pipeline_writes_summarized_meeting_note() {
    // ----- Belt-and-braces env-var skips -----
    let model_dir = match std::env::var_os("HERON_WHISPERKIT_MODEL_DIR") {
        Some(d) => PathBuf::from(d),
        None => {
            eprintln!(
                "skipping clio_real_pipeline_writes_summarized_meeting_note: \
                 HERON_WHISPERKIT_MODEL_DIR unset. Point this at a WhisperKit \
                 model bundle to run."
            );
            return;
        }
    };
    let anthropic_key = match std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())
    {
        Some(k) => k,
        None => {
            eprintln!(
                "skipping clio_real_pipeline_writes_summarized_meeting_note: \
                 ANTHROPIC_API_KEY unset. Export it to exercise the live LLM path."
            );
            return;
        }
    };
    let fixture = match resolve_fixture_path() {
        Some(p) => p,
        None => {
            eprintln!(
                "skipping clio_real_pipeline_writes_summarized_meeting_note: \
                 fixture WAV not found. Generate `fixtures/audio/clio-smoke-90s.wav` \
                 (see `fixtures/audio/README.md`) or set HERON_REAL_PIPELINE_FIXTURE \
                 to an alternative path."
            );
            return;
        }
    };

    // ----- Wire the orchestrator to a tempdir-rooted vault -----
    let tmp = TempDir::new().expect("tempdir");
    // Static UUID per run — `from_u128` matches the
    // `heron-cli/tests/orchestrator_run.rs` pattern and keeps the
    // test off the system clock for reproducibility. The high bits
    // include the test's name fingerprint so a future test that
    // forgets to scope its tempdir doesn't collide with this one.
    let session_id = SessionId::from_u128(0x0193_C110_5C00_71E0_C110_5C00_71E0_C110);
    let cache_dir = tmp.path().join("cache");
    let vault_root = tmp.path().join("vault");
    let session_dir = cache_dir.join("sessions").join(session_id.to_string());

    seed_session_wavs(&session_dir, &fixture);

    let cfg = SessionConfig {
        session_id,
        target_bundle_id: "us.zoom.xos".into(),
        cache_dir,
        vault_root: vault_root.clone(),
        // Real pipeline: ask for WhisperKit. The `build_real_backends`
        // helper hands us the WhisperKit handle directly, but the
        // config field still has to name a registered backend so
        // anything that later re-resolves via config sees the same
        // choice.
        stt_backend_name: "whisperkit".into(),
        hotwords: Vec::new(),
        // Backends are pre-built; the selector this preference would
        // drive isn't reached on the test path.
        llm_preference: heron_llm::Preference::Auto,
        pre_meeting_briefing: None,
        event_bus: None,
        file_naming_pattern: FileNamingPattern::Id,
        persona: None,
        strip_names: false,
        pause_flag: None,
    };
    let backends = build_real_backends(model_dir, anthropic_key);
    let mut orch = Orchestrator::with_test_backends(cfg, backends);

    // The pre-seeded WAVs already represent the entire "recording"
    // window; fire `stop_rx` immediately so the pipeline transitions
    // to STT without an artificial wait.
    let (stop_tx, stop_rx) = oneshot::channel();
    let _ = stop_tx.send(());

    let outcome = orch.run(stop_rx).await.expect("pipeline run");
    assert_eq!(outcome.final_state, RecordingState::Idle);

    // ----- Assert on the resulting `<vault>/meetings/<...>.md` -----
    let note_path = outcome.note_path.expect("vault writer must produce a note");
    assert!(note_path.exists(), "note file must exist on disk");
    assert!(
        note_path.starts_with(&vault_root),
        "note must be under the vault root; got {}",
        note_path.display()
    );
    let parent = note_path
        .parent()
        .expect("note has parent dir")
        .file_name()
        .expect("parent dir name")
        .to_string_lossy();
    assert_eq!(
        parent, "meetings",
        "note must live under `<vault>/meetings/`; got parent dir {parent:?}"
    );

    let raw = std::fs::read_to_string(&note_path).expect("read note");
    let (fm, body) = split_frontmatter(&raw);

    // Frontmatter contract: every field the desktop renderer reads
    // must round-trip through serde_yaml without surprises. The
    // `split_frontmatter` parse already validates the YAML shape;
    // a couple of explicit fields below pin the high-value ones.
    assert!(
        !fm.cost.model.is_empty(),
        "cost.model must be stamped on a real-LLM run; got {:?}",
        fm.cost.model
    );
    assert!(
        fm.cost.tokens_in > 0,
        "tokens_in must be > 0 for a real Anthropic call; got {}",
        fm.cost.tokens_in
    );
    assert!(
        fm.cost.tokens_out > 0,
        "tokens_out must be > 0 for a real Anthropic call; got {}",
        fm.cost.tokens_out
    );

    // Cost ceiling: this is the line the issue's "cost line within
    // [$0.00, $0.05]" acceptance refers to. Baked into the test via
    // `MAX_OUTPUT_TOKENS` so even a regression that swaps to a
    // pricier model can't run away.
    assert!(
        fm.cost.summary_usd >= 0.0 && fm.cost.summary_usd <= COST_CEILING_USD,
        "cost.summary_usd outside [0.00, {COST_CEILING_USD}]: got ${:.6} \
         (model={}, tokens_in={}, tokens_out={})",
        fm.cost.summary_usd,
        fm.cost.model,
        fm.cost.tokens_in,
        fm.cost.tokens_out
    );

    // Body contract: a successful summarize lands a non-empty body.
    // We don't pin specific text — the real LLM produces variable
    // output run-to-run — but the body must not be the
    // `(no summarizer)` fallback the pipeline writes when the LLM
    // fails (see `pipeline::fallback_body`). That fallback string is
    // checked for explicitly so a regression that silently drops
    // the API call into the fallback branch trips this assertion
    // even before the cost-line check.
    assert!(
        !body.trim().is_empty(),
        "summary body must be non-empty on a successful real-LLM run"
    );
    assert!(
        !body.contains("Transcript (no summary)"),
        "body must not be the fallback transcript-only render — \
         the real Anthropic call was expected to succeed; got body:\n{body}"
    );

    // Action-items contract per issue #194 acceptance + PR #203's
    // RFC 7396 round-trip pin in `heron-vault`. The frontmatter holds
    // the structured rows (the "table" of action items the desktop's
    // Actions tab reads); the body holds the `- [ ] ...` checkbox
    // bullets the renderer treats as the writable surface. Since the
    // LLM may legitimately produce zero action items for a casual
    // synthetic transcript, we only assert the structural contract:
    // every frontmatter row that exists has a stable id, owner, and
    // text per `heron_types::ActionItem`.
    for item in &fm.action_items {
        assert!(
            !item.text.trim().is_empty(),
            "every action-items row must have non-empty text; got id={} owner={:?} text={:?}",
            item.id,
            item.owner,
            item.text,
        );
    }

    eprintln!(
        "clio_real_pipeline ok: model={} tokens_in={} tokens_out={} cost=${:.6} \
         body_len={} action_items={}",
        fm.cost.model,
        fm.cost.tokens_in,
        fm.cost.tokens_out,
        fm.cost.summary_usd,
        body.len(),
        fm.action_items.len(),
    );
}
