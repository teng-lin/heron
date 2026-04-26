//! v2 escape hatch: thin HTTP client that delegates session-control
//! commands to the localhost `herond` daemon.
//!
//! Why this exists. Before this module the CLI ran the v1 in-process
//! [`crate::session::Orchestrator`] for every `record` / `status`
//! invocation, while the desktop shell drove the daemon via HTTP. That
//! left two session-control surfaces — and a CLI user could not poke
//! the same daemon the desktop app was driving. This client is the
//! bridge: load the bearer token `herond` writes at startup
//! (`~/.heron/cli-token` per [`herond::auth::default_token_path`]),
//! dial `http://127.0.0.1:7384/v1`, and forward to the OpenAPI surface.
//!
//! Scope. The client is deliberately narrow:
//!
//! - `POST /v1/meetings` — start a manual capture (escape hatch; the
//!   happy path is ambient detection).
//! - `POST /v1/meetings/{id}/end` — gracefully terminate a capture.
//! - `GET /v1/meetings` / `GET /v1/meetings/{id}` — list / fetch.
//! - `GET /v1/health` — liveness without bearer (the auth middleware
//!   allowlists health).
//! - `GET /v1/events` — SSE tail / replay against the orchestrator's
//!   event bus.
//!
//! What it does NOT do. v1 commands (`record`, `summarize`,
//! `salvage`, `synthesize`, `ax-dump`, `verify-m4a`, `status`) keep
//! their existing in-process behaviour. The two surfaces coexist
//! while v2 is still incomplete; eventually v1 deprecates, but the
//! migration is tracked elsewhere.
//!
//! Token discovery. `herond` mints + persists the bearer token at
//! `~/.heron/cli-token` with mode `0600` on first start. We read the
//! same path. If the file is missing, callers get a typed
//! [`DaemonError::TokenMissing`] that the CLI surfaces as a clear
//! "run onboarding (or `herond`) first" message — never a silent
//! 401.

use std::path::{Path, PathBuf};
use std::time::Duration;

use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use heron_session::{
    EventEnvelope, Health, ListMeetingsPage, Meeting, MeetingId, Platform, StartCaptureArgs,
};
use reqwest::Url;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue};
use serde::Serialize;
use thiserror::Error;

/// The default localhost base — pinned by the OpenAPI
/// `servers[0].url`. Mirrors [`herond::DEFAULT_BIND`] but as the full
/// URL the daemon serves under, including the `/v1` prefix.
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:7384/v1";

/// Default per-request timeout. Matches Recall's per-request budget
/// in [`heron_bot::recall::client`]: 30s is generous enough for a
/// long `start_capture` (which currently spins through the FSM in
/// memory but, after the orchestration PR, may negotiate a real
/// bot/bridge handshake) without being so long that a wedged daemon
/// hangs interactive shells indefinitely.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Default location of the bearer-token file `herond` writes at
/// startup. Mirror of [`herond::auth::default_token_path`] — kept as
/// a tiny shim so this crate doesn't need to depend on `herond` (the
/// dependency direction is CLI → daemon, never the reverse: a future
/// `herond` refactor must not pull `heron-cli` into its build graph).
pub fn default_token_path() -> Result<PathBuf, DaemonError> {
    let mut path = dirs::home_dir().ok_or(DaemonError::NoHome)?;
    path.push(".heron");
    path.push("cli-token");
    Ok(path)
}

/// Configuration for [`DaemonClient`].
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Bearer token. Loaded from disk via [`load_bearer`] — never
    /// from an env var, since stuffing a long-lived secret into the
    /// shell environment is a foot-gun (`ps -ef` exposes it on
    /// poorly-configured systems).
    pub bearer: String,
    /// Base URL including `/v1`. Tests inject the `MockServer`'s URI
    /// here.
    pub base_url: String,
    /// Per-request timeout for non-streaming endpoints. The SSE
    /// stream installs its own (no timeout) connection; only the
    /// initial response-header phase honours this value.
    pub timeout: Duration,
}

