//! In-memory scripted [`RealtimeBackend`] for tests and integration
//! suites.
//!
//! The mock satisfies the [`RealtimeBackend`] trait without contacting
//! a vendor: every method is local, every event is broadcast through a
//! per-session [`tokio::sync::broadcast`] channel, and a small scripting
//! API on the concrete struct lets a test drive the canned response
//! stream the consumer (`heron-policy::SpeechController`,
//! `heron-orchestrator::LocalSessionOrchestrator`) would observe from
//! a real backend.
//!
//! See [`docs/archives/api-design-spec.md`](../../../docs/archives/api-design-spec.md)
//! §6 (session lifecycle) and §9 (speech-control contract / capability
//! matrix) for the contract being modelled. The real `OpenAiRealtime`
//! backend ships in a follow-up PR; until then, every consumer of the
//! trait codes against this mock.
//!
//! ## Concurrency model
//!
//! State lives behind a single [`std::sync::Mutex`] because (a) every
//! critical section is a HashMap lookup + a [`broadcast::Sender::send`]
//! at most, never an `.await`, and (b) [`RealtimeBackend::subscribe_events`]
//! is a synchronous trait method, so `tokio::sync::Mutex` would force a
//! `blocking_lock` that panics from inside a Tokio runtime. Poisoned
//! locks are recovered via [`std::sync::PoisonError::into_inner`] (the
//! same pattern `heron-orchestrator` uses for short-lived locks) so we
//! satisfy the workspace `unwrap_used = "deny"` lint without surfacing
//! poison as a user-visible error.
//!
//! ## Scripting surface
//!
//! The construction-time helpers — [`MockRealtimeBackend::script_emit`],
//! [`MockRealtimeBackend::script_response`],
//! [`MockRealtimeBackend::script_input_speech`],
//! [`MockRealtimeBackend::script_tool_call`],
//! [`MockRealtimeBackend::expect_tool_result`] — are *not* on the
//! [`RealtimeBackend`] trait. They exist only on the concrete struct so
//! that test code can drive the backend deterministically and assert on
//! tool-result captures. Production code holds a `dyn RealtimeBackend`
//! and never sees them.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::Utc;
use tokio::sync::broadcast;

use crate::{
    RealtimeBackend, RealtimeCapabilities, RealtimeError, RealtimeEvent, ResponseId, SessionConfig,
    SessionId, validate_session,
};

/// Per-session broadcast capacity. Big enough that a typical scripted
/// response (a handful of deltas + audio + done) can be enqueued before
/// any subscriber polls without lagging; small enough that a runaway
/// script in a test surfaces backpressure quickly. Tests that need
/// long event streams can subscribe before scripting.
const BROADCAST_CAPACITY: usize = 256;

/// Per-session bookkeeping. Held inside the backend's single
/// [`Mutex`]; never escapes the lock.
struct SessionState {
    sender: broadcast::Sender<RealtimeEvent>,
    /// Responses currently between `ResponseCreated` and `ResponseDone`.
    /// Mutated by [`MockRealtimeBackend::script_response`] /
    /// [`MockRealtimeBackend::script_tool_call`] and observed by
    /// [`RealtimeBackend::response_cancel`] — its
    /// `HashSet::remove` return value is what makes `cancel`
    /// idempotent (a second cancel observes the response is already
    /// gone and emits nothing).
    in_flight: HashSet<ResponseId>,
    /// Latest `audio_end_ms` passed to `truncate_current` for each
    /// response, exposed via [`MockRealtimeBackend::truncate_point`] so
    /// tests can assert the controller's truncate target.
    truncate_points: HashMap<ResponseId, u32>,
    /// Captured tool-call results keyed by `tool_call_id`, exposed via
    /// [`MockRealtimeBackend::expect_tool_result`].
    tool_results: HashMap<String, serde_json::Value>,
    /// Captured `response_create` requests keyed by the minted
    /// `ResponseId`, exposed via
    /// [`MockRealtimeBackend::expect_response_request`] so tests can
    /// assert the controller forwarded the right text and voice.
    requested_responses: HashMap<ResponseId, RequestedResponse>,
}

/// Captured arguments from a [`RealtimeBackend::response_create`] call,
/// retrievable via [`MockRealtimeBackend::expect_response_request`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestedResponse {
    pub text: String,
    pub voice_override: Option<String>,
}

