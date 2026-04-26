//! `herond` — localhost HTTP daemon, vertical slice.
//!
//! A thin axum app that projects [`heron_session::SessionOrchestrator`]
//! over the wire format pinned in
//! [`docs/api-desktop-openapi.yaml`](../../../docs/api-desktop-openapi.yaml).
//!
//! Scope of this slice:
//! - `/health` — full implementation (no auth required, per spec).
//! - `/events` — SSE projection of the orchestrator's event bus, with
//!   15s heartbeat keep-alive and `Last-Event-ID` / `?since_event_id`
//!   resume against [`heron_event::ReplayCache`].
//! - Bearer auth from `~/.heron/cli-token` (skipped for `/health`).
//! - CORS denial: any request carrying an `Origin` header is rejected
//!   so the daemon doesn't accidentally answer browser-side `fetch()`.
//! - Every other endpoint in the OpenAPI returns the
//!   `HERON_E_NOT_YET_IMPLEMENTED` envelope at `501`. The routing
//!   surface stays complete; the bodies are honest about what's not
//!   wired yet.
//!
//! The orchestrator is supplied by the caller via [`AppState`]; the
//! production binary plugs in [`stub::StubOrchestrator`] until the
//! `LocalSessionOrchestrator` consolidation lands. Tests supply their
//! own — that's the whole point of the trait-driven design.

use std::sync::Arc;

use axum::Router;
use heron_session::SessionOrchestrator;

pub mod auth;
pub mod error;
pub mod routes;
pub mod stub;

pub use auth::AuthConfig;
pub use error::{WireError, status_for};

/// Default localhost bind for the daemon. Pinned by the OpenAPI
/// `servers[0].url`; v1 is localhost-only and any networked mode
/// requires a separate auth model (mTLS / pairing).
pub const DEFAULT_BIND: &str = "127.0.0.1:7384";

/// Shared state injected into every handler. Cheap to clone — both
/// fields are `Arc`-backed.
#[derive(Clone)]
pub struct AppState {
    /// The orchestrator the daemon projects. `Arc<dyn …>` so the same
    /// state can hold a stub, a real `LocalSessionOrchestrator`, or a
    /// test fake without recompiling the router.
    pub orchestrator: Arc<dyn SessionOrchestrator>,
    /// Bearer-token config. Held in an `Arc` so the auth middleware
    /// can borrow without forcing every handler to clone the token
    /// string.
    pub auth: Arc<AuthConfig>,
}

/// Build the axum app. Used by `main.rs` to bind, and by the
/// integration tests to drive requests in-process via
/// `tower::ServiceExt::oneshot` — no port binding required.
pub fn build_app(state: AppState) -> Router {
    Router::new()
        .merge(routes::health::router())
        .merge(routes::events::router())
        .merge(routes::unimpl::router())
        .layer(axum::middleware::from_fn(auth::reject_browser_origin))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_bearer_except_health,
        ))
        .with_state(state)
}
