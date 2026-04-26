//! OpenAI Realtime WebSocket backend.
//!
//! This is the first production [`crate::RealtimeBackend`] implementation.
//! It deliberately owns only session lifecycle and JSON event translation:
//! audio transport still belongs to `heron-bridge`, and the orchestrator
//! will decide when PCM channels are connected.

use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::{AUTHORIZATION, HeaderValue};
use tokio_tungstenite::tungstenite::protocol::Message;

use crate::{
    RealtimeBackend, RealtimeCapabilities, RealtimeError, RealtimeEvent, ResponseId, SessionConfig,
    SessionId, ToolSpec, TurnDetection, validate_session,
};

const DEFAULT_MODEL: &str = "gpt-realtime";
const DEFAULT_ENDPOINT: &str = "wss://api.openai.com/v1/realtime";
const BROADCAST_CAPACITY: usize = 256;
const WRITER_CAPACITY: usize = 256;

/// Configuration for [`OpenAiRealtime`].
#[derive(Debug, Clone)]
pub struct OpenAiRealtimeConfig {
    /// OpenAI API key. Prefer [`Self::from_env`] in production so the
    /// secret stays out of config files.
    pub api_key: String,
    /// Realtime model name. Defaults to `gpt-realtime`.
    pub model: String,
    /// WebSocket endpoint. Defaults to OpenAI's public Realtime endpoint.
    pub endpoint: String,
}

impl OpenAiRealtimeConfig {
    /// Build config from environment:
    ///
    /// - `OPENAI_API_KEY` (required)
    /// - `HERON_OPENAI_REALTIME_MODEL` (optional, default `gpt-realtime`)
    /// - `HERON_OPENAI_REALTIME_ENDPOINT` (optional)
    pub fn from_env() -> Result<Self, RealtimeError> {
        let api_key = env::var("OPENAI_API_KEY").map_err(|_| {
            RealtimeError::BadConfig("OPENAI_API_KEY is required for OpenAiRealtime".to_owned())
        })?;
        Ok(Self {
            api_key,
            model: env::var("HERON_OPENAI_REALTIME_MODEL")
                .unwrap_or_else(|_| DEFAULT_MODEL.to_owned()),
            endpoint: env::var("HERON_OPENAI_REALTIME_ENDPOINT")
                .unwrap_or_else(|_| DEFAULT_ENDPOINT.to_owned()),
        })
    }
}

/// Production backend that talks to OpenAI Realtime over WebSocket.
pub struct OpenAiRealtime {
    config: OpenAiRealtimeConfig,
    sessions: Arc<Mutex<HashMap<SessionId, SessionState>>>,
}

struct SessionState {
    tx: mpsc::Sender<Message>,
    events: broadcast::Sender<RealtimeEvent>,
    pending_responses: VecDeque<ResponseId>,
    response_ids: HashMap<ResponseId, String>,
    audio_started: HashSet<ResponseId>,
    latest_response: Option<ResponseId>,
    latest_item_id: Option<String>,
}

