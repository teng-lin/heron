//! `heron-orchestrator` — in-process [`SessionOrchestrator`]
//! implementation for the desktop daemon.
//!
//! [`LocalSessionOrchestrator`] is the consolidation point that
//! `architecture.md` and the `heron-session` trait docs keep
//! deferring to. It owns the daemon-facing lifecycle, event bus,
//! replay cache, active-meeting index, and read-side vault projection.
//! When configured with a vault root, manual capture delegates to the
//! same audio → STT → LLM → vault pipeline used by `heron record`.
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
//! What's wired today:
//!
//! - **Capture lifecycle FSM.** [`SessionOrchestrator::start_capture`]
//!   and [`SessionOrchestrator::end_meeting`] drive a
//!   [`heron_types::RecordingFsm`] — the same FSM
//!   `heron-pipeline::session::Orchestrator` runs on the live audio
//!   path — and publish `meeting.detected` / `meeting.armed` /
//!   `meeting.started` / `meeting.ended` / `meeting.completed`
//!   envelopes onto the bus on each transition.
//! - **Vault-backed capture pipeline.** When a vault root is present,
//!   `start_capture` spawns the v1 capture pipeline (now in
//!   `heron-pipeline`; `heron-cli` re-exports it) on a dedicated
//!   blocking thread with a current-thread Tokio runtime.
//!   `end_meeting` signals that pipeline to stop, publishes
//!   `meeting.ended`, and returns without holding the HTTP request open
//!   through STT/LLM work. A background waiter publishes
//!   `meeting.completed` after WAV finalization, transcript merge, LLM
//!   summarization, and vault note finalization.
//! - **Daemon ID continuity.** Completed meetings are indexed in
//!   memory by the `MeetingId` returned from `POST /meetings`, so the
//!   `Location` header remains readable after the note is written even
//!   though vault-discovered notes still have path-derived IDs.
//!
//! What's NOT here:
//!
//! - **No v2 bot / realtime composition.** This wires the native v1
//!   capture path into the daemon; it does not yet compose Recall,
//!   `AudioBridge`, speech policy, or a production realtime backend.
//! - **No cross-restart active state.** The cache, active-meeting
//!   bookkeeping, and daemon-ID-to-note-path index are in-memory. A
//!   daemon restart loses in-flight captures and the path-derived vault
//!   IDs become the read-side source of truth.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use heron_event::ReplayCache;
use heron_event_http::InMemoryReplayCache;
use heron_session::{
    AutoRecordList, CalendarEvent, EventPayload, Health, ListMeetingsPage, ListMeetingsQuery,
    Meeting, MeetingId, PreMeetingContext, PreMeetingContextRequest, PrepareContextRequest,
    SessionError, SessionEventBus, SessionOrchestrator, SetEventAutoRecordRequest,
    StartCaptureArgs, Summary, Transcript,
};

use crate::live_session::LiveSessionFactory;
use crate::state::{ActiveMeeting, FinalizedMeeting, PendingContexts};
use heron_vault::{CalendarReader, FileNamingPattern};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

pub mod live_session;

pub(crate) mod auto_record;
mod builder;
mod capture;
mod compose;
mod context;
mod health;
mod metrics_names;
mod pipeline_glue;
mod platform;
mod read_side;
mod state;
mod validation;
mod vault_read;

pub use vault_read::MEETING_ID_NAMESPACE;

/// Cap on a single JSONL transcript line. A turn is a few hundred
/// bytes typically; 1 MiB bounds the OOM blast radius for a
/// malformed transcript that lost its newlines and presents as one
/// gigantic line.
pub(crate) const MAX_TRANSCRIPT_LINE_BYTES: usize = 1024 * 1024;

/// How close to the calendar start time an auto-record event must be
/// before the scheduler fires capture.
const AUTO_RECORD_START_WINDOW: chrono::Duration = chrono::Duration::minutes(5);

/// How long a fired auto-record event stays suppressed. Bounds retry
/// spam when capture is already active or the platform cannot start.
const AUTO_RECORD_DEDUP_TTL: chrono::Duration = chrono::Duration::hours(12);

/// Production scheduler cadence for per-event auto-record. The
/// start window is wider than the tick interval so a short runtime
/// stall does not skip an event entirely.
const AUTO_RECORD_TICK_INTERVAL: Duration = Duration::from_secs(30);

/// Per-tick cap on calendar events the auto-record scheduler pulls.
/// `list_upcoming_calendar` defaults to 20, which is the right shape
/// for the Home rail's upcoming-meetings widget but would silently
/// skip auto-record-enabled meetings for users with a packed week.
/// 100 mirrors the existing hard ceiling inside
/// `list_upcoming_calendar` (`limit.unwrap_or(20).min(100)`) — past
/// that, EventKit reads start to dominate per-tick latency.
const AUTO_RECORD_EVENT_LIMIT: u32 = 100;

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
    /// `Some` when the daemon was launched with a configured vault;
    /// read endpoints (`list_meetings`, `read_transcript`, etc.) use
    /// this to scan notes on disk. `None` reverts every read method
    /// to `NotYetImplemented` — the original phase 81 substrate
    /// behavior, preserved as the test default so the bus / cache
    /// fixtures don't need a tempdir.
    vault_root: Option<PathBuf>,
    /// Calendar bridge for `list_upcoming_calendar`. Defaults to the
    /// EventKit reader; tests inject a fake to bypass macOS TCC.
    calendar: Arc<dyn CalendarReader>,
    cache_dir: PathBuf,
    stt_backend_name: String,
    /// Tier 4 #17 vocabulary-boost hotwords forwarded to the WhisperKit
    /// backend at `start_capture` time. The desktop / `herond` shell
    /// populates this from `Settings::hotwords`; the legacy CLI path
    /// (`heron record`) leaves it empty so the v1 decode is byte-
    /// identical to pre-Tier-4. Cloned per session so a live-edit in
    /// the Settings pane doesn't mutate an in-flight session's prompt.
    hotwords: Vec<String>,
    llm_preference: heron_llm::Preference,
    /// Tier 4 #19: vault-writer slug strategy forwarded to every
    /// `CliSessionConfig` this orchestrator hands to the v1 pipeline.
    /// Read once from `Settings::file_naming_pattern` at orchestrator
    /// construction, mirroring the existing `stt_backend_name` /
    /// `llm_preference` cadence — runtime changes via the Settings
    /// pane only land on the next app launch. Default
    /// [`FileNamingPattern::Id`] preserves the legacy
    /// `<date>-<hhmm> <slug>.md` template (heron-cli's pre-Tier-4
    /// behavior on `Id`).
    file_naming_pattern: FileNamingPattern,
    /// In-flight captures keyed by `MeetingId`. Each entry pairs the
    /// last-published `Meeting` snapshot with the [`RecordingFsm`]
    /// driving its lifecycle. Held under a sync `Mutex` (no `.await`
    /// while locked) because every operation on it is short and CPU-
    /// bound: lookup, FSM transition, `bus.publish` (which is sync).
    /// Entries are removed on terminal transitions so the map stays
    /// the size of currently-active meetings.
    active_meetings: Mutex<HashMap<MeetingId, ActiveMeeting>>,
    /// Finalized meetings whose daemon-facing ID is the UUID minted
    /// at `POST /meetings` time. Vault notes already have a stable
    /// path-derived ID for read-side discovery; this index preserves
    /// the stronger API contract that the `Location` returned by
    /// `start_capture` remains readable after the background pipeline
    /// writes the note.
    finalized_meetings: Arc<Mutex<HashMap<MeetingId, FinalizedMeeting>>>,
    /// Pre-meeting contexts staged via `attach_context`, keyed by
    /// `calendar_event_id`. `start_capture` consumes the entry whose
    /// id matches `StartCaptureArgs::calendar_event_id`, attaching it
    /// to the resulting `ActiveMeeting`. Same sync-`Mutex` discipline
    /// as `active_meetings`: insert / remove / lookup are CPU-bound
    /// and the lock is never held across `.await`. In-memory only —
    /// a daemon restart drops staged context, matching the bus /
    /// cache resume contract.
    ///
    /// **Lock-ordering contract**: when both are taken in the same
    /// scope, the order is `active_meetings` first, then
    /// `pending_contexts`. `start_capture` no longer holds
    /// `active_meetings` while taking `pending_contexts` (it now
    /// scopes the active-meetings guard separately to keep the lock
    /// off the live-session-factory `.await`), but the order
    /// constraint is preserved as a forward-compatibility rule for
    /// any future code path that needs both.
    pending_contexts: PendingContexts,
    /// Per-event auto-record registry (Tier 5 #26). Persisted under
    /// `<vault_root>/.heron/auto_record.json` when a vault root is
    /// configured; in-memory only otherwise. The orchestrator
    /// mirrors `contains` onto each `CalendarEvent.auto_record` in
    /// `list_upcoming_calendar` so the rail's toggle reflects the
    /// current set without a second round trip.
    auto_record_registry: Arc<auto_record::AutoRecordRegistry>,
    /// Per-event "we already fired auto-record for this id, suppress
    /// re-fires until TTL elapses" map. Keyed by `calendar_event_id`,
    /// value is the wall-clock fire time. The auto-record start
    /// window for a single event is ~60s wide and the scheduler
    /// ticks every ~30s, so without this guard the same event would
    /// re-fire on every tick inside its window. TTL prunes happen
    /// inside `auto_record_tick` (no separate sweeper task).
    auto_record_fired: Mutex<HashMap<String, DateTime<Utc>>>,
    /// Held in a `Mutex<Option<…>>` so [`Self::shutdown`] (taking
    /// `&self`) can still consume the sender. Real callers don't
    /// touch the lock; the test seam takes it once.
    shutdown_tx: Mutex<Option<oneshot::Sender<()>>>,
    /// Same `Mutex<Option<…>>` rationale: lets `shutdown` move out
    /// of the join handle without `&mut self`.
    recorder: Mutex<Option<JoinHandle<()>>>,
    /// Background waiters that finish STT/LLM/vault finalization
    /// (and live v2 session shutdown) after `end_meeting` has
    /// returned. `shutdown()` drains them before stopping the
    /// replay recorder so terminal events still land in the cache.
    /// Pruned opportunistically by [`prune_finished_finalizers`]
    /// each time a new handle is pushed, so a long-running daemon
    /// does not accumulate handles for already-completed tasks.
    finalizers: Mutex<Vec<JoinHandle<()>>>,
    /// Optional v2 live-session factory. When set, `start_capture`
    /// composes the four-layer v2 stack
    /// (`MeetingBotDriver` + `RealtimeBackend` + `AudioBridge` +
    /// `SpeechController`) alongside the v1 vault pipeline. When
    /// unset (the default), `start_capture` only runs the v1
    /// pipeline, preserving the substrate-only behaviour every
    /// existing test relies on.
    ///
    /// Failures to compose the v2 stack are logged and tolerated:
    /// the v1 vault-backed path remains the fallback per
    /// `docs/archives/codebase-gaps.md`. The factory is what
    /// `apps/desktop/src-tauri` and `crates/herond` install at boot
    /// once an `OPENAI_API_KEY` and `RECALL_API_KEY` are available.
    live_session_factory: Option<Arc<dyn LiveSessionFactory>>,
    /// Tier 4 #23: gate for any future meeting-app detector loop.
    /// Read by [`LocalSessionOrchestrator::auto_detect_meeting_app`];
    /// the detector path (when one lands) must consult that getter
    /// before invoking `start_capture` on its own initiative. Manual
    /// capture paths (UI, hotkey, HTTP) do not consult this flag —
    /// it gates only the *automatic* arm path. `true` (the default)
    /// preserves the pre-Tier-4 behavior; the desktop shell flips it
    /// to `false` when the user has unchecked Settings → Recording
    /// → "Auto-detect meeting apps".
    auto_detect_meeting_app: bool,
}