/// Load the bearer token from `path`, trimming a trailing newline so
/// `printf` / `echo` round-trips don't shift the comparison `herond`
/// does. Returns [`DaemonError::TokenMissing`] when the file is
/// absent — the CLI converts that to an actionable error rather than
/// surfacing a 401 the user can't act on.
pub fn load_bearer(path: &Path) -> Result<String, DaemonError> {
    match std::fs::read_to_string(path) {
        Ok(raw) => {
            let trimmed = raw.trim().to_owned();
            if trimmed.is_empty() {
                return Err(DaemonError::TokenEmpty {
                    path: path.to_path_buf(),
                });
            }
            Ok(trimmed)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(DaemonError::TokenMissing {
            path: path.to_path_buf(),
        }),
        Err(err) => Err(DaemonError::TokenIo {
            path: path.to_path_buf(),
            source: err,
        }),
    }
}

/// Errors surfaced by the daemon client. Carry enough context that
/// the CLI's `eprintln!` can produce an actionable message — wire
/// errors echo the daemon's `code` / `message` so a `HERON_E_*` code
/// reaches the user without parsing the stack-traced inner.
#[derive(Debug, Error)]
pub enum DaemonError {
    #[error("home directory not resolvable")]
    NoHome,

    #[error(
        "bearer token not found at {path}; start the daemon (`herond`) once \
         to mint it, or finish onboarding in the desktop app"
    )]
    TokenMissing { path: PathBuf },

    #[error(
        "bearer token at {path} is empty; delete the file and restart `herond` to mint a fresh one"
    )]
    TokenEmpty { path: PathBuf },

    #[error("reading bearer token at {path}: {source}")]
    TokenIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid base URL {url}: {detail}")]
    InvalidBaseUrl { url: String, detail: String },

    #[error("building HTTP client: {0}")]
    Build(String),

    #[error(
        "daemon unreachable at {base_url}; is `herond` running on localhost? \
         underlying: {source}"
    )]
    Unreachable {
        base_url: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("HTTP error from daemon: {0}")]
    Http(#[from] reqwest::Error),

    #[error("decoding response body: {0}")]
    Decode(String),

    /// A non-2xx response carrying a `HERON_E_*` envelope (or, for a
    /// non-conforming response, a fallback shape). The CLI prints
    /// `code`, `message`, and `status` so the user gets exactly the
    /// signal `herond` raised — no double-mapping through a fresh
    /// taxonomy.
    #[error("daemon returned {status} ({code}): {message}")]
    Api {
        status: u16,
        code: String,
        message: String,
        details: serde_json::Value,
    },

    #[error("SSE stream error: {0}")]
    Sse(String),
}

/// Wire shape of the error envelope `herond::error::WireError`
/// emits. Subset — we only deserialize the fields the CLI surfaces
/// to the user. `details` is `serde_json::Value` so a future variant
/// adding a structured field doesn't force a client-side schema bump.
#[derive(Debug, serde::Deserialize)]
struct WireErrorBody {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(rename = "statusCode", default)]
    status_code: Option<u16>,
    #[serde(default)]
    details: serde_json::Value,
}

/// Thin wrapper around `reqwest::Client` pinned to the daemon's base
/// URL + bearer. Cheap to clone (`reqwest::Client` is `Arc`-internal).
#[derive(Clone)]
pub struct DaemonClient {
    http: reqwest::Client,
    /// Pre-parsed so we can join paths without re-parsing per request.
    base_url: Url,
    bearer: String,
}

/// Body shape for `POST /v1/meetings`. Mirrors
/// [`herond::routes::meetings::StartCaptureBody`].
#[derive(Debug, Serialize)]
struct StartCaptureBody<'a> {
    platform: Platform,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<&'a str>,
}

