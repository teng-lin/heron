//! Manage `herond` as an in-process axum service co-tenanted with the
//! Tauri runtime — Gap #7 from `docs/archives/codebase-gaps.md`.
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
use tauri::async_runtime::JoinHandle;
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
///
/// Drift between this constant and `herond::DEFAULT_BIND` /
/// `herond::API_PREFIX` is caught by `health_url_matches_herond_constants`
/// in the test module — `concat!` only accepts literals so we
/// can't compose at const-eval time without a procedural-macro
/// dep, and adding `const_format` for one assertion isn't worth
/// the build-graph weight.
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
    /// The process-global Prometheus metrics recorder failed to
    /// install. The installer in `heron-metrics` is idempotent, so
    /// in practice this only fires if the underlying
    /// `metrics-exporter-prometheus` builder rejects the config.
    #[error("install Prometheus metrics recorder: {0}")]
    MetricsRecorder(String),
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
    ///
    /// Issue #206 (vault rebuild): the field is now mutated in place
    /// by [`Self::replace_for_rebuild`] when the user changes
    /// `Settings.vault_root` and the daemon must be re-bound onto a
    /// freshly-built orchestrator. The `Mutex<Option<…>>` shape
    /// already supports the take-and-replace pattern; the only
    /// addition is the sibling `join_handle` slot below so the
    /// rebuild path can `await` axum drain before re-binding the
    /// port.
    shutdown_tx: Mutex<Option<oneshot::Sender<()>>>,
    /// `Some` until the rebuild path takes it to `await` axum drain
    /// (see [`Self::take_join_handle`]). Held in the same
    /// `Mutex<Option<…>>` shape as `shutdown_tx` so the take/replace
    /// semantics are uniform across the two coupled lifecycle
    /// signals — they are always installed and removed together.
    ///
    /// `None` after construction in the bind-failure branch (no
    /// axum task was spawned); also `None` between the rebuild
    /// path's [`Self::take_join_handle`] and
    /// [`Self::replace_for_rebuild`] calls.
    join_handle: Mutex<Option<JoinHandle<()>>>,
    /// Shared with the daemon's [`AppState`]. Future Tauri commands
    /// that need to make authenticated calls to the daemon (i.e.
    /// every endpoint except `/health`) read the bearer from here.
    ///
    /// Constant across rebuilds: the bearer comes from
    /// `~/.heron/cli-token` via [`herond::auth::load_or_mint`]
    /// (idempotent on a populated file), so a vault swap doesn't
    /// require minting a new token. Public so the meetings/calendar
    /// command shims can clone it without going through a getter.
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
        // Mutex poisoned by a previous panic — recover the inner
        // value. We're sending a one-shot signal; the lock's
        // invariant (an `Option`) is intact regardless. Same
        // recover-on-poison rationale applies to the other locks
        // in this file (`take_join_handle`, `replace_for_rebuild`).
        let mut guard = lock_recover(&self.shutdown_tx);
        if let Some(tx) = guard.take() {
            // `send` returns Err when the receiver dropped (i.e. the
            // axum task already exited for another reason — bind
            // failure, panic). That's the operational equivalent of
            // "already shut down"; nothing to do.
            let _ = tx.send(());
        }
    }

    /// Issue #206: take the axum task's [`JoinHandle`] so the rebuild
    /// path can `await` axum drain before binding a fresh listener
    /// onto the same port. Returns `None` when the daemon was
    /// installed in its bind-failure branch (no task spawned), or
    /// when the rebuild path is mid-flight (after this call but
    /// before the matching [`Self::replace_for_rebuild`]).
    ///
    /// Uniform `Mutex<Option<…>>` take-semantics with
    /// [`Self::signal_shutdown`]: callers are expected to follow up
    /// with [`Self::replace_for_rebuild`] (rebuild succeeded) so the
    /// next rebuild has both signals installed again. A panic
    /// between the two calls leaves the handle in the "no task"
    /// shape, which is identical to the bind-failure boot state —
    /// safe but means the next [`Self::signal_shutdown`] is a no-op.
    pub fn take_join_handle(&self) -> Option<JoinHandle<()>> {
        lock_recover(&self.join_handle).take()
    }

    /// Issue #206: install a freshly-spawned axum task's lifecycle
    /// signals into this handle. Called exclusively by
    /// [`bind_after_rebuild`] after the new listener bound and the
    /// new task started. The
    /// previous `shutdown_tx` / `join_handle` MUST already be
    /// `None` — either from the bind-failure boot (no task ever
    /// spawned) or because the rebuild path drained them via
    /// [`Self::signal_shutdown`] + [`Self::take_join_handle`]. We
    /// don't enforce this with an assert because the rebuild path
    /// is the only caller and is always paired correctly; an
    /// errant double-install would silently lose the previous
    /// task's shutdown signal, but the previous task already
    /// exited (rebuild only calls us after its `await`).
    ///
    /// Mirrors the take half: both fields swapped together so the
    /// next [`Self::signal_shutdown`] / [`Self::take_join_handle`]
    /// pair sees a consistent state.
    fn replace_for_rebuild(&self, shutdown_tx: oneshot::Sender<()>, join_handle: JoinHandle<()>) {
        *lock_recover(&self.shutdown_tx) = Some(shutdown_tx);
        *lock_recover(&self.join_handle) = Some(join_handle);
    }
}

