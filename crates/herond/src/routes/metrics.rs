//! `GET /v1/__metrics` — Prometheus exposition for local inspection.
//!
//! Renders the snapshot held by [`heron_metrics::MetricsHandle`] in
//! the standard text exposition format (Content-Type:
//! `text/plain; version=0.0.4`). Bearer-auth-gated like every other
//! non-`/health` route — the endpoint is reachable only with the
//! `~/.heron/cli-token` value.
//!
//! The path uses a leading `__` to signal "internal / debug" without
//! conflicting with the public `/v1/meetings*` surface in
//! `api-desktop-openapi.yaml`. It is intentionally NOT in the
//! OpenAPI: the wire shape is Prometheus exposition, not JSON, and
//! we don't want client SDKs treating it as a stable contract.
//!
//! See `docs/observability.md` for the curl invocation.

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;

use crate::AppState;

/// Prometheus text exposition Content-Type (per
/// <https://prometheus.io/docs/instrumenting/exposition_formats/#text-based-format>).
const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

pub fn router() -> Router<AppState> {
    Router::new().route("/__metrics", get(get_metrics))
}

async fn get_metrics(State(state): State<AppState>) -> impl IntoResponse {
    let body = state.metrics.render();
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, HeaderValue::from_static(CONTENT_TYPE))],
        body,
    )
}