impl DaemonClient {
    /// Build a client from explicit config. Tests use this with the
    /// wiremock `MockServer::uri()`; production callers go through
    /// [`Self::from_default_token`].
    pub fn new(config: ClientConfig) -> Result<Self, DaemonError> {
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|e| DaemonError::Build(e.to_string()))?;
        // Trim a trailing slash so `join` semantics are predictable —
        // `Url::join` against a base ending in `/v1/` strips the path
        // segment when the joined ref starts with `/`.
        let normalised = config.base_url.trim_end_matches('/');
        // We append `/` deliberately so `join("meetings")` produces
        // `…/v1/meetings` instead of stomping on the last segment.
        let with_trailing = format!("{normalised}/");
        let base_url = Url::parse(&with_trailing).map_err(|e| DaemonError::InvalidBaseUrl {
            url: config.base_url.clone(),
            detail: e.to_string(),
        })?;
        Ok(Self {
            http,
            base_url,
            bearer: config.bearer,
        })
    }

    /// Convenience: read the bearer token from
    /// [`default_token_path`] and dial [`DEFAULT_BASE_URL`].
    pub fn from_default_token() -> Result<Self, DaemonError> {
        let path = default_token_path()?;
        let bearer = load_bearer(&path)?;
        Self::new(ClientConfig {
            bearer,
            base_url: DEFAULT_BASE_URL.to_owned(),
            timeout: DEFAULT_TIMEOUT,
        })
    }

    fn url(&self, path: &str) -> Result<Url, DaemonError> {
        // `path` is always a literal we control (no user-supplied
        // segments before `meetings/{id}` — and the meeting id is
        // already a typed UUID by the time it reaches here).
        self.base_url
            .join(path.trim_start_matches('/'))
            .map_err(|e| DaemonError::InvalidBaseUrl {
                url: self.base_url.to_string(),
                detail: e.to_string(),
            })
    }

    fn auth_headers(&self) -> Result<HeaderMap, DaemonError> {
        let mut headers = HeaderMap::new();
        // The daemon's auth middleware accepts `Bearer` case-
        // insensitive (`bearer`, `BEARER`); we send the canonical
        // capitalization so logs / replays match byte-for-byte.
        let value = HeaderValue::from_str(&format!("Bearer {}", self.bearer))
            .map_err(|e| DaemonError::Build(format!("bearer header: {e}")))?;
        headers.insert(AUTHORIZATION, value);
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        Ok(headers)
    }

    /// `GET /v1/health`. No bearer required by spec; we send one
    /// anyway because the daemon ignores it on the allowlisted path
    /// and a single code path is easier to maintain than two.
    pub async fn health(&self) -> Result<Health, DaemonError> {
        let url = self.url("health")?;
        let resp = self
            .http
            .get(url)
            .headers(self.auth_headers()?)
            .send()
            .await
            .map_err(|e| self.classify_transport(e))?;
        decode_or_error(resp).await
    }

    /// `POST /v1/meetings` — manual capture escape hatch. Returns the
    /// freshly-created [`Meeting`] resource the daemon synthesizes.
    pub async fn start_capture(&self, args: StartCaptureArgs) -> Result<Meeting, DaemonError> {
        let url = self.url("meetings")?;
        let body = StartCaptureBody {
            platform: args.platform,
            hint: args.hint.as_deref(),
        };
        let resp = self
            .http
            .post(url)
            .headers(self.auth_headers()?)
            .json(&body)
            .send()
            .await
            .map_err(|e| self.classify_transport(e))?;
        // The OpenAPI returns 202 (Accepted) for `start_capture`. We
        // accept any 2xx so a future move to 200 doesn't break us.
        // The `Location` header is informational; the body carries
        // the same id so callers don't need it.
        decode_or_error(resp).await
    }

    /// `POST /v1/meetings/{id}/end`. Returns `()` on success — the
    /// daemon emits `204 No Content` when the FSM transition is
    /// accepted; the eventual lifecycle changes ride on `/events`.
    pub async fn end_meeting(&self, meeting_id: &MeetingId) -> Result<(), DaemonError> {
        let url = self.url(&format!("meetings/{meeting_id}/end"))?;
        let resp = self
            .http
            .post(url)
            .headers(self.auth_headers()?)
            .send()
            .await
            .map_err(|e| self.classify_transport(e))?;
        if !resp.status().is_success() {
            return Err(api_error(resp).await);
        }
        Ok(())
    }

    /// `GET /v1/meetings`.
    pub async fn list_meetings(
        &self,
        platform: Option<Platform>,
        limit: Option<u32>,
    ) -> Result<ListMeetingsPage, DaemonError> {
        let mut url = self.url("meetings")?;
        {
            let mut q = url.query_pairs_mut();
            if let Some(p) = platform {
                // Explicit match rather than a serde round-trip: a
                // future `Platform` variant becomes a compile error
                // here (which is what we want for a wire-format
                // match — silently sending the wrong filter is the
                // exact misroute typed enums are supposed to
                // prevent).
                let s = match p {
                    Platform::Zoom => "zoom",
                    Platform::GoogleMeet => "google_meet",
                    Platform::MicrosoftTeams => "microsoft_teams",
                    Platform::Webex => "webex",
                };
                q.append_pair("platform", s);
            }
            if let Some(n) = limit {
                q.append_pair("limit", &n.to_string());
            }
        }
        let resp = self
            .http
            .get(url)
            .headers(self.auth_headers()?)
            .send()
            .await
            .map_err(|e| self.classify_transport(e))?;
        decode_or_error(resp).await
    }

    /// `GET /v1/meetings/{id}`.
    pub async fn get_meeting(&self, meeting_id: &MeetingId) -> Result<Meeting, DaemonError> {
        let url = self.url(&format!("meetings/{meeting_id}"))?;
        let resp = self
            .http
            .get(url)
            .headers(self.auth_headers()?)
            .send()
            .await
            .map_err(|e| self.classify_transport(e))?;
        decode_or_error(resp).await
    }

    /// `GET /v1/events` — SSE projection of the orchestrator event
    /// bus. Returns a stream of typed [`EventEnvelope`]s. The caller
    /// owns the loop; this method drops the connection only when the
    /// returned stream is dropped.
    ///
    /// `since_event_id` corresponds to the spec's `?since_event_id`
    /// resume hint; pass `None` for a fresh tail.
    pub async fn events(&self, since_event_id: Option<&str>) -> Result<EventStream, DaemonError> {
        let mut url = self.url("events")?;
        if let Some(since) = since_event_id {
            url.query_pairs_mut().append_pair("since_event_id", since);
        }
        // `Accept: text/event-stream` per RFC 8895 / SSE convention.
        // The daemon serves SSE regardless, but a correct Accept
        // matters for a future content-negotiated alternative
        // projection (e.g. NDJSON).
        let mut headers = self.auth_headers()?;
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        // SSE streams are long-lived; reqwest's per-client request
        // timeout would otherwise tear the tail down at the 30s
        // mark. Override with a per-request timeout long enough to
        // exceed any legitimate SSE silence (the daemon emits a
        // 15s heartbeat by spec; we give ~10x headroom). A future
        // refinement could use `read_timeout` if reqwest exposes it
        // on a per-request builder.
        let resp = self
            .http
            .get(url)
            .headers(headers)
            .timeout(Duration::from_secs(60 * 60 * 24))
            .send()
            .await
            .map_err(|e| self.classify_transport(e))?;
        if !resp.status().is_success() {
            return Err(api_error(resp).await);
        }
        // Box the bytes stream so the public `EventStream` type
        // doesn't propagate reqwest's internal stream type. The box
        // is one extra heap indirection per chunk — an SSE stream
        // emits at most ~1 frame/sec under steady state, so the cost
        // is in the noise compared to the network read itself.
        let bytes: BoxedBytesStream = Box::new(resp.bytes_stream());
        Ok(EventStream {
            inner: bytes.eventsource(),
        })
    }

    /// Map a low-level transport error into the actionable
    /// "daemon unreachable" message when the cause looks like a
    /// connection refusal. Only `is_connect()` is treated as
    /// unreachable: a `is_timeout()` can fire after a successful
    /// TCP connect (the daemon accepted the socket but is wedged
    /// mid-handshake or in the middle of a long `start_capture`),
    /// and surfacing that as "is `herond` running?" would mislead
    /// the user. The non-connect timeout path falls through to
    /// `Http(_)` so the original `reqwest` error message reaches
    /// the CLI.
    fn classify_transport(&self, err: reqwest::Error) -> DaemonError {
        if err.is_connect() {
            DaemonError::Unreachable {
                base_url: self.base_url.to_string(),
                source: err,
            }
        } else {
            DaemonError::Http(err)
        }
    }
}

