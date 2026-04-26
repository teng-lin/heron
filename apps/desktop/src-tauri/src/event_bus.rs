//! In-process event bus + Tauri IPC fan-out.
//!
//! Owns a [`LocalSessionOrchestrator`] for the lifetime of the
//! Tauri app and pipes its [`SessionEventBus`] into a
//! [`TauriEventSink`]. Today the wiring is **dormant** — there are
//! no in-process publishers in the desktop app yet (every domain
//! event flows over herond's HTTP/SSE per `docs/codebase-gaps.md`
//! gap #11). The slot exists so that when an in-process publisher
//! lands (e.g. a future `heron-cli` v2 command path that ran
//! locally instead of round-tripping through herond, or an
//! ambient-detection signal), it has a bus to publish to and the
//! WebView immediately sees the events as Tauri IPC events.
//!
//! ## Wire shape
//!
//! Each envelope becomes one Tauri event whose name is the
//! envelope's `event_type` discriminator, with `.` mapped to `:`
//! per [`heron_event_tauri::TauriEventSink`]. Frontend listeners:
//!
//! ```ts
//! import { listen } from "@tauri-apps/api/event";
//! await listen("meeting:detected", (e) => console.log(e.payload));
//! ```
//!
//! ## Lifecycle
//!
//! - The orchestrator is constructed in [`install`] (called from
//!   `lib::run`'s `.setup()` closure). Construction needs a Tokio
//!   thread-local because [`LocalSessionOrchestrator::new`] internally
//!   `tokio::spawn`s its recorder task; the setup hook runs on
//!   Tauri's main thread *without* that thread-local set, so we
//!   wrap construction in [`tauri::async_runtime::block_on`] which
//!   enters the Tauri-managed runtime for the duration. Same reason
//!   the forwarder uses [`tauri::async_runtime::spawn`] rather than
//!   bare `tokio::spawn` (matches the established `tray::install`
//!   pattern in this crate).
//! - The forwarder task exits on `RecvError::Closed` — i.e. when
//!   every `broadcast::Sender` clone (the managed orchestrator
//!   plus any future external `event_bus()` clones) is dropped.
//!   At process exit the Tauri runtime aborts the task whether
//!   or not the bus has closed; once an explicit app-teardown
//!   hook lands, switching to a oneshot shutdown signal here will
//!   make teardown deterministic. Today's reliance on
//!   abort-at-exit is acceptable for the dormant phase but worth
//!   revisiting once a real publisher exists.
//! - The orchestrator is `Arc`-wrapped and stored via
//!   [`tauri::Manager::manage`] so command handlers can reach it
//!   for a publish via `State<Arc<LocalSessionOrchestrator>>`.
//!
//! ## Replay semantics
//!
//! The orchestrator's [`heron_event_http::InMemoryReplayCache`] is
//! local to *this* desktop process and is **not** served as SSE
//! anywhere — herond runs as a separate process with its own
//! orchestrator + cache. A WebView reload, a late `listen()`, or a
//! frame dropped because the forwarder errored on
//! [`EventSink::forward`] is therefore **lost permanently** on the
//! Tauri path. SSE consumers (browser, CLI talking to herond) have
//! `Last-Event-ID` resume; the WebView does not. Document this
//! mismatch explicitly so a future in-process publisher doesn't
//! assume the cache backstops Tauri delivery.

use std::sync::Arc;

use heron_event::EventSink;
use heron_event_tauri::TauriEventSink;
use heron_orchestrator::LocalSessionOrchestrator;
// `SessionOrchestrator` is the trait that provides `event_bus()` —
// brought into scope here so the call on `Arc<LocalSessionOrchestrator>`
// resolves through the trait method.
use heron_session::SessionOrchestrator;
use tauri::{AppHandle, Manager, Runtime};
use thiserror::Error;
use tokio::sync::broadcast::error::RecvError;

