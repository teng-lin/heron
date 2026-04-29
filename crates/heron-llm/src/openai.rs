//! OpenAI Chat Completions client for the [`Summarizer`] trait.
//!
//! Sister of [`crate::anthropic::AnthropicClient`]. Uses the
//! `/v1/chat/completions` endpoint with JSON-mode (`response_format`)
//! to coerce a `SummarizerOutput` shape, the same approach the Anthropic
//! client uses with `messages.create`'s structured output.
//!
//! Defaults to `gpt-4o-mini` per `Settings.openai_model` (Tier 1). Auth
//! is `Authorization: Bearer <OPENAI_API_KEY>`; the key is resolved via
//! [`KeyResolver`] so the desktop crate's keychain layer can supply it
//! identically to the Anthropic path.

use std::time::Duration;

use async_trait::async_trait;
use heron_types::MeetingType;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};

use crate::content::parse_content_json;
use crate::key_resolver::{EnvKeyResolver, KeyName, KeyResolveError, KeyResolver};
use crate::transcript::{TRANSCRIPT_WARN_BYTES, build_user_content, read_transcript_capped};
use crate::{LlmError, Summarizer, SummarizerInput, SummarizerOutput, render_meeting_prompt};

/// Default API origin. Tests inject a wiremock URL via
/// [`OpenAIClientConfig::base_url`].
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com";

/// Default model. The user can override via `Settings.openai_model`
/// (Tier 1). `gpt-4o-mini` is chosen as the default for cost
/// efficiency; callers wanting higher capability should set
/// `openai_model = "gpt-4o"` in settings.
pub const DEFAULT_MODEL: &str = "gpt-4o-mini";

/// Hard cap on the body snippet we paste into a non-2xx error
/// message. Mirrors [`crate::anthropic::ERROR_BODY_SNIPPET_BYTES`].
pub const ERROR_BODY_SNIPPET_BYTES: usize = 2 * 1024;

/// Hard cap on a successful response body. At 4_096 output tokens and
/// ~4 bytes/token the completion JSON is ~64 KB; 2 MB gives 30× head-
/// room while bounding runaway-response OOM risk.
pub const MAX_RESPONSE_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Default `max_tokens` requested from the API per call.
pub const DEFAULT_MAX_TOKENS: u32 = 4_096;

/// Default request timeout. Mirrors the Anthropic client's 120 s
/// safety net for large transcripts.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// Caller-supplied configuration. Use [`OpenAIClientConfig::from_env`]
/// for the standard `OPENAI_API_KEY` flow; tests construct the
/// struct directly to swap `base_url` and `model`.
#[derive(Debug, Clone)]
pub struct OpenAIClientConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub max_tokens: u32,
    pub timeout: Duration,
}

impl OpenAIClientConfig {
    /// Read `OPENAI_API_KEY` from the environment; default the rest.
    /// Returns [`LlmError::MissingApiKey`] if the env var is unset
    /// or empty.
    ///
    /// Implemented in terms of [`Self::from_resolver`] with an
    /// [`EnvKeyResolver`] so the resolver-aware constructor stays the
    /// single source of truth for key-precedence behaviour. Desktop
    /// callers that want the keychain fallback should reach for
    /// `from_resolver` directly with an `EnvThenKeychainResolver`.
    pub fn from_env() -> Result<Self, LlmError> {
        Self::from_resolver(&EnvKeyResolver)
    }

    /// Build a config by asking `resolver` for the OpenAI API key.
    ///
    /// Maps `KeyResolveError::NotFound` to [`LlmError::MissingApiKey`]
    /// so callers see the same error variant regardless of whether the
    /// resolver consulted env-only or also checked the keychain.
    /// Other resolver failures (a macOS keychain backend error) come
    /// back as `LlmError::Backend` so they can be surfaced as a clean
    /// renderer-side toast distinct from "no key configured".
    pub fn from_resolver(resolver: &dyn KeyResolver) -> Result<Self, LlmError> {
        let api_key = resolver
            .resolve(KeyName::OpenAiApiKey)
            .map_err(|e| match e {
                KeyResolveError::NotFound(_) => LlmError::MissingApiKey,
                KeyResolveError::Backend(msg) => {
                    LlmError::Backend(format!("api key resolver: {msg}"))
                }
            })?;
        Ok(Self {
            api_key,
            base_url: DEFAULT_BASE_URL.to_owned(),
            model: DEFAULT_MODEL.to_owned(),
            max_tokens: DEFAULT_MAX_TOKENS,
            timeout: DEFAULT_TIMEOUT,
        })
    }
}