/// Builder for [`LocalSessionOrchestrator`] — exposed so the daemon
/// (or tests) can tune capacities + retention without growing a
/// constructor surface that pins every dial as positional args.
#[derive(Clone)]
pub struct Builder {
    bus_capacity: usize,
    cache_capacity: usize,
    cache_window: Duration,
    vault_root: Option<PathBuf>,
    calendar: Option<Arc<dyn CalendarReader>>,
    cache_dir: PathBuf,
    stt_backend_name: String,
    /// Initial value for [`LocalSessionOrchestrator::hotwords`]. The
    /// desktop / daemon boot path calls
    /// [`Builder::hotwords`] to seed this from `Settings::hotwords`.
    hotwords: Vec<String>,
    llm_preference: heron_llm::Preference,
    file_naming_pattern: FileNamingPattern,
    live_session_factory: Option<Arc<dyn LiveSessionFactory>>,
    /// Tier 4 #23: gate for any future meeting-app detector loop that
    /// would auto-arm a recording without an explicit user gesture.
    /// `true` (the default) preserves the pre-Tier-4 behavior where
    /// the detector path — once it lands — runs unconditionally; `false`
    /// suppresses the auto-arm so only the manual hotkey / UI / HTTP
    /// `POST /v1/meetings` paths can start a capture. The desktop
    /// shell sets this from `Settings.auto_detect_meeting_app` at boot.
    auto_detect_meeting_app: bool,
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

    /// Shortcut: orchestrator with the read endpoints pointed at
    /// `vault_root`. Equivalent to
    /// `Builder::default().vault_root(root).build()`.
    pub fn with_vault(vault_root: PathBuf) -> Self {
        Builder::default().vault_root(vault_root).build()
    }

    /// Number of envelopes currently in the replay cache. Diagnostic
    /// only — production callers route through
    /// [`SessionOrchestrator::replay_cache`]. Tests use this to
    /// synchronize with the recorder task without polling
    /// `replay_since`.
    pub fn cache_len(&self) -> usize {
        self.cache.len()
    }

    /// Tier 4 #23: gate-point for a future meeting-app detector loop.
    ///
    /// Returns `true` when an auto-detect path is permitted to call
    /// [`SessionOrchestrator::start_capture`] on its own initiative,
    /// `false` when the user has disabled Settings → Recording →
    /// "Auto-detect meeting apps". Default is `true` (matching
    /// `Settings::default()` and the pre-Tier-4 contract).
    ///
    /// **Contract for detector authors.** Any code path that arms a
    /// recording without an explicit user gesture (hotkey press, UI
    /// click, HTTP `POST /v1/meetings`) MUST read this getter and
    /// short-circuit when it returns `false`. Manual paths are not
    /// gated by this flag — the user clicking Start in the UI is, by
    /// definition, an explicit gesture and should always work even
    /// when auto-detect is off.
    pub fn auto_detect_meeting_app(&self) -> bool {
        self.auto_detect_meeting_app
    }

    /// Snapshot of the `PreMeetingContext` currently staged for
    /// `calendar_event_id`, or `None` if `attach_context` was never
    /// called for that id (or `start_capture` already consumed it).
    /// Lookup normalizes the id (trim) the same way `attach_context`
    /// does so callers don't have to remember which form was stored.
    /// Diagnostic only — the production consumer is the future
    /// realtime / bot composition path that reads
    /// `ActiveMeeting::applied_context`.
    pub fn pending_context(&self, calendar_event_id: &str) -> Option<PreMeetingContext> {
        self.pending_contexts.get_cloned(calendar_event_id.trim())
    }

    /// Snapshot of the `PreMeetingContext` that `start_capture`
    /// consumed for the active meeting `id`, if any. Returns `None`
    /// when the meeting is unknown or no context was attached.
    pub fn applied_context(&self, id: &MeetingId) -> Option<PreMeetingContext> {
        lock_or_recover(&self.active_meetings)
            .get(id)
            .and_then(|m| m.applied_context.clone())
    }

    /// Whether `start_capture` successfully composed the v2 live
    /// session (bot + realtime + bridge + speech controller) for
    /// `id`. Diagnostic only — used by tests pinning the wiring
    /// from gap #1 and by future health probes.
    pub fn has_live_session(&self, id: &MeetingId) -> bool {
        lock_or_recover(&self.active_meetings)
            .get(id)
            .is_some_and(|m| m.live_session.is_some())
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
        let finalizers = std::mem::take(&mut *lock_or_recover(&self.finalizers));
        for handle in finalizers {
            handle.await?;
        }
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

    /// One pass of the auto-record scheduler (Tier 5 #26). For every
    /// upcoming event with `auto_record == true` whose start lies in
    /// `[now, now + AUTO_RECORD_START_WINDOW]` and that hasn't already
    /// been fired in the last `AUTO_RECORD_DEDUP_TTL`, drives
    /// `start_capture` with the event's id attached. Returns the
    /// number of fires this tick triggered — exposed for tests so
    /// they can drive the scheduler deterministically without
    /// orchestrating real time. Production callers go through
    /// [`spawn_auto_record_scheduler`].
    ///
    /// Errors from `start_capture` (`CaptureInProgress`,
    /// `PermissionMissing`, …) are logged at warn level and counted
    /// against `recently_fired` regardless — the scheduler has done
    /// its part; re-firing every tick just because the FSM rejected
    /// the request would spam the log without changing the outcome.
    ///
    /// Platform inference: today's `list_upcoming_calendar` always
    /// returns `meeting_url: None` (the Swift bridge doesn't expose
    /// it yet), so the scheduler defaults every fire to
    /// `Platform::Zoom`. When `meeting_url` is wired upstream, this
    /// branch picks the right platform per event and skips
    /// unrecognized providers instead of launching the wrong client.
    pub async fn auto_record_tick(&self, now: DateTime<Utc>) -> usize {
        auto_record::tick(self, now).await
    }

    /// Spawn the production per-event auto-record scheduler.
    ///
    /// The task owns only a weak reference between ticks. Dropping the
    /// returned [`JoinHandle`] detaches the task; dropping every
    /// `Arc<LocalSessionOrchestrator>` lets the scheduler exit on its
    /// next interval tick instead of keeping the orchestrator alive.
    pub fn spawn_auto_record_scheduler(self: &Arc<Self>) -> JoinHandle<()> {
        let orchestrator = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(AUTO_RECORD_TICK_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                let Some(orchestrator) = orchestrator.upgrade() else {
                    tracing::debug!("auto-record scheduler exiting: orchestrator dropped");
                    return;
                };
                let fired = orchestrator.auto_record_tick(Utc::now()).await;
                if fired > 0 {
                    tracing::debug!(fired, "auto-record scheduler tick completed");
                }
            }
        })
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
        // Active v2 live sessions can't be torn down here — their
        // shutdown calls are async and `Drop` cannot `await`. Each
        // session's own `Drop` already logs a warning when shut
        // down was skipped, but log here too with the orchestrator-
        // level count so an operator sees one aggregate signal
        // rather than N per-session lines. The fix is to call
        // `shutdown().await` in the daemon's exit path.
        let active = lock_or_recover(&self.active_meetings);
        let live_count = active.values().filter(|m| m.live_session.is_some()).count();
        if live_count > 0 {
            tracing::warn!(
                live_sessions = live_count,
                "LocalSessionOrchestrator dropped with active v2 live sessions; \
                 vendor bots may not be released cleanly. Call shutdown().await on \
                 the graceful-exit path.",
            );
        }
    }
}

/// Acquire the mutex, recovering the inner data on poisoning.
/// Every call site here holds the lock briefly for a synchronous
/// CPU-bound operation (consuming an `Option`, mutating a small
/// `HashMap` / `VecDeque`); poisoning would mean a panic happened
/// while one of those was in progress, which is benign because the
/// data structure is left in a consistent state and we're not
/// preserving cross-call invariants across the panic.
pub(crate) fn lock_or_recover<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
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
pub(crate) fn spawn_recorder(
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

pub(crate) fn default_cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("heron")
        .join("daemon")
}

/// Snapshot active captures matching a [`ListMeetingsQuery`]'s filters
/// (since / status / platform), newest-first. Caller is responsible
/// for limit / cursor handling — active captures never paginate.
pub(crate) fn collect_active_for_query(
    active: &Mutex<HashMap<MeetingId, ActiveMeeting>>,
    q: &ListMeetingsQuery,
) -> Vec<Meeting> {
    let mut items: Vec<Meeting> = lock_or_recover(active)
        .values()
        .map(|m| m.meeting.clone())
        .filter(|m| q.since.is_none_or(|since| m.started_at >= since))
        .filter(|m| q.status.is_none_or(|s| m.status == s))
        .filter(|m| q.platform.is_none_or(|p| m.platform == p))
        .collect();
    // Newest started_at first. Two captures with the same instant
    // is implausible (UUIDv7 minting + the start_capture lock
    // serialize them), so a strict-cmp on started_at is enough.
    items.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    items
}

#[async_trait]
impl SessionOrchestrator for LocalSessionOrchestrator {
    // Read endpoints scan the configured vault when `vault_root` is
    // `Some`, otherwise fall through to `NotYetImplemented` — same
    // shape as the substrate-only behavior phase 81 shipped, so
    // tests that don't configure a vault still get the original
    // surface.

