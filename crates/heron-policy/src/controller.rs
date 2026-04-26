//! [`DefaultSpeechController`] — the production [`SpeechController`]
//! impl that wires a [`heron_realtime::RealtimeBackend`] to the
//! pure-logic [`crate::SpeechQueue`] + [`crate::PolicyProfile`]
//! filter from [`crate::filter::evaluate`] per
//! [`docs/archives/api-design-spec.md`](../../../docs/archives/api-design-spec.md) §9.
//!
//! The controller is the load-bearing seam between layer-3 (policy)
//! and layer-4 (realtime backend). Per spec §9 / Invariant 11 it
//! issues `Priority::Replace` as a single primitive when the
//! backend supports `atomic_response_cancel`, falling back to
//! cancel-then-speak (with a `tracing::warn` for the audit log) when
//! it doesn't.
//!
//! ### Concurrency model
//!
//! - `profile`: `std::sync::Mutex` rather than `ArcSwap`. The hot
//!   read path is one `evaluate()` call per `speak()`, well below
//!   the contention threshold where lock-free shines, and `Mutex`
//!   keeps the dependency surface minimal.
//! - `queue`: `std::sync::Mutex`. The queue is mutated under the
//!   same critical section that mints `UtteranceId`s and decides
//!   whether the new utterance starts immediately, so a single
//!   lock keeps the model linearizable.
//! - `current`: `std::sync::Mutex<Option<InFlight>>`. Maps the
//!   in-flight utterance to the backend's response handle plus
//!   timing data so the listener task can translate
//!   `ResponseDone {response} → SpeechEvent::Completed {id}` with
//!   the right `duration_ms`.
//! - `response_to_utterance`: `std::sync::Mutex<HashMap<…>>`. The
//!   listener consults it to map every backend `RealtimeEvent` to
//!   the right `UtteranceId`, which the listener doesn't otherwise
//!   know.
//!
//! Holding `std::sync::Mutex` across an `await` would deadlock the
//! tokio runtime under contention; every lock acquisition in this
//! file is scoped tightly enough that no `await` runs while a guard
//! is alive.
//!
//! ### Why `speak()` is serialized
//!
//! Every entry point on the controller (`speak`, `cancel`, etc.)
//! takes a [`tokio::sync::Mutex`] (`speak_lock`) before touching the
//! backend. The reason is concrete: the `Replace`/`Interrupt`
//! planners read `inner.current` to decide whether to issue
//! `response_cancel`, and that read MUST observe the value the
//! previous in-flight `start_utterance` writes. A naive lock-free
//! version with two concurrent `speak` calls would let the second
//! caller see `current = None` (the first's `response_create` is
//! still awaiting), miss the cancel, and double-speak. The
//! serializer is cheap because TTS turns are inherently sequential
//! at this layer.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::Utc;
use heron_realtime::{RealtimeBackend, RealtimeCapabilities, RealtimeEvent, ResponseId, SessionId};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::escalation::{EscalationHook, LoggingEscalationHook};
use crate::filter::{PolicyDecision, evaluate};
use crate::queue::{QueuedUtterance, SpeechQueue};
use crate::{
    CancelReason, PolicyProfile, Priority, SpeechCapabilities, SpeechController, SpeechError,
    SpeechEvent, UtteranceId, VoiceId,
};

/// Default capacity for the [`SpeechEvent`] broadcast channel. Sized
/// to absorb a long utterance's `Progress` deltas without dropping a
/// slow subscriber (UI layer); subscribers that lag past this size
/// will see `RecvError::Lagged` and reconnect.
pub const DEFAULT_EVENT_CAPACITY: usize = 256;

/// Per-utterance bookkeeping the listener task needs to translate
/// realtime backend events into [`SpeechEvent`]s.
#[derive(Debug)]
struct InFlight {
    utterance: UtteranceId,
    response: ResponseId,
    started_at: chrono::DateTime<Utc>,
    words_seen: u32,
}

/// Trait-object handle the listener uses to drive the next utterance
/// when one finishes naturally. We can't carry `Arc<B: RealtimeBackend>`
/// through `Inner` without parameterizing it, so the constructor wraps
/// the backend in an adapter that closes over the concrete type.
type StartFn = dyn Fn(QueuedUtterance) -> StartFuture + Send + Sync + 'static;
type StartFuture = std::pin::Pin<Box<dyn std::future::Future<Output = StartResult> + Send>>;

#[derive(Debug)]
enum StartResult {
    Started {
        response: ResponseId,
        started_at: chrono::DateTime<Utc>,
    },
    Failed {
        error: String,
    },
}

struct Inner {
    profile: Mutex<PolicyProfile>,
    queue: Mutex<SpeechQueue>,
    /// Single-slot in-flight tracker. Mutates under `queue.lock()`
    /// for linearizability — see module-level concurrency notes.
    current: Mutex<Option<InFlight>>,
    /// `ResponseId → UtteranceId` mapping the listener uses to
    /// translate `ResponseDone`/`ResponseTextDelta`/`Error` into
    /// the matching [`SpeechEvent`]. Populated in `speak()`,
    /// removed in the listener on terminal events.
    response_to_utterance: Mutex<HashMap<ResponseId, UtteranceId>>,
    /// Fan-out for [`SpeechEvent`]s. Cloned on each
    /// `subscribe_events` call.
    events: broadcast::Sender<SpeechEvent>,
    /// Adapter that calls `backend.response_create` for the supplied
    /// utterance, wrapped in a trait object so the listener task can
    /// drive promoted queue heads without parameterizing `Inner` over
    /// the backend type. Set once at construction; never mutated.
    start_response: Box<StartFn>,
    /// Same speak-lock as on `DefaultSpeechController` (clone of the
    /// inner `Arc`) so the listener task and the user-facing methods
    /// serialize against each other.
    speak_lock: Arc<tokio::sync::Mutex<()>>,
}

/// Production [`SpeechController`] over a [`RealtimeBackend`]. Spec §9.
///
/// Construct via [`Self::new`]; the constructor spawns a listener
/// task that translates `RealtimeEvent`s on
/// `backend.subscribe_events(session)` into `SpeechEvent`s on the
/// controller's broadcast channel. The listener exits when the
/// controller is dropped (the channel sender is the only strong
/// reference; the backend's broadcast `Receiver` stays alive but the
/// task drops out of its loop on the next event).
pub struct DefaultSpeechController<B: RealtimeBackend + 'static> {
    backend: Arc<B>,
    session: SessionId,
    inner: Arc<Inner>,
    /// Serializes `speak`, `cancel`, `cancel_all_queued`, and
    /// `cancel_current_and_clear` so concurrent callers can't race on
    /// the `inner.current` slot. The same lock is held inside `Inner`
    /// so the listener task can serialize against user-facing
    /// methods when promoting a queued utterance on `ResponseDone`.
    /// See module-level concurrency notes.
    speak_lock: Arc<tokio::sync::Mutex<()>>,
    listener: Mutex<Option<JoinHandle<()>>>,
    /// Side-channel for [`PolicyDecision::Escalate`] outcomes. Defaults
    /// to [`LoggingEscalationHook`]; production callers swap in a
    /// transport-aware impl via
    /// [`Self::with_escalation_hook`]. Held as `Arc<dyn ..>` so the
    /// hot-path call site can clone cheaply without locking.
    escalation: Arc<dyn EscalationHook>,
}

