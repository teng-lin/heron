//! `GET /health` — liveness + capability check.
//!
//! Per the OpenAPI: `security: []` (no bearer required). The auth
//! middleware allowlists this path; nothing else needs special-casing
//! here.

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::routing::get;
use heron_session::Health;

use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/health", get(get_health))
}

async fn get_health(State(state): State<AppState>) -> Json<Health> {
    Json(state.orchestrator.health().await)
}
