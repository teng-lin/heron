//! Catch-all routes for OpenAPI endpoints not yet wired in the
//! vertical slice.
//!
//! Each path mounts a handler that returns the `HERON_E_NOT_YET_IMPLEMENTED`
//! envelope at HTTP 501. The routing surface stays complete — a
//! consumer hitting `/meetings/mtg_xyz` gets a structured rejection
//! that names what's missing, not a 404 that pretends the endpoint
//! doesn't exist.
//!
//! When the orchestrator consolidation lands, each handler here gets
//! replaced by a real one (or moved into per-endpoint files); the
//! routing list is the migration checklist.

use axum::Router;
use axum::routing::{get, post, put};
use heron_session::SessionError;

use crate::AppState;
use crate::error::WireError;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/meetings", get(stub).post(stub))
        .route("/meetings/{meeting_id}", get(stub))
        .route("/meetings/{meeting_id}/end", post(stub))
        .route("/meetings/{meeting_id}/transcript", get(stub))
        .route("/meetings/{meeting_id}/summary", get(stub))
        .route("/meetings/{meeting_id}/audio", get(stub))
        .route("/calendar/upcoming", get(stub))
        .route("/context", put(stub))
}

/// Single body for every unimplemented route. `WireError::from(SessionError)`
/// already maps `NotYetImplemented` to status 501 with the
/// `HERON_E_NOT_YET_IMPLEMENTED` code, so there's nothing endpoint-
/// specific to thread through.
async fn stub() -> WireError {
    WireError::from(SessionError::NotYetImplemented)
}
