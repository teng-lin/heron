//! `heron-event-http` — HTTP/SSE projection building blocks for the
//! `/events` endpoint.
//!
//! Per `docs/archives/api-design-spec.md` Invariant 13, every transport is a
//! projection of the canonical [`heron_event`] bus. This crate ships
//! the building blocks the desktop daemon (`herond`) needs to project
//! the bus onto Server-Sent Events:
//!
//! - [`InMemoryReplayCache`] — bounded in-memory ring backing
//!   `Last-Event-ID` / `?since_event_id` resume on `/events`.
//! - [`SseEventSink`] — per-connection [`heron_event::EventSink`] that
//!   formats envelopes as SSE frames and writes them to a tokio mpsc
//!   channel the HTTP framework drains into the response body.
//! - [`format_sse_frame`] / [`heartbeat_frame`] — pure helpers for the
//!   wire format pinned by `docs/api-desktop-openapi.yaml`. Live as
//!   free functions so an HTTP handler can use them outside the
//!   `EventSink` trait (e.g. to serialize a single replay batch on
//!   the connection's first response).
//! - [`TopicFilter`] — comma-separated glob list matching the
//!   `?topics=` query.
//!
//! What lives in *this* crate is the bus → SSE-bytes adapter; what
//! lives in `herond` is the HTTP framework wiring (axum handler,
//! body-streaming impl, header serialization). Splitting at this seam
//! keeps the SSE format testable in isolation and lets a future MCP
//! or webhook projection reuse the same trait pattern without
//! inheriting an HTTP framework dependency.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use heron_event::{Envelope, EventId, EventSink, ReplayCache, ReplayError, SinkError};
use serde::Serialize;
use tokio::sync::mpsc;

// ── public constants ──────────────────────────────────────────────────

/// Response header name used by the `/events` endpoint to advertise
/// the replay window. Pinned in `docs/api-desktop-openapi.yaml`.
pub const REPLAY_WINDOW_HEADER: &str = "X-Heron-Replay-Window-Seconds";

/// Default replay window. Matches the OpenAPI default of 3600s; the
/// daemon may override per [`InMemoryReplayCache::with_window`].
pub const DEFAULT_REPLAY_WINDOW: Duration = Duration::from_secs(3600);

// ── replay cache ──────────────────────────────────────────────────────

/// In-memory bounded ring backing the SSE `Last-Event-ID` resume
/// contract.
///
/// Stores up to `capacity` envelopes; older entries are evicted FIFO
/// when capacity is reached or when their age exceeds the configured
/// retention window. The [`ReplayCache`] trait impl scans linearly —
/// O(n) per replay — which is fine for the desktop daemon's expected
/// load (single-digit subscribers, sub-second replay sizes). A
/// future server-side projection that needs sublinear lookup can
/// implement [`ReplayCache`] over a different backing structure
/// without touching anything in `heron-event`.
///
/// **Why a separate `record` API rather than auto-subscribing to a
/// bus.** The cache has no opinion on which bus it backs; the daemon
/// owns the subscription loop and pushes envelopes through `record`.
/// That keeps the cache reusable across `EventBus<EventPayload>`
/// (production) and `EventBus<TestPayload>` (tests) without a
/// runtime spawn.
pub struct InMemoryReplayCache<P: Clone + Send + 'static> {
    entries: Mutex<VecDeque<Entry<P>>>,
    window: Duration,
    capacity: usize,
}

struct Entry<P> {
    /// Monotonic timestamp at insertion. Used for window-based
    /// eviction. Deliberately not [`Envelope::created_at`]: a
    /// replayed-and-rebroadcast event could carry an arbitrary past
    /// time and trick the window into evicting fresh entries.
    /// `Instant` (monotonic) over `SystemTime` so a daylight-savings
    /// jump or NTP correction can't reshape the cache.
    recorded_at: Instant,
    envelope: Envelope<P>,
}

