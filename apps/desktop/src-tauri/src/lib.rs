//! Tauri v2 desktop shell — v0 scaffold.
//!
//! The full 5-step onboarding + recording UX + review UI lands
//! across §13–§16. This module ships:
//!
//! - the Tauri builder bootstrap (no-op for plugins; the audio /
//!   speech / vault wiring goes in once `heron-cli::session`
//!   gains a `run()` async path),
//! - one demonstrative `heron_status` command that returns the
//!   FSM state + a few orchestrator-side flags so the frontend
//!   has something concrete to render before week 11.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct HeronStatus {
    pub version: String,
    /// Serializes via `RecordingState`'s `#[serde(rename_all =
    /// "snake_case")]`, so the wire format matches what the
    /// frontend already parses ("idle" / "armed" / etc.).
    pub fsm_state: heron_types::RecordingState,
    pub audio_available: bool,
    pub ax_backend: String,
}

#[tauri::command]
fn heron_status() -> HeronStatus {
    let fsm = heron_types::RecordingFsm::new();
    HeronStatus {
        version: env!("CARGO_PKG_VERSION").to_string(),
        fsm_state: fsm.state(),
        // Once heron-audio's real capture lands, this will probe the
        // process tap permissions; v0 reports the stub state.
        audio_available: false,
        ax_backend: "ax-observer".into(),
    }
}

/// Entry point used by `main.rs` and (eventually) by Tauri's mobile
/// build target. The function name + `#[cfg_attr(...)]` line below
/// are required by Tauri 2's mobile entry point glue.
///
/// Panics if the Tauri context fails to start. This is the right
/// failure mode at the binary entry point — there is nothing the
/// caller can do to recover, and the panic message lands in the
/// system log.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
#[allow(clippy::expect_used)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            // No-op for v0; week 11 wires the capture + status
            // pipelines here.
            let _ = app;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![heron_status])
        .run(tauri::generate_context!())
        .expect("error while running heron-desktop");
}