/// Failure modes for [`install`]. Modeled as an enum so callers can
/// `match` on the variant rather than parsing a string; `thiserror`
/// gives us a `Display` impl for the setup-hook error path that
/// boxes us as `dyn std::error::Error`.
#[derive(Debug, Error)]
pub enum InstallError {
    /// `install` was called more than once on the same `AppHandle`.
    /// Tauri's setup hook fires once per app, so a duplicate is a
    /// programming bug rather than a recoverable runtime condition.
    #[error("event_bus::install called twice on the same AppHandle")]
    AlreadyInstalled,
}

/// Opaque label the [`TauriEventSink`] reports for diagnostics. We
/// don't peer-multiplex (the Tauri runtime fans out to every webview
/// itself), so a single static label is sufficient.
const SINK_LABEL: &str = "tauri-ipc:desktop";

/// Install the in-process bus into the Tauri app.
///
/// Constructs a fresh [`LocalSessionOrchestrator`] (with default
/// capacities, no vault root), stores it as a managed state via
/// [`tauri::Manager::manage`] (so Tauri commands can grab
/// `State<Arc<LocalSessionOrchestrator>>`), and spawns a forwarder
/// task that pumps every envelope from the orchestrator's bus into
/// a [`TauriEventSink`].
///
/// **Production callers** (the desktop's `lib::run` setup hook)
/// should call [`install_with`] instead and supply the same
/// orchestrator the in-process daemon (`daemon::install`) is using
/// — that way an in-process publisher fans out across **both**
/// transports (HTTP/SSE via the daemon, Tauri IPC via this sink)
/// off one bus. This zero-arg `install` exists for back-compat with
/// the original phase 82 wiring and for tests that don't care about
/// the daemon.
///
/// # Errors
///
/// Returns [`InstallError::AlreadyInstalled`] if the
/// [`LocalSessionOrchestrator`] is already managed (calling `install`
/// twice). The Tauri `setup` hook fires once per app, so a duplicate
/// is a programming error. We detect it via
/// [`tauri::Manager::manage`]'s return value — `false` means the
/// type was already in the state map, atomically.
pub fn install<R: Runtime>(app: &AppHandle<R>) -> Result<(), InstallError> {
    // Construct the orchestrator inside whatever runtime context is
    // available. `LocalSessionOrchestrator::new` calls
    // `tokio::spawn` internally (recorder task) so it requires a
    // Tokio thread-local. Two cases:
    // - Production: setup hook runs on Tauri's main thread without
    //   a Tokio thread-local, so we enter the Tauri-managed runtime
    //   via `block_on`.
    // - `#[tokio::test]`: the test already provides a runtime, and
    //   nested `block_on` panics with "Cannot start a runtime from
    //   within a runtime." Construct directly.
    let orchestrator: Arc<LocalSessionOrchestrator> =
        if tokio::runtime::Handle::try_current().is_ok() {
            Arc::new(LocalSessionOrchestrator::new())
        } else {
            tauri::async_runtime::block_on(async { Arc::new(LocalSessionOrchestrator::new()) })
        };
    install_with(app, orchestrator)
}

