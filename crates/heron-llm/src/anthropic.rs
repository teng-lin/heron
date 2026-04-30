//! Real Anthropic Messages-API backend per
//! [`docs/archives/implementation.md`](../../../docs/archives/implementation.md) §11.1.
//!
//! Wraps `reqwest` with a strict request/response shape and a
//! configurable base URL so the unit tests can substitute a mock
//! server. The on-the-wire format follows the public Messages API:
//!
//! ```text
//! POST {base_url}/v1/messages
//! x-api-key: <key>
//! anthropic-version: 2023-06-01
//! content-type: application/json
//!
//! { "model": "...", "max_tokens": N, "messages": [...] }
//! ```
//!
//! The user message bundles the rendered prompt + the transcript
//! body. The transcript file is read with a hard size cap
//! ([`MAX_TRANSCRIPT_BYTES`]) so a runaway capture or a planted
//! file can't blow up memory / token budget.
//!
//! Cost is computed offline against
//! [`crate::cost::RATE_TABLE`] from the response's `usage` block —
//! the API is the source of truth, not the dashboard (per §11.4).

use std::time::Duration;

use async_trait::async_trait;
use heron_types::MeetingType;
use reqwest::header::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};

use crate::content::parse_content_json;
use crate::key_resolver::{EnvKeyResolver, KeyName, KeyResolveError, KeyResolver};
use crate::transcript::{build_user_content, read_transcript_capped, strip_speaker_names};
use crate::{LlmError, Summarizer, SummarizerInput, SummarizerOutput, render_meeting_prompt};

// Re-export the transcript-cap constants at their historical names
// so downstream callers (heron-cli status, the diagnostics tab) keep
// importing them from `heron_llm::anthropic::*`.
pub use crate::transcript::{
    MAX_TRANSCRIPT_BYTES, MAX_TRANSCRIPT_LINE_BYTES, TRANSCRIPT_WARN_BYTES,
};

/// Default API origin. Tests inject a wiremock URL via
/// [`AnthropicClientConfig::base_url`].
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// Pinned Messages API version. Bump when the wire shape moves; CI
/// fixtures will need to be regenerated against the new version.
pub const ANTHROPIC_API_VERSION: &str = "2023-06-01";

/// Default model. The user can override via
/// [`AnthropicClientConfig::model`]; the per-session config in the
/// orchestrator (week 11) ultimately wins.
pub const DEFAULT_MODEL: &str = "claude-sonnet-4-6";

/// Hard cap on the body snippet we paste into a non-2xx error
/// message. Misbehaving proxies could echo unbounded responses; bound
/// the snippet so logs don't flood and any echoed sensitive header
/// (defense-in-depth — Anthropic itself does not echo) is truncated.
pub const ERROR_BODY_SNIPPET_BYTES: usize = 2 * 1024;

/// Default `max_tokens` requested from the API per call. Generous
/// enough for a long structured summary; keeps the cost bounded.
pub const DEFAULT_MAX_TOKENS: u32 = 4_096;

/// Default request timeout. The Messages API typically responds in
/// 5–30 s for our prompt sizes; 120 s is the safety net for a large
/// transcript with retries chained downstream.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// Caller-supplied configuration. Use [`AnthropicClientConfig::from_env`]
/// for the standard `ANTHROPIC_API_KEY` flow; tests construct the
/// struct directly to swap `base_url` and `model`.
#[derive(Debug, Clone)]
pub struct AnthropicClientConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub max_tokens: u32,
    pub timeout: Duration,
}

impl AnthropicClientConfig {
    /// Read `ANTHROPIC_API_KEY` from the environment; default the
    /// rest. Returns [`LlmError::MissingApiKey`] if the env var is
    /// unset or empty so a misconfigured launch doesn't silently
    /// emit unauthenticated requests that 401 instead.
    ///
    /// Implemented in terms of [`Self::from_resolver`] with an
    /// [`EnvKeyResolver`] so the resolver-aware constructor stays the
    /// single source of truth for key-precedence behaviour. PR-μ /
    /// phase 74 — desktop callers that want the keychain fallback
    /// should reach for `from_resolver` directly with an
    /// `EnvThenKeychainResolver` (defined in the desktop crate at
    /// `apps/desktop/src-tauri/src/keychain_resolver.rs`).
    pub fn from_env() -> Result<Self, LlmError> {
        Self::from_resolver(&EnvKeyResolver)
    }

