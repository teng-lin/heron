//! Composition point for the v2 live meeting stack.
//!
//! The lower-layer crates now have concrete pieces (`RecallDriver`,
//! `NaiveBridge`, `DefaultSpeechController`, realtime backends), but
//! none of those crates should decide how a meeting session is
//! assembled or torn down. This module is the orchestrator-owned
//! boundary that creates them in order and returns one handle that owns
//! their shared lifetime.

use std::fmt::Display;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use heron_bot::{BotCreateArgs, BotError, BotId, MeetingBotDriver};
use heron_bridge::{AudioBridge, BridgeHealth, NaiveBridge};
use heron_policy::{
    DefaultSpeechController, PolicyProfile, SpeechController,
    ValidationError as PolicyValidationError, validate as validate_policy,
};
use heron_realtime::{RealtimeBackend, RealtimeError, SessionConfig, SessionId, validate_session};
use heron_types::MeetingId;
use thiserror::Error;
use tokio::time::timeout;

/// Default budget for best-effort cleanup after startup fails.
pub const DEFAULT_STARTUP_CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);

/// Default per-step budget for normal async shutdown.
pub const DEFAULT_SHUTDOWN_STEP_TIMEOUT: Duration = Duration::from_secs(10);

/// Inputs needed to start one live v2 meeting session.
#[derive(Debug, Clone)]
pub struct LiveSessionStartArgs {
    /// Public meeting identity minted by the desktop/daemon
    /// orchestrator. The bot and realtime layers each mint their own
    /// typed ids; this keeps the outer session tied to the v1/v2
    /// daemon API's `MeetingId`.
    pub meeting_id: MeetingId,
    pub bot: BotCreateArgs,
    pub realtime: SessionConfig,
    pub policy: PolicyProfile,
}

/// Creates and owns live session components.
///
/// The bridge is supplied as a factory because most bridge
/// implementations spawn runtime-bound forwarding tasks at
/// construction time. Keeping creation inside `start` guarantees the
/// bridge lifetime matches the bot + realtime session.
pub struct LiveSessionOwner<D, B, F, A>
where
    D: MeetingBotDriver + 'static,
    B: RealtimeBackend + 'static,
    F: Fn() -> A + Send + Sync + 'static,
    A: AudioBridge + 'static,
{
    bot_driver: Arc<D>,
    realtime_backend: Arc<B>,
    bridge_factory: F,
    startup_cleanup_timeout: Duration,
    shutdown_step_timeout: Duration,
}

impl<D, B, F, A> LiveSessionOwner<D, B, F, A>
where
    D: MeetingBotDriver + 'static,
    B: RealtimeBackend + 'static,
    F: Fn() -> A + Send + Sync + 'static,
    A: AudioBridge + 'static,
{
    pub fn new(bot_driver: Arc<D>, realtime_backend: Arc<B>, bridge_factory: F) -> Self {
        Self {
            bot_driver,
            realtime_backend,
            bridge_factory,
            startup_cleanup_timeout: DEFAULT_STARTUP_CLEANUP_TIMEOUT,
            shutdown_step_timeout: DEFAULT_SHUTDOWN_STEP_TIMEOUT,
        }
    }

    /// Override the best-effort cleanup budget used when startup
    /// creates a bot but cannot open realtime.
    pub fn with_startup_cleanup_timeout(mut self, timeout: Duration) -> Self {
        self.startup_cleanup_timeout = timeout;
        self
    }

    /// Override the per-step budget used by [`LiveSession::shutdown`].
    pub fn with_shutdown_step_timeout(mut self, timeout: Duration) -> Self {
        self.shutdown_step_timeout = timeout;
        self
    }

    /// Start one live session by creating the bot, opening realtime,
    /// installing policy, and retaining the bridge for audio adapters.
    ///
    /// Validation happens before vendor side effects. If realtime
    /// startup fails after the bot was created, the owner makes a
    /// best-effort pre-join termination so callers do not leak a
    /// vendor bot.
    pub async fn start(
        &self,
        args: LiveSessionStartArgs,
    ) -> Result<LiveSession<D, B, A>, LiveSessionError> {
        validate_session(&args.realtime)?;
        validate_policy(&args.policy)?;

        let bridge = (self.bridge_factory)();
        let bot_id = self.bot_driver.bot_create(args.bot).await?;

        let realtime_session = match self.realtime_backend.session_open(args.realtime).await {
            Ok(id) => id,
            Err(err) => {
                let cleanup = result_or_timeout(
                    "bot_terminate",
                    self.startup_cleanup_timeout,
                    self.bot_driver.bot_terminate(bot_id),
                )
                .await;
                return Err(LiveSessionError::RealtimeStartup {
                    source: err,
                    bot_cleanup_error: cleanup,
                });
            }
        };

        let controller = DefaultSpeechController::new(
            Arc::clone(&self.realtime_backend),
            realtime_session,
            args.policy,
        );

        Ok(LiveSession {
            meeting_id: args.meeting_id,
            bot_id,
            realtime_session,
            bot_driver: Arc::clone(&self.bot_driver),
            realtime_backend: Arc::clone(&self.realtime_backend),
            bridge,
            controller,
            shutdown_step_timeout: self.shutdown_step_timeout,
            shutdown: false,
        })
    }
}

