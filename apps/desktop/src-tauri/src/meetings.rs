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

use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use futures_util::StreamExt;
use heron_session::{
    CalendarEvent, ListMeetingsPage, ListMeetingsQuery, Meeting, MeetingId, MeetingStatus,
    Platform, PreMeetingContextRequest, Summary, Transcript,
};
use serde::{Deserialize, Serialize};
use tauri::State;
use tokio::io::AsyncWriteExt;

use crate::daemon::DaemonHandle;

/// Loopback URL for the daemon API. Same pattern as
/// [`crate::daemon::HEALTH_URL`]; see module docs for why this isn't
/// renderer-supplied.
const BASE_URL: &str = "http://127.0.0.1:7384/v1";

/// Per-request timeout. See module docs for the choice of 5 s.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const AUDIO_READ_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_DAEMON_AUDIO_BYTES: u64 = 2 * 1024 * 1024 * 1024;

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

fn parse_meeting_id<T: Serialize>(meeting_id: &str) -> Result<MeetingId, DaemonOutcome<T>> {
    MeetingId::from_str(meeting_id).map_err(|e| DaemonOutcome::Unavailable {
        detail: format!("invalid meeting_id: {e}"),
    })
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

/// Tauri command: fetch a single meeting. Proxies
/// `GET /v1/meetings/{id}`.
///
/// Review uses this to resolve canonical metadata (title, status,
/// participants) from the daemon instead of treating the route param
/// as the whole meeting model. Same ID validation / URL-space defence
/// as [`heron_meeting_summary`].
#[tauri::command]
pub async fn heron_get_meeting(
    state: State<'_, DaemonHandle>,
    meeting_id: String,
) -> Result<DaemonOutcome<Meeting>, String> {
    let parsed = match parse_meeting_id(&meeting_id) {
        Ok(id) => id,
        Err(outcome) => return Ok(outcome),
    };
    let bearer = state.auth.bearer.clone();
    Ok(get_meeting_at(BASE_URL, &bearer, &parsed.to_string()).await)
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
    let parsed = match parse_meeting_id(&meeting_id) {
        Ok(id) => id,
        Err(outcome) => return Ok(outcome),
    };
    let bearer = state.auth.bearer.clone();
    Ok(get_summary_at(BASE_URL, &bearer, &parsed.to_string()).await)
}

/// Tauri command: fetch the finalized transcript. Proxies
/// `GET /v1/meetings/{id}/transcript`.
///
/// Live partials still flow over SSE; this command is for Review's
/// read-only Transcript tab after the daemon has a finalized sidecar.
#[tauri::command]
pub async fn heron_meeting_transcript(
    state: State<'_, DaemonHandle>,
    meeting_id: String,
) -> Result<DaemonOutcome<Transcript>, String> {
    let parsed = match parse_meeting_id(&meeting_id) {
        Ok(id) => id,
        Err(outcome) => return Ok(outcome),
    };
    let bearer = state.auth.bearer.clone();
    Ok(get_transcript_at(BASE_URL, &bearer, &parsed.to_string()).await)
}

/// Tauri command: fetch daemon-streamed audio into the app cache and
/// return a local file path playable via `convertFileSrc`.
///
/// We deliberately do not return the m4a bytes over Tauri IPC: long
/// meetings can be hundreds of MB. The daemon streams the response;
/// this proxy preserves that memory profile by writing chunks to
/// `<cache>/daemon-audio/<meeting_id>.m4a.tmp` and atomically renaming
/// once the body completes.
#[tauri::command]
pub async fn heron_meeting_audio(
    state: State<'_, DaemonHandle>,
    meeting_id: String,
) -> Result<DaemonOutcome<DaemonAudioSource>, String> {
    let parsed = match parse_meeting_id(&meeting_id) {
        Ok(id) => id,
        Err(outcome) => return Ok(outcome),
    };
    let bearer = state.auth.bearer.clone();
    Ok(fetch_audio_at(
        BASE_URL,
        &bearer,
        &parsed.to_string(),
        &crate::default_cache_root(),
    )
    .await)
}

/// Tauri command: start a manual capture. Proxies `POST /v1/meetings`.
///
/// Mirrors the heron-cli "manual capture escape hatch" — the user
/// clicks Start Recording on the Home page and we ask the daemon to
/// arm + start a session for the requested platform. Without this
/// command the desktop UI was a passive observer of meetings started
/// by some other path (CLI, future detector); Gap #7's whole point is
/// closing that loop.
///
/// Daemon error mapping mirrors the read commands: any non-2xx HTTP
/// status (the orchestrator rejects with 409 if a session is already
/// live, 5xx if the platform isn't running) collapses to
/// `Unavailable` with the detail string. The frontend surfaces that
/// via toast and stays on `/home` rather than navigating into a
/// recording page that has no meeting.
#[tauri::command]
pub async fn heron_start_capture(
    state: State<'_, DaemonHandle>,
    platform: Platform,
    hint: Option<String>,
    calendar_event_id: Option<String>,
) -> Result<DaemonOutcome<Meeting>, String> {
    let bearer = state.auth.bearer.clone();
    Ok(start_capture_at(BASE_URL, &bearer, platform, hint, calendar_event_id).await)
}

/// Tauri command: end a live capture. Proxies
/// `POST /v1/meetings/{id}/end`.
///
/// Stop & save on the Recording page funnels through here. The
/// daemon's 204 carries no body, so we synthesize an
/// [`EndMeetingAck`] echoing the meeting id back so the frontend
/// has a typed handle to clear local state on success.
///
/// `meeting_id` is validated with [`MeetingId::from_str`] before it
/// touches the URL — same defence-in-depth as
/// [`heron_meeting_summary`].
#[tauri::command]
pub async fn heron_end_meeting(
    state: State<'_, DaemonHandle>,
    meeting_id: String,
) -> Result<DaemonOutcome<EndMeetingAck>, String> {
    let parsed = match MeetingId::from_str(&meeting_id) {
        Ok(id) => id,
        Err(e) => {
            return Ok(DaemonOutcome::Unavailable {
                detail: format!("invalid meeting_id: {e}"),
            });
        }
    };
    let bearer = state.auth.bearer.clone();
    Ok(end_meeting_at(BASE_URL, &bearer, &parsed).await)
}

/// Tauri command: list upcoming calendar events. Proxies
/// `GET /v1/calendar/upcoming`.
///
/// Powers the Home page's upcoming-meetings rail. The daemon's
/// `LocalSessionOrchestrator::list_upcoming_calendar` reads from
/// EventKit via `heron_vault::CalendarReader`, so this proxy is the
/// only way the webview can see the user's week without granting it
/// raw EventKit access (which the privacy posture explicitly forbids).
///
/// `from` / `to` arrive as RFC3339 strings from the JS side and are
/// parsed via [`TsCalendarQuery::try_from`]. A malformed value
/// surfaces as `Unavailable` with a parse error in `detail` rather
/// than silently widening the request to "everything" — same
/// rationale as the `since` field in [`heron_list_meetings`].
#[tauri::command]
pub async fn heron_list_calendar_upcoming(
    state: State<'_, DaemonHandle>,
    query: TsCalendarQuery,
) -> Result<DaemonOutcome<CalendarPage>, String> {
    let bearer = state.auth.bearer.clone();
    let parsed = match CalendarQuery::try_from(query) {
        Ok(q) => q,
        Err(detail) => return Ok(DaemonOutcome::Unavailable { detail }),
    };
    Ok(list_upcoming_calendar_at(BASE_URL, &bearer, parsed).await)
}

/// Tauri command: attach pre-meeting context to a calendar event.
/// Proxies `PUT /v1/context`.
///
/// Pre-staged from the upcoming-meetings rail: clicking "Start with
/// context" attaches the calendar event's title / attendees / agenda
/// before the matching `start_capture` fires, so the orchestrator
/// finds the context already in `pending_contexts` (keyed by
/// `calendar_event_id`) when the meeting starts. The daemon emits
/// `204 No Content` on success; we synthesize an [`AttachContextAck`]
/// echoing the validated `calendar_event_id` so the JS side has a
/// typed handle without parsing an empty body — same pattern as
/// [`heron_end_meeting`].
#[tauri::command]
pub async fn heron_attach_context(
    state: State<'_, DaemonHandle>,
    request: PreMeetingContextRequest,
) -> Result<DaemonOutcome<AttachContextAck>, String> {
    let bearer = state.auth.bearer.clone();
    Ok(attach_context_at(BASE_URL, &bearer, &request).await)
}

/// Body shape for `POST /v1/meetings`. Mirrors
/// [`herond::routes::meetings::StartCaptureBody`] (and
/// `heron_cli::daemon::StartCaptureBody`); we don't share the cli
/// crate's struct because pulling `heron-cli` into the desktop crate
/// just for one wire-format mirror would balloon compile times for
/// no real reuse.
#[derive(Debug, Serialize)]
struct StartCaptureBody<'a> {
    platform: Platform,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    calendar_event_id: Option<&'a str>,
}

