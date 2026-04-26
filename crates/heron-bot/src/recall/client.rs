//! Thin Recall.ai REST client. The driver (`super::driver`) layers
//! FSM logic + state tracking on top; this module owns the wire
//! contract.
//!
//! Mirrors the spike harness (`examples/recall-spike.rs`) but pares it
//! down to what `RecallDriver` actually needs: create / get / leave /
//! delete. The transcript & output_audio endpoints stay in the spike
//! — they belong to the speech-control surface, which is a separate
//! gap (heron-policy + heron-realtime).

use std::time::Duration;

use futures_util::StreamExt;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue, RETRY_AFTER};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;

/// Default per-request timeout. Recall's documented p95 is sub-second
/// but `create_bot` against a busy region has been seen up to ~700ms;
/// 30s gives generous headroom without holding a polling task hostage
/// to a stuck connection.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Response cap used when echoing vendor error bodies into [`HttpError`].
/// Keeps a runaway 5MB Recall HTML stack-trace from blowing up the
/// log line.
const ERROR_BODY_SNIPPET_BYTES: usize = 1024;

/// Cap for response bodies the driver actually parses. Recall's `BotDetail`
/// is a few KB at most, even with a long `status_changes` history; 1MB
/// gives generous headroom while preventing a malformed / runaway response
/// from forcing an unbounded allocation. Per the gemini-code-assist
/// review on PR #121.
const MAX_RESPONSE_BYTES: usize = 1_048_576;

/// Configuration for a [`Client`]. The driver constructs one of these
/// at startup; per-request state lives on [`Client`] itself.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub api_key: String,
    pub base_url: String,
    pub timeout: Duration,
}

