//! `herond` daemon binary entry point.
//!
//! Loads (or mints) the bearer token, builds the
//! [`LocalSessionOrchestrator`]-backed [`AppState`], binds the
//! OpenAPI-pinned `127.0.0.1:7384`, and serves until SIGINT.
//!
//! The orchestrator brings a real bus + replay cache (from
//! `heron-event-http`) — the SSE `Last-Event-ID` resume contract is
//! live end-to-end as soon as any future publisher exists. The FSM
//! methods (`start_capture`, `end_meeting`, transcript / summary /
//! audio reads, calendar, context) still return
//! `NotYetImplemented`; those land one PR at a time as the
//! underlying subsystems wire in.

use std::sync::Arc;

use anyhow::{Context, Result};
use heron_orchestrator::LocalSessionOrchestrator;
use herond::{AppState, AuthConfig, DEFAULT_BIND, build_app};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,herond=debug")),
        )
        .init();

    let token_path =
        herond::auth::default_token_path().context("resolving ~/.heron/cli-token path")?;
    let auth: AuthConfig = herond::auth::load_or_mint(&token_path)
        .with_context(|| format!("loading bearer token from {}", token_path.display()))?;
    tracing::info!(
        token_path = %token_path.display(),
        "bearer token loaded; rotate by deleting the file and restarting"
    );

    // `LocalSessionOrchestrator::new` spawns the bus → cache
    // recorder task; it must run inside the `#[tokio::main]`
    // runtime, which we're already in here.
    let state = AppState {
        orchestrator: Arc::new(LocalSessionOrchestrator::new()),
        auth: Arc::new(auth),
    };
    let app = build_app(state);

    let listener = tokio::net::TcpListener::bind(DEFAULT_BIND)
        .await
        .with_context(|| format!("binding {DEFAULT_BIND}"))?;
    tracing::info!(bind = %DEFAULT_BIND, "herond listening (localhost-only; v1 declines networked binds)");
    axum::serve(listener, app).await.context("axum::serve")?;
    Ok(())
}
