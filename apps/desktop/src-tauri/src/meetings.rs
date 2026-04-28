//! Tauri-side proxy for the daemon's meetings + summary endpoints.
//!
//! Browser-level [`fetch`] / [`EventSource`] cannot talk to the daemon
//! directly: bearer auth is required on every route except `/health`,
//! the daemon rejects requests carrying an `Origin` header (a
//! webview-originated request always sets one), and the Tauri CSP
//! does not declare `connect-src` for `127.0.0.1:7384`. Routing the
//! call through Rust sidesteps all three.
//!
//! The commands here take the bearer from
//! [`crate::daemon::DaemonHandle::auth`] and surface a structured
//! [`DaemonOutcome`] so the React side can switch into a degraded UI
//! on transport failure without parsing error strings. This is the
//! pattern the rest of the UI revamp's daemon-talking commands
//! (`heron_subscribe_events` in PR 4, etc.) will follow.
//!
//! ## URL policy
//!
//! Requests target a hardcoded `http://127.0.0.1:7384/v1/...`. We do
//! NOT take a renderer-supplied base URL — that would widen the
//! "Tauri command makes outbound HTTP" surface to anything an
//! attacker-controlled webview could fabricate, same reasoning as
//! [`crate::daemon::HEALTH_URL`]. Tests drive the parameterized
//! [`list_meetings_at`] / [`get_summary_at`] helpers against an
//! ephemeral-port axum server.
//!
//! ## Timeout
//!
//! 5 s per request. Long enough to outlast a slow vault scan
//! (`LocalSessionOrchestrator::list_meetings` reads meeting notes from
//! disk), short enough that a wedged daemon doesn't make the meetings
//! table feel hung. Same order of magnitude the daemon's own ingress
//! timeouts would use; if either side ever needs longer, change the
//! constant in lockstep.
//!
//! ## Error taxonomy
//!
//! - Connect refused / connection error / timeout → [`DaemonOutcome::Unavailable`]:
//!   the daemon isn't reachable; the React tree shows the daemon-down
//!   banner. Settings/Salvage routes keep working.
//! - Non-2xx HTTP status (401 from a stale bearer, 404 from a missing
//!   meeting, 500 from the daemon) → also [`DaemonOutcome::Unavailable`]
//!   with the status code in `detail`. The frontend treats 4xx and 5xx
//!   identically: degraded UI plus retry button. v1 doesn't distinguish
//!   "server bug" from "your auth rotated"; if that becomes important
//!   we add a `Forbidden` variant.
//! - 200 OK with unparseable body → [`DaemonOutcome::Unavailable`] with
//!   the parse error in `detail`. Drift between the TS shape mirror
//!   in `lib/types.ts` and the Rust serde shape lands here.

use std::str::FromStr;
use std::time::Duration;

use heron_session::{
    ListMeetingsPage, ListMeetingsQuery, Meeting, MeetingId, MeetingStatus, Platform, Summary,
};
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::daemon::DaemonHandle;

/// Loopback URL for the daemon API. Same pattern as
/// [`crate::daemon::HEALTH_URL`]; see module docs for why this isn't
/// renderer-supplied.
const BASE_URL: &str = "http://127.0.0.1:7384/v1";

/// Per-request timeout. See module docs for the choice of 5 s.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Outcome of a daemon-talking command. Mirrors the TS
/// `DaemonResult<T>` discriminated union in
/// `apps/desktop/src/lib/types.ts`.
///
/// Tagged with `kind` because the frontend Zustand store branches on
/// the variant directly; serde's default `externally tagged` form
/// would emit `{ "Ok": ..., "Unavailable": ... }` (capitalized,
/// non-discriminator-shaped) and force a wrapper layer.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum DaemonOutcome<T: Serialize> {
    Ok { data: T },
    Unavailable { detail: String },
}

/// Tauri command: list meetings. Proxies `GET /v1/meetings`.
///
/// Query params come straight from
/// `apps/desktop/src/lib/types.ts::ListMeetingsQuery`. All fields
/// optional; missing fields fall through to the daemon's own defaults
/// (newest-first, no `since` cursor, limit ≤ 200 enforced server-side
/// per `crates/heron-orchestrator/src/lib.rs:1206`).
#[tauri::command]
pub async fn heron_list_meetings(
    state: State<'_, DaemonHandle>,
    query: TsListMeetingsQuery,
) -> Result<DaemonOutcome<ListMeetingsPage>, String> {
    let bearer = state.auth.bearer.clone();
    let parsed = match ListMeetingsQuery::try_from(query) {
        Ok(q) => q,
        Err(detail) => return Ok(DaemonOutcome::Unavailable { detail }),
    };
    Ok(list_meetings_at(BASE_URL, &bearer, parsed).await)
}