/// Type alias for the boxed bytes stream feeding the SSE parser. The
/// box keeps the public [`EventStream`] type from leaking reqwest's
/// internal stream type into our API.
type BoxedBytesStream =
    Box<dyn futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send + Unpin>;

/// Server-Sent Events stream. Each `next` yields a typed
/// [`EventEnvelope`] or a [`DaemonError::Sse`] for a malformed frame.
/// Callers drive a `while let Some(evt) = stream.next().await` loop.
pub struct EventStream {
    inner: eventsource_stream::EventStream<BoxedBytesStream>,
}

impl EventStream {
    /// Yield the next event envelope. `Ok(None)` means the server
    /// closed the stream (and the caller should reconnect with the
    /// last seen `event_id` if it cares about resuming).
    pub async fn next(&mut self) -> Option<Result<EventEnvelope, DaemonError>> {
        let msg = self.inner.next().await?;
        match msg {
            Ok(event) => match serde_json::from_str::<EventEnvelope>(&event.data) {
                Ok(env) => Some(Ok(env)),
                Err(e) => Some(Err(DaemonError::Decode(format!(
                    "envelope: {e} (data: {})",
                    snippet(&event.data, 256)
                )))),
            },
            Err(e) => Some(Err(DaemonError::Sse(e.to_string()))),
        }
    }
}

