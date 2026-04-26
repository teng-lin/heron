//! `heron-orchestrator` — in-process [`SessionOrchestrator`]
//! implementation for the desktop daemon.
//!
//! [`LocalSessionOrchestrator`] is the consolidation point that
//! `architecture.md` and the `heron-session` trait docs keep
//! deferring to. The full v1 wiring (audio capture → speech
//! recognition → vault writes → LLM summary) lands incrementally
//! by replacing the `NotYetImplemented` branches one at a time;
//! what's here today is the **infrastructure substrate** that all of
//! those impls share:
//!
//! - A live [`heron_event::EventBus`] every future publisher writes
//!   to.
//! - An [`heron_event_http::InMemoryReplayCache`] subscribed to the
//!   bus, so [`heron_session::SessionOrchestrator::replay_cache`]
//!   returns a real cache rather than `None`. The `/events` SSE
//!   `Last-Event-ID` resume contract works end-to-end as soon as
//!   any publisher exists.
//! - A background recorder task that pulls envelopes off the bus and
//!   pushes them into the cache. Lifecycle is governed by an
//!   explicit `oneshot` shutdown signal — Drop fires it best-effort,
//!   and [`LocalSessionOrchestrator::shutdown`] fires-and-joins for
//!   the deterministic-teardown path. The signal is needed because
//!   [`heron_session::SessionOrchestrator::event_bus`] hands out
//!   cheap clones; an external clone keeping the broadcast channel
//!   alive past orchestrator drop would otherwise leak the recorder.
//!   On `RecvError::Lagged` the recorder calls
//!   [`heron_event_http::InMemoryReplayCache::clear`] — a partial
//!   replay that skips a gap with no `WindowExceeded` would silently
//!   violate the spec's resume contract.
//!
//! Per the v2 trait sketches in `heron-session`, the
//! `SessionOrchestrator` is the only handle non-bus consumers (the
//! HTTP daemon, the Tauri frontend, future MCP) hold. Swapping the
//! stub for `LocalSessionOrchestrator` in `herond`'s `AppState` is
//! the cutover; routes don't change.
//!
//! What's NOT here:
//!
//! - **No real FSM.** `start_capture`, `end_meeting`, transcript /
//!   summary / audio reads, calendar reads, context attach all
//!   return [`heron_session::SessionError::NotYetImplemented`]
//!   exactly like the test stub. They land one PR at a time as the
//!   underlying subsystems (heron-cli's session FSM, heron-zoom's
//!   AXObserver, heron-vault, heron-llm) get wrapped.
//! - **No persistent state.** The cache is in-memory and the bus is
//!   a Tokio broadcast channel. A daemon restart loses both — the
//!   spec's `Last-Event-ID` resume contract honors this by
//!   returning `WindowExceeded` on cross-restart resumes (the
//!   client reconnects fresh).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use heron_event::{EventBus, ReplayCache};
use heron_event_http::{DEFAULT_REPLAY_WINDOW, InMemoryReplayCache};
use heron_session::{
    CalendarEvent, ComponentState, EventPayload, Health, HealthComponent, HealthComponents,
    HealthStatus, ListMeetingsPage, ListMeetingsQuery, Meeting, MeetingId,
    PreMeetingContextRequest, SessionError, SessionEventBus, SessionOrchestrator, StartCaptureArgs,
    Summary, Transcript,
};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// Default broadcast bus capacity. 1024 covers a long meeting's
/// worth of `transcript.partial` deltas without dropping for any
/// realistic subscriber count. Override via [`Builder`] when load
/// profiles change.
pub const DEFAULT_BUS_CAPACITY: usize = 1024;

/// Default replay cache capacity. Sized larger than the bus
/// (4× headroom) so a brief recorder-task lag doesn't produce gaps
/// in the cache the moment it catches up — the cache evicts FIFO,
/// and we'd rather it evict by time-window than by capacity pressure.
/// Note: the headroom only helps; on actual `Lagged` the recorder
/// calls [`InMemoryReplayCache::clear`] to make every prior
/// `replay_since` collapse to `WindowExceeded` (that's the only
/// honest answer once the cache has a hole).
pub const DEFAULT_CACHE_CAPACITY: usize = 4096;