/// Live OpenAI Chat Completions summarizer.
///
/// Reuses one `reqwest::Client` across calls so connection pooling +
/// rustls session caching kick in for the typical "summarize once
/// per meeting" workload that runs every few minutes.
pub struct OpenAIClient {
    config: OpenAIClientConfig,
    http: reqwest::Client,
}

impl OpenAIClient {
    /// Build a client with the supplied config. Returns a
    /// [`LlmError::Backend`] if `reqwest` can't construct the inner
    /// client (e.g. the rustls bundle failed to load).
    pub fn new(config: OpenAIClientConfig) -> Result<Self, LlmError> {
        let bearer = format!("Bearer {}", config.api_key);
        let mut auth = HeaderValue::from_str(&bearer)
            .map_err(|e| LlmError::Backend(format!("api key invalid as HTTP header: {e}")))?;
        auth.set_sensitive(true);

        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, auth);

        let http = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(config.timeout)
            .build()
            .map_err(|e| LlmError::Backend(format!("reqwest::Client build: {e}")))?;
        Ok(Self { config, http })
    }
}

#[async_trait]
impl Summarizer for OpenAIClient {
    async fn summarize(&self, input: SummarizerInput<'_>) -> Result<SummarizerOutput, LlmError> {
        let prompt = render_meeting_prompt(&input)?;
        let transcript_text = read_transcript_capped(input.transcript)?;
        if (transcript_text.len() as u64) >= TRANSCRIPT_WARN_BYTES {
            tracing::warn!(
                bytes = transcript_text.len(),
                "transcript approaches model input-token limit; \
                 consider chunked summarize or a higher-context model"
            );
        }
        let user_content = build_user_content(&prompt, &transcript_text);

        let body = ChatCompletionsRequest {
            model: &self.config.model,
            messages: vec![
                ChatMessage {
                    role: "system",
                    content: "You are a meeting summarizer. \
                              Output only valid JSON matching the requested schema.",
                },
                ChatMessage {
                    role: "user",
                    content: &user_content,
                },
            ],
            response_format: ResponseFormat {
                r#type: "json_object",
            },
            max_tokens: self.config.max_tokens,
            temperature: 0.0,
        };

        let url = format!(
            "{base}/v1/chat/completions",
            base = self.config.base_url.trim_end_matches('/'),
        );

        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| LlmError::Backend(format!("POST {url}: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let snippet = read_capped_body_snippet(resp).await;
            // Try to extract the `error.message` field from OpenAI's
            // standard error envelope so the user sees something like:
            //   "openai 401: invalid API key"
            // rather than the raw JSON. Fall back to the raw snippet if
            // parsing fails (e.g., a proxy returned HTML).
            let message = try_extract_openai_error_message(&snippet).unwrap_or(snippet);
            return Err(LlmError::Backend(format!(
                "openai API returned {status}: {message}"
            )));
        }

        // Stream the body up to MAX_RESPONSE_BODY_BYTES so a runaway or
        // malicious response cannot OOM the process (mirrors the error-
        // path's `read_capped_body_snippet`). Returns an error if the
        // body exceeds the cap rather than silently truncating, so the
        // caller gets a clear message instead of a partial-JSON parse
        // failure.
        let body_bytes = read_capped_response_bytes(resp, MAX_RESPONSE_BODY_BYTES)
            .await
            .map_err(|e| LlmError::Backend(format!("reading response: {e}")))?;
        let response: ChatCompletionsResponse = serde_json::from_slice(&body_bytes)
            .map_err(|e| LlmError::Backend(format!("response JSON parse: {e}")))?;

        parse_chat_completions_response(response, input.meeting_type)
    }
}

/// Try to extract `error.message` from an OpenAI standard error body.
/// Returns `None` if the body isn't an OpenAI error envelope — in
/// that case the caller uses the raw snippet. Never panics.
fn try_extract_openai_error_message(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v.get("error")?.get("message")?.as_str().map(str::to_owned)
}

/// Stream `resp.bytes_stream()` until we have at most
/// [`ERROR_BODY_SNIPPET_BYTES`] worth of payload, then drop the rest.
/// Mirrors `anthropic::read_capped_body_snippet` exactly.
async fn read_capped_body_snippet(mut resp: reqwest::Response) -> String {
    let mut buf: Vec<u8> = Vec::with_capacity(ERROR_BODY_SNIPPET_BYTES.min(8 * 1024));
    let mut total_observed: usize = 0;
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                total_observed = total_observed.saturating_add(chunk.len());
                if buf.len() < ERROR_BODY_SNIPPET_BYTES {
                    let remaining = ERROR_BODY_SNIPPET_BYTES - buf.len();
                    let take = chunk.len().min(remaining);
                    buf.extend_from_slice(&chunk[..take]);
                }
                if total_observed > ERROR_BODY_SNIPPET_BYTES * 8 {
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => return "<unreadable body>".to_owned(),
        }
    }
    if buf.is_empty() && total_observed == 0 {
        return "<no body>".to_owned();
    }
    // `String::from_utf8_lossy` replaces any incomplete multi-byte
    // sequence introduced by the byte-boundary truncation with U+FFFD
    // rather than discarding the whole snippet. The replacement char
    // is visually obvious so the user still gets the useful ASCII
    // portion of the error message.
    let body_str = String::from_utf8_lossy(&buf);
    if total_observed > buf.len() {
        format!("{body_str} ...[truncated, observed at least {total_observed} bytes]",)
    } else {
        body_str.into_owned()
    }
}