/// Synthetic ack for a successful `POST /v1/meetings/{id}/end`.
///
/// The daemon emits `204 No Content` (per the OpenAPI), so the JSON
/// body is empty; the TS side wants the meeting id back to flush its
/// local stores without re-deriving it from the request payload. We
/// echo the parsed id rather than the input string so the
/// [`MeetingId::from_str`] validation result is the source of truth
/// — guards against accidental ID drift on future refactors.
#[derive(Debug, Serialize, Deserialize)]
pub struct EndMeetingAck {
    pub meeting_id: MeetingId,
}

/// Local-file source returned by [`heron_meeting_audio`].
#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonAudioSource {
    pub path: String,
    pub content_type: Option<String>,
}

/// Synthetic ack for a successful `PUT /v1/context`.
///
/// Same rationale as [`EndMeetingAck`]: the daemon emits
/// `204 No Content`, but the JS side wants confirmation of which
/// `calendar_event_id` the context was stored under so it can clear
/// any optimistic UI state without re-deriving from the request.
#[derive(Debug, Serialize, Deserialize)]
pub struct AttachContextAck {
    pub calendar_event_id: String,
}

/// Wire shape for `GET /v1/calendar/upcoming`. Mirrors the daemon's
/// internal `CalendarPage` (defined as serialize-only inside herond);
/// we redefine it here with `Deserialize` so the proxy can decode the
/// response without pulling herond into the desktop crate just for
/// one type alias.
#[derive(Debug, Serialize, Deserialize)]
pub struct CalendarPage {
    pub items: Vec<CalendarEvent>,
}