/// Install the in-process bus, reusing a caller-supplied
/// orchestrator. This is the entry point production code uses so the
/// `daemon::install` axum service and the [`TauriEventSink`]
/// forwarder share **one** bus.
///
/// # Errors
///
/// Same as [`install`]: [`InstallError::AlreadyInstalled`] if a
/// `LocalSessionOrchestrator` is already in the state map.
pub fn install_with<R: Runtime>(
    app: &AppHandle<R>,
    orchestrator: Arc<LocalSessionOrchestrator>,
) -> Result<(), InstallError> {
    // Atomically take the state slot. `manage` returns `false` when
    // a value of this type is already managed — mirrors the TOCTOU-
    // free "set if absent" idiom without needing a separate guard
    // around `try_state`.
    if !app.manage(Arc::clone(&orchestrator)) {
        return Err(InstallError::AlreadyInstalled);
    }

    let mut rx = orchestrator.event_bus().subscribe();
    let sink = TauriEventSink::new(SINK_LABEL, app.clone());

    // `tauri::async_runtime::spawn` uses Tauri's globally-managed
    // Tokio runtime — no caller-side thread-local required. Matches
    // the pattern `tray::install` uses (see `tray.rs:259, 298`).
    tauri::async_runtime::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(envelope) => {
                    if let Err(err) = EventSink::forward(&sink, &envelope).await {
                        // Log-and-continue: a broken sink for one
                        // event shouldn't tear down the forwarder.
                        // The frame is **lost on the WebView path**
                        // — see crate docs §"Replay semantics" —
                        // because the in-process cache here isn't
                        // served back over Tauri IPC.
                        tracing::warn!(
                            %err,
                            sink = SINK_LABEL,
                            "tauri-event forwarder failed; frame lost on WebView path",
                        );
                    }
                }
                Err(RecvError::Lagged(skipped)) => {
                    // The forwarder doesn't own the cache; gaps
                    // surface to listeners via `event_id`
                    // discontinuity. The orchestrator's recorder
                    // task already clears its own cache on its own
                    // Lagged.
                    tracing::warn!(skipped, "tauri-event forwarder lagged the bus");
                }
                Err(RecvError::Closed) => {
                    tracing::debug!("tauri-event forwarder exiting (bus closed)");
                    return;
                }
            }
        }
    });

    tracing::info!(
        sink = SINK_LABEL,
        "in-process event bus installed; forwarder running"
    );
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
#[allow(clippy::unwrap_used)]
mod tests {
    //! Pin the wiring end-to-end: an envelope published into the
    //! orchestrator's bus arrives at a `tauri::Listener` callback as
    //! the sanitized event name (`meeting.detected` →
    //! `meeting:detected`).

    use super::*;
    use heron_event::Envelope;
    use heron_session::{
        EventPayload, Meeting, MeetingId, MeetingStatus, Platform, SummaryLifecycle,
        TranscriptLifecycle,
    };
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    use tauri::Listener;

    fn sample_envelope() -> Envelope<EventPayload> {
        let meeting = Meeting {
            id: MeetingId::now_v7(),
            status: MeetingStatus::Detected,
            platform: Platform::Zoom,
            title: Some("Standup".into()),
            calendar_event_id: None,
            started_at: chrono::Utc::now(),
            ended_at: None,
            duration_secs: None,
            participants: vec![],
            transcript_status: TranscriptLifecycle::Pending,
            summary_status: SummaryLifecycle::Pending,
        };
        let id = meeting.id;
        Envelope::new(EventPayload::MeetingDetected(meeting)).with_meeting(id.to_string())
    }

    /// Poll until the listener-callback capture has at least one
    /// entry, panicking with `timeout_message` if it never does. The
    /// forwarder runs on the same Tokio runtime as the test; the
    /// generous 2s budget is a hedge against scheduler jitter, not
    /// the expected delay.
    async fn wait_for_capture(captured: &Arc<Mutex<Vec<String>>>, timeout_message: &str) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if !captured.lock().expect("lock").is_empty() {
                return;
            }
            if Instant::now() >= deadline {
                panic!("{timeout_message}");
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    #[tokio::test]
    async fn published_envelope_reaches_tauri_listener() {
        let app = tauri::test::mock_app();
        install(app.handle()).expect("install");

        // Capture every payload that arrives on the sanitized name.
        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);
        app.handle().listen("meeting:detected", move |evt| {
            captured_clone
                .lock()
                .expect("lock")
                .push(evt.payload().to_owned());
        });

        // Publish via the managed orchestrator handle — same path a
        // future in-process publisher would take.
        let orch = app
            .handle()
            .state::<Arc<LocalSessionOrchestrator>>()
            .inner()
            .clone();
        let bus = orch.event_bus();
        bus.publish(sample_envelope());

        wait_for_capture(
            &captured,
            "forwarder never delivered the event to the listener",
        )
        .await;
        let entries = captured.lock().expect("lock");
        assert_eq!(entries.len(), 1);
        assert!(
            entries[0].contains("\"event_type\":\"meeting.detected\""),
            "payload missing inner event_type: {}",
            entries[0],
        );
    }