impl SessionState {
    fn new() -> Self {
        let (sender, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            sender,
            in_flight: HashSet::new(),
            truncate_points: HashMap::new(),
            tool_results: HashMap::new(),
            requested_responses: HashMap::new(),
        }
    }
}

struct Inner {
    sessions: HashMap<SessionId, SessionState>,
    capabilities: RealtimeCapabilities,
}

/// In-memory scripted [`RealtimeBackend`].
///
/// See the module docs for the scripting model. Default
/// [`RealtimeCapabilities`] (every primitive `true`) match the
/// "full-fat" backend profile so consumers exercise the `Native`
/// strategies in [`crate::fallback::plan`] by default; downgrade with
/// [`MockRealtimeBackend::with_capabilities`] to drive emulation paths.
pub struct MockRealtimeBackend {
    inner: Mutex<Inner>,
}

impl MockRealtimeBackend {
    /// Construct a backend that advertises every capability as `true`.
    /// See [`Self::with_capabilities`] to drive emulation paths.
    pub fn new() -> Self {
        Self::with_capabilities(RealtimeCapabilities {
            bidirectional_audio: true,
            server_vad: true,
            atomic_response_cancel: true,
            tool_calling: true,
            text_deltas: true,
        })
    }

    /// Construct a backend that reports `caps` from
    /// [`RealtimeBackend::capabilities`]. Use this to drive
    /// [`crate::fallback`] emulation strategies in tests.
    pub fn with_capabilities(caps: RealtimeCapabilities) -> Self {
        Self {
            inner: Mutex::new(Inner {
                sessions: HashMap::new(),
                capabilities: caps,
            }),
        }
    }

    /// Publish a fully-formed [`RealtimeEvent`] to a session's
    /// subscribers. Does not validate ordering; the caller is the test
    /// scripting the scenario. Returns `Err` if `session` is unknown.
    pub fn script_emit(
        &self,
        session: SessionId,
        event: RealtimeEvent,
    ) -> Result<(), RealtimeError> {
        let mut inner = lock(&self.inner);
        let state = inner
            .sessions
            .get_mut(&session)
            .ok_or_else(unknown_session)?;
        // `send` errors only when there are zero receivers, which is a
        // valid state for a script that runs ahead of its subscriber.
        let _ = state.sender.send(event);
        Ok(())
    }

    /// Play a canned response: emits `ResponseCreated`, optionally
    /// `ResponseAudioStarted`, every `ResponseTextDelta`, then
    /// `ResponseDone` if `done_after`. Marks the response in-flight
    /// between created and done so [`RealtimeBackend::response_cancel`]
    /// can observe it.
    ///
    /// `audio_started` distinguishes "the model has begun a response"
    /// from "audio is actually flowing." Real backends (OpenAI
    /// Realtime, Gemini Live) emit `ResponseCreated` strictly before
    /// `ResponseAudioStarted`; tests exercising barge-in policy need
    /// to script that ordering precisely.
    pub fn script_response(
        &self,
        session: SessionId,
        response: ResponseId,
        deltas: &[&str],
        audio_started: bool,
        done_after: bool,
    ) -> Result<(), RealtimeError> {
        let mut inner = lock(&self.inner);
        let state = inner
            .sessions
            .get_mut(&session)
            .ok_or_else(unknown_session)?;
        state.in_flight.insert(response);

        let _ = state.sender.send(RealtimeEvent::ResponseCreated {
            session,
            response,
            at: Utc::now(),
        });
        if audio_started {
            let _ = state.sender.send(RealtimeEvent::ResponseAudioStarted {
                session,
                response,
                at: Utc::now(),
            });
        }
        for delta in deltas {
            let _ = state.sender.send(RealtimeEvent::ResponseTextDelta {
                session,
                response,
                text: (*delta).to_owned(),
            });
        }
        if done_after {
            state.in_flight.remove(&response);
            let _ = state.sender.send(RealtimeEvent::ResponseDone {
                session,
                response,
                at: Utc::now(),
            });
        }
        Ok(())
    }