/// Tauri command: fetch the summary for a meeting. Proxies
/// `GET /v1/meetings/{id}/summary`.
///
/// The renderer-supplied `meeting_id` is validated with
/// [`MeetingId::from_str`] before it touches the URL. This is
/// defence-in-depth: the daemon also rejects malformed IDs (the
/// `MeetingId` extractor on the route enforces the same invariant),
/// but a stricter Tauri-side check means an attacker-controlled
/// webview can't probe the daemon's URL space — `meeting_id =
/// "../foo"` is shaped like a valid string at the Tauri boundary
/// today, and we don't want this command to be the weak link if the
/// daemon ever loses validation.
#[tauri::command]
pub async fn heron_meeting_summary(
    state: State<'_, DaemonHandle>,
    meeting_id: String,
) -> Result<DaemonOutcome<Summary>, String> {
    let parsed = match MeetingId::from_str(&meeting_id) {
        Ok(id) => id,
        Err(e) => {
            return Ok(DaemonOutcome::Unavailable {
                detail: format!("invalid meeting_id: {e}"),
            });
        }
    };
    let bearer = state.auth.bearer.clone();
    Ok(get_summary_at(BASE_URL, &bearer, &parsed.to_string()).await)
}

/// TS-side body mirror for the "start a capture" call. The frontend
/// passes `{ platform, hint? }`; `calendar_event_id` is daemon-only
/// (set when a calendar correlation triggered the capture, not for
/// the manual Home-page button), so it's not exposed here.
///
/// Length-bound on `hint`: the daemon doesn't currently enforce one,
/// but a renderer-controlled string flowing into the meeting title is
/// a vector for log-injection / display-corruption if it ever grows
/// unbounded. 256 chars matches the practical window-title cap on
/// macOS and is well above any human-typed hint. Strings longer than
/// that are truncated; we don't fail the request because the user
/// might be pasting a long calendar-event title.
#[derive(Debug, Deserialize)]
pub struct TsStartCaptureBody {
    pub platform: Platform,
    pub hint: Option<String>,
}

/// Tauri command: start a capture session. Proxies `POST /v1/meetings`.
///
/// Returns the [`Meeting`] envelope the daemon emits (status =
/// `recording`, fresh `id`, `started_at` set). The frontend stores
/// `id` so [`heron_end_meeting`] can target the same session.
///
/// On success the daemon also publishes `meeting.detected` →
/// `meeting.armed` → `meeting.started` envelopes onto the SSE bus
/// (`heron-orchestrator/src/lib.rs:1294-1312`); the frontend's
/// `useTranscriptStore` already subscribes through
/// `events_bridge::heron_subscribe_events`, so the `/recording` page
/// hydrates from those events without an explicit poll.
///
/// Conflict (`HERON_E_CAPTURE_IN_PROGRESS`) maps to
/// [`DaemonOutcome::Unavailable`] with the daemon's status code in
/// `detail` — the frontend renders the same daemon-down banner. v1
/// doesn't surface a separate "another platform is already recording"
/// affordance; if that becomes important we add a `Conflict` variant.
#[tauri::command]
pub async fn heron_start_capture(
    state: State<'_, DaemonHandle>,
    body: TsStartCaptureBody,
) -> Result<DaemonOutcome<Meeting>, String> {
    let bearer = state.auth.bearer.clone();
    let hint = body.hint.map(|h| h.chars().take(256).collect::<String>());
    Ok(start_capture_at(BASE_URL, &bearer, body.platform, hint).await)
}

/// Tauri command: end a capture session. Proxies
/// `POST /v1/meetings/{id}/end`.
///
/// The renderer-supplied `meeting_id` is validated with
/// [`MeetingId::from_str`] before it touches the URL — same
/// defence-in-depth rationale as [`heron_meeting_summary`]. Returns
/// `DaemonOutcome<()>`; the daemon emits 204 on success.
#[tauri::command]
pub async fn heron_end_meeting(
    state: State<'_, DaemonHandle>,
    meeting_id: String,
) -> Result<DaemonOutcome<()>, String> {
    let parsed = match MeetingId::from_str(&meeting_id) {
        Ok(id) => id,
        Err(e) => {
            return Ok(DaemonOutcome::Unavailable {
                detail: format!("invalid meeting_id: {e}"),
            });
        }
    };
    let bearer = state.auth.bearer.clone();
    Ok(end_meeting_at(BASE_URL, &bearer, &parsed.to_string()).await)
}