/// Recover-on-poison helper for the [`DaemonHandle`] mutexes. Each
/// field is `Mutex<Option<T>>` and the only mutation is take/replace
/// of the `Option`, so a poisoned guard is still safe to use — the
/// `Option` invariant doesn't depend on what the panicking thread
/// was doing.
fn lock_recover<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
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
    // A failure here (e.g. unwritable home dir) downgrades to a
    // **fresh transient UUID** rather than an empty bearer. An empty
    // bearer is NOT fail-closed: `Authorization: Bearer ` (with an
    // empty value) splits into the right shape and `bearer_eq("","")`
    // succeeds — that would silently turn every protected localhost
    // route into an unauthenticated one on a token-load failure. A
    // transient UUID locks out external CLIs (which can't read it
    // from disk, since we never persisted it) while still letting
    // any future Tauri command in this process authenticate via the
    // shared `Arc<AuthConfig>` in the `DaemonHandle`. The user's
    // recovery path is "fix the home-dir perms and restart"; the CLI
    // will then see the persisted token again on next launch.
    let auth = match herond::auth::default_token_path().and_then(|p| herond::auth::load_or_mint(&p))
    {
        Ok(a) => Arc::new(a),
        Err(e) => {
            let transient = uuid::Uuid::now_v7().to_string();
            tracing::warn!(
                error = %e,
                "could not load bearer token from ~/.heron/cli-token; \
                 minted a transient in-memory token. External CLIs will \
                 not be able to authenticate until the file is restored.",
            );
            Arc::new(AuthConfig { bearer: transient })
        }
    };

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    // Build the same router `crates/herond/src/main.rs` builds. The
    // `Arc<dyn SessionOrchestrator>` upcast happens at the AppState
    // boundary — the trait bound on `AppState::orchestrator` is
    // `Arc<dyn SessionOrchestrator>`, and `LocalSessionOrchestrator`
    // implements that trait, so the Arc::clone re-types implicitly.
    //
    // The Tauri-embedded daemon shares the same process-global
    // Prometheus recorder as the standalone `herond` binary; the
    // installer is idempotent so a desktop launch that brings up
    // the embedded daemon doesn't fight a future side-by-side
    // CLI daemon over the global slot.
    let metrics = heron_metrics::init_prometheus_recorder()
        .map_err(|e| InstallError::MetricsRecorder(e.to_string()))?;
    let state = AppState {
        orchestrator,
        auth: Arc::clone(&auth),
        metrics,
    };
    let app_router = build_app(state);

    // Bind on the OpenAPI-pinned port. `tokio::net::TcpListener::bind`
    // is async; we're already inside the Tauri-managed runtime
    // (the caller `await`s us from the setup hook via
    // `tauri::async_runtime::block_on`). `EADDRINUSE` and other
    // bind errors flow into the soft-fail branch below.
    let join_handle = match tokio::net::TcpListener::bind(DEFAULT_BIND).await {
        Ok(listener) => {
            tracing::info!(
                bind = DEFAULT_BIND,
                "in-process herond listening (Gap #7); shutdown via DaemonHandle::signal_shutdown",
            );
            // Issue #206: capture the spawn handle so a future vault
            // rebuild can `await` axum drain before re-binding the
            // same port. Pre-#206 the handle was discarded.
            Some(tauri::async_runtime::spawn(async move {
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
            }))
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
            None
        }
    };

    // Issue #206: install the handle *after* the bind branch so the
    // join-handle slot reflects whether a task was actually spawned.
    // Pre-#206 the handle was installed first (so `signal_shutdown`
    // worked even on bind failure); preserving that contract — the
    // shutdown_tx is installed unconditionally so the Exit hook
    // remains a safe no-op call regardless of the bind outcome.
    let handle = DaemonHandle {
        shutdown_tx: Mutex::new(Some(shutdown_tx)),
        join_handle: Mutex::new(join_handle),
        auth,
    };

    if !app.manage(handle) {
        return Err(InstallError::AlreadyInstalled);
    }

    Ok(())
}

