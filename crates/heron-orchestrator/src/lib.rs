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

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, NaiveTime, TimeZone, Utc};
use heron_event::{EventBus, ReplayCache};
use heron_event_http::{DEFAULT_REPLAY_WINDOW, InMemoryReplayCache};
use heron_session::{
    AttendeeContext, CalendarEvent, ComponentState, EventPayload, Health, HealthComponent,
    HealthComponents, HealthStatus, IdentifierKind, ListMeetingsPage, ListMeetingsQuery, Meeting,
    MeetingId, MeetingStatus, Participant, Platform, PreMeetingContextRequest, SessionError,
    SessionEventBus, SessionOrchestrator, StartCaptureArgs, Summary, SummaryLifecycle, Transcript,
    TranscriptLifecycle, TranscriptSegment,
};
use heron_vault::{
    CalendarReader, EventKitCalendarReader, VaultError, epoch_seconds_to_utc, read_note,
};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use uuid::Uuid;

/// Namespace UUID seeded into [`uuid::Uuid::new_v5`] when deriving
/// a `MeetingId` from a vault-relative note path. The byte pattern
/// is arbitrary but FIXED — changing it would re-key every meeting
/// in every consumer cache and break `Last-Event-ID` resume
/// expectations. If a future change really needs a different
/// derivation, bump it AND emit a synthetic `daemon.error` so
/// consumers know to invalidate their caches.
pub const MEETING_ID_NAMESPACE: Uuid = Uuid::from_bytes([
    0x68, 0x65, 0x72, 0x6f, 0x6e, 0x6d, 0x74, 0x67, 0x21, 0x21, 0x21, 0x21, 0x21, 0x21, 0x21, 0x21,
]);

/// Cap on a single JSONL transcript line. A turn is a few hundred
/// bytes typically; 1 MiB bounds the OOM blast radius for a
/// malformed transcript that lost its newlines and presents as one
/// gigantic line.
const MAX_TRANSCRIPT_LINE_BYTES: usize = 1024 * 1024;

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
#[derive(Clone)]
pub struct Builder {
    bus_capacity: usize,
    cache_capacity: usize,
    cache_window: Duration,
    vault_root: Option<PathBuf>,
    calendar: Option<Arc<dyn CalendarReader>>,
}

impl std::fmt::Debug for Builder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Builder")
            .field("bus_capacity", &self.bus_capacity)
            .field("cache_capacity", &self.cache_capacity)
            .field("cache_window", &self.cache_window)
            .field("vault_root", &self.vault_root)
            .field("calendar", &"<Arc<dyn CalendarReader>>")
            .finish()
    }
}