impl<B: RealtimeBackend + 'static> DefaultSpeechController<B> {
    /// Construct a controller bound to `session` on `backend` with
    /// the supplied `profile`. Spawns the realtime → speech event
    /// translator on the current Tokio runtime.
    ///
    /// # Panics
    ///
    /// Panics if not called from within a Tokio runtime — see the
    /// matching note on [`heron_orchestrator::Builder::build`].
    pub fn new(backend: Arc<B>, session: SessionId, profile: PolicyProfile) -> Self {
        Self::with_event_capacity(backend, session, profile, DEFAULT_EVENT_CAPACITY)
    }

    /// Like [`Self::new`] but allows tuning the event broadcast
    /// channel capacity. Tests use this to pin lag behavior.
    pub fn with_event_capacity(
        backend: Arc<B>,
        session: SessionId,
        profile: PolicyProfile,
        event_capacity: usize,
    ) -> Self {
        Self::build(
            backend,
            session,
            profile,
            event_capacity,
            Arc::new(LoggingEscalationHook),
        )
    }

    /// Construct with a custom [`EscalationHook`]. Production
    /// deployments use this to surface
    /// [`PolicyDecision::Escalate`] outcomes through their preferred
    /// transport (HTTP webhook, push notification, vault note).
    /// Tests use it to assert that the controller wires escalation
    /// from the filter through to the user-visible side-channel.
    ///
    /// The default constructor ([`Self::new`]) installs
    /// [`LoggingEscalationHook`], which writes a `tracing::warn!` per
    /// escalation. That's the right floor for v1 — escalations land
    /// in the daemon log even before any richer transport is wired.
    pub fn with_escalation_hook(
        backend: Arc<B>,
        session: SessionId,
        profile: PolicyProfile,
        hook: Arc<dyn EscalationHook>,
    ) -> Self {
        Self::build(backend, session, profile, DEFAULT_EVENT_CAPACITY, hook)
    }

    /// Internal builder shared by every public constructor — keeps the
    /// `Inner`/listener wiring in one place so a future construction
    /// option (e.g. preloaded queue state) can't drift.
    fn build(
        backend: Arc<B>,
        session: SessionId,
        profile: PolicyProfile,
        event_capacity: usize,
        escalation: Arc<dyn EscalationHook>,
    ) -> Self {
        let (events_tx, _) = broadcast::channel(event_capacity);
        let speak_lock = Arc::new(tokio::sync::Mutex::new(()));
        let start_response: Box<StartFn> = {
            let backend = Arc::clone(&backend);
            Box::new(move |utt: QueuedUtterance| {
                let backend = Arc::clone(&backend);
                Box::pin(async move {
                    match backend.response_create(session, &utt.text, None).await {
                        Ok(response) => StartResult::Started {
                            response,
                            started_at: Utc::now(),
                        },
                        Err(e) => StartResult::Failed {
                            error: e.to_string(),
                        },
                    }
                }) as StartFuture
            })
        };
        let inner = Arc::new(Inner {
            profile: Mutex::new(profile),
            queue: Mutex::new(SpeechQueue::new()),
            current: Mutex::new(None),
            response_to_utterance: Mutex::new(HashMap::new()),
            events: events_tx,
            start_response,
            speak_lock: Arc::clone(&speak_lock),
        });
        let listener = spawn_listener(Arc::clone(&backend), session, Arc::clone(&inner));
        Self {
            backend,
            session,
            inner,
            speak_lock,
            listener: Mutex::new(Some(listener)),
            escalation,
        }
    }

    /// Live-update the policy profile. Subsequent `speak()` calls
    /// see the new profile; in-flight utterances are not re-evaluated.
    pub fn set_profile(&self, profile: PolicyProfile) {
        *lock(&self.inner.profile) = profile;
    }

    /// Drive `utt` against the backend's `response_create`. Caller
    /// must hold the speak-lock and the queue's `current()` must
    /// already be `utt`.
    ///
    /// On backend failure: emit `Failed`, drain the queue past the
    /// failed utterance, and promote+start the next queued head if
    /// any. Without the recursive start the queue would be left
    /// "stuck" (model says `current = next`, backend has no in-flight
    /// response, the next `speak(Append)` queues behind a phantom).
    async fn start_utterance(
        &self,
        utt: QueuedUtterance,
        voice_override: Option<VoiceId>,
    ) -> Result<(), SpeechError> {
        let voice = voice_override.map(|v| v.to_string());
        match self
            .backend
            .response_create(self.session, &utt.text, voice)
            .await
        {
            Ok(response_id) => {
                let started_at = Utc::now();
                let _ = self.inner.events.send(SpeechEvent::Started {
                    id: utt.id,
                    started_at,
                });
                *lock(&self.inner.current) = Some(InFlight {
                    utterance: utt.id,
                    response: response_id,
                    started_at,
                    words_seen: 0,
                });
                lock(&self.inner.response_to_utterance).insert(response_id, utt.id);
                Ok(())
            }
            Err(e) => {
                let err = e.to_string();
                let _ = self.inner.events.send(SpeechEvent::Failed {
                    id: utt.id,
                    error: err.clone(),
                });
                let promoted = {
                    let mut queue = lock(&self.inner.queue);
                    let _ = queue.cancel(utt.id);
                    queue.current().cloned()
                };
                if let Some(next) = promoted {
                    promote_and_start(&self.inner, next).await;
                }
                Err(SpeechError::Backend(err))
            }
        }
    }

    /// Fire `Cancelled { reason }` for `ids` in iteration order.
    /// `Replace` passes `Replaced { by }`; explicit user cancels pass
    /// `UserRequested`.
    fn emit_cancelled(&self, ids: &[UtteranceId], reason: CancelReason) {
        for id in ids {
            let _ = self.inner.events.send(SpeechEvent::Cancelled {
                id: *id,
                reason: reason.clone(),
            });
        }
    }

    /// Fire the spec §9 audit event for a policy-blocked utterance.
    /// `filter::PolicyDecision::{Denied, Escalate}` both promise this
    /// shape via their docstring: one `Cancelled { reason:
    /// PolicyDenied { rule } }` per blocked emission. The utterance
    /// never made it to the queue, so we mint a synthetic
    /// [`UtteranceId`] purely as the correlation handle on the event;
    /// the caller still gets the rule via `Err(SpeechError::PolicyDenied)`.
    ///
    /// **Note for downstream subscribers:** these ids appear as
    /// "orphans" — no `Started` / `Progress` / `Completed` event
    /// shares them, because the speak path errored before the queue
    /// step. A consumer building a per-utterance lifecycle FSM should
    /// treat `Cancelled { reason: PolicyDenied }` as a valid first
    /// observation of an id, not assume `Started` always precedes it.
    fn emit_policy_denied(&self, rule: &str) {
        let _ = self.inner.events.send(SpeechEvent::Cancelled {
            id: UtteranceId::now_v7(),
            reason: CancelReason::PolicyDenied {
                rule: rule.to_owned(),
            },
        });
    }
}