/// In-process orchestrator. Owns one shared bus + replay cache for
/// the lifetime of the daemon.
///
/// On drop, signals the recorder task to stop and lets it exit
/// cooperatively (`Drop` can't `await`, so the actual join happens
/// when the task next polls). Callers that need deterministic
/// shutdown — tests asserting the recorder exited, or the desktop
/// shutdown path — should call [`Self::shutdown`] explicitly and
/// `await` it.
pub struct LocalSessionOrchestrator {
    bus: SessionEventBus,
    cache: Arc<InMemoryReplayCache<EventPayload>>,
    /// Held in a `Mutex<Option<…>>` so [`Self::shutdown`] (taking
    /// `&self`) can still consume the sender. Real callers don't
    /// touch the lock; the test seam takes it once.
    shutdown_tx: Mutex<Option<oneshot::Sender<()>>>,
    /// Same `Mutex<Option<…>>` rationale: lets `shutdown` move out
    /// of the join handle without `&mut self`.
    recorder: Mutex<Option<JoinHandle<()>>>,
}

/// Builder for [`LocalSessionOrchestrator`] — exposed so the daemon
/// (or tests) can tune capacities + retention without growing a
/// constructor surface that pins every dial as positional args.
#[derive(Debug, Clone)]
pub struct Builder {
    bus_capacity: usize,
    cache_capacity: usize,
    cache_window: Duration,
}

impl Default for Builder {
    fn default() -> Self {
        Self {
            bus_capacity: DEFAULT_BUS_CAPACITY,
            cache_capacity: DEFAULT_CACHE_CAPACITY,
            cache_window: DEFAULT_REPLAY_WINDOW,
        }
    }
}

impl Builder {
    /// Override the broadcast bus capacity. Must be > 0
    /// (see [`heron_event::EventBus::new`]).
    pub fn bus_capacity(mut self, capacity: usize) -> Self {
        self.bus_capacity = capacity;
        self
    }

    /// Override the replay cache capacity. Must be > 0
    /// (see [`heron_event_http::InMemoryReplayCache::new`]).
    pub fn cache_capacity(mut self, capacity: usize) -> Self {
        self.cache_capacity = capacity;
        self
    }

    /// Override the replay cache retention window. Surfaced via
    /// `ReplayCache::window` and copied into the
    /// `X-Heron-Replay-Window-Seconds` header by the SSE projection.
    /// Default is 3600s (matches the OpenAPI doc); call this when
    /// running with a different `?since_event_id` budget.
    pub fn cache_window(mut self, window: Duration) -> Self {
        self.cache_window = window;
        self
    }

    /// Construct the orchestrator and spawn its recorder task.
    ///
    /// # Panics
    ///
    /// Panics with a heron-specific message if called outside a
    /// Tokio runtime context. The recorder is `tokio::spawn`-ed;
    /// without a runtime there's nothing to spawn onto. The
    /// daemon's `#[tokio::main]` (or any `#[tokio::test]`)
    /// satisfies this. If you're hitting this panic from a sync
    /// `#[test]`, switch to `#[tokio::test]`.
    pub fn build(self) -> LocalSessionOrchestrator {
        // Cheap up-front check so the failure mode points at *us*,
        // not into Tokio's `spawn` macro. The downstream `tokio::spawn`
        // would panic too, but with a generic "no reactor running"
        // message that doesn't tell the caller which library required
        // the runtime.
        assert!(
            tokio::runtime::Handle::try_current().is_ok(),
            "LocalSessionOrchestrator::build must be called from a Tokio \
             runtime; wrap your entry point in #[tokio::main] or invoke \
             via Runtime::block_on",
        );
        let bus: SessionEventBus = EventBus::new(self.bus_capacity);
        let cache = Arc::new(InMemoryReplayCache::with_window(
            self.cache_capacity,
            self.cache_window,
        ));
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let recorder = spawn_recorder(&bus, Arc::clone(&cache), shutdown_rx);
        LocalSessionOrchestrator {
            bus,
            cache,
            shutdown_tx: Mutex::new(Some(shutdown_tx)),
            recorder: Mutex::new(Some(recorder)),
        }
    }
}