    /// Emit a sequence of `InputTranscriptDelta` events bracketed by
    /// `InputSpeechStarted` / `InputSpeechStopped`, mimicking what a
    /// server-VAD backend produces for one user utterance.
    ///
    /// Each `(text, is_final)` segment becomes one delta in order. The
    /// final segment's `is_final` flag is preserved so a test can pin
    /// "this is the closing partial" without an extra parameter.
    pub fn script_input_speech(
        &self,
        session: SessionId,
        transcript_segments: &[(&str, bool)],
    ) -> Result<(), RealtimeError> {
        let mut inner = lock(&self.inner);
        let state = inner
            .sessions
            .get_mut(&session)
            .ok_or_else(unknown_session)?;
        let _ = state.sender.send(RealtimeEvent::InputSpeechStarted {
            session,
            at: Utc::now(),
        });
        for (text, is_final) in transcript_segments {
            let _ = state.sender.send(RealtimeEvent::InputTranscriptDelta {
                session,
                text: (*text).to_owned(),
                is_final: *is_final,
            });
        }
        let _ = state.sender.send(RealtimeEvent::InputSpeechStopped {
            session,
            at: Utc::now(),
        });
        Ok(())
    }

    /// Emit a `ToolCall` event so the controller exercises its
    /// tool-dispatch path. The response is treated as in-flight but is
    /// *not* auto-finished — the test drives `tool_result` and a
    /// follow-up `script_response(.., done_after = true)` if it wants
    /// to model the post-tool continuation.
    pub fn script_tool_call(
        &self,
        session: SessionId,
        response: ResponseId,
        tool_call_id: &str,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<(), RealtimeError> {
        let mut inner = lock(&self.inner);
        let state = inner
            .sessions
            .get_mut(&session)
            .ok_or_else(unknown_session)?;
        state.in_flight.insert(response);
        let _ = state.sender.send(RealtimeEvent::ToolCall {
            session,
            response,
            tool_call_id: tool_call_id.to_owned(),
            tool_name: tool_name.to_owned(),
            arguments,
        });
        Ok(())
    }

    /// Retrieve a captured tool-result by `tool_call_id`, the
    /// assertion handle for tests that drove
    /// [`RealtimeBackend::tool_result`]. Returns `None` if the session
    /// is unknown or no result was recorded for the given id.
    pub fn expect_tool_result(
        &self,
        session: SessionId,
        tool_call_id: &str,
    ) -> Option<serde_json::Value> {
        let inner = lock(&self.inner);
        inner
            .sessions
            .get(&session)?
            .tool_results
            .get(tool_call_id)
            .cloned()
    }

    /// Retrieve the most recent `audio_end_ms` recorded by
    /// [`RealtimeBackend::truncate_current`] for `response`. Tests
    /// asserting barge-in cut-points use this. `None` when the session
    /// is unknown or the response was never truncated.
    pub fn truncate_point(&self, session: SessionId, response: ResponseId) -> Option<u32> {
        let inner = lock(&self.inner);
        inner
            .sessions
            .get(&session)?
            .truncate_points
            .get(&response)
            .copied()
    }

    /// Retrieve the captured arguments from a
    /// [`RealtimeBackend::response_create`] call by `response`. Tests
    /// asserting that the controller forwarded the right text or voice
    /// override use this. `None` when the session is unknown or no
    /// `response_create` was recorded for the given id.
    pub fn expect_response_request(
        &self,
        session: SessionId,
        response: ResponseId,
    ) -> Option<RequestedResponse> {
        let inner = lock(&self.inner);
        inner
            .sessions
            .get(&session)?
            .requested_responses
            .get(&response)
            .cloned()
    }
}

impl Default for MockRealtimeBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RealtimeBackend for MockRealtimeBackend {
    async fn session_open(&self, config: SessionConfig) -> Result<SessionId, RealtimeError> {
        validate_session(&config)?;
        let id = SessionId::now_v7();
        let mut inner = lock(&self.inner);
        inner.sessions.insert(id, SessionState::new());
        Ok(id)
    }

    async fn session_close(&self, id: SessionId) -> Result<(), RealtimeError> {
        let mut inner = lock(&self.inner);
        inner.sessions.remove(&id).ok_or_else(unknown_session)?;
        Ok(())
    }