    /// Build a config by asking `resolver` for the Anthropic API key.
    ///
    /// PR-μ / phase 74: this is the resolver-aware path the
    /// `select_summarizer_with_resolver` factory feeds. Maps a
    /// `KeyResolveError::NotFound` to [`LlmError::MissingApiKey`] so
    /// callers see the same error variant regardless of whether the
    /// resolver consulted env-only or also checked the keychain.
    /// Other resolver failures (a macOS keychain backend error) come
    /// back as `LlmError::Backend` so they can be surfaced as a clean
    /// renderer-side toast distinct from "no key configured".
    pub fn from_resolver(resolver: &dyn KeyResolver) -> Result<Self, LlmError> {
        let api_key = resolver
            .resolve(KeyName::AnthropicApiKey)
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

/// Live Anthropic Messages-API summarizer.
///
/// Reuses one `reqwest::Client` across calls so connection pooling +
/// rustls session caching kick in for the typical "summarize once
/// per meeting" workload that runs every few minutes.
pub struct AnthropicClient {
    config: AnthropicClientConfig,
    http: reqwest::Client,
}

impl AnthropicClient {
    /// Build a client with the supplied config. Returns a
    /// [`LlmError::Backend`] if `reqwest` can't construct the inner
    /// client (e.g. the rustls bundle failed to load).
    pub fn new(config: AnthropicClientConfig) -> Result<Self, LlmError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            "anthropic-version",
            HeaderValue::from_static(ANTHROPIC_API_VERSION),
        );
        let mut auth = HeaderValue::from_str(&config.api_key)
            .map_err(|e| LlmError::Backend(format!("api key invalid as HTTP header: {e}")))?;
        auth.set_sensitive(true);
        headers.insert("x-api-key", auth);

        let http = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(config.timeout)
            .build()
            .map_err(|e| LlmError::Backend(format!("reqwest::Client build: {e}")))?;
        Ok(Self { config, http })
    }
}

#[async_trait]
impl Summarizer for AnthropicClient {
    async fn summarize(&self, input: SummarizerInput<'_>) -> Result<SummarizerOutput, LlmError> {
        let prompt = render_meeting_prompt(&input)?;
        let transcript_text = read_transcript_capped(input.transcript)?;
        if (transcript_text.len() as u64) >= TRANSCRIPT_WARN_BYTES {
            tracing::warn!(
                bytes = transcript_text.len(),
                cap = MAX_TRANSCRIPT_BYTES,
                warn_at = TRANSCRIPT_WARN_BYTES,
                "transcript approaches model input-token limit; \
                 consider chunked summarize or a higher-context model"
            );
        }
        // Tier 4 #21: pseudonymize speaker names *only* for the LLM
        // input. The orchestrator's `attendees` round-trip still uses
        // real names re-read from the prior summary.
        let transcript_for_llm = if input.strip_names {
            strip_speaker_names(&transcript_text)
        } else {
            transcript_text
        };
        let user_content = build_user_content(&prompt, &transcript_for_llm);

        let body = MessagesRequest {
            model: &self.config.model,
            max_tokens: self.config.max_tokens,
            messages: vec![Message {
                role: "user",
                content: &user_content,
            }],
        };
        let url = format!(
            "{base}/v1/messages",
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
            // Stream the body up to the snippet cap. `resp.text()` would
            // buffer the entire response into memory before we get a
            // chance to truncate, so a misbehaving proxy returning a
            // 10 GB body would OOM us before reaching the formatter.
            let snippet = read_capped_body_snippet(resp).await;
            return Err(LlmError::Backend(format!(
                "Anthropic API returned {status}: {snippet}"
            )));
        }

        let response: MessagesResponse = resp
            .json()
            .await
            .map_err(|e| LlmError::Backend(format!("response JSON parse: {e}")))?;
        parse_messages_response(response, input.meeting_type)
    }
}

/// Stream `resp.bytes_stream()` until we have at most
/// [`ERROR_BODY_SNIPPET_BYTES`] worth of payload, then drop the rest.
/// Avoids the OOM risk of `resp.text().await` on a misbehaving proxy
/// echoing an unbounded body. Returns a synthetic `<no body>` /
/// `<unreadable>` placeholder when the stream is empty or errors out
/// before we can truncate-render — neither case should leak the
/// outer status into the snippet.
async fn read_capped_body_snippet(mut resp: reqwest::Response) -> String {
    let mut buf: Vec<u8> = Vec::with_capacity(ERROR_BODY_SNIPPET_BYTES.min(8 * 1024));
    let mut total_observed: usize = 0;
    // `Response::chunk()` reads one HTTP chunk at a time; no Stream
    // trait dance needed. We pull chunks until we either fill the
    // snippet or hit a hard ceiling on how much we'll drain to be
    // polite to the connection pool.
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
                    // Hard ceiling: don't burn time on a 10 GiB
                    // nuisance response just to be polite.
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
    let body_str = match std::str::from_utf8(&buf) {
        Ok(s) => s,
        Err(_) => return "<non-utf8 body>".to_owned(),
    };
    if total_observed > buf.len() {
        format!("{body_str} ...[truncated, observed at least {total_observed} bytes]",)
    } else {
        body_str.to_owned()
    }
}

