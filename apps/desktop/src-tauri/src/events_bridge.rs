//! Tauri-side SSE bridge.
//!
//! The daemon publishes events on `GET /v1/events` as a Server-Sent
//! Events stream. The webview cannot connect directly:
//!
//! 1. `EventSource` cannot send `Authorization` headers (per MDN's
//!    constructor), and the daemon's `require_bearer_except_health`
//!    middleware rejects unauthenticated requests
//!    (`crates/herond/src/auth.rs:10`).
//! 2. The daemon also rejects any request carrying an `Origin`
//!    header (`crates/herond/src/auth.rs:164`); webview HTTP requests
//!    always set one.
//! 3. The Tauri CSP at `apps/desktop/src-tauri/tauri.conf.json:24`
//!    inherits `default-src 'self'` for `connect-src`, blocking the
//!    webview from reaching `127.0.0.1:7384` outright.
//!
//! Routing the connection through Rust sidesteps all three:
//! `reqwest` doesn't add an `Origin` by default, the bearer attaches
//! via `bearer_auth`, and the IPC sink delivers events to the
//! webview as Tauri events that aren't subject to CSP.
//!
//! ## Singleton lifecycle
//!
//! One bridge task per app process. Multiple React subscribers
//! (Recording.tsx, Review.tsx, the chrome's REC pill) all listen to
//! the same `heron://event` Tauri event the bridge emits — so the
//! bridge is **app-lifetime**, not component-lifetime. Setting it up
//! is idempotent: the first call to `start` spawns; later calls see
//! `Some(handle)` and return immediately.
//!
//! Cancellation fires from `tauri::RunEvent::Exit` via
//! [`SseBridge::shutdown`]. Without the explicit shutdown the tokio
//! task would survive Tauri's runtime teardown (briefly) and the
//! axum graceful-shutdown path on the in-process daemon would block
//! waiting for the streaming response to drain.
//!
//! ## Reconnect policy
//!
//! On a stream-side error (network blip, daemon restart) the task
//! sleeps with capped exponential backoff (1 s → 5 s → 30 s),
//! re-issues the GET with `?since_event_id=<last_seen>`, and
//! resumes. The daemon's replay cache covers up to the
//! `X-Heron-Replay-Window-Seconds` advertised on connect.
//!
//! On a 401 the task does NOT reconnect — the bearer rotated mid-
//! stream, and silently retrying would mask the auth failure. The
//! frontend's daemon-down banner takes over.

use std::sync::Mutex;
use std::time::Duration;

use tauri::async_runtime::JoinHandle;
use tauri::{AppHandle, Emitter, Manager, Runtime, State};
use tokio::sync::oneshot;

use crate::daemon::DaemonHandle;

/// Loopback URL for the daemon's SSE stream. Same hardcoding policy
/// as [`crate::daemon::HEALTH_URL`] / `crate::meetings::BASE_URL`:
/// the URL is not renderer-supplied so an attacker-controlled webview
/// cannot fabricate a target.
const EVENTS_URL: &str = "http://127.0.0.1:7384/v1/events";

/// Tauri event name the bridge emits envelopes on. The frontend
/// `useSseEvents` hook listens via
/// `@tauri-apps/api/event::listen("heron://event", ...)`.
pub const FRONTEND_EVENT: &str = "heron://event";

/// Initial reconnect delay. Doubles up to [`MAX_BACKOFF`] on
/// successive failures.
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);

/// Cap on the reconnect delay. 30 s matches the daemon's heartbeat
/// cadence — a wedged daemon shouldn't get hammered.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Per-connect read timeout. Generous because SSE is long-poll —
/// idle connections are normal. We rely on the daemon's heartbeats
/// (every 15 s per `crates/herond/src/routes/events.rs:78`) to keep
/// the stream warm; if 60 s passes with no bytes at all, treat the
/// connection as dead and reconnect.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// State stashed in Tauri's manager for the lifetime of the app.
/// The mutex guards the "is the bridge running?" flag (the join
/// handle) and the cancellation channel together so a parallel
/// [`start`] / [`shutdown`] can't race.
#[derive(Default)]
pub struct SseBridge {
    inner: Mutex<BridgeState>,
}