    async fn list_meetings(&self, q: ListMeetingsQuery) -> Result<ListMeetingsPage, SessionError> {
        read_side::list_meetings(self, q).await
    }

    async fn get_meeting(&self, id: &MeetingId) -> Result<Meeting, SessionError> {
        read_side::get_meeting(self, id).await
    }

    async fn start_capture(&self, args: StartCaptureArgs) -> Result<Meeting, SessionError> {
        capture::start_capture(self, args).await
    }

    async fn end_meeting(&self, id: &MeetingId) -> Result<(), SessionError> {
        capture::end_meeting(self, id).await
    }

    async fn pause_capture(&self, id: &MeetingId) -> Result<(), SessionError> {
        capture::pause_capture(self, id).await
    }

    async fn resume_capture(&self, id: &MeetingId) -> Result<(), SessionError> {
        capture::resume_capture(self, id).await
    }

    async fn read_transcript(&self, id: &MeetingId) -> Result<Transcript, SessionError> {
        read_side::read_transcript(self, id).await
    }

    async fn read_summary(&self, id: &MeetingId) -> Result<Option<Summary>, SessionError> {
        read_side::read_summary(self, id).await
    }

    async fn audio_path(&self, id: &MeetingId) -> Result<PathBuf, SessionError> {
        read_side::audio_path(self, id).await
    }

    async fn list_upcoming_calendar(
        &self,
        from: Option<DateTime<Utc>>,
        to: Option<DateTime<Utc>>,
        limit: Option<u32>,
    ) -> Result<Vec<CalendarEvent>, SessionError> {
        read_side::list_upcoming_calendar(self, from, to, limit).await
    }

    async fn attach_context(&self, req: PreMeetingContextRequest) -> Result<(), SessionError> {
        context::attach_context(self, req).await
    }

    async fn prepare_context(&self, req: PrepareContextRequest) -> Result<(), SessionError> {
        context::prepare_context(self, req).await
    }

    async fn set_event_auto_record(
        &self,
        req: SetEventAutoRecordRequest,
    ) -> Result<(), SessionError> {
        auto_record::set_event_auto_record(self, req).await
    }

    async fn list_auto_record_events(&self) -> Result<AutoRecordList, SessionError> {
        auto_record::list_auto_record_events(self).await
    }