/// Strongly-typed calendar query. Mirrors herond's `CalendarParams`
/// shape so the proxy can serialize the same `from` / `to` / `limit`
/// query string the daemon expects.
#[derive(Debug, Default)]
pub struct CalendarQuery {
    pub from: Option<chrono::DateTime<chrono::Utc>>,
    pub to: Option<chrono::DateTime<chrono::Utc>>,
    pub limit: Option<u32>,
}

/// TS-side mirror for the calendar query. Same `TsListMeetingsQuery`
/// rationale: the JS layer hands us strings, we parse them once at
/// the boundary and surface a typed `Unavailable` on malformed input
/// rather than silently widening the request to the daemon's default
/// window.
#[derive(Debug, Default, serde::Deserialize)]
pub struct TsCalendarQuery {
    pub from: Option<String>,
    pub to: Option<String>,
    pub limit: Option<u32>,
}

impl TryFrom<TsCalendarQuery> for CalendarQuery {
    type Error = String;

    fn try_from(q: TsCalendarQuery) -> Result<Self, Self::Error> {
        let parse = |s: String, name: &str| -> Result<chrono::DateTime<chrono::Utc>, String> {
            chrono::DateTime::parse_from_rfc3339(&s)
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .map_err(|e| format!("invalid `{name}` (RFC3339 expected): {e}"))
        };
        Ok(CalendarQuery {
            from: q.from.map(|s| parse(s, "from")).transpose()?,
            to: q.to.map(|s| parse(s, "to")).transpose()?,
            limit: q.limit,
        })
    }
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

/// Parameterized list — split out so unit tests can drive it against
/// an ephemeral-port axum server.
pub async fn list_meetings_at(
    base_url: &str,
    bearer: &str,
    query: ListMeetingsQuery,
) -> DaemonOutcome<ListMeetingsPage> {
    let client = match reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            return DaemonOutcome::Unavailable {
                detail: format!("client build: {e}"),
            };
        }
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
        Err(e) => DaemonOutcome::Unavailable {
            detail: e.to_string(),
        },
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
    get_json_at(base_url, bearer, &format!("/meetings/{meeting_id}/summary")).await
}

/// Parameterized get-meeting fetch. Same split-out rationale as
/// [`get_summary_at`].
pub async fn get_meeting_at(
    base_url: &str,
    bearer: &str,
    meeting_id: &str,
) -> DaemonOutcome<Meeting> {
    get_json_at(base_url, bearer, &format!("/meetings/{meeting_id}")).await
}

/// Parameterized transcript fetch. Same split-out rationale as
/// [`get_summary_at`].
pub async fn get_transcript_at(
    base_url: &str,
    bearer: &str,
    meeting_id: &str,
) -> DaemonOutcome<Transcript> {
    get_json_at(
        base_url,
        bearer,
        &format!("/meetings/{meeting_id}/transcript"),
    )
    .await
}

async fn get_json_at<T>(base_url: &str, bearer: &str, path: &str) -> DaemonOutcome<T>
where
    T: serde::de::DeserializeOwned + Serialize,
{
    let client = match reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            return DaemonOutcome::Unavailable {
                detail: format!("client build: {e}"),
            };
        }
    };
    let url = format!("{base_url}{path}");
    match client.get(url).bearer_auth(bearer).send().await {
        Ok(resp) => parse_response(resp).await,
        Err(e) => DaemonOutcome::Unavailable {
            detail: e.to_string(),
        },
    }
}

/// Parameterized audio fetch. Streams the daemon's `audio/mp4` body to
/// a local cache file so the WebView can play it via Tauri's asset
/// protocol without IPC-ing the whole recording.
pub async fn fetch_audio_at(
    base_url: &str,
    bearer: &str,
    meeting_id: &str,
    cache_root: &Path,
) -> DaemonOutcome<DaemonAudioSource> {
    let dir = cache_root.join("daemon-audio");
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        return DaemonOutcome::Unavailable {
            detail: format!("audio cache mkdir: {e}"),
        };
    }
    let final_path = dir.join(format!("{meeting_id}.m4a"));
    if let Ok(meta) = tokio::fs::metadata(&final_path).await
        && meta.is_file()
        && meta.len() > 0
    {
        return DaemonOutcome::Ok {
            data: DaemonAudioSource {
                path: final_path.to_string_lossy().into_owned(),
                content_type: Some("audio/mp4".to_owned()),
            },
        };
    }
    cleanup_stale_audio_temps(&dir).await;

    let client = match reqwest::Client::builder()
        // Audio may be large; keep connect/read errors bounded by the
        // daemon and OS rather than the metadata endpoint's 5s budget.
        .connect_timeout(REQUEST_TIMEOUT)
        .read_timeout(AUDIO_READ_TIMEOUT)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return DaemonOutcome::Unavailable {
                detail: format!("client build: {e}"),
            };
        }
    };
    let url = format!("{base_url}/meetings/{meeting_id}/audio");
    let resp = match client.get(url).bearer_auth(bearer).send().await {
        Ok(r) => r,
        Err(e) => {
            return DaemonOutcome::Unavailable {
                detail: e.to_string(),
            };
        }
    };
    let status = resp.status();
    if !status.is_success() {
        return DaemonOutcome::Unavailable {
            detail: format!("daemon returned {status}"),
        };
    }
    if let Some(len) = resp.content_length()
        && len > MAX_DAEMON_AUDIO_BYTES
    {
        return DaemonOutcome::Unavailable {
            detail: format!("audio response too large: {len} bytes"),
        };
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    if !is_supported_audio_content_type(content_type.as_deref()) {
        return DaemonOutcome::Unavailable {
            detail: format!(
                "unsupported audio content type: {}",
                content_type.as_deref().unwrap_or("<missing>")
            ),
        };
    }
    let tmp_path = dir.join(format!("{meeting_id}.{}.m4a.tmp", uuid::Uuid::now_v7()));
    let mut file = match tokio::fs::File::create(&tmp_path).await {
        Ok(f) => f,
        Err(e) => {
            return DaemonOutcome::Unavailable {
                detail: format!("audio cache create: {e}"),
            };
        }
    };
    let mut stream = resp.bytes_stream();
    let mut total = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                return DaemonOutcome::Unavailable {
                    detail: format!("audio stream: {e}"),
                };
            }
        };
        total = match total.checked_add(chunk.len() as u64) {
            Some(n) if n <= MAX_DAEMON_AUDIO_BYTES => n,
            _ => {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                return DaemonOutcome::Unavailable {
                    detail: format!(
                        "audio response too large: over {MAX_DAEMON_AUDIO_BYTES} bytes"
                    ),
                };
            }
        };
        if let Err(e) = file.write_all(&chunk).await {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return DaemonOutcome::Unavailable {
                detail: format!("audio cache write: {e}"),
            };
        }
    }
    if total == 0 {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return DaemonOutcome::Unavailable {
            detail: "audio response was empty".to_owned(),
        };
    }
    if let Err(e) = file.flush().await {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return DaemonOutcome::Unavailable {
            detail: format!("audio cache flush: {e}"),
        };
    }
    drop(file);
    if let Err(e) = tokio::fs::rename(&tmp_path, &final_path).await {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return DaemonOutcome::Unavailable {
            detail: format!("audio cache rename: {e}"),
        };
    }
    DaemonOutcome::Ok {
        data: DaemonAudioSource {
            path: final_path.to_string_lossy().into_owned(),
            content_type,
        },
    }
}