impl Default for Builder {
    fn default() -> Self {
        Self {
            bus_capacity: DEFAULT_BUS_CAPACITY,
            cache_capacity: DEFAULT_CACHE_CAPACITY,
            cache_window: DEFAULT_REPLAY_WINDOW,
            vault_root: None,
            calendar: None,
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

    /// Configure the on-disk vault root that the read endpoints
    /// (`list_meetings` / `get_meeting` / `read_transcript` /
    /// `read_summary` / `audio_path`) scan for `<vault>/meetings/*.md`
    /// notes. Without this, every read method returns
    /// `NotYetImplemented` (the substrate-only behavior).
    pub fn vault_root(mut self, root: PathBuf) -> Self {
        self.vault_root = Some(root);
        self
    }

    /// Inject a custom [`CalendarReader`]. Tests use this to bypass
    /// the EventKit Swift bridge (which on linux CI doesn't exist
    /// and on macOS without TCC blocks waiting for the permission
    /// prompt). Defaults to [`EventKitCalendarReader`] when unset.
    pub fn calendar(mut self, reader: Arc<dyn CalendarReader>) -> Self {
        self.calendar = Some(reader);
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
        let calendar = self
            .calendar
            .unwrap_or_else(|| Arc::new(EventKitCalendarReader));
        LocalSessionOrchestrator {
            bus,
            cache,
            vault_root: self.vault_root,
            calendar,
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
    // Read endpoints scan the configured vault when `vault_root` is
    // `Some`, otherwise fall through to `NotYetImplemented` — same
    // shape as the substrate-only behavior phase 81 shipped, so
    // tests that don't configure a vault still get the original
    // surface.

    async fn list_meetings(&self, q: ListMeetingsQuery) -> Result<ListMeetingsPage, SessionError> {
        let Some(root) = self.vault_root.as_deref() else {
            return Err(SessionError::NotYetImplemented);
        };
        list_meetings_impl(root, q)
    }

    async fn get_meeting(&self, id: &MeetingId) -> Result<Meeting, SessionError> {
        let Some(root) = self.vault_root.as_deref() else {
            return Err(SessionError::NotYetImplemented);
        };
        let path = find_note_path_by_id(root, id)?;
        meeting_from_note(root, &path)
    }

    async fn start_capture(&self, _args: StartCaptureArgs) -> Result<Meeting, SessionError> {
        Err(SessionError::NotYetImplemented)
    }

    async fn end_meeting(&self, _id: &MeetingId) -> Result<(), SessionError> {
        Err(SessionError::NotYetImplemented)
    }

    async fn read_transcript(&self, id: &MeetingId) -> Result<Transcript, SessionError> {
        let Some(root) = self.vault_root.as_deref() else {
            return Err(SessionError::NotYetImplemented);
        };
        let path = find_note_path_by_id(root, id)?;
        let (frontmatter, _) = read_note(&path).map_err(vault_to_session_err)?;
        let transcript_path = resolve_vault_path(root, &frontmatter.transcript, "transcript")?;
        let segments = read_transcript_segments(&transcript_path)?;
        Ok(Transcript {
            meeting_id: *id,
            status: TranscriptLifecycle::Complete,
            language: None,
            segments,
        })
    }

    async fn read_summary(&self, id: &MeetingId) -> Result<Option<Summary>, SessionError> {
        let Some(root) = self.vault_root.as_deref() else {
            return Err(SessionError::NotYetImplemented);
        };
        let path = find_note_path_by_id(root, id)?;
        let (frontmatter, body) = read_note(&path).map_err(vault_to_session_err)?;
        let action_items = frontmatter
            .action_items
            .iter()
            .map(|a| heron_session::ActionItem {
                text: a.text.clone(),
                owner: if a.owner.is_empty() {
                    None
                } else {
                    Some(a.owner.clone())
                },
                due: a.due.as_deref().and_then(parse_iso_date),
            })
            .collect();
        Ok(Some(Summary {
            meeting_id: *id,
            generated_at: started_at_from_frontmatter(&frontmatter),
            text: body,
            action_items,
            llm_provider: None,
            llm_model: None,
        }))
    }

    async fn audio_path(&self, id: &MeetingId) -> Result<PathBuf, SessionError> {
        let Some(root) = self.vault_root.as_deref() else {
            return Err(SessionError::NotYetImplemented);
        };
        let path = find_note_path_by_id(root, id)?;
        let (frontmatter, _) = read_note(&path).map_err(vault_to_session_err)?;
        let recording = resolve_vault_path(root, &frontmatter.recording, "recording")?;
        if !recording.exists() {
            // Don't echo the resolved host path into the wire error
            // — keeps a vault-layout exfil channel closed even on
            // an authenticated request. The meeting id is sufficient
            // for the consumer to act on.
            return Err(SessionError::NotFound {
                what: format!("audio for meeting {id}"),
            });
        }
        Ok(recording)
    }

    async fn list_upcoming_calendar(
        &self,
        from: Option<DateTime<Utc>>,
        to: Option<DateTime<Utc>>,
        limit: Option<u32>,
    ) -> Result<Vec<CalendarEvent>, SessionError> {
        let now = Utc::now();
        let from = from.unwrap_or(now);
        let to = to.unwrap_or_else(|| from + chrono::Duration::days(7));
        let raw = self
            .calendar
            .read_window(from, to)
            .map_err(|e| match e {
                heron_vault::CalendarError::Denied => SessionError::PermissionMissing {
                    permission: "calendar",
                },
                other => SessionError::VaultLocked {
                    detail: format!("calendar read failed: {other}"),
                },
            })?
            .unwrap_or_default();
        let cap = limit.unwrap_or(20).min(100) as usize;
        let events = raw
            .into_iter()
            .take(cap)
            .map(|ev| CalendarEvent {
                // EventKit doesn't yet expose a stable per-event id
                // through the Swift bridge; until it does, synthesize
                // a deterministic id from `(start, end, title)` so a
                // future `attach_context` impl can correlate. Long
                // titles are SHA-collision-resistant — `format!` of
                // the raw f64 bits + full title string is enough at
                // this scope; collision-free across realistic vaults.
                id: format!(
                    "synth_{}_{}_{}",
                    ev.start.to_bits(),
                    ev.end.to_bits(),
                    ev.title
                ),
                title: ev.title,
                start: epoch_seconds_to_utc(ev.start),
                end: epoch_seconds_to_utc(ev.end),
                attendees: ev
                    .attendees
                    .into_iter()
                    .map(|a| AttendeeContext {
                        name: a.name,
                        email: Some(a.email).filter(|s| !s.is_empty()),
                        last_seen_in: None,
                        relationship: None,
                        notes: None,
                    })
                    .collect(),
                meeting_url: None,
                related_meetings: Vec::new(),
            })
            .collect();
        Ok(events)
    }

    async fn attach_context(&self, _req: PreMeetingContextRequest) -> Result<(), SessionError> {
        // Storage layer for pre-meeting context lands with the FSM-
        // merge PR (the orchestrator that consumes the context at
        // capture-start time also owns the storage seam).
        Err(SessionError::NotYetImplemented)
    }

    async fn health(&self) -> Health {
        // Substrate-only baseline (every component `Down + "not yet
        // wired"`). When a `vault_root` is configured, flip the
        // `vault` component to a real path-existence probe; the
        // rest stay honest until their FSM-merge wires them.
        // EventKit access is NOT probed here — `calendar_has_access`
        // delegates to a Swift FFI that on a CI runner without
        // pre-granted TCC blocks waiting for the system permission
        // prompt. Real EventKit access surfaces on
        // `/v1/calendar/upcoming`, which already returns 503 on
        // `Denied`; that's the right contract for liveness to defer
        // to.
        let vault = match self.vault_root.as_deref() {
            Some(root) if root.exists() => HealthComponent {
                state: ComponentState::Ok,
                message: Some(format!("vault root: {}", root.display())),
                last_check: Some(Utc::now()),
            },
            Some(root) => HealthComponent {
                state: ComponentState::PermissionMissing,
                message: Some(format!("vault root not found: {}", root.display())),
                last_check: Some(Utc::now()),
            },
            None => not_yet_wired("vault writer"),
        };
        Health {
            status: HealthStatus::Degraded,
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            components: HealthComponents {
                capture: not_yet_wired("audio capture"),
                whisperkit: not_yet_wired("speech recognition"),
                vault,
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

// ── vault read helpers ────────────────────────────────────────────────

fn list_meetings_impl(
    vault_root: &Path,
    q: ListMeetingsQuery,
) -> Result<ListMeetingsPage, SessionError> {
    let paths = note_paths_newest_first(vault_root)?;
    let limit = q.limit.unwrap_or(50).min(200) as usize;
    let after = q.cursor.as_deref();
    let mut started_after = after.is_none();
    let mut items = Vec::with_capacity(limit);
    let mut next_cursor: Option<String> = None;
    let mut last_kept_rel: Option<String> = None;
    for path in paths {
        let rel = path
            .strip_prefix(vault_root)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| path.clone());
        let rel_str = rel.to_string_lossy().to_string();
        if !started_after {
            if Some(rel_str.as_str()) == after {
                started_after = true;
            }
            continue;
        }
        let meeting = match meeting_from_note(vault_root, &path) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "skipping malformed note in list_meetings",
                );
                continue;
            }
        };
        if let Some(since) = q.since
            && meeting.started_at < since
        {
            continue;
        }
        if let Some(status) = q.status
            && meeting.status != status
        {
            continue;
        }
        if let Some(platform) = q.platform
            && meeting.platform != platform
        {
            continue;
        }
        if items.len() == limit {
            next_cursor = last_kept_rel.clone();
            break;
        }
        items.push(meeting);
        last_kept_rel = Some(rel_str);
    }
    Ok(ListMeetingsPage { items, next_cursor })
}

fn note_paths_newest_first(vault_root: &Path) -> Result<Vec<PathBuf>, SessionError> {
    let dir = vault_root.join("meetings");
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| SessionError::VaultLocked {
            detail: format!("read_dir({}): {e}", dir.display()),
        })?
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_type().map(|t| t.is_file()).unwrap_or(false)
                && e.path().extension().and_then(|s| s.to_str()) == Some("md")
        })
        .map(|e| e.path())
        .collect();
    // Note filenames are `YYYY-MM-DD-HHMM <slug>.md` per `docs/plan.md`
    // §3.2, so a lex-descending sort IS a date-descending sort.
    entries.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    Ok(entries)
}

/// Linear scan for the note whose derived `MeetingId` matches `id`.
/// Used by every per-meeting read endpoint. Replaceable with an
/// in-memory index when capture lifecycle ships and the bus starts
/// publishing events (the index is the natural piggyback on the
/// recorder).
fn find_note_path_by_id(vault_root: &Path, id: &MeetingId) -> Result<PathBuf, SessionError> {
    note_paths_newest_first(vault_root)?
        .into_iter()
        .find(|p| derive_meeting_id(vault_root, p) == *id)
        .ok_or_else(|| SessionError::NotFound {
            what: format!("meeting {id}"),
        })
}

/// Resolve a frontmatter path field against the vault root,
/// rejecting absolute paths and `..` traversal. Without this
/// `read_transcript` and `audio_path` are file-read primitives over
/// loopback-auth.
fn resolve_vault_path(
    vault_root: &Path,
    candidate: &Path,
    field: &'static str,
) -> Result<PathBuf, SessionError> {
    if candidate.is_absolute() {
        return Err(SessionError::Validation {
            detail: format!("{field} path must be vault-relative"),
        });
    }
    // Canonicalize the vault root FIRST so the prefix check below
    // compares apples to apples — on macOS, `/var/...` canonicalizes
    // to `/private/var/...` (system symlink). Without this, a non-
    // canonical vault_root + non-canonical candidate would fail the
    // canonical prefix check, mistakenly rejecting a perfectly-
    // relative path.
    let root_canonical = vault_root
        .canonicalize()
        .unwrap_or_else(|_| vault_root.to_path_buf());
    let safe_relative = normalize_no_traverse(candidate)?;
    let joined = root_canonical.join(&safe_relative);
    let resolved = if joined.exists() {
        joined
            .canonicalize()
            .map_err(|e| SessionError::VaultLocked {
                detail: format!("canonicalize {field}: {e}"),
            })?
    } else {
        joined
    };
    if !resolved.starts_with(&root_canonical) {
        return Err(SessionError::Validation {
            detail: format!("{field} path escapes vault"),
        });
    }
    Ok(resolved)
}

fn normalize_no_traverse(path: &Path) -> Result<PathBuf, SessionError> {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                return Err(SessionError::Validation {
                    detail: "path contains '..' which is forbidden".to_owned(),
                });
            }
            Component::Normal(_)
            | Component::RootDir
            | Component::Prefix(_)
            | Component::CurDir => {
                out.push(c.as_os_str());
            }
        }
    }
    Ok(out)
}

