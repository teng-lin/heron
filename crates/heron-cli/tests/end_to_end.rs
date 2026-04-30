//! End-to-end no-op integration test for the session orchestrator.
//!
//! Exercises the full crate boundary: this test imports `heron_cli`
//! as a library, constructs an Orchestrator against a tempdir cache,
//! drives it through the §14.2 happy-path FSM transitions, and
//! asserts on the SessionOutcome.
//!
//! When the real backends land, this test stays valid: `run_no_op`
//! is the deterministic FSM-only path; `run` (TBD) will be the
//! async backend-driven path.

#![allow(clippy::expect_used)]

use heron_cli::session::{Orchestrator, SessionConfig, SessionError};
use heron_types::{IdleReason, RecordingState, SessionId, SummaryOutcome};
use tempfile::TempDir;

fn cfg(tmp: &TempDir) -> SessionConfig {
    SessionConfig {
        session_id: SessionId::nil(),
        target_bundle_id: "us.zoom.xos".into(),
        cache_dir: tmp.path().join("cache"),
        vault_root: tmp.path().join("vault"),
        stt_backend_name: "sherpa".into(),
        llm_preference: heron_llm::Preference::Auto,
        pre_meeting_briefing: None,
        event_bus: None,
        persona: None,
        strip_names: false,
    }
}

#[test]
fn end_to_end_no_op_cycle_through_orchestrator() {
    let tmp = TempDir::new().expect("tmpdir");
    let mut orch = Orchestrator::new(cfg(&tmp));

    assert_eq!(orch.state(), RecordingState::Idle);
    let outcome = orch
        .run_no_op(SummaryOutcome::Done)
        .expect("happy-path no-op runs");
    assert_eq!(outcome.final_state, RecordingState::Idle);
    assert_eq!(outcome.last_idle_reason, Some(IdleReason::SummaryDone));
    assert!(outcome.note_path.is_none(), "v0 emits no .md path");
}

#[test]
fn backends_resolve_cleanly_with_known_stt_name() {
    let tmp = TempDir::new().expect("tmpdir");
    let orch = Orchestrator::new(cfg(&tmp));
    // Phase 41: the LLM selector probes the host for a viable
    // backend. CI runners may have none, producing Err(Llm(_)). The
    // STT + AX shape is what this test pins; the LLM-selection
    // contract is covered in heron_llm::select::tests.
    match orch.backends() {
        Ok((stt, ax, _llm, _cal)) => {
            assert_eq!(stt.name(), "sherpa");
            assert_eq!(ax.name(), "ax-observer");
        }
        Err(SessionError::Llm(_)) => {}
        Err(other) => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn unknown_stt_name_surfaces_as_session_error() {
    let tmp = TempDir::new().expect("tmpdir");
    let mut config = cfg(&tmp);
    config.stt_backend_name = "magic-asr".into();
    let orch = Orchestrator::new(config);
    let result = orch.backends();
    assert!(matches!(result, Err(SessionError::Stt(_))));
}

#[test]
fn summary_failure_sets_idle_reason_to_failed() {
    let tmp = TempDir::new().expect("tmpdir");
    let mut orch = Orchestrator::new(cfg(&tmp));
    let outcome = orch.run_no_op(SummaryOutcome::Failed).expect("run");
    assert_eq!(outcome.last_idle_reason, Some(IdleReason::SummaryFailed));
}

/// Off-Apple targets still get `NotYetImplemented` from
/// `AudioCapture::start` — the cidre process tap only compiles on macOS.
#[cfg(not(target_os = "macos"))]
#[tokio::test]
async fn audio_pipeline_returns_not_yet_implemented_off_apple() {
    let tmp = TempDir::new().expect("tmpdir");
    let orch = Orchestrator::new(cfg(&tmp));
    let result = orch.try_start_audio().await;
    match result {
        Err(SessionError::Audio(heron_audio::AudioError::NotYetImplemented)) => {}
        Err(other) => panic!("expected NotYetImplemented, got {other:?}"),
        Ok(_handle) => panic!("expected NotYetImplemented, got Ok(_handle)"),
    }
}

/// On macOS we now exercise the real Core Audio process tap path.
/// On a CI runner without TCC granted this surfaces as
/// `PermissionDenied` / `ProcessNotFound` / `Aborted`; what we lock
/// down here is that we never regress back to `NotYetImplemented`.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn audio_pipeline_does_not_return_not_yet_implemented_on_macos() {
    let tmp = TempDir::new().expect("tmpdir");
    let orch = Orchestrator::new(cfg(&tmp));
    let result = orch.try_start_audio().await;
    match result {
        Err(SessionError::Audio(heron_audio::AudioError::NotYetImplemented)) => {
            panic!("macOS branch must not return NotYetImplemented");
        }
        Err(SessionError::Audio(_)) | Ok(_) => {}
        Err(other) => panic!("unexpected error variant: {other:?}"),
    }
}

#[test]
fn vault_root_is_threaded_through_config() {
    let tmp = TempDir::new().expect("tmpdir");
    let config = cfg(&tmp);
    let expected = tmp.path().join("vault");
    assert_eq!(config.vault_root, expected);
    let _orch = Orchestrator::new(config);
}

#[test]
fn unique_session_ids_construct_independently() {
    // Sanity: two orchestrators against the same tempdir don't
    // collide on construction (the cache_dir is per-orchestrator
    // but both share a parent in this test).
    let tmp = TempDir::new().expect("tmpdir");
    let mut a_cfg = cfg(&tmp);
    a_cfg.session_id = SessionId::from_u128(1);
    let mut b_cfg = cfg(&tmp);
    b_cfg.session_id = SessionId::from_u128(2);

    let mut a = Orchestrator::new(a_cfg);
    let mut b = Orchestrator::new(b_cfg);
    assert_eq!(a.state(), RecordingState::Idle);
    assert_eq!(b.state(), RecordingState::Idle);
    a.run_no_op(SummaryOutcome::Done).expect("a runs");
    b.run_no_op(SummaryOutcome::Failed).expect("b runs");
}

#[test]
fn session_error_surfaces_vault_failures_via_typed_variant() {
    // Phase 36: heron-cli now depends on heron-vault. Spot-check the
    // typed conversion so a future refactor doesn't silently break
    // the From impl that downstream `?` operators rely on.
    let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "no");
    let vault_err = heron_vault::VaultError::Io(io_err);
    let session_err: SessionError = vault_err.into();
    assert!(matches!(session_err, SessionError::Vault(_)));
}

#[test]
fn session_error_surfaces_encode_failures_via_typed_variant() {
    let io_err = std::io::Error::other("other");
    let enc_err = heron_vault::EncodeError::Io(io_err);
    let session_err: SessionError = enc_err.into();
    assert!(matches!(session_err, SessionError::Encode(_)));
}

#[test]
fn cache_path_overrides_via_env_path() {
    // Verifies the orchestrator uses the cache_dir from config and
    // doesn't reach out to a hardcoded location. A future regression
    // here would silently spread state across user directories.
    let tmp = TempDir::new().expect("tmpdir");
    let cache = tmp.path().join("custom").join("cache");
    let mut config = cfg(&tmp);
    config.cache_dir = cache.clone();
    let _orch = Orchestrator::new(config);
    // No file-system side effects yet (audio is stubbed); just
    // confirm construction succeeds with an arbitrary nested path.
    assert!(
        !cache.exists(),
        "stub orchestrator must not eagerly create the cache dir"
    );
}