impl LocalSessionOrchestrator {
    /// Construct with default capacities. Equivalent to
    /// `Builder::default().build()`. Same Tokio-runtime requirement
    /// as [`Builder::build`].
    //
    // Deliberately no `Default` impl — `Default::default()` is
    // conventionally infallible, and `new()` panics outside a Tokio
    // runtime. Construct via `new()` or `Builder::default().build()`.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Builder::default().build()
    }

    /// Number of envelopes currently in the replay cache. Diagnostic
    /// only — production callers route through
    /// [`SessionOrchestrator::replay_cache`]. Tests use this to
    /// synchronize with the recorder task without polling
    /// `replay_since`.
    pub fn cache_len(&self) -> usize {
        self.cache.len()
    }

    /// Signal the recorder task to exit and await its termination.
    /// Idempotent — repeated calls return `Ok(())` immediately
    /// after the first (the join handle is consumed). Use this in
    /// the daemon's graceful-shutdown path; otherwise [`Drop`]
    /// fires the same signal but can't `await` the task.
    ///
    /// Returns the task's `JoinError` if it panicked; success
    /// otherwise.
    pub async fn shutdown(&self) -> Result<(), tokio::task::JoinError> {
        // Send the signal under the lock — the recorder selects on
        // `shutdown_rx` and the live bus, so a dropped sender
        // unblocks it whether or not the bus is closed.
        if let Some(tx) = lock_or_recover(&self.shutdown_tx).take() {
            // Recorder may already be gone; that's fine.
            let _ = tx.send(());
        }
        let handle = lock_or_recover(&self.recorder).take();
        if let Some(h) = handle {
            h.await?;
        }
        Ok(())
    }
}

impl Drop for LocalSessionOrchestrator {
    fn drop(&mut self) {
        // Best-effort: send the shutdown signal so the task exits at
        // its next poll. Can't `await` here, so we don't block on
        // join — callers that need deterministic teardown call
        // `shutdown().await` explicitly. External `event_bus()`
        // clones holding a `Sender` will keep the channel alive,
        // but the shutdown signal still ends the recorder regardless.
        if let Some(tx) = lock_or_recover(&self.shutdown_tx).take() {
            let _ = tx.send(());
        }
    }
}

/// Acquire the mutex, recovering the inner data on poisoning. We
/// only ever hold the lock briefly to take the `Option`'s value;
/// poisoning here would mean another thread panicked between `take`
/// calls, which is benign since we're just consuming an option.
fn lock_or_recover<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Spawn the bus → cache recorder. Returns the `JoinHandle` so the
/// orchestrator can `await` clean shutdown. The task selects on
/// (a) the bus subscription, (b) the explicit shutdown signal —
/// whichever fires first wins. On `Lagged` it calls
/// [`InMemoryReplayCache::clear`] to enforce the discontinuity-
/// recovery contract: a partial replay would silently hand a client
/// events that skip the gap, so the only honest answer is to make
/// every subsequent `replay_since` `WindowExceeded`.
fn spawn_recorder(
    bus: &SessionEventBus,
    cache: Arc<InMemoryReplayCache<EventPayload>>,
    shutdown_rx: oneshot::Receiver<()>,
) -> JoinHandle<()> {
    let mut rx = bus.subscribe();
    tokio::spawn(async move {
        tracing::debug!("replay-cache recorder started");
        let mut shutdown_rx = shutdown_rx;
        loop {
            tokio::select! {
                // Biased select would prioritize shutdown; we don't
                // need it because the channel is one-shot and the
                // bus recv is cancel-safe — either branch ending the
                // loop is fine, and ordering between a near-
                // simultaneous shutdown + final event doesn't matter
                // (the next consumer reconnects fresh anyway).
                _ = &mut shutdown_rx => {
                    tracing::debug!("replay-cache recorder shutdown signaled");
                    return;
                }
                msg = rx.recv() => {
                    match msg {
                        Ok(envelope) => cache.record(envelope),
                        Err(RecvError::Lagged(skipped)) => {
                            tracing::warn!(
                                skipped,
                                "replay-cache recorder lagged the bus; \
                                 clearing cache to enforce WindowExceeded \
                                 on every prior resume id",
                            );
                            cache.clear();
                        }
                        Err(RecvError::Closed) => {
                            // All Senders dropped — bus has no future
                            // publishers. Exit cleanly.
                            tracing::debug!(
                                "replay-cache recorder exiting (bus closed)",
                            );
                            return;
                        }
                    }
                }
            }
        }
    })
}