fn derive_meeting_id(vault_root: &Path, note_path: &Path) -> MeetingId {
    let rel = note_path.strip_prefix(vault_root).unwrap_or(note_path);
    let bytes = rel.as_os_str().as_encoded_bytes();
    MeetingId(Uuid::new_v5(&MEETING_ID_NAMESPACE, bytes))
}

fn meeting_from_note(vault_root: &Path, path: &Path) -> Result<Meeting, SessionError> {
    let (fm, body) = read_note(path).map_err(vault_to_session_err)?;
    let id = derive_meeting_id(vault_root, path);
    let started_at = started_at_from_frontmatter(&fm);
    let ended_at = Some(started_at + chrono::Duration::minutes(fm.duration_min as i64));
    let participants = fm
        .attendees
        .iter()
        .map(|a| Participant {
            display_name: a.name.clone(),
            identifier_kind: IdentifierKind::Fallback,
            is_user: false,
        })
        .collect();
    let transcript_resolved = resolve_vault_path(vault_root, &fm.transcript, "transcript").ok();
    let transcript_status = match transcript_resolved {
        Some(p) if p.exists() => TranscriptLifecycle::Complete,
        _ => TranscriptLifecycle::Failed,
    };
    let summary_status = if body.trim().is_empty() {
        SummaryLifecycle::Pending
    } else {
        SummaryLifecycle::Ready
    };
    Ok(Meeting {
        id,
        // Notes are only finalized for completed meetings, so the
        // status is always `Done`. A meeting still in `Recording`
        // doesn't have a finalized note on disk for us to surface.
        status: MeetingStatus::Done,
        platform: platform_from_source_app(&fm.source_app),
        title: fm.company.clone(),
        calendar_event_id: None,
        started_at,
        ended_at,
        duration_secs: Some((fm.duration_min as u64) * 60),
        participants,
        transcript_status,
        summary_status,
    })
}