#[async_trait]
impl<B: RealtimeBackend + 'static> SpeechController for DefaultSpeechController<B> {
    async fn speak(
        &self,
        text: &str,
        priority: Priority,
        voice_override: Option<VoiceId>,
    ) -> Result<UtteranceId, SpeechError> {
        let _guard = self.speak_lock.lock().await;

        // Run the policy filter against the active profile.
        // Snapshot so the lock isn't held across the await below.
        // `validate::validate` is for PolicyProfile shape and is
        // run at session-init by the orchestrator; the per-utterance
        // matcher is `evaluate`.
        let profile_snapshot = lock(&self.inner.profile).clone();
        match evaluate(text, &profile_snapshot) {
            PolicyDecision::Allowed => {}
            PolicyDecision::Denied { rule } => {
                self.emit_policy_denied(&rule);
                return Err(SpeechError::PolicyDenied { rule });
            }
            PolicyDecision::Escalate { rule, via } => {
                // Same audit-log shape as `Denied`; the hook is then
                // dispatched on a detached `tokio::spawn` so a slow,
                // hung, or panicking hook can never block the
                // `speak_lock`. Holding the lock across the hook
                // would serialize every subsequent `speak`/`cancel`
                // on the controller behind the hook's transport,
                // turning a missing webhook acknowledgement into a
                // controller-wide availability failure. The hook
                // contract is fire-and-forget: the decision is final
                // by the time we emit the `Cancelled` event, and the
                // hook only drives the user-visible side-channel.
                self.emit_policy_denied(&rule);
                let escalation = Arc::clone(&self.escalation);
                let rule_for_hook = rule.clone();
                tokio::spawn(async move {
                    escalation.escalate(rule_for_hook, via).await;
                });
                return Err(SpeechError::PolicyDenied { rule });
            }
        }

        // 3. Update the queue model + decide what to do under the
        //    queue lock. The block returns an action plan the caller
        //    executes after releasing the lock — we never await
        //    while holding `std::sync::Mutex`.
        let plan: SpeakPlan = {
            let mut queue = lock(&self.inner.queue);
            match priority {
                Priority::Append => {
                    let outcome = queue.enqueue(text.to_owned(), Priority::Append);
                    if outcome.start_immediately {
                        let utt = queue.current().cloned().ok_or_else(|| {
                            SpeechError::Backend(
                                "queue invariant violated: start_immediately but no current".into(),
                            )
                        })?;
                        SpeakPlan::Start {
                            new_id: outcome.new,
                            utt,
                            cancel_response: None,
                            replaced: Vec::new(),
                        }
                    } else {
                        SpeakPlan::Queued {
                            new_id: outcome.new,
                        }
                    }
                }
                Priority::Replace | Priority::Interrupt => {
                    let cancel_response = lock(&self.inner.current).as_ref().map(|c| c.response);
                    let outcome = queue.enqueue(text.to_owned(), priority);
                    let utt = queue.current().cloned().ok_or_else(|| {
                        SpeechError::Backend(
                            "queue invariant violated: replace/interrupt did not promote".into(),
                        )
                    })?;
                    SpeakPlan::Start {
                        new_id: outcome.new,
                        utt,
                        cancel_response,
                        replaced: outcome.cancellations,
                    }
                }
            }
        };

        match plan {
            SpeakPlan::Queued { new_id } => Ok(new_id),
            SpeakPlan::Start {
                new_id,
                utt,
                cancel_response,
                replaced,
            } => {
                // Issue any backend-side cancel BEFORE emitting the
                // `Replaced` events + starting the new response, so
                // an audit log reads in cause→effect order.
                if let Some(resp) = cancel_response {
                    let atomic = self.backend.capabilities().atomic_response_cancel;
                    if !atomic {
                        // Spec §9 Invariant 11: emulated replace has
                        // a cancel-then-speak race; surface it loudly
                        // so an operator reviewing the audit log
                        // sees the degraded path was used.
                        tracing::warn!(
                            session = %self.session,
                            response = %resp,
                            "atomic_response_cancel not supported; emulating Replace via cancel+speak (race exists per spec §9)",
                        );
                    }
                    if let Err(e) = self.backend.response_cancel(self.session, resp).await {
                        return Err(SpeechError::Backend(e.to_string()));
                    }
                    // Drop the now-stale entry from the response map.
                    lock(&self.inner.response_to_utterance).remove(&resp);
                    // Clear the in-flight slot before `start_utterance`
                    // fills it with the new response.
                    *lock(&self.inner.current) = None;
                }

                if !replaced.is_empty() {
                    self.emit_cancelled(&replaced, CancelReason::Replaced { by: new_id });
                }

                self.start_utterance(utt, voice_override).await?;
                Ok(new_id)
            }
        }
    }

    async fn cancel(&self, id: UtteranceId) -> Result<(), SpeechError> {
        let _guard = self.speak_lock.lock().await;

        // Snapshot the queue's view of `id` and the current
        // response handle under one lock, then act outside it.
        enum CancelPlan {
            CurrentWithBackend {
                response: ResponseId,
                promoted: Option<QueuedUtterance>,
            },
            Queued,
            NotFound,
        }

        let plan = {
            let mut queue = lock(&self.inner.queue);
            let mut current = lock(&self.inner.current);
            let is_current = current.as_ref().is_some_and(|c| c.utterance == id);
            if is_current {
                let response = current
                    .as_ref()
                    .map(|c| c.response)
                    .ok_or_else(|| SpeechError::Backend("invariant: current dropped".into()))?;
                let outcome = queue.cancel(id);
                let promoted = match outcome {
                    crate::queue::CancelOutcome::CancelledCurrent { promoted } => promoted,
                    // Queue and `current` are kept consistent by us;
                    // a divergence is a bug — surface as Backend error.
                    other => {
                        return Err(SpeechError::Backend(format!(
                            "queue/current divergence on cancel: {other:?}"
                        )));
                    }
                };
                *current = None;
                lock(&self.inner.response_to_utterance).remove(&response);
                CancelPlan::CurrentWithBackend { response, promoted }
            } else {
                drop(current);
                match queue.cancel(id) {
                    crate::queue::CancelOutcome::CancelledQueued => CancelPlan::Queued,
                    crate::queue::CancelOutcome::NotFound => CancelPlan::NotFound,
                    crate::queue::CancelOutcome::CancelledCurrent { .. } => {
                        // Unreachable: we already checked `is_current`.
                        return Err(SpeechError::Backend(
                            "queue/current divergence on cancel: unexpected current".into(),
                        ));
                    }
                }
            }
        };

        match plan {
            CancelPlan::CurrentWithBackend { response, promoted } => {
                if let Err(e) = self.backend.response_cancel(self.session, response).await {
                    return Err(SpeechError::Backend(e.to_string()));
                }
                let _ = self.inner.events.send(SpeechEvent::Cancelled {
                    id,
                    reason: CancelReason::UserRequested,
                });
                if let Some(next) = promoted {
                    self.start_utterance(next, None).await?;
                }
            }
            CancelPlan::Queued => {
                let _ = self.inner.events.send(SpeechEvent::Cancelled {
                    id,
                    reason: CancelReason::UserRequested,
                });
            }
            // Idempotent per the trait contract: unknown / done / already
            // cancelled returns Ok(()).
            CancelPlan::NotFound => {}
        }
        Ok(())
    }

    async fn cancel_all_queued(&self) -> Result<(), SpeechError> {
        let _guard = self.speak_lock.lock().await;
        let drained = {
            let mut queue = lock(&self.inner.queue);
            queue.cancel_all_queued()
        };
        self.emit_cancelled(&drained, CancelReason::UserRequested);
        Ok(())
    }

    async fn cancel_current_and_clear(&self) -> Result<(), SpeechError> {
        let _guard = self.speak_lock.lock().await;
        let (cancelled, current_response) = {
            let mut queue = lock(&self.inner.queue);
            let mut current = lock(&self.inner.current);
            let cancelled = queue.cancel_current_and_clear();
            let current_response = current.as_ref().map(|c| c.response);
            *current = None;
            if let Some(resp) = current_response {
                lock(&self.inner.response_to_utterance).remove(&resp);
            }
            (cancelled, current_response)
        };

        if let Some(resp) = current_response
            && let Err(e) = self.backend.response_cancel(self.session, resp).await
        {
            return Err(SpeechError::Backend(e.to_string()));
        }
        self.emit_cancelled(&cancelled, CancelReason::UserRequested);
        Ok(())
    }

    fn subscribe_events(&self) -> broadcast::Receiver<SpeechEvent> {
        self.inner.events.subscribe()
    }

    fn capabilities(&self) -> SpeechCapabilities {
        capabilities_from_backend(self.backend.capabilities())
    }
}

impl<B: RealtimeBackend + 'static> Drop for DefaultSpeechController<B> {
    fn drop(&mut self) {
        // Abort the listener task on drop — it holds an `Arc<Inner>`
        // and would otherwise survive the controller (waiting for
        // backend events that nothing is correlating any more).
        if let Some(handle) = lock(&self.listener).take() {
            handle.abort();
        }
    }
}

/// Translate [`RealtimeCapabilities`] into the speech-contract
/// [`SpeechCapabilities`] surface per spec §9.
///
/// Mapping (rationale per `docs/archives/api-design-research.md` "Layer 3"):
/// - `atomic_response_cancel` → `atomic_replace`: the atomic-cancel
///   primitive is exactly what makes `Priority::Replace` race-free.
/// - `server_vad` → `barge_in_detect`: server VAD is the only way
///   to detect barge-in without the controller running its own VAD.
/// - `text_deltas` → `utterance_ids` & `per_utterance_cancel`: the
///   backend always assigns a `ResponseId` to a response, and
///   [`RealtimeBackend::response_cancel`] is per-response — both
///   trait-level guarantees, surfaced as `true` so callers don't
///   have to special-case.
/// - `queue` is `true` unconditionally: the queue lives in
///   `heron-policy` and is independent of backend support.
pub(crate) fn capabilities_from_backend(caps: RealtimeCapabilities) -> SpeechCapabilities {
    SpeechCapabilities {
        utterance_ids: true,
        per_utterance_cancel: true,
        queue: true,
        atomic_replace: caps.atomic_response_cancel,
        barge_in_detect: caps.server_vad,
    }
}