/// Function item suitable for [`LiveSessionOwner::new`] when the
/// current test/prototype bridge is acceptable.
pub fn naive_bridge() -> NaiveBridge {
    NaiveBridge::with_defaults()
}

/// Current concrete owner shape for the Recall-backed bot path with
/// the available naive bridge. Swap the bridge type/factory when a
/// production AEC bridge lands.
pub type RecallNaiveSessionOwner<B> =
    LiveSessionOwner<heron_bot::RecallDriver, B, fn() -> NaiveBridge, NaiveBridge>;

pub fn recall_naive_session_owner<B>(
    bot_driver: Arc<heron_bot::RecallDriver>,
    realtime_backend: Arc<B>,
) -> RecallNaiveSessionOwner<B>
where
    B: RealtimeBackend + 'static,
{
    LiveSessionOwner::new(bot_driver, realtime_backend, naive_bridge)
}

/// Owns all resources for one live v2 meeting.
pub struct LiveSession<D, B, A>
where
    D: MeetingBotDriver + 'static,
    B: RealtimeBackend + 'static,
    A: AudioBridge + 'static,
{
    meeting_id: MeetingId,
    bot_id: BotId,
    realtime_session: SessionId,
    bot_driver: Arc<D>,
    realtime_backend: Arc<B>,
    bridge: A,
    controller: DefaultSpeechController<B>,
    shutdown_step_timeout: Duration,
    shutdown: bool,
}

impl<D, B, A> LiveSession<D, B, A>
where
    D: MeetingBotDriver + 'static,
    B: RealtimeBackend + 'static,
    A: AudioBridge + 'static,
{
    pub fn meeting_id(&self) -> MeetingId {
        self.meeting_id
    }

    pub fn bot_id(&self) -> BotId {
        self.bot_id
    }

    pub fn realtime_session(&self) -> SessionId {
        self.realtime_session
    }

    pub fn speech_controller(&self) -> &dyn SpeechController {
        &self.controller
    }

    pub fn bridge(&self) -> &A {
        &self.bridge
    }

    pub fn bridge_health(&self) -> BridgeHealth {
        self.bridge.health()
    }

    /// Tear down the session in dependency order while attempting
    /// every cleanup step: stop speech, close realtime, then leave the
    /// meeting bot.
    pub async fn shutdown(mut self) -> Result<(), LiveSessionError> {
        let speech = result_or_timeout(
            "cancel_current_and_clear",
            self.shutdown_step_timeout,
            self.controller.cancel_current_and_clear(),
        )
        .await;
        let realtime = result_or_timeout(
            "session_close",
            self.shutdown_step_timeout,
            self.realtime_backend.session_close(self.realtime_session),
        )
        .await;
        let bot = result_or_timeout(
            "bot_leave",
            self.shutdown_step_timeout,
            self.bot_driver.bot_leave(self.bot_id),
        )
        .await;

        self.shutdown = true;
        if speech.is_some() || realtime.is_some() || bot.is_some() {
            return Err(LiveSessionError::Shutdown {
                speech,
                realtime,
                bot,
            });
        }
        Ok(())
    }
}

async fn result_or_timeout<T, E, F>(
    operation: &'static str,
    budget: Duration,
    fut: F,
) -> Option<String>
where
    E: Display,
    F: Future<Output = Result<T, E>>,
{
    match timeout(budget, fut).await {
        Ok(Ok(_)) => None,
        Ok(Err(err)) => Some(err.to_string()),
        Err(_) => Some(format!(
            "{operation} timed out after {}ms",
            budget.as_millis()
        )),
    }
}