    #[tokio::test]
    async fn install_twice_returns_err() {
        let app = tauri::test::mock_app();
        install(app.handle()).expect("first install");
        let result = install(app.handle());
        assert!(result.is_err(), "second install should fail");
    }

    #[tokio::test]
    async fn dot_namespaced_event_type_arrives_under_colon_sanitized_name() {
        // End-to-end sanitization: `transcript.partial` becomes the
        // Tauri event name `transcript:partial` per the
        // `heron-event-tauri` mapping. Pin the contract beyond just
        // `meeting.detected` so a future refactor of the sanitizer
        // visibly breaks here too.
        let app = tauri::test::mock_app();
        install(app.handle()).expect("install");

        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);
        app.handle().listen("transcript:partial", move |evt| {
            captured_clone
                .lock()
                .expect("lock")
                .push(evt.payload().to_owned());
        });

        let segment = heron_session::TranscriptSegment {
            speaker: heron_session::Participant {
                display_name: "Alice".into(),
                identifier_kind: heron_session::IdentifierKind::AxTree,
                is_user: false,
            },
            text: "hello world".into(),
            start_secs: 0.0,
            end_secs: 1.0,
            confidence: heron_session::Confidence::High,
            is_final: false,
        };
        let env = Envelope::new(EventPayload::TranscriptPartial(segment));
        let orch = app
            .handle()
            .state::<Arc<LocalSessionOrchestrator>>()
            .inner()
            .clone();
        orch.event_bus().publish(env);

        wait_for_capture(
            &captured,
            "transcript.partial never reached the colon-sanitized listener",
        )
        .await;
    }

    #[tokio::test]
    async fn forwarder_survives_lagged_bus_and_keeps_delivering() {
        // Drive the bus past its capacity to provoke `RecvError::Lagged`
        // in the forwarder. The forwarder must log-and-continue, not
        // exit; a fresh publish after the lag must still reach the
        // listener. Without this, a one-time burst could silently
        // disable the WebView event path until app restart.
        //
        // Tighten the bus via the `Builder` so 50 publishes definitively
        // exceeds capacity. Use a fresh orchestrator (not the managed
        // one) so the test owns the bus and can flood it
        // deterministically.
        let app = tauri::test::mock_app();
        let orch = Arc::new(
            heron_orchestrator::Builder::default()
                .bus_capacity(2)
                .cache_capacity(2)
                .build(),
        );
        let mut rx = orch.event_bus().subscribe();
        let sink = TauriEventSink::new("test-lag", app.handle().clone());

        // Spawn a forwarder with the same shape as install() — but
        // with the locally-controlled orchestrator + a small bus.
        tauri::async_runtime::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(env) => {
                        let _ = EventSink::forward(&sink, &env).await;
                    }
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => return,
                }
            }
        });

        // Listen for the post-lag event under its sanitized name.
        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);
        app.handle().listen("meeting:detected", move |evt| {
            captured_clone
                .lock()
                .expect("lock")
                .push(evt.payload().to_owned());
        });

        // Burst past capacity to provoke Lagged.
        let bus = orch.event_bus();
        for _ in 0..50 {
            bus.publish(sample_envelope());
        }
        // Give the forwarder a moment to drain + log lag, then
        // publish a fresh envelope and confirm it lands.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let post_lag = sample_envelope();
        bus.publish(post_lag);

        wait_for_capture(
            &captured,
            "forwarder didn't recover from Lagged; post-lag publish lost",
        )
        .await;
    }
}
