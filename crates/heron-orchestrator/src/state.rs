//! Capture-lifecycle state types owned by the orchestrator.
//!
//! `LocalSessionOrchestrator` keeps a small handful of in-memory maps
//! to track in-flight and recently-finalized captures. The struct
//! definitions and the bounded `PendingContexts` helper around
//! `attach_context` live here; the per-`MeetingId` lifecycle methods
//! that mutate them remain on `LocalSessionOrchestrator` itself.
//!
//! The maps are held under sync `std::sync::Mutex` because every
//! operation on them is short and CPU-bound (insert / remove / lookup
//! / FSM transition / `bus.publish` which is itself sync). No `.await`
//! happens while a guard is held; the lock-ordering contract is
//! `active_meetings` first, then `pending_contexts` whenever both are
//! taken in the same scope.
//!
//! [`PendingContexts`] is a bounded staging map for `attach_context`.
//! It pairs a `HashMap` with a FIFO `VecDeque` so the cap-eviction
//! order is deterministic ("oldest insertion drops first") rather than
//! `HashMap`'s iteration-order non-determinism, and it holds both
//! fields under a single inner `Mutex` so no caller can ever observe
//! one being mutated without the other.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

use heron_cli::session::{SessionError as CliSessionError, SessionOutcome as CliSessionOutcome};
use heron_session::{Meeting, PreMeetingContext};
use heron_types::RecordingFsm;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::live_session::DynLiveSession;
use crate::lock_or_recover;

/// Cap on the number of `PreMeetingContext` entries the in-memory
/// staging map holds. Per-entry caps don't bound map size — a
/// caller spraying unique `calendar_event_id`s without ever calling
/// `start_capture` would otherwise grow the map without bound. At
/// the cap a fresh `attach_context` evicts the oldest entry first
/// (insertion-order FIFO via the `PendingContextsInner::order`
/// queue). 1024 covers ~weeks of upcoming-calendar events and is
/// orders of magnitude larger than any realistic working set.
pub(crate) const MAX_PENDING_CONTEXTS: usize = 1024;

/// Maximum number of completed daemon-issued IDs retained in memory
/// for post-finalization `Location` continuity. Vault notes remain
/// the durable source of truth; this only prevents a long-running
/// daemon from growing an unbounded compatibility index.
pub(crate) const FINALIZED_MEETING_INDEX_CAP: usize = 512;

/// Per-meeting state tracked while a capture is in flight. The
/// [`RecordingFsm`] is the same one `heron-cli`'s session orchestrator
/// drives in the live audio path; here it provides the legality check
/// for every transition `start_capture` / `end_meeting` triggers, and
/// the `meeting` snapshot is the latest copy that has been published
/// on the bus. `applied_context` carries the `PreMeetingContext`
/// (agenda / persona / briefing) that was staged via
/// `attach_context` and consumed at `start_capture`-time; the bot /
/// realtime / policy wiring will read it when those layers compose.
pub(crate) struct ActiveMeeting {
    pub(crate) fsm: RecordingFsm,
    pub(crate) meeting: Meeting,
    pub(crate) runtime: CaptureRuntime,
    pub(crate) applied_context: Option<PreMeetingContext>,
    /// Live v2 stack (bot + realtime + bridge + policy controller)
    /// when the orchestrator is configured with a
    /// [`crate::live_session::LiveSessionFactory`] AND the factory
    /// accepted the start args. `None` means either no factory was
    /// installed (vault-only mode) or the factory call failed and
    /// `start_capture` fell back to the v1 path. `end_meeting` shuts
    /// this down in dependency order before — and independently of —
    /// the v1 pipeline finalizer.
    pub(crate) live_session: Option<Box<dyn DynLiveSession>>,
    /// Tier 3 #16 pause flag. The orchestrator owns the flag; the
    /// pipeline reads it via a clone passed through `SessionConfig`.
    /// `pause_capture` flips it to `true` (alongside the FSM transition
    /// to `Paused`); `resume_capture` flips it back. Capture-pipeline
    /// WAV writers and the AX collector check it on every frame /
    /// event and drop on the floor when set. Synthetic captures keep a
    /// flag too so the orchestrator's pause/resume contract is uniform
    /// across runtime variants — the synthetic path just has no
    /// pipeline to read it.
    pub(crate) pause_flag: Arc<AtomicBool>,
}