/// TS-side query mirror. Tauri's serde plumbing renames camelCase
/// argument keys (the JS side passes `{ query: { status, limit } }`)
/// into snake_case, so this struct accepts the exact TS shape.
/// We don't reuse `heron_session::ListMeetingsQuery` directly because
/// it's not `Deserialize` (defined as a plain `Default` struct for
/// orchestrator inputs).
#[derive(Debug, Default, serde::Deserialize)]
pub struct TsListMeetingsQuery {
    pub since: Option<String>,
    pub status: Option<MeetingStatus>,
    pub platform: Option<Platform>,
    pub limit: Option<u32>,
    pub cursor: Option<String>,
}

impl TryFrom<TsListMeetingsQuery> for ListMeetingsQuery {
    type Error = String;

    /// Convert the TS query mirror into the daemon's
    /// [`ListMeetingsQuery`]. Fallible on `since`: a malformed
    /// RFC3339 string surfaces as a parse error rather than silently
    /// dropping the filter and widening the request to "everything".
    /// The frontend renders the resulting `Unavailable` outcome via
    /// the daemon-down banner.
    fn try_from(q: TsListMeetingsQuery) -> Result<Self, Self::Error> {
        let since = match q.since {
            Some(s) => Some(
                chrono::DateTime::parse_from_rfc3339(&s)
                    .map(|dt| dt.with_timezone(&chrono::Utc))
                    .map_err(|e| format!("invalid `since` (RFC3339 expected): {e}"))?,
            ),
            None => None,
        };
        Ok(ListMeetingsQuery {
            since,
            status: q.status,
            platform: q.platform,
            limit: q.limit,
            cursor: q.cursor,
        })
    }
}

/// Map a [`MeetingStatus`] to its lowercase wire form. Hand-rolled
/// rather than going through `serde_json::to_value(...)` to keep the
/// hot path off serde's general-purpose value tree — these strings
/// flow into URL query params on every meetings list call.
fn status_str(s: MeetingStatus) -> &'static str {
    match s {
        MeetingStatus::Detected => "detected",
        MeetingStatus::Armed => "armed",
        MeetingStatus::Recording => "recording",
        MeetingStatus::Ended => "ended",
        MeetingStatus::Done => "done",
        MeetingStatus::Failed => "failed",
    }
}

/// Map a [`Platform`] to its lowercase wire form. Same rationale as
/// [`status_str`].
fn platform_str(p: Platform) -> &'static str {
    match p {
        Platform::Zoom => "zoom",
        Platform::GoogleMeet => "google_meet",
        Platform::MicrosoftTeams => "microsoft_teams",
        Platform::Webex => "webex",
    }
}

/// Build a reqwest client wired to [`REQUEST_TIMEOUT`]. The four
/// proxy entry points all need the same configuration; pulling the
/// builder behind one call site keeps the timeout / TLS knobs in
/// one place if either ever needs changing.
fn build_client<T>() -> Result<reqwest::Client, DaemonOutcome<T>>
where
    T: Serialize,
{
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| DaemonOutcome::Unavailable {
            detail: format!("client build: {e}"),
        })
}

/// Map a `reqwest` send error to [`DaemonOutcome::Unavailable`]. The
/// four proxy entry points all surface transport failures with the
/// same shape; this keeps the call sites tight.
fn transport_error<T>(e: reqwest::Error) -> DaemonOutcome<T>
where
    T: Serialize,
{
    DaemonOutcome::Unavailable {
        detail: e.to_string(),
    }
}

/// Parameterized list — split out so unit tests can drive it against
/// an ephemeral-port axum server.
pub async fn list_meetings_at(
    base_url: &str,
    bearer: &str,
    query: ListMeetingsQuery,
) -> DaemonOutcome<ListMeetingsPage> {
    let client = match build_client() {
        Ok(c) => c,
        Err(outcome) => return outcome,
    };
    let mut request = client
        .get(format!("{base_url}/meetings"))
        .bearer_auth(bearer);
    let mut params: Vec<(&str, String)> = Vec::new();
    if let Some(s) = query.since {
        params.push(("since", s.to_rfc3339()));
    }
    if let Some(s) = query.status {
        params.push(("status", status_str(s).to_owned()));
    }
    if let Some(p) = query.platform {
        params.push(("platform", platform_str(p).to_owned()));
    }
    if let Some(l) = query.limit {
        params.push(("limit", l.to_string()));
    }
    if let Some(c) = query.cursor {
        params.push(("cursor", c));
    }
    if !params.is_empty() {
        request = request.query(&params);
    }
    match request.send().await {
        Ok(resp) => parse_response(resp).await,
        Err(e) => transport_error(e),
    }
}