    async fn health(&self) -> Health {
        health::current(self).await
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
    use crate::compose::pre_meeting_briefing_for_v1;
    use crate::health::aggregate_health_status;
    use crate::live_session::{DynLiveSession, LiveSessionStartArgs};
    use crate::pipeline_glue::complete_pipeline_meeting;
    use crate::state::MAX_PENDING_CONTEXTS;
    use crate::validation::{MAX_CALENDAR_EVENT_ID_BYTES, MAX_PRE_MEETING_CONTEXT_BYTES};
    use crate::vault_read::platform_from_meeting_url;
    use heron_event::Envelope;
    use heron_session::{
        ComponentState, HealthComponent, HealthComponents, HealthStatus, Meeting, MeetingOutcome,
        MeetingStatus, Platform, SummaryLifecycle, TranscriptLifecycle,
    };
    use heron_types::RecordingFsm;
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
            tags: vec![],
            processing: None,
            action_items: vec![],
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

    #[test]
    fn platform_from_meeting_url_matches_known_providers_only() {
        assert_eq!(
            platform_from_meeting_url(Some("https://zoom.us/j/123")),
            Some(Platform::Zoom),
        );
        assert_eq!(
            platform_from_meeting_url(Some("https://meet.google.com/abc-defg-hij")),
            Some(Platform::GoogleMeet),
        );
        assert_eq!(
            platform_from_meeting_url(Some("https://teams.microsoft.com/l/meetup-join/x")),
            Some(Platform::MicrosoftTeams),
        );
        assert_eq!(
            platform_from_meeting_url(Some("https://example.com/teams.fake/meeting")),
            None,
            "unrecognized URLs must not be treated as Teams just because they contain `teams.`",
        );
    }

    /// Tier 4 #23: the auto-detect gate defaults to `true` so the
    /// pre-Tier-4 detector contract is preserved for every existing
    /// caller that doesn't opt in to the builder method.
    #[tokio::test]
    async fn auto_detect_meeting_app_defaults_true() {
        let orch = LocalSessionOrchestrator::new();
        assert!(
            orch.auto_detect_meeting_app(),
            "default builder must enable auto-detect (preserves pre-Tier-4 behavior)",
        );
    }

    /// Tier 4 #23: the builder setter round-trips through the getter,
    /// covering both branches (the desktop wires `false` from
    /// `Settings.auto_detect_meeting_app` when the user has unchecked
    /// the toggle, and re-enables it when they re-check).
    #[tokio::test]
    async fn auto_detect_meeting_app_round_trips_through_builder() {
        for enabled in [true, false] {
            let orch = Builder::default().auto_detect_meeting_app(enabled).build();
            assert_eq!(
                orch.auto_detect_meeting_app(),
                enabled,
                "builder setter for {enabled:?} must round-trip through the getter",
            );
        }
    }

    /// Tier 4 #23: the gate must NOT affect manual `start_capture`
    /// — the user clicking Start in the UI is an explicit gesture and
    /// the manual path always proceeds, even with auto-detect off.
    /// This test pins the "manual path is unaffected" contract by
    /// running `start_capture` against an orchestrator built with
    /// `auto_detect_meeting_app(false)` and asserting the full
    /// `MeetingDetected → MeetingArmed → MeetingStarted` envelope
    /// trio still publishes to the bus.
    #[tokio::test]
    async fn manual_start_capture_unaffected_when_auto_detect_disabled() {
        let orch = Builder::default().auto_detect_meeting_app(false).build();
        let mut rx = orch.event_bus().subscribe();
        let result = orch
            .start_capture(StartCaptureArgs {
                platform: Platform::Zoom,
                hint: Some("Test".into()),
                calendar_event_id: None,
            })
            .await;
        assert!(
            result.is_ok(),
            "auto_detect_meeting_app(false) must not block manual start_capture; got {result:?}",
        );

        // Drain the three FSM-walk envelopes the substrate-only path
        // emits (`MeetingDetected` → `MeetingArmed` → `MeetingStarted`).
        // Use a generous timeout so a slow test runner doesn't flake;
        // under normal load this completes within microseconds.
        let mut kinds = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while kinds.len() < 3 {
            if Instant::now() > deadline {
                panic!(
                    "expected 3 envelopes (detected/armed/started); got {} ({kinds:?})",
                    kinds.len(),
                );
            }
            match tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
                Ok(Ok(env)) => kinds.push(env.payload.event_type().to_owned()),
                Ok(Err(_)) | Err(_) => continue,
            }
        }
        assert_eq!(
            kinds,
            vec![
                "meeting.detected".to_owned(),
                "meeting.armed".to_owned(),
                "meeting.started".to_owned(),
            ],
            "manual start_capture must publish the full FSM-walk trio regardless of auto-detect",
        );

        // Cleanup — terminate the in-flight meeting so the test
        // shutdown path is deterministic. Same `lock_or_recover`
        // discipline as production callers (treat a poisoned mutex as
        // recoverable rather than masking it).
        let active_id = lock_or_recover(&orch.active_meetings)
            .keys()
            .next()
            .copied();
        if let Some(id) = active_id {
            let _ = orch.end_meeting(&id).await;
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
    async fn substrate_only_methods_return_not_yet_implemented_without_vault() {
        // Pin the "stub for now" contract per-method when no
        // `vault_root` is configured. Read endpoints fall back to
        // `NotYetImplemented` because there's no on-disk source to
        // scan. Capture-lifecycle methods (`start_capture` /
        // `end_meeting`) are NOT in this set — FSM-merge wired them
        // to drive the `RecordingFsm` and publish bus events directly,
        // no vault dependency. `list_upcoming_calendar` is also NOT
        // in this set — it works as soon as a CalendarReader is
        // configured. `attach_context` is also NOT in this set:
        // pre-meeting context lives in an in-memory map keyed by
        // calendar event id, independent of the vault.
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
    }

    #[tokio::test]
    async fn health_reports_configured_orchestrator_capabilities() {
        // /health reports the local orchestrator's configured
        // capabilities without triggering side effects such as
        // EventKit TCC prompts, model downloads, or hosted LLM calls.
        let orch = LocalSessionOrchestrator::new();
        let h = orch.health().await;
        // Aggregate isn't pinned here: capture and vault are both
        // `Degraded` (no configured root → synthetic-only), but the
        // `llm` probe depends on env keys / `claude` / `codex` on
        // PATH and may be `Down`, which would dominate. The truth-
        // table test pins the aggregation contract directly.
        assert!(matches!(
            h.components.capture.state,
            ComponentState::Degraded
        ));
        assert!(matches!(h.components.vault.state, ComponentState::Degraded));
        assert!(matches!(h.components.eventkit.state, ComponentState::Ok));
        // The default `sherpa` STT backend reports available
        // unconditionally, so whisperkit pins to `Ok` regardless of
        // the host machine — pin it to guard against silent regressions
        // if the default flips to a backend with environment-dependent
        // availability. `llm` is intentionally not asserted: its state
        // depends on env keys / `claude` / `codex` on PATH.
        assert!(matches!(h.components.whisperkit.state, ComponentState::Ok));
        assert!(
            !h.components
                .capture
                .message
                .as_deref()
                .unwrap_or_default()
                .contains("not yet wired"),
            "capture health should no longer report placeholder wiring"
        );
        assert!(
            !h.components
                .eventkit
                .message
                .as_deref()
                .unwrap_or_default()
                .contains("not yet wired"),
            "EventKit health should no longer report placeholder wiring"
        );
    }

    #[test]
    fn aggregate_health_status_truth_table() {
        // Pin the contract directly — the end-to-end /health tests
        // only exercise paths through the live orchestrator, so
        // `Degraded`-stickiness and `PermissionMissing`-short-circuit
        // can regress silently without a focused test.
        fn component(state: ComponentState) -> HealthComponent {
            HealthComponent {
                state,
                message: None,
                last_check: None,
            }
        }
        fn components(states: [ComponentState; 5]) -> HealthComponents {
            HealthComponents {
                capture: component(states[0]),
                whisperkit: component(states[1]),
                vault: component(states[2]),
                eventkit: component(states[3]),
                llm: component(states[4]),
            }
        }
        use ComponentState::{Degraded, Down, Ok as Up, PermissionMissing};

        assert!(matches!(
            aggregate_health_status(&components([Up, Up, Up, Up, Up])),
            HealthStatus::Ok
        ));
        assert!(matches!(
            aggregate_health_status(&components([Up, Degraded, Up, Up, Up])),
            HealthStatus::Degraded
        ));
        assert!(matches!(
            aggregate_health_status(&components([Up, Up, Up, Up, Down])),
            HealthStatus::Down
        ));
        // PermissionMissing must collapse to Down, not Degraded —
        // otherwise a denied TCC permission masquerades as a soft
        // degradation and consumers stop alerting.
        assert!(matches!(
            aggregate_health_status(&components([Up, Up, Up, PermissionMissing, Up])),
            HealthStatus::Down
        ));
        // Down dominates Degraded.
        assert!(matches!(
            aggregate_health_status(&components([Degraded, Up, Down, Up, Up])),
            HealthStatus::Down
        ));
    }

    #[tokio::test]
    async fn health_reports_vault_down_when_configured_root_missing() {
        // Configured-but-missing vault root must report `Down`, not
        // `PermissionMissing`. The latter would route operators down a
        // TCC-debugging dead end for what is really a misconfig — the
        // path on disk doesn't exist.
        let parent = tempfile::tempdir().expect("tempdir");
        let missing = parent.path().join("vault-that-was-never-created");
        assert!(!missing.exists());
        let orch = Builder::default().vault_root(missing.clone()).build();
        let h = orch.health().await;
        assert!(matches!(h.status, HealthStatus::Down));
        let vault = &h.components.vault;
        assert!(
            matches!(vault.state, ComponentState::Down),
            "expected Down for missing vault root, got {:?}",
            vault.state,
        );
        let msg = vault.message.as_deref().unwrap_or_default();
        assert!(
            msg.contains(&missing.display().to_string()),
            "expected message to include path, got {msg:?}",
        );
        assert!(
            msg.contains("does not exist"),
            "expected message to say 'does not exist', got {msg:?}",
        );
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

    /// Tier 4 #19: `Builder::file_naming_pattern` threads through to
    /// `LocalSessionOrchestrator::file_naming_pattern`, the field
    /// `start_capture` reads when assembling each `CliSessionConfig`.
    /// Without this hand-off the desktop / herond boot path's
    /// `read_settings(...).file_naming_pattern` value lands nowhere.
    #[tokio::test]
    async fn builder_file_naming_pattern_threads_through() {
        let orch = Builder::default()
            .file_naming_pattern(FileNamingPattern::DateSlug)
            .build();
        assert_eq!(orch.file_naming_pattern, FileNamingPattern::DateSlug);

        // Default stays at `Id` so unrelated tests don't see a behavior
        // change. Pinned alongside the override path so a later
        // regression that flips the default falls into this test.
        let default_orch = LocalSessionOrchestrator::new();
        assert_eq!(default_orch.file_naming_pattern, FileNamingPattern::Id);
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

    // ── FSM-merge: capture lifecycle ──────────────────────────────────

    /// Drain every envelope currently buffered in `rx` into a vector.
    /// Used by capture-lifecycle tests so they can assert the exact
    /// sequence of `meeting.*` events `start_capture` / `end_meeting`
    /// emits without racing the recorder task.
    fn drain(
        rx: &mut tokio::sync::broadcast::Receiver<Envelope<EventPayload>>,
    ) -> Vec<Envelope<EventPayload>> {
        let mut out = Vec::new();
        while let Ok(env) = rx.try_recv() {
            out.push(env);
        }
        out
    }

    #[tokio::test]
    async fn start_capture_walks_fsm_and_publishes_three_events() {
        // Pin the bus contract for the manual-capture escape hatch:
        // exactly three events fire (`detected → armed → started`),
        // each carries the same `MeetingId` in its envelope frame
        // (Envelope.meeting_id consistency invariant), and the
        // returned `Meeting` lands at `Recording`.
        let orch = LocalSessionOrchestrator::new();
        let mut rx = orch.event_bus().subscribe();

        let meeting = orch
            .start_capture(StartCaptureArgs {
                platform: Platform::Zoom,
                hint: Some("Standup".into()),
                calendar_event_id: None,
            })
            .await
            .expect("start_capture");
        assert!(matches!(meeting.status, MeetingStatus::Recording));
        assert_eq!(meeting.title.as_deref(), Some("Standup"));

        let events = drain(&mut rx);
        let kinds: Vec<&str> = events.iter().map(|e| e.payload.event_type()).collect();
        assert_eq!(
            kinds,
            ["meeting.detected", "meeting.armed", "meeting.started"],
            "unexpected event sequence: {kinds:?}",
        );
        let id_str = meeting.id.to_string();
        for env in &events {
            assert_eq!(
                env.meeting_id.as_deref(),
                Some(id_str.as_str()),
                "envelope.meeting_id must match payload meeting id",
            );
        }
    }

    /// Per-issue #224 acceptance: a capture lifecycle bumps the four
    /// metrics from `metrics_names` (the smoke counter from #223 +
    /// the new ended counter + the active gauge + the salvage
    /// recovery counter via the synthetic-runtime path that emits
    /// `MeetingCompleted{outcome=Success}` directly). Mirrors the
    /// shape of `metrics_endpoint_returns_prometheus_exposition_with_bearer`
    /// from `crates/herond/tests/api.rs`.
    #[tokio::test]
    async fn capture_lifecycle_metrics_emit_on_happy_path() {
        let handle = heron_metrics::init_prometheus_recorder().expect("install recorder for test");

        let orch = LocalSessionOrchestrator::new();
        // Snapshot before so the assertions tolerate other tests in
        // the same `cargo test` process bumping the same counters.
        let before = handle.render();

        let meeting = orch
            .start_capture(StartCaptureArgs {
                platform: Platform::Zoom,
                hint: None,
                calendar_event_id: None,
            })
            .await
            .expect("start_capture");
        orch.end_meeting(&meeting.id).await.expect("end_meeting");

        let after = handle.render();

        // Smoke counter: the foundation's existing assertion still
        // holds — start_capture bumped `capture_started_total` for
        // platform=zoom.
        assert!(
            after.contains(heron_metrics::SMOKE_CAPTURE_STARTED_TOTAL),
            "rendered exposition must contain smoke counter; got:\n{after}"
        );

        // Ended counter for `reason="user_stop"` — the request-handler
        // emission from `end_meeting` is unconditional.
        let user_stop_count = scrape_counter_with_label(
            &after,
            metrics_names::CAPTURE_ENDED_TOTAL,
            "reason=\"user_stop\"",
        );
        let user_stop_before = scrape_counter_with_label(
            &before,
            metrics_names::CAPTURE_ENDED_TOTAL,
            "reason=\"user_stop\"",
        );
        assert_eq!(
            user_stop_count - user_stop_before,
            1,
            "exactly one user_stop reason emission per end_meeting; rendered:\n{after}"
        );

        // The synthetic runtime in this test (no vault root) emits
        // `MeetingCompleted{outcome=Success}` directly from
        // `end_meeting`, NOT through `complete_pipeline_meeting`.
        // That's why the `reason="success"` arm doesn't fire here —
        // the pipeline-side disposition counter is exercised
        // separately by the v1-pipeline integration test under
        // `tests/clio_full_pipeline.rs`. Pinning the synthetic-path
        // contract: the `reason="success"` count does NOT bump on
        // the synthetic path.
        let success_count = scrape_counter_with_label(
            &after,
            metrics_names::CAPTURE_ENDED_TOTAL,
            "reason=\"success\"",
        );
        let success_before = scrape_counter_with_label(
            &before,
            metrics_names::CAPTURE_ENDED_TOTAL,
            "reason=\"success\"",
        );
        assert_eq!(
            success_count, success_before,
            "synthetic runtime path must not bump pipeline-side success counter; rendered:\n{after}",
        );

        // Salvage candidates pending: built fresh per orchestrator,
        // but the cache root may be inherited from the user's
        // environment — assert the metric line exists.
        assert!(
            after.contains(metrics_names::SALVAGE_CANDIDATES_PENDING),
            "rendered exposition must contain salvage_candidates_pending; got:\n{after}"
        );
    }

    /// Failure-path coverage for the pipeline-side disposition
    /// counter. Drives `complete_pipeline_meeting` directly with a
    /// failure result so we don't need a real audio pipeline.
    #[test]
    fn complete_pipeline_meeting_emits_error_and_abandoned_on_failure() {
        let handle = heron_metrics::init_prometheus_recorder().expect("install recorder for test");

        // Construct a minimal fake meeting + bus and drive the
        // helper. The assertions key on counter deltas so other
        // tests in the same process can run interleaved.
        let bus: SessionEventBus = heron_event::EventBus::new(8);
        let finalized: std::sync::Mutex<HashMap<MeetingId, FinalizedMeeting>> =
            std::sync::Mutex::new(HashMap::new());
        let id = MeetingId::now_v7();
        let mut fsm = RecordingFsm::new();
        // Walk the FSM far enough that `on_transcribe_done` is legal.
        fsm.on_hotkey().expect("idle->armed");
        fsm.on_yes().expect("armed->recording");
        fsm.on_hotkey().expect("recording->transcribing");
        let meeting = Meeting {
            id,
            status: MeetingStatus::Recording,
            platform: Platform::Zoom,
            title: None,
            calendar_event_id: None,
            started_at: Utc::now(),
            ended_at: None,
            duration_secs: None,
            participants: vec![],
            transcript_status: TranscriptLifecycle::Pending,
            summary_status: SummaryLifecycle::Pending,
            tags: vec![],
            processing: None,
            action_items: vec![],
        };

        let before = handle.render();
        let before_error = scrape_counter_with_label(
            &before,
            metrics_names::CAPTURE_ENDED_TOTAL,
            "reason=\"error\"",
        );
        let before_abandoned = scrape_counter_with_label(
            &before,
            metrics_names::SALVAGE_RECOVERY_TOTAL,
            "outcome=\"abandoned\"",
        );

        complete_pipeline_meeting(
            &bus,
            &finalized,
            id,
            fsm,
            meeting,
            Err(SessionError::Validation {
                detail: "synthetic failure".into(),
            }),
        );

        let after = handle.render();
        let after_error = scrape_counter_with_label(
            &after,
            metrics_names::CAPTURE_ENDED_TOTAL,
            "reason=\"error\"",
        );
        let after_abandoned = scrape_counter_with_label(
            &after,
            metrics_names::SALVAGE_RECOVERY_TOTAL,
            "outcome=\"abandoned\"",
        );
        assert_eq!(
            after_error - before_error,
            1,
            "pipeline-failure path must bump capture_ended_total{{reason=error}} exactly once; \
             rendered:\n{after}"
        );
        assert_eq!(
            after_abandoned - before_abandoned,
            1,
            "pipeline-failure path must bump salvage_recovery_total{{outcome=abandoned}} \
             exactly once; rendered:\n{after}"
        );
    }

    /// Helper for metric-test assertions: parse the
    /// `<name>{label_match...} <value>` line out of the Prometheus
    /// exposition body. Returns 0 when the metric isn't present
    /// (lazy registration; nothing emitted yet for that label set).
    fn scrape_counter_with_label(body: &str, name: &str, label_match: &str) -> u64 {
        for line in body.lines() {
            if line.starts_with('#') {
                continue;
            }
            if line.starts_with(name)
                && line.contains(label_match)
                && let Some(val) = line.rsplit(' ').next()
                && let Ok(n) = val.parse::<u64>()
            {
                return n;
            }
        }
        0
    }

    #[tokio::test]
    async fn end_meeting_publishes_ended_then_completed() {
        // The other half of the bus contract: end_meeting fires
        // `meeting.ended` then a single `meeting.completed` with
        // `outcome: success` (Invariant 9 — there is no
        // `meeting.failed` variant).
        let orch = LocalSessionOrchestrator::new();
        let mut rx = orch.event_bus().subscribe();
        let meeting = orch
            .start_capture(StartCaptureArgs {
                platform: Platform::Zoom,
                hint: None,
                calendar_event_id: None,
            })
            .await
            .expect("start_capture");
        // Drain start_capture's events so the assertions below scope
        // strictly to end_meeting's emissions.
        let _ = drain(&mut rx);

        orch.end_meeting(&meeting.id).await.expect("end_meeting");

        let events = drain(&mut rx);
        assert_eq!(events.len(), 2, "expected ended + completed");
        assert!(matches!(events[0].payload, EventPayload::MeetingEnded(_)));
        match &events[1].payload {
            EventPayload::MeetingCompleted(data) => {
                assert!(matches!(data.outcome, MeetingOutcome::Success));
                assert!(matches!(data.meeting.status, MeetingStatus::Done));
                assert!(data.meeting.ended_at.is_some());
                assert!(data.meeting.duration_secs.is_some());
            }
            other => panic!("expected MeetingCompleted, got {}", other.event_type()),
        }
    }

    #[tokio::test]
    async fn pause_then_resume_walks_status_through_paused() {
        // Tier 3 #16 happy path: pause flips `MeetingStatus::Paused`,
        // resume flips it back to `Recording`. Round-tripping must
        // also leave the active meeting still endable — `end_meeting`
        // through the FSM must work after the pause/resume cycle.
        let orch = LocalSessionOrchestrator::new();
        let meeting = orch
            .start_capture(StartCaptureArgs {
                platform: Platform::Zoom,
                hint: None,
                calendar_event_id: None,
            })
            .await
            .expect("start_capture");

        orch.pause_capture(&meeting.id).await.expect("pause");
        let snapshot = orch.get_meeting(&meeting.id).await.expect("get_meeting");
        assert!(matches!(snapshot.status, MeetingStatus::Paused));

        orch.resume_capture(&meeting.id).await.expect("resume");
        let snapshot = orch.get_meeting(&meeting.id).await.expect("get_meeting");
        assert!(matches!(snapshot.status, MeetingStatus::Recording));

        // After a pause/resume cycle the meeting must still finalize
        // through `end_meeting`. Without this the cycle could leave
        // the FSM in a state from which `on_hotkey` is illegal —
        // exactly the regression Tier 3 #16 is supposed to prevent.
        orch.end_meeting(&meeting.id).await.expect("end_meeting");
    }

    #[tokio::test]
    async fn pause_while_paused_returns_invalid_state() {
        // Idempotent guards: a second `pause_capture` on an
        // already-paused meeting must surface `InvalidState` so the
        // HTTP layer returns `409`. Pin the typed error so the wire
        // shape doesn't drift to `Validation` / `NotFound` on a
        // future refactor.
        let orch = LocalSessionOrchestrator::new();
        let meeting = orch
            .start_capture(StartCaptureArgs {
                platform: Platform::Zoom,
                hint: None,
                calendar_event_id: None,
            })
            .await
            .expect("start_capture");
        orch.pause_capture(&meeting.id).await.expect("pause");
        let err = orch
            .pause_capture(&meeting.id)
            .await
            .expect_err("second pause must error");
        assert!(matches!(err, SessionError::InvalidState { .. }));
    }

    #[tokio::test]
    async fn resume_while_recording_returns_invalid_state() {
        // Mirror image of the pause-while-paused guard. A `resume_capture`
        // on a meeting that was never paused must be `InvalidState`,
        // not silently succeed.
        let orch = LocalSessionOrchestrator::new();
        let meeting = orch
            .start_capture(StartCaptureArgs {
                platform: Platform::Zoom,
                hint: None,
                calendar_event_id: None,
            })
            .await
            .expect("start_capture");
        let err = orch
            .resume_capture(&meeting.id)
            .await
            .expect_err("resume from recording must error");
        assert!(matches!(err, SessionError::InvalidState { .. }));
    }

    #[tokio::test]
    async fn pause_unknown_id_is_not_found() {
        let orch = LocalSessionOrchestrator::new();
        let err = orch
            .pause_capture(&MeetingId::now_v7())
            .await
            .expect_err("unknown id must error");
        assert!(matches!(err, SessionError::NotFound { .. }));
        let err = orch
            .resume_capture(&MeetingId::now_v7())
            .await
            .expect_err("unknown id must error");
        assert!(matches!(err, SessionError::NotFound { .. }));
    }

    #[tokio::test]
    async fn end_meeting_while_paused_finalizes() {
        // Stop while paused must finalize the note via the same
        // `end_meeting` path. Without this, a user who paused and
        // then hit Stop would be stuck — `end_meeting` from
        // `MeetingStatus::Paused` was the FSM-level regression that
        // motivated Tier 3 #16's `Paused → Transcribing` edge.
        let orch = LocalSessionOrchestrator::new();
        let meeting = orch
            .start_capture(StartCaptureArgs {
                platform: Platform::Zoom,
                hint: None,
                calendar_event_id: None,
            })
            .await
            .expect("start_capture");
        orch.pause_capture(&meeting.id).await.expect("pause");
        orch.end_meeting(&meeting.id)
            .await
            .expect("end_meeting while paused");
    }

    #[tokio::test]
    async fn start_capture_rejects_second_capture_for_same_platform() {
        // Singleton-per-platform invariant: a second `start_capture`
        // for an already-recording platform is `409 CaptureInProgress`.
        // A different platform is allowed in parallel.
        let orch = LocalSessionOrchestrator::new();
        let _first = orch
            .start_capture(StartCaptureArgs {
                platform: Platform::Zoom,
                hint: None,
                calendar_event_id: None,
            })
            .await
            .expect("first start");

        let err = orch
            .start_capture(StartCaptureArgs {
                platform: Platform::Zoom,
                hint: None,
                calendar_event_id: None,
            })
            .await
            .expect_err("second Zoom start must conflict");
        assert!(
            matches!(
                err,
                SessionError::CaptureInProgress {
                    platform: Platform::Zoom
                }
            ),
            "expected CaptureInProgress, got {err:?}",
        );

        // A different platform doesn't conflict.
        orch.start_capture(StartCaptureArgs {
            platform: Platform::GoogleMeet,
            hint: None,
            calendar_event_id: None,
        })
        .await
        .expect("second start on a different platform");
    }

    #[tokio::test]
    async fn start_capture_after_end_releases_the_platform_singleton() {
        // Once a meeting is terminal (entry removed on end_meeting),
        // a fresh capture on the same platform must succeed —
        // otherwise the daemon would refuse all future captures
        // after the first one ends.
        let orch = LocalSessionOrchestrator::new();
        let first = orch
            .start_capture(StartCaptureArgs {
                platform: Platform::Zoom,
                hint: None,
                calendar_event_id: None,
            })
            .await
            .expect("first start");
        orch.end_meeting(&first.id).await.expect("end first");

        let second = orch
            .start_capture(StartCaptureArgs {
                platform: Platform::Zoom,
                hint: None,
                calendar_event_id: None,
            })
            .await
            .expect("second start after end");
        assert_ne!(first.id, second.id, "fresh meeting id expected");
    }

    #[tokio::test]
    async fn end_meeting_unknown_id_is_not_found() {
        // A meeting id the orchestrator never saw collapses to
        // `NotFound` — the HTTP projection maps that to `404`. We
        // deliberately don't store terminal meetings in the active
        // map, so a second `end_meeting` for a just-completed
        // meeting also lands here (documented in the impl).
        let orch = LocalSessionOrchestrator::new();
        let err = orch
            .end_meeting(&MeetingId::now_v7())
            .await
            .expect_err("unknown id must error");
        assert!(matches!(err, SessionError::NotFound { .. }));
    }

    #[tokio::test]
    async fn capture_lifecycle_events_land_in_replay_cache() {
        // The fired-and-forgotten contract for `/events`: events
        // published from `start_capture` / `end_meeting` flow through
        // the bus → recorder → replay cache pipeline so a late SSE
        // subscriber resuming with `Last-Event-ID` can still see the
        // capture's history. Without this the FSM-merge wiring would
        // be invisible to a reconnecting client.
        let orch = LocalSessionOrchestrator::new();
        let meeting = orch
            .start_capture(StartCaptureArgs {
                platform: Platform::Zoom,
                hint: None,
                calendar_event_id: None,
            })
            .await
            .expect("start_capture");
        orch.end_meeting(&meeting.id).await.expect("end_meeting");

        // Five envelopes total: detected, armed, started, ended, completed.
        let deadline = Instant::now() + Duration::from_secs(2);
        while orch.cache_len() < 5 {
            if Instant::now() > deadline {
                panic!(
                    "recorder never reached 5 entries (cur={})",
                    orch.cache_len(),
                );
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(orch.cache_len(), 5);
    }

    #[tokio::test]
    async fn get_meeting_returns_active_capture_before_vault_lookup() {
        // Closes the wire-contract regression where
        // `POST /meetings` returns `Location: /v1/meetings/{id}`
        // but `GET /meetings/{id}` 404s because the read endpoint
        // only scanned the disk vault. Active captures must be
        // visible to `get_meeting` so the Location header doesn't
        // dangle.
        let orch = LocalSessionOrchestrator::new();
        let started = orch
            .start_capture(StartCaptureArgs {
                platform: Platform::Zoom,
                hint: Some("Standup".into()),
                calendar_event_id: None,
            })
            .await
            .expect("start_capture");

        let fetched = orch.get_meeting(&started.id).await.expect("get_meeting");
        assert_eq!(fetched.id, started.id);
        assert!(matches!(fetched.status, MeetingStatus::Recording));
        assert_eq!(fetched.title.as_deref(), Some("Standup"));

        // After end_meeting, the entry moves from the active set to
        // the finalized index so the `Location: /v1/meetings/{id}`
        // returned by start_capture remains readable for this daemon
        // process even before the vault-backed pipeline writes a note.
        orch.end_meeting(&started.id).await.expect("end_meeting");
        let done = orch
            .get_meeting(&started.id)
            .await
            .expect("finalized meeting");
        assert_eq!(done.id, started.id);
        assert!(matches!(done.status, MeetingStatus::Done));
    }

    #[tokio::test]
    async fn list_meetings_surfaces_active_capture_without_vault() {
        // A vault-less daemon can still capture; `list_meetings`
        // must surface in-flight meetings so a client polling the
        // REST surface (rather than subscribing to /events) can
        // discover them. Without a vault and zero captures the
        // method preserves the substrate-only `NotYetImplemented`
        // contract — that's covered by the existing
        // substrate_only_methods_return_not_yet_implemented_without_vault
        // test, which doesn't start a capture.
        let orch = LocalSessionOrchestrator::new();
        let started = orch
            .start_capture(StartCaptureArgs {
                platform: Platform::Zoom,
                hint: None,
                calendar_event_id: None,
            })
            .await
            .expect("start_capture");

        let page = orch
            .list_meetings(ListMeetingsQuery::default())
            .await
            .expect("list_meetings");
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].id, started.id);
        assert!(matches!(page.items[0].status, MeetingStatus::Recording));
        assert!(page.next_cursor.is_none());
    }

    #[tokio::test]
    async fn list_meetings_filters_active_capture_by_platform_and_status() {
        // The existing query filters (since / status / platform)
        // apply to active captures the same as to disk results so a
        // client polling `?status=recording` doesn't get a vault note
        // mixed in, and vice versa.
        let orch = LocalSessionOrchestrator::new();
        let started = orch
            .start_capture(StartCaptureArgs {
                platform: Platform::Zoom,
                hint: None,
                calendar_event_id: None,
            })
            .await
            .expect("start_capture");

        // Filter: matching platform, recording status — should hit.
        let page = orch
            .list_meetings(ListMeetingsQuery {
                platform: Some(Platform::Zoom),
                status: Some(MeetingStatus::Recording),
                ..Default::default()
            })
            .await
            .expect("list_meetings");
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].id, started.id);

        // Filter: non-matching platform — should miss.
        let err = orch
            .list_meetings(ListMeetingsQuery {
                platform: Some(Platform::Webex),
                ..Default::default()
            })
            .await
            .expect_err("no Webex captures, no vault — should be NotYetImplemented");
        assert!(matches!(err, SessionError::NotYetImplemented));
    }

    // ── pre-meeting context (gap #4) ──────────────────────────────────

    fn ctx_with_agenda(agenda: &str) -> heron_session::PreMeetingContext {
        heron_session::PreMeetingContext {
            agenda: Some(agenda.to_owned()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn attach_context_persists_and_is_retrievable() {
        let orch = LocalSessionOrchestrator::new();
        orch.attach_context(PreMeetingContextRequest {
            calendar_event_id: "evt_alpha".into(),
            context: ctx_with_agenda("standup"),
        })
        .await
        .expect("attach");
        let got = orch
            .pending_context("evt_alpha")
            .expect("staged context retrievable");
        assert_eq!(got.agenda.as_deref(), Some("standup"));
        // Unrelated id stays unstaged.
        assert!(orch.pending_context("evt_other").is_none());
    }

    #[tokio::test]
    async fn attach_context_overwrites_for_same_calendar_event_id() {
        let orch = LocalSessionOrchestrator::new();
        orch.attach_context(PreMeetingContextRequest {
            calendar_event_id: "evt_alpha".into(),
            context: ctx_with_agenda("first"),
        })
        .await
        .expect("first attach");
        orch.attach_context(PreMeetingContextRequest {
            calendar_event_id: "evt_alpha".into(),
            context: ctx_with_agenda("second"),
        })
        .await
        .expect("second attach");
        let got = orch.pending_context("evt_alpha").expect("staged");
        assert_eq!(got.agenda.as_deref(), Some("second"));
    }

    #[tokio::test]
    async fn attach_context_rejects_empty_calendar_event_id() {
        let orch = LocalSessionOrchestrator::new();
        for cid in ["", "   "] {
            let err = orch
                .attach_context(PreMeetingContextRequest {
                    calendar_event_id: cid.into(),
                    context: Default::default(),
                })
                .await
                .expect_err("empty id must be rejected");
            assert!(
                matches!(err, SessionError::Validation { .. }),
                "expected Validation, got {err:?}",
            );
        }
    }

    #[tokio::test]
    async fn attach_context_rejects_oversized_payload() {
        let orch = LocalSessionOrchestrator::new();
        let big_briefing = "x".repeat(MAX_PRE_MEETING_CONTEXT_BYTES + 1);
        let err = orch
            .attach_context(PreMeetingContextRequest {
                calendar_event_id: "evt_big".into(),
                context: heron_session::PreMeetingContext {
                    user_briefing: Some(big_briefing),
                    ..Default::default()
                },
            })
            .await
            .expect_err("oversized payload must be rejected");
        assert!(
            matches!(err, SessionError::Validation { .. }),
            "expected Validation, got {err:?}",
        );
        assert!(orch.pending_context("evt_big").is_none());
    }

    #[tokio::test]
    async fn attach_context_rejects_oversized_calendar_event_id() {
        let orch = LocalSessionOrchestrator::new();
        let huge_id = "a".repeat(MAX_CALENDAR_EVENT_ID_BYTES + 1);
        let err = orch
            .attach_context(PreMeetingContextRequest {
                calendar_event_id: huge_id,
                context: Default::default(),
            })
            .await
            .expect_err("oversized id must be rejected");
        assert!(
            matches!(err, SessionError::Validation { .. }),
            "expected Validation, got {err:?}",
        );
    }

    #[tokio::test]
    async fn prepare_context_stages_default_with_attendees_known() {
        // The auto-prime path lifts the calendar event's attendees
        // into `attendees_known` and leaves the rest of
        // `PreMeetingContext` at default. The rail uses this to flip
        // the `primed` indicator without forcing the user to supply
        // anything extra.
        let orch = LocalSessionOrchestrator::new();
        orch.prepare_context(heron_session::PrepareContextRequest {
            calendar_event_id: "evt_alpha".into(),
            attendees: vec![heron_session::AttendeeContext {
                name: "Alex Chen".into(),
                email: Some("alex@example.com".into()),
                last_seen_in: None,
                relationship: None,
                notes: None,
            }],
        })
        .await
        .expect("prepare");
        let staged = orch.pending_context("evt_alpha").expect("staged");
        assert_eq!(staged.attendees_known.len(), 1);
        assert_eq!(staged.attendees_known[0].name, "Alex Chen");
        assert!(staged.agenda.is_none());
        assert!(staged.related_notes.is_empty());
    }

    #[tokio::test]
    async fn prepare_context_is_idempotent_and_does_not_clobber_attach_context() {
        // The rail re-fans `prepare_context` on every `ensureFresh`,
        // so a richer context the user already attached manually MUST
        // survive subsequent prepare calls. The orchestrator skips
        // the synthesizer when an entry already exists.
        let orch = LocalSessionOrchestrator::new();
        orch.attach_context(PreMeetingContextRequest {
            calendar_event_id: "evt_alpha".into(),
            context: ctx_with_agenda("rich agenda from user"),
        })
        .await
        .expect("attach");
        orch.prepare_context(heron_session::PrepareContextRequest {
            calendar_event_id: "evt_alpha".into(),
            attendees: vec![heron_session::AttendeeContext {
                name: "Alex Chen".into(),
                email: None,
                last_seen_in: None,
                relationship: None,
                notes: None,
            }],
        })
        .await
        .expect("prepare must succeed but skip");
        let staged = orch.pending_context("evt_alpha").expect("staged");
        assert_eq!(staged.agenda.as_deref(), Some("rich agenda from user"));
        assert!(
            staged.attendees_known.is_empty(),
            "manual attach had no attendees_known; prepare must not have overwritten it",
        );
    }

    #[tokio::test]
    async fn prepare_context_under_concurrent_attach_does_not_clobber() {
        // The rail fans `prepare_context` out in parallel after every
        // calendar load; meanwhile the user can click "Start with
        // context" and trigger an `attach_context` for the same id.
        // `PendingContexts::insert_if_absent` must hold the lock
        // across the existence probe and the insert so prepare losers
        // never overwrite a manual attach. Spam both calls in
        // parallel — at least one of the prepares lands either before
        // attach (legal: attach overwrites) or after (legal: prepare
        // is no-op). The invariant: when both have settled, the
        // staged context has the manual-attach agenda.
        let orch = Arc::new(LocalSessionOrchestrator::new());
        let attach = {
            let orch = Arc::clone(&orch);
            tokio::spawn(async move {
                orch.attach_context(PreMeetingContextRequest {
                    calendar_event_id: "evt_race".into(),
                    context: ctx_with_agenda("manual"),
                })
                .await
            })
        };
        let prepares: Vec<_> = (0..16)
            .map(|_| {
                let orch = Arc::clone(&orch);
                tokio::spawn(async move {
                    orch.prepare_context(heron_session::PrepareContextRequest {
                        calendar_event_id: "evt_race".into(),
                        attendees: Vec::new(),
                    })
                    .await
                })
            })
            .collect();
        attach.await.expect("attach join").expect("attach");
        for j in prepares {
            j.await.expect("prepare join").expect("prepare");
        }
        let staged = orch.pending_context("evt_race").expect("staged");
        // Manual attach is the always-overwrites caller, so its
        // agenda must be the final value regardless of interleaving:
        // - prepare→attach: attach overwrites the synth context.
        // - attach→prepare: insert_if_absent sees the attach entry
        //   and skips.
        assert_eq!(staged.agenda.as_deref(), Some("manual"));
    }

    #[tokio::test]
    async fn attach_context_after_prepare_overwrites_with_rich_context() {
        // Manual attach is always the latest-wins authority — pin
        // that prepare_context's idempotent skip doesn't accidentally
        // turn into an attach-context skip too.
        let orch = LocalSessionOrchestrator::new();
        orch.prepare_context(heron_session::PrepareContextRequest {
            calendar_event_id: "evt_alpha".into(),
            attendees: vec![heron_session::AttendeeContext {
                name: "Alex".into(),
                email: None,
                last_seen_in: None,
                relationship: None,
                notes: None,
            }],
        })
        .await
        .expect("prepare");
        orch.attach_context(PreMeetingContextRequest {
            calendar_event_id: "evt_alpha".into(),
            context: ctx_with_agenda("rich"),
        })
        .await
        .expect("attach");
        let staged = orch.pending_context("evt_alpha").expect("staged");
        assert_eq!(staged.agenda.as_deref(), Some("rich"));
        assert!(
            staged.attendees_known.is_empty(),
            "rich attach should fully replace the prepare's synth attendees",
        );
    }

    #[tokio::test]
    async fn prepare_context_rejects_empty_calendar_event_id() {
        let orch = LocalSessionOrchestrator::new();
        for cid in ["", "   "] {
            let err = orch
                .prepare_context(heron_session::PrepareContextRequest {
                    calendar_event_id: cid.into(),
                    attendees: Vec::new(),
                })
                .await
                .expect_err("empty id must be rejected");
            assert!(
                matches!(err, SessionError::Validation { .. }),
                "expected Validation, got {err:?}",
            );
        }
    }

    #[tokio::test]
    async fn start_capture_consumes_pending_context_for_matching_calendar_event_id() {
        let orch = LocalSessionOrchestrator::new();
        orch.attach_context(PreMeetingContextRequest {
            calendar_event_id: "evt_alpha".into(),
            context: ctx_with_agenda("kickoff"),
        })
        .await
        .expect("attach");
        let meeting = orch
            .start_capture(StartCaptureArgs {
                platform: Platform::Zoom,
                hint: None,
                calendar_event_id: Some("evt_alpha".into()),
            })
            .await
            .expect("start_capture");
        assert_eq!(meeting.calendar_event_id.as_deref(), Some("evt_alpha"));
        let applied = orch
            .applied_context(&meeting.id)
            .expect("context applied to active meeting");
        assert_eq!(applied.agenda.as_deref(), Some("kickoff"));
        // Consuming the pending entry empties the staging map.
        assert!(orch.pending_context("evt_alpha").is_none());
    }

    #[tokio::test]
    async fn start_capture_without_calendar_event_id_does_not_consume_context() {
        let orch = LocalSessionOrchestrator::new();
        orch.attach_context(PreMeetingContextRequest {
            calendar_event_id: "evt_alpha".into(),
            context: ctx_with_agenda("kickoff"),
        })
        .await
        .expect("attach");
        let meeting = orch
            .start_capture(StartCaptureArgs {
                platform: Platform::Zoom,
                hint: None,
                calendar_event_id: None,
            })
            .await
            .expect("start_capture");
        assert!(orch.applied_context(&meeting.id).is_none());
        assert!(orch.pending_context("evt_alpha").is_some());
    }

    #[tokio::test]
    async fn start_capture_with_unmatched_calendar_event_id_attaches_no_context() {
        let orch = LocalSessionOrchestrator::new();
        orch.attach_context(PreMeetingContextRequest {
            calendar_event_id: "evt_alpha".into(),
            context: ctx_with_agenda("kickoff"),
        })
        .await
        .expect("attach");
        let meeting = orch
            .start_capture(StartCaptureArgs {
                platform: Platform::Zoom,
                hint: None,
                calendar_event_id: Some("evt_other".into()),
            })
            .await
            .expect("start_capture");
        assert_eq!(meeting.calendar_event_id.as_deref(), Some("evt_other"));
        assert!(orch.applied_context(&meeting.id).is_none());
        // The pending entry for the original id is untouched.
        assert!(orch.pending_context("evt_alpha").is_some());
    }

    #[tokio::test]
    async fn attach_and_start_capture_normalize_whitespace_symmetrically() {
        // A caller that whitespace-pads either side of the id on
        // either route still hits the staged entry. Without symmetric
        // trimming, attach would store under "evt_alpha" while
        // start_capture would look up " evt_alpha " and miss.
        let orch = LocalSessionOrchestrator::new();
        orch.attach_context(PreMeetingContextRequest {
            calendar_event_id: "  evt_alpha\n".into(),
            context: ctx_with_agenda("trimmed"),
        })
        .await
        .expect("attach");
        let meeting = orch
            .start_capture(StartCaptureArgs {
                platform: Platform::Zoom,
                hint: None,
                calendar_event_id: Some("\tevt_alpha ".into()),
            })
            .await
            .expect("start_capture");
        assert_eq!(meeting.calendar_event_id.as_deref(), Some("evt_alpha"));
        let applied = orch
            .applied_context(&meeting.id)
            .expect("context consumed despite whitespace");
        assert_eq!(applied.agenda.as_deref(), Some("trimmed"));
    }

    #[tokio::test]
    async fn start_capture_validates_calendar_event_id() {
        let orch = LocalSessionOrchestrator::new();
        for cid in ["", "   "] {
            let err = orch
                .start_capture(StartCaptureArgs {
                    platform: Platform::Zoom,
                    hint: None,
                    calendar_event_id: Some(cid.into()),
                })
                .await
                .expect_err("empty id must be rejected on start_capture too");
            assert!(
                matches!(err, SessionError::Validation { .. }),
                "expected Validation, got {err:?}",
            );
        }
        let huge = "a".repeat(MAX_CALENDAR_EVENT_ID_BYTES + 1);
        let err = orch
            .start_capture(StartCaptureArgs {
                platform: Platform::Zoom,
                hint: None,
                calendar_event_id: Some(huge),
            })
            .await
            .expect_err("oversized id must be rejected on start_capture too");
        assert!(
            matches!(err, SessionError::Validation { .. }),
            "expected Validation, got {err:?}",
        );
    }

    #[tokio::test]
    async fn pending_contexts_evict_oldest_at_cap() {
        // The map is bounded at MAX_PENDING_CONTEXTS to defend against
        // a caller spraying unique ids without ever calling
        // start_capture. At the cap, a fresh attach evicts the oldest
        // entry FIFO; an existing key keeps its slot when overwritten.
        let orch = LocalSessionOrchestrator::new();
        for i in 0..MAX_PENDING_CONTEXTS {
            orch.attach_context(PreMeetingContextRequest {
                calendar_event_id: format!("evt_{i}"),
                context: ctx_with_agenda(&format!("a{i}")),
            })
            .await
            .expect("attach within cap");
        }
        // At cap — every prior id is still resident.
        assert!(orch.pending_context("evt_0").is_some());
        assert!(
            orch.pending_context(&format!("evt_{}", MAX_PENDING_CONTEXTS - 1))
                .is_some(),
        );

        // One past the cap: the oldest entry is evicted, the newest
        // is resident.
        orch.attach_context(PreMeetingContextRequest {
            calendar_event_id: "evt_overflow".into(),
            context: ctx_with_agenda("overflow"),
        })
        .await
        .expect("attach past cap");
        assert!(orch.pending_context("evt_0").is_none());
        assert!(orch.pending_context("evt_overflow").is_some());
        assert!(orch.pending_context("evt_1").is_some());
    }

    #[tokio::test]
    async fn overwriting_pending_context_does_not_reset_eviction_clock() {
        // When the same id is re-attached, FIFO eviction order should
        // treat it as if the original insert is what counts —
        // overwriting late shouldn't push older entries off the cliff.
        let orch = LocalSessionOrchestrator::new();
        orch.attach_context(PreMeetingContextRequest {
            calendar_event_id: "evt_0".into(),
            context: ctx_with_agenda("first"),
        })
        .await
        .expect("attach");
        for i in 1..MAX_PENDING_CONTEXTS {
            orch.attach_context(PreMeetingContextRequest {
                calendar_event_id: format!("evt_{i}"),
                context: ctx_with_agenda(&format!("a{i}")),
            })
            .await
            .expect("attach");
        }
        // Overwrite evt_0 — its FIFO position is unchanged.
        orch.attach_context(PreMeetingContextRequest {
            calendar_event_id: "evt_0".into(),
            context: ctx_with_agenda("second"),
        })
        .await
        .expect("overwrite");
        // Push past cap — evt_0 (oldest) should still be evicted.
        orch.attach_context(PreMeetingContextRequest {
            calendar_event_id: "evt_overflow".into(),
            context: ctx_with_agenda("overflow"),
        })
        .await
        .expect("attach past cap");
        assert!(orch.pending_context("evt_0").is_none());
        assert!(orch.pending_context("evt_1").is_some());
    }

    // ── live session wiring (gap #1 + pre-meeting context hand-off) ───

    use crate::live_session::LiveSessionError;
    use heron_bot::BotId as LiveBotId;
    use heron_realtime::SessionId as RealtimeId;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};

    /// Test factory that records calls + returns a stub session
    /// implementing [`DynLiveSession`]. Lets the wiring tests verify
    /// `start_capture` -> factory -> attach, and `end_meeting` ->
    /// shutdown, without spinning up real Recall / OpenAI / bridge.
    struct RecordingFactory {
        calls: Mutex<Vec<LiveSessionStartArgs>>,
        fail: AtomicBool,
        shutdowns: Arc<AtomicUsize>,
    }

    impl RecordingFactory {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                fail: AtomicBool::new(false),
                shutdowns: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn fail_next(&self) {
            self.fail.store(true, AtomicOrdering::SeqCst);
        }

        fn calls_snapshot(&self) -> Vec<LiveSessionStartArgs> {
            lock_or_recover(&self.calls).clone()
        }
    }

    struct StubLiveSession {
        meeting_id: MeetingId,
        bot_id: LiveBotId,
        realtime_session: RealtimeId,
        shutdowns: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl DynLiveSession for StubLiveSession {
        fn meeting_id(&self) -> MeetingId {
            self.meeting_id
        }
        fn bot_id(&self) -> LiveBotId {
            self.bot_id
        }
        fn realtime_session(&self) -> RealtimeId {
            self.realtime_session
        }
        fn bridge_health(&self) -> heron_bridge::BridgeHealth {
            heron_bridge::BridgeHealth {
                aec_tracking: true,
                jitter_ms: 0.0,
                recent_drops: 0,
            }
        }
        async fn shutdown(self: Box<Self>) -> Result<(), LiveSessionError> {
            self.shutdowns.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(())
        }
    }

    #[async_trait]
    impl LiveSessionFactory for RecordingFactory {
        async fn start(
            &self,
            args: LiveSessionStartArgs,
        ) -> Result<Box<dyn DynLiveSession>, LiveSessionError> {
            lock_or_recover(&self.calls).push(args.clone());
            if self.fail.swap(false, AtomicOrdering::SeqCst) {
                return Err(LiveSessionError::PolicyValidation(
                    heron_policy::ValidationError::EmptyNotifyDestination,
                ));
            }
            Ok(Box::new(StubLiveSession {
                meeting_id: args.meeting_id,
                bot_id: LiveBotId::now_v7(),
                realtime_session: RealtimeId::now_v7(),
                shutdowns: Arc::clone(&self.shutdowns),
            }))
        }
    }

    #[tokio::test]
    async fn start_capture_invokes_live_session_factory_and_attaches_session() {
        // Pin the headline behavior of gap #1: when a factory is
        // installed, `start_capture` calls it, attaches the live
        // session to the active meeting, and `end_meeting` tears it
        // down. Without this assertion the wiring is invisible.
        let factory = Arc::new(RecordingFactory::new());
        let shutdowns = Arc::clone(&factory.shutdowns);
        let orch = Builder::default()
            .live_session_factory(Arc::clone(&factory) as Arc<dyn LiveSessionFactory>)
            .build();

        let started = orch
            .start_capture(StartCaptureArgs {
                platform: Platform::Zoom,
                hint: Some("https://zoom.us/j/123".into()),
                calendar_event_id: None,
            })
            .await
            .expect("start_capture");
        assert!(orch.has_live_session(&started.id));
        let calls = factory.calls_snapshot();
        assert_eq!(
            calls.len(),
            1,
            "factory called exactly once per start_capture",
        );
        assert_eq!(calls[0].meeting_id, started.id);
        assert_eq!(calls[0].bot.meeting_url, "https://zoom.us/j/123");

        orch.end_meeting(&started.id).await.expect("end_meeting");
        // The shutdown happens on a background finalizer task; drain
        // it through the explicit orchestrator shutdown path before
        // asserting.
        orch.shutdown().await.expect("orch shutdown");
        assert_eq!(
            shutdowns.load(AtomicOrdering::SeqCst),
            1,
            "live session shutdown invoked exactly once",
        );
    }

    #[tokio::test]
    async fn start_capture_falls_back_to_v1_when_live_session_factory_errors() {
        // Gap #1 acceptance criterion: factory failure (e.g. missing
        // OPENAI_API_KEY, vendor flake) MUST NOT fail the request.
        // The v1 vault-backed path remains a fallback. Pin both
        // (a) the meeting still starts and (b) no live session is
        // attached.
        let factory = Arc::new(RecordingFactory::new());
        factory.fail_next();
        let orch = Builder::default()
            .live_session_factory(Arc::clone(&factory) as Arc<dyn LiveSessionFactory>)
            .build();
        let started = orch
            .start_capture(StartCaptureArgs {
                platform: Platform::Zoom,
                hint: None,
                calendar_event_id: None,
            })
            .await
            .expect("start_capture must succeed despite factory failure");
        assert!(matches!(started.status, MeetingStatus::Recording));
        assert!(
            !orch.has_live_session(&started.id),
            "no live session attached on factory failure",
        );
    }

    #[tokio::test]
    async fn live_session_args_carry_attached_pre_meeting_context() {
        // Pre-meeting-context consumer-side: the staged
        // `PreMeetingContext` must flow into `LiveSessionStartArgs`
        // so the realtime backend sees the agenda / attendees /
        // briefing in its system prompt and the bot driver sees the
        // same context. Without this, calling `attach_context` is
        // invisible to the v2 stack.
        let factory = Arc::new(RecordingFactory::new());
        let orch = Builder::default()
            .live_session_factory(Arc::clone(&factory) as Arc<dyn LiveSessionFactory>)
            .build();
        orch.attach_context(PreMeetingContextRequest {
            calendar_event_id: "evt_alpha".into(),
            context: heron_session::PreMeetingContext {
                agenda: Some("ship the alpha".into()),
                attendees_known: vec![heron_session::AttendeeContext {
                    name: "Ada".into(),
                    email: Some("ada@example.com".into()),
                    last_seen_in: None,
                    relationship: None,
                    notes: None,
                }],
                related_notes: vec![],
                prior_decisions: vec![],
                user_briefing: Some("focus on the wiring story".into()),
            },
        })
        .await
        .expect("attach");
        orch.start_capture(StartCaptureArgs {
            platform: Platform::Zoom,
            hint: None,
            calendar_event_id: Some("evt_alpha".into()),
        })
        .await
        .expect("start");

        let calls = factory.calls_snapshot();
        assert_eq!(calls.len(), 1);
        let prompt = &calls[0].realtime.system_prompt;
        assert!(
            prompt.contains("ship the alpha"),
            "agenda must reach the realtime system prompt; got: {prompt}",
        );
        assert!(
            prompt.contains("Ada"),
            "attendee must reach the realtime system prompt; got: {prompt}",
        );
        assert!(
            prompt.contains("focus on the wiring story"),
            "briefing must reach the realtime system prompt; got: {prompt}",
        );
        let bot_ctx = &calls[0].bot.context;
        assert_eq!(bot_ctx.agenda.as_deref(), Some("ship the alpha"));
        assert_eq!(bot_ctx.attendees_known.len(), 1);
        assert_eq!(bot_ctx.attendees_known[0].name, "Ada");
    }

    #[tokio::test]
    async fn start_capture_without_factory_does_not_attach_live_session() {
        // Regression guard: every existing test path constructs the
        // orchestrator without a factory and expects the v1 substrate
        // behavior. Confirm that staying on the default constructor
        // leaves `live_session: None` so those tests don't change
        // shape.
        let orch = LocalSessionOrchestrator::new();
        let started = orch
            .start_capture(StartCaptureArgs {
                platform: Platform::Zoom,
                hint: None,
                calendar_event_id: None,
            })
            .await
            .expect("start_capture");
        assert!(!orch.has_live_session(&started.id));
    }

    /// Slow-start factory that lets the test deterministically drive
    /// the race between `start_capture` finishing the factory call
    /// and a concurrent `end_meeting` removing the active entry.
    /// Without this, the orphan-cleanup branch in `start_capture` is
    /// only reachable by chance.
    struct GatedFactory {
        gate: tokio::sync::Notify,
        shutdowns: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl LiveSessionFactory for GatedFactory {
        async fn start(
            &self,
            args: LiveSessionStartArgs,
        ) -> Result<Box<dyn DynLiveSession>, LiveSessionError> {
            self.gate.notified().await;
            Ok(Box::new(StubLiveSession {
                meeting_id: args.meeting_id,
                bot_id: LiveBotId::now_v7(),
                realtime_session: RealtimeId::now_v7(),
                shutdowns: Arc::clone(&self.shutdowns),
            }))
        }
    }

    #[tokio::test]
    async fn live_session_orphan_is_torn_down_when_meeting_ends_during_factory_call() {
        // Race the orphan-cleanup branch: `end_meeting` removes the
        // active entry before the factory returns, so the late
        // session has no home. The orchestrator must shut it down
        // rather than leak the vendor bot.
        let factory = Arc::new(GatedFactory {
            gate: tokio::sync::Notify::new(),
            shutdowns: Arc::new(AtomicUsize::new(0)),
        });
        let shutdowns = Arc::clone(&factory.shutdowns);
        let orch = Arc::new(
            Builder::default()
                .live_session_factory(Arc::clone(&factory) as Arc<dyn LiveSessionFactory>)
                .build(),
        );
        let orch_clone = Arc::clone(&orch);
        let start = tokio::spawn(async move {
            orch_clone
                .start_capture(StartCaptureArgs {
                    platform: Platform::Zoom,
                    hint: None,
                    calendar_event_id: None,
                })
                .await
                .expect("start_capture")
        });

        // Wait for the active entry to appear, then end the meeting
        // before releasing the factory gate. The pending entry has
        // `live_session: None` because the factory has not returned
        // yet; `end_meeting` removes it cleanly. Snapshot the id
        // outside the `.await` so the `MutexGuard` (sync, !Send)
        // is dropped before yielding.
        let id = loop {
            let snapshot = lock_or_recover(&orch.active_meetings)
                .keys()
                .next()
                .copied();
            if let Some(id) = snapshot {
                break id;
            }
            tokio::task::yield_now().await;
        };
        orch.end_meeting(&id).await.expect("end_meeting");
        // Now release the factory; `start_capture` will see the
        // entry has vanished and tear the orphan down.
        factory.gate.notify_one();
        let _ = start.await.expect("start_capture join");
        // Drain finalizers (the v1 finalizer task) AND give the
        // orphan-cleanup `.await` a chance to run.
        orch.shutdown().await.expect("orch shutdown");
        assert_eq!(
            shutdowns.load(AtomicOrdering::SeqCst),
            1,
            "orphan live session shutdown invoked exactly once",
        );
    }

    #[test]
    fn pre_meeting_briefing_for_v1_is_none_without_context() {
        let id = MeetingId::now_v7();
        assert!(pre_meeting_briefing_for_v1(None, id).is_none());
    }

    #[test]
    fn pre_meeting_briefing_for_v1_is_none_for_empty_context() {
        // A staged-but-empty `PreMeetingContext` (every field default)
        // should not produce a stranded `## Pre-meeting context`
        // header in the v1 summarizer prompt — the heron-llm template
        // already suppresses empty briefings, but we suppress earlier
        // here too so callers can rely on `Some`/`None` instead of
        // re-checking emptiness.
        let id = MeetingId::now_v7();
        let ctx = PreMeetingContext::default();
        assert!(pre_meeting_briefing_for_v1(Some(&ctx), id).is_none());
    }

    #[test]
    fn pre_meeting_briefing_for_v1_renders_populated_context() {
        let id = MeetingId::now_v7();
        let ctx = PreMeetingContext {
            agenda: Some("Q3 launch readiness review".into()),
            attendees_known: vec![heron_session::AttendeeContext {
                name: "Alice".into(),
                email: Some("alice@example.com".into()),
                last_seen_in: None,
                relationship: Some("CFO".into()),
                notes: None,
            }],
            related_notes: vec!["meetings/2026-04-12.md".into()],
            // `prior_decisions` is dropped by the heron_bot translation
            // (the bot shape doesn't carry it); we still exercise it
            // here so the test pins the lossy translation rather than
            // regressing silently if a future render adds support.
            prior_decisions: Vec::new(),
            user_briefing: Some("Alice will push for slipping the date.".into()),
        };
        let rendered =
            pre_meeting_briefing_for_v1(Some(&ctx), id).expect("populated context renders");
        assert!(rendered.contains("Q3 launch readiness review"));
        assert!(rendered.contains("Alice"));
        assert!(rendered.contains("Alice will push for slipping the date."));
    }
}