/// Spawn the backend → speech-event translator. The task lives
/// until the controller is dropped (which `abort()`s it) or the
/// backend's broadcast channel closes.
fn spawn_listener<B: RealtimeBackend + 'static>(
    backend: Arc<B>,
    session: SessionId,
    inner: Arc<Inner>,
) -> JoinHandle<()> {
    let mut rx = backend.subscribe_events(session);
    tokio::spawn(async move {
        loop {
            let event = match rx.recv().await {
                Ok(e) => e,
                Err(broadcast::error::RecvError::Closed) => return,
                // Lagging the listener means the controller's
                // SpeechEvent stream is missing transitions from the
                // gap. We can't reconstruct them, but we can at
                // least keep going so post-gap events still flow.
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(
                        skipped,
                        session = %session,
                        "controller listener lagged backend events; subsequent SpeechEvents may be missing"
                    );
                    continue;
                }
            };
            handle_realtime_event(&inner, event).await;
        }
    })
}

async fn handle_realtime_event(inner: &Inner, event: RealtimeEvent) {
    match event {
        RealtimeEvent::ResponseAudioStarted { .. } => {
            // `Started` was already emitted synchronously from
            // `speak()` so the caller knows the utterance id before
            // the audio has actually started flowing. Re-emitting
            // here would double-fire.
        }
        RealtimeEvent::ResponseTextDelta { response, text, .. } => {
            let utt_id = lock(&inner.response_to_utterance).get(&response).copied();
            if let Some(id) = utt_id {
                let mut current = lock(&inner.current);
                if let Some(c) = current.as_mut()
                    && c.utterance == id
                {
                    let new_words = count_words(&text);
                    c.words_seen = c.words_seen.saturating_add(new_words);
                    let words_spoken = c.words_seen;
                    drop(current);
                    let _ = inner
                        .events
                        .send(SpeechEvent::Progress { id, words_spoken });
                }
            }
        }
        RealtimeEvent::ResponseDone { response, at, .. } => {
            // Snapshot the in-flight, emit Completed, then promote
            // the queue head and start its TTS via the backend
            // adapter. Acquires the same speak_lock as user-facing
            // methods so a concurrent `speak`/`cancel` can't race
            // with the listener over `inner.current`.
            let _guard = inner.speak_lock.lock().await;
            let utt_id = lock(&inner.response_to_utterance).remove(&response);
            if let Some(id) = utt_id {
                let duration_ms = {
                    let mut current = lock(&inner.current);
                    let dur = current
                        .as_ref()
                        .filter(|c| c.utterance == id)
                        .map(|c| {
                            let delta = at.signed_duration_since(c.started_at);
                            u64::try_from(delta.num_milliseconds().max(0)).unwrap_or(0)
                        })
                        .unwrap_or(0);
                    if current.as_ref().is_some_and(|c| c.utterance == id) {
                        *current = None;
                    }
                    dur
                };
                let _ = inner
                    .events
                    .send(SpeechEvent::Completed { id, duration_ms });

                // Promote the next queued utterance and start it.
                let promoted = {
                    let mut queue = lock(&inner.queue);
                    queue.finish_current()
                };
                if let Some(next) = promoted {
                    promote_and_start(inner, next).await;
                }
            }
        }
        RealtimeEvent::Error { error, .. } => {
            // Backend errors aren't tied to a specific response;
            // attribute to the in-flight utterance if any. This
            // matches the spec's `Failed { id, error }` shape.
            let _guard = inner.speak_lock.lock().await;
            let to_fail = {
                let mut current = lock(&inner.current);
                let taken = current.take();
                if let Some(c) = &taken {
                    lock(&inner.response_to_utterance).remove(&c.response);
                }
                taken
            };
            if let Some(c) = to_fail {
                let _ = inner.events.send(SpeechEvent::Failed {
                    id: c.utterance,
                    error,
                });
                // Drop the failed utterance from the queue model + start
                // the next one if any. Without this, the queue would
                // think `c.utterance` is still current and the next
                // `speak()` would queue behind a dead utterance.
                let promoted = {
                    let mut queue = lock(&inner.queue);
                    let _ = queue.cancel(c.utterance);
                    // `cancel` already promoted the head; surface it.
                    queue.current().cloned()
                };
                if let Some(next) = promoted {
                    promote_and_start(inner, next).await;
                }
            }
        }
        RealtimeEvent::ResponseCreated { .. }
        | RealtimeEvent::InputSpeechStarted { .. }
        | RealtimeEvent::InputSpeechStopped { .. }
        | RealtimeEvent::InputTranscriptDelta { .. }
        | RealtimeEvent::ToolCall { .. } => {
            // Not part of the speech-control vocabulary; the policy
            // layer handles barge-in / turn-taking elsewhere.
        }
    }
}

/// Drive `next` through the backend's `response_create`, emitting
/// `Started` on success and `Failed` on backend error. Caller must
/// hold `inner.speak_lock` and the queue must already have promoted
/// `next` to `current()`. On failure, drains the queue past `next`
/// and recursively starts the new head — otherwise a single backend
/// hiccup would freeze the queue forever.
async fn promote_and_start(inner: &Inner, next: QueuedUtterance) {
    // Defensively bound the recursion: if every queued utterance
    // fails to start, we still need to terminate. `MAX_PROMOTE_RETRIES`
    // is well beyond any realistic queue depth in this layer.
    const MAX_PROMOTE_RETRIES: usize = 16;
    let mut current_utt = next;
    for _ in 0..MAX_PROMOTE_RETRIES {
        match (inner.start_response)(current_utt.clone()).await {
            StartResult::Started {
                response,
                started_at,
            } => {
                let _ = inner.events.send(SpeechEvent::Started {
                    id: current_utt.id,
                    started_at,
                });
                *lock(&inner.current) = Some(InFlight {
                    utterance: current_utt.id,
                    response,
                    started_at,
                    words_seen: 0,
                });
                lock(&inner.response_to_utterance).insert(response, current_utt.id);
                return;
            }
            StartResult::Failed { error } => {
                let _ = inner.events.send(SpeechEvent::Failed {
                    id: current_utt.id,
                    error,
                });
                let promoted = {
                    let mut queue = lock(&inner.queue);
                    let _ = queue.cancel(current_utt.id);
                    queue.current().cloned()
                };
                match promoted {
                    Some(p) => current_utt = p,
                    None => return,
                }
            }
        }
    }
}