/// Cap on the size of any response body the daemon client will
/// deserialize. A typical `Meeting` envelope is a few KB; a paged
/// `ListMeetingsPage` for a long-running vault is a few hundred KB
/// at most. 4 MiB is generous headroom while still preventing a
/// runaway / hostile response from forcing an unbounded allocation.
/// Mirrors the equivalent cap in [`heron_bot::recall::client`].
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// Drain a `reqwest::Response` into bytes, capping the stream at
/// [`MAX_RESPONSE_BYTES`] so a hostile/runaway daemon can't OOM the
/// CLI. Streaming into a `Vec` chunk-by-chunk lets us short-circuit
/// before allocating the whole thing — a cheaper failure mode than
/// `resp.bytes()` (which buffers internally without bound when no
/// `Content-Length` is set) or `resp.text()`.
async fn drain_capped(resp: reqwest::Response) -> Result<Vec<u8>, DaemonError> {
    let mut stream = resp.bytes_stream();
    let mut buf = Vec::with_capacity(8 * 1024);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(DaemonError::Http)?;
        if buf.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(DaemonError::Decode(format!(
                "response body exceeds {MAX_RESPONSE_BYTES}-byte cap"
            )));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

async fn decode_or_error<T: for<'de> serde::Deserialize<'de>>(
    resp: reqwest::Response,
) -> Result<T, DaemonError> {
    if !resp.status().is_success() {
        return Err(api_error(resp).await);
    }
    let body = drain_capped(resp).await?;
    serde_json::from_slice::<T>(&body).map_err(|e| DaemonError::Decode(e.to_string()))
}

async fn api_error(resp: reqwest::Response) -> DaemonError {
    let status = resp.status().as_u16();
    let raw = drain_capped(resp).await.unwrap_or_default();
    let body = String::from_utf8_lossy(&raw);
    match serde_json::from_slice::<WireErrorBody>(&raw) {
        Ok(envelope) => DaemonError::Api {
            status: envelope.status_code.unwrap_or(status),
            code: envelope
                .code
                .unwrap_or_else(|| "HERON_E_UNKNOWN".to_owned()),
            message: envelope
                .message
                .or(envelope.error)
                .unwrap_or_else(|| snippet(body.as_ref(), 256)),
            details: envelope.details,
        },
        Err(_) => DaemonError::Api {
            status,
            code: status_to_code(status).to_owned(),
            message: snippet(body.as_ref(), 256),
            details: serde_json::Value::Null,
        },
    }
}

fn status_to_code(status: u16) -> &'static str {
    // Plain numeric literals — `StatusCode::FOO.as_u16()` isn't a
    // pattern-legal expression in stable Rust, and the codes here
    // are the OpenAPI-pinned subset we expect to surface from the
    // daemon. The HTTP status numbers themselves are stable across
    // RFC revisions.
    match status {
        401 => "HERON_E_UNAUTHORIZED",
        403 => "HERON_E_ORIGIN_DENIED",
        404 => "HERON_E_NOT_FOUND",
        409 => "HERON_E_INVALID_STATE",
        501 => "HERON_E_NOT_YET_IMPLEMENTED",
        _ => "HERON_E_UNKNOWN",
    }
}