/// `Down` plus a "not yet wired" message — the honest answer for a
/// substrate-only orchestrator. Deliberately not `PermissionMissing`
/// (which would suggest a TCC permission gap and route consumers
/// down a debugging dead end). When a subsystem actually wires in,
/// its branch flips to a real probe; until then this is the cleanest
/// signal that the daemon is up but the subsystem is not.
fn not_yet_wired(subsystem: &str) -> HealthComponent {
    HealthComponent {
        state: ComponentState::Down,
        message: Some(format!(
            "{subsystem} not yet wired into LocalSessionOrchestrator",
        )),
        last_check: None,
    }
}

#[async_trait]
impl SessionOrchestrator for LocalSessionOrchestrator {
    // ── data / FSM methods: NotYetImplemented until each
    // subsystem is wired in its own follow-up PR. The trait surface
    // is the contract; replacing one method at a time is safe
    // because the rest still answer the same way.

    async fn list_meetings(&self, _q: ListMeetingsQuery) -> Result<ListMeetingsPage, SessionError> {
        Err(SessionError::NotYetImplemented)
    }

    async fn get_meeting(&self, _id: &MeetingId) -> Result<Meeting, SessionError> {
        Err(SessionError::NotYetImplemented)
    }

    async fn start_capture(&self, _args: StartCaptureArgs) -> Result<Meeting, SessionError> {
        Err(SessionError::NotYetImplemented)
    }

    async fn end_meeting(&self, _id: &MeetingId) -> Result<(), SessionError> {
        Err(SessionError::NotYetImplemented)
    }

    async fn read_transcript(&self, _id: &MeetingId) -> Result<Transcript, SessionError> {
        Err(SessionError::NotYetImplemented)
    }

    async fn read_summary(&self, _id: &MeetingId) -> Result<Option<Summary>, SessionError> {
        Err(SessionError::NotYetImplemented)
    }

    async fn audio_path(&self, _id: &MeetingId) -> Result<PathBuf, SessionError> {
        Err(SessionError::NotYetImplemented)
    }

    async fn list_upcoming_calendar(
        &self,
        _from: Option<DateTime<Utc>>,
        _to: Option<DateTime<Utc>>,
        _limit: Option<u32>,
    ) -> Result<Vec<CalendarEvent>, SessionError> {
        Err(SessionError::NotYetImplemented)
    }

    async fn attach_context(&self, _req: PreMeetingContextRequest) -> Result<(), SessionError> {
        Err(SessionError::NotYetImplemented)
    }

    async fn health(&self) -> Health {
        // Degraded with every component reporting `Down` plus a
        // "not yet wired" message. Deliberately not
        // `PermissionMissing` (the test-stub's choice) — that state
        // means a TCC permission gap, and routing `/health` consumers
        // to the System Settings → Privacy debugging path for an
        // unimplemented subsystem is misleading. `Down` honestly
        // says "this subsystem is unavailable"; the per-component
        // message tells operators it's the implementation that's
        // missing, not a permission. When a subsystem wires in, its
        // branch flips to a real probe; everything else keeps
        // reporting honestly.
        Health {
            status: HealthStatus::Degraded,
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            components: HealthComponents {
                capture: not_yet_wired("audio capture"),
                whisperkit: not_yet_wired("speech recognition"),
                vault: not_yet_wired("vault writer"),
                eventkit: not_yet_wired("EventKit calendar reads"),
                llm: not_yet_wired("LLM summarizer"),
            },
        }
    }