fn count_words(text: &str) -> u32 {
    let n = text.split_whitespace().count();
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Internal action plan returned from the queue critical section,
/// executed after releasing the lock so we never `await` while
/// holding `std::sync::Mutex`.
enum SpeakPlan {
    Queued {
        new_id: UtteranceId,
    },
    Start {
        new_id: UtteranceId,
        utt: QueuedUtterance,
        /// `Some(resp)` when a backend response_cancel must fire
        /// before starting the new utterance (Replace / Interrupt).
        cancel_response: Option<ResponseId>,
        /// Utterances the queue model dropped (FIFO: current then
        /// queued) so the controller emits `Cancelled { Replaced }`
        /// for each.
        replaced: Vec<UtteranceId>,
    },
}

/// Lock helper — recovers the inner data on poisoning. Same shape
/// as [`heron_orchestrator::lock_or_recover`]; benign because every
/// guard is held briefly.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::{EscalationMode, PolicyProfile};
    use async_trait::async_trait;
    use chrono::Utc;
    use heron_realtime::{
        RealtimeBackend, RealtimeCapabilities, RealtimeError, RealtimeEvent, ResponseId,
        SessionConfig, SessionId,
    };
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;
    use tokio::sync::broadcast;
    use tokio::time::timeout;

    /// Recorded backend call. Tests assert on the sequence to pin
    /// the spec's `Replace = single response_cancel + single
    /// response_create` invariant (§9 / Invariant 11).
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum BackendCall {
        ResponseCreate { text: String, voice: Option<String> },
        ResponseCancel { response: ResponseId },
    }

    struct TestRealtimeBackend {
        session: SessionId,
        events_tx: broadcast::Sender<RealtimeEvent>,
        capabilities: StdMutex<RealtimeCapabilities>,
        calls: StdMutex<Vec<BackendCall>>,
        next_response_seq: StdMutex<Vec<ResponseId>>,
        fail_next_create: StdMutex<bool>,
    }

    impl TestRealtimeBackend {
        fn new(caps: RealtimeCapabilities) -> Arc<Self> {
            let (tx, _) = broadcast::channel(64);
            Arc::new(Self {
                session: SessionId::now_v7(),
                events_tx: tx,
                capabilities: StdMutex::new(caps),
                calls: StdMutex::new(Vec::new()),
                next_response_seq: StdMutex::new(Vec::new()),
                fail_next_create: StdMutex::new(false),
            })
        }

        fn fail_next_create(&self) {
            *self.fail_next_create.lock().expect("flag lock") = true;
        }

        fn session(&self) -> SessionId {
            self.session
        }

        fn record_calls(&self) -> Vec<BackendCall> {
            self.calls.lock().expect("calls lock").clone()
        }

        /// Force the next `response_create` to return this id.
        /// Useful for tests that need to assert event correlation
        /// against a known id.
        fn queue_response_id(&self, id: ResponseId) {
            self.next_response_seq.lock().expect("seq lock").push(id);
        }

        fn emit(&self, event: RealtimeEvent) {
            // Ignore if no subscribers — listener may not have
            // subscribed yet, but tests `subscribe_events` first.
            let _ = self.events_tx.send(event);
        }
    }

    #[async_trait]
    impl RealtimeBackend for TestRealtimeBackend {
        async fn session_open(&self, _config: SessionConfig) -> Result<SessionId, RealtimeError> {
            Ok(self.session)
        }

        async fn session_close(&self, _id: SessionId) -> Result<(), RealtimeError> {
            Ok(())
        }

        async fn response_create(
            &self,
            _session: SessionId,
            text: &str,
            voice_override: Option<String>,
        ) -> Result<ResponseId, RealtimeError> {
            if std::mem::take(&mut *self.fail_next_create.lock().expect("flag lock")) {
                return Err(RealtimeError::Backend("simulated".into()));
            }
            let response = self
                .next_response_seq
                .lock()
                .expect("seq lock")
                .pop()
                .unwrap_or_else(ResponseId::now_v7);
            self.calls
                .lock()
                .expect("calls lock")
                .push(BackendCall::ResponseCreate {
                    text: text.to_owned(),
                    voice: voice_override,
                });
            Ok(response)
        }

        async fn response_cancel(
            &self,
            _session: SessionId,
            response: ResponseId,
        ) -> Result<(), RealtimeError> {
            self.calls
                .lock()
                .expect("calls lock")
                .push(BackendCall::ResponseCancel { response });
            Ok(())
        }

        async fn truncate_current(
            &self,
            _session: SessionId,
            _audio_end_ms: u32,
        ) -> Result<(), RealtimeError> {
            Ok(())
        }

        async fn tool_result(
            &self,
            _session: SessionId,
            _tool_call_id: String,
            _result: serde_json::Value,
        ) -> Result<(), RealtimeError> {
            Ok(())
        }

        fn subscribe_events(&self, _id: SessionId) -> broadcast::Receiver<RealtimeEvent> {
            self.events_tx.subscribe()
        }

        fn capabilities(&self) -> RealtimeCapabilities {
            *self.capabilities.lock().expect("caps lock")
        }
    }

    fn open_profile() -> PolicyProfile {
        PolicyProfile {
            allow_topics: vec![],
            deny_topics: vec![],
            mute: false,
            escalation: EscalationMode::None,
        }
    }

    fn caps_atomic() -> RealtimeCapabilities {
        RealtimeCapabilities {
            bidirectional_audio: true,
            server_vad: true,
            atomic_response_cancel: true,
            tool_calling: true,
            text_deltas: true,
        }
    }

    fn caps_no_atomic() -> RealtimeCapabilities {
        RealtimeCapabilities {
            bidirectional_audio: true,
            server_vad: true,
            atomic_response_cancel: false,
            tool_calling: true,
            text_deltas: true,
        }
    }

    /// Drain all pending speech events. Best-effort: returns what
    /// shows up within `dur`, then stops. Used to wait for the
    /// listener task to translate a backend event before asserting.
    async fn drain_events(
        rx: &mut broadcast::Receiver<SpeechEvent>,
        max: usize,
        dur: Duration,
    ) -> Vec<SpeechEvent> {
        let mut out = Vec::with_capacity(max);
        for _ in 0..max {
            match timeout(dur, rx.recv()).await {
                Ok(Ok(e)) => out.push(e),
                _ => break,
            }
        }
        out
    }

    #[tokio::test]
    async fn append_enqueues_and_starts_then_completes_on_response_done() {
        let backend = TestRealtimeBackend::new(caps_atomic());
        let session = backend.session();
        let response_id = ResponseId::now_v7();
        backend.queue_response_id(response_id);

        let controller =
            DefaultSpeechController::new(Arc::clone(&backend) as _, session, open_profile());
        let mut events = controller.subscribe_events();

        let utt = controller
            .speak("hello", Priority::Append, None)
            .await
            .expect("speak");

        // Backend received exactly one response_create.
        assert_eq!(
            backend.record_calls(),
            vec![BackendCall::ResponseCreate {
                text: "hello".into(),
                voice: None,
            }]
        );

        // Started event fires.
        let evs = drain_events(&mut events, 1, Duration::from_millis(50)).await;
        assert!(matches!(evs.as_slice(), [SpeechEvent::Started { id, .. }] if *id == utt));

        // Backend reports done.
        backend.emit(RealtimeEvent::ResponseDone {
            session,
            response: response_id,
            at: Utc::now(),
        });

        let evs = drain_events(&mut events, 1, Duration::from_millis(200)).await;
        assert!(
            matches!(evs.as_slice(), [SpeechEvent::Completed { id, .. }] if *id == utt),
            "got {evs:?}",
        );
    }

    #[tokio::test]
    async fn replace_atomic_issues_one_cancel_then_one_speak() {
        let backend = TestRealtimeBackend::new(caps_atomic());
        let session = backend.session();
        let first_resp = ResponseId::now_v7();
        let second_resp = ResponseId::now_v7();
        backend.queue_response_id(second_resp);
        backend.queue_response_id(first_resp);

        let controller =
            DefaultSpeechController::new(Arc::clone(&backend) as _, session, open_profile());
        let mut events = controller.subscribe_events();

        let first = controller
            .speak("first", Priority::Append, None)
            .await
            .expect("first");

        let second = controller
            .speak("boss", Priority::Replace, None)
            .await
            .expect("replace");

        // Two creates and one cancel, in order: create(first),
        // cancel(first), create(boss).
        let calls = backend.record_calls();
        assert_eq!(
            calls,
            vec![
                BackendCall::ResponseCreate {
                    text: "first".into(),
                    voice: None,
                },
                BackendCall::ResponseCancel {
                    response: first_resp,
                },
                BackendCall::ResponseCreate {
                    text: "boss".into(),
                    voice: None,
                },
            ],
            "calls = {calls:?}"
        );

        // We expect: Started(first), Cancelled{Replaced{by:second}}(first), Started(second).
        let evs = drain_events(&mut events, 3, Duration::from_millis(100)).await;
        let mut saw_started_first = false;
        let mut saw_cancelled_first_by_second = false;
        let mut saw_started_second = false;
        for e in &evs {
            match e {
                SpeechEvent::Started { id, .. } if *id == first => saw_started_first = true,
                SpeechEvent::Started { id, .. } if *id == second => {
                    saw_started_second = true;
                }
                SpeechEvent::Cancelled {
                    id,
                    reason: CancelReason::Replaced { by },
                } if *id == first && *by == second => saw_cancelled_first_by_second = true,
                _ => {}
            }
        }
        assert!(saw_started_first, "missing Started(first): {evs:?}");
        assert!(
            saw_cancelled_first_by_second,
            "missing Cancelled{{Replaced}}(first by second): {evs:?}",
        );
        assert!(saw_started_second, "missing Started(second): {evs:?}");
    }

    #[tokio::test]
    async fn replace_non_atomic_falls_back_to_cancel_then_speak() {
        let backend = TestRealtimeBackend::new(caps_no_atomic());
        let session = backend.session();
        let controller =
            DefaultSpeechController::new(Arc::clone(&backend) as _, session, open_profile());

        controller
            .speak("first", Priority::Append, None)
            .await
            .expect("first");
        controller
            .speak("boss", Priority::Replace, None)
            .await
            .expect("replace");

        let calls = backend.record_calls();
        // Same call ordering as the atomic case (cancel BEFORE the new
        // create). The race is at the wire level — backends without
        // atomic_response_cancel may emit a brief gap between the two
        // operations. The controller logs a tracing::warn for the
        // audit trail; we don't assert the warn here to avoid
        // pulling in tracing-test as a dev-dep.
        assert!(
            calls
                .iter()
                .any(|c| matches!(c, BackendCall::ResponseCancel { .. })),
            "expected a cancel: {calls:?}",
        );
        let creates = calls
            .iter()
            .filter(|c| matches!(c, BackendCall::ResponseCreate { .. }))
            .count();
        assert_eq!(creates, 2, "expected 2 creates: {calls:?}");
    }

    #[tokio::test]
    async fn interrupt_cancels_current_but_keeps_queue() {
        let backend = TestRealtimeBackend::new(caps_atomic());
        let session = backend.session();
        let controller =
            DefaultSpeechController::new(Arc::clone(&backend) as _, session, open_profile());

        controller
            .speak("first", Priority::Append, None)
            .await
            .expect("first");
        controller
            .speak("second", Priority::Append, None)
            .await
            .expect("second");
        controller
            .speak("third", Priority::Append, None)
            .await
            .expect("third");

        // State: current=first, queue=[second, third]. After
        // Interrupt: current=correction, queue=[second, third].
        controller
            .speak("correction", Priority::Interrupt, None)
            .await
            .expect("interrupt");

        let queue = controller.inner.queue.lock().expect("queue lock");
        let current_text = queue.current().map(|u| u.text.clone());
        let queued_texts: Vec<_> = queue.queued().map(|u| u.text.clone()).collect();
        assert_eq!(current_text.as_deref(), Some("correction"));
        assert_eq!(queued_texts, vec!["second", "third"]);
    }

    #[tokio::test]
    async fn policy_denied_for_mute() {
        let backend = TestRealtimeBackend::new(caps_atomic());
        let session = backend.session();
        let mut profile = open_profile();
        profile.mute = true;
        let controller = DefaultSpeechController::new(Arc::clone(&backend) as _, session, profile);

        let err = controller
            .speak("hi", Priority::Append, None)
            .await
            .expect_err("muted");
        match err {
            SpeechError::PolicyDenied { rule } => assert_eq!(rule, "muted"),
            other => panic!("expected PolicyDenied(muted), got {other:?}"),
        }
        // Backend was not called at all when policy blocked the speak.
        assert!(backend.record_calls().is_empty());
    }

    #[tokio::test]
    async fn policy_denied_for_deny_topic() {
        let backend = TestRealtimeBackend::new(caps_atomic());
        let session = backend.session();
        let mut profile = open_profile();
        profile.deny_topics = vec!["legal".into()];
        let controller = DefaultSpeechController::new(Arc::clone(&backend) as _, session, profile);

        let err = controller
            .speak("the legal contract", Priority::Append, None)
            .await
            .expect_err("denied");
        match err {
            SpeechError::PolicyDenied { rule } => {
                assert!(rule.starts_with("deny_topic:"), "rule = {rule}");
            }
            other => panic!("expected PolicyDenied, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn policy_denied_for_allow_list_miss() {
        let backend = TestRealtimeBackend::new(caps_atomic());
        let session = backend.session();
        let mut profile = open_profile();
        profile.allow_topics = vec!["pricing".into()];
        let controller = DefaultSpeechController::new(Arc::clone(&backend) as _, session, profile);

        let err = controller
            .speak("the weather", Priority::Append, None)
            .await
            .expect_err("denied");
        match err {
            SpeechError::PolicyDenied { rule } => assert_eq!(rule, "not_in_allow_list"),
            other => panic!("expected PolicyDenied(not_in_allow_list), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancel_is_idempotent() {
        let backend = TestRealtimeBackend::new(caps_atomic());
        let session = backend.session();
        let controller =
            DefaultSpeechController::new(Arc::clone(&backend) as _, session, open_profile());

        let id = controller
            .speak("hello", Priority::Append, None)
            .await
            .expect("speak");

        controller.cancel(id).await.expect("first cancel");
        controller
            .cancel(id)
            .await
            .expect("second cancel idempotent");
        // Unknown id also Ok.
        controller
            .cancel(UtteranceId::now_v7())
            .await
            .expect("unknown id idempotent");
    }

    #[tokio::test]
    async fn cancel_all_queued_drains_queue_leaves_current() {
        let backend = TestRealtimeBackend::new(caps_atomic());
        let session = backend.session();
        let controller =
            DefaultSpeechController::new(Arc::clone(&backend) as _, session, open_profile());

        let _first = controller
            .speak("first", Priority::Append, None)
            .await
            .expect("first");
        let second = controller
            .speak("second", Priority::Append, None)
            .await
            .expect("second");
        let third = controller
            .speak("third", Priority::Append, None)
            .await
            .expect("third");

        let mut events = controller.subscribe_events();
        controller
            .cancel_all_queued()
            .await
            .expect("cancel_all_queued");

        // Queue drained, current keeps speaking.
        {
            let queue = controller.inner.queue.lock().expect("queue lock");
            assert_eq!(
                queue.current().map(|u| u.text.clone()).as_deref(),
                Some("first")
            );
            assert_eq!(queue.queue_len(), 0);
        }

        // Two Cancelled events for second + third (order matches FIFO).
        let evs = drain_events(&mut events, 2, Duration::from_millis(50)).await;
        let cancelled_ids: Vec<UtteranceId> = evs
            .iter()
            .filter_map(|e| match e {
                SpeechEvent::Cancelled { id, .. } => Some(*id),
                _ => None,
            })
            .collect();
        assert_eq!(cancelled_ids, vec![second, third]);

        // Backend was never asked to cancel — it's still speaking.
        let cancels = backend
            .record_calls()
            .into_iter()
            .filter(|c| matches!(c, BackendCall::ResponseCancel { .. }))
            .count();
        assert_eq!(cancels, 0);
    }

    #[tokio::test]
    async fn cancel_current_and_clear_cancels_backend_and_drains_queue() {
        let backend = TestRealtimeBackend::new(caps_atomic());
        let session = backend.session();
        let controller =
            DefaultSpeechController::new(Arc::clone(&backend) as _, session, open_profile());

        controller
            .speak("first", Priority::Append, None)
            .await
            .expect("first");
        controller
            .speak("second", Priority::Append, None)
            .await
            .expect("second");
        controller
            .speak("third", Priority::Append, None)
            .await
            .expect("third");

        controller.cancel_current_and_clear().await.expect("clear");

        {
            let queue = controller.inner.queue.lock().expect("queue lock");
            assert!(queue.is_idle());
        }

        // Backend got exactly one response_cancel (for the in-flight).
        let cancels = backend
            .record_calls()
            .into_iter()
            .filter(|c| matches!(c, BackendCall::ResponseCancel { .. }))
            .count();
        assert_eq!(cancels, 1);
    }

    #[test]
    fn capabilities_mapping_for_four_matrices() {
        // Matrix 1: full atomic + VAD.
        let caps = capabilities_from_backend(RealtimeCapabilities {
            bidirectional_audio: true,
            server_vad: true,
            atomic_response_cancel: true,
            tool_calling: true,
            text_deltas: true,
        });
        assert!(caps.atomic_replace);
        assert!(caps.barge_in_detect);

        // Matrix 2: no atomic cancel.
        let caps = capabilities_from_backend(RealtimeCapabilities {
            atomic_response_cancel: false,
            server_vad: true,
            ..Default::default()
        });
        assert!(!caps.atomic_replace);
        assert!(caps.barge_in_detect);

        // Matrix 3: no server VAD.
        let caps = capabilities_from_backend(RealtimeCapabilities {
            atomic_response_cancel: true,
            server_vad: false,
            ..Default::default()
        });
        assert!(caps.atomic_replace);
        assert!(!caps.barge_in_detect);

        // Matrix 4: nothing — the controller still reports utterance_ids,
        // per_utterance_cancel, and queue as `true` because those are
        // implemented in heron-policy.
        let caps = capabilities_from_backend(RealtimeCapabilities::default());
        assert!(caps.utterance_ids);
        assert!(caps.per_utterance_cancel);
        assert!(caps.queue);
        assert!(!caps.atomic_replace);
        assert!(!caps.barge_in_detect);
    }

    #[tokio::test]
    async fn set_profile_live_update_blocks_subsequent_speak() {
        let backend = TestRealtimeBackend::new(caps_atomic());
        let session = backend.session();
        let controller =
            DefaultSpeechController::new(Arc::clone(&backend) as _, session, open_profile());

        // Open profile: speak succeeds.
        controller
            .speak("hi", Priority::Append, None)
            .await
            .expect("first speak");

        // Tighten the profile mid-session.
        let mut tightened = open_profile();
        tightened.mute = true;
        controller.set_profile(tightened);

        let err = controller
            .speak("hi", Priority::Append, None)
            .await
            .expect_err("muted after set_profile");
        assert!(matches!(err, SpeechError::PolicyDenied { .. }));
    }

    #[tokio::test]
    async fn response_done_promotes_and_starts_next_queued_utterance() {
        let backend = TestRealtimeBackend::new(caps_atomic());
        let session = backend.session();
        let first_resp = ResponseId::now_v7();
        let second_resp = ResponseId::now_v7();
        backend.queue_response_id(second_resp);
        backend.queue_response_id(first_resp);

        let controller =
            DefaultSpeechController::new(Arc::clone(&backend) as _, session, open_profile());
        let mut events = controller.subscribe_events();

        let first = controller
            .speak("first", Priority::Append, None)
            .await
            .expect("first");
        let second = controller
            .speak("second", Priority::Append, None)
            .await
            .expect("second");

        // Drain Started(first); second is queued.
        let _ = drain_events(&mut events, 1, Duration::from_millis(50)).await;

        // Backend reports first done. The listener should:
        //   1. emit Completed(first)
        //   2. promote `second` to current
        //   3. call backend.response_create("second")
        //   4. emit Started(second)
        backend.emit(RealtimeEvent::ResponseDone {
            session,
            response: first_resp,
            at: Utc::now(),
        });

        let evs = drain_events(&mut events, 2, Duration::from_millis(500)).await;
        let mut saw_completed_first = false;
        let mut saw_started_second = false;
        for e in &evs {
            match e {
                SpeechEvent::Completed { id, .. } if *id == first => saw_completed_first = true,
                SpeechEvent::Started { id, .. } if *id == second => saw_started_second = true,
                _ => {}
            }
        }
        assert!(saw_completed_first, "missing Completed(first): {evs:?}");
        assert!(
            saw_started_second,
            "listener must drive next queued utterance: {evs:?}"
        );

        // Backend received two response_create calls — one for each.
        let creates: Vec<_> = backend
            .record_calls()
            .into_iter()
            .filter_map(|c| match c {
                BackendCall::ResponseCreate { text, .. } => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(creates, vec!["first", "second"]);
    }

    #[tokio::test]
    async fn backend_failure_rolls_queue_back_and_emits_failed() {
        let backend = TestRealtimeBackend::new(caps_atomic());
        let session = backend.session();
        let controller =
            DefaultSpeechController::new(Arc::clone(&backend) as _, session, open_profile());
        let mut events = controller.subscribe_events();

        backend.fail_next_create();
        let err = controller
            .speak("doomed", Priority::Append, None)
            .await
            .expect_err("backend failure");
        assert!(matches!(err, SpeechError::Backend(_)));

        // Queue rolled back to idle so the next speak can proceed.
        {
            let queue = controller.inner.queue.lock().expect("queue lock");
            assert!(queue.is_idle(), "queue should roll back on failure");
        }

        // A Failed event was emitted for the failed utterance.
        let evs = drain_events(&mut events, 1, Duration::from_millis(50)).await;
        assert!(
            evs.iter().any(|e| matches!(e, SpeechEvent::Failed { .. })),
            "expected SpeechEvent::Failed: {evs:?}",
        );

        // Subsequent speak proceeds normally.
        let _ = controller
            .speak("recovery", Priority::Append, None)
            .await
            .expect("recovery speak");
    }

    #[tokio::test]
    async fn progress_event_counts_words_from_text_delta() {
        let backend = TestRealtimeBackend::new(caps_atomic());
        let session = backend.session();
        let resp = ResponseId::now_v7();
        backend.queue_response_id(resp);
        let controller =
            DefaultSpeechController::new(Arc::clone(&backend) as _, session, open_profile());
        let mut events = controller.subscribe_events();

        let utt = controller
            .speak("hello", Priority::Append, None)
            .await
            .expect("speak");
        // Drain Started.
        let _ = drain_events(&mut events, 1, Duration::from_millis(50)).await;

        backend.emit(RealtimeEvent::ResponseTextDelta {
            session,
            response: resp,
            text: "hello there friend".into(),
        });

        let evs = drain_events(&mut events, 1, Duration::from_millis(200)).await;
        match evs.as_slice() {
            [SpeechEvent::Progress { id, words_spoken }] => {
                assert_eq!(*id, utt);
                assert_eq!(*words_spoken, 3);
            }
            other => panic!("expected Progress, got {other:?}"),
        }
    }

    // ── Policy enforcement audit-log + escalation tests ──────────────
    //
    // The filter-only tests in `filter.rs` exercise `evaluate()`
    // directly. These pin the *call path*: when a real
    // [`DefaultSpeechController`] runs `speak()`, the filter fires,
    // emits the spec-required `Cancelled { reason: PolicyDenied }`
    // audit event, and (for `Escalate`) drives the configured
    // [`EscalationHook`]. Closing gap #8 from `docs/archives/codebase-gaps.md`:
    // "policy filter is defined but never invoked" through the
    // controller's user-facing surface — the previous controller path
    // returned the right Err but skipped the audit event entirely.

    use crate::escalation::RecordingEscalationHook;

    /// Helper: drain pending events looking for the first
    /// `Cancelled { reason: PolicyDenied { rule } }` and return its
    /// rule string. Bails after `dur` so a regression that drops the
    /// event surfaces as a `None` rather than hanging the test.
    async fn first_policy_denied_rule(
        rx: &mut broadcast::Receiver<SpeechEvent>,
        dur: Duration,
    ) -> Option<String> {
        let evs = drain_events(rx, 4, dur).await;
        for e in evs {
            if let SpeechEvent::Cancelled {
                reason: CancelReason::PolicyDenied { rule },
                ..
            } = e
            {
                return Some(rule);
            }
        }
        None
    }

    #[tokio::test]
    async fn mute_emits_policy_denied_cancelled_event() {
        let backend = TestRealtimeBackend::new(caps_atomic());
        let session = backend.session();
        let mut profile = open_profile();
        profile.mute = true;
        let controller = DefaultSpeechController::new(Arc::clone(&backend) as _, session, profile);
        let mut events = controller.subscribe_events();

        let _ = controller
            .speak("hi", Priority::Append, None)
            .await
            .expect_err("muted");

        let rule = first_policy_denied_rule(&mut events, Duration::from_millis(50)).await;
        assert_eq!(
            rule.as_deref(),
            Some("muted"),
            "controller must emit PolicyDenied audit event on mute",
        );
        // Backend untouched on a policy-blocked speak.
        assert!(backend.record_calls().is_empty());
    }

    #[tokio::test]
    async fn deny_topic_without_escalation_emits_policy_denied_event_no_hook() {
        let backend = TestRealtimeBackend::new(caps_atomic());
        let session = backend.session();
        let mut profile = open_profile();
        profile.deny_topics = vec!["compensation".into()];
        // EscalationMode::None ⇒ filter returns Denied, not Escalate;
        // the hook must NOT fire.
        let hook = RecordingEscalationHook::new();
        let controller = DefaultSpeechController::with_escalation_hook(
            Arc::clone(&backend) as _,
            session,
            profile,
            Arc::clone(&hook) as _,
        );
        let mut events = controller.subscribe_events();

        let _ = controller
            .speak("their compensation package", Priority::Append, None)
            .await
            .expect_err("denied");

        let rule = first_policy_denied_rule(&mut events, Duration::from_millis(50))
            .await
            .expect("Cancelled event missing");
        assert!(
            rule.starts_with("deny_topic:") && rule.contains("compensation"),
            "rule = {rule}",
        );
        assert!(
            hook.calls().is_empty(),
            "escalation hook must not fire when EscalationMode::None: {:?}",
            hook.calls(),
        );
    }

    #[tokio::test]
    async fn deny_topic_with_notify_drives_escalation_hook() {
        let backend = TestRealtimeBackend::new(caps_atomic());
        let session = backend.session();
        let mut profile = open_profile();
        profile.deny_topics = vec!["legal".into()];
        profile.escalation = EscalationMode::Notify {
            destination: "ops@example.com".into(),
        };
        let hook = RecordingEscalationHook::new();
        let controller = DefaultSpeechController::with_escalation_hook(
            Arc::clone(&backend) as _,
            session,
            profile,
            Arc::clone(&hook) as _,
        );
        let mut events = controller.subscribe_events();

        let err = controller
            .speak("send the legal contract", Priority::Append, None)
            .await
            .expect_err("escalated");
        // Caller still sees PolicyDenied — escalation is a side
        // channel, not a different return shape.
        assert!(matches!(err, SpeechError::PolicyDenied { .. }));

        // Audit event present.
        let rule = first_policy_denied_rule(&mut events, Duration::from_millis(50))
            .await
            .expect("Cancelled event missing");
        assert!(rule.contains("legal"), "rule = {rule}");

        // Hook fired exactly once with the matching rule + via.
        let calls = hook.calls();
        assert_eq!(calls.len(), 1, "hook called {} times", calls.len());
        let (hook_rule, hook_via) = &calls[0];
        assert!(hook_rule.contains("legal"), "hook rule = {hook_rule}");
        assert!(
            matches!(
                hook_via,
                EscalationMode::Notify { destination } if destination == "ops@example.com",
            ),
            "hook via = {hook_via:?}",
        );

        // Backend never saw a response_create — escalation blocks
        // emission, same as plain Denied.
        assert!(backend.record_calls().is_empty());
    }

    #[tokio::test]
    async fn deny_topic_with_leave_meeting_drives_escalation_hook() {
        let backend = TestRealtimeBackend::new(caps_atomic());
        let session = backend.session();
        let mut profile = open_profile();
        profile.deny_topics = vec!["pricing".into()];
        profile.escalation = EscalationMode::LeaveMeeting;
        let hook = RecordingEscalationHook::new();
        let controller = DefaultSpeechController::with_escalation_hook(
            Arc::clone(&backend) as _,
            session,
            profile,
            Arc::clone(&hook) as _,
        );
        let mut events = controller.subscribe_events();

        let _ = controller
            .speak("let's discuss pricing", Priority::Append, None)
            .await
            .expect_err("escalated");

        // Audit event present.
        assert!(
            first_policy_denied_rule(&mut events, Duration::from_millis(50))
                .await
                .is_some(),
            "Cancelled event missing for LeaveMeeting escalation",
        );

        // Hook saw the LeaveMeeting variant.
        let calls = hook.calls();
        assert_eq!(calls.len(), 1);
        assert!(matches!(calls[0].1, EscalationMode::LeaveMeeting));
    }

    #[tokio::test]
    async fn allow_list_miss_emits_policy_denied_event_no_hook() {
        let backend = TestRealtimeBackend::new(caps_atomic());
        let session = backend.session();
        let mut profile = open_profile();
        profile.allow_topics = vec!["pricing".into()];
        let hook = RecordingEscalationHook::new();
        let controller = DefaultSpeechController::with_escalation_hook(
            Arc::clone(&backend) as _,
            session,
            profile,
            Arc::clone(&hook) as _,
        );
        let mut events = controller.subscribe_events();

        let _ = controller
            .speak("the weather is great", Priority::Append, None)
            .await
            .expect_err("not_in_allow_list");

        let rule = first_policy_denied_rule(&mut events, Duration::from_millis(50))
            .await
            .expect("Cancelled event missing");
        assert_eq!(rule, "not_in_allow_list");

        // The allow-list miss is a plain Denied, not an escalation —
        // the hook must stay silent.
        assert!(hook.calls().is_empty());
    }

    #[tokio::test]
    async fn allow_list_hit_lets_speak_proceed_no_hook_no_extra_cancelled() {
        let backend = TestRealtimeBackend::new(caps_atomic());
        let session = backend.session();
        let mut profile = open_profile();
        profile.allow_topics = vec!["pricing".into()];
        let hook = RecordingEscalationHook::new();
        let controller = DefaultSpeechController::with_escalation_hook(
            Arc::clone(&backend) as _,
            session,
            profile,
            Arc::clone(&hook) as _,
        );
        let mut events = controller.subscribe_events();

        let _ = controller
            .speak("share the Q3 pricing", Priority::Append, None)
            .await
            .expect("allowed");

        // Backend received exactly one response_create — the speak
        // path proceeded normally.
        let creates = backend
            .record_calls()
            .into_iter()
            .filter(|c| matches!(c, BackendCall::ResponseCreate { .. }))
            .count();
        assert_eq!(creates, 1);

        // Started fires; no PolicyDenied audit entry.
        let evs = drain_events(&mut events, 2, Duration::from_millis(50)).await;
        assert!(
            evs.iter().any(|e| matches!(e, SpeechEvent::Started { .. })),
            "missing Started: {evs:?}",
        );
        assert!(
            !evs.iter().any(|e| matches!(
                e,
                SpeechEvent::Cancelled {
                    reason: CancelReason::PolicyDenied { .. },
                    ..
                }
            )),
            "Allowed path must not emit PolicyDenied: {evs:?}",
        );

        // Hook silent on the happy path.
        assert!(hook.calls().is_empty());
    }

    /// Hook impl that blocks forever. Used to pin that the
    /// controller's `speak_lock` is *not* held across the hook's
    /// await — regression guard for the multi-model review finding.
    struct BlockingEscalationHook;

    #[async_trait]
    impl EscalationHook for BlockingEscalationHook {
        async fn escalate(&self, _rule: String, _via: EscalationMode) {
            // Park forever. If the controller awaits this on the
            // speak path, every subsequent `speak` / `cancel` will
            // hang and this test will time out.
            std::future::pending::<()>().await;
        }
    }

    #[tokio::test]
    async fn slow_escalation_hook_does_not_block_speak_lock() {
        // Pins the fix for the multi-model review's Major finding:
        // the controller must NOT hold `speak_lock` across the
        // escalation hook's await. A hook that hangs (or takes
        // seconds to fire a webhook) would otherwise serialize every
        // later `speak`/`cancel` behind it, turning a missing
        // webhook ack into a controller-wide availability failure.
        let backend = TestRealtimeBackend::new(caps_atomic());
        let session = backend.session();
        let mut profile = open_profile();
        profile.deny_topics = vec!["legal".into()];
        profile.escalation = EscalationMode::Notify {
            destination: "ops@example.com".into(),
        };
        let controller = DefaultSpeechController::with_escalation_hook(
            Arc::clone(&backend) as _,
            session,
            profile,
            Arc::new(BlockingEscalationHook),
        );

        // Trigger an escalation. The hook will park forever, but
        // `speak()` must still return promptly with PolicyDenied
        // because the hook is dispatched detached.
        let escalated = timeout(
            Duration::from_secs(2),
            controller.speak("send the legal contract", Priority::Append, None),
        )
        .await
        .expect("speak() must not block on hook")
        .expect_err("policy denied");
        assert!(matches!(escalated, SpeechError::PolicyDenied { .. }));

        // And follow-up controller calls also proceed without
        // waiting on the parked hook.
        let cancel = timeout(
            Duration::from_secs(2),
            controller.cancel(UtteranceId::now_v7()),
        )
        .await
        .expect("cancel() must not block on hook");
        assert!(cancel.is_ok());
    }
}