    async fn response_create(
        &self,
        session: SessionId,
        text: &str,
        voice_override: Option<String>,
    ) -> Result<ResponseId, RealtimeError> {
        let response = ResponseId::now_v7();
        let mut inner = lock(&self.inner);
        let state = inner
            .sessions
            .get_mut(&session)
            .ok_or_else(unknown_session)?;
        state.in_flight.insert(response);
        state.requested_responses.insert(
            response,
            RequestedResponse {
                text: text.to_owned(),
                voice_override,
            },
        );
        // Real backends emit `ResponseCreated` synchronously when they
        // accept the request; mirror that so consumers see the event
        // without a separate scripting call. Audio + done events stay
        // the test's responsibility via [`Self::script_response`] /
        // [`Self::script_emit`] so a paused-mid-response scenario is
        // expressible without racing the broadcast.
        let _ = state.sender.send(RealtimeEvent::ResponseCreated {
            session,
            response,
            at: Utc::now(),
        });
        Ok(response)
    }

    async fn response_cancel(
        &self,
        session: SessionId,
        response: ResponseId,
    ) -> Result<(), RealtimeError> {
        let mut inner = lock(&self.inner);
        let state = inner
            .sessions
            .get_mut(&session)
            .ok_or_else(unknown_session)?;
        // Idempotent by construction: only synthesize `ResponseDone`
        // if the response was actually in flight. The first cancel
        // removes it from `in_flight` and emits Done; every
        // subsequent cancel sees `remove` return false and is a
        // silent `Ok(())`. Cancelling a response that already
        // finished naturally takes the same silent branch.
        if state.in_flight.remove(&response) {
            let _ = state.sender.send(RealtimeEvent::ResponseDone {
                session,
                response,
                at: Utc::now(),
            });
        }
        Ok(())
    }

    async fn truncate_current(
        &self,
        session: SessionId,
        audio_end_ms: u32,
    ) -> Result<(), RealtimeError> {
        let mut inner = lock(&self.inner);
        let state = inner
            .sessions
            .get_mut(&session)
            .ok_or_else(unknown_session)?;
        // The trait doesn't take a `ResponseId` here (it mirrors
        // OpenAI's `conversation.item.truncate` which always targets
        // "the current item"). Record the truncate against every
        // currently in-flight response so tests that scripted
        // exactly one in-flight response can assert against it
        // without guessing which id was current. Destructured to make
        // the disjoint-field borrow explicit to the borrow checker.
        let SessionState {
            in_flight,
            truncate_points,
            ..
        } = state;
        for response in in_flight.iter().copied() {
            truncate_points.insert(response, audio_end_ms);
        }
        Ok(())
    }

    async fn tool_result(
        &self,
        session: SessionId,
        tool_call_id: String,
        result: serde_json::Value,
    ) -> Result<(), RealtimeError> {
        let mut inner = lock(&self.inner);
        let state = inner
            .sessions
            .get_mut(&session)
            .ok_or_else(unknown_session)?;
        state.tool_results.insert(tool_call_id, result);
        Ok(())
    }

    fn subscribe_events(&self, id: SessionId) -> broadcast::Receiver<RealtimeEvent> {
        // The trait method is sync and infallible, so an unknown
        // `SessionId` can't surface as a `Result::Err`. Return a
        // pre-closed receiver in that case: the caller's
        // `recv().await` resolves to `Err(RecvError::Closed)`
        // immediately, surfacing the misuse without polluting the
        // session map (mutating the map here would silently make a
        // later `script_emit` for the same id succeed).
        let inner = lock(&self.inner);
        if let Some(state) = inner.sessions.get(&id) {
            state.sender.subscribe()
        } else {
            let (tx, rx) = broadcast::channel::<RealtimeEvent>(1);
            drop(tx);
            rx
        }
    }

    fn capabilities(&self) -> RealtimeCapabilities {
        lock(&self.inner).capabilities
    }
}

/// Acquire `inner`'s lock, recovering from poison. Mirrors
/// `heron-orchestrator::lock_or_recover`: every critical section here
/// is a brief HashMap mutation, so a panicking thread mid-section
/// leaves no half-built invariant a later thread couldn't proceed
/// past.
fn lock(m: &Mutex<Inner>) -> std::sync::MutexGuard<'_, Inner> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn unknown_session() -> RealtimeError {
    // Pin the exact wording so tests in this crate and downstream
    // (heron-policy / heron-orchestrator) can match on it. The trait
    // surface uses `RealtimeError::Backend` for "the backend rejected
    // this op for a non-network reason"; an unknown session id is
    // exactly that shape.
    RealtimeError::Backend("session not found".to_owned())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::{ToolSpec, TurnDetection};
    use serde_json::json;
    use std::time::Duration;
    use tokio::sync::broadcast::error::TryRecvError;