/// Stream `resp.bytes_stream()` into a `Vec<u8>`, returning an error if
/// the body exceeds `limit`. Used for 2xx success responses so that a
/// runaway or malicious response cannot OOM the process.
async fn read_capped_response_bytes(
    mut resp: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, String> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                if buf.len() + chunk.len() > limit {
                    return Err(format!(
                        "response body exceeded {limit} bytes — possible runaway response"
                    ));
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(e) => return Err(format!("reading response body: {e}")),
        }
    }
    Ok(buf)
}

/// Pure parser: takes a `ChatCompletionsResponse` and produces a
/// `SummarizerOutput`. Split out so unit tests can exercise the
/// JSON-content parsing without an HTTP round-trip.
pub fn parse_chat_completions_response(
    response: ChatCompletionsResponse,
    meeting_type_fallback: MeetingType,
) -> Result<SummarizerOutput, LlmError> {
    let text = response
        .choices
        .first()
        .map(|c| c.message.content.as_str())
        .ok_or_else(|| LlmError::Parse("response had no choices".to_owned()))?;

    let body = parse_content_json(text)?;

    let cost = crate::cost::compute_cost(
        &response.model,
        response.usage.prompt_tokens,
        response.usage.completion_tokens,
    )
    .map_err(|e| LlmError::Backend(format!("cost lookup: {e}")))?;

    Ok(SummarizerOutput {
        body: body.body,
        company: body.company,
        meeting_type: body.meeting_type.unwrap_or(meeting_type_fallback),
        tags: body.tags.unwrap_or_default(),
        action_items: body.action_items.unwrap_or_default(),
        attendees: body.attendees.unwrap_or_default(),
        cost,
    })
}

// ── wire types ────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct ChatCompletionsRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    response_format: ResponseFormat,
    max_tokens: u32,
    temperature: f32,
}

#[derive(Debug, Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Debug, Serialize)]
struct ResponseFormat {
    r#type: &'static str,
}