impl OpenAiRealtime {
    pub fn new(config: OpenAiRealtimeConfig) -> Self {
        Self {
            config,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn from_env() -> Result<Self, RealtimeError> {
        OpenAiRealtimeConfig::from_env().map(Self::new)
    }

    fn session_sender(&self, session: SessionId) -> Result<mpsc::Sender<Message>, RealtimeError> {
        let sessions = lock(&self.sessions);
        sessions
            .get(&session)
            .map(|state| state.tx.clone())
            .ok_or_else(unknown_session)
    }

    async fn send_json(
        &self,
        session: SessionId,
        value: serde_json::Value,
    ) -> Result<(), RealtimeError> {
        let tx = self.session_sender(session)?;
        tx.send(Message::Text(value.to_string().into()))
            .await
            .map_err(|_| RealtimeError::Network("OpenAI Realtime writer closed".to_owned()))
    }
}

#[async_trait]
impl RealtimeBackend for OpenAiRealtime {
    async fn session_open(&self, config: SessionConfig) -> Result<SessionId, RealtimeError> {
        validate_session(&config)?;
        if config.turn_detection.auto_create_response {
            return Err(RealtimeError::BadConfig(
                "OpenAiRealtime requires turn_detection.auto_create_response=false; \
                 the controller must create responses explicitly so response IDs stay correlated"
                    .to_owned(),
            ));
        }
        if self.config.api_key.trim().is_empty() {
            return Err(RealtimeError::BadConfig(
                "OpenAiRealtimeConfig.api_key is empty".to_owned(),
            ));
        }

        let session = SessionId::now_v7();
        let (events, _) = broadcast::channel(BROADCAST_CAPACITY);
        let (tx, mut rx) = mpsc::channel::<Message>(WRITER_CAPACITY);
        let url = format!("{}?model={}", self.config.endpoint, self.config.model);
        let mut request = url
            .into_client_request()
            .map_err(|e| RealtimeError::BadConfig(format!("invalid Realtime endpoint: {e}")))?;
        let auth = HeaderValue::from_str(&format!("Bearer {}", self.config.api_key))
            .map_err(|e| RealtimeError::BadConfig(format!("invalid OpenAI API key header: {e}")))?;
        request.headers_mut().insert(AUTHORIZATION, auth);
        request
            .headers_mut()
            .insert("OpenAI-Beta", HeaderValue::from_static("realtime=v1"));

        let (ws, _) = connect_async(request)
            .await
            .map_err(|e| RealtimeError::Network(format!("OpenAI Realtime connect failed: {e}")))?;
        let (mut writer, mut reader) = ws.split();

        tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                if writer.send(message).await.is_err() {
                    break;
                }
            }
            let _ = writer.close().await;
        });

        let reader_context = Arc::new(ReaderContext {
            session,
            events: events.clone(),
        });
        let context = Arc::clone(&reader_context);
        let state_map = Arc::clone(&self.sessions);
        tokio::spawn(async move {
            while let Some(message) = reader.next().await {
                match message {
                    Ok(Message::Text(text)) => handle_server_text(&state_map, &context, &text),
                    Ok(Message::Close(_)) => break,
                    Ok(_) => {}
                    Err(e) => {
                        let _ = context.events.send(RealtimeEvent::Error {
                            session: context.session,
                            error: format!("OpenAI Realtime read failed: {e}"),
                        });
                        break;
                    }
                }
            }
        });

        {
            let mut sessions = lock(&self.sessions);
            sessions.insert(
                session,
                SessionState {
                    tx,
                    events,
                    pending_responses: VecDeque::new(),
                    response_ids: HashMap::new(),
                    audio_started: HashSet::new(),
                    latest_response: None,
                    latest_item_id: None,
                },
            );
        }