impl<D, B, A> Drop for LiveSession<D, B, A>
where
    D: MeetingBotDriver + 'static,
    B: RealtimeBackend + 'static,
    A: AudioBridge + 'static,
{
    fn drop(&mut self) {
        if !self.shutdown {
            tracing::warn!(
                meeting_id = %self.meeting_id,
                bot_id = %self.bot_id,
                realtime_session = %self.realtime_session,
                "live session dropped without async shutdown",
            );
        }
    }
}

#[derive(Debug, Error)]
pub enum LiveSessionError {
    #[error("bot startup failed: {0}")]
    Bot(#[from] BotError),
    #[error("realtime config failed validation: {0}")]
    RealtimeValidation(#[from] RealtimeError),
    #[error("policy profile failed validation: {0}")]
    PolicyValidation(#[from] PolicyValidationError),
    #[error("realtime startup failed: {source}; bot cleanup: {bot_cleanup_error:?}")]
    RealtimeStartup {
        source: RealtimeError,
        bot_cleanup_error: Option<String>,
    },
    #[error("live session shutdown failed; speech={speech:?}; realtime={realtime:?}; bot={bot:?}")]
    Shutdown {
        speech: Option<String>,
        realtime: Option<String>,
        bot: Option<String>,
    },
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use heron_bot::{
        AttendeeContext, BotCreateArgs, BotState, BotStateEvent, DisclosureProfile,
        DriverCapabilities, PersonaId, Platform, PreMeetingContext,
    };
    use heron_bridge::NaiveBridge;
    use heron_policy::{EscalationMode, Priority};
    use heron_realtime::{
        MockRealtimeBackend, RealtimeCapabilities, RealtimeEvent, ResponseId, ToolSpec,
        TurnDetection,
    };
    use serde_json::json;
    use std::sync::Mutex;
    use tokio::sync::broadcast;
    use uuid::Uuid;

    #[derive(Default)]
    struct FakeBotDriver {
        created: Mutex<Vec<BotId>>,
        left: Mutex<Vec<BotId>>,
        terminated: Mutex<Vec<BotId>>,
    }

    struct FailingRealtimeBackend;

    #[derive(Default)]
    struct HangingTerminateBotDriver {
        created: Mutex<Vec<BotId>>,
    }

    #[derive(Default)]
    struct HangingLeaveBotDriver {
        created: Mutex<Vec<BotId>>,
    }

    #[async_trait]
    impl RealtimeBackend for FailingRealtimeBackend {
        async fn session_open(&self, _config: SessionConfig) -> Result<SessionId, RealtimeError> {
            Err(RealtimeError::Network("offline".to_owned()))
        }

        async fn session_close(&self, _id: SessionId) -> Result<(), RealtimeError> {
            Ok(())
        }

        async fn response_create(
            &self,
            _session: SessionId,
            _text: &str,
            _voice_override: Option<String>,
        ) -> Result<ResponseId, RealtimeError> {
            Err(RealtimeError::Network("offline".to_owned()))
        }

        async fn response_cancel(
            &self,
            _session: SessionId,
            _response: ResponseId,
        ) -> Result<(), RealtimeError> {
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
            let (tx, rx) = broadcast::channel(1);
            drop(tx);
            rx
        }

        fn capabilities(&self) -> RealtimeCapabilities {
            RealtimeCapabilities::default()
        }
    }

    #[async_trait]
    impl MeetingBotDriver for FakeBotDriver {
        async fn bot_create(&self, _args: BotCreateArgs) -> Result<BotId, BotError> {
            let id = BotId::now_v7();
            self.created.lock().unwrap().push(id);
            Ok(id)
        }

        async fn bot_leave(&self, id: BotId) -> Result<(), BotError> {
            self.left.lock().unwrap().push(id);
            Ok(())
        }

        async fn bot_terminate(&self, id: BotId) -> Result<(), BotError> {
            self.terminated.lock().unwrap().push(id);
            Ok(())
        }

        fn current_state(&self, id: BotId) -> Option<BotState> {
            if self.created.lock().unwrap().contains(&id) {
                Some(BotState::InMeeting)
            } else {
                None
            }
        }

        fn subscribe_state(&self, _id: BotId) -> broadcast::Receiver<BotStateEvent> {
            let (tx, rx) = broadcast::channel(1);
            drop(tx);
            rx
        }

        fn capabilities(&self) -> DriverCapabilities {
            DriverCapabilities {
                platforms: &[Platform::Zoom],
                live_partial_transcripts: true,
                granular_eject_reasons: true,
                raw_pcm_access: true,
            }
        }
    }

    #[async_trait]
    impl MeetingBotDriver for HangingTerminateBotDriver {
        async fn bot_create(&self, _args: BotCreateArgs) -> Result<BotId, BotError> {
            let id = BotId::now_v7();
            self.created.lock().unwrap().push(id);
            Ok(id)
        }

        async fn bot_leave(&self, _id: BotId) -> Result<(), BotError> {
            Ok(())
        }

        async fn bot_terminate(&self, _id: BotId) -> Result<(), BotError> {
            std::future::pending().await
        }

        fn current_state(&self, _id: BotId) -> Option<BotState> {
            Some(BotState::Joining)
        }

        fn subscribe_state(&self, _id: BotId) -> broadcast::Receiver<BotStateEvent> {
            let (tx, rx) = broadcast::channel(1);
            drop(tx);
            rx
        }

        fn capabilities(&self) -> DriverCapabilities {
            DriverCapabilities {
                platforms: &[Platform::Zoom],
                live_partial_transcripts: true,
                granular_eject_reasons: true,
                raw_pcm_access: true,
            }
        }
    }

    #[async_trait]
    impl MeetingBotDriver for HangingLeaveBotDriver {
        async fn bot_create(&self, _args: BotCreateArgs) -> Result<BotId, BotError> {
            let id = BotId::now_v7();
            self.created.lock().unwrap().push(id);
            Ok(id)
        }

        async fn bot_leave(&self, _id: BotId) -> Result<(), BotError> {
            std::future::pending().await
        }

        async fn bot_terminate(&self, _id: BotId) -> Result<(), BotError> {
            Ok(())
        }

        fn current_state(&self, _id: BotId) -> Option<BotState> {
            Some(BotState::InMeeting)
        }

        fn subscribe_state(&self, _id: BotId) -> broadcast::Receiver<BotStateEvent> {
            let (tx, rx) = broadcast::channel(1);
            drop(tx);
            rx
        }

        fn capabilities(&self) -> DriverCapabilities {
            DriverCapabilities {
                platforms: &[Platform::Zoom],
                live_partial_transcripts: true,
                granular_eject_reasons: true,
                raw_pcm_access: true,
            }
        }
    }

    fn start_args() -> LiveSessionStartArgs {
        LiveSessionStartArgs {
            meeting_id: MeetingId::now_v7(),
            bot: BotCreateArgs {
                meeting_url: "https://zoom.us/j/123".to_owned(),
                persona_id: PersonaId::now_v7(),
                disclosure: DisclosureProfile {
                    text_template: "Heron is recording and assisting.".to_owned(),
                    objection_patterns: vec![],
                    objection_timeout_secs: 30,
                    re_announce_on_join: false,
                },
                context: PreMeetingContext {
                    agenda: Some("Discuss launch".to_owned()),
                    attendees_known: vec![AttendeeContext {
                        name: "Ada".to_owned(),
                        email: Some("ada@example.com".to_owned()),
                        last_seen_in: None,
                        relationship: None,
                        notes: None,
                    }],
                    related_notes: vec![],
                    user_briefing: None,
                },
                metadata: json!({ "meeting_id": "test" }),
                idempotency_key: Uuid::now_v7(),
            },
            realtime: SessionConfig {
                system_prompt: "You are a concise meeting assistant.".to_owned(),
                tools: vec![ToolSpec {
                    name: "noop".to_owned(),
                    description: "No-op test tool".to_owned(),
                    parameters_schema: json!({ "type": "object", "properties": {} }),
                }],
                turn_detection: TurnDetection {
                    vad_threshold: 0.5,
                    prefix_padding_ms: 300,
                    silence_duration_ms: 500,
                    interrupt_response: true,
                    auto_create_response: true,
                },
                voice: "alloy".to_owned(),
            },
            policy: PolicyProfile {
                allow_topics: vec![],
                deny_topics: vec!["secret".to_owned()],
                mute: false,
                escalation: EscalationMode::None,
            },
        }
    }

    #[tokio::test]
    async fn start_composes_bot_bridge_realtime_and_policy() {
        let bot = Arc::new(FakeBotDriver::default());
        let realtime = Arc::new(MockRealtimeBackend::new());
        let owner = LiveSessionOwner::new(Arc::clone(&bot), Arc::clone(&realtime), naive_bridge);

        let session = owner.start(start_args()).await.expect("start live session");

        assert_eq!(bot.created.lock().unwrap().as_slice(), &[session.bot_id()]);
        assert!(session.bridge_health().aec_tracking);

        let utterance = session
            .speech_controller()
            .speak("Please summarize the agenda.", Priority::Append, None)
            .await
            .expect("allowed speech");
        assert!(utterance.to_string().starts_with("utt_"));

        let realtime_session = session.realtime_session();
        session.shutdown().await.expect("shutdown");
        assert_eq!(bot.left.lock().unwrap().len(), 1);
        assert!(
            realtime
                .script_emit(
                    realtime_session,
                    heron_realtime::RealtimeEvent::InputSpeechStopped {
                        session: realtime_session,
                        at: chrono::Utc::now(),
                    },
                )
                .is_err(),
            "realtime session should be closed after shutdown",
        );
    }

    #[tokio::test]
    async fn invalid_policy_fails_before_creating_vendor_bot() {
        let bot = Arc::new(FakeBotDriver::default());
        let realtime = Arc::new(MockRealtimeBackend::new());
        let owner = LiveSessionOwner::new(Arc::clone(&bot), realtime, NaiveBridge::with_defaults);
        let mut args = start_args();
        args.policy.allow_topics = vec!["secret".to_owned()];

        let err = match owner.start(args).await {
            Ok(_) => panic!("policy validation should fail"),
            Err(err) => err,
        };

        assert!(matches!(err, LiveSessionError::PolicyValidation(_)));
        assert!(bot.created.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn realtime_start_failure_terminates_created_bot() {
        let bot = Arc::new(FakeBotDriver::default());
        let realtime = Arc::new(FailingRealtimeBackend);
        let owner = LiveSessionOwner::new(Arc::clone(&bot), realtime, NaiveBridge::with_defaults);

        let err = match owner.start(start_args()).await {
            Ok(_) => panic!("realtime startup should fail"),
            Err(err) => err,
        };

        assert!(matches!(
            err,
            LiveSessionError::RealtimeStartup {
                bot_cleanup_error: None,
                ..
            }
        ));
        assert_eq!(bot.created.lock().unwrap().len(), 1);
        assert_eq!(bot.terminated.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn realtime_start_failure_times_out_hung_bot_cleanup() {
        let bot = Arc::new(HangingTerminateBotDriver::default());
        let realtime = Arc::new(FailingRealtimeBackend);
        let owner = LiveSessionOwner::new(Arc::clone(&bot), realtime, NaiveBridge::with_defaults)
            .with_startup_cleanup_timeout(Duration::from_millis(1));

        let err = match owner.start(start_args()).await {
            Ok(_) => panic!("realtime startup should fail"),
            Err(err) => err,
        };

        match err {
            LiveSessionError::RealtimeStartup {
                bot_cleanup_error: Some(cleanup),
                ..
            } => assert!(cleanup.contains("timed out after 1ms")),
            other => panic!("expected cleanup timeout, got {other:?}"),
        }
        assert_eq!(bot.created.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn shutdown_times_out_hung_cleanup_step() {
        let bot = Arc::new(HangingLeaveBotDriver::default());
        let realtime = Arc::new(MockRealtimeBackend::new());
        let owner = LiveSessionOwner::new(Arc::clone(&bot), realtime, NaiveBridge::with_defaults)
            .with_shutdown_step_timeout(Duration::from_millis(1));
        let session = owner.start(start_args()).await.expect("start live session");

        let err = session.shutdown().await.expect_err("shutdown timeout");

        match err {
            LiveSessionError::Shutdown {
                speech: None,
                realtime: None,
                bot: Some(bot),
            } => assert!(bot.contains("timed out after 1ms")),
            other => panic!("expected bot shutdown timeout, got {other:?}"),
        }
        assert_eq!(bot.created.lock().unwrap().len(), 1);
    }
}