/// Public so [`parse_chat_completions_response`] can accept it from a test.
#[derive(Debug, Deserialize)]
pub struct ChatCompletionsResponse {
    pub model: String,
    pub choices: Vec<Choice>,
    pub usage: Usage,
}

#[derive(Debug, Deserialize)]
pub struct Choice {
    pub message: AssistantMessage,
}

#[derive(Debug, Deserialize)]
pub struct AssistantMessage {
    pub content: String,
}

/// Token counts as returned by the Chat Completions API.
#[derive(Debug, Deserialize, Default)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::io::Write;
    use std::path::PathBuf;

    use heron_types::MeetingType;

    use super::*;
    use crate::test_env::ENV_LOCK;

    fn write_tmp_jsonl(lines: &[&str]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("transcript.jsonl");
        let mut f = std::fs::File::create(&path).expect("create");
        for line in lines {
            writeln!(f, "{line}").expect("write");
        }
        (dir, path)
    }

    fn sample_response(content_json: &str, model: &str) -> ChatCompletionsResponse {
        ChatCompletionsResponse {
            model: model.to_owned(),
            choices: vec![Choice {
                message: AssistantMessage {
                    content: content_json.to_owned(),
                },
            }],
            usage: Usage {
                prompt_tokens: 1_000,
                completion_tokens: 200,
                total_tokens: 1_200,
            },
        }
    }

    // ── parse_chat_completions_response unit tests ─────────────────────────

    #[test]
    fn parse_response_extracts_body_and_computes_cost() {
        let body_json = r#"{
            "body":"Meeting summary here.",
            "company":"Acme",
            "meeting_type":"client",
            "tags":["acme","pricing"],
            "action_items":[
                {"id":"00000000-0000-0000-0000-000000000001","owner":"me","text":"Follow up","due":null}
            ],
            "attendees":[]
        }"#;
        let resp = sample_response(body_json, "gpt-4o-mini");
        let out = parse_chat_completions_response(resp, MeetingType::Other).expect("parse");
        assert_eq!(out.body, "Meeting summary here.");
        assert_eq!(out.company.as_deref(), Some("Acme"));
        assert_eq!(out.meeting_type, MeetingType::Client);
        assert_eq!(out.tags, vec!["acme".to_owned(), "pricing".to_owned()]);
        assert_eq!(out.action_items.len(), 1);
        assert_eq!(out.action_items[0].owner, "me");
        // 1000 in × $0.15/M + 200 out × $0.60/M = $0.00015 + $0.00012 = $0.00027
        // round_cents (4dp): 0.00027 × 10000 = 2.7 → rounds to 3 → $0.0003
        assert_eq!(out.cost.summary_usd, 0.00027_f64.round_4dp());
        assert_eq!(out.cost.tokens_in, 1_000);
        assert_eq!(out.cost.tokens_out, 200);
        assert_eq!(out.cost.model, "gpt-4o-mini");
    }

    trait Round4dp {
        fn round_4dp(self) -> f64;
    }
    impl Round4dp for f64 {
        fn round_4dp(self) -> f64 {
            (self * 10_000.0).round() / 10_000.0
        }
    }

    #[test]
    fn parse_response_falls_back_to_caller_meeting_type_when_missing() {
        let body_json = r#"{ "body":"x" }"#;
        let resp = sample_response(body_json, "gpt-4o-mini");
        let out = parse_chat_completions_response(resp, MeetingType::Internal).expect("parse");
        assert_eq!(out.meeting_type, MeetingType::Internal);
    }

    #[test]
    fn parse_response_errors_on_no_choices() {
        let resp = ChatCompletionsResponse {
            model: "gpt-4o-mini".into(),
            choices: vec![],
            usage: Usage::default(),
        };
        let err =
            parse_chat_completions_response(resp, MeetingType::Other).expect_err("no choices");
        assert!(matches!(err, LlmError::Parse(_)));
    }

    #[test]
    fn parse_response_errors_on_malformed_content_json() {
        let resp = sample_response("not json at all", "gpt-4o-mini");
        let err = parse_chat_completions_response(resp, MeetingType::Other).expect_err("malformed");
        assert!(matches!(err, LlmError::Parse(_)));
    }

    #[test]
    fn parse_response_errors_on_unknown_model_via_cost_lookup() {
        let resp = sample_response(r#"{"body":"x"}"#, "unknown-model-xyz");
        let err =
            parse_chat_completions_response(resp, MeetingType::Other).expect_err("unknown model");
        match err {
            LlmError::Backend(s) => {
                assert!(s.contains("cost lookup"), "missing wrapper: {s}");
                assert!(s.contains("unknown-model-xyz"), "missing model name: {s}");
            }
            other => panic!("expected Backend, got {other:?}"),
        }
    }

    #[test]
    fn from_resolver_uses_resolver_value() {
        struct Stub;
        impl KeyResolver for Stub {
            fn resolve(&self, _name: KeyName) -> Result<String, KeyResolveError> {
                Ok("from-stub-resolver".to_owned())
            }
        }
        let cfg = OpenAIClientConfig::from_resolver(&Stub).expect("ok");
        assert_eq!(cfg.api_key, "from-stub-resolver");
    }

    #[test]
    fn from_resolver_maps_not_found_to_missing_api_key() {
        struct Stub;
        impl KeyResolver for Stub {
            fn resolve(&self, name: KeyName) -> Result<String, KeyResolveError> {
                Err(KeyResolveError::NotFound(name))
            }
        }
        let err = OpenAIClientConfig::from_resolver(&Stub).expect_err("missing");
        assert!(matches!(err, LlmError::MissingApiKey));
    }

    #[test]
    fn from_env_errors_when_api_key_missing() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let saved = std::env::var_os("OPENAI_API_KEY");
        // SAFETY: ENV_LOCK serializes env-touching tests.
        unsafe {
            std::env::remove_var("OPENAI_API_KEY");
        }
        let err = OpenAIClientConfig::from_env().expect_err("missing key");
        assert!(matches!(err, LlmError::MissingApiKey));
        // SAFETY: restoring prior value.
        unsafe {
            if let Some(v) = saved {
                std::env::set_var("OPENAI_API_KEY", v);
            }
        }
    }

    // ── wiremock integration tests ─────────────────────────────────────────

    #[tokio::test]
    async fn summarize_returns_parsed_output_on_200() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body_json = serde_json::json!({
            "id": "chatcmpl-synthetic",
            "object": "chat.completion",
            "created": 1_700_000_000_u64,
            "model": "gpt-4o-mini",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": r#"{
                        "body":"summary",
                        "company":"Acme",
                        "meeting_type":"client",
                        "tags":["acme"],
                        "action_items":[],
                        "attendees":[]
                    }"#
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 1_500_u64,
                "completion_tokens": 300_u64,
                "total_tokens": 1_800_u64
            }
        });
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer test-key-not-real"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&body_json))
            .mount(&server)
            .await;

        let cfg = OpenAIClientConfig {
            api_key: "test-key-not-real".into(),
            base_url: server.uri(),
            model: "gpt-4o-mini".into(),
            max_tokens: DEFAULT_MAX_TOKENS,
            timeout: Duration::from_secs(5),
        };
        let client = OpenAIClient::new(cfg).expect("client");
        let (_dir, transcript) = write_tmp_jsonl(&[r#"{"t0":0,"t1":1,"text":"hi"}"#]);
        let out = client
            .summarize(SummarizerInput {
                transcript: &transcript,
                meeting_type: MeetingType::Client,
                existing_action_items: None,
                existing_attendees: None,
                pre_meeting_briefing: None,
            })
            .await
            .expect("summarize");
        assert_eq!(out.body, "summary");
        assert_eq!(out.tags, vec!["acme".to_owned()]);
        assert_eq!(out.cost.tokens_in, 1_500);
        assert_eq!(out.cost.tokens_out, 300);
        // 1500 × $0.15/M + 300 × $0.60/M = $0.000225 + $0.00018 = $0.000405
        assert!(out.cost.summary_usd > 0.0, "cost must be positive");
    }

    #[tokio::test]
    async fn summarize_propagates_4xx_with_status_and_body() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_string(r#"{"error":{"message":"Incorrect API key provided"}}"#),
            )
            .mount(&server)
            .await;

        let cfg = OpenAIClientConfig {
            api_key: "bad-key".into(),
            base_url: server.uri(),
            model: "gpt-4o-mini".into(),
            max_tokens: DEFAULT_MAX_TOKENS,
            timeout: Duration::from_secs(5),
        };
        let client = OpenAIClient::new(cfg).expect("client");
        let (_dir, transcript) = write_tmp_jsonl(&[r#"{"text":"hi"}"#]);
        let err = client
            .summarize(SummarizerInput {
                transcript: &transcript,
                meeting_type: MeetingType::Client,
                existing_action_items: None,
                existing_attendees: None,
                pre_meeting_briefing: None,
            })
            .await
            .expect_err("4xx must surface");
        match err {
            LlmError::Backend(msg) => {
                assert!(msg.contains("401"), "missing status: {msg}");
                assert!(
                    msg.contains("Incorrect API key provided"),
                    "missing error message: {msg}"
                );
                // Distinguish from Anthropic 401 errors.
                assert!(
                    msg.contains("openai"),
                    "must be prefixed with openai: {msg}"
                );
            }
            other => panic!("expected Backend, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn summarize_propagates_5xx_with_status_and_body() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500).set_body_string("internal server error"))
            .mount(&server)
            .await;

        let cfg = OpenAIClientConfig {
            api_key: "test-key-not-real".into(),
            base_url: server.uri(),
            model: "gpt-4o-mini".into(),
            max_tokens: DEFAULT_MAX_TOKENS,
            timeout: Duration::from_secs(5),
        };
        let client = OpenAIClient::new(cfg).expect("client");
        let (_dir, transcript) = write_tmp_jsonl(&[r#"{"text":"hi"}"#]);
        let err = client
            .summarize(SummarizerInput {
                transcript: &transcript,
                meeting_type: MeetingType::Client,
                existing_action_items: None,
                existing_attendees: None,
                pre_meeting_briefing: None,
            })
            .await
            .expect_err("5xx must surface");
        match err {
            LlmError::Backend(msg) => {
                assert!(msg.contains("500"), "missing status: {msg}");
                assert!(
                    msg.contains("openai"),
                    "must be prefixed with openai: {msg}"
                );
            }
            other => panic!("expected Backend, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn summarize_handles_malformed_inner_json_with_loud_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // 200 OK but the choices[0].message.content is not valid JSON
        let body_json = serde_json::json!({
            "id": "chatcmpl-synthetic",
            "object": "chat.completion",
            "created": 1_700_000_000_u64,
            "model": "gpt-4o-mini",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "this is not json at all"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 100_u64,
                "completion_tokens": 10_u64,
                "total_tokens": 110_u64
            }
        });
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&body_json))
            .mount(&server)
            .await;

        let cfg = OpenAIClientConfig {
            api_key: "test-key-not-real".into(),
            base_url: server.uri(),
            model: "gpt-4o-mini".into(),
            max_tokens: DEFAULT_MAX_TOKENS,
            timeout: Duration::from_secs(5),
        };
        let client = OpenAIClient::new(cfg).expect("client");
        let (_dir, transcript) = write_tmp_jsonl(&[r#"{"text":"hi"}"#]);
        let err = client
            .summarize(SummarizerInput {
                transcript: &transcript,
                meeting_type: MeetingType::Client,
                existing_action_items: None,
                existing_attendees: None,
                pre_meeting_briefing: None,
            })
            .await
            .expect_err("malformed content must error");
        assert!(
            matches!(err, LlmError::Parse(_)),
            "expected Parse error for malformed inner JSON, got {err:?}"
        );
    }
}