        self.send_json(session, session_update(&config)).await?;
        Ok(session)
    }

    async fn session_close(&self, id: SessionId) -> Result<(), RealtimeError> {
        let state = {
            let mut sessions = lock(&self.sessions);
            sessions.remove(&id).ok_or_else(unknown_session)?
        };
        state
            .tx
            .send(Message::Close(None))
            .await
            .map_err(|_| RealtimeError::Network("OpenAI Realtime writer closed".to_owned()))
    }

    async fn response_create(
        &self,
        session: SessionId,
        text: &str,
        voice_override: Option<String>,
    ) -> Result<ResponseId, RealtimeError> {
        let response = ResponseId::now_v7();
        {
            let mut sessions = lock(&self.sessions);
            let state = sessions.get_mut(&session).ok_or_else(unknown_session)?;
            state.pending_responses.push_back(response);
        }

        if let Err(err) = self
            .send_json(session, response_create(text, voice_override))
            .await
        {
            forget_pending_response(&self.sessions, session, response);
            return Err(err);
        }
        Ok(response)
    }

    async fn response_cancel(
        &self,
        session: SessionId,
        response: ResponseId,
    ) -> Result<(), RealtimeError> {
        let response_id = {
            let sessions = lock(&self.sessions);
            sessions
                .get(&session)
                .ok_or_else(unknown_session)?
                .response_ids
                .get(&response)
                .cloned()
        };

        let mut event = json!({"type": "response.cancel"});
        if let Some(response_id) = response_id {
            event["response_id"] = Value::String(response_id);
        }
        self.send_json(session, event).await
    }

    async fn truncate_current(
        &self,
        session: SessionId,
        audio_end_ms: u32,
    ) -> Result<(), RealtimeError> {
        let item_id = {
            let sessions = lock(&self.sessions);
            sessions
                .get(&session)
                .ok_or_else(unknown_session)?
                .latest_item_id
                .clone()
        };
        let Some(item_id) = item_id else {
            return Ok(());
        };
        self.send_json(
            session,
            json!({
                "type": "conversation.item.truncate",
                "item_id": item_id,
                "content_index": 0,
                "audio_end_ms": audio_end_ms,
            }),
        )
        .await
    }

    async fn tool_result(
        &self,
        session: SessionId,
        tool_call_id: String,
        result: serde_json::Value,
    ) -> Result<(), RealtimeError> {
        self.send_json(session, tool_result_item(tool_call_id, result))
            .await
    }

    fn subscribe_events(&self, id: SessionId) -> broadcast::Receiver<RealtimeEvent> {
        let sessions = lock(&self.sessions);
        if let Some(state) = sessions.get(&id) {
            state.events.subscribe()
        } else {
            let (tx, rx) = broadcast::channel::<RealtimeEvent>(1);
            drop(tx);
            rx
        }
    }

    fn capabilities(&self) -> RealtimeCapabilities {
        RealtimeCapabilities {
            bidirectional_audio: true,
            server_vad: true,
            atomic_response_cancel: true,
            tool_calling: true,
            text_deltas: true,
        }
    }
}

struct ReaderContext {
    session: SessionId,
    events: broadcast::Sender<RealtimeEvent>,
}

fn session_update(config: &SessionConfig) -> Value {
    json!({
        "type": "session.update",
        "session": {
            "type": "realtime",
            "instructions": config.system_prompt,
            "voice": config.voice,
            "modalities": ["text", "audio"],
            "turn_detection": turn_detection(config.turn_detection),
            "tools": config.tools.iter().map(tool_spec).collect::<Vec<_>>(),
        }
    })
}

fn turn_detection(config: TurnDetection) -> Value {
    json!({
        "type": "server_vad",
        "threshold": config.vad_threshold,
        "prefix_padding_ms": config.prefix_padding_ms,
        "silence_duration_ms": config.silence_duration_ms,
        "interrupt_response": config.interrupt_response,
        "create_response": config.auto_create_response,
    })
}

fn tool_spec(tool: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "name": tool.name,
        "description": tool.description,
        "parameters": tool.parameters_schema,
    })
}

fn response_create(text: &str, voice_override: Option<String>) -> Value {
    let mut event = json!({
        "type": "response.create",
        "response": {
            "modalities": ["text", "audio"],
            "instructions": format!("Speak exactly this text and no other text:\n\n{text}"),
        }
    });
    if let Some(voice) = voice_override {
        event["response"]["voice"] = Value::String(voice);
    }
    event
}

fn tool_result_item(tool_call_id: String, result: Value) -> Value {
    let output = match result {
        Value::String(s) => s,
        other => other.to_string(),
    };
    json!({
        "type": "conversation.item.create",
        "item": {
            "type": "function_call_output",
            "call_id": tool_call_id,
            "output": output,
        }
    })
}