/// Runtime backing for an active capture.
pub(crate) enum CaptureRuntime {
    /// Vault-less constructors keep the historical FSM-only behavior
    /// for substrate tests and for callers that intentionally build
    /// without a writable vault.
    Synthetic,
    /// Vault-backed daemon sessions run the same audio → STT → LLM →
    /// vault pipeline used by `heron record`.
    Pipeline {
        stop_tx: oneshot::Sender<()>,
        handle: JoinHandle<Result<CliSessionOutcome, CliSessionError>>,
    },
}

pub(crate) struct FinalizedMeeting {
    pub(crate) meeting: Meeting,
    pub(crate) note_path: Option<PathBuf>,
}

/// Bounded staging map for `attach_context`. Pairs a `HashMap` with
/// a FIFO `VecDeque` so the cap-eviction order is "oldest insertion
/// drops first" rather than HashMap's iteration-order
/// non-determinism. The `Mutex` wrapper holds both fields together
/// so no caller can ever observe one being mutated without the
/// other.
pub(crate) struct PendingContexts {
    inner: Mutex<PendingContextsInner>,
}

struct PendingContextsInner {
    map: HashMap<String, PreMeetingContext>,
    /// Insertion order of keys currently in `map`. On overwrite of
    /// an existing key the queue is left unchanged (the key keeps
    /// its original FIFO position) — that matches the spec's
    /// "latest call wins" without resetting the eviction clock for
    /// callers that re-attach the same id.
    order: VecDeque<String>,
}

impl PendingContexts {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(PendingContextsInner {
                map: HashMap::new(),
                order: VecDeque::new(),
            }),
        }
    }

    /// Insert or overwrite. Returns whether an existing entry for
    /// `key` was overwritten. Caps the map at `MAX_PENDING_CONTEXTS`
    /// by evicting the oldest unrelated entry FIFO when a new key
    /// would push past the cap.
    pub(crate) fn insert(&self, key: String, value: PreMeetingContext) -> bool {
        let mut g = lock_or_recover(&self.inner);
        let overwrote = g.map.insert(key.clone(), value).is_some();
        if !overwrote {
            g.order.push_back(key);
            while g.order.len() > MAX_PENDING_CONTEXTS {
                if let Some(oldest) = g.order.pop_front() {
                    g.map.remove(&oldest);
                }
            }
        }
        overwrote
    }

    /// Remove and return the entry for `key`, if any. Used by
    /// `start_capture` to consume a staged context once the FSM has
    /// committed to materializing the session.
    pub(crate) fn remove(&self, key: &str) -> Option<PreMeetingContext> {
        let mut g = lock_or_recover(&self.inner);
        let value = g.map.remove(key)?;
        if let Some(pos) = g.order.iter().position(|k| k == key) {
            g.order.remove(pos);
        }
        Some(value)
    }

    /// Snapshot the entry for `key` without consuming it. Diagnostic
    /// only — production callers consume via `remove`.
    pub(crate) fn get_cloned(&self, key: &str) -> Option<PreMeetingContext> {
        lock_or_recover(&self.inner).map.get(key).cloned()
    }

    /// Whether an entry exists for `key`. Cheaper than `get_cloned`
    /// (no clone of the context body) — used by
    /// `list_upcoming_calendar` to mirror the `primed` flag onto each
    /// returned event without dragging the full context across.
    pub(crate) fn contains_key(&self, key: &str) -> bool {
        lock_or_recover(&self.inner).map.contains_key(key)
    }

    /// Insert only when no entry exists for `key`. Returns `true` when
    /// the insert happened, `false` when an existing entry preserved
    /// the staged context. Atomic across the check + insert (single
    /// lock acquisition) so a concurrent `insert` (manual
    /// `attach_context`) racing with this call cannot land between a
    /// `contains_key` probe and the insert and silently get clobbered.
    /// Same FIFO cap discipline as [`insert`](Self::insert).
    pub(crate) fn insert_if_absent(&self, key: String, value: PreMeetingContext) -> bool {
        let mut g = lock_or_recover(&self.inner);
        if g.map.contains_key(&key) {
            return false;
        }
        g.map.insert(key.clone(), value);
        g.order.push_back(key);
        while g.order.len() > MAX_PENDING_CONTEXTS {
            if let Some(oldest) = g.order.pop_front() {
                g.map.remove(&oldest);
            }
        }
        true
    }
}
