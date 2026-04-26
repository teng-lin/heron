//! Manage `herond` as an in-process axum service co-tenanted with the
//! Tauri runtime — Gap #7 from `docs/codebase-gaps.md`.
//!
//! Before this module shipped, the onboarding wizard's five Test
//! buttons verified TCC permissions in isolation and then the user
//! landed on the main UI — but the daemon (`crates/herond`) was
//! never started. Anything in the WebView that talks to
//! `http://127.0.0.1:7384/v1/...` (meeting list, settings, future
//! tray actions) silently failed on first use because nothing was
//! listening. This module fixes that by:
//!
//! 1. Building the same [`herond::build_app`] router the standalone
//!    `herond` binary in `crates/herond/src/main.rs` builds, sharing
//!    the [`LocalSessionOrchestrator`] [`event_bus::install`] already
//!    constructed for the desktop crate (so a future in-process
//!    publisher fans out across **both** the SSE projection and the
//!    Tauri IPC sink without going through the loopback).
//! 2. Binding [`herond::DEFAULT_BIND`] (= `127.0.0.1:7384`, pinned by
//!    the OpenAPI's `servers[0].url`) and serving until the Tauri
//!    [`tauri::RunEvent::Exit`] hook fires the shutdown signal.
//! 3. Failing **soft** on bind error. If port 7384 is already in use
//!    — almost always because the user has a separate `herond` binary
//!    running, e.g. for development — we log a warning and continue
//!    without spawning. The desktop app keeps working; the
//!    [`probe`] status check sees the existing daemon and the UI
//!    renders normally. Crashing the desktop here would be the
//!    wrong tradeoff: the visible failure ("the app won't open")
//!    is worse than the silent one this module replaces.
//!
//! ## Bearer-token sharing
//!
//! The daemon mints / loads the bearer token at
//! `~/.heron/cli-token` via [`herond::auth::load_or_mint`]. We call
//! the same function from the Tauri side and stash the resulting
//! [`AuthConfig`] in [`DaemonHandle`], so any future authenticated
//! Tauri command (the OpenAPI surface other than `/health` requires
//! `Authorization: Bearer <token>`) can read it without re-deriving.
//! Both processes converge on the same token because
//! `load_or_mint` is idempotent on a populated file.
//!
//! `/v1/health` carries `security: []` per the OpenAPI, so the
//! liveness probe in [`probe`] does **not** need the bearer.
//!
//! ## Probe timeout
//!
//! The daemon-status probe runs from onboarding, where it sits behind
//! a Test button the user just clicked. **1 second** is the budget:
//! long enough to comfortably outlast a healthy `/v1/health` (a
//! direct vault scan in [`heron_orchestrator`] is sub-millisecond,
//! plus single-digit-ms loopback overhead), short enough that a
//! truly wedged daemon doesn't make the wizard feel hung. The 500 ms
//! TCC probes elsewhere in `onboarding.rs` chose 500 ms because the
//! call sites are pure CoreAudio FFI; for a TCP loopback round-trip
//! the looser bound is the right one.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use heron_orchestrator::LocalSessionOrchestrator;
use herond::auth::AuthConfig;
use herond::{AppState, DEFAULT_BIND, build_app};
use serde::Serialize;
use tauri::{AppHandle, Manager, Runtime};
use thiserror::Error;
use tokio::sync::oneshot;

/// Maximum time the [`probe`] helper waits for a `/v1/health`
/// response. See module docs for the choice of 1 s.
const PROBE_TIMEOUT: Duration = Duration::from_millis(1_000);

/// URL the [`probe`] helper hits. Hardcoded against
/// [`herond::DEFAULT_BIND`] because (a) the OpenAPI pins this port
/// and (b) accepting a renderer-supplied URL here would widen the
/// "Tauri command makes outbound HTTP" surface to anything an
/// attacker-controlled webview could fabricate. The unit tests
/// exercise the parameterized [`probe_url`] directly with an
/// ephemeral port.
pub(crate) const HEALTH_URL: &str = "http://127.0.0.1:7384/v1/health";