fn handle_server_text(
    sessions: &Arc<Mutex<HashMap<SessionId, SessionState>>>,
    context: &ReaderContext,
    text: &str,
) {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        let _ = context.events.send(RealtimeEvent::Error {
            session: context.session,
            error: "OpenAI Realtime sent invalid JSON".to_owned(),
        });
        return;
    };
    let Some(kind) = value.get("type").and_then(Value::as_str) else {
        return;
    };

    match kind {
        "input_audio_buffer.speech_started" => {
            let _ = context.events.send(RealtimeEvent::InputSpeechStarted {
                session: context.session,
                at: Utc::now(),
            });
        }
        "input_audio_buffer.speech_stopped" => {
            let _ = context.events.send(RealtimeEvent::InputSpeechStopped {
                session: context.session,
                at: Utc::now(),
            });
        }
        "conversation.item.input_audio_transcription.delta" => {
            if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                let _ = context.events.send(RealtimeEvent::InputTranscriptDelta {
                    session: context.session,
                    text: delta.to_owned(),
                    is_final: false,
                });
            }
        }
        "conversation.item.input_audio_transcription.completed" => {
            if let Some(transcript) = value.get("transcript").and_then(Value::as_str) {
                let _ = context.events.send(RealtimeEvent::InputTranscriptDelta {
                    session: context.session,
                    text: transcript.to_owned(),
                    is_final: true,
                });
            }
        }
        "response.created" => {
            if let Some((response, vendor_response)) =
                bind_response(sessions, context.session, &value)
            {
                let _ = context.events.send(RealtimeEvent::ResponseCreated {
                    session: context.session,
                    response,
                    at: Utc::now(),
                });
                remember_vendor_response(sessions, context.session, response, vendor_response);
            }
        }
        "response.output_item.added" => {
            remember_output_item(sessions, context.session, &value);
        }
        "response.output_text.delta"
        | "response.text.delta"
        | "response.audio_transcript.delta" => {
            if let Some(response) = response_for_event(sessions, context.session, &value)
                && let Some(delta) = value.get("delta").and_then(Value::as_str)
            {
                let _ = context.events.send(RealtimeEvent::ResponseTextDelta {
                    session: context.session,
                    response,
                    text: delta.to_owned(),
                });
            }
        }
        "response.audio.delta" => {
            if let Some(response) = response_for_event(sessions, context.session, &value)
                && mark_audio_started(sessions, context.session, response)
            {
                let _ = context.events.send(RealtimeEvent::ResponseAudioStarted {
                    session: context.session,
                    response,
                    at: Utc::now(),
                });
            }
        }
        "response.done" => {
            if let Some(response) = response_for_event(sessions, context.session, &value) {
                let _ = context.events.send(RealtimeEvent::ResponseDone {
                    session: context.session,
                    response,
                    at: Utc::now(),
                });
                forget_response(sessions, context.session, response);
            }
        }
        "response.function_call_arguments.done" => {
            if let Some(response) = response_for_event(sessions, context.session, &value) {
                let call_id = value
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let tool_name = value
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let arguments = value
                    .get("arguments")
                    .and_then(Value::as_str)
                    .and_then(|raw| serde_json::from_str(raw).ok())
                    .unwrap_or(Value::Null);
                let _ = context.events.send(RealtimeEvent::ToolCall {
                    session: context.session,
                    response,
                    tool_call_id: call_id,
                    tool_name,
                    arguments,
                });
            }
        }
        "error" => {
            let error = value
                .get("error")
                .and_then(|v| v.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("OpenAI Realtime error")
                .to_owned();
            let _ = context.events.send(RealtimeEvent::Error {
                session: context.session,
                error,
            });
        }
        _ => {}
    }
}

fn bind_response(
    sessions: &Arc<Mutex<HashMap<SessionId, SessionState>>>,
    session: SessionId,
    value: &Value,
) -> Option<(ResponseId, String)> {
    let vendor_response = value
        .get("response")
        .and_then(response_id_from_value)
        .unwrap_or_default()
        .to_owned();
    let mut sessions = lock(sessions);
    let state = sessions.get_mut(&session)?;
    let response = state
        .pending_responses
        .pop_front()
        .unwrap_or_else(ResponseId::now_v7);
    state.latest_response = Some(response);
    Some((response, vendor_response))
}