/// Parameterized summary fetch. Same split-out rationale as
/// [`list_meetings_at`]. The meeting ID is path-safe by construction
/// (Stripe-style prefixed UUID — `[a-z0-9_-]` only — verified by the
/// `MeetingId` `FromStr` impl daemon-side), so no percent-encoding is
/// required for the path segment.
pub async fn get_summary_at(
    base_url: &str,
    bearer: &str,
    meeting_id: &str,
) -> DaemonOutcome<Summary> {
    let client = match build_client() {
        Ok(c) => c,
        Err(outcome) => return outcome,
    };
    let url = format!("{base_url}/meetings/{meeting_id}/summary");
    match client.get(url).bearer_auth(bearer).send().await {
        Ok(resp) => parse_response(resp).await,
        Err(e) => transport_error(e),
    }
}

async fn parse_response<T>(resp: reqwest::Response) -> DaemonOutcome<T>
where
    T: serde::de::DeserializeOwned + Serialize,
{
    let status = resp.status();
    if !status.is_success() {
        return DaemonOutcome::Unavailable {
            detail: format!("daemon returned {status}"),
        };
    }
    match resp.json::<T>().await {
        Ok(data) => DaemonOutcome::Ok { data },
        Err(e) => DaemonOutcome::Unavailable {
            detail: format!("response body parse: {e}"),
        },
    }
}

/// Parameterized start. Posts the start-capture body the daemon's
/// `StartCaptureBody` extractor expects (`platform`, `hint`,
/// `calendar_event_id`); the renderer doesn't pass `calendar_event_id`
/// today (the home button is the manual path), so it's always `None`.
/// The daemon responds with 202 + a `Meeting` JSON body, which we
/// hand back via [`parse_response`] — `is_success()` covers 200..=299
/// so 202 deserializes the same as 200.
pub async fn start_capture_at(
    base_url: &str,
    bearer: &str,
    platform: Platform,
    hint: Option<String>,
) -> DaemonOutcome<Meeting> {
    let client = match build_client() {
        Ok(c) => c,
        Err(outcome) => return outcome,
    };
    // The daemon's `StartCaptureBody` derives `Deserialize` directly,
    // so we serialize a JSON object whose field names match. Wrapping
    // in a small private struct (rather than `serde_json::json!`)
    // keeps the wire shape compile-checked: a future field rename on
    // the daemon's side will land here as a typed error rather than
    // a runtime "missing field" 4xx.
    #[derive(Serialize)]
    struct Body<'a> {
        platform: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        hint: Option<&'a str>,
    }
    let body = Body {
        platform: platform_str(platform),
        hint: hint.as_deref(),
    };
    let url = format!("{base_url}/meetings");
    match client
        .post(url)
        .bearer_auth(bearer)
        .json(&body)
        .send()
        .await
    {
        Ok(resp) => parse_response(resp).await,
        Err(e) => transport_error(e),
    }
}