/// Issue #206: failure modes for [`shutdown_for_rebuild`] and
/// [`bind_after_rebuild`]. Modeled as an enum so the caller
/// (`heron_write_settings` via
/// [`crate::heron_write_settings`]) can map each variant to a stable
/// string form for the JS bridge without parsing a free-form message.
#[derive(Debug, Error)]
pub enum RebuildError {
    /// [`shutdown_for_rebuild`] / [`bind_after_rebuild`] was called
    /// before [`install`] ever ran. Programming bug — `setup` always
    /// installs before any settings command can fire.
    #[error("daemon rebuild called before daemon::install")]
    NotInstalled,
    /// Rebinding `127.0.0.1:7384` after the previous task drained
    /// failed. Usually transient (TIME_WAIT, another process raced
    /// the slot), but the user's vault swap will not take effect
    /// until the next launch unless we report it.
    #[error("could not rebind in-process herond after vault swap: {0}")]
    Bind(#[source] std::io::Error),
    /// Re-deriving the `Metrics` handle for the new `AppState`
    /// failed. The underlying installer in `heron-metrics` is
    /// idempotent (a no-op on the second call) so this should
    /// effectively never fire after a successful boot install,
    /// but we surface it as a distinct variant so a future
    /// metrics-surface change can't masquerade as a `Bind` error.
    #[error("re-derive Prometheus metrics handle for rebuilt daemon: {0}")]
    Metrics(String),
}

/// Issue #206: signal the existing in-process axum task to stop,
/// and `await` its drain. Pair with [`bind_after_rebuild`] — the
/// caller (`heron_write_settings`'s rebuild path) sequences the
/// orchestrator shutdown between the two so the old recorder task
/// has finished by the time the new daemon serves a request.
///
/// Lifecycle:
///
/// 1. [`DaemonHandle::signal_shutdown`] fires the existing task's
///    oneshot. axum stops accepting new connections and drains
///    in-flight ones.
/// 2. [`DaemonHandle::take_join_handle`] takes the spawn handle and
///    we `await` it. Bounded by [`REBUILD_DRAIN_TIMEOUT`] so a
///    wedged in-flight request can't deadlock the user's settings
///    save.
///
/// On a drain timeout we abort the old task forcefully and wait
/// briefly for the abort to land. This is required, not optional:
/// `bind_after_rebuild` re-binds the same port, and an axum task
/// still holding the LISTENING socket would refuse the new bind
/// with `EADDRINUSE` (Linux/macOS `SO_REUSEADDR` permits TIME_WAIT
/// reuse, not concurrent active listeners). Aborting drops the
/// listener so the rebind in `bind_after_rebuild` can succeed.
pub async fn shutdown_for_rebuild<R: Runtime>(app: &AppHandle<R>) -> Result<(), RebuildError> {
    let Some(state) = app.try_state::<DaemonHandle>() else {
        return Err(RebuildError::NotInstalled);
    };
    state.signal_shutdown();
    let Some(handle) = state.take_join_handle() else {
        return Ok(());
    };
    // `tokio::time::timeout(dur, &mut handle)` lets us keep
    // ownership of `handle` past the timeout so we can `abort()`
    // it. Calling `timeout(dur, handle)` would consume the handle
    // on timeout, leaving the spawned task running with its
    // LISTENING socket still bound — the next `bind` would fail
    // with `EADDRINUSE`.
    let mut handle = handle;
    match tokio::time::timeout(REBUILD_DRAIN_TIMEOUT, &mut handle).await {
        Ok(Ok(())) => {
            tracing::debug!("rebuild: previous axum task drained cleanly");
        }
        Ok(Err(join_err)) => {
            // Task panicked; the orchestrator is in an unknown
            // state but we're discarding it anyway. Log + proceed.
            tracing::warn!(
                error = %join_err,
                "rebuild: previous axum task join error; proceeding with rebind",
            );
        }
        Err(_) => {
            // Drain budget exceeded. Forcefully abort the task so
            // its TCP listener drops and the rebind can succeed.
            // The abort is best-effort: the kernel takes a moment
            // to release the LISTENING socket; we await the join
            // handle (which now resolves with a Cancelled join
            // error) to flush that delay before returning.
            tracing::warn!(
                timeout_ms = REBUILD_DRAIN_TIMEOUT.as_millis() as u64,
                "rebuild: previous axum task did not drain in time; aborting",
            );
            handle.abort();
            // Bound the abort-await with a short timeout so a
            // pathological task that ignores cancellation can't
            // deadlock the rebuild. If the await still doesn't
            // resolve, we proceed and let bind_after_rebuild's
            // `EADDRINUSE` flow surface as a `RebuildError::Bind`.
            let _ = tokio::time::timeout(REBUILD_ABORT_TIMEOUT, handle).await;
        }
    }
    Ok(())
}

/// Issue #206: rebind `127.0.0.1:7384` onto a fresh axum task built
/// against a new orchestrator. Pair with [`shutdown_for_rebuild`] —
/// callers in the `heron_write_settings` rebuild path call
/// `shutdown_for_rebuild` first, then `LocalSessionOrchestrator::shutdown`
/// on the previous orchestrator (deterministic recorder teardown),
/// then this function.
///
/// On a bind error the previous handle slots remain empty — safe
/// (next [`DaemonHandle::signal_shutdown`] is a no-op until the
/// next install) but means the user's vault swap leaves no daemon
/// running until the next launch. The caller propagates the
/// `RebuildError::Bind` to the renderer as a Sonner toast.
pub async fn bind_after_rebuild<R: Runtime>(
    app: &AppHandle<R>,
    orchestrator: Arc<LocalSessionOrchestrator>,
) -> Result<(), RebuildError> {
    let Some(state) = app.try_state::<DaemonHandle>() else {
        return Err(RebuildError::NotInstalled);
    };
    // Tokio's `TcpListener::bind` sets `SO_REUSEADDR` on Unix,
    // which avoids the TIME_WAIT trap on macOS so the rebind
    // succeeds even when the previous socket is still in the
    // kernel's wait queue.
    let listener = tokio::net::TcpListener::bind(DEFAULT_BIND)
        .await
        .map_err(RebuildError::Bind)?;

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    // `init_prometheus_recorder` is idempotent on the process-global
    // metrics slot (see `heron_metrics::init_prometheus_recorder`
    // docs), so this re-call after a vault swap is a cheap no-op
    // that returns the same `Metrics` handle the boot install
    // received. We re-derive it here rather than threading it
    // through `RebuildSlotInner` so a future change to the metrics
    // surface (e.g. multi-recorder) doesn't have to update the
    // rebuild plumbing.
    let metrics = heron_metrics::init_prometheus_recorder()
        .map_err(|e| RebuildError::Metrics(e.to_string()))?;
    let app_state = AppState {
        orchestrator,
        auth: Arc::clone(&state.auth),
        metrics,
    };
    let app_router = build_app(app_state);
    let join_handle = tauri::async_runtime::spawn(async move {
        let server = axum::serve(listener, app_router).with_graceful_shutdown(async move {
            let _ = shutdown_rx.await;
        });
        if let Err(e) = server.await {
            tracing::error!(error = %e, "rebuild: in-process herond axum::serve exited with error");
        } else {
            tracing::debug!("rebuild: in-process herond axum::serve exited cleanly");
        }
    });
    state.replace_for_rebuild(shutdown_tx, join_handle);
    tracing::info!(
        bind = DEFAULT_BIND,
        "rebuild: in-process herond rebound onto fresh orchestrator (issue #206)",
    );
    Ok(())
}

/// Issue #206: how long [`shutdown_for_rebuild`] waits for an
/// aborted axum task to release its TCP listener before letting the
/// caller move on to bind. 200 ms is enough for the kernel to drop
/// the LISTENING socket after the task panics out of its `select!`;
/// `bind` with `SO_REUSEADDR` succeeds immediately afterwards. A
/// pathological task that ignores cancellation past this budget
/// surfaces as `RebuildError::Bind` (EADDRINUSE) on the next step,
/// which the renderer toasts.
const REBUILD_ABORT_TIMEOUT: Duration = Duration::from_millis(200);

/// Issue #206: bound on how long [`shutdown_for_rebuild`] waits for
/// the previous axum task to drain in-flight requests before
/// returning. 5 s is
/// generous for the daemon's protected routes (none of them block
/// on long I/O — the longest is `heron_meeting_audio` which streams
/// the body chunked but completes the response headers fast) and
/// cheap enough that a user clicking Save and getting a wedged
/// request still sees their setting take effect.
const REBUILD_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

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
            metrics: heron_metrics::init_prometheus_recorder()
                .expect("install Prometheus recorder for test state"),
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
    async fn probe_url_honors_timeout_against_silent_server() {
        // Deterministic timeout test: bind a TCP listener that
        // accepts connections but never reads or responds. The
        // probe's `client.timeout` must fire — no environment
        // sensitivity (the previous TEST-NET-1 / blackhole-IP
        // approach was non-deterministic in sandboxed CI, where
        // the unroutable address fails immediately on connect
        // rather than exercising the read-side timeout).
        //
        // We accept the connection so the connect-side completes
        // and the test exercises the response-wait timeout, which
        // is the path a wedged real daemon would take.
        use tokio::time::Instant;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind silent");
        let addr = listener.local_addr().expect("local_addr");
        let (close_tx, mut close_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            // Accept one connection and hold it open until the test
            // finishes. Dropping the stream at end of select closes
            // the connection — by then the probe has already
            // observed its timeout.
            tokio::select! {
                accepted = listener.accept() => {
                    if let Ok((stream, _)) = accepted {
                        // Hold the stream until close_rx fires.
                        let _stream = stream;
                        let _ = (&mut close_rx).await;
                    }
                }
                _ = &mut close_rx => {}
            }
        });

        let url = format!("http://{addr}/v1/health");
        let start = Instant::now();
        let status = probe_url(&url).await;
        let elapsed = start.elapsed();
        let _ = close_tx.send(()); // release the silent server

        assert!(
            !status.running,
            "expected running=false against silent server, got {status:?}",
        );
        // 1 s probe timeout + scheduler jitter; 3 s is generous.
        // The lower bound (>= 800 ms) proves the timeout actually
        // engaged rather than the request returning instantly.
        assert!(
            elapsed >= Duration::from_millis(800),
            "probe returned too early; expected >= 800ms, took {elapsed:?}",
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

    /// Drift guard: [`HEALTH_URL`] must equal
    /// `format!("http://{DEFAULT_BIND}{API_PREFIX}/health")`. If
    /// herond ever changes either constant, this test catches it
    /// before a probe silently starts hitting the wrong path on a
    /// real daemon. We can't compose at const-eval (no
    /// `const_format` dep) so the runtime check stands in.
    #[test]
    fn health_url_matches_herond_constants() {
        let composed = format!("http://{}{}/health", DEFAULT_BIND, herond::API_PREFIX);
        assert_eq!(HEALTH_URL, composed);
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