fn forget_pending_response(
    sessions: &Arc<Mutex<HashMap<SessionId, SessionState>>>,
    session: SessionId,
    response: ResponseId,
) {
    let mut sessions = lock(sessions);
    if let Some(state) = sessions.get_mut(&session) {
        state
            .pending_responses
            .retain(|candidate| *candidate != response);
    }
}

fn remember_vendor_response(
    sessions: &Arc<Mutex<HashMap<SessionId, SessionState>>>,
    session: SessionId,
    response: ResponseId,
    vendor_response: String,
) {
    if vendor_response.is_empty() {
        return;
    }
    let mut sessions = lock(sessions);
    if let Some(state) = sessions.get_mut(&session) {
        state.response_ids.insert(response, vendor_response);
    }
}

fn remember_output_item(
    sessions: &Arc<Mutex<HashMap<SessionId, SessionState>>>,
    session: SessionId,
    value: &Value,
) {
    let Some(item_id) = value
        .get("item")
        .and_then(|item| item.get("id"))
        .and_then(Value::as_str)
    else {
        return;
    };
    let mut sessions = lock(sessions);
    if let Some(state) = sessions.get_mut(&session) {
        state.latest_item_id = Some(item_id.to_owned());
    }
}

fn mark_audio_started(
    sessions: &Arc<Mutex<HashMap<SessionId, SessionState>>>,
    session: SessionId,
    response: ResponseId,
) -> bool {
    let mut sessions = lock(sessions);
    sessions
        .get_mut(&session)
        .is_some_and(|state| state.audio_started.insert(response))
}

fn forget_response(
    sessions: &Arc<Mutex<HashMap<SessionId, SessionState>>>,
    session: SessionId,
    response: ResponseId,
) {
    let mut sessions = lock(sessions);
    if let Some(state) = sessions.get_mut(&session) {
        state.response_ids.remove(&response);
        state.audio_started.remove(&response);
    }
}

fn response_for_event(
    sessions: &Arc<Mutex<HashMap<SessionId, SessionState>>>,
    session: SessionId,
    value: &Value,
) -> Option<ResponseId> {
    let vendor_response = response_id_from_value(value);
    let sessions = lock(sessions);
    let state = sessions.get(&session)?;
    if let Some(vendor_response) = vendor_response
        && let Some((response, _)) = state
            .response_ids
            .iter()
            .find(|(_, vendor)| vendor.as_str() == vendor_response)
    {
        return Some(*response);
    }
    state.latest_response
}