fn platform_from_source_app(source_app: &str) -> Platform {
    let s = source_app.to_ascii_lowercase();
    if s.contains("zoom") {
        Platform::Zoom
    } else if s.contains("meet.google") || s.contains("googlemeet") || s.contains("google_meet") {
        Platform::GoogleMeet
    } else if s.contains("teams") || s.contains("microsoft") {
        Platform::MicrosoftTeams
    } else if s.contains("webex") {
        Platform::Webex
    } else {
        if !source_app.is_empty() {
            tracing::warn!(
                source_app,
                "unrecognized source_app; defaulting to Platform::Zoom"
            );
        }
        Platform::Zoom
    }
}

fn started_at_from_frontmatter(fm: &heron_types::Frontmatter) -> DateTime<Utc> {
    let date: NaiveDate = fm.date;
    let time = NaiveTime::parse_from_str(&fm.start, "%H:%M")
        .or_else(|_| NaiveTime::parse_from_str(&fm.start, "%H:%M:%S"))
        .unwrap_or_else(|_| NaiveTime::from_hms_opt(0, 0, 0).unwrap_or_default());
    let naive = date.and_time(time);
    // Frontmatter has no explicit timezone field. The vault writer
    // records meetings in the user's local clock (the
    // `YYYY-MM-DD-HHMM` filename matches the user's wall clock at
    // capture time), so the API contract is "local time projected
    // to UTC." Earliest mapping wins on the autumn DST overlap;
    // the gap (spring) falls back to naive-as-UTC with a warn so a
    // single missing-hour frontmatter doesn't fail the whole list.
    use chrono::Local;
    use chrono::offset::LocalResult;
    match Local.from_local_datetime(&naive) {
        LocalResult::Single(local) => local.with_timezone(&Utc),
        LocalResult::Ambiguous(earliest, _latest) => earliest.with_timezone(&Utc),
        LocalResult::None => {
            tracing::warn!(
                date = %fm.date,
                start = %fm.start,
                "frontmatter datetime in DST gap; treating naive value as UTC",
            );
            Utc.from_utc_datetime(&naive)
        }
    }
}