/// Pure parser: takes a `MessagesResponse` and produces a
/// `SummarizerOutput`. Split out so the unit tests can exercise the
/// JSON-content parsing without an HTTP round-trip.
pub fn parse_messages_response(
    response: MessagesResponse,
    meeting_type_fallback: MeetingType,
) -> Result<SummarizerOutput, LlmError> {
    let text = response
        .content
        .iter()
        .find_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            ContentBlock::Other => None,
        })
        .ok_or_else(|| LlmError::Parse("response had no `text` content block".to_owned()))?;
    let body = parse_content_json(text)?;

    // Per §11.4: the API is the source of truth for token counts,
    // including prompt-cache fields. Fold cache_creation +
    // cache_read into the input token count so cost reflects the
    // actual billed amount, not just non-cached input tokens.
    let total_input_tokens = response
        .usage
        .input_tokens
        .saturating_add(response.usage.cache_creation_input_tokens)
        .saturating_add(response.usage.cache_read_input_tokens);
    let cost = crate::cost::compute_cost(
        &response.model,
        total_input_tokens,
        response.usage.output_tokens,
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

#[derive(Debug, Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: Vec<Message<'a>>,
}

#[derive(Debug, Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

/// Public so [`parse_messages_response`] can accept it from a test.
#[derive(Debug, Deserialize)]
pub struct MessagesResponse {
    pub model: String,
    pub content: Vec<ContentBlock>,
    pub usage: Usage,
}

/// Content block from the Messages API. We only act on `Text`; other
/// kinds (`tool_use`, `thinking`, `image`) are accepted via the
/// `Other` catch-all and silently skipped, so a model that emits
/// thinking/tool blocks before its text answer doesn't poison the
/// whole response with a deserializer error.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    #[serde(other)]
    Other,
}