#[derive(Default)]
struct BridgeState {
    handle: Option<JoinHandle<()>>,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl SseBridge {
    /// Spawn the bridge task if it isn't already running.
    /// Idempotent — concurrent or repeated calls return Ok and are
    /// no-ops once the task is up.
    fn start<R: Runtime>(&self, app: AppHandle<R>, bearer: String) {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        // Idempotent on a *live* task. A previous task that returned
        // (401, build-error, panic) leaves `handle = Some(_)` because
        // we don't drive the JoinHandle. Without `is_finished()` here,
        // a frontend retry — e.g. the user fixes auth and clicks the
        // daemon-down banner's Retry — would silently no-op forever.
        if guard
            .handle
            .as_ref()
            .is_some_and(|h| !h.inner().is_finished())
        {
            return;
        }
        // Stale handle from a finished task, or never set. Drop it
        // along with any orphan shutdown sender before spawning fresh.
        guard.handle = None;
        guard.shutdown_tx = None;
        let (tx, rx) = oneshot::channel::<()>();
        let app_for_task = app.clone();
        let task = tauri::async_runtime::spawn(async move {
            run_loop(app_for_task, bearer, rx).await;
        });
        guard.handle = Some(task);
        guard.shutdown_tx = Some(tx);
    }

    /// Best-effort shutdown. Idempotent.
    pub fn shutdown(&self) {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Some(tx) = guard.shutdown_tx.take() {
            // Receiver dropping (task already exited) is a benign Err.
            let _ = tx.send(());
        }
        // We don't await the JoinHandle here: Tauri's Exit hook is
        // sync and the runtime tears down anyway. Drop the handle so
        // a subsequent `start` (e.g. in a test) can spawn a fresh
        // task.
        guard.handle = None;
    }
}

/// Tauri command: ensure the SSE bridge is running.
///
/// The frontend calls this once on app mount via the `useSseEvents`
/// hook. Multiple subscribers share the same bridge — the frontend
/// hook listens on the bus directly via `@tauri-apps/api/event`, no
/// per-component subscription is required server-side.
#[tauri::command]
pub fn heron_subscribe_events<R: Runtime>(
    app: AppHandle<R>,
    daemon: State<'_, DaemonHandle>,
    bridge: State<'_, SseBridge>,
) -> Result<(), String> {
    bridge.start(app, daemon.auth.bearer.clone());
    Ok(())
}

/// Tauri command: cancel the SSE bridge.
///
/// Called from `tauri::RunEvent::Exit`. The frontend doesn't
/// generally call it on component unmount because the bridge is app-
/// lifetime, not component-lifetime.
#[tauri::command]
pub fn heron_unsubscribe_events(bridge: State<'_, SseBridge>) -> Result<(), String> {
    bridge.shutdown();
    Ok(())
}

/// Reconnect loop. Owns the lifecycle of one streaming reqwest call;
/// on error it backs off and re-issues with the last seen
/// `event_id` as the replay cursor.
async fn run_loop<R: Runtime>(
    app: AppHandle<R>,
    bearer: String,
    mut shutdown_rx: oneshot::Receiver<()>,
) {
    let mut last_event_id: Option<String> = None;
    let mut backoff = INITIAL_BACKOFF;

    loop {
        // Fast-fail check — if shutdown fired between iterations,
        // bail before opening another connection.
        if shutdown_rx.try_recv().is_ok() {
            return;
        }

        let cursor = last_event_id.clone();
        let outcome = run_once(
            EVENTS_URL,
            &bearer,
            cursor.as_deref(),
            &app,
            &mut last_event_id,
            &mut shutdown_rx,
        )
        .await;

        match outcome {
            ConnectOutcome::Shutdown => return,
            ConnectOutcome::AuthFailed => {
                // Bearer rotated — silently retrying would mask the
                // auth failure. Stop the loop; the frontend's
                // daemon-down banner takes over and the user can
                // restart the app to re-authenticate.
                tracing::warn!(
                    "SSE bridge: bearer rotated mid-stream (401). Stopping; user must restart."
                );
                return;
            }
            ConnectOutcome::Reconnect => {
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = &mut shutdown_rx => return,
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
            ConnectOutcome::ResetBackoff => {
                // We received at least one event before the stream
                // closed — reset the backoff so a transient blip
                // doesn't push us into the 30 s cap.
                backoff = INITIAL_BACKOFF;
            }
        }
    }
}

#[derive(Debug)]
enum ConnectOutcome {
    Shutdown,
    AuthFailed,
    Reconnect,
    ResetBackoff,
}

/// One pass of the SSE connection. Returns when either the stream
/// closes (graceful or error), the daemon returns 401, or the
/// shutdown channel fires.
async fn run_once<R: Runtime>(
    base_url: &str,
    bearer: &str,
    since_event_id: Option<&str>,
    app: &AppHandle<R>,
    last_event_id: &mut Option<String>,
    shutdown_rx: &mut oneshot::Receiver<()>,
) -> ConnectOutcome {
    let client = match reqwest::Client::builder()
        .read_timeout(READ_TIMEOUT)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "SSE bridge: client build failed");
            return ConnectOutcome::Reconnect;
        }
    };
    let mut request = client.get(base_url).bearer_auth(bearer);
    if let Some(cursor) = since_event_id {
        request = request.query(&[("since_event_id", cursor)]);
    }
    let response = match request.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(error = %e, "SSE bridge: connect failed");
            return ConnectOutcome::Reconnect;
        }
    };
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        return ConnectOutcome::AuthFailed;
    }
    if !response.status().is_success() {
        tracing::warn!(status = %response.status(), "SSE bridge: non-success status");
        return ConnectOutcome::Reconnect;
    }

    let mut received_any = false;
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();

    use futures_util::StreamExt;
    loop {
        tokio::select! {
            chunk = stream.next() => {
                match chunk {
                    Some(Ok(bytes)) => {
                        buffer.push_str(&String::from_utf8_lossy(&bytes));
                        while let Some((idx, term_len)) = find_event_terminator(&buffer) {
                            let frame = buffer[..idx].to_string();
                            buffer.drain(..idx + term_len);
                            if let Some(envelope) = parse_event_frame(&frame) {
                                if let Some(id) = json_event_id(&envelope) {
                                    *last_event_id = Some(id);
                                }
                                if let Err(e) = app.emit(FRONTEND_EVENT, envelope) {
                                    tracing::warn!(error = %e, "SSE bridge: frontend emit failed");
                                }
                                received_any = true;
                            }
                        }
                    }
                    Some(Err(e)) => {
                        tracing::debug!(error = %e, "SSE bridge: stream error");
                        return if received_any {
                            ConnectOutcome::ResetBackoff
                        } else {
                            ConnectOutcome::Reconnect
                        };
                    }
                    None => {
                        // Stream closed cleanly. Treat as transient
                        // (the daemon may have been restarted) and
                        // reconnect.
                        return if received_any {
                            ConnectOutcome::ResetBackoff
                        } else {
                            ConnectOutcome::Reconnect
                        };
                    }
                }
            }
            _ = &mut *shutdown_rx => {
                return ConnectOutcome::Shutdown;
            }
        }
    }
}