fn is_supported_audio_content_type(content_type: Option<&str>) -> bool {
    let Some(content_type) = content_type else {
        return false;
    };
    matches!(
        content_type.split(';').next().map(str::trim),
        Some("audio/mp4" | "audio/mpeg" | "audio/x-m4a")
    )
}

async fn cleanup_stale_audio_temps(dir: &Path) {
    let cutoff = Duration::from_secs(24 * 60 * 60);
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let is_tmp = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(".m4a.tmp"));
        if !is_tmp {
            continue;
        }
        let Ok(meta) = entry.metadata().await else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if modified.elapsed().is_ok_and(|age| age >= cutoff) {
            let _ = tokio::fs::remove_file(path).await;
        }
    }
}

/// Parameterized start — same split-out rationale as
/// [`list_meetings_at`]. Posts the [`StartCaptureBody`] envelope, then
/// decodes the daemon's 202 body into a [`Meeting`]. Any non-2xx
/// (including the orchestrator's 409 "already recording" and the 5xx
/// platform-not-running cases) collapses to [`DaemonOutcome::Unavailable`]
/// with the status code in `detail`, matching the existing read
/// proxies' behaviour.
pub async fn start_capture_at(
    base_url: &str,
    bearer: &str,
    platform: Platform,
    hint: Option<String>,
    calendar_event_id: Option<String>,
) -> DaemonOutcome<Meeting> {
    let client = match reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            return DaemonOutcome::Unavailable {
                detail: format!("client build: {e}"),
            };
        }
    };
    let body = StartCaptureBody {
        platform,
        hint: hint.as_deref(),
        calendar_event_id: calendar_event_id.as_deref(),
    };
    let url = format!("{base_url}/meetings");
    let response = client
        .post(url)
        .bearer_auth(bearer)
        .json(&body)
        .send()
        .await;
    match response {
        Ok(resp) => parse_response(resp).await,
        Err(e) => DaemonOutcome::Unavailable {
            detail: e.to_string(),
        },
    }
}

/// Parameterized end — same split-out rationale as
/// [`list_meetings_at`]. The daemon emits `204 No Content` on success;
/// we synthesize an [`EndMeetingAck`] echoing the validated meeting id
/// so the JS side has a typed handle without parsing an empty body.
pub async fn end_meeting_at(
    base_url: &str,
    bearer: &str,
    meeting_id: &MeetingId,
) -> DaemonOutcome<EndMeetingAck> {
    let client = match reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            return DaemonOutcome::Unavailable {
                detail: format!("client build: {e}"),
            };
        }
    };
    let url = format!("{base_url}/meetings/{meeting_id}/end");
    let resp = match client.post(url).bearer_auth(bearer).send().await {
        Ok(r) => r,
        Err(e) => {
            return DaemonOutcome::Unavailable {
                detail: e.to_string(),
            };
        }
    };
    let status = resp.status();
    if !status.is_success() {
        return DaemonOutcome::Unavailable {
            detail: format!("daemon returned {status}"),
        };
    }
    DaemonOutcome::Ok {
        data: EndMeetingAck {
            meeting_id: *meeting_id,
        },
    }
}

