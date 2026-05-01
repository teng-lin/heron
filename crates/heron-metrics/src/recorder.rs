//! Prometheus recorder install + handle for the daemon HTTP exporter.
//!
//! The `metrics` crate is a facade: every `metrics::counter!` /
//! `histogram!` / `gauge!` macro call dispatches into a globally
//! installed [`metrics::Recorder`]. We install
//! [`metrics_exporter_prometheus::PrometheusRecorder`] once at daemon
//! startup; thereafter the [`MetricsHandle`] (which is just
//! `metrics_exporter_prometheus::PrometheusHandle`) renders the
//! current snapshot in Prometheus exposition format on demand.
//!
//! The handle is cheap to clone — share one across handlers.
//!
//! [`init_prometheus_recorder`] is idempotent: a second call returns
//! a clone of the cached handle without re-installing. Concurrent
//! callers race-serialize through an internal `Mutex` so only one
//! thread ever calls the underlying `install_recorder()`. Test
//! harnesses spinning up multiple `herond::AppState` instances in
//! one process therefore don't stomp on each other.

use std::sync::OnceLock;

use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

/// Process-global handle. Stored so `init_prometheus_recorder` is
/// idempotent — once installed, every subsequent call returns the
/// same handle. This matters for tests: a `cargo test` run that
/// brings up two `AppState`s in one process needs both to share the
/// recorder rather than one calling `install_recorder()` twice and
/// failing.
static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Errors from [`init_prometheus_recorder`]. The "already
/// installed" case is NOT an error in this crate's model — the
/// installer is idempotent and returns a clone of the cached
/// handle. The only error path is the underlying builder
/// failing.
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    /// Building the recorder failed. In practice this is the
    /// underlying `metrics_exporter_prometheus::BuildError` (or
    /// the install-mutex being poisoned by a panicking caller),
    /// surfaced as `String` so we don't leak the inner crate's
    /// error type into our public API.
    #[error("build prometheus recorder: {0}")]
    Build(String),
}

/// Install a process-global Prometheus recorder. Idempotent: returns
/// the existing handle if one is already installed.
///
/// Concurrency: tests run in parallel and can race the install. We
/// serialize the slow path behind a `Mutex` so only one thread ever
/// calls `PrometheusBuilder::install_recorder()`; the others wait
/// and read the cached handle from [`HANDLE`].
///
/// The handle exposes `.render()` → `String` in Prometheus exposition
/// format. Wire it into a daemon HTTP route (see
/// `crates/herond/src/routes/metrics.rs`) for local inspection.
pub fn init_prometheus_recorder() -> Result<MetricsHandle, InstallError> {
    // Hot path: the recorder is already installed and the handle is
    // cached. Cheap clone, no lock.
    if let Some(existing) = HANDLE.get() {
        return Ok(MetricsHandle {
            inner: existing.clone(),
        });
    }

    // Slow path: race-serialized through a static `Mutex`. We
    // re-check `HANDLE` inside the lock so the loser of the race
    // returns the winner's handle instead of double-installing.
    use std::sync::Mutex;
    static INSTALL_LOCK: Mutex<()> = Mutex::new(());
    // `Mutex::lock()` only fails on poisoning. A poisoned install
    // mutex means a previous installer panicked mid-install, and
    // the recorder is in an indeterminate state. Surface that as a
    // `Build` error instead of unwrapping into a panic — the caller
    // (daemon startup, test setup) can decide what to do.
    let _guard = INSTALL_LOCK
        .lock()
        .map_err(|_| InstallError::Build("install mutex poisoned".to_owned()))?;
    if let Some(existing) = HANDLE.get() {
        return Ok(MetricsHandle {
            inner: existing.clone(),
        });
    }

    let handle = PrometheusBuilder::new()
        .install_recorder()
        .map_err(|e| InstallError::Build(e.to_string()))?;
    // We hold the install lock and `HANDLE` was None inside the
    // lock, so this `set` is the first one. Even so, ignore a
    // hypothetical `Err` rather than panicking — if it ever fires,
    // the freshly-installed handle is the canonical source and is
    // returned below.
    let _ = HANDLE.set(handle.clone());
    Ok(MetricsHandle { inner: handle })
}

/// Handle for rendering the current snapshot. Cheap to clone.
#[derive(Clone)]
pub struct MetricsHandle {
    inner: PrometheusHandle,
}

impl MetricsHandle {
    /// Render the current metrics snapshot in Prometheus exposition
    /// format. Suitable as the body of a `GET /__metrics` handler.
    pub fn render(&self) -> String {
        self.inner.render()
    }
}

impl std::fmt::Debug for MetricsHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // PrometheusHandle's own Debug impl can dump the entire
        // metric registry. Truncate so a `tracing::debug!(?handle)`
        // doesn't accidentally spill thousands of metric lines into
        // the log.
        f.debug_struct("MetricsHandle").finish_non_exhaustive()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::SMOKE_CAPTURE_STARTED_TOTAL;

    #[test]
    fn init_is_idempotent_and_records_smoke_metric() {
        // Both calls succeed; the second returns a clone of the
        // already-installed handle. Important: `cargo test` runs
        // tests in one process, so multiple test cases sharing the
        // global recorder is the steady state.
        let handle = init_prometheus_recorder().expect("install");
        let _again = init_prometheus_recorder().expect("re-install is idempotent");

        // Drive the smoke metric and assert it shows up in the
        // exposition output. Using the canonical const so a rename
        // of the smoke metric flows through.
        metrics::counter!(SMOKE_CAPTURE_STARTED_TOTAL).increment(1);
        let body = handle.render();

        assert!(
            body.contains(SMOKE_CAPTURE_STARTED_TOTAL),
            "rendered exposition must contain smoke metric name. Got:\n{body}"
        );
    }
}
