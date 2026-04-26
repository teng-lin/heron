//! `heron-event-tauri` — Tauri IPC projection of the canonical
//! `heron-event` bus.
//!
//! Sibling adapter to [`heron_event_http`]: same canonical
//! [`heron_event::EventBus`] feeds both, but where the HTTP crate
//! framecasts onto SSE for browser/CLI subscribers, this one re-emits
//! envelopes as Tauri IPC events for the desktop frontend.
//!
//! Per `docs/api-design-spec.md` Invariant 13 (transports are
//! projections), the canonical `heron_event::EventSink` trait stays
//! the only contract publishers care about. This crate adds one
//! concrete implementor: [`TauriEventSink`].
//!
//! ## Wire shape
//!
//! Each envelope becomes one Tauri event whose **name is the
//! envelope's `event_type` discriminator** with `.` mapped to `:`
//! (Tauri's validator rejects `.`; see [`sanitize_event_name`])
//! and whose **payload is the full envelope as JSON**. So
//! `meeting.detected` becomes the Tauri event `meeting:detected`,
//! matching the existing desktop convention (`hotkey:fired`,
//! `nav:settings`). The payload's inner `event_type` field still
//! carries the raw dotted form for cross-transport identity.
//!
//! A frontend listener subscribes per type:
//!
//! ```ts
//! import { listen } from "@tauri-apps/api/event";
//! await listen("meeting:detected", (e) => console.log(e.payload));
//! ```
//!
//! Payloads without a top-level `event_type` field fall through as
//! Tauri event name `"message"` — the same convention
//! `heron_event_http::format_sse_frame` uses, so the two adapters
//! stay coherent. (Plain backticks rather than an intra-doc link
//! because this crate doesn't depend on `heron-event-http`; rustdoc
//! would otherwise flag a broken-link warning.)
//!
//! ## What's *not* here
//!
//! - **No replay cache.** Tauri IPC is in-process between the
//!   Rust core and the WebView; there's no reconnect / resume
//!   semantics to back. The HTTP `Last-Event-ID` story is replay
//!   cache territory.
//! - **No heartbeat.** Tauri keeps the IPC channel alive itself;
//!   frontends don't see idle drops the way an HTTP/SSE consumer
//!   does over the public network.
//! - **No topic filter at the sink.** Tauri's frontend
//!   `listen("name", …)` already filters per type. Adding a sink-
//!   side filter would just re-implement that with worse ergonomics.
//!
//! ## Multi-window targeting
//!
//! [`TauriEventSink`] uses [`tauri::Emitter::emit`], which fans out
//! to every webview / window the app currently hosts — the
//! v1 desktop topology (one main window, occasional settings
//! window). When per-window routing matters (e.g. emit only to a
//! tray popover), the daemon constructs a separate sink per target
//! and uses [`tauri::Emitter::emit_to`] directly; we don't pre-bake
//! that into this trait surface.

use async_trait::async_trait;
use heron_event::{Envelope, EventSink, SinkError};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Runtime};

/// Per-app [`EventSink`] that re-emits envelopes onto Tauri IPC.
///
/// Construct one per `AppHandle` (typically just one for the desktop
/// app's lifetime — the handle is `Clone` and cheap, no need for a
/// pool). Drop / replace it during shutdown.
///
/// Generic over the Tauri runtime so the same code is exercised by
/// both production (`tauri::Wry`) and tests (`tauri::test::MockRuntime`)
/// without a feature flag dance.
pub struct TauriEventSink<R: Runtime> {
    label: String,
    app: AppHandle<R>,
}

impl<R: Runtime> TauriEventSink<R> {
    /// Construct a sink. `label` is the diagnostic string returned by
    /// [`Self::label`]; the daemon typically uses
    /// `"tauri-ipc:<window-or-app>"`.
    pub fn new(label: impl Into<String>, app: AppHandle<R>) -> Self {
        Self {
            label: label.into(),
            app,
        }
    }

    /// Diagnostic label for this sink. Inherent so callers can reach
    /// it without a trait-disambiguation turbofish (the [`EventSink`]
    /// impl is generic over `P` but `label` doesn't depend on the
    /// payload type).
    pub fn label(&self) -> &str {
        &self.label
    }
}

