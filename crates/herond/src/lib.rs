//! `herond` — localhost HTTP daemon, vertical slice.
//!
//! A thin axum app that projects [`heron_session::SessionOrchestrator`]
//! over the wire format pinned in
//! [`docs/api-desktop-openapi.yaml`](../../../docs/api-desktop-openapi.yaml).
//!
//! Scope of this crate:
//! - `/health` — full implementation (no auth required, per spec).
//! - `/events` — SSE projection of the orchestrator's event bus, with
//!   15s heartbeat keep-alive and `Last-Event-ID` / `?since_event_id`
//!   resume against [`heron_event::ReplayCache`].
//! - Bearer auth from `~/.heron/cli-token` (skipped for `/health`).
//! - CORS denial: any request carrying an `Origin` header is rejected
//!   so the daemon doesn't accidentally answer browser-side `fetch()`.
//! - Meeting, transcript, summary, audio, calendar, and context routes
//!   are thin projections over the injected orchestrator.
//!
//! The orchestrator is supplied by the caller via [`AppState`]; the
//! production binary plugs in
//! [`heron_orchestrator::LocalSessionOrchestrator`]. Tests can supply
//! [`stub::StubOrchestrator`] or narrower fakes when they only need to
//! exercise router/auth/error mapping.

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

/// API version prefix. The OpenAPI server URL is
/// `http://127.0.0.1:7384/v1`, so every endpoint listed under
/// `paths:` is mounted under this prefix in the actual router.
/// Pull-out as a const so a future v2 (`api-bot-openapi.yaml`)
/// nest can use a different one without searching for string
/// literals.
pub const API_PREFIX: &str = "/v1";

/// Build the axum app. Used by `main.rs` to bind, and by the
/// integration tests to drive requests in-process via
/// `tower::ServiceExt::oneshot` — no port binding required.
///
/// Middleware ordering note: axum's `.layer()` is "last-added is
/// outermost", so to make `Origin`-rejection pre-empt
/// bearer-auth (the security expectation: a hostile browser page
/// gets a 403 before the daemon even considers its credentials),
/// `require_bearer_except_health` is added FIRST and
/// `reject_browser_origin` is added LAST.
pub fn build_app(state: AppState) -> Router {
    let v1 = Router::new()
        .merge(routes::health::router())
        .merge(routes::events::router())
        .merge(routes::meetings::router());
    Router::new()
        .nest(API_PREFIX, v1)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_bearer_except_health,
        ))
        .layer(axum::middleware::from_fn(auth::reject_browser_origin))
        .with_state(state)
}