/// Find the next event terminator in the buffer. Returns `(idx,
/// terminator_len)` so the caller can drain `idx + terminator_len`
/// without ambiguity — `\n\n` is 2 bytes, `\r\n\r\n` is 4. The
/// previous "drop 2" form left `\r\n` stuck at the front of the
/// buffer when the daemon (or an intermediate proxy) emitted CRLF.
fn find_event_terminator(buf: &str) -> Option<(usize, usize)> {
    let lf = buf.find("\n\n");
    let crlf = buf.find("\r\n\r\n");
    match (lf, crlf) {
        (Some(l), Some(c)) if l <= c => Some((l, 2)),
        (Some(_), Some(c)) => Some((c, 4)),
        (Some(l), None) => Some((l, 2)),
        (None, Some(c)) => Some((c, 4)),
        (None, None) => None,
    }
}

/// Parse one SSE frame into a JSON value. The daemon emits frames
/// with `id: <event_id>` and `data: <json>` lines per
/// `crates/herond/src/routes/events.rs`. We only care about the
/// `data:` line for the envelope payload — the `id:` is also
/// embedded inside the JSON envelope, so parsing it from the
/// metadata line is redundant.
fn parse_event_frame(frame: &str) -> Option<serde_json::Value> {
    let mut data_lines: Vec<&str> = Vec::new();
    for line in frame.lines() {
        // Heartbeat comments start with ":"; skip.
        if line.starts_with(':') {
            continue;
        }
        // Field lines are `<field>: <value>`. We only care about
        // `data` for the JSON envelope.
        if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim_start());
        }
    }
    if data_lines.is_empty() {
        return None;
    }
    let combined = data_lines.join("\n");
    serde_json::from_str(&combined).ok()
}