fn response_id_from_value(value: &Value) -> Option<&str> {
    value
        .get("response_id")
        .and_then(Value::as_str)
        .or_else(|| value.get("id").and_then(Value::as_str))
        .or_else(|| {
            value
                .get("response")
                .and_then(|response| response.get("id"))
                .and_then(Value::as_str)
        })
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn unknown_session() -> RealtimeError {
    RealtimeError::Backend("session not found".to_owned())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::{ToolSpec, TurnDetection};
    use serde_json::json;

    fn config() -> SessionConfig {
        SessionConfig {
            system_prompt: "You are a helpful meeting assistant.".to_owned(),
            tools: vec![ToolSpec {
                name: "lookup".to_owned(),
                description: "Look up a fact".to_owned(),
                parameters_schema: json!({"type": "object", "properties": {}}),
            }],
            turn_detection: TurnDetection {
                vad_threshold: 0.5,
                prefix_padding_ms: 300,
                silence_duration_ms: 500,
                interrupt_response: true,
                auto_create_response: false,
            },
            voice: "alloy".to_owned(),
        }
    }

    #[test]
    fn session_update_serializes_openai_shape() {
        let event = session_update(&config());
        assert_eq!(event["type"], "session.update");
        assert_eq!(event["session"]["type"], "realtime");
        assert_eq!(event["session"]["instructions"], config().system_prompt);
        assert_eq!(event["session"]["voice"], "alloy");
        assert_eq!(event["session"]["turn_detection"]["type"], "server_vad");
        assert_eq!(event["session"]["turn_detection"]["create_response"], false);
        assert_eq!(event["session"]["tools"][0]["type"], "function");
        assert_eq!(event["session"]["tools"][0]["name"], "lookup");
    }

    #[test]
    fn response_create_serializes_voice_override_only_when_present() {
        let without = response_create("hello", None);
        assert_eq!(without["response"]["modalities"], json!(["text", "audio"]));
        assert!(
            without["response"]["instructions"]
                .as_str()
                .expect("instructions")
                .contains("hello")
        );
        assert!(without["response"].get("voice").is_none());

        let with = response_create("hello", Some("verse".to_owned()));
        assert_eq!(with["response"]["voice"], "verse");
        assert!(
            with["response"]["instructions"]
                .as_str()
                .expect("instructions")
                .contains("hello")
        );
    }

    #[test]
    fn tool_result_item_preserves_plain_string_outputs() {
        let string = tool_result_item("call_1".to_owned(), Value::String("ok".to_owned()));
        assert_eq!(string["item"]["output"], "ok");

        let object = tool_result_item("call_2".to_owned(), json!({"ok": true}));
        assert_eq!(object["item"]["output"], r#"{"ok":true}"#);
    }

    #[test]
    fn maps_openai_response_events_to_trait_events() {
        let session = SessionId::now_v7();
        let response = ResponseId::now_v7();
        let (events, mut rx) = broadcast::channel(16);
        let sessions = Arc::new(Mutex::new(HashMap::from([(
            session,
            SessionState {
                tx: mpsc::channel(1).0,
                events: events.clone(),
                pending_responses: VecDeque::from([response]),
                response_ids: HashMap::new(),
                audio_started: HashSet::new(),
                latest_response: None,
                latest_item_id: None,
            },
        )])));
        let context = ReaderContext { session, events };

        handle_server_text(
            &sessions,
            &context,
            r#"{"type":"response.created","response":{"id":"resp_vendor"}}"#,
        );
        handle_server_text(
            &sessions,
            &context,
            r#"{"type":"response.output_text.delta","response_id":"resp_vendor","delta":"hello"}"#,
        );
        handle_server_text(
            &sessions,
            &context,
            r#"{"type":"response.audio_transcript.delta","response_id":"resp_vendor","delta":" world"}"#,
        );
        handle_server_text(
            &sessions,
            &context,
            r#"{"type":"response.audio.delta","response_id":"resp_vendor","delta":"abc"}"#,
        );
        handle_server_text(
            &sessions,
            &context,
            r#"{"type":"response.audio.delta","response_id":"resp_vendor","delta":"def"}"#,
        );
        handle_server_text(
            &sessions,
            &context,
            r#"{"type":"response.done","response":{"id":"resp_vendor"}}"#,
        );

        assert!(matches!(
            rx.try_recv().expect("created"),
            RealtimeEvent::ResponseCreated { response: got, .. } if got == response
        ));
        assert!(matches!(
            rx.try_recv().expect("delta"),
            RealtimeEvent::ResponseTextDelta { response: got, text, .. }
                if got == response && text == "hello"
        ));
        assert!(matches!(
            rx.try_recv().expect("audio transcript"),
            RealtimeEvent::ResponseTextDelta { response: got, text, .. }
                if got == response && text == " world"
        ));
        assert!(matches!(
            rx.try_recv().expect("audio started"),
            RealtimeEvent::ResponseAudioStarted { response: got, .. } if got == response
        ));
        assert!(matches!(
            rx.try_recv().expect("done"),
            RealtimeEvent::ResponseDone { response: got, .. } if got == response
        ));
        assert!(rx.try_recv().is_err());
    }
}
