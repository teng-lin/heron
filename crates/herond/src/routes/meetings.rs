//! `/v1/meetings*`, `/v1/calendar/upcoming`, `/v1/context` routes.
//!
//! Forwards to [`heron_session::SessionOrchestrator`] held in
//! [`crate::AppState`]. The trait does the work; these handlers are
//! pure projections — extract typed parameters, call the trait,
//! map [`heron_session::SessionError`] to the OpenAPI `Error`
//! envelope.
//!
//! `start_capture` (`POST /meetings`), `end_meeting`
//! (`POST /meetings/{id}/end`), and `attach_context` (`PUT /context`)
//! all run the real FSM-driven implementation in
//! `heron_orchestrator::LocalSessionOrchestrator`: the start
//! handler walks `idle → armed → recording`, spawns the audio
//! pipeline, and returns the freshly-created `Meeting`; end
//! drains the pipeline and finalizes WAV writers. The router
//! doesn't know the difference between trait stubs and the live
//! impl — it just forwards.

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use chrono::{DateTime, Utc};
use heron_session::{
    CalendarEvent, ListMeetingsQuery, MeetingId, MeetingStatus, Platform, PreMeetingContextRequest,
    StartCaptureArgs, Summary, Transcript,
};
use serde::Deserialize;

use crate::AppState;
use crate::error::WireError;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/meetings", get(list_meetings).post(start_capture))
        .route("/meetings/{meeting_id}", get(get_meeting))
        .route("/meetings/{meeting_id}/end", post(end_meeting))
        .route("/meetings/{meeting_id}/transcript", get(read_transcript))
        .route("/meetings/{meeting_id}/summary", get(read_summary))
        .route("/meetings/{meeting_id}/audio", get(read_audio))
        .route("/calendar/upcoming", get(list_upcoming_calendar))
        .route("/context", put(attach_context))
}

// ── meetings ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
pub struct ListMeetingsParams {
    pub since: Option<DateTime<Utc>>,
    pub status: Option<MeetingStatus>,
    pub platform: Option<Platform>,
    pub limit: Option<u32>,
    pub cursor: Option<String>,
}

async fn list_meetings(
    State(state): State<AppState>,
    Query(p): Query<ListMeetingsParams>,
) -> Response {
    let q = ListMeetingsQuery {
        since: p.since,
        status: p.status,
        platform: p.platform,
        limit: p.limit,
        cursor: p.cursor,
    };
    match state.orchestrator.list_meetings(q).await {
        Ok(page) => Json(page).into_response(),
        Err(e) => WireError::from(e).into_response(),
    }
}

async fn get_meeting(State(state): State<AppState>, Path(meeting_id): Path<MeetingId>) -> Response {
    match state.orchestrator.get_meeting(&meeting_id).await {
        Ok(meeting) => Json(meeting).into_response(),
        Err(e) => WireError::from(e).into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct StartCaptureBody {
    pub platform: Platform,
    pub hint: Option<String>,
    #[serde(default)]
    pub calendar_event_id: Option<String>,
}

async fn start_capture(
    State(state): State<AppState>,
    Json(body): Json<StartCaptureBody>,
) -> Response {
    let args = StartCaptureArgs {
        platform: body.platform,
        hint: body.hint,
        calendar_event_id: body.calendar_event_id,
    };
    match state.orchestrator.start_capture(args).await {
        Ok(meeting) => {
            // OpenAPI: `202 Accepted` with a `Location` header
            // pointing at the newly-created meeting resource.
            let location = format!("/v1/meetings/{}", meeting.id);
            let mut res = (StatusCode::ACCEPTED, Json(meeting)).into_response();
            if let Ok(value) = location.parse() {
                res.headers_mut().insert(header::LOCATION, value);
            }
            res
        }
        Err(e) => WireError::from(e).into_response(),
    }
}

async fn end_meeting(State(state): State<AppState>, Path(meeting_id): Path<MeetingId>) -> Response {
    match state.orchestrator.end_meeting(&meeting_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => WireError::from(e).into_response(),
    }
}

// ── transcripts / summaries ───────────────────────────────────────────

async fn read_transcript(
    State(state): State<AppState>,
    Path(meeting_id): Path<MeetingId>,
) -> Response {
    match state.orchestrator.read_transcript(&meeting_id).await {
        Ok(t) => Json::<Transcript>(t).into_response(),
        Err(e) => WireError::from(e).into_response(),
    }
}

async fn read_summary(
    State(state): State<AppState>,
    Path(meeting_id): Path<MeetingId>,
) -> Response {
    match state.orchestrator.read_summary(&meeting_id).await {
        Ok(Some(summary)) => Json::<Summary>(summary).into_response(),
        Ok(None) => {
            // Spec: 202 + `Retry-After` hint when summary not yet
            // generated. The orchestrator distinguishes "exists,
            // pending" from "not found" — only the former returns
            // `Ok(None)`.
            let mut res = StatusCode::ACCEPTED.into_response();
            if let Ok(v) = "30".parse() {
                res.headers_mut().insert(header::RETRY_AFTER, v);
            }
            res
        }
        Err(e) => WireError::from(e).into_response(),
    }
}

// ── audio ─────────────────────────────────────────────────────────────

async fn read_audio(State(state): State<AppState>, Path(meeting_id): Path<MeetingId>) -> Response {
    let path = match state.orchestrator.audio_path(&meeting_id).await {
        Ok(p) => p,
        Err(e) => return WireError::from(e).into_response(),
    };
    // Stream the m4a as a chunked body instead of `tokio::fs::read`-
    // ing the whole file into memory — m4a sidecars can be hundreds
    // of MB for long meetings, and an OOM here is a denial-of-
    // service against the daemon. `ReaderStream` reads in 8KiB
    // chunks (its default capacity), which is the right tradeoff
    // between syscall count and memory footprint for an audio
    // download. Byte-range + `ETag` + `If-None-Match` is a follow-
    // up; the `WireError` map already lines up with `416` / `423`
    // / `425` for when those land.
    let file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "audio read failed; redacting host path from response"
            );
            return WireError::new(
                "NotFound",
                "HERON_E_NOT_FOUND",
                StatusCode::NOT_FOUND,
                "audio not readable",
            )
            .into_response();
        }
    };
    let content_length = file.metadata().await.ok().map(|m| m.len());
    let stream = tokio_util::io::ReaderStream::new(file);
    let mut res = Response::new(Body::from_stream(stream));
    if let Ok(v) = "audio/mp4".parse() {
        res.headers_mut().insert(header::CONTENT_TYPE, v);
    }
    if let Some(len) = content_length
        && let Ok(v) = len.to_string().parse()
    {
        res.headers_mut().insert(header::CONTENT_LENGTH, v);
    }
    res
}

// ── calendar ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
pub struct CalendarParams {
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub limit: Option<u32>,
}

#[derive(serde::Serialize)]
struct CalendarPage {
    items: Vec<CalendarEvent>,
}

async fn list_upcoming_calendar(
    State(state): State<AppState>,
    Query(p): Query<CalendarParams>,
) -> Response {
    match state
        .orchestrator
        .list_upcoming_calendar(p.from, p.to, p.limit)
        .await
    {
        Ok(items) => Json(CalendarPage { items }).into_response(),
        Err(e) => WireError::from(e).into_response(),
    }
}

// ── pre-meeting context ───────────────────────────────────────────────

async fn attach_context(
    State(state): State<AppState>,
    Json(req): Json<PreMeetingContextRequest>,
) -> Response {
    match state.orchestrator.attach_context(req).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => WireError::from(e).into_response(),
    }
}
