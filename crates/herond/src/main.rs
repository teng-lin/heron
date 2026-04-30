//! `herond` daemon binary entry point.
//!
//! Loads (or mints) the bearer token, builds an [`AppState`] backed
//! by [`heron_orchestrator::LocalSessionOrchestrator`], binds the
//! OpenAPI-pinned `127.0.0.1:7384`, and serves until SIGINT.
//!
//! The orchestrator brings a real bus + replay cache (from
//! `heron-event-http`) and implements manual capture, pre-meeting
//! context staging, calendar reads, and vault-backed meeting reads
//! when a vault root is configured. The `/events` SSE
//! `Last-Event-ID` resume contract is live end-to-end for every
//! event the orchestrator publishes.
//!
//! Vault root resolution: the `HERON_VAULT_ROOT` env var wins;
//! falls back to `~/heron-vault`.

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
        "wiring LocalSessionOrchestrator"
    );
    let orchestrator = Arc::new(LocalSessionOrchestrator::with_vault(vault_root));
    std::mem::drop(orchestrator.spawn_auto_record_scheduler());

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
/// default. We don't `mkdir` here; an absent vault is reported as a
/// down vault component on `/health`, and `list_meetings` returns an
/// empty page.
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