    fn event_bus(&self) -> SessionEventBus {
        // Cheap clone — the bus is `Arc`-backed inside.
        self.bus.clone()
    }

    fn replay_cache(&self) -> Option<&dyn ReplayCache<EventPayload>> {
        Some(&*self.cache)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    //! Pin the substrate behaviours that herond / future Tauri-side
    //! consumers rely on:
    //! - the bus is live and clone-shareable,
    //! - published envelopes are recorded into the cache (recorder
    //!   task is alive and forwarding),
    //! - the cache surfaced via `replay_cache()` is the same one the
    //!   recorder writes to.
    //!
    //! Use the typed `EventPayload` so we exercise the same envelope
    //! shape herond and the SSE projection see end-to-end.

    use super::*;
    use heron_event::Envelope;
    use heron_session::{Meeting, MeetingStatus, Platform, SummaryLifecycle, TranscriptLifecycle};
    use std::time::{Duration, Instant};

    fn sample_envelope() -> Envelope<EventPayload> {
        let meeting = Meeting {
            id: MeetingId::now_v7(),
            status: MeetingStatus::Detected,
            platform: Platform::Zoom,
            title: Some("Standup".into()),
            calendar_event_id: None,
            started_at: Utc::now(),
            ended_at: None,
            duration_secs: None,
            participants: vec![],
            transcript_status: TranscriptLifecycle::Pending,
            summary_status: SummaryLifecycle::Pending,
        };
        let id = meeting.id;
        Envelope::new(EventPayload::MeetingDetected(meeting)).with_meeting(id.to_string())
    }

    /// Poll until the recorder has caught up to `expected` cache
    /// entries, panicking with a clear message if it never does. The
    /// recorder runs on the same Tokio runtime as the test; under
    /// normal load this returns within a microsecond, so the
    /// generous 2s budget is just a hedge against scheduler jitter.
    async fn wait_for_cache_len(orch: &LocalSessionOrchestrator, expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while orch.cache_len() < expected {
            if Instant::now() > deadline {
                panic!(
                    "recorder never reached {expected} entries (cur={})",
                    orch.cache_len(),
                );
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    #[tokio::test]
    async fn published_envelopes_land_in_replay_cache() {
        let orch = LocalSessionOrchestrator::new();
        let bus = orch.event_bus();
        let env = sample_envelope();
        let id = env.event_id;

        bus.publish(env);
        wait_for_cache_len(&orch, 1).await;
        assert_eq!(orch.cache_len(), 1);

        // The cache the trait surfaces is the one the recorder wrote
        // into — confirm by replaying from a synthetic earlier id and
        // expecting a `WindowExceeded` (since `id` is the only entry,
        // any other since-marker is "not in cache").
        let cache = orch.replay_cache().expect("cache present");
        let result = cache.replay_since(heron_event::EventId::now_v7()).await;
        assert!(
            matches!(result, Err(heron_event::ReplayError::WindowExceeded { .. })),
            "unknown since should be WindowExceeded, got {result:?}",
        );
        // Replaying from `id` itself (the only entry) returns Ok(empty)
        // — caller is caught up.
        let from_self = cache.replay_since(id).await.expect("ok");
        assert!(from_self.is_empty(), "since=newest should be caught up");
    }

    #[tokio::test]
    async fn replay_returns_events_strictly_after_resume_marker() {
        // Two envelopes; resume from the first and expect the second.
        let orch = LocalSessionOrchestrator::new();
        let bus = orch.event_bus();
        let env1 = sample_envelope();
        let env2 = sample_envelope();
        let id1 = env1.event_id;
        let id2 = env2.event_id;

        bus.publish(env1);
        bus.publish(env2);
        wait_for_cache_len(&orch, 2).await;

        let cache = orch.replay_cache().expect("cache");
        let replay = cache.replay_since(id1).await.expect("ok");
        assert_eq!(replay.len(), 1, "expected exactly the second envelope");
        assert_eq!(replay[0].event_id, id2);
    }

    #[tokio::test]
    async fn every_fsm_method_returns_not_yet_implemented() {
        // Pin the "stub for now" contract per-method so a future
        // accidental wiring (e.g. someone returning Ok(empty page)
        // from list_meetings without implementing the underlying
        // store) breaks loudly here. All 9 methods covered — codex
        // review flagged the partial coverage as a regression hole.
        let orch = LocalSessionOrchestrator::new();
        let id = MeetingId::now_v7();

        assert!(matches!(
            orch.list_meetings(ListMeetingsQuery::default()).await,
            Err(SessionError::NotYetImplemented)
        ));
        assert!(matches!(
            orch.get_meeting(&id).await,
            Err(SessionError::NotYetImplemented)
        ));
        assert!(matches!(
            orch.start_capture(StartCaptureArgs {
                platform: Platform::Zoom,
                hint: None,
            })
            .await,
            Err(SessionError::NotYetImplemented)
        ));
        assert!(matches!(
            orch.end_meeting(&id).await,
            Err(SessionError::NotYetImplemented)
        ));
        assert!(matches!(
            orch.read_transcript(&id).await,
            Err(SessionError::NotYetImplemented)
        ));
        assert!(matches!(
            orch.read_summary(&id).await,
            Err(SessionError::NotYetImplemented)
        ));
        assert!(matches!(
            orch.audio_path(&id).await,
            Err(SessionError::NotYetImplemented)
        ));
        assert!(matches!(
            orch.list_upcoming_calendar(None, None, None).await,
            Err(SessionError::NotYetImplemented)
        ));
        assert!(matches!(
            orch.attach_context(PreMeetingContextRequest {
                calendar_event_id: "evt_x".into(),
                context: Default::default(),
            })
            .await,
            Err(SessionError::NotYetImplemented)
        ));
    }

    #[tokio::test]
    async fn health_reports_degraded_with_down_components() {
        // Pin the "Down + reason" contract per-component. Reviewers
        // flagged that `PermissionMissing` would mislead `/health`
        // consumers into thinking a TCC permission is missing —
        // `Down` is the honest state for "subsystem not yet wired".
        let orch = LocalSessionOrchestrator::new();
        let h = orch.health().await;
        assert!(matches!(h.status, HealthStatus::Degraded));
        for component in [
            &h.components.capture,
            &h.components.whisperkit,
            &h.components.vault,
            &h.components.eventkit,
            &h.components.llm,
        ] {
            assert!(
                matches!(component.state, ComponentState::Down),
                "expected Down for not-yet-wired subsystem, got {:?}",
                component.state,
            );
            let msg = component.message.as_deref().unwrap_or_default();
            assert!(
                msg.contains("not yet wired"),
                "expected 'not yet wired' in message, got {msg:?}",
            );
        }
    }

    #[tokio::test]
    async fn builder_overrides_capacities() {
        // Smoke test the dial: a tiny cache shouldn't break the
        // recorder loop, just evict aggressively. Three publishes
        // into a 2-entry cache leaves only the newest two.
        let orch = Builder::default().bus_capacity(8).cache_capacity(2).build();
        let bus = orch.event_bus();
        for _ in 0..3 {
            bus.publish(sample_envelope());
        }
        // Wait for the recorder to drain the publishes; then assert
        // the cache evicted to its capacity.
        let deadline = Instant::now() + Duration::from_secs(2);
        while orch.cache_len() < 2 {
            if Instant::now() > deadline {
                panic!("recorder never reached cap (cur={})", orch.cache_len());
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        // After three publishes into capacity-2, the cache stabilises
        // at 2 (FIFO eviction). Give the recorder a moment to absorb
        // any lingering item before re-asserting.
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert_eq!(orch.cache_len(), 2, "FIFO eviction should cap at 2");
    }

    #[tokio::test]
    async fn cache_window_builder_threads_through_to_replay_cache() {
        // The retention window dial flows from `Builder::cache_window`
        // through `with_window` into the cache, which the SSE layer
        // copies into `X-Heron-Replay-Window-Seconds`. Pin the path.
        let orch = Builder::default()
            .cache_window(Duration::from_secs(120))
            .build();
        let cache = orch.replay_cache().expect("cache present");
        assert_eq!(cache.window(), Duration::from_secs(120));
    }

    #[tokio::test]
    async fn shutdown_terminates_recorder_task() {
        // `shutdown()` joins the recorder so callers can rely on
        // "after this returns, the task is gone." External
        // `event_bus()` clones holding a Sender would otherwise keep
        // the broadcast channel alive past orchestrator drop — the
        // explicit signal forces an exit regardless.
        let orch = LocalSessionOrchestrator::new();
        // Hold an external bus clone so the only thing ending the
        // recorder is the shutdown signal, not channel closure.
        let _external_bus = orch.event_bus();
        orch.shutdown().await.expect("recorder joined");
        // Idempotency: a second call is a no-op.
        orch.shutdown().await.expect("idempotent shutdown");
    }

    #[tokio::test]
    async fn drop_signals_recorder_to_exit() {
        // Drop fires the same signal as `shutdown()` — the task
        // exits at its next poll. Without an `await`-able join we
        // probe via the cache: after drop, no further publishes
        // can land in the cache (the orchestrator's bus/cache are
        // gone). This is more about confirming Drop doesn't leak
        // than asserting timing.
        {
            let orch = LocalSessionOrchestrator::new();
            let bus = orch.event_bus();
            bus.publish(sample_envelope());
            wait_for_cache_len(&orch, 1).await;
            // orch dropped here; bus clone goes too at end of block.
        }
        // If Drop didn't deadlock the runtime or panic, the
        // contract holds. (A leaked task isn't observable from a
        // test without `tracing-test` + log inspection — adding
        // that dep is out of scope for this PR.)
    }

    #[tokio::test]
    async fn lagged_recorder_clears_cache_to_enforce_window_exceeded() {
        // The CRITICAL fix from review: on `RecvError::Lagged`, the
        // recorder calls `cache.clear()` so any prior `replay_since`
        // returns `WindowExceeded`. Without this, a partial replay
        // would silently hand a client events that skip the gap.
        //
        // Force lag by oversaturating a tiny bus before the recorder
        // gets to run. The broadcast channel's `Lagged` error fires
        // when the recv lag exceeds capacity — capacity=2 with 50
        // synchronous publishes guarantees lag.
        let orch = Builder::default()
            .bus_capacity(2)
            .cache_capacity(64)
            .build();
        let bus = orch.event_bus();

        // Record one envelope first so the cache has a known entry,
        // wait for it to land, then deliberately overrun the bus to
        // trigger the lagged path on the next recv.
        let pre = sample_envelope();
        let pre_id = pre.event_id;
        bus.publish(pre);
        wait_for_cache_len(&orch, 1).await;

        // Now overrun. Publishing N >> capacity in tight succession
        // means the broadcast ring overwrites entries before the
        // recorder polls them. The recorder's next `recv()` returns
        // `Lagged(skipped)`, triggers `cache.clear()`, then resumes
        // recording the events still in the ring — so the cache may
        // re-fill after the clear. Test-stable target: poll on
        // `replay_since(pre_id)` (the regression-guard assertion
        // itself), not on `cache_len`, since len oscillates while the
        // recorder drains the post-clear residual.
        for _ in 0..50 {
            bus.publish(sample_envelope());
        }
        let cache = orch.replay_cache().expect("cache");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match cache.replay_since(pre_id).await {
                Err(heron_event::ReplayError::WindowExceeded { .. }) => break,
                _ if Instant::now() >= deadline => panic!(
                    "post-lag replay never collapsed to WindowExceeded; \
                     pre_id was still findable in cache after 2s",
                ),
                _ => tokio::time::sleep(Duration::from_millis(1)).await,
            }
        }
    }
}
