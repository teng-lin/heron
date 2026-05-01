//! Per-component health probes for `/health`.
//!
//! Each `*_health_component` here returns a [`HealthComponent`] for a
//! single concern (STT backend, LLM selector, vault root, EventKit,
//! capture pipeline). [`aggregate_health_status`] folds the
//! [`HealthComponents`] struct into a single [`HealthStatus`] —
//! `Down` if any component is `Down` / `PermissionMissing`, `Degraded`
//! if any is `Degraded`, otherwise `Ok`. The components are
//! destructured exhaustively so adding a new field to
//! `HealthComponents` upstream becomes a compile error here rather
//! than silently never flipping `/health` to `Down`.
//!
//! `eventkit_health_component` deliberately does NOT call into
//! EventKit on the `/health` path: a fresh macOS install would surface
//! the TCC permission prompt to whatever process polled `/health`,
//! which is the wrong UX. The read endpoints (`list_upcoming_calendar`)
//! report the real PermissionMissing/Timeout when they actually try.

use std::path::Path;

use chrono::Utc;
use heron_session::{ComponentState, Health, HealthComponent, HealthComponents, HealthStatus};

use crate::LocalSessionOrchestrator;

pub(crate) fn health_component(
    state: ComponentState,
    message: impl Into<String>,
) -> HealthComponent {
    HealthComponent {
        state,
        message: Some(message.into()),
        last_check: Some(Utc::now()),
    }
}

pub(crate) fn aggregate_health_status(components: &HealthComponents) -> HealthStatus {
    // Destructure so the compiler errors on a new `HealthComponents`
    // field that this aggregator forgets — otherwise a broken new
    // component could silently never flip `/health` to `Down`
    // (`aggregate_health_status_truth_table` only pins the components
    // currently listed).
    let HealthComponents {
        capture,
        whisperkit,
        vault,
        eventkit,
        llm,
    } = components;
    let mut degraded = false;
    for component in [capture, whisperkit, vault, eventkit, llm] {
        match component.state {
            ComponentState::Down | ComponentState::PermissionMissing => return HealthStatus::Down,
            ComponentState::Degraded => degraded = true,
            ComponentState::Ok => {}
        }
    }
    if degraded {
        HealthStatus::Degraded
    } else {
        HealthStatus::Ok
    }
}

pub(crate) fn stt_health_component(backend_name: &str) -> HealthComponent {
    match heron_speech::build_backend(backend_name, &[]) {
        Ok(backend) if backend.is_available() => health_component(
            ComponentState::Ok,
            format!("STT backend configured: {}", backend.name()),
        ),
        Ok(backend) => health_component(
            ComponentState::Down,
            format!("STT backend configured but unavailable: {}", backend.name()),
        ),
        Err(err) => health_component(
            ComponentState::Down,
            format!("STT backend configuration failed for {backend_name}: {err}"),
        ),
    }
}

pub(crate) fn llm_health_component(preference: heron_llm::Preference) -> HealthComponent {
    let availability = heron_llm::Availability::detect();
    match heron_llm::select_backend(preference, &availability) {
        Ok((backend, reason)) => {
            let reason_msg = match &reason {
                heron_llm::SelectionReason::PreferredBackendAvailable(_) => "preferred".to_owned(),
                heron_llm::SelectionReason::FellBackTo { because, .. } => {
                    format!("fell back: {because}")
                }
            };
            health_component(
                ComponentState::Ok,
                format!("LLM backend selected: {backend:?} ({reason_msg})"),
            )
        }
        Err(err) => health_component(
            ComponentState::Down,
            format!("no LLM backend available for {preference:?}: {err}"),
        ),
    }
}

pub(crate) fn eventkit_health_component() -> HealthComponent {
    // Avoid calling EventKit from /health: on a fresh macOS install
    // the permission prompt blocks the caller. The read endpoint
    // reports PermissionMissing/Timeout with the real TCC result.
    health_component(
        ComponentState::Ok,
        "calendar reader configured; EventKit permission is checked on read",
    )
}

pub(crate) fn capture_health_component(vault_root: Option<&Path>) -> HealthComponent {
    match vault_root {
        Some(root) if root.exists() => health_component(
            ComponentState::Ok,
            format!(
                "capture pipeline configured with vault root: {}",
                root.display()
            ),
        ),
        Some(root) => health_component(
            ComponentState::Degraded,
            format!(
                "capture pipeline configured, but vault root does not exist yet: {}",
                root.display(),
            ),
        ),
        None => health_component(
            ComponentState::Degraded,
            "synthetic capture available; configure a vault for persisted audio/transcript/summary",
        ),
    }
}

pub(crate) fn vault_health_component(vault_root: Option<&Path>) -> HealthComponent {
    match vault_root {
        Some(root) if root.exists() => health_component(
            ComponentState::Ok,
            format!("vault root: {}", root.display()),
        ),
        Some(root) => health_component(
            ComponentState::Down,
            format!(
                "configured vault root does not exist on disk: {}",
                root.display(),
            ),
        ),
        // Unconfigured vault keeps the daemon usable via synthetic
        // capture (see `capture_health_component`); reporting `Down`
        // here would flip `/health` to `Down` for what is really a
        // soft, recoverable misconfig — operators expect `Down` to
        // mean "broken", not "not yet pointed at a directory".
        None => health_component(
            ComponentState::Degraded,
            "vault root is not configured; persistence is disabled until one is set",
        ),
    }
}

pub(crate) async fn current(orch: &LocalSessionOrchestrator) -> Health {
    // Keep /health side-effect-free: no EventKit permission prompt,
    // no model download, no hosted-LLM network request. The
    // endpoint reports local orchestrator wiring and cheap backend
    // availability; operation-specific failures still surface from
    // the corresponding read/capture/summarize paths.
    //
    // The probes do touch the filesystem (`Path::exists`) and
    // PATH (`which` inside `heron_llm::Availability::detect`),
    // both blocking syscalls — run them on the blocking pool so
    // an unlucky disk stall can't park the async runtime.
    let vault_root = orch.vault_root.clone();
    let stt_backend_name = orch.stt_backend_name.clone();
    let llm_preference = orch.llm_preference;
    let probe = tokio::task::spawn_blocking(move || {
        let components = HealthComponents {
            capture: capture_health_component(vault_root.as_deref()),
            whisperkit: stt_health_component(&stt_backend_name),
            vault: vault_health_component(vault_root.as_deref()),
            eventkit: eventkit_health_component(),
            llm: llm_health_component(llm_preference),
        };
        let status = aggregate_health_status(&components);
        Health {
            status,
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            components,
        }
    })
    .await;
    match probe {
        Ok(health) => health,
        // Probe functions don't panic and the runtime doesn't
        // cancel us, so a `JoinError` here means a real bug —
        // surface it as `Down` rather than panic, so a single
        // bad health probe can't take the daemon down with it.
        Err(err) => Health {
            status: HealthStatus::Down,
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            components: HealthComponents {
                capture: health_component(
                    ComponentState::Down,
                    format!("health probe task failed: {err}"),
                ),
                whisperkit: health_component(ComponentState::Down, "health probe task failed"),
                vault: health_component(ComponentState::Down, "health probe task failed"),
                eventkit: health_component(ComponentState::Down, "health probe task failed"),
                llm: health_component(ComponentState::Down, "health probe task failed"),
            },
        },
    }
}