impl ClientConfig {
    /// Sensible defaults for production use; caller must supply the
    /// API key + base URL themselves (no env reads here — those live
    /// in [`crate::recall::RecallDriverConfig::from_env`]).
    pub fn new(api_key: String, base_url: String) -> Self {
        Self {
            api_key,
            base_url,
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

/// Recall.ai REST client. Cheap to clone — `reqwest::Client` is
/// `Arc`-internal.
#[derive(Clone)]
pub(crate) struct Client {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl Client {
    pub(crate) fn new(config: ClientConfig) -> Result<Self, HttpError> {
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|e| HttpError::Build(e.to_string()))?;
        Ok(Self {
            http,
            base_url: config.base_url,
            api_key: config.api_key,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url.trim_end_matches('/'), path)
    }

    fn auth_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        // `Token <key>` is Recall's documented auth scheme. `from_str`
        // can fail only on non-ASCII bytes; the API key is opaque ASCII
        // by Recall's contract, so we surface the (unlikely) error
        // path as `HttpError::Build`.
        if let Ok(value) = HeaderValue::from_str(&format!("Token {}", self.api_key)) {
            headers.insert(AUTHORIZATION, value);
        }
        headers
    }

    /// `POST /api/v1/bot/` — dispatch a bot. The `Idempotency-Key`
    /// header is forwarded verbatim per spec Invariant 14.
    pub(crate) async fn create_bot(
        &self,
        args: CreateBotArgs<'_>,
        idempotency_key: Uuid,
    ) -> Result<BotDetail, HttpError> {
        let mut body = json!({
            "meeting_url": args.meeting_url,
            "bot_name": args.bot_name,
            "recording_config": {
                "transcript": {
                    "provider": { "meeting_captions": {} }
                }
            },
        });
        body["automatic_audio_output"] = json!({
            "in_call_recording": {
                "data": { "kind": "mp3", "b64_data": args.placeholder_audio_b64 }
            }
        });
        // Echo our caller-supplied metadata into Recall's
        // `metadata` field. Recall stores it as opaque JSON and
        // re-emits on webhooks — useful for cross-correlating once
        // the webhook receiver lands.
        if !args.metadata.is_null() {
            body["metadata"] = args.metadata.clone();
        }

        let mut req = self
            .http
            .post(self.url("/api/v1/bot/"))
            .headers(self.auth_headers())
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            // Lowercase hyphenated form per spec Invariant 14 + Recall's
            // documented header. Recall is case-insensitive but we pin
            // a single form so logs / replays match byte-for-byte.
            .header(
                "Idempotency-Key",
                idempotency_key.as_hyphenated().to_string(),
            )
            .json(&body);
        if let Some(extra) = args.metadata_header.as_deref() {
            req = req.header("X-Heron-Metadata", extra);
        }

        let resp = req.send().await.map_err(http_send_error)?;
        let (status, headers, body_text, truncated) = drain(resp).await?;
        if !status.is_success() {
            return Err(classify_http_error(status, &headers, body_text));
        }
        if truncated {
            return Err(HttpError::Decode(format!(
                "create_bot response exceeded {MAX_RESPONSE_BYTES}-byte cap"
            )));
        }
        let detail: BotDetail = serde_json::from_str(&body_text).map_err(|e| {
            HttpError::Decode(format!(
                "decode create_bot response: {e} (body: {})",
                truncate(&body_text, 256)
            ))
        })?;
        Ok(detail)
    }

    /// `GET /api/v1/bot/{id}/` — full detail (incl. `status_changes`).
    pub(crate) async fn get_bot(&self, vendor_id: &str) -> Result<BotDetail, HttpError> {
        let path = format!("/api/v1/bot/{vendor_id}/");
        let resp = self
            .http
            .get(self.url(&path))
            .headers(self.auth_headers())
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(http_send_error)?;
        let (status, headers, body_text, truncated) = drain(resp).await?;
        if !status.is_success() {
            return Err(classify_http_error(status, &headers, body_text));
        }
        if truncated {
            return Err(HttpError::Decode(format!(
                "get_bot response exceeded {MAX_RESPONSE_BYTES}-byte cap"
            )));
        }
        let detail: BotDetail = serde_json::from_str(&body_text).map_err(|e| {
            HttpError::Decode(format!(
                "decode get_bot response: {e} (body: {})",
                truncate(&body_text, 256)
            ))
        })?;
        Ok(detail)
    }

    /// `POST /api/v1/bot/{id}/leave_call/` — graceful leave. Returns
    /// the response body as opaque JSON (Recall echoes the bot
    /// detail; the driver doesn't use it).
    pub(crate) async fn leave_call(&self, vendor_id: &str) -> Result<(), HttpError> {
        let path = format!("/api/v1/bot/{vendor_id}/leave_call/");
        let resp = self
            .http
            .post(self.url(&path))
            .headers(self.auth_headers())
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(http_send_error)?;
        let (status, headers, body_text, _) = drain(resp).await?;
        if !status.is_success() {
            return Err(classify_http_error(status, &headers, body_text));
        }
        Ok(())
    }

    /// `DELETE /api/v1/bot/{id}/` — only legal pre-join per Recall.
    /// The driver gates this against the FSM state before calling.
    pub(crate) async fn delete_bot(&self, vendor_id: &str) -> Result<(), HttpError> {
        let path = format!("/api/v1/bot/{vendor_id}/");
        let resp = self
            .http
            .delete(self.url(&path))
            .headers(self.auth_headers())
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(http_send_error)?;
        let (status, headers, body_text, _) = drain(resp).await?;
        if !status.is_success() {
            return Err(classify_http_error(status, &headers, body_text));
        }
        Ok(())
    }
}

/// Snapshot status + headers, then read the body with the cap. The
/// `(status, headers, body, truncated)` tuple is the lowest-common-
/// denominator the four request methods share.
async fn drain(
    resp: reqwest::Response,
) -> Result<(reqwest::StatusCode, HeaderMap, String, bool), HttpError> {
    let status = resp.status();
    let headers = resp.headers().clone();
    let (text, truncated) = read_capped(resp).await?;
    Ok((status, headers, text, truncated))
}

/// Arguments to [`Client::create_bot`]. Borrowed so the driver can
/// supply its in-flight `BotCreateArgs` without re-allocating.
pub(crate) struct CreateBotArgs<'a> {
    pub meeting_url: &'a str,
    pub bot_name: &'a str,
    /// Base64-encoded MP3 — required by Recall before any
    /// `output_audio` POST will work. Even though `RecallDriver`
    /// doesn't expose `output_audio` yet (out of scope), shipping a
    /// placeholder here means the production speech path won't need
    /// a second `bot_create` to flip the flag.
    pub placeholder_audio_b64: &'a str,
    /// Echoed back on every Recall event for this bot. Always sent
    /// (even when `Value::Null`) so downstream webhook receivers
    /// see a stable key.
    pub metadata: &'a Value,
    /// Optional client-tag forwarded as `X-Heron-Metadata`. Useful for
    /// the eventual webhook receiver to correlate without parsing the
    /// echoed JSON.
    pub metadata_header: Option<String>,
}

/// Recall's bot detail. Subset of the documented schema — fields the
/// driver actually consumes. Other fields are silently dropped.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct BotDetail {
    pub id: String,
    #[serde(default)]
    pub status_changes: Vec<StatusChange>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct StatusChange {
    pub code: String,
    #[serde(default)]
    pub sub_code: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

/// Errors raised by [`Client`] methods. Mapped 1:1 to
/// [`crate::BotError`] variants by the driver.
#[derive(Debug, Error)]
pub(crate) enum HttpError {
    #[error("recall network error: {0}")]
    Network(String),

    /// 404 — the vendor doesn't recognize the bot id. Distinct so
    /// `bot_leave` / `bot_terminate` can treat it as idempotent.
    #[error("recall reports bot not found")]
    NotFound,

    /// 429 — rate limit. `retry_after_secs` is parsed from the
    /// `Retry-After` header (defaulted to 60s if missing).
    #[error("recall rate limited; retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },

    /// 507 — capacity exhausted. Per [`docs/archives/spike-findings.md`]
    /// §"Recommendations" item 7, Recall reserves this code for
    /// "warm-bot pool depleted on Create Bot" only and recommends
    /// polling every 30s. We default to 30s if `Retry-After` is
    /// missing rather than the protocol-default 300s — a 5-minute
    /// wait would mask the recovery window.
    #[error("recall capacity exhausted; retry after {retry_after_secs}s")]
    CapacityExhausted { retry_after_secs: u64 },

    /// Other 4xx / 5xx. Status + body snippet preserved.
    #[error("recall vendor error: status {status}: {body}")]
    Vendor { status: u16, body: String },

    /// Build-time failure (TLS init, header parse).
    #[error("recall client build failure: {0}")]
    Build(String),

    /// Response body didn't decode against our schema. Distinct from
    /// `Network` so a Recall schema change is loud rather than
    /// disguised as a connection problem.
    #[error("recall response decode error: {0}")]
    Decode(String),
}

fn http_send_error(e: reqwest::Error) -> HttpError {
    HttpError::Network(e.to_string())
}

/// Drain `resp` into a `String`, refusing to buffer more than
/// [`MAX_RESPONSE_BYTES`]. Returns `(text, truncated)` so the caller
/// can tag a decode error as "body too large." Streaming chunk-by-
/// chunk means the worst case is one chunk over the limit, never
/// the full vendor payload.
async fn read_capped(resp: reqwest::Response) -> Result<(String, bool), HttpError> {
    let mut bytes = Vec::with_capacity(8 * 1024);
    let mut stream = resp.bytes_stream();
    let mut truncated = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| HttpError::Network(e.to_string()))?;
        if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
            // Take what fits; mark truncated. We don't error here so
            // a too-big *error* body still surfaces a useful snippet.
            let remaining = MAX_RESPONSE_BYTES.saturating_sub(bytes.len());
            bytes.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }
        bytes.extend_from_slice(&chunk);
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Ok((text, truncated))
}

fn classify_http_error(
    status: reqwest::StatusCode,
    headers: &HeaderMap,
    body: String,
) -> HttpError {
    let body = truncate(&body, ERROR_BODY_SNIPPET_BYTES);
    match status.as_u16() {
        404 => HttpError::NotFound,
        429 => HttpError::RateLimited {
            retry_after_secs: parse_retry_after(headers).unwrap_or(60),
        },
        507 => HttpError::CapacityExhausted {
            retry_after_secs: parse_retry_after(headers).unwrap_or(30),
        },
        other => HttpError::Vendor {
            status: other,
            body,
        },
    }
}

/// Parse `Retry-After` per RFC 7231 §7.1.3. Recall's docs only specify
/// the integer-seconds form, so we don't bother with HTTP-date parsing.
fn parse_retry_after(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
}

/// Truncate to a UTF-8 char boundary at or below `n`. Same pattern as
/// the spike harness — Recall errors are typically ASCII but we proxy
/// them as-is, so multi-byte safety matters.
fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut cut = n;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…(+{} bytes)", &s[..cut], s.len() - cut)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn truncate_ascii_under_cap_passes_through() {
        assert_eq!(truncate("hi", 100), "hi");
    }

    #[test]
    fn truncate_long_ascii_carries_overflow_size() {
        let long = "x".repeat(2000);
        let out = truncate(&long, 100);
        assert!(out.starts_with(&"x".repeat(100)));
        assert!(out.contains("(+1900 bytes)"));
    }

    #[test]
    fn truncate_snaps_to_utf8_boundary() {
        // 4-byte emoji at boundary — never panic.
        let s = "abc😀def";
        let out = truncate(s, 5);
        assert!(out.starts_with("abc") || out == s);
    }

    #[test]
    fn parse_retry_after_integer_seconds() {
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, HeaderValue::from_static("42"));
        assert_eq!(parse_retry_after(&h), Some(42));
    }

    #[test]
    fn parse_retry_after_missing_header_yields_none() {
        assert_eq!(parse_retry_after(&HeaderMap::new()), None);
    }

    #[test]
    fn parse_retry_after_garbage_yields_none() {
        // HTTP-date form ("Wed, 21 Oct 2015 ...") — not supported by
        // Recall's docs and not parsed; falls back to default.
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, HeaderValue::from_static("Wed, 21 Oct 2015"));
        assert_eq!(parse_retry_after(&h), None);
    }

    #[test]
    fn classify_404_is_not_found() {
        let err = classify_http_error(
            reqwest::StatusCode::NOT_FOUND,
            &HeaderMap::new(),
            "{}".into(),
        );
        assert!(matches!(err, HttpError::NotFound));
    }

    #[test]
    fn classify_429_uses_default_when_header_missing() {
        let err = classify_http_error(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            &HeaderMap::new(),
            "{}".into(),
        );
        assert!(matches!(
            err,
            HttpError::RateLimited {
                retry_after_secs: 60
            }
        ));
    }

    #[test]
    fn classify_507_uses_default_when_header_missing() {
        let err = classify_http_error(
            reqwest::StatusCode::INSUFFICIENT_STORAGE,
            &HeaderMap::new(),
            "{}".into(),
        );
        // Per spike-findings §"Recommendations" item 7: Recall
        // recommends polling every 30s for the warm-bot-pool case.
        assert!(matches!(
            err,
            HttpError::CapacityExhausted {
                retry_after_secs: 30
            }
        ));
    }

    #[test]
    fn classify_429_respects_retry_after_header() {
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, HeaderValue::from_static("17"));
        let err = classify_http_error(reqwest::StatusCode::TOO_MANY_REQUESTS, &h, "{}".into());
        assert!(matches!(
            err,
            HttpError::RateLimited {
                retry_after_secs: 17
            }
        ));
    }

    #[test]
    fn classify_500_is_vendor() {
        let err = classify_http_error(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            &HeaderMap::new(),
            "boom".into(),
        );
        match err {
            HttpError::Vendor { status, body } => {
                assert_eq!(status, 500);
                assert_eq!(body, "boom");
            }
            other => panic!("expected Vendor, got {other:?}"),
        }
    }
}
