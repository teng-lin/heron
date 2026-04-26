//! `herond` daemon binary entry point.
//!
//! Loads (or mints) the bearer token, builds an [`AppState`] backed
//! by [`heron_orchestrator::LocalSessionOrchestrator`] (read-side
//! fully wired against the user's vault on disk; capture-lifecycle
//! endpoints still 501 until the FSM-merge PR lands), binds the
//! OpenAPI-pinned `127.0.0.1:7384`, and serves until SIGINT.
//!
//! The orchestrator brings a real bus + replay cache (from
//! `heron-event-http`) — the SSE `Last-Event-ID` resume contract is
//! live end-to-end as soon as any future publisher exists. The
//! capture-lifecycle methods (`start_capture`, `end_meeting`,
//! `attach_context`) still return `NotYetImplemented` until the
//! FSM-merge wires the heron-cli session driver into this trait.
//!
//! Vault root resolution: the `HERON_VAULT_ROOT` env var wins;
//! falls back to `~/heron-vault`. The directory is created lazily
//! by the vault writer when the FSM-merge PR adds capture; this
//! binary just reads.

use std::path::PathBuf;
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

    let vault_root = resolve_vault_root().context("resolving vault root")?;
    tracing::info!(
        vault_root = %vault_root.display(),
        "wiring LocalSessionOrchestrator (read-side; capture-lifecycle still 501 until FSM-merge)"
    );
    // `LocalSessionOrchestrator::with_vault` spawns the bus → cache
    // recorder task; it must run inside the `#[tokio::main]`
    // runtime, which we're already in here.
    let orchestrator = Arc::new(LocalSessionOrchestrator::with_vault(vault_root));

    let state = AppState {
        orchestrator,
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

/// Vault root precedence: `HERON_VAULT_ROOT` env var > `~/heron-vault`
/// default. We don't `mkdir` here; an absent vault is reported as
/// `permission_missing` on `/health` and `list_meetings` returns an
/// empty page, which is the right signal to a freshly-installed
/// daemon's first liveness probe.
fn resolve_vault_root() -> Result<PathBuf> {
    // Treat an empty / whitespace-only `HERON_VAULT_ROOT` as unset.
    // The naïve `PathBuf::from("")` resolves to the current working
    // directory at runtime — fine if you launched the daemon from
    // your vault, terrible if you launched it from a random
    // checkout. Failing closed (use the `~/heron-vault` default)
    // is the safe pick.
    if let Ok(s) = std::env::var("HERON_VAULT_ROOT") {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return Ok(PathBuf::from(trimmed));
        }
    }
    let home = dirs::home_dir().context("home directory not resolvable")?;
    Ok(home.join("heron-vault"))
}