/// Parameterized calendar fetch — same split-out rationale as
/// [`list_meetings_at`]. Only attaches query params that are actually
/// set so the daemon falls back to its own defaults
/// (now → +7d, limit ≤ 100) for any field the renderer omitted.
pub async fn list_upcoming_calendar_at(
    base_url: &str,
    bearer: &str,
    query: CalendarQuery,
) -> DaemonOutcome<CalendarPage> {
    let client = match reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            return DaemonOutcome::Unavailable {
                detail: format!("client build: {e}"),
            };
        }
    };
    let mut request = client
        .get(format!("{base_url}/calendar/upcoming"))
        .bearer_auth(bearer);
    let mut params: Vec<(&str, String)> = Vec::new();
    if let Some(f) = query.from {
        params.push(("from", f.to_rfc3339()));
    }
    if let Some(t) = query.to {
        params.push(("to", t.to_rfc3339()));
    }
    if let Some(l) = query.limit {
        params.push(("limit", l.to_string()));
    }
    if !params.is_empty() {
        request = request.query(&params);
    }
    match request.send().await {
        Ok(resp) => parse_response(resp).await,
        Err(e) => DaemonOutcome::Unavailable {
            detail: e.to_string(),
        },
    }
}

/// Parameterized context attach — same split-out rationale as
/// [`end_meeting_at`]. PUTs the [`PreMeetingContextRequest`] body and
/// synthesizes the [`AttachContextAck`] from the request itself on a
/// 2xx, since the daemon's 204 carries no body. The daemon validates
/// `calendar_event_id` (non-empty, ≤ MAX_CALENDAR_EVENT_ID_BYTES) and
/// the serialized context size; both surface here as non-2xx and
/// collapse to `Unavailable` with the status code in `detail`.
pub async fn attach_context_at(
    base_url: &str,
    bearer: &str,
    request: &PreMeetingContextRequest,
) -> DaemonOutcome<AttachContextAck> {
    let client = match reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build() {
        Ok(c) => c,
        Err(e) => {
            return DaemonOutcome::Unavailable {
                detail: format!("client build: {e}"),
            };
        }
    };
    let url = format!("{base_url}/context");
    let resp = match client
        .put(url)
        .bearer_auth(bearer)
        .json(request)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return DaemonOutcome::Unavailable {
                detail: e.to_string(),
            };
        }
    };
    let status = resp.status();
    if !status.is_success() {
        return DaemonOutcome::Unavailable {
            detail: format!("daemon returned {status}"),
        };
    }
    DaemonOutcome::Ok {
        data: AttachContextAck {
            // Echo the same `calendar_event_id` the orchestrator
            // stores. The daemon's `normalize_calendar_event_id`
            // trims surrounding whitespace before keying
            // `pending_contexts`; if we echoed the un-trimmed
            // request value, a caller comparing the ack to its own
            // record would think they disagreed. Trim here to match.
            calendar_event_id: request.calendar_event_id.trim().to_owned(),
        },
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
        TranscriptSegment,
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
            tags: vec![],
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

    fn sample_transcript(id: MeetingId) -> Transcript {
        Transcript {
            meeting_id: id,
            status: TranscriptLifecycle::Complete,
            language: Some("en".to_owned()),
            segments: vec![TranscriptSegment {
                speaker: Participant {
                    display_name: "Alex Chen".to_owned(),
                    identifier_kind: IdentifierKind::AxTree,
                    is_user: false,
                },
                text: "Let's ship the daemon wiring.".to_owned(),
                start_secs: 12.0,
                end_secs: 15.5,
                confidence: heron_session::Confidence::High,
                is_final: true,
            }],
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
        let transcript = Arc::new(sample_transcript(meeting.id));
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

        let gm = Arc::clone(&meeting);
        let get_meeting_handler = move |Path(_id): Path<String>| {
            let gm = Arc::clone(&gm);
            async move { Json((*gm).clone()) }
        };

        let t = Arc::clone(&transcript);
        let transcript_handler = move |Path(_id): Path<String>| {
            let t = Arc::clone(&t);
            async move { Json((*t).clone()) }
        };

        let audio_handler = move |Path(_id): Path<String>| async move {
            (
                [(axum::http::header::CONTENT_TYPE, "audio/mp4")],
                vec![0_u8, 1, 2, 3],
            )
        };

        let app = Router::new()
            .route("/v1/meetings", get(list_handler))
            .route("/v1/meetings/{id}", get(get_meeting_handler))
            .route("/v1/meetings/{id}/summary", get(summary_handler))
            .route("/v1/meetings/{id}/transcript", get(transcript_handler))
            .route("/v1/meetings/{id}/audio", get(audio_handler))
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
    async fn get_meeting_happy_path() {
        let m = Arc::new(sample_meeting());
        let id = m.id;
        let (addr, _tx, _q) = spawn(m).await;
        let base = format!("http://{addr}/v1");

        let outcome = get_meeting_at(&base, "test", &id.to_string()).await;
        match outcome {
            DaemonOutcome::Ok { data } => {
                assert_eq!(data.id, id);
                assert_eq!(data.title.as_deref(), Some("Weekly product sync"));
            }
            DaemonOutcome::Unavailable { detail } => {
                panic!("expected Ok, got Unavailable: {detail}")
            }
        }
    }

    #[tokio::test]
    async fn get_transcript_happy_path() {
        let m = Arc::new(sample_meeting());
        let id = m.id;
        let (addr, _tx, _q) = spawn(m).await;
        let base = format!("http://{addr}/v1");

        let outcome = get_transcript_at(&base, "test", &id.to_string()).await;
        match outcome {
            DaemonOutcome::Ok { data } => {
                assert_eq!(data.meeting_id, id);
                assert_eq!(data.segments.len(), 1);
                assert_eq!(data.segments[0].text, "Let's ship the daemon wiring.");
            }
            DaemonOutcome::Unavailable { detail } => {
                panic!("expected Ok, got Unavailable: {detail}")
            }
        }
    }

    #[tokio::test]
    async fn fetch_audio_happy_path_writes_cache_file() {
        let m = Arc::new(sample_meeting());
        let id = m.id;
        let (addr, _tx, _q) = spawn(m).await;
        let base = format!("http://{addr}/v1");
        let tmp = tempfile::TempDir::new().expect("tmp");

        let outcome = fetch_audio_at(&base, "test", &id.to_string(), tmp.path()).await;
        match outcome {
            DaemonOutcome::Ok { data } => {
                assert_eq!(data.content_type.as_deref(), Some("audio/mp4"));
                let bytes = std::fs::read(&data.path).expect("audio cache readable");
                assert_eq!(bytes, vec![0_u8, 1, 2, 3]);
                let cache_dir = std::path::Path::new(&data.path)
                    .parent()
                    .expect("audio cache dir");
                let leaked_tmp = std::fs::read_dir(cache_dir)
                    .expect("read audio cache dir")
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .any(|path| {
                        path.file_name()
                            .and_then(|n| n.to_str())
                            .map(|n| n.starts_with(&id.to_string()) && n.ends_with(".m4a.tmp"))
                            .unwrap_or(false)
                    });
                assert!(!leaked_tmp, "leaked temp audio file for meeting");
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

    // ---- Gap #7 capture-wiring tests ----------------------------------
    //
    // The list/summary `spawn` above is read-only. The capture proxies
    // POST and consume `204 No Content`, so we use a purpose-built
    // server here rather than overloading the existing fixture — keeps
    // the read-side tests' invariants (one captured query map, fixed
    // meeting body) untangled from the capture handlers' state.

    /// Captures the last POST body the start handler observed, plus the
    /// path the end handler was hit with, so tests assert the wire
    /// payload + URL shape.
    type LastCapture = Arc<Mutex<Option<serde_json::Value>>>;
    type LastEndPath = Arc<Mutex<Option<String>>>;

    async fn spawn_capture(
        meeting: Meeting,
    ) -> (SocketAddr, oneshot::Sender<()>, LastCapture, LastEndPath) {
        let last_body: LastCapture = Arc::new(Mutex::new(None));
        let last_path: LastEndPath = Arc::new(Mutex::new(None));

        let m = Arc::new(meeting);
        let mc = Arc::clone(&m);
        let body_slot = Arc::clone(&last_body);
        let start_handler = move |Json(body): Json<serde_json::Value>| {
            let mc = Arc::clone(&mc);
            let body_slot = Arc::clone(&body_slot);
            async move {
                if let Ok(mut slot) = body_slot.lock() {
                    *slot = Some(body);
                }
                // Mirror the daemon's 202-with-Meeting body. The
                // `Location` header isn't asserted by the proxy, so a
                // bare `Json` response is enough.
                (StatusCode::ACCEPTED, Json((*mc).clone()))
            }
        };

        let path_slot = Arc::clone(&last_path);
        let end_handler = move |Path(id): Path<String>| {
            let path_slot = Arc::clone(&path_slot);
            async move {
                if let Ok(mut slot) = path_slot.lock() {
                    *slot = Some(id);
                }
                StatusCode::NO_CONTENT
            }
        };

        let app = Router::new()
            .route("/v1/meetings", axum::routing::post(start_handler))
            .route("/v1/meetings/{id}/end", axum::routing::post(end_handler))
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
        (addr, tx, last_body, last_path)
    }

    #[tokio::test]
    async fn start_capture_happy_path_returns_meeting() {
        let m = sample_meeting();
        let id = m.id;
        let platform = m.platform;
        let (addr, _tx, _body, _path) = spawn_capture(m).await;
        let base = format!("http://{addr}/v1");

        let outcome = start_capture_at(&base, "test", platform, None, None).await;
        match outcome {
            DaemonOutcome::Ok { data } => {
                assert_eq!(data.id, id);
                assert_eq!(data.platform, platform);
            }
            DaemonOutcome::Unavailable { detail } => {
                panic!("expected Ok, got Unavailable: {detail}")
            }
        }
    }

    #[tokio::test]
    async fn start_capture_propagates_hint_and_calendar_id() {
        let m = sample_meeting();
        let (addr, _tx, last_body, _path) = spawn_capture(m).await;
        let base = format!("http://{addr}/v1");

        let _ = start_capture_at(
            &base,
            "test",
            Platform::Zoom,
            Some("1:1 with Alex".to_owned()),
            Some("EVT-123".to_owned()),
        )
        .await;

        // Wire format: `platform` + optional `hint` + optional
        // `calendar_event_id`. None-shaped fields skip serialization
        // (matches the daemon's `Option` shape and avoids sending
        // explicit `null`s the orchestrator would have to ignore).
        let body = last_body
            .lock()
            .expect("body mutex")
            .clone()
            .expect("start handler should have observed a body");
        assert_eq!(body["platform"], "zoom");
        assert_eq!(body["hint"], "1:1 with Alex");
        assert_eq!(body["calendar_event_id"], "EVT-123");
    }

    #[tokio::test]
    async fn start_capture_omits_none_fields() {
        let m = sample_meeting();
        let (addr, _tx, last_body, _path) = spawn_capture(m).await;
        let base = format!("http://{addr}/v1");

        let _ = start_capture_at(&base, "test", Platform::Zoom, None, None).await;

        let body = last_body
            .lock()
            .expect("body mutex")
            .clone()
            .expect("start handler should have observed a body");
        // The proxy skips serializing `None`s entirely so a future
        // daemon that warns on unknown explicit-null fields stays
        // green. (Belt-and-suspenders — serde already does this for
        // us via `skip_serializing_if`.)
        assert!(body.get("hint").is_none());
        assert!(body.get("calendar_event_id").is_none());
    }

    #[tokio::test]
    async fn start_capture_unauthorized_when_bearer_wrong() {
        let m = sample_meeting();
        let (addr, _tx, _body, _path) = spawn_capture(m).await;
        let base = format!("http://{addr}/v1");

        let outcome = start_capture_at(&base, "wrong-token", Platform::Zoom, None, None).await;
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
    async fn end_meeting_happy_path_returns_ack() {
        let m = sample_meeting();
        let id = m.id;
        let (addr, _tx, _body, last_path) = spawn_capture(m).await;
        let base = format!("http://{addr}/v1");

        let outcome = end_meeting_at(&base, "test", &id).await;
        match outcome {
            DaemonOutcome::Ok { data } => {
                assert_eq!(data.meeting_id, id);
            }
            DaemonOutcome::Unavailable { detail } => {
                panic!("expected Ok, got Unavailable: {detail}")
            }
        }
        // The handler captured the path the proxy hit — assert it
        // matches the meeting id, no path traversal or extra
        // segments.
        let path = last_path
            .lock()
            .expect("path mutex")
            .clone()
            .expect("end handler should have observed a request");
        assert_eq!(path, id.to_string());
    }

    #[tokio::test]
    async fn end_meeting_unavailable_when_port_closed() {
        // Same pattern as `list_meetings_unavailable_when_port_closed`
        // but for the end endpoint — proves a transport-level error
        // collapses to `Unavailable` rather than panicking through.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        drop(listener);
        let base = format!("http://{addr}/v1");

        let id = MeetingId::now_v7();
        let outcome = end_meeting_at(&base, "test", &id).await;
        assert!(matches!(outcome, DaemonOutcome::Unavailable { .. }));
    }

    // ---- Gap #8 calendar / context / transcript tests ------------------
    //
    // Spawn a fresh server per group rather than overloading the read /
    // capture fixtures. The endpoints under test have distinct shapes
    // (PUT, query-string fan-out, large JSON body) and folding them
    // into the existing handlers would muddy the captured-state
    // assertions the older tests already rely on.

    use heron_session::{AttendeeContext, PreMeetingContext};

    fn sample_calendar_event() -> CalendarEvent {
        CalendarEvent {
            id: "EVT-week-12".to_owned(),
            title: "Quarterly review".to_owned(),
            start: Utc::now(),
            end: Utc::now() + chrono::Duration::hours(1),
            attendees: vec![AttendeeContext {
                name: "Alex Chen".to_owned(),
                email: Some("alex@example.com".to_owned()),
                last_seen_in: None,
                relationship: None,
                notes: None,
            }],
            meeting_url: Some("https://zoom.us/j/123".to_owned()),
            related_meetings: Vec::new(),
        }
    }

    type LastCalendarQuery = Arc<Mutex<Option<HashMap<String, String>>>>;
    type LastAttachBody = Arc<Mutex<Option<serde_json::Value>>>;

    async fn spawn_gap8(
        event: CalendarEvent,
    ) -> (
        SocketAddr,
        oneshot::Sender<()>,
        LastCalendarQuery,
        LastAttachBody,
    ) {
        let last_query: LastCalendarQuery = Arc::new(Mutex::new(None));
        let last_body: LastAttachBody = Arc::new(Mutex::new(None));

        let event = Arc::new(event);
        let evc = Arc::clone(&event);
        let lqc = Arc::clone(&last_query);
        let calendar_handler = move |Query(params): Query<HashMap<String, String>>| {
            let evc = Arc::clone(&evc);
            let lqc = Arc::clone(&lqc);
            async move {
                if let Ok(mut slot) = lqc.lock() {
                    *slot = Some(params);
                }
                Json(CalendarPage {
                    items: vec![(*evc).clone()],
                })
            }
        };

        let lbc = Arc::clone(&last_body);
        let attach_handler = move |Json(body): Json<serde_json::Value>| {
            let lbc = Arc::clone(&lbc);
            async move {
                if let Ok(mut slot) = lbc.lock() {
                    *slot = Some(body);
                }
                StatusCode::NO_CONTENT
            }
        };

        let app = Router::new()
            .route("/v1/calendar/upcoming", get(calendar_handler))
            .route("/v1/context", axum::routing::put(attach_handler))
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
        (addr, tx, last_query, last_body)
    }

    #[tokio::test]
    async fn list_upcoming_calendar_happy_path_returns_events() {
        let evt = sample_calendar_event();
        let (addr, _tx, _q, _b) = spawn_gap8(evt.clone()).await;
        let base = format!("http://{addr}/v1");

        let outcome = list_upcoming_calendar_at(&base, "test", CalendarQuery::default()).await;
        match outcome {
            DaemonOutcome::Ok { data } => {
                assert_eq!(data.items.len(), 1);
                assert_eq!(data.items[0].id, evt.id);
                assert_eq!(data.items[0].title, evt.title);
            }
            DaemonOutcome::Unavailable { detail } => {
                panic!("expected Ok, got Unavailable: {detail}")
            }
        }
    }

    #[tokio::test]
    async fn list_upcoming_calendar_propagates_query_params() {
        let evt = sample_calendar_event();
        let (addr, _tx, last_query, _b) = spawn_gap8(evt).await;
        let base = format!("http://{addr}/v1");

        let from = chrono::DateTime::parse_from_rfc3339("2026-04-28T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let to = chrono::DateTime::parse_from_rfc3339("2026-05-05T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let q = CalendarQuery {
            from: Some(from),
            to: Some(to),
            limit: Some(25),
        };
        let outcome = list_upcoming_calendar_at(&base, "test", q).await;
        assert!(matches!(outcome, DaemonOutcome::Ok { .. }));

        let params = last_query
            .lock()
            .expect("last_query mutex")
            .clone()
            .expect("calendar handler should have observed a request");
        // Round-trip through `to_rfc3339` so the assertion matches what
        // chrono actually emits (e.g. `+00:00` vs `Z`) — we only care
        // that the parsed datetimes ride the wire faithfully.
        assert_eq!(
            params.get("from").map(String::as_str),
            Some(from.to_rfc3339().as_str())
        );
        assert_eq!(
            params.get("to").map(String::as_str),
            Some(to.to_rfc3339().as_str())
        );
        assert_eq!(params.get("limit").map(String::as_str), Some("25"));
    }

    #[tokio::test]
    async fn list_upcoming_calendar_omits_none_params() {
        let evt = sample_calendar_event();
        let (addr, _tx, last_query, _b) = spawn_gap8(evt).await;
        let base = format!("http://{addr}/v1");

        let _ = list_upcoming_calendar_at(&base, "test", CalendarQuery::default()).await;

        let params = last_query
            .lock()
            .expect("last_query mutex")
            .clone()
            .expect("calendar handler should have observed a request");
        assert!(!params.contains_key("from"));
        assert!(!params.contains_key("to"));
        assert!(!params.contains_key("limit"));
    }

    #[tokio::test]
    async fn try_from_ts_calendar_query_rejects_malformed_from() {
        let bad = TsCalendarQuery {
            from: Some("not-a-date".to_owned()),
            ..TsCalendarQuery::default()
        };
        assert!(CalendarQuery::try_from(bad).is_err());
    }

    #[tokio::test]
    async fn list_upcoming_calendar_unauthorized_when_bearer_wrong() {
        let evt = sample_calendar_event();
        let (addr, _tx, _q, _b) = spawn_gap8(evt).await;
        let base = format!("http://{addr}/v1");

        let outcome =
            list_upcoming_calendar_at(&base, "wrong-token", CalendarQuery::default()).await;
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
    async fn attach_context_happy_path_echoes_calendar_event_id() {
        let evt = sample_calendar_event();
        let (addr, _tx, _q, last_body) = spawn_gap8(evt).await;
        let base = format!("http://{addr}/v1");

        let req = PreMeetingContextRequest {
            calendar_event_id: "EVT-attach-1".to_owned(),
            context: PreMeetingContext {
                agenda: Some("Quarterly review".to_owned()),
                attendees_known: Vec::new(),
                related_notes: Vec::new(),
                prior_decisions: Vec::new(),
                user_briefing: Some("Focus on Q3 retro".to_owned()),
            },
        };
        let outcome = attach_context_at(&base, "test", &req).await;
        match outcome {
            DaemonOutcome::Ok { data } => {
                assert_eq!(data.calendar_event_id, "EVT-attach-1");
            }
            DaemonOutcome::Unavailable { detail } => {
                panic!("expected Ok, got Unavailable: {detail}")
            }
        }

        let body = last_body
            .lock()
            .expect("body mutex")
            .clone()
            .expect("attach handler should have observed a body");
        assert_eq!(body["calendar_event_id"], "EVT-attach-1");
        assert_eq!(body["context"]["agenda"], "Quarterly review");
        assert_eq!(body["context"]["user_briefing"], "Focus on Q3 retro");
    }

    #[tokio::test]
    async fn attach_context_ack_echoes_trimmed_calendar_event_id() {
        let evt = sample_calendar_event();
        let (addr, _tx, _q, _last_body) = spawn_gap8(evt).await;
        let base = format!("http://{addr}/v1");

        // Daemon-side `normalize_calendar_event_id` trims surrounding
        // whitespace before keying `pending_contexts`. The proxy's
        // ack must reflect the trimmed form so a caller comparing
        // the ack to its own record sees the same key the
        // orchestrator stored.
        let req = PreMeetingContextRequest {
            calendar_event_id: "  EVT-padded  ".to_owned(),
            context: PreMeetingContext::default(),
        };
        let outcome = attach_context_at(&base, "test", &req).await;
        match outcome {
            DaemonOutcome::Ok { data } => {
                assert_eq!(data.calendar_event_id, "EVT-padded");
            }
            DaemonOutcome::Unavailable { detail } => {
                panic!("expected Ok, got Unavailable: {detail}")
            }
        }
    }

    #[tokio::test]
    async fn attach_context_unavailable_when_port_closed() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        drop(listener);
        let base = format!("http://{addr}/v1");

        let req = PreMeetingContextRequest {
            calendar_event_id: "EVT-x".to_owned(),
            context: PreMeetingContext::default(),
        };
        let outcome = attach_context_at(&base, "test", &req).await;
        assert!(matches!(outcome, DaemonOutcome::Unavailable { .. }));
    }
}