/// Failure modes for [`install`]. Modeled as an enum so callers in
/// `lib::run`'s setup hook can tell "you tried to install twice"
/// (programming bug — we panic) apart from "TCP bind failed for an
/// expected reason" (operational — we log + continue).
#[derive(Debug, Error)]
pub enum InstallError {
    /// `install` was called more than once. Tauri's setup hook fires
    /// once per app, so a duplicate is a programming bug rather than
    /// a recoverable runtime condition.
    #[error("daemon::install called twice on the same AppHandle")]
    AlreadyInstalled,
}

/// Stashed in Tauri's state map by [`install`] for the lifetime of
/// the app. Holds the channel we use to ask the in-process axum
/// service to stop, plus the shared bearer token so Tauri commands
/// can authenticate to the daemon.
///
/// `shutdown_tx` lives in a `Mutex<Option<…>>` so the
/// [`tauri::RunEvent::Exit`] callback (which only has `&AppHandle`)
/// can `take` the sender, send the signal, and let the axum task
/// finish its in-flight requests. The mutex is held for tens of
/// nanoseconds; lock contention is irrelevant.
pub struct DaemonHandle {
    /// `Some` until shutdown is signaled, then `None`.
    /// `Mutex<Option<oneshot::Sender<()>>>` instead of
    /// `OnceCell<oneshot::Sender>` because `oneshot::Sender::send`
    /// consumes by value — we need the lock anyway to take ownership.
    shutdown_tx: Mutex<Option<oneshot::Sender<()>>>,
    /// Shared with the daemon's [`AppState`]. Future Tauri commands
    /// that need to make authenticated calls to the daemon (i.e.
    /// every endpoint except `/health`) read the bearer from here.
    pub auth: Arc<AuthConfig>,
}

impl DaemonHandle {
    /// Best-effort shutdown signal. Idempotent: a second call after
    /// the first return is a no-op. The `oneshot` receiver lives
    /// inside the spawned axum task's `with_graceful_shutdown` — when
    /// the sender drops or sends, the future resolves and axum
    /// stops accepting new requests, drains in-flight ones, and
    /// returns from `serve`.
    ///
    /// Doesn't `await` task completion: the `tauri::RunEvent::Exit`
    /// callback that calls this is sync (`FnMut(&AppHandle, RunEvent)`)
    /// and Tauri tears down its own runtime almost immediately
    /// afterward. The graceful-shutdown contract is "stop accepting
    /// new connections" — drain happens on a best-effort basis as
    /// the runtime drains. This matches the pattern `event_bus`
    /// uses for its forwarder task (relies on Tauri-runtime
    /// teardown for the final join).
    pub fn signal_shutdown(&self) {
        let mut guard = match self.shutdown_tx.lock() {
            Ok(g) => g,
            // Mutex poisoned by a previous panic — recover the inner
            // value. We're sending a one-shot signal; the lock's
            // invariant (an `Option`) is intact regardless.
            Err(p) => p.into_inner(),
        };
        if let Some(tx) = guard.take() {
            // `send` returns Err when the receiver dropped (i.e. the
            // axum task already exited for another reason — bind
            // failure, panic). That's the operational equivalent of
            // "already shut down"; nothing to do.
            let _ = tx.send(());
        }
    }
}

/// Wire-format reply for [`heron_daemon_status`].
///
/// The frontend distinguishes a real daemon (Pass on the onboarding
/// 6th step) from a missing one by inspecting `running`, so the
/// shape is intentionally minimal. `version` is `Option<String>`
/// because a healthy daemon may not yet populate
/// [`heron_session::Health::version`]; when it does, the wizard can
/// surface it. `error` is the Display form of the lower-level error
/// if the probe failed — kept as a single string to avoid coupling
/// the JS side to reqwest's error taxonomy.
#[derive(Debug, Clone, Serialize)]
pub struct DaemonStatus {
    pub running: bool,
    pub version: Option<String>,
    pub error: Option<String>,
}