// `P: Sync` is structural: async-trait boxes the body's future as
// `Send`, and the future captures `&Envelope<P>` across its synthetic
// await point — `&T` is `Send` only when `T: Sync`. The
// canonical `EventSink` trait only demands `P: Clone + Send +
// 'static`, so this is a localized adapter constraint rather than a
// regression on the bus contract; in practice every payload that
// flows through `EventBus<P>` already satisfies `Sync` because
// `Clone + Send` payloads almost always do.
#[async_trait]
impl<R: Runtime, P: Clone + Send + Sync + 'static + Serialize> EventSink<P> for TauriEventSink<R> {
    /// Forward an envelope as a Tauri event.
    ///
    /// Tauri's `emit` is synchronous from the caller's perspective
    /// (it serializes + queues to each webview's IPC channel), so
    /// there's no backpressure surface — the only failure mode is a
    /// serialization error or the runtime tearing down. Both fold
    /// into [`SinkError::Transport`].
    async fn forward(&self, envelope: &Envelope<P>) -> Result<(), SinkError> {
        // One serialization, then peek at the discriminator without
        // re-walking the payload — same shape as
        // heron-event-http's `format_sse_frame_from_value`.
        let value =
            serde_json::to_value(envelope).map_err(|e| SinkError::Transport(e.to_string()))?;
        let event_name = sanitize_event_name(event_type_of(&value));
        self.app
            .emit(&event_name, value)
            .map_err(|e| SinkError::Transport(e.to_string()))
    }

    fn label(&self) -> &str {
        TauriEventSink::label(self)
    }
}

/// Pull the Tauri event-name discriminator out of a serialized
/// envelope. Returns `"message"` (the default channel, same default
/// SSE uses) when:
/// - the field is absent (untagged payload), OR
/// - the field is non-string (buggy publisher), OR
/// - the field is an empty string (would otherwise produce a
///   never-subscribable empty Tauri event name).
fn event_type_of(value: &serde_json::Value) -> &str {
    value
        .get("event_type")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("message")
}

/// Map a heron `event_type` discriminator into a Tauri-legal event
/// name. Tauri's validator (`tauri::event::event_name`) accepts only
/// alphanumeric + `-` / `/` / `:` / `_` — notably *not* `.`, which
/// every heron event_type uses as the namespace separator
/// (`meeting.detected`, `transcript.partial`, …).
///
/// Mapping rules:
/// - `.` → `:` so namespaced types stay namespaced. Matches the
///   existing desktop event-name convention (`hotkey:fired`,
///   `nav:settings`); a frontend listener subscribes with
///   `listen("meeting:detected", …)`.
/// - Any other illegal character (control chars, whitespace,
///   smuggled CR/LF) → `_`. Defends against a typoed or
///   upstream-injected `event_type` corrupting the wire — the event
///   still delivers, just under a renamed channel rather than
///   bouncing as a runtime [`SinkError::Transport`].
///
/// Already-legal names pass through unchanged, so an `event_type`
/// that already follows Tauri conventions (e.g. `nav:settings`)
/// round-trips identically.
///
/// **Source-side invariant (relied on by injectivity).** Heron
/// `event_type` discriminators per the OpenAPI spec use lowercase
/// alphanumerics + `.` only; they MUST NOT contain `:`. Under that
/// constraint the `.` → `:` mapping is reversible and no two
/// upstream types collide on the same Tauri name. If a future
/// `event_type` ever introduces `:`, this mapping would collide with
/// dotted names that share the rest of the string (e.g. `a.b` and
/// `a:b` both become `a:b`) — at that point we'd need to escape
/// rather than substitute.
fn sanitize_event_name(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '.' => ':',
            c if c.is_alphanumeric() || matches!(c, '-' | '/' | ':' | '_') => c,
            _ => '_',
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::expect_used)]
#[allow(clippy::unwrap_used)]
mod tests {
    //! The tests pin three things callers depend on:
    //! - the Tauri event name equals the envelope's `event_type`,
    //! - the JSON payload round-trips back into the original
    //!   `Envelope<P>`, and
    //! - control-char injection in `event_type` can't smuggle a
    //!   different event name onto the wire.
    //!
    //! Use a dummy `TestPayload` rather than `heron-session`'s
    //! `EventPayload` so this crate stays free of a domain
    //! dependency.