/// Pull the `event_id` field out of a parsed envelope so the
/// reconnect loop can replay from there. Returns `None` if the
/// envelope shape is unexpected — the next connect will start
/// without a cursor and the daemon will replay from the head of
/// its window.
fn json_event_id(value: &serde_json::Value) -> Option<String> {
    value
        .get("event_id")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
}

/// Test helper: parse one SSE frame as if the bridge had received
/// it. Exposed so the unit tests can pin the parser without
/// spinning up a server.
#[cfg(test)]
fn parse_for_test(frame: &str) -> Option<serde_json::Value> {
    parse_event_frame(frame)
}

/// Wire a [`SseBridge`] into the running Tauri app. Called from
/// `lib::run`'s setup hook after [`DaemonHandle`] is in place.
pub fn install<R: Runtime>(app: &AppHandle<R>) {
    if !app.manage(SseBridge::default()) {
        tracing::warn!("SseBridge already managed");
    }
}

/// Convenience: pull the bridge out of Tauri state and shut it down.
/// Used from the Exit hook.
pub fn shutdown_from_state<R: Runtime>(app: &AppHandle<R>) {
    if let Some(bridge) = app.try_state::<SseBridge>() {
        bridge.shutdown();
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_event_frame_extracts_data() {
        let frame =
            "id: evt_001\nevent: meeting.started\ndata: {\"event_id\":\"evt_001\",\"foo\":\"bar\"}";
        let v = parse_for_test(frame).expect("parsed");
        assert_eq!(v["event_id"], "evt_001");
        assert_eq!(v["foo"], "bar");
    }

    #[test]
    fn parse_event_frame_skips_heartbeat_comments() {
        let frame = ": ping\n";
        assert!(parse_for_test(frame).is_none());
    }

    #[test]
    fn parse_event_frame_joins_multiline_data() {
        // SSE spec: multiple `data:` lines get joined with `\n` to
        // form the field value. The daemon emits single-line JSON,
        // but tolerate the multi-line form so a future pretty-print
        // doesn't break the parser.
        let frame = "data: {\"event_id\":\"evt_002\",\ndata: \"key\":\"value\"}";
        let v = parse_for_test(frame).expect("parsed");
        assert_eq!(v["event_id"], "evt_002");
    }

    #[test]
    fn find_event_terminator_returns_lf_index() {
        let buf = "id: evt_001\ndata: {}\n\nleftover";
        let (idx, len) = find_event_terminator(buf).expect("terminator");
        assert_eq!(&buf[..idx], "id: evt_001\ndata: {}");
        assert_eq!(len, 2);
    }

    #[test]
    fn find_event_terminator_handles_crlf() {
        let buf = "id: evt_001\r\ndata: {}\r\n\r\nleftover";
        let (idx, len) = find_event_terminator(buf).expect("terminator");
        assert!(buf[..idx].ends_with("data: {}"));
        assert_eq!(len, 4);
    }

    #[test]
    fn crlf_frame_followed_by_frame_drains_without_corruption() {
        // Two complete CRLF-terminated frames back-to-back. The
        // previous off-by-2 drain left `\r\n` stuck after frame 1,
        // which prefixed frame 2 with stray bytes and made
        // `find_event_terminator` return the wrong index for frame 2
        // (the residual `\r\n` plus the next CRLF is `\r\n\r\n`,
        // matching the wrong terminator earlier in the stream).
        let mut buf = String::from(
            "id: 1\r\ndata: {\"event_id\":\"a\"}\r\n\r\nid: 2\r\ndata: {\"event_id\":\"b\"}\r\n\r\n",
        );
        let mut frames = Vec::new();
        while let Some((idx, len)) = find_event_terminator(&buf) {
            let frame = buf[..idx].to_string();
            buf.drain(..idx + len);
            frames.push(frame);
        }
        assert_eq!(frames.len(), 2);
        assert!(frames[0].contains("\"event_id\":\"a\""));
        assert!(frames[1].starts_with("id: 2"));
        assert!(frames[1].contains("\"event_id\":\"b\""));
        assert!(buf.is_empty(), "buffer should be drained, got {buf:?}");
    }
}