/// Construct the in-process daemon's [`AppState`], bind
/// [`herond::DEFAULT_BIND`], and spawn an axum task that serves
/// until the [`DaemonHandle`] is signaled. Stash the handle in the
/// Tauri state map so the [`tauri::RunEvent::Exit`] callback can
/// reach it.
///
/// The `orchestrator` argument is the **same** `Arc` that
/// `event_bus::install_with` holds. A future in-process publisher
/// (a heron-cli v2 command running locally instead of round-tripping
/// through HTTP, an ambient detection signal, etc.) thus fans out
/// across both transports off one bus.
///
/// # Errors
///
/// - [`InstallError::AlreadyInstalled`] if a [`DaemonHandle`] is
///   already managed for this `AppHandle`. Programming bug — caller
///   should propagate to the setup hook (which crashes loudly, the
///   right call for a missing-init-step).
///
/// # Bind-failure semantics
///
/// A failed `bind()` (typically `EADDRINUSE` because a separate
/// `herond` is already running) is logged as a warning and
/// **swallowed**. The function still installs the [`DaemonHandle`]
/// (with a closed-receiver shutdown channel) so [`signal_shutdown`]
/// stays a no-op-safe call from the Exit hook, but no axum task
/// is spawned. The status probe will then see the *external*
/// daemon and report `running: true`, which is the right user
/// experience: their UI works either way. Returning an error here
/// would make first launch crash for any developer running a
/// separate `herond` in another terminal.
pub async fn install<R: Runtime>(
    app: &AppHandle<R>,
    orchestrator: Arc<LocalSessionOrchestrator>,
) -> Result<(), InstallError> {
    // Resolve / mint the bearer token from the same `~/.heron/cli-token`
    // file the standalone `herond` binary uses. `load_or_mint` is
    // idempotent on an existing populated file (proven in
    // `crates/herond/src/auth.rs::tests`), so a desktop start that
    // races a `herond` start converges on the same token regardless
    // of who got there first.
    //
    // A failure here (e.g. unwritable home dir) downgrades to an
    // empty-bearer config. The daemon will still answer `/health`
    // (which carries `security: []`) so the status probe stays
    // green; every other route returns 401 until the user fixes
    // their home-dir perms and restarts. The alternative —
    // crashing the desktop here — would block launch on a fixable
    // file-permission problem.
    let auth = match herond::auth::default_token_path().and_then(|p| herond::auth::load_or_mint(&p))
    {
        Ok(a) => Arc::new(a),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "could not load bearer token; daemon will start without authenticated routes",
            );
            Arc::new(AuthConfig {
                bearer: String::new(),
            })
        }
    };

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let handle = DaemonHandle {
        shutdown_tx: Mutex::new(Some(shutdown_tx)),
        auth: Arc::clone(&auth),
    };

    if !app.manage(handle) {
        return Err(InstallError::AlreadyInstalled);
    }

    // Build the same router `crates/herond/src/main.rs` builds. The
    // `Arc<dyn SessionOrchestrator>` upcast happens at the AppState
    // boundary — the trait bound on `AppState::orchestrator` is
    // `Arc<dyn SessionOrchestrator>`, and `LocalSessionOrchestrator`
    // implements that trait, so the Arc::clone re-types implicitly.
    let state = AppState { orchestrator, auth };
    let app_router = build_app(state);

    // Bind on the OpenAPI-pinned port. `tokio::net::TcpListener::bind`
    // is async; we're already inside the Tauri-managed runtime
    // (the caller `await`s us from the setup hook via
    // `tauri::async_runtime::block_on`). `EADDRINUSE` and other
    // bind errors flow into the soft-fail branch below.
    match tokio::net::TcpListener::bind(DEFAULT_BIND).await {
        Ok(listener) => {
            tracing::info!(
                bind = DEFAULT_BIND,
                "in-process herond listening (Gap #7); shutdown via DaemonHandle::signal_shutdown",
            );
            tauri::async_runtime::spawn(async move {
                let server = axum::serve(listener, app_router).with_graceful_shutdown(async move {
                    // Receiver completes on either `send(())` from
                    // the Exit hook OR on the sender being dropped
                    // (e.g. desktop crash). Both are valid "stop
                    // serving" signals.
                    let _ = shutdown_rx.await;
                });
                if let Err(e) = server.await {
                    tracing::error!(error = %e, "in-process herond axum::serve exited with error");
                } else {
                    tracing::debug!("in-process herond axum::serve exited cleanly");
                }
            });
        }
        Err(e) => {
            // The most likely cause is that a separate `herond`
            // binary is already bound to 7384 (developer running
            // `cargo run -p herond` in another terminal). Don't
            // crash the desktop — the status probe will see that
            // external daemon and the UI works.
            tracing::warn!(
                bind = DEFAULT_BIND,
                error = %e,
                "could not bind in-process herond; another daemon may already own the port. \
                 Status probe will discover any external daemon at the same address.",
            );
        }
    }

    Ok(())
}