/// Token counts as returned by the Messages API. `input_tokens` is
/// the non-cached input. Per §11.4 we fold the cache fields into the
/// billed total when computing cost. Both cache fields default to 0
/// for old responses / non-cached calls.
#[derive(Debug, Deserialize, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::io::Write;
    use std::path::PathBuf;

    use heron_types::{ActionItem, ItemId, MeetingType};

    use super::*;
    // Crate-wide single ENV_LOCK so this module's tests don't race
    // `key_resolver::tests` over `ANTHROPIC_API_KEY`. See
    // [`crate::test_env`] for the rationale (two module-private
    // mutexes would race the same env var).
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

    fn sample_messages_response(content_json: &str, model: &str) -> MessagesResponse {
        MessagesResponse {
            model: model.to_owned(),
            content: vec![ContentBlock::Text {
                text: content_json.to_owned(),
            }],
            usage: Usage {
                input_tokens: 1_000,
                output_tokens: 200,
                ..Usage::default()
            },
        }
    }

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
        let resp = sample_messages_response(body_json, "claude-sonnet-4-6");
        let out = parse_messages_response(resp, MeetingType::Other).expect("parse");
        assert_eq!(out.body, "Meeting summary here.");
        assert_eq!(out.company.as_deref(), Some("Acme"));
        assert_eq!(out.meeting_type, MeetingType::Client);
        assert_eq!(out.tags, vec!["acme".to_owned(), "pricing".to_owned()]);
        assert_eq!(out.action_items.len(), 1);
        assert_eq!(out.action_items[0].owner, "me");
        // 1000 in × $3/M  + 200 out × $15/M = $0.003 + $0.003 = $0.006.
        assert_eq!(out.cost.summary_usd, 0.006);
        assert_eq!(out.cost.tokens_in, 1_000);
        assert_eq!(out.cost.tokens_out, 200);
        assert_eq!(out.cost.model, "claude-sonnet-4-6");
    }

    #[test]
    fn parse_response_falls_back_to_caller_meeting_type_when_missing() {
        // The LLM omits meeting_type; we should fall back rather than
        // forcing the user to specify it twice (input + output).
        let body_json = r#"{ "body":"x" }"#;
        let resp = sample_messages_response(body_json, "claude-sonnet-4-6");
        let out = parse_messages_response(resp, MeetingType::Internal).expect("parse");
        assert_eq!(out.meeting_type, MeetingType::Internal);
    }

    #[test]
    fn parse_response_errors_on_no_text_block() {
        let resp = MessagesResponse {
            model: "claude-sonnet-4-6".into(),
            content: vec![],
            usage: Usage::default(),
        };
        let err = parse_messages_response(resp, MeetingType::Other).expect_err("no content");
        assert!(matches!(err, LlmError::Parse(_)));
    }

    #[test]
    fn parse_response_errors_on_malformed_content_json() {
        let resp = sample_messages_response("not json at all", "claude-sonnet-4-6");
        let err = parse_messages_response(resp, MeetingType::Other).expect_err("malformed");
        assert!(matches!(err, LlmError::Parse(_)));
    }

    #[test]
    fn parse_response_errors_on_unknown_model_via_cost_lookup() {
        let resp = sample_messages_response(r#"{"body":"x"}"#, "gpt-99-supergiant");
        let err = parse_messages_response(resp, MeetingType::Other).expect_err("unknown model");
        // CostError's Display starts with `unknown model "..."`; the
        // wrapping LlmError::Backend prefixes `cost lookup: ...`.
        match err {
            LlmError::Backend(s) => {
                assert!(s.contains("cost lookup"), "missing wrapper: {s}");
                assert!(s.contains("gpt-99-supergiant"), "missing model name: {s}");
            }
            other => panic!("expected Backend, got {other:?}"),
        }
    }

    #[test]
    fn parse_response_resolves_model_with_version_suffix() {
        // API often returns identifiers with a date suffix.
        let resp = sample_messages_response(r#"{"body":"x"}"#, "claude-haiku-4-5-20251001");
        let out = parse_messages_response(resp, MeetingType::Other).expect("ok");
        assert_eq!(out.cost.model, "claude-haiku-4-5-20251001");
    }

    #[test]
    fn parse_response_preserves_action_item_ids_for_merge() {
        let id = ItemId::from_u128(0x12345678_9abc_4def_8000_000000000001);
        let body_json = format!(
            r#"{{ "body":"x", "action_items":[{{"id":"{id}","owner":"me","text":"go"}}] }}"#
        );
        let resp = sample_messages_response(&body_json, "claude-sonnet-4-6");
        let out = parse_messages_response(resp, MeetingType::Client).expect("ok");
        assert_eq!(out.action_items.len(), 1);
        assert_eq!(out.action_items[0].id, id);
    }

    #[test]
    fn read_transcript_capped_passes_small_files_through() {
        let (_dir, path) = write_tmp_jsonl(&[
            r#"{"t0":0,"t1":1,"text":"hi","channel":"mic","speaker":"me","speaker_source":"self","confidence":0.9}"#,
            r#"{"t0":1,"t1":2,"text":"there","channel":"mic","speaker":"me","speaker_source":"self","confidence":0.9}"#,
        ]);
        let body = read_transcript_capped(&path).expect("read");
        assert!(body.contains(r#""text":"hi""#));
        assert!(body.contains(r#""text":"there""#));
    }

    #[test]
    fn read_transcript_capped_rejects_oversize_file() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("transcript.jsonl");
        let payload = vec![b'x'; (MAX_TRANSCRIPT_BYTES + 1024) as usize];
        std::fs::write(&path, payload).expect("write");
        let err = read_transcript_capped(&path).expect_err("over-cap");
        assert!(matches!(err, LlmError::Backend(s) if s.contains("exceeds")));
    }

    #[test]
    fn read_transcript_capped_counts_drained_bytes_toward_total_cap() {
        // Per gemini's PR-36 review: a file made of repeated over-cap
        // lines used to read its way past MAX_TRANSCRIPT_BYTES, since
        // skipped lines didn't count toward the running total. Plant
        // enough oversize lines + drain bytes to exceed the cap and
        // verify the function stops short rather than slurping the
        // whole file.
        //
        // Tighter than the OOM check this guards against — we want
        // the *total bytes consumed* (line + drained tail) to exceed
        // the cap and confirm the loop bails.
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("transcript.jsonl");
        let mut f = std::fs::File::create(&path).expect("create");

        // Write oversize lines until we comfortably exceed the cap.
        // Each iteration writes (MAX_TRANSCRIPT_LINE_BYTES + 100)
        // bytes plus a newline.
        let per_line = (MAX_TRANSCRIPT_LINE_BYTES + 100) as usize + 1;
        let needed = (MAX_TRANSCRIPT_BYTES as usize) + 4 * per_line;
        let line: Vec<u8> = vec![b'a'; per_line - 1];
        let mut written = 0usize;
        while written < needed {
            f.write_all(&line).expect("oversize");
            f.write_all(b"\n").expect("nl");
            written += per_line;
        }
        // File is over the cap; fail-closed at the top-level
        // metadata gate (since file size > MAX_TRANSCRIPT_BYTES).
        let err = read_transcript_capped(&path).expect_err("over-cap file rejected");
        match err {
            LlmError::Backend(s) => assert!(s.contains("exceeds"), "wrong error: {s}"),
            other => panic!("expected Backend, got {other:?}"),
        }
    }

    #[test]
    fn read_transcript_capped_oversize_then_okay_lines_still_caps_total() {
        // Gemini's #2 + #3 review: build a file that's *under* the
        // 4 MiB top-level cap but whose drained over-cap-line bytes
        // would otherwise let us exceed the per-call running total.
        // Write 5 over-cap lines + a few small lines; the per-call
        // total counter should account for the drained bytes and
        // bail without slurping past MAX_TRANSCRIPT_BYTES.
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("transcript.jsonl");
        // Use a smaller test cap by lowering the under-cap line
        // count so the file fits under MAX_TRANSCRIPT_BYTES but the
        // drained bytes still bounce us out early.
        let mut f = std::fs::File::create(&path).expect("create");
        // 3 oversize lines of MAX_LINE+10 each — well under the
        // 4 MiB file cap (each line ~64 KiB).
        let oversize: Vec<u8> = vec![b'b'; (MAX_TRANSCRIPT_LINE_BYTES + 10) as usize];
        for _ in 0..3 {
            f.write_all(&oversize).expect("oversize");
            f.write_all(b"\n").expect("nl");
        }
        f.write_all(b"keeper-1\nkeeper-2\n").expect("keepers");
        drop(f);

        let body = read_transcript_capped(&path).expect("read");
        // Both small keeper lines arrive; oversize bodies don't.
        assert!(body.contains("keeper-1"));
        assert!(body.contains("keeper-2"));
        assert!(
            !body.contains(&"b".repeat(MAX_TRANSCRIPT_LINE_BYTES as usize)),
            "oversize line bodies must not survive"
        );
    }

    #[test]
    fn read_transcript_capped_drops_oversize_lines_but_keeps_following_lines() {
        // A single over-cap line should be dropped; subsequent lines
        // must still appear. Defensive: the transcript writer never
        // emits such a line, but a corrupted file shouldn't poison
        // the whole prompt.
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("transcript.jsonl");
        let mut f = std::fs::File::create(&path).expect("create");
        let oversize_line = vec![b'a'; (MAX_TRANSCRIPT_LINE_BYTES + 100) as usize];
        f.write_all(&oversize_line).expect("oversize");
        f.write_all(b"\n").expect("nl");
        f.write_all(b"keeper line\n").expect("keeper");

        let body = read_transcript_capped(&path).expect("read");
        assert!(body.contains("keeper line"));
        // The oversize line's `aaaaaa...` must NOT survive.
        assert!(
            !body.contains(&"a".repeat(MAX_TRANSCRIPT_LINE_BYTES as usize)),
            "oversize line should have been dropped"
        );
    }

    #[test]
    fn from_env_errors_when_api_key_missing() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let saved = std::env::var_os("ANTHROPIC_API_KEY");
        // SAFETY: process-global env mutation is unsafe under Rust 2024
        // edition. ENV_LOCK serializes env-touching tests; the restore
        // on exit keeps the post-test state matching the pre-test state.
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
        }
        let err = AnthropicClientConfig::from_env().expect_err("missing key");
        assert!(matches!(err, LlmError::MissingApiKey));
        // SAFETY: restoring prior value, see above.
        unsafe {
            if let Some(v) = saved {
                std::env::set_var("ANTHROPIC_API_KEY", v);
            }
        }
    }

    #[test]
    fn from_resolver_uses_resolver_value() {
        // A bespoke resolver that returns a fixed key — proves
        // `from_resolver` is wired to the trait, not to the env var
        // directly. This is the contract the desktop crate's
        // `EnvThenKeychainResolver` relies on.
        struct Stub;
        impl KeyResolver for Stub {
            fn resolve(&self, _name: KeyName) -> Result<String, KeyResolveError> {
                Ok("from-stub-resolver".to_owned())
            }
        }
        let cfg = AnthropicClientConfig::from_resolver(&Stub).expect("ok");
        assert_eq!(cfg.api_key, "from-stub-resolver");
    }

    #[test]
    fn from_resolver_maps_not_found_to_missing_api_key() {
        // `NotFound` from the resolver is the "neither env nor keychain
        // had a value" case. The Anthropic constructor must surface
        // it as `MissingApiKey` so the existing renderer toast
        // ("set ANTHROPIC_API_KEY ...") fires unchanged.
        struct Stub;
        impl KeyResolver for Stub {
            fn resolve(&self, name: KeyName) -> Result<String, KeyResolveError> {
                Err(KeyResolveError::NotFound(name))
            }
        }
        let err = AnthropicClientConfig::from_resolver(&Stub).expect_err("missing");
        assert!(matches!(err, LlmError::MissingApiKey));
    }

    #[test]
    fn from_resolver_maps_backend_to_llm_backend_error() {
        // Distinct path from `MissingApiKey` — a keychain backend
        // failure shouldn't masquerade as "user forgot to set the
        // key". The renderer can render a different toast.
        struct Stub;
        impl KeyResolver for Stub {
            fn resolve(&self, _name: KeyName) -> Result<String, KeyResolveError> {
                Err(KeyResolveError::Backend("simulated keychain error".into()))
            }
        }
        let err = AnthropicClientConfig::from_resolver(&Stub).expect_err("backend");
        match err {
            LlmError::Backend(msg) => {
                assert!(
                    msg.contains("simulated keychain error"),
                    "expected backend error to be wrapped, got: {msg}"
                );
                assert!(
                    msg.contains("api key resolver"),
                    "expected resolver-prefixed error, got: {msg}"
                );
            }
            other => panic!("expected Backend, got {other:?}"),
        }
    }

    #[test]
    fn from_env_errors_when_api_key_is_empty_string() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        // Some shells `export VAR=` without unsetting it. Treat that
        // as missing rather than passing an empty Authorization header.
        let saved = std::env::var_os("ANTHROPIC_API_KEY");
        // SAFETY: see from_env_errors_when_api_key_missing.
        unsafe {
            std::env::set_var("ANTHROPIC_API_KEY", "");
        }
        let err = AnthropicClientConfig::from_env().expect_err("empty key");
        assert!(matches!(err, LlmError::MissingApiKey));
        // SAFETY: restoring prior value, see above.
        unsafe {
            match saved {
                Some(v) => std::env::set_var("ANTHROPIC_API_KEY", v),
                None => std::env::remove_var("ANTHROPIC_API_KEY"),
            }
        }
    }

    // The phase-35 truncate_for_log helper was removed when the 4xx
    // path switched to the chunked `read_capped_body_snippet`
    // streamer that bounds memory before truncation. The byte-level
    // cap is now exercised end-to-end via the wiremock 4xx test
    // below — a synthetic 401 with an oversized body asserts the
    // streamer caps the snippet without OOMing.
    #[tokio::test]
    async fn capped_body_snippet_truncates_oversize_4xx_body_in_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Plant a body larger than the snippet cap. The test uses
        // a deterministic ASCII payload so the assert can pin the
        // cap behavior without UTF-8 boundary noise.
        let oversized = "X".repeat(ERROR_BODY_SNIPPET_BYTES * 4);
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_string(oversized.clone()))
            .mount(&server)
            .await;

        let cfg = AnthropicClientConfig {
            api_key: "test-key-not-real".into(),
            base_url: server.uri(),
            model: "claude-sonnet-4-6".into(),
            max_tokens: 4_096,
            timeout: Duration::from_secs(5),
        };
        let client = AnthropicClient::new(cfg).expect("client");
        let (_dir, transcript) = write_tmp_jsonl(&[r#"{"text":"hi"}"#]);
        let err = client
            .summarize(SummarizerInput {
                transcript: &transcript,
                meeting_type: MeetingType::Client,
                existing_action_items: None,
                existing_attendees: None,
                pre_meeting_briefing: None,
                persona: None,
                strip_names: false,
            })
            .await
            .expect_err("4xx should error");

        let LlmError::Backend(msg) = err else {
            panic!("expected Backend, got other variant");
        };
        // The full status + a snippet of the body should appear,
        // but the formatted error mustn't carry the entire 4×-cap
        // payload — that's the OOM-defense contract.
        assert!(msg.contains("401"), "missing status: {}", &msg[..100]);
        assert!(
            msg.len() < oversized.len(),
            "snippet must be smaller than the body — got {} vs {}",
            msg.len(),
            oversized.len()
        );
        assert!(
            msg.contains("truncated"),
            "missing truncation marker: {}",
            &msg[..200]
        );
    }

    #[test]
    fn parse_response_skips_non_text_blocks_and_finds_text() {
        // The model can emit thinking + tool_use blocks before the
        // text answer. The parser must skip them rather than fail.
        let resp_json = serde_json::json!({
            "model": "claude-sonnet-4-6",
            "content": [
                {"type": "thinking", "thinking": "let me think..."},
                {"type": "tool_use", "id": "t1", "name": "noop", "input": {}},
                {"type": "text", "text": r#"{"body":"the answer"}"#}
            ],
            "usage": {"input_tokens": 1_000, "output_tokens": 100}
        });
        let resp: MessagesResponse =
            serde_json::from_value(resp_json).expect("response deserialize");
        let out = parse_messages_response(resp, MeetingType::Other).expect("ok");
        assert_eq!(out.body, "the answer");
    }

    #[test]
    fn parse_response_folds_cache_tokens_into_cost() {
        // Per §11.4: prompt-cache fields count toward billed input
        // tokens. A response with non-trivial cache_read should
        // increase the computed cost vs the same call without
        // caching.
        let body_text = r#"{"body":"x"}"#;
        let cached = MessagesResponse {
            model: "claude-sonnet-4-6".into(),
            content: vec![ContentBlock::Text {
                text: body_text.into(),
            }],
            usage: Usage {
                input_tokens: 1_000,
                output_tokens: 100,
                cache_creation_input_tokens: 5_000,
                cache_read_input_tokens: 2_000,
            },
        };
        let uncached = MessagesResponse {
            model: "claude-sonnet-4-6".into(),
            content: vec![ContentBlock::Text {
                text: body_text.into(),
            }],
            usage: Usage {
                input_tokens: 1_000,
                output_tokens: 100,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            },
        };
        let cached_cost = parse_messages_response(cached, MeetingType::Other)
            .expect("cached")
            .cost
            .summary_usd;
        let uncached_cost = parse_messages_response(uncached, MeetingType::Other)
            .expect("uncached")
            .cost
            .summary_usd;
        assert!(
            cached_cost > uncached_cost,
            "cached run should bill higher input total: {cached_cost} vs {uncached_cost}"
        );
    }

    #[test]
    fn client_construction_attaches_required_headers() {
        // Build with a synthetic key + a base_url that the client
        // will not actually contact in this test (no live HTTP);
        // we're verifying the constructor wiring, not behavior.
        let cfg = AnthropicClientConfig {
            api_key: "test-key-not-real".into(),
            base_url: DEFAULT_BASE_URL.into(),
            model: DEFAULT_MODEL.into(),
            max_tokens: DEFAULT_MAX_TOKENS,
            timeout: Duration::from_secs(5),
        };
        let client = AnthropicClient::new(cfg).expect("client");
        // Sanity: model + base_url are persisted for tests / debug.
        assert_eq!(client.config.model, DEFAULT_MODEL);
        assert_eq!(client.config.base_url, DEFAULT_BASE_URL);
    }

    #[tokio::test]
    async fn live_summarize_against_wiremock_round_trips() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body_json = serde_json::json!({
            "id": "msg_synthetic",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-6",
            "content": [
                {"type": "text", "text": r#"{
                    "body":"summary",
                    "company":"Acme",
                    "meeting_type":"client",
                    "tags":["acme"],
                    "action_items":[],
                    "attendees":[]
                }"#}
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1_500, "output_tokens": 300}
        });
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("anthropic-version", ANTHROPIC_API_VERSION))
            .and(header("x-api-key", "test-key-not-real"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&body_json))
            .mount(&server)
            .await;

        let cfg = AnthropicClientConfig {
            api_key: "test-key-not-real".into(),
            base_url: server.uri(),
            model: "claude-sonnet-4-6".into(),
            max_tokens: 4_096,
            timeout: Duration::from_secs(5),
        };
        let client = AnthropicClient::new(cfg).expect("client");
        let (_dir, transcript) = write_tmp_jsonl(&[r#"{"t0":0,"t1":1,"text":"hi"}"#]);
        let out = client
            .summarize(SummarizerInput {
                transcript: &transcript,
                meeting_type: MeetingType::Client,
                existing_action_items: None,
                existing_attendees: None,
                pre_meeting_briefing: None,
                persona: None,
                strip_names: false,
            })
            .await
            .expect("summarize");
        assert_eq!(out.body, "summary");
        assert_eq!(out.tags, vec!["acme".to_owned()]);
        assert_eq!(out.cost.tokens_in, 1_500);
        assert_eq!(out.cost.tokens_out, 300);
    }

    #[tokio::test]
    async fn summarize_propagates_4xx_with_status_and_body() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(401).set_body_string(r#"{"error":{"message":"bad key"}}"#),
            )
            .mount(&server)
            .await;

        let cfg = AnthropicClientConfig {
            api_key: "test-key-not-real".into(),
            base_url: server.uri(),
            model: "claude-sonnet-4-6".into(),
            max_tokens: 4_096,
            timeout: Duration::from_secs(5),
        };
        let client = AnthropicClient::new(cfg).expect("client");
        let (_dir, transcript) = write_tmp_jsonl(&[r#"{"text":"hi"}"#]);
        let err = client
            .summarize(SummarizerInput {
                transcript: &transcript,
                meeting_type: MeetingType::Client,
                existing_action_items: None,
                existing_attendees: None,
                pre_meeting_briefing: None,
                persona: None,
                strip_names: false,
            })
            .await
            .expect_err("4xx must surface");
        match err {
            LlmError::Backend(msg) => {
                assert!(msg.contains("401"), "missing status: {msg}");
                assert!(msg.contains("bad key"), "missing body: {msg}");
            }
            other => panic!("expected Backend, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn summarize_propagates_500_with_status() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
            .mount(&server)
            .await;

        let cfg = AnthropicClientConfig {
            api_key: "test-key-not-real".into(),
            base_url: server.uri(),
            model: "claude-sonnet-4-6".into(),
            max_tokens: 4_096,
            timeout: Duration::from_secs(5),
        };
        let client = AnthropicClient::new(cfg).expect("client");
        let (_dir, transcript) = write_tmp_jsonl(&[r#"{"text":"hi"}"#]);
        let err = client
            .summarize(SummarizerInput {
                transcript: &transcript,
                meeting_type: MeetingType::Client,
                existing_action_items: None,
                existing_attendees: None,
                pre_meeting_briefing: None,
                persona: None,
                strip_names: false,
            })
            .await
            .expect_err("5xx must surface");
        if let LlmError::Backend(msg) = err {
            assert!(msg.contains("500"), "missing status: {msg}");
        } else {
            panic!("expected Backend variant");
        }
    }

    #[tokio::test]
    async fn summarize_request_includes_existing_id_block_for_resummarize() {
        // Re-summarize path: the request body's user content should
        // carry the prior action-item ID so the LLM can preserve it
        // (per §10.5 layer 1). Capture the body via a wiremock
        // request matcher.
        use wiremock::matchers::{body_string_contains, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let id = ItemId::from_u128(0xdead_beef_cafe_4f00_8000_0000_0000_0001);
        let server = MockServer::start().await;
        let body_json = serde_json::json!({
            "id": "msg_synth",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-6",
            "content": [{"type": "text", "text": r#"{"body":"x"}"#}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(body_string_contains(id.to_string()))
            .respond_with(ResponseTemplate::new(200).set_body_json(&body_json))
            .mount(&server)
            .await;

        let cfg = AnthropicClientConfig {
            api_key: "test-key-not-real".into(),
            base_url: server.uri(),
            model: "claude-sonnet-4-6".into(),
            max_tokens: 4_096,
            timeout: Duration::from_secs(5),
        };
        let client = AnthropicClient::new(cfg).expect("client");
        let (_dir, transcript) = write_tmp_jsonl(&[r#"{"text":"hi"}"#]);
        let priors = vec![ActionItem {
            id,
            owner: "me".into(),
            text: "preexisting".into(),
            due: None,
        }];
        let out = client
            .summarize(SummarizerInput {
                transcript: &transcript,
                meeting_type: MeetingType::Client,
                existing_action_items: Some(&priors),
                existing_attendees: None,
                pre_meeting_briefing: None,
                persona: None,
                strip_names: false,
            })
            .await
            .expect("ok");
        assert_eq!(out.body, "x");
    }
}