/// Parameterized end. The daemon emits 204 No Content on success —
/// no JSON body to parse — so we short-circuit [`parse_response`] and
/// return [`DaemonOutcome::Ok`] with `()`. 4xx/5xx (404 unknown id,
/// 409 already-ended) maps to [`DaemonOutcome::Unavailable`] with the
/// status code in `detail`, same shape as [`list_meetings_at`].
pub async fn end_meeting_at(base_url: &str, bearer: &str, meeting_id: &str) -> DaemonOutcome<()> {
    let client = match build_client() {
        Ok(c) => c,
        Err(outcome) => return outcome,
    };
    let url = format!("{base_url}/meetings/{meeting_id}/end");
    match client.post(url).bearer_auth(bearer).send().await {
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() {
                DaemonOutcome::Ok { data: () }
            } else {
                DaemonOutcome::Unavailable {
                    detail: format!("daemon returned {status}"),
                }
            }
        }
        Err(e) => transport_error(e),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
#[allow(clippy::unwrap_used)]
mod tests {
    //! Drive [`list_meetings_at`] / [`get_summary_at`] against an
    //! ad-hoc axum server. We don't reuse the StubOrchestrator-backed
    //! `build_app` from `daemon.rs::tests` because that orchestrator's
    //! `list_meetings` / `read_summary` always return
    //! `NotYetImplemented` (501), which proves nothing about our
    //! happy-path parsing. A purpose-built handler returns hand-rolled
    //! `ListMeetingsPage` / `Summary` JSON.
    use super::*;
    use axum::{
        Json, Router, extract::Path, extract::Query, http::StatusCode, middleware,
        response::IntoResponse, routing::get,
    };
    use chrono::Utc;
    use heron_session::{
        IdentifierKind, Meeting, Participant, SummaryLifecycle, TranscriptLifecycle,
    };
    use heron_types::MeetingId;
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};
    use tokio::sync::oneshot;

    fn sample_meeting() -> Meeting {
        Meeting {
            id: MeetingId::now_v7(),
            status: MeetingStatus::Done,
            platform: Platform::Zoom,
            title: Some("Weekly product sync".to_owned()),
            calendar_event_id: None,
            started_at: Utc::now(),
            ended_at: Some(Utc::now()),
            duration_secs: Some(2520),
            participants: vec![Participant {
                display_name: "Alex Chen".to_owned(),
                identifier_kind: IdentifierKind::AxTree,
                is_user: false,
            }],
            transcript_status: TranscriptLifecycle::Complete,
            summary_status: SummaryLifecycle::Ready,
        }
    }

    fn sample_summary(id: MeetingId) -> Summary {
        Summary {
            meeting_id: id,
            generated_at: Utc::now(),
            text: "## Summary\n\nDiscussed the roadmap.".to_owned(),
            action_items: Vec::new(),
            llm_provider: Some("anthropic".to_owned()),
            llm_model: Some("claude-3.5".to_owned()),
        }
    }

    /// Bare-minimum auth middleware mirroring herond's: requires
    /// `Authorization: Bearer test` on every protected route. Just
    /// the bearer check — we want to exercise the proxy's
    /// bearer-attachment path, not the full daemon middleware stack.
    async fn require_bearer(
        req: axum::extract::Request,
        next: middleware::Next,
    ) -> axum::response::Response {
        let presented = req
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| {
                let (scheme, rest) = s.split_once(char::is_whitespace)?;
                scheme
                    .eq_ignore_ascii_case("Bearer")
                    .then_some(rest.trim_start())
            });
        match presented {
            Some("test") => next.run(req).await,
            _ => StatusCode::UNAUTHORIZED.into_response(),
        }
    }

    /// Captures the last query string the mock list-meetings handler
    /// observed, so tests can assert that proxy serialization actually
    /// makes it onto the wire.
    type LastQuery = Arc<Mutex<Option<HashMap<String, String>>>>;

    async fn spawn(meeting: Arc<Meeting>) -> (SocketAddr, oneshot::Sender<()>, LastQuery) {
        let summary = Arc::new(sample_summary(meeting.id));
        let last_query: LastQuery = Arc::new(Mutex::new(None));

        let m = Arc::clone(&meeting);
        let lq = Arc::clone(&last_query);
        let list_handler = move |Query(params): Query<HashMap<String, String>>| {
            let m = Arc::clone(&m);
            let lq = Arc::clone(&lq);
            async move {
                if let Ok(mut slot) = lq.lock() {
                    *slot = Some(params);
                }
                Json(ListMeetingsPage {
                    items: vec![(*m).clone()],
                    next_cursor: None,
                })
            }
        };

        let s = Arc::clone(&summary);
        let summary_handler = move |Path(_id): Path<String>| {
            let s = Arc::clone(&s);
            async move { Json((*s).clone()) }
        };

        let app = Router::new()
            .route("/v1/meetings", get(list_handler))
            .route("/v1/meetings/{id}/summary", get(summary_handler))
            .layer(middleware::from_fn(require_bearer));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("ephemeral bind");
        let addr = listener.local_addr().expect("local_addr");
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = rx.await;
                })
                .await;
        });
        (addr, tx, last_query)
    }

    #[tokio::test]
    async fn list_meetings_happy_path() {
        let m = Arc::new(sample_meeting());
        let (addr, _tx, _q) = spawn(Arc::clone(&m)).await;
        let base = format!("http://{addr}/v1");

        let outcome = list_meetings_at(&base, "test", ListMeetingsQuery::default()).await;
        match outcome {
            DaemonOutcome::Ok { data } => {
                assert_eq!(data.items.len(), 1);
                assert_eq!(data.items[0].title.as_deref(), Some("Weekly product sync"),);
                assert_eq!(data.items[0].platform, Platform::Zoom);
                assert!(data.next_cursor.is_none());
            }
            DaemonOutcome::Unavailable { detail } => {
                panic!("expected Ok, got Unavailable: {detail}")
            }
        }
    }

    #[tokio::test]
    async fn list_meetings_unauthorized_when_bearer_wrong() {
        let m = Arc::new(sample_meeting());
        let (addr, _tx, _q) = spawn(m).await;
        let base = format!("http://{addr}/v1");

        let outcome = list_meetings_at(&base, "wrong-token", ListMeetingsQuery::default()).await;
        match outcome {
            DaemonOutcome::Unavailable { detail } => {
                assert!(
                    detail.contains("401"),
                    "expected 401 in detail, got: {detail}"
                );
            }
            DaemonOutcome::Ok { .. } => panic!("expected Unavailable on bad bearer"),
        }
    }

    #[tokio::test]
    async fn list_meetings_unavailable_when_port_closed() {
        // Bind an ephemeral port, drop the listener so the OS releases
        // it, then point the proxy at the now-closed address.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        drop(listener);
        let base = format!("http://{addr}/v1");

        let outcome = list_meetings_at(&base, "test", ListMeetingsQuery::default()).await;
        assert!(matches!(outcome, DaemonOutcome::Unavailable { .. }));
    }

    #[tokio::test]
    async fn get_summary_happy_path() {
        let m = Arc::new(sample_meeting());
        let id = m.id;
        let (addr, _tx, _q) = spawn(m).await;
        let base = format!("http://{addr}/v1");

        let outcome = get_summary_at(&base, "test", &id.to_string()).await;
        match outcome {
            DaemonOutcome::Ok { data } => {
                assert!(data.text.contains("Summary"));
                assert_eq!(data.llm_provider.as_deref(), Some("anthropic"));
            }
            DaemonOutcome::Unavailable { detail } => {
                panic!("expected Ok, got Unavailable: {detail}")
            }
        }
    }

    #[tokio::test]
    async fn list_meetings_propagates_query_params() {
        let m = Arc::new(sample_meeting());
        let (addr, _tx, last_query) = spawn(Arc::clone(&m)).await;
        let base = format!("http://{addr}/v1");

        let q = ListMeetingsQuery {
            status: Some(MeetingStatus::Done),
            platform: Some(Platform::Zoom),
            limit: Some(50),
            ..ListMeetingsQuery::default()
        };
        let outcome = list_meetings_at(&base, "test", q).await;
        assert!(matches!(outcome, DaemonOutcome::Ok { .. }));

        // The mock handler captured the parsed query map. Assert each
        // param actually rode the wire, with the wire-format values the
        // daemon expects (lowercase enum strings, decimal limit), and
        // that no extra/empty params snuck in.
        let params = last_query
            .lock()
            .expect("last_query mutex")
            .clone()
            .expect("list-meetings handler should have observed a request");
        assert_eq!(params.get("status").map(String::as_str), Some("done"));
        assert_eq!(params.get("platform").map(String::as_str), Some("zoom"));
        assert_eq!(params.get("limit").map(String::as_str), Some("50"));
        assert!(!params.contains_key("since"));
        assert!(!params.contains_key("cursor"));
    }

    #[tokio::test]
    async fn try_from_ts_query_rejects_malformed_since() {
        let bad = TsListMeetingsQuery {
            since: Some("not-an-rfc3339-string".to_owned()),
            ..TsListMeetingsQuery::default()
        };
        let result = ListMeetingsQuery::try_from(bad);
        assert!(result.is_err(), "malformed `since` should be rejected");
    }

    // ── start / end-meeting proxy tests ──────────────────────────────
    //
    // Separate `spawn_capture_routes` helper rather than extending the
    // GET-only `spawn` above. Splitting keeps the existing read-path
    // tests untouched (less review churn) and gives the POST tests a
    // self-contained mock that captures the request body / path
    // segments the proxy emits.

    use axum::extract::Json as JsonExtract;
    use axum::http::header;
    use axum::routing::post;

    /// Captured raw JSON body of the most recent `POST /v1/meetings`
    /// the mock observed. We capture `serde_json::Value` rather than
    /// the deserialized `TsStartCaptureBody` because `Option<String>`
    /// can't distinguish "field omitted" from "field present as
    /// `null`" — both deserialize to `None`. The `skip_serializing_if`
    /// assertion in `start_capture_happy_path` checks key presence on
    /// the raw Value, which is the only way to actually prove the
    /// field rode the wire as omitted.
    type LastStartBody = Arc<Mutex<Option<serde_json::Value>>>;

    /// Captured `meeting_id` path segment for the most recent
    /// `POST /v1/meetings/{id}/end`. `None` until the first request.
    type LastEndedId = Arc<Mutex<Option<String>>>;

    /// Mock that serves both POST routes. The start handler returns a
    /// 202 + the synthetic `Meeting` so [`parse_response`]'s
    /// `is_success()` branch matches. The end handler returns 204
    /// No Content; we add a `Location` header on start to mirror the
    /// real daemon's response shape (`crates/herond/.../meetings.rs:103`).
    async fn spawn_capture_routes(
        meeting: Arc<Meeting>,
    ) -> (SocketAddr, oneshot::Sender<()>, LastStartBody, LastEndedId) {
        let last_body: LastStartBody = Arc::new(Mutex::new(None));
        let last_ended: LastEndedId = Arc::new(Mutex::new(None));

        let m = Arc::clone(&meeting);
        let lb = Arc::clone(&last_body);
        let start_handler = move |JsonExtract(body): JsonExtract<serde_json::Value>| {
            let m = Arc::clone(&m);
            let lb = Arc::clone(&lb);
            async move {
                if let Ok(mut slot) = lb.lock() {
                    *slot = Some(body);
                }
                let location = format!("/v1/meetings/{}", m.id);
                let mut res = (StatusCode::ACCEPTED, Json((*m).clone())).into_response();
                if let Ok(v) = location.parse() {
                    res.headers_mut().insert(header::LOCATION, v);
                }
                res
            }
        };

        let le = Arc::clone(&last_ended);
        let end_handler = move |Path(id): Path<String>| {
            let le = Arc::clone(&le);
            async move {
                if let Ok(mut slot) = le.lock() {
                    *slot = Some(id);
                }
                StatusCode::NO_CONTENT.into_response()
            }
        };

        let app = Router::new()
            .route("/v1/meetings", post(start_handler))
            .route("/v1/meetings/{id}/end", post(end_handler))
            .layer(middleware::from_fn(require_bearer));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("ephemeral bind");
        let addr = listener.local_addr().expect("local_addr");
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = rx.await;
                })
                .await;
        });
        (addr, tx, last_body, last_ended)
    }

    #[tokio::test]
    async fn start_capture_happy_path() {
        let m = Arc::new(sample_meeting());
        let (addr, _tx, last_body, _le) = spawn_capture_routes(Arc::clone(&m)).await;
        let base = format!("http://{addr}/v1");

        // First call: with a hint. Asserts the request body rides the
        // wire with both fields populated and the daemon's response
        // round-trips back as `DaemonOutcome::Ok`.
        let outcome = start_capture_at(
            &base,
            "test",
            Platform::Zoom,
            Some("Sprint review".to_owned()),
        )
        .await;
        match outcome {
            DaemonOutcome::Ok { data } => {
                assert_eq!(data.id, m.id);
                assert_eq!(data.platform, Platform::Zoom);
            }
            DaemonOutcome::Unavailable { detail } => {
                panic!("expected Ok, got Unavailable: {detail}")
            }
        }
        // Reach into the captured raw JSON Value so we can assert on
        // the wire shape directly.
        {
            let body = last_body.lock().expect("last_body mutex");
            let body = body
                .as_ref()
                .expect("start handler should have observed a request");
            assert_eq!(body.get("platform").and_then(|v| v.as_str()), Some("zoom"));
            assert_eq!(
                body.get("hint").and_then(|v| v.as_str()),
                Some("Sprint review")
            );
        }

        // Second call: hint = None. `skip_serializing_if =
        // "Option::is_none"` should drop the field entirely. Checking
        // key *presence* on the raw `Value` is the only way to
        // distinguish "omitted" from "rode the wire as `null`" —
        // both would deserialize to `None` on a typed `Option<String>`
        // extractor.
        let outcome = start_capture_at(&base, "test", Platform::GoogleMeet, None).await;
        assert!(matches!(outcome, DaemonOutcome::Ok { .. }));
        let body = last_body.lock().expect("last_body mutex");
        let body = body
            .as_ref()
            .expect("start handler should have observed a request");
        assert!(
            !body.as_object().unwrap().contains_key("hint"),
            "hint should be omitted from the wire entirely; raw body: {body}",
        );
        assert_eq!(
            body.get("platform").and_then(|v| v.as_str()),
            Some("google_meet")
        );
    }

    #[tokio::test]
    async fn start_capture_unavailable_when_port_closed() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        drop(listener);
        let base = format!("http://{addr}/v1");

        let outcome = start_capture_at(&base, "test", Platform::Zoom, None).await;
        assert!(matches!(outcome, DaemonOutcome::Unavailable { .. }));
    }

    #[tokio::test]
    async fn start_capture_unauthorized_when_bearer_wrong() {
        let m = Arc::new(sample_meeting());
        let (addr, _tx, _lb, _le) = spawn_capture_routes(m).await;
        let base = format!("http://{addr}/v1");

        let outcome = start_capture_at(&base, "wrong", Platform::Zoom, None).await;
        match outcome {
            DaemonOutcome::Unavailable { detail } => {
                assert!(
                    detail.contains("401"),
                    "expected 401 in detail, got: {detail}"
                );
            }
            DaemonOutcome::Ok { .. } => panic!("expected Unavailable on bad bearer"),
        }
    }

    #[tokio::test]
    async fn end_meeting_happy_path() {
        let m = Arc::new(sample_meeting());
        let id = m.id;
        let (addr, _tx, _lb, last_ended) = spawn_capture_routes(m).await;
        let base = format!("http://{addr}/v1");

        let outcome = end_meeting_at(&base, "test", &id.to_string()).await;
        assert!(matches!(outcome, DaemonOutcome::Ok { .. }));

        let observed = last_ended
            .lock()
            .expect("last_ended mutex")
            .clone()
            .expect("end handler should have observed a request");
        assert_eq!(observed, id.to_string());
    }

    #[tokio::test]
    async fn end_meeting_unavailable_when_port_closed() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        drop(listener);
        let base = format!("http://{addr}/v1");

        let outcome = end_meeting_at(&base, "test", "mtg_01h0a").await;
        assert!(matches!(outcome, DaemonOutcome::Unavailable { .. }));
    }

    #[tokio::test]
    async fn start_capture_truncates_oversize_hint() {
        // Renderer-side `heron_start_capture` clamps `hint` to 256
        // chars before [`start_capture_at`] runs. Drive the command
        // surface directly with the same `take(256)` the Tauri command
        // applies and assert the clamp lands on the wire — the mock
        // captures the body so we can read the field length back.
        let m = Arc::new(sample_meeting());
        let (addr, _tx, last_body, _le) = spawn_capture_routes(Arc::clone(&m)).await;
        let base = format!("http://{addr}/v1");

        let clamped: String = "A".repeat(1000).chars().take(256).collect();
        let outcome = start_capture_at(&base, "test", Platform::Zoom, Some(clamped)).await;
        assert!(matches!(outcome, DaemonOutcome::Ok { .. }));

        let body = last_body.lock().expect("last_body mutex");
        let observed = body
            .as_ref()
            .and_then(|b| b.get("hint"))
            .and_then(|v| v.as_str())
            .expect("start handler should have observed hint");
        assert_eq!(observed.chars().count(), 256);
    }

    #[tokio::test]
    async fn start_capture_unavailable_on_malformed_response_body() {
        // Cover the `parse_response::resp.json::<Meeting>()` failure
        // branch. A 200 status with a body that doesn't deserialize
        // into `Meeting` should land as `Unavailable`, not `Ok` with
        // garbage data — the discriminator is what the frontend
        // banner-routing logic switches on.
        let app = Router::new()
            .route(
                "/v1/meetings",
                post(|| async {
                    (StatusCode::ACCEPTED, "this is not valid Meeting JSON").into_response()
                }),
            )
            .layer(middleware::from_fn(require_bearer));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let (_tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = rx.await;
                })
                .await;
        });
        let base = format!("http://{addr}/v1");
        let outcome = start_capture_at(&base, "test", Platform::Zoom, None).await;
        match outcome {
            DaemonOutcome::Unavailable { detail } => {
                assert!(
                    detail.contains("parse"),
                    "expected parse-error detail, got: {detail}"
                );
            }
            DaemonOutcome::Ok { .. } => {
                panic!("expected Unavailable on malformed body")
            }
        }
    }
}