/// Probe the daemon's `/v1/health` against [`HEALTH_URL`]. Used by
/// the `heron_daemon_status` Tauri command.
pub async fn probe() -> DaemonStatus {
    probe_url(HEALTH_URL).await
}

/// Parameterized core of [`probe`]. Split out so unit tests can
/// drive it against an ephemeral-port test server.
///
/// Returns `running: true` iff a 200 OK with a parseable body comes
/// back inside [`PROBE_TIMEOUT`]. A 200 with an unparseable body is
/// still `running: true` — the daemon answered, just not in a shape
/// we recognize. The `error` field captures the parse failure so a
/// developer can see what shape arrived.
pub async fn probe_url(url: &str) -> DaemonStatus {
    let client = match reqwest::Client::builder().timeout(PROBE_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            return DaemonStatus {
                running: false,
                version: None,
                error: Some(format!("client build: {e}")),
            };
        }
    };
    match client.get(url).send().await {
        Ok(resp) => {
            let status = resp.status();
            if !status.is_success() {
                return DaemonStatus {
                    running: false,
                    version: None,
                    error: Some(format!("non-success status: {status}")),
                };
            }
            match resp.json::<serde_json::Value>().await {
                Ok(body) => DaemonStatus {
                    running: true,
                    version: body
                        .get("version")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned),
                    error: None,
                },
                Err(e) => DaemonStatus {
                    running: true,
                    version: None,
                    error: Some(format!("response body parse: {e}")),
                },
            }
        }
        Err(e) => DaemonStatus {
            running: false,
            version: None,
            // `reqwest::Error::Display` includes the URL when the
            // failure is a connect or timeout, which is exactly what
            // a developer staring at the onboarding wizard wants to
            // see ("connection refused at 127.0.0.1:7384" → port not
            // bound; "timeout" → daemon wedged).
            error: Some(e.to_string()),
        },
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
#[allow(clippy::unwrap_used)]
mod tests {
    //! These tests bind a fresh in-process axum server (the same
    //! router `crates/herond/src/main.rs` builds, but pointed at a
    //! [`herond::stub::StubOrchestrator`] so we don't need a vault
    //! or a Tokio-runtime'd `LocalSessionOrchestrator`) on an
    //! ephemeral port and drive [`probe_url`] against it.
    //!
    //! The shared health response from `StubOrchestrator` carries
    //! `version = env!("CARGO_PKG_VERSION")` (see
    //! `crates/herond/src/stub.rs::health`), so we can pin the
    //! parsed-version assertion without coupling to a hardcoded
    //! string.

    use super::*;
    use herond::stub::StubOrchestrator;
    use herond::{AppState, AuthConfig};
    use std::net::SocketAddr;