    fn turn_detection() -> TurnDetection {
        TurnDetection {
            vad_threshold: 0.5,
            prefix_padding_ms: 300,
            silence_duration_ms: 500,
            interrupt_response: true,
            auto_create_response: true,
        }
    }

    fn config() -> SessionConfig {
        SessionConfig {
            system_prompt: "You are a helpful meeting assistant.".to_owned(),
            tools: vec![],
            turn_detection: turn_detection(),
            voice: "alloy".to_owned(),
        }
    }

    /// Drain a receiver until the predicate fires, with a hard timeout
    /// so a missed event fails fast instead of hanging the test.
    async fn drain_until<F>(
        rx: &mut broadcast::Receiver<RealtimeEvent>,
        mut stop: F,
    ) -> Vec<RealtimeEvent>
    where
        F: FnMut(&RealtimeEvent) -> bool,
    {
        let mut out = Vec::new();
        let deadline = tokio::time::sleep(Duration::from_secs(2));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                ev = rx.recv() => {
                    let ev = ev.expect("receiver closed before stop predicate fired");
                    let last = stop(&ev);
                    out.push(ev);
                    if last {
                        return out;
                    }
                }
                () = &mut deadline => {
                    panic!("timed out waiting for stop predicate; got: {out:?}");
                }
            }
        }
    }

    #[tokio::test]
    async fn roundtrip_open_subscribe_script_response() {
        let backend = MockRealtimeBackend::new();
        let session = backend.session_open(config()).await.expect("open session");
        let mut rx = backend.subscribe_events(session);

        let response = ResponseId::now_v7();
        backend
            .script_response(session, response, &["hello, ", "world"], true, true)
            .expect("script response");

        let events = drain_until(&mut rx, |ev| {
            matches!(ev, RealtimeEvent::ResponseDone { .. })
        })
        .await;

        // Expected order: Created, AudioStarted, two TextDelta, Done.
        assert_eq!(events.len(), 5, "got: {events:?}");
        assert!(matches!(events[0], RealtimeEvent::ResponseCreated { .. }));
        assert!(matches!(
            events[1],
            RealtimeEvent::ResponseAudioStarted { .. }
        ));
        match &events[2] {
            RealtimeEvent::ResponseTextDelta { text, .. } => assert_eq!(text, "hello, "),
            other => panic!("expected TextDelta, got {other:?}"),
        }
        match &events[3] {
            RealtimeEvent::ResponseTextDelta { text, .. } => assert_eq!(text, "world"),
            other => panic!("expected TextDelta, got {other:?}"),
        }
        assert!(matches!(events[4], RealtimeEvent::ResponseDone { .. }));
    }

    #[tokio::test]
    async fn session_open_rejects_oversize_prompt() {
        let backend = MockRealtimeBackend::new();
        let mut c = config();
        c.system_prompt = "x".repeat(crate::MAX_SYSTEM_PROMPT_BYTES + 1);
        let err = backend.session_open(c).await.expect_err("oversize");
        assert!(matches!(err, RealtimeError::PromptTooLarge), "got {err:?}");
    }

    #[tokio::test]
    async fn session_open_rejects_oversize_tool_list() {
        let backend = MockRealtimeBackend::new();
        let mut c = config();
        c.tools = (0..=crate::MAX_TOOL_COUNT)
            .map(|i| ToolSpec {
                name: format!("tool_{i}"),
                description: "test".to_owned(),
                parameters_schema: json!({"type": "object"}),
            })
            .collect();
        let err = backend.session_open(c).await.expect_err("too many tools");
        match err {
            RealtimeError::BadConfig(s) => assert!(s.contains("exceeds"), "got: {s}"),
            other => panic!("expected BadConfig, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn operations_on_closed_session_return_error() {
        let backend = MockRealtimeBackend::new();
        let session = backend.session_open(config()).await.expect("open");
        backend.session_close(session).await.expect("close");

        let response = ResponseId::now_v7();
        let cancel_err = backend
            .response_cancel(session, response)
            .await
            .expect_err("cancel on closed");
        assert!(matches!(cancel_err, RealtimeError::Backend(s) if s == "session not found"));

        let truncate_err = backend
            .truncate_current(session, 1_000)
            .await
            .expect_err("truncate on closed");
        assert!(matches!(truncate_err, RealtimeError::Backend(_)));

        let tool_err = backend
            .tool_result(session, "call_x".to_owned(), json!({}))
            .await
            .expect_err("tool_result on closed");
        assert!(matches!(tool_err, RealtimeError::Backend(_)));

        let close_err = backend
            .session_close(session)
            .await
            .expect_err("double close");
        assert!(matches!(close_err, RealtimeError::Backend(_)));
    }

    #[tokio::test]
    async fn response_cancel_is_idempotent() {
        let backend = MockRealtimeBackend::new();
        let session = backend.session_open(config()).await.expect("open");
        let mut rx = backend.subscribe_events(session);

        let response = ResponseId::now_v7();
        backend
            .script_response(session, response, &["mid"], false, false)
            .expect("script in-flight response");

        backend
            .response_cancel(session, response)
            .await
            .expect("first cancel");
        // Second call must be Ok and must NOT emit a duplicate
        // `ResponseDone`.
        backend
            .response_cancel(session, response)
            .await
            .expect("second cancel idempotent");

        let events = drain_until(&mut rx, |ev| {
            matches!(ev, RealtimeEvent::ResponseDone { .. })
        })
        .await;
        let dones = events
            .iter()
            .filter(|e| matches!(e, RealtimeEvent::ResponseDone { .. }))
            .count();
        assert_eq!(dones, 1, "expected exactly one synthesized Done");

        // No further events should be queued after the second cancel.
        match rx.try_recv() {
            Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => {}
            other => panic!("expected empty/closed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn response_cancel_after_done_is_ok_and_emits_nothing() {
        let backend = MockRealtimeBackend::new();
        let session = backend.session_open(config()).await.expect("open");
        let mut rx = backend.subscribe_events(session);

        let response = ResponseId::now_v7();
        backend
            .script_response(session, response, &["x"], false, true)
            .expect("script complete response");

        // Drain until Done arrives.
        let _ = drain_until(&mut rx, |ev| {
            matches!(ev, RealtimeEvent::ResponseDone { .. })
        })
        .await;

        backend
            .response_cancel(session, response)
            .await
            .expect("cancel-after-done is Ok");

        // Nothing further should be broadcast.
        match rx.try_recv() {
            Err(TryRecvError::Empty) => {}
            other => panic!("expected empty receiver, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn response_create_emits_created_and_records_request() {
        let backend = MockRealtimeBackend::new();
        let session = backend.session_open(config()).await.expect("open");
        let mut rx = backend.subscribe_events(session);

        let response = backend
            .response_create(session, "hello there", Some("voice_alt".to_owned()))
            .await
            .expect("response_create");

        let events = drain_until(&mut rx, |ev| {
            matches!(ev, RealtimeEvent::ResponseCreated { .. })
        })
        .await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            RealtimeEvent::ResponseCreated {
                session: s,
                response: r,
                ..
            } => {
                assert_eq!(*s, session);
                assert_eq!(*r, response);
            }
            other => panic!("expected ResponseCreated, got {other:?}"),
        }

        let req = backend
            .expect_response_request(session, response)
            .expect("captured");
        assert_eq!(req.text, "hello there");
        assert_eq!(req.voice_override.as_deref(), Some("voice_alt"));

        // Response is now in flight; cancel observes it and emits Done.
        backend
            .response_cancel(session, response)
            .await
            .expect("cancel in-flight response");
        let after_cancel = drain_until(&mut rx, |ev| {
            matches!(ev, RealtimeEvent::ResponseDone { .. })
        })
        .await;
        assert!(
            after_cancel
                .iter()
                .any(|e| matches!(e, RealtimeEvent::ResponseDone { .. }))
        );
    }

    #[tokio::test]
    async fn response_create_on_unknown_session_errors() {
        let backend = MockRealtimeBackend::new();
        let phantom = SessionId::now_v7();
        let err = backend
            .response_create(phantom, "x", None)
            .await
            .expect_err("unknown session");
        assert!(matches!(err, RealtimeError::Backend(_)));
    }

    #[tokio::test]
    async fn tool_result_is_captured_and_retrievable() {
        let backend = MockRealtimeBackend::new();
        let session = backend.session_open(config()).await.expect("open");

        let payload = json!({"answer": 42, "ok": true});
        backend
            .tool_result(session, "call_abc".to_owned(), payload.clone())
            .await
            .expect("inject result");

        let got = backend
            .expect_tool_result(session, "call_abc")
            .expect("captured");
        assert_eq!(got, payload);

        // Unknown tool_call_id => None, not an error.
        assert!(
            backend
                .expect_tool_result(session, "call_missing")
                .is_none()
        );
    }

    #[tokio::test]
    async fn capabilities_round_trip_what_was_configured() {
        let caps = RealtimeCapabilities {
            bidirectional_audio: true,
            server_vad: false,
            atomic_response_cancel: false,
            tool_calling: true,
            text_deltas: false,
        };
        let backend = MockRealtimeBackend::with_capabilities(caps);
        let got = backend.capabilities();
        assert_eq!(got.bidirectional_audio, caps.bidirectional_audio);
        assert_eq!(got.server_vad, caps.server_vad);
        assert_eq!(got.atomic_response_cancel, caps.atomic_response_cancel);
        assert_eq!(got.tool_calling, caps.tool_calling);
        assert_eq!(got.text_deltas, caps.text_deltas);
    }

    #[tokio::test]
    async fn truncate_records_audio_end_against_in_flight_response() {
        let backend = MockRealtimeBackend::new();
        let session = backend.session_open(config()).await.expect("open");
        let response = ResponseId::now_v7();
        backend
            .script_response(session, response, &["hi"], true, false)
            .expect("script in-flight");

        backend
            .truncate_current(session, 1_234)
            .await
            .expect("truncate");

        assert_eq!(backend.truncate_point(session, response), Some(1_234));
    }

    #[tokio::test]
    async fn concurrent_sessions_stay_isolated() {
        let backend = MockRealtimeBackend::new();
        let s_a = backend.session_open(config()).await.expect("open A");
        let s_b = backend.session_open(config()).await.expect("open B");

        let mut rx_a = backend.subscribe_events(s_a);
        let mut rx_b = backend.subscribe_events(s_b);

        let r_a = ResponseId::now_v7();
        backend
            .script_response(s_a, r_a, &["a-only"], false, true)
            .expect("script A");

        // A's stream sees Created + delta + Done (no audio).
        let a_events = drain_until(&mut rx_a, |ev| {
            matches!(ev, RealtimeEvent::ResponseDone { .. })
        })
        .await;
        assert_eq!(a_events.len(), 3);

        // B's stream sees nothing.
        match rx_b.try_recv() {
            Err(TryRecvError::Empty) => {}
            other => panic!("session B saw a leaked event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn scripted_event_round_trips_through_serde() {
        // Wire-shape regression mirror of `prefix_tests::
        // realtime_event_round_trips_with_prefixed_ids`: an event the
        // mock just emitted should serialize to the same wire form
        // and deserialize back to an equal value.
        let backend = MockRealtimeBackend::new();
        let session = backend.session_open(config()).await.expect("open");
        let mut rx = backend.subscribe_events(session);

        let response = ResponseId::now_v7();
        backend
            .script_response(session, response, &[], false, false)
            .expect("emit Created");

        let event = match rx.recv().await.expect("recv created") {
            ev @ RealtimeEvent::ResponseCreated { .. } => ev,
            other => panic!("expected ResponseCreated, got {other:?}"),
        };

        let json = serde_json::to_string(&event).expect("serialize");
        assert!(
            json.contains(r#""session":"session_"#),
            "missing session prefix on the wire: {json}"
        );
        assert!(
            json.contains(r#""response":"resp_"#),
            "missing response prefix on the wire: {json}"
        );
        let _back: RealtimeEvent = serde_json::from_str(&json).expect("deserialize");
    }

    #[tokio::test]
    async fn script_input_speech_brackets_segments_with_started_stopped() {
        let backend = MockRealtimeBackend::new();
        let session = backend.session_open(config()).await.expect("open");
        let mut rx = backend.subscribe_events(session);

        backend
            .script_input_speech(session, &[("hello", false), ("hello world", true)])
            .expect("script speech");

        let events = drain_until(&mut rx, |ev| {
            matches!(ev, RealtimeEvent::InputSpeechStopped { .. })
        })
        .await;
        assert_eq!(events.len(), 4);
        assert!(matches!(
            events[0],
            RealtimeEvent::InputSpeechStarted { .. }
        ));
        match &events[1] {
            RealtimeEvent::InputTranscriptDelta { text, is_final, .. } => {
                assert_eq!(text, "hello");
                assert!(!is_final);
            }
            other => panic!("expected InputTranscriptDelta, got {other:?}"),
        }
        match &events[2] {
            RealtimeEvent::InputTranscriptDelta { text, is_final, .. } => {
                assert_eq!(text, "hello world");
                assert!(is_final);
            }
            other => panic!("expected InputTranscriptDelta, got {other:?}"),
        }
        assert!(matches!(
            events[3],
            RealtimeEvent::InputSpeechStopped { .. }
        ));
    }

    #[tokio::test]
    async fn script_tool_call_emits_tool_call_event() {
        let backend = MockRealtimeBackend::new();
        let session = backend.session_open(config()).await.expect("open");
        let mut rx = backend.subscribe_events(session);

        let response = ResponseId::now_v7();
        backend
            .script_tool_call(
                session,
                response,
                "call_42",
                "lookup",
                json!({"q": "weather"}),
            )
            .expect("script tool call");

        let event = rx.recv().await.expect("recv tool call");
        match event {
            RealtimeEvent::ToolCall {
                tool_call_id,
                tool_name,
                arguments,
                ..
            } => {
                assert_eq!(tool_call_id, "call_42");
                assert_eq!(tool_name, "lookup");
                assert_eq!(arguments, json!({"q": "weather"}));
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }

        // The response is in-flight: a follow-up cancel should
        // synthesize a `ResponseDone` (the tool-call path is
        // observable to the cancel logic).
        backend
            .response_cancel(session, response)
            .await
            .expect("cancel after tool call");
        let done = rx.recv().await.expect("recv done");
        assert!(matches!(done, RealtimeEvent::ResponseDone { .. }));
    }

    #[tokio::test]
    async fn script_emit_on_unknown_session_errors() {
        // Gate the scripting helpers on session existence so a typo
        // in a test surfaces locally rather than as a missing event.
        let backend = MockRealtimeBackend::new();
        let unknown = SessionId::now_v7();
        let err = backend
            .script_emit(
                unknown,
                RealtimeEvent::Error {
                    session: unknown,
                    error: "ignored".to_owned(),
                },
            )
            .expect_err("unknown session");
        assert!(matches!(err, RealtimeError::Backend(_)));
    }

    #[tokio::test]
    async fn subscribe_on_unknown_session_returns_closed_receiver() {
        // The trait's `subscribe_events` is sync + infallible, so
        // unknown ids surface as a pre-closed receiver. Critically,
        // calling subscribe MUST NOT make the session "exist" — a
        // later `script_emit` for the same id should still error.
        let backend = MockRealtimeBackend::new();
        let unknown = SessionId::now_v7();

        let mut rx = backend.subscribe_events(unknown);
        match rx.recv().await {
            Err(broadcast::error::RecvError::Closed) => {}
            other => panic!("expected Closed, got {other:?}"),
        }

        let err = backend
            .script_emit(
                unknown,
                RealtimeEvent::Error {
                    session: unknown,
                    error: "ignored".to_owned(),
                },
            )
            .expect_err("subscribe must not auto-create session");
        assert!(matches!(err, RealtimeError::Backend(_)));
    }

    #[tokio::test]
    async fn subscribe_after_session_close_returns_closed_receiver() {
        // Once a session is closed, its broadcast::Sender is
        // dropped — any prior receiver sees Closed, and a fresh
        // subscribe also gets a closed receiver since the session
        // is unknown again.
        let backend = MockRealtimeBackend::new();
        let session = backend.session_open(config()).await.expect("open");
        let mut rx_before = backend.subscribe_events(session);
        backend.session_close(session).await.expect("close");

        match rx_before.recv().await {
            Err(broadcast::error::RecvError::Closed) => {}
            other => panic!("expected Closed on prior subscriber, got {other:?}"),
        }

        let mut rx_after = backend.subscribe_events(session);
        match rx_after.recv().await {
            Err(broadcast::error::RecvError::Closed) => {}
            other => panic!("expected Closed on post-close subscriber, got {other:?}"),
        }
    }
}