impl<P: Clone + Send + 'static> InMemoryReplayCache<P> {
    /// Construct a cache with the default 3600s window.
    ///
    /// # Panics
    ///
    /// Panics if `capacity == 0`. A zero-capacity cache is a misuse —
    /// every `record` would discard the new entry on the spot and
    /// every `replay_since` would 410-Gone. Surface the
    /// misconfiguration at construction rather than letting
    /// subscribers see silent data loss.
    pub fn new(capacity: usize) -> Self {
        Self::with_window(capacity, DEFAULT_REPLAY_WINDOW)
    }

    /// Construct a cache with a custom retention window. The window
    /// is exposed via [`ReplayCache::window`] so the HTTP layer can
    /// copy it into the [`REPLAY_WINDOW_HEADER`] response header.
    ///
    /// # Panics
    ///
    /// Panics if `capacity == 0` (see [`Self::new`]) or if `window`
    /// is zero. A zero window evicts every entry on the next
    /// `record`, making every `replay_since` `WindowExceeded` — the
    /// degenerate cache that fails closed for no reason. Surface the
    /// misconfiguration at construction.
    pub fn with_window(capacity: usize, window: Duration) -> Self {
        assert!(capacity > 0, "InMemoryReplayCache requires capacity > 0");
        assert!(!window.is_zero(), "InMemoryReplayCache requires window > 0");
        Self {
            entries: Mutex::new(VecDeque::with_capacity(capacity)),
            window,
            capacity,
        }
    }

    /// Record an envelope. The daemon's subscriber loop calls this
    /// once per bus event; ordering follows the bus order, which is
    /// the order [`format_sse_frame`] will emit on the wire.
    ///
    /// Eviction runs on every record: entries past the retention
    /// window are dropped, and if the resulting deque exceeds
    /// `capacity` the oldest entries are popped until it fits. The
    /// time-window evict happens first so a long-idle cache doesn't
    /// hold stale entries until capacity pressure forces them out.
    pub fn record(&self, envelope: Envelope<P>) {
        let now = Instant::now();
        let mut entries = lock_or_recover(&self.entries);
        evict_expired(&mut entries, now, self.window);
        while entries.len() >= self.capacity {
            entries.pop_front();
        }
        entries.push_back(Entry {
            recorded_at: now,
            envelope,
        });
    }

    /// Current number of entries — diagnostic only. Tests use it to
    /// pin eviction behaviour; production code should not branch on
    /// it.
    pub fn len(&self) -> usize {
        lock_or_recover(&self.entries).len()
    }

    /// `true` when the cache has no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drop every entry. Intended for the discontinuity-recovery
    /// path: when a bus subscriber driving this cache reports lag
    /// (e.g. `tokio::sync::broadcast::error::RecvError::Lagged`),
    /// a partial replay would silently hand a client events that
    /// skip the gap with no `WindowExceeded`. Clearing the cache
    /// downgrades every subsequent `replay_since` to
    /// `WindowExceeded` until fresh entries arrive — same recovery
    /// shape clients already handle (reconnect without resume).
    pub fn clear(&self) {
        lock_or_recover(&self.entries).clear();
    }
}

#[async_trait]
impl<P: Clone + Send + Sync + 'static> ReplayCache<P> for InMemoryReplayCache<P> {
    async fn replay_since(&self, since: EventId) -> Result<Vec<Envelope<P>>, ReplayError> {
        let mut entries = lock_or_recover(&self.entries);

        // Evict on read too, not just on `record`. An idle daemon
        // (no events flowing) would otherwise let an
        // older-than-window entry match a resume request, silently
        // violating the trait contract that an out-of-window
        // `since` collapses to `WindowExceeded`. Same helper as
        // `record` so the two paths can't drift.
        evict_expired(&mut entries, Instant::now(), self.window);

        // The trait contract collapses "older than retention" and
        // "never recorded" into the same `WindowExceeded` outcome
        // (the recovery is identical: reconnect without resume). An
        // empty cache satisfies the "never recorded" branch — the
        // tempting "caught up = Ok(empty)" shortcut would silently
        // hide a real gap from a daemon-restart-then-reconnect path.
        let Some(idx) = entries.iter().position(|e| e.envelope.event_id == since) else {
            return Err(ReplayError::WindowExceeded {
                requested: since,
                window_secs: self.window.as_secs(),
            });
        };

        Ok(entries
            .iter()
            .skip(idx + 1)
            .map(|e| e.envelope.clone())
            .collect())
    }

    fn window(&self) -> Duration {
        self.window
    }
}

/// Acquire the mutex, recovering the inner data on poisoning. The
/// cache holds plain envelopes — a panic mid-record cannot leave the
/// `VecDeque` in an inconsistent state, so it's safe to keep using
/// after another thread panicked.
fn lock_or_recover<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Drop entries older than `now - window` from the front of the
/// deque. Shared between the `record` write path and the
/// `replay_since` read path so an idle daemon can't accidentally
/// preserve out-of-window resume anchors. `checked_sub` so a window
/// larger than the process uptime (possible on a freshly-started
/// daemon with `with_window` set to e.g. 24h) doesn't underflow
/// `Instant`.
fn evict_expired<P>(entries: &mut VecDeque<Entry<P>>, now: Instant, window: Duration) {
    let Some(cutoff) = now.checked_sub(window) else {
        return;
    };
    while entries.front().is_some_and(|e| e.recorded_at < cutoff) {
        entries.pop_front();
    }
}

// ── SSE wire format ───────────────────────────────────────────────────

/// One serialized SSE frame, ready for the HTTP body. Wraps a
/// `String` (SSE is always UTF-8) so the HTTP framework can stream it
/// without re-encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame(String);