fn snippet(s: &str, max: usize) -> String {
    // Single pass: `char_indices().nth(max)` walks until the (max)-
    // th char boundary, returning `None` if the string is shorter
    // and the byte index of the truncation point otherwise. Slicing
    // at a `char_indices` boundary never splits a UTF-8 grapheme.
    match s.char_indices().nth(max) {
        None => s.to_owned(),
        Some((byte_idx, _)) => {
            let mut out = String::with_capacity(byte_idx + '…'.len_utf8());
            out.push_str(&s[..byte_idx]);
            out.push('…');
            out
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn load_bearer_trims_newline() {
        let dir =
            std::env::temp_dir().join(format!("heron-cli-daemon-token-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cli-token");
        std::fs::write(&path, "abc\n").unwrap();
        let bearer = load_bearer(&path).unwrap();
        assert_eq!(bearer, "abc");
    }

    #[test]
    fn load_bearer_missing_file_is_typed_error() {
        let dir = std::env::temp_dir().join(format!(
            "heron-cli-daemon-token-missing-{}",
            uuid::Uuid::now_v7()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("does-not-exist");
        let err = load_bearer(&path).unwrap_err();
        assert!(
            matches!(err, DaemonError::TokenMissing { .. }),
            "expected TokenMissing, got {err:?}"
        );
    }

    #[test]
    fn load_bearer_empty_file_is_typed_error() {
        let dir = std::env::temp_dir().join(format!(
            "heron-cli-daemon-token-empty-{}",
            uuid::Uuid::now_v7()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cli-token");
        std::fs::write(&path, "  \n").unwrap();
        let err = load_bearer(&path).unwrap_err();
        assert!(
            matches!(err, DaemonError::TokenEmpty { .. }),
            "expected TokenEmpty, got {err:?}"
        );
    }

    #[test]
    fn snippet_handles_multi_byte_utf8() {
        // Long enough to exceed the cap, with multibyte chars.
        let s = "日本語の長い文字列".repeat(50);
        let snip = snippet(&s, 16);
        // Just check we don't panic + result has the ellipsis suffix.
        assert!(snip.ends_with("…"));
    }
}