    use super::*;
    use heron_event::Envelope;
    use serde::{Deserialize, Serialize};
    use std::sync::{Arc, Mutex};
    use tauri::Listener;

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "event_type", content = "data", rename_all = "snake_case")]
    enum TestPayload {
        #[serde(rename = "meeting.detected")]
        MeetingDetected { title: String },
        #[serde(rename = "transcript.final")]
        TranscriptFinal { text: String },
    }

    /// Captured `(event_name, raw_payload_json)` pairs for assertions.
    /// Behind `Arc<Mutex<…>>` because Tauri's listener callback is
    /// `Fn + Send + 'static` and can fire on any worker thread.
    type Captured = Arc<Mutex<Vec<(String, String)>>>;

    fn install_listener(app: &tauri::AppHandle<tauri::test::MockRuntime>, name: &str) -> Captured {
        let captured: Captured = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);
        let event_name = name.to_owned();
        app.listen(name, move |evt| {
            let payload = evt.payload().to_owned();
            captured_clone
                .lock()
                .expect("lock listener capture")
                .push((event_name.clone(), payload));
        });
        captured
    }

    fn envelope(payload: TestPayload) -> Envelope<TestPayload> {
        Envelope::new(payload).with_meeting("mtg_test")
    }

    #[tokio::test]
    async fn forwards_envelope_as_tauri_event_named_after_event_type() {
        // `meeting.detected` → `meeting:detected` per the
        // dot-to-colon mapping that bridges heron's namespace
        // convention to Tauri's allowed-character set. Frontend
        // listens with `listen("meeting:detected", …)`.
        let app = tauri::test::mock_app();
        let captured = install_listener(app.handle(), "meeting:detected");
        let sink = TauriEventSink::new("test", app.handle().clone());
        let env = envelope(TestPayload::MeetingDetected {
            title: "Standup".into(),
        });
        EventSink::forward(&sink, &env).await.expect("forward");

        let entries = captured.lock().expect("lock capture");
        assert_eq!(entries.len(), 1, "expected one event delivered");
        assert_eq!(entries[0].0, "meeting:detected");
        // Payload is the full envelope as JSON; round-trip it back
        // and confirm the discriminator + body survived. The payload
        // still carries the raw `event_type` ("meeting.detected"),
        // not the sanitized form — only the wire name is mapped.
        let back: Envelope<TestPayload> =
            serde_json::from_str(&entries[0].1).expect("payload JSON");
        match back.payload {
            TestPayload::MeetingDetected { title } => assert_eq!(title, "Standup"),
            other => panic!("unexpected payload variant: {other:?}"),
        }
        assert_eq!(back.event_id, env.event_id);
    }

    #[tokio::test]
    async fn different_envelopes_route_to_different_listeners() {
        // Frontend pattern: one `listen()` per event type. Confirm
        // that two envelopes with different `event_type` discriminators
        // land on the right (sanitized) channel each.
        let app = tauri::test::mock_app();
        let m_cap = install_listener(app.handle(), "meeting:detected");
        let t_cap = install_listener(app.handle(), "transcript:final");
        let sink = TauriEventSink::new("test", app.handle().clone());

        EventSink::forward(
            &sink,
            &envelope(TestPayload::MeetingDetected { title: "x".into() }),
        )
        .await
        .expect("forward 1");
        EventSink::forward(
            &sink,
            &envelope(TestPayload::TranscriptFinal { text: "y".into() }),
        )
        .await
        .expect("forward 2");

        assert_eq!(m_cap.lock().expect("lock m").len(), 1);
        assert_eq!(t_cap.lock().expect("lock t").len(), 1);
    }

    #[test]
    fn sanitize_event_name_maps_dots_to_colons_and_passes_legal_names_through() {
        // Pin the dot→colon mapping that the wire shape relies on,
        // and confirm names already in Tauri's allowed set
        // round-trip identically (no double-mapping when an
        // `event_type` like `nav:settings` flows through).
        assert_eq!(sanitize_event_name("meeting.detected"), "meeting:detected");
        assert_eq!(sanitize_event_name("nav:settings"), "nav:settings");
        assert_eq!(
            sanitize_event_name("download-progress"),
            "download-progress"
        );
        assert_eq!(sanitize_event_name("evt/123"), "evt/123");
        // Multi-namespace dot replacement.
        assert_eq!(sanitize_event_name("a.b.c"), "a:b:c");
    }

    #[tokio::test]
    async fn falls_back_to_message_event_when_no_discriminator() {
        // Tagged enums always carry `event_type`, but a custom payload
        // without one still has to deliver — to the SSE-conventional
        // `message` channel. Pin that contract.
        #[derive(Clone, Serialize)]
        struct Untagged {
            note: String,
        }
        let app = tauri::test::mock_app();
        let captured = install_listener(app.handle(), "message");
        let sink = TauriEventSink::new("test", app.handle().clone());
        let env: Envelope<Untagged> = Envelope::new(Untagged { note: "hi".into() });
        EventSink::forward(&sink, &env).await.expect("forward");
        assert_eq!(captured.lock().expect("lock").len(), 1);
    }

    #[tokio::test]
    async fn sanitizes_control_chars_in_event_type() {
        // A payload whose `event_type` smuggles a `\n` would otherwise
        // either be rejected by Tauri (best case) or potentially
        // confuse a subscriber doing string-based dispatch. Replace
        // with `_` so the event still delivers under a predictable
        // sanitized name.
        #[derive(Clone, Serialize)]
        struct Hostile {
            event_type: &'static str,
            data: &'static str,
        }
        let app = tauri::test::mock_app();
        // The sanitized name is what the listener subscribes to.
        let captured = install_listener(app.handle(), "evil_fake");
        let sink = TauriEventSink::new("test", app.handle().clone());
        let env = Envelope::new(Hostile {
            event_type: "evil\nfake",
            data: "x",
        });
        EventSink::forward(&sink, &env).await.expect("forward");
        assert_eq!(
            captured.lock().expect("lock").len(),
            1,
            "expected delivery under sanitized name",
        );
    }

    #[tokio::test]
    async fn label_round_trips_through_inherent_method() {
        let app = tauri::test::mock_app();
        let sink = TauriEventSink::new("tauri-ipc:main", app.handle().clone());
        // Inherent method — no trait-disambiguation turbofish needed.
        assert_eq!(sink.label(), "tauri-ipc:main");
    }

    #[tokio::test]
    async fn empty_event_type_falls_back_to_message_channel() {
        // Review-flagged regression guard: an empty `event_type`
        // would sanitize to an empty Tauri event name, which is
        // effectively unsubscribable. Fall back to the `"message"`
        // default channel instead so the event still delivers.
        #[derive(Clone, Serialize)]
        struct Empty {
            event_type: &'static str,
            data: &'static str,
        }
        let app = tauri::test::mock_app();
        let captured = install_listener(app.handle(), "message");
        let sink = TauriEventSink::new("test", app.handle().clone());
        let env = Envelope::new(Empty {
            event_type: "",
            data: "x",
        });
        EventSink::forward(&sink, &env).await.expect("forward");
        assert_eq!(
            captured.lock().expect("lock").len(),
            1,
            "empty event_type should land on the message channel",
        );
    }

    #[tokio::test]
    async fn non_string_event_type_falls_back_to_message_channel() {
        // Same fallback path: a buggy publisher whose `event_type`
        // serializes as a non-string (number / object / null) lands
        // on `"message"` rather than vanishing.
        #[derive(Clone, Serialize)]
        struct NumericTag {
            event_type: u32,
            data: &'static str,
        }
        let app = tauri::test::mock_app();
        let captured = install_listener(app.handle(), "message");
        let sink = TauriEventSink::new("test", app.handle().clone());
        let env = Envelope::new(NumericTag {
            event_type: 42,
            data: "x",
        });
        EventSink::forward(&sink, &env).await.expect("forward");
        assert_eq!(captured.lock().expect("lock").len(), 1);
    }
}