impl SseFrame {
    /// Underlying UTF-8 string view.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Underlying bytes — what the HTTP body actually writes.
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    /// Consume into a `String` for frameworks that want owned data.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::fmt::Display for SseFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Literal heartbeat payload. Public so an HTTP framework writing
/// directly to the socket can emit it without going through
/// [`SseFrame`] (and the allocation that wrapping would require).
pub const HEARTBEAT_PAYLOAD: &str = ":heartbeat\n\n";

/// Heartbeat frame the daemon emits every 15s on idle SSE
/// connections. Spec-compliant clients ignore SSE comment lines, so
/// it carries no semantic payload — its sole purpose is to defeat
/// idle-connection drops (macOS sleep, NAT timeouts, proxy
/// eviction). Allocates because [`SseFrame`] owns its `String`; if
/// the per-tick allocation matters at 15s cadence × N connections,
/// write [`HEARTBEAT_PAYLOAD`] to the socket directly.
pub fn heartbeat_frame() -> SseFrame {
    SseFrame(HEARTBEAT_PAYLOAD.to_owned())
}

/// Format an envelope as an SSE frame.
///
/// Wire shape (matches `docs/api-desktop-openapi.yaml` `/events`):
///
/// ```text
/// id: <event_id>
/// event: <event_type or "message">
/// data: <compact-JSON envelope>
/// \n
/// ```
///
/// The framing's `id:` and `event:` are the SSE-standard fields the
/// User-Agent uses for `Last-Event-ID` and dispatch; the same
/// values also appear inside the JSON body so non-SSE projections
/// (webhook, MCP) carry the envelope verbatim without losing
/// typing — that's the explicit contract the spec calls out.
///
/// `event_type` is extracted from the serialized envelope's top-level
/// `event_type` field. Payloads without one (e.g. the bus-mechanics
/// `Envelope<u32>` used in tests) fall back to `"message"`, matching
/// the SSE spec default.
///
/// # Errors
///
/// Returns `serde_json::Error` only when `P`'s [`Serialize`] impl
/// itself fails — a programming bug in the payload type, not a
/// runtime condition the caller can recover from.
pub fn format_sse_frame<P: Serialize>(env: &Envelope<P>) -> Result<SseFrame, serde_json::Error> {
    // Going through `serde_json::Value` once lets us peek at the
    // serialized `event_type` discriminator without forcing every
    // payload `P` to implement a trait announcing it, and lets the
    // sink path filter-then-format using a single serialization (see
    // `SseEventSink::forward`).
    let value = serde_json::to_value(env)?;
    Ok(format_sse_frame_from_value(env.event_id, &value))
}

/// Build an SSE frame from an already-serialized envelope. Private
/// helper shared by [`format_sse_frame`] and [`SseEventSink::forward`]
/// so the sink can extract the discriminator for the topic filter
/// without re-serializing.
///
/// Sanitizes `event_type` against CR/LF before interpolating: the
/// SSE wire is line-oriented, so a stray newline in the
/// discriminator would split the frame and let downstream consumers
/// see fabricated events. Today's `EventPayload` discriminators are
/// safe const tags, but `format_sse_frame` is a public helper whose
/// signature accepts any `Serialize` payload — defending here keeps
/// a future payload's typo or upstream-injected value from corrupting
/// the stream. `data:` is compact JSON, which serde escapes any
/// embedded newlines into `\n` literals, so it needs no scrub.
fn format_sse_frame_from_value(event_id: EventId, value: &serde_json::Value) -> SseFrame {
    let event_type = sanitize_sse_field(event_type_of(value));
    // `Value::to_string` calls its compact-JSON `Display` impl —
    // cheaper than going back through `serde_json::to_string` because
    // it walks the already-typed tree rather than re-serializing
    // through the `Serialize` reflection machinery.
    let data = value.to_string();
    SseFrame(format!(
        "id: {event_id}\nevent: {event_type}\ndata: {data}\n\n",
    ))
}

/// Pull the SSE `event:` discriminator out of a serialized envelope.
/// Returns `"message"` (the SSE spec default) when no `event_type`
/// field is present.
fn event_type_of(value: &serde_json::Value) -> &str {
    value
        .get("event_type")
        .and_then(|v| v.as_str())
        .unwrap_or("message")
}

/// Replace CR/LF with `_` so an interpolated string can't terminate
/// or fabricate an SSE field. `\r` and `\n` are the only line
/// separators SSE recognizes, so stripping just these is sufficient
/// — control chars like `\t` are permitted in field values per the
/// SSE spec.
fn sanitize_sse_field(s: &str) -> std::borrow::Cow<'_, str> {
    if s.bytes().any(|b| b == b'\n' || b == b'\r') {
        std::borrow::Cow::Owned(
            s.chars()
                .map(|c| if c == '\n' || c == '\r' { '_' } else { c })
                .collect(),
        )
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

// ── topic filter ──────────────────────────────────────────────────────

/// Filter for the SSE `?topics=` query. Comma-separated globs; `*`
/// matches any run of characters.
///
/// Examples (from the OpenAPI doc):
/// - `meeting.*,transcript.final`
/// - `transcript.partial`
/// - omitted / empty → [`Self::All`]
#[derive(Debug, Clone, Default)]
pub enum TopicFilter {
    /// Match every event_type. The default when `?topics=` is absent.
    #[default]
    All,
    /// Match if any glob in the list matches.
    Globs(Vec<TopicGlob>),
}

impl TopicFilter {
    /// Parse the `?topics=` query value. Empty / whitespace-only
    /// input yields [`TopicFilter::All`] rather than a
    /// matches-nothing filter — that's the spec default and the
    /// no-filter ergonomic. A bare `*` glob anywhere in the list
    /// also collapses to [`TopicFilter::All`] so the hot match path
    /// can short-circuit instead of running the glob engine per
    /// event.
    pub fn parse(spec: &str) -> Self {
        let globs: Vec<TopicGlob> = spec
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(TopicGlob::new)
            .collect();
        if globs.is_empty() || globs.iter().any(|g| g.is_match_all()) {
            Self::All
        } else {
            Self::Globs(globs)
        }
    }

    /// `true` when this filter would let `event_type` through.
    pub fn matches(&self, event_type: &str) -> bool {
        match self {
            Self::All => true,
            Self::Globs(globs) => globs.iter().any(|g| g.matches(event_type)),
        }
    }
}

/// A single glob from a [`TopicFilter`]. Public so callers can pre-
/// compile patterns; constructed via [`Self::new`].
///
/// Only `*` is special (matches any run of chars, including empty);
/// every other character is literal. There is no escape for a literal
/// `*` because no documented `event_type` contains one — if that ever
/// changes, `TopicGlob` is the sole place that needs to grow an escape
/// rule.
#[derive(Debug, Clone)]
pub struct TopicGlob {
    pattern: String,
}

impl TopicGlob {
    /// Compile a glob pattern.
    pub fn new(pattern: &str) -> Self {
        Self {
            pattern: pattern.to_owned(),
        }
    }

    /// `true` when this glob would match every input — `"*"`, `"**"`,
    /// or any string of nothing-but-`*`. Used by [`TopicFilter::parse`]
    /// to short-circuit to [`TopicFilter::All`].
    pub fn is_match_all(&self) -> bool {
        !self.pattern.is_empty() && self.pattern.bytes().all(|b| b == b'*')
    }

    /// `true` when this glob matches `text`.
    pub fn matches(&self, text: &str) -> bool {
        glob_match(self.pattern.as_bytes(), text.as_bytes())
    }
}

/// Match `pattern` against `text` byte-by-byte. `*` consumes any run
/// of bytes (including empty). Iterative two-pointer algorithm — no
/// recursion and O(n·m) worst case, so a `?topics=` query crafted
/// like `*a*a*a*…` cannot trigger exponential backtracking.
///
/// We work in bytes rather than chars: `event_type` is ASCII per the
/// OpenAPI schema (snake_case + `.`), and `*` itself is single-byte
/// in UTF-8, so the byte-level match agrees with the char-level
/// match. A pattern with non-ASCII literals would still match
/// correctly — UTF-8 is self-synchronizing, so a multi-byte char's
/// bytes only match other instances of the same char.
fn glob_match(pattern: &[u8], text: &[u8]) -> bool {
    let mut p = 0usize; // pattern index
    let mut t = 0usize; // text index
    let mut star_p: Option<usize> = None; // last `*` position in pattern
    let mut star_t: usize = 0; // text position when we hit that `*`

    while t < text.len() {
        if p < pattern.len() && pattern[p] == b'*' {
            star_p = Some(p);
            star_t = t;
            p += 1;
        } else if p < pattern.len() && pattern[p] == text[t] {
            p += 1;
            t += 1;
        } else if let Some(sp) = star_p {
            // Backtrack: extend the previous `*`'s match by one
            // character, retry the rest of the pattern from there.
            p = sp + 1;
            star_t += 1;
            t = star_t;
        } else {
            return false;
        }
    }
    // Drain trailing `*`s in the pattern — they all match the empty
    // suffix of text.
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

// ── SSE event sink ────────────────────────────────────────────────────

/// Per-connection [`EventSink`] that serializes envelopes onto an
/// SSE-formatted [`mpsc::Sender`]. The HTTP framework drains the
/// matching [`mpsc::Receiver`] into the response body.
///
/// Construct one per active SSE connection; drop it when the client
/// disconnects (the bus's broadcast subscription handle should be
/// dropped at the same time).
///
/// `topics` filters which events forward; events that don't match
/// the filter are silently skipped — neither an error nor a frame is
/// emitted. The bus subscription stays live so the same sink can
/// resume forwarding when matching events arrive later.
pub struct SseEventSink {
    label: String,
    tx: mpsc::Sender<SseFrame>,
    topics: TopicFilter,
}

impl SseEventSink {
    /// Construct a sink. `label` is the diagnostic string returned by
    /// [`Self::label`]; the daemon typically uses
    /// `"http-sse:<peer-addr>"`.
    pub fn new(label: impl Into<String>, tx: mpsc::Sender<SseFrame>, topics: TopicFilter) -> Self {
        Self {
            label: label.into(),
            tx,
            topics,
        }
    }

    /// Diagnostic label for this sink. Inherent so callers can reach
    /// it without a trait-disambiguation turbofish — the [`EventSink`]
    /// impl is generic over `P` but `label` doesn't depend on the
    /// payload type.
    pub fn label(&self) -> &str {
        &self.label
    }
}

#[async_trait]
impl<P: Clone + Send + Sync + 'static + Serialize> EventSink<P> for SseEventSink {
    /// Forward an envelope onto the SSE channel.
    ///
    /// Filters against the serialized `event_type` discriminator;
    /// payloads without one fall through as `"message"` and are
    /// subject to the same glob — predictable behaviour rather than a
    /// hidden carve-out.
    ///
    /// Awaits `mpsc::Sender::send`, so a full channel backpressures
    /// the caller (the daemon's subscriber loop). A dropped receiver
    /// yields [`SinkError::Disconnected`]; the daemon should drop the
    /// sink and let the next reconnect re-establish.
    async fn forward(&self, envelope: &Envelope<P>) -> Result<(), SinkError> {
        let value =
            serde_json::to_value(envelope).map_err(|e| SinkError::Transport(e.to_string()))?;
        if !self.topics.matches(event_type_of(&value)) {
            return Ok(());
        }
        let frame = format_sse_frame_from_value(envelope.event_id, &value);
        self.tx
            .send(frame)
            .await
            .map_err(|_| SinkError::Disconnected)
    }

    fn label(&self) -> &str {
        SseEventSink::label(self)
    }
}

// ── tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    //! The tests pin behaviour callers depend on:
    //! - the SSE wire shape exactly matches what
    //!   `docs/api-desktop-openapi.yaml` `/events` documents,
    //! - the replay cache honors the trait's "exact-match or
    //!   `WindowExceeded`" contract,
    //! - the topic filter handles the documented glob examples.
    //!
    //! Use a dummy `TestPayload` rather than `heron-session`'s
    //! `EventPayload` so this crate stays free of a domain
    //! dependency.

    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "event_type", content = "data", rename_all = "snake_case")]
    enum TestPayload {
        #[serde(rename = "meeting.detected")]
        MeetingDetected { title: String },
        #[serde(rename = "transcript.final")]
        TranscriptFinal { text: String },
    }

    fn envelope(payload: TestPayload) -> Envelope<TestPayload> {
        Envelope::new(payload).with_meeting("mtg_test")
    }

    // ── format_sse_frame ─────────────────────────────────────────────

    #[test]
    fn format_sse_frame_emits_id_event_data_blank_line() {
        let env = envelope(TestPayload::MeetingDetected {
            title: "Standup".into(),
        });
        let frame = format_sse_frame(&env).expect("format");
        let s = frame.as_str();
        // Pin the line shape: SSE clients depend on `id: `, `event: `,
        // `data: ` prefixes (with the space) and the blank line.
        let mut lines = s.lines();
        assert_eq!(lines.next(), Some(format!("id: {}", env.event_id).as_str()),);
        assert_eq!(lines.next(), Some("event: meeting.detected"));
        let data_line = lines.next().expect("data line");
        assert!(
            data_line.starts_with("data: "),
            "data prefix missing: {data_line}",
        );
        // The trailing blank line is required by SSE spec — an event
        // is only dispatched after a blank line. `lines()` strips the
        // separators, so the final `\n\n` shows up as an empty entry
        // followed by `None`.
        assert_eq!(lines.next(), Some(""));
        assert_eq!(lines.next(), None);
        // And the raw bytes end with `\n\n`, no trailing whitespace.
        assert!(s.ends_with("\n\n"), "missing terminating blank line");
    }

    #[test]
    fn format_sse_frame_round_trips_envelope_in_data() {
        // Non-SSE projections rely on the `data:` JSON being a
        // complete `Envelope<P>`. Pin that.
        let env = envelope(TestPayload::TranscriptFinal { text: "ok".into() });
        let frame = format_sse_frame(&env).expect("format");
        let data_line = frame
            .as_str()
            .lines()
            .find(|l| l.starts_with("data: "))
            .expect("data line");
        let json = data_line.strip_prefix("data: ").expect("strip prefix");
        let back: Envelope<TestPayload> = serde_json::from_str(json).expect("round-trip");
        assert_eq!(back.event_id, env.event_id);
        assert!(matches!(back.payload, TestPayload::TranscriptFinal { .. }));
    }

    #[test]
    fn format_sse_frame_falls_back_to_message_event_for_untagged_payload() {
        // Payload struct without an `event_type` discriminator — the
        // SSE standard's default `event:` is `"message"`. A subscriber
        // listening on the default channel should receive these.
        // (A scalar `Envelope<u32>` can't actually flatten through
        // serde, so we use a struct payload instead — that's the
        // realistic shape an untagged adapter would carry anyway.)
        #[derive(Serialize)]
        struct PingPayload {
            ts: i64,
        }
        let env: Envelope<PingPayload> = Envelope::new(PingPayload { ts: 1 });
        let frame = format_sse_frame(&env).expect("format");
        assert!(
            frame.as_str().contains("\nevent: message\n"),
            "missing default event line: {}",
            frame.as_str(),
        );
    }

    #[test]
    fn heartbeat_frame_is_sse_comment_with_blank_line() {
        // Per the spec, heartbeats are ":heartbeat\n\n" — a comment
        // line (leading `:`) followed by the blank-line dispatch.
        // Spec-compliant SSE clients ignore comments entirely.
        let h = heartbeat_frame();
        assert_eq!(h.as_str(), ":heartbeat\n\n");
    }

    // ── topic filter ─────────────────────────────────────────────────

    #[test]
    fn topic_filter_default_is_all() {
        assert!(TopicFilter::All.matches("meeting.detected"));
        assert!(TopicFilter::default().matches("anything.at.all"));
    }

    #[test]
    fn topic_filter_parses_documented_examples() {
        // Documented in api-desktop-openapi.yaml: comma-separated
        // globs, prefix-wildcard form is the common case.
        let filter = TopicFilter::parse("meeting.*,transcript.final");
        assert!(filter.matches("meeting.detected"));
        assert!(filter.matches("meeting.completed"));
        assert!(filter.matches("transcript.final"));
        assert!(!filter.matches("transcript.partial"));
        assert!(!filter.matches("daemon.error"));
    }

    #[test]
    fn topic_filter_empty_input_is_all() {
        // A `?topics=` query that is empty / whitespace / only commas
        // should still pass everything, not nothing — matches the
        // "default: all" spec default.
        for spec in ["", "   ", ",,,", " , , "] {
            let f = TopicFilter::parse(spec);
            assert!(f.matches("any.event"), "spec {spec:?} should match all");
        }
    }

    #[test]
    fn topic_filter_handles_lone_star_and_internal_star() {
        let star = TopicFilter::parse("*");
        assert!(star.matches("meeting.detected"));
        assert!(star.matches(""));

        // Internal `*` (less common but supported by the glob
        // semantics) — `meeting.*.foo` would match nothing today
        // because no event_type has that shape, but the matcher
        // shouldn't reject the pattern out of hand.
        let mid = TopicFilter::parse("meeting.*.foo");
        assert!(mid.matches("meeting.x.foo"));
        assert!(!mid.matches("meeting.x.bar"));
    }

    // ── replay cache ─────────────────────────────────────────────────

    #[test]
    #[should_panic(expected = "capacity > 0")]
    fn replay_cache_zero_capacity_panics() {
        let _: InMemoryReplayCache<u32> = InMemoryReplayCache::new(0);
    }

    #[tokio::test]
    async fn replay_cache_empty_yields_window_exceeded() {
        // Trait contract: an unknown `since` (whether evicted or
        // never recorded) collapses to WindowExceeded so the client
        // reconnects without resume. The "empty cache → Ok(empty)"
        // shortcut would silently hide the daemon-just-restarted
        // gap from a long-running client.
        let cache: InMemoryReplayCache<u32> = InMemoryReplayCache::new(8);
        let result = cache.replay_since(EventId::now_v7()).await;
        assert!(matches!(result, Err(ReplayError::WindowExceeded { .. })));
    }

    #[tokio::test]
    async fn replay_cache_returns_events_strictly_after_match() {
        let cache: InMemoryReplayCache<u32> = InMemoryReplayCache::new(8);
        let envs: Vec<Envelope<u32>> = (0..5).map(Envelope::new).collect();
        for e in &envs {
            cache.record(e.clone());
        }
        // Resume from the middle entry.
        let since = envs[2].event_id;
        let replay = cache.replay_since(since).await.expect("replay");
        let payloads: Vec<u32> = replay.iter().map(|e| e.payload).collect();
        assert_eq!(
            payloads,
            vec![3, 4],
            "must return entries strictly after `since`",
        );
    }

    #[tokio::test]
    async fn replay_cache_unknown_id_yields_window_exceeded() {
        // The trait collapses "older than retention" and "never
        // recorded" into the same recovery (reconnect without
        // resume), so a fresh-but-unknown id must produce
        // WindowExceeded too — never silently zero-replay.
        let cache: InMemoryReplayCache<u32> = InMemoryReplayCache::new(8);
        cache.record(Envelope::new(1));
        let result = cache.replay_since(EventId::now_v7()).await;
        assert!(matches!(result, Err(ReplayError::WindowExceeded { .. })));
    }

    #[tokio::test]
    async fn replay_cache_capacity_evicts_oldest_first() {
        // FIFO eviction: filling beyond capacity drops the head, not
        // the tail. The trait's contract is order-preserving replay.
        let cache: InMemoryReplayCache<u32> = InMemoryReplayCache::new(3);
        let envs: Vec<Envelope<u32>> = (0..5).map(Envelope::new).collect();
        for e in &envs {
            cache.record(e.clone());
        }
        assert_eq!(cache.len(), 3);
        // Oldest two evicted; their IDs should now be window-exceeded.
        let result = cache.replay_since(envs[0].event_id).await;
        assert!(matches!(result, Err(ReplayError::WindowExceeded { .. })));
        // Newest entries still resumable.
        let replay = cache.replay_since(envs[2].event_id).await.expect("replay");
        assert_eq!(
            replay.iter().map(|e| e.payload).collect::<Vec<_>>(),
            vec![3, 4],
        );
    }

    #[tokio::test]
    async fn replay_cache_window_advertised_via_trait() {
        let cache: InMemoryReplayCache<u32> =
            InMemoryReplayCache::with_window(4, Duration::from_secs(120));
        // The HTTP layer reads this via `ReplayCache::window()` to
        // populate the X-Heron-Replay-Window-Seconds header.
        let w = <InMemoryReplayCache<u32> as ReplayCache<u32>>::window(&cache);
        assert_eq!(w, Duration::from_secs(120));
    }

    #[tokio::test]
    async fn replay_cache_clear_drops_all_entries() {
        // Pin the discontinuity-recovery contract: after `clear()`,
        // every prior id must be `WindowExceeded`. A bus subscriber
        // that lagged calls `clear()` so subsequent resumes don't
        // silently return events that skip the gap.
        let cache: InMemoryReplayCache<u32> = InMemoryReplayCache::new(8);
        let envs: Vec<Envelope<u32>> = (0..3).map(Envelope::new).collect();
        for e in &envs {
            cache.record(e.clone());
        }
        assert_eq!(cache.len(), 3);
        cache.clear();
        assert_eq!(cache.len(), 0);
        for e in &envs {
            let result = cache.replay_since(e.event_id).await;
            assert!(
                matches!(result, Err(ReplayError::WindowExceeded { .. })),
                "post-clear replay must be WindowExceeded for every prior id",
            );
        }
    }

    // ── SSE event sink ───────────────────────────────────────────────

    #[tokio::test]
    async fn sse_sink_forwards_matching_event_to_channel() {
        let (tx, mut rx) = mpsc::channel(8);
        let sink = SseEventSink::new("test", tx, TopicFilter::All);
        let env = envelope(TestPayload::MeetingDetected {
            title: "Standup".into(),
        });
        EventSink::forward(&sink, &env).await.expect("forward");
        let frame = rx.recv().await.expect("frame");
        assert!(frame.as_str().contains("event: meeting.detected"));
    }

    #[tokio::test]
    async fn sse_sink_filters_non_matching_event_silently() {
        // Topic filter says transcripts only; meeting events should
        // be dropped without producing a frame and without an error
        // — the subscription stays live for the next event.
        let (tx, mut rx) = mpsc::channel(8);
        let sink = SseEventSink::new("test", tx, TopicFilter::parse("transcript.*"));
        let env = envelope(TestPayload::MeetingDetected { title: "x".into() });
        EventSink::forward(&sink, &env).await.expect("forward");
        // No frame should arrive; use try_recv to assert empty.
        assert!(rx.try_recv().is_err(), "filtered event leaked through");
    }

    #[tokio::test]
    async fn sse_sink_reports_disconnected_when_receiver_dropped() {
        let (tx, rx) = mpsc::channel(8);
        let sink = SseEventSink::new("test", tx, TopicFilter::All);
        drop(rx);
        let env = envelope(TestPayload::TranscriptFinal { text: "x".into() });
        let err = EventSink::forward(&sink, &env)
            .await
            .expect_err("should fail on dropped receiver");
        assert!(matches!(err, SinkError::Disconnected));
    }

    #[tokio::test]
    async fn sse_sink_label_round_trips() {
        let (tx, _rx) = mpsc::channel::<SseFrame>(1);
        let sink = SseEventSink::new("http-sse:127.0.0.1:54321", tx, TopicFilter::All);
        // Inherent method — no trait-disambiguation turbofish needed.
        assert_eq!(sink.label(), "http-sse:127.0.0.1:54321");
    }

    // ── new behaviours pinned by review ──────────────────────────────

    #[test]
    #[should_panic(expected = "window > 0")]
    fn replay_cache_zero_window_panics() {
        let _: InMemoryReplayCache<u32> = InMemoryReplayCache::with_window(8, Duration::ZERO);
    }

    #[tokio::test]
    async fn replay_cache_evicts_by_time_window() {
        // Pin the time-based eviction path: a 50ms window with two
        // records 100ms apart leaves only the second entry.
        let cache: InMemoryReplayCache<u32> =
            InMemoryReplayCache::with_window(8, Duration::from_millis(50));
        let first = Envelope::new(1u32);
        cache.record(first.clone());
        tokio::time::sleep(Duration::from_millis(100)).await;
        let second = Envelope::new(2u32);
        cache.record(second.clone());
        assert_eq!(cache.len(), 1, "first entry should be window-evicted");
        // The evicted id is now WindowExceeded.
        let result = cache.replay_since(first.event_id).await;
        assert!(matches!(result, Err(ReplayError::WindowExceeded { .. })));
    }

    #[tokio::test]
    async fn replay_since_evicts_expired_entries_on_idle_cache() {
        // CodeRabbit-flagged regression guard: an idle daemon (no
        // record traffic) used to let an out-of-window entry serve
        // as a resume anchor. With read-time eviction wired in, the
        // first replay request after the window expires must
        // collapse to WindowExceeded — even if no record happened
        // in between.
        let cache: InMemoryReplayCache<u32> =
            InMemoryReplayCache::with_window(8, Duration::from_millis(50));
        let entry = Envelope::new(1u32);
        cache.record(entry.clone());
        assert_eq!(cache.len(), 1);
        tokio::time::sleep(Duration::from_millis(100)).await;
        // No record() call — only replay. Read-time eviction must
        // still kick in.
        let result = cache.replay_since(entry.event_id).await;
        assert!(matches!(result, Err(ReplayError::WindowExceeded { .. })));
        assert_eq!(
            cache.len(),
            0,
            "replay_since must drop the expired entry it scanned over",
        );
    }

    #[test]
    fn topic_filter_lone_star_collapses_to_all() {
        // After parse, a `*` (or `**`) in the list should produce
        // `All` rather than a `Globs([…])` we'd run per event.
        match TopicFilter::parse("*") {
            TopicFilter::All => {}
            other => panic!("expected All, got {other:?}"),
        }
        match TopicFilter::parse("meeting.detected,*") {
            TopicFilter::All => {}
            other => panic!("expected All, got {other:?}"),
        }
    }

    #[test]
    fn topic_filter_resists_pathological_glob_backtracking() {
        // The recursive matcher would have been exponential on this
        // input. The iterative two-pointer matcher is O(n·m) — under
        // a millisecond. If this test ever times out, the matcher
        // regressed back to recursion or worse.
        let pattern = "*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*b";
        let text = "a".repeat(64);
        let glob = TopicGlob::new(pattern);
        let start = std::time::Instant::now();
        assert!(!glob.matches(&text));
        assert!(
            start.elapsed() < Duration::from_millis(50),
            "glob matcher took {:?} — regression to exponential backtracking?",
            start.elapsed(),
        );
    }

    #[test]
    fn format_sse_frame_sanitizes_newlines_in_event_type() {
        // A payload whose `event_type` smuggles a `\n` would, without
        // sanitization, terminate the SSE `event:` line and let the
        // remainder be reparsed as a separate field. Stripped to `_`.
        #[derive(Serialize)]
        struct Hostile {
            event_type: &'static str,
            data: &'static str,
        }
        let env = Envelope::new(Hostile {
            event_type: "evil\nfake-id: spoofed",
            data: "x",
        });
        let frame = format_sse_frame(&env).expect("format");
        let event_line = frame
            .as_str()
            .lines()
            .find(|l| l.starts_with("event: "))
            .expect("event line");
        assert!(
            !event_line.contains('\n') && !event_line.contains('\r'),
            "raw newline leaked into event line: {event_line:?}",
        );
        assert!(
            event_line.contains("evil_fake-id"),
            "expected sanitized form, got {event_line:?}",
        );
    }

    #[test]
    fn format_sse_frame_id_line_matches_envelope_event_id() {
        // The OpenAPI contract says the framing's `id:` and the
        // payload's `event_id` are the same value — clients depend on
        // this so they can resume from either source. Pin it.
        let env = envelope(TestPayload::MeetingDetected {
            title: "Standup".into(),
        });
        let frame = format_sse_frame(&env).expect("format");
        let id_line = frame
            .as_str()
            .lines()
            .find(|l| l.starts_with("id: "))
            .expect("id line");
        let stripped = id_line.strip_prefix("id: ").expect("strip");
        assert_eq!(stripped, env.event_id.to_string());
    }

    #[test]
    fn sse_frame_display_round_trips_as_str() {
        let env = envelope(TestPayload::TranscriptFinal { text: "ok".into() });
        let frame = format_sse_frame(&env).expect("format");
        assert_eq!(frame.to_string(), frame.as_str());
    }
}
