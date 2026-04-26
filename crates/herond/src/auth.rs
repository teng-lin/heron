//! Bearer auth + browser-origin denial.
//!
//! Two cross-cutting middlewares applied in [`crate::build_app`]:
//!
//! 1. [`reject_browser_origin`] — any request with an `Origin` header
//!    is rejected with `403`. The OpenAPI `info.description` is
//!    explicit: browser-style consumers must use a non-`fetch`
//!    transport. Denying here keeps a malicious page on the same
//!    machine from coaxing the daemon into doing work.
//! 2. [`require_bearer_except_health`] — every path other than
//!    `/health` must carry `Authorization: Bearer <token>` matching
//!    the configured value. `/health` carries `security: []` per the
//!    OpenAPI so a liveness probe doesn't need credentials.

use std::path::PathBuf;

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::AppState;
use crate::error::WireError;

/// Configuration for the bearer-token check. The token is read once
/// at daemon startup; rotation is "delete the file and restart" per
/// the OpenAPI `securitySchemes.bearerAuth.description`.
#[derive(Debug, Clone)]
pub struct AuthConfig {
    pub bearer: String,
}

/// Errors surfaced when loading / minting the token file.
#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("could not resolve home directory")]
    NoHome,
    #[error("token file io: {0}")]
    Io(#[from] std::io::Error),
    #[error("token file is empty: {path}")]
    Empty { path: String },
}

/// Default location of the token file: `~/.heron/cli-token`. The
/// production binary uses this; tests bypass it entirely by
/// constructing [`AuthConfig`] directly.
pub fn default_token_path() -> Result<PathBuf, TokenError> {
    let home = std::env::var_os("HOME").ok_or(TokenError::NoHome)?;
    let mut path = PathBuf::from(home);
    path.push(".heron");
    path.push("cli-token");
    Ok(path)
}

/// Load the bearer token from `path`, minting a fresh one if absent.
/// Mints a UUIDv7 (so the token is unguessable but log-greppable as
/// a heron-shaped ID) and writes it with mode 0600. Newline-trimmed
/// on read so a `printf` vs `echo` round-trip doesn't shift the
/// comparison.
pub fn load_or_mint(path: &std::path::Path) -> Result<AuthConfig, TokenError> {
    if path.exists() {
        let raw = std::fs::read_to_string(path)?;
        let bearer = raw.trim().to_owned();
        if bearer.is_empty() {
            return Err(TokenError::Empty {
                path: path.display().to_string(),
            });
        }
        return Ok(AuthConfig { bearer });
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bearer = uuid::Uuid::now_v7().to_string();
    write_with_mode_0600(path, &bearer)?;
    Ok(AuthConfig { bearer })
}

#[cfg(unix)]
fn write_with_mode_0600(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(contents.as_bytes())?;
    Ok(())
}

#[cfg(not(unix))]
fn write_with_mode_0600(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    // Non-unix: fall back to default perms. The daemon is
    // macOS-targeted and CI runs on linux/macos, but a Windows test
    // runner shouldn't fail the build.
    std::fs::write(path, contents)
}

/// Reject any request with an `Origin` header. The OpenAPI denies
/// CORS by default; this middleware enforces it at the daemon edge
/// rather than relying on tower-http's CORS layer (which is set up
/// for permitting origins, not denying them).
pub async fn reject_browser_origin(req: Request<Body>, next: Next) -> Response {
    if req.headers().contains_key(header::ORIGIN) {
        return WireError::new(
            "OriginDenied",
            "HERON_E_ORIGIN_DENIED",
            StatusCode::FORBIDDEN,
            "browser-origin requests are denied; use a non-fetch transport",
        )
        .into_response();
    }
    next.run(req).await
}

/// Bearer-token check. `/health` is allowlisted because the OpenAPI
/// declares it `security: []`. Constant-time string comparison would
/// be belt-and-suspenders here — the token is a 36-byte UUID so a
/// timing oracle leaks at most the prefix length and the daemon is
/// localhost-only — but we still use a length-then-bytes compare so
/// the trivial mismatches short-circuit fast without leaking length.
pub async fn require_bearer_except_health(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    if req.uri().path() == "/health" {
        return next.run(req).await;
    }

    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));

    let ok = match presented {
        Some(token) => bearer_eq(token, &state.auth.bearer),
        None => false,
    };

    if !ok {
        return WireError::new(
            "Unauthorized",
            "HERON_E_UNAUTHORIZED",
            StatusCode::UNAUTHORIZED,
            "bearer token missing or invalid",
        )
        .into_response();
    }
    next.run(req).await
}

/// Compare two bearer tokens. Length-first short-circuit (a wrong
/// token of the wrong length doesn't proceed to the byte loop), then
/// constant-time-over-equal-length comparison so timing doesn't leak
/// where the mismatch is.
fn bearer_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.as_bytes().iter().zip(b.as_bytes().iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn bearer_eq_handles_length_mismatch_and_constant_time_paths() {
        assert!(bearer_eq("abc", "abc"));
        assert!(!bearer_eq("abc", "abcd"));
        assert!(!bearer_eq("abc", "abd"));
        assert!(bearer_eq("", ""));
    }

    #[test]
    fn load_or_mint_creates_token_and_re_reads_same_value() {
        let dir = tempdir();
        let path = dir.join("cli-token");
        let first = load_or_mint(&path).expect("mint");
        let again = load_or_mint(&path).expect("re-read");
        assert_eq!(first.bearer, again.bearer);
        assert!(!first.bearer.is_empty());
        // Re-read must not mutate the file.
        let on_disk = std::fs::read_to_string(&path)
            .expect("read back")
            .trim()
            .to_owned();
        assert_eq!(on_disk, first.bearer);
    }

    #[test]
    fn load_or_mint_rejects_empty_token_file() {
        let dir = tempdir();
        let path = dir.join("cli-token");
        std::fs::write(&path, "\n").expect("write empty");
        let err = load_or_mint(&path).expect_err("should reject empty");
        assert!(matches!(err, TokenError::Empty { .. }));
    }

    fn tempdir() -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("herond-auth-test-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&p).expect("mkdir");
        p
    }
}