fn read_transcript_segments(path: &Path) -> Result<Vec<TranscriptSegment>, SessionError> {
    use std::io::{BufRead, Read};
    if !path.exists() {
        return Err(SessionError::NotFound {
            what: format!("transcript file: {}", path.display()),
        });
    }
    let file = std::fs::File::open(path).map_err(|e| SessionError::VaultLocked {
        detail: format!("open transcript {}: {e}", path.display()),
    })?;
    let mut reader = std::io::BufReader::new(file);
    let mut segments = Vec::new();
    let mut lineno = 0usize;
    loop {
        let mut buf = Vec::with_capacity(256);
        // Cap each read at MAX_TRANSCRIPT_LINE_BYTES so a malformed
        // transcript without newlines can't pull the whole file
        // into one allocation. Lines longer than the cap are
        // warn-skipped — corrupt entries don't stall the rest.
        let n = (&mut reader)
            .take(MAX_TRANSCRIPT_LINE_BYTES as u64 + 1)
            .read_until(b'\n', &mut buf)
            .map_err(|e| SessionError::VaultLocked {
                detail: format!("read transcript line {lineno}: {e}"),
            })?;
        if n == 0 {
            break;
        }
        if n > MAX_TRANSCRIPT_LINE_BYTES {
            tracing::warn!(
                line = lineno,
                bytes = n,
                "transcript line exceeds MAX_TRANSCRIPT_LINE_BYTES; skipping",
            );
            buf.clear();
            let _ = reader.read_until(b'\n', &mut buf);
            lineno += 1;
            continue;
        }
        let line = match std::str::from_utf8(&buf) {
            Ok(s) => s.trim_end_matches('\n').trim_end_matches('\r').to_owned(),
            Err(_) => {
                tracing::warn!(line = lineno, "non-utf8 transcript line; skipping");
                lineno += 1;
                continue;
            }
        };
        if line.trim().is_empty() {
            lineno += 1;
            continue;
        }
        match serde_json::from_str::<heron_types::Turn>(&line) {
            Ok(turn) => {
                let is_user = matches!(turn.speaker_source, heron_types::SpeakerSource::Self_);
                let identifier_kind = match turn.speaker_source {
                    heron_types::SpeakerSource::Self_ => IdentifierKind::Mic,
                    heron_types::SpeakerSource::Ax => IdentifierKind::AxTree,
                    heron_types::SpeakerSource::Channel => IdentifierKind::Fallback,
                    heron_types::SpeakerSource::Cluster => IdentifierKind::Fallback,
                };
                let confidence = match turn.confidence {
                    Some(c) if c >= 0.7 => heron_session::Confidence::High,
                    _ => heron_session::Confidence::Low,
                };
                segments.push(TranscriptSegment {
                    speaker: Participant {
                        display_name: turn.speaker,
                        identifier_kind,
                        is_user,
                    },
                    text: turn.text,
                    start_secs: turn.t0,
                    end_secs: turn.t1,
                    confidence,
                    is_final: true,
                });
            }
            Err(e) => {
                tracing::warn!(line = lineno, error = %e, "skipping malformed turn");
            }
        }
        lineno += 1;
    }
    Ok(segments)
}

fn vault_to_session_err(err: VaultError) -> SessionError {
    match err {
        VaultError::Io(e) if e.kind() == std::io::ErrorKind::NotFound => SessionError::NotFound {
            what: format!("vault file io: {e}"),
        },
        other => SessionError::VaultLocked {
            detail: other.to_string(),
        },
    }
}

fn parse_iso_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
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
    async fn substrate_only_methods_return_not_yet_implemented_without_vault() {
        // Pin the "stub for now" contract per-method when no
        // `vault_root` is configured. Read endpoints fall back to
        // `NotYetImplemented` because there's no on-disk source to
        // scan; capture-lifecycle methods + attach_context stay
        // `NotYetImplemented` regardless of vault config until
        // FSM-merge wires the heron-cli session driver.
        // `list_upcoming_calendar` is explicitly NOT in this set —
        // it works as soon as a CalendarReader is configured, which
        // is independent of the vault.
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