    /// Spin up an ephemeral-port axum server using the same
    /// `herond::build_app` the production daemon uses. Returns the
    /// bound address + a oneshot sender that, when fired, asks the
    /// server to gracefully exit. The caller drops the sender at
    /// the end of the test which also shuts the server down.
    async fn spawn_test_server() -> (SocketAddr, oneshot::Sender<()>) {
        let state = AppState {
            orchestrator: Arc::new(StubOrchestrator::new()),
            auth: Arc::new(AuthConfig {
                bearer: "test".to_owned(),
            }),
        };
        let router = build_app(state);
        // Bind 127.0.0.1:0 to get an ephemeral free port. Avoids
        // races with anything else on the box (parallel test runs,
        // a real herond, etc.).
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("ephemeral bind");
        let addr = listener.local_addr().expect("local_addr");
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = rx.await;
                })
                .await;
        });
        (addr, tx)
    }

    #[tokio::test]
    async fn probe_url_returns_running_against_real_health() {
        let (addr, _tx) = spawn_test_server().await;
        let url = format!("http://{addr}/v1/health");
        let status = probe_url(&url).await;
        assert!(status.running, "expected running=true, got {status:?}");
        // The stub orchestrator pins
        // `version = Some(env!("CARGO_PKG_VERSION").to_owned())` —
        // we only need to assert the field round-tripped, not the
        // value, so a future cargo bump doesn't break this test.
        assert!(
            status.version.is_some(),
            "expected version to round-trip, got {status:?}",
        );
        assert!(status.error.is_none(), "unexpected error: {status:?}");
    }

    #[tokio::test]
    async fn probe_url_reports_not_running_when_port_unbound() {
        // Bind an ephemeral port, immediately drop the listener so
        // the OS releases it. The probe to that address should fail
        // fast with "connection refused".
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        drop(listener);
        let url = format!("http://{addr}/v1/health");
        let status = probe_url(&url).await;
        assert!(
            !status.running,
            "expected running=false against unbound port, got {status:?}",
        );
        assert!(status.error.is_some(), "missing error string: {status:?}");
    }

    #[tokio::test]
    async fn probe_url_reports_not_running_on_404() {
        let (addr, _tx) = spawn_test_server().await;
        // The daemon's API is mounted under /v1; an unrelated path
        // returns 404. Probe must surface this as not-running so a
        // future router rename doesn't silently green-light a wrong
        // process.
        let url = format!("http://{addr}/no-such-path");
        let status = probe_url(&url).await;
        assert!(
            !status.running,
            "expected running=false on 404, got {status:?}",
        );
    }

    #[tokio::test]
    async fn probe_url_times_out_quickly_against_blackhole() {
        // 192.0.2.0/24 is RFC 5737 TEST-NET-1: documented as
        // unroutable. A connect attempt should hang until the OS
        // gives up — the probe's 1 s `client.timeout` must beat
        // that, otherwise the onboarding wizard would wait minutes.
        // We allow a 3 s budget here to comfortably outlast the
        // 1 s probe timeout plus scheduler jitter; if the assertion
        // ever flakes, the regression is in the timeout wiring,
        // not in the test budget.
        use tokio::time::Instant;
        let url = "http://192.0.2.1:7384/v1/health";
        let start = Instant::now();
        let status = probe_url(url).await;
        let elapsed = start.elapsed();
        assert!(
            !status.running,
            "expected running=false against blackhole, got {status:?}",
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "probe didn't honor its timeout; took {elapsed:?}",
        );
    }

    #[test]
    fn health_url_is_loopback_v1() {
        // Defence against a future refactor that picks a different
        // port or path: the OpenAPI pins both. If this assertion
        // ever fails, also update `herond::DEFAULT_BIND` and
        // `crates/herond/src/lib.rs::API_PREFIX`.
        assert_eq!(HEALTH_URL, "http://127.0.0.1:7384/v1/health");
    }

    #[test]
    fn daemon_status_serializes_with_expected_fields() {
        let s = DaemonStatus {
            running: true,
            version: Some("0.1.0".into()),
            error: None,
        };
        let v = serde_json::to_value(&s).expect("ser");
        assert_eq!(v["running"], true);
        assert_eq!(v["version"], "0.1.0");
        // `error: None` serializes as `null`; lock that so a future
        // `#[serde(skip_serializing_if = "Option::is_none")]` change
        // is a deliberate one (the React side currently checks for
        // both the field's absence and a `null`, but pinning the
        // wire shape stops accidental drift).
        assert!(v["error"].is_null());
    }
}
