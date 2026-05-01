//! Characterization tests for the per-event auto-record path on
//! [`heron_orchestrator::LocalSessionOrchestrator`]
//! (`auto_record_tick`, `set_event_auto_record`,
//! `list_auto_record_events`).
//!
//! These tests exist to pin the **current** observable behavior of
//! the public surface BEFORE the #222 plan's PR B (commit 6) bundles
//! the Pending/Suppressed bookkeeping into a `pub(crate) struct
//! AutoRecordState` and routes the trait methods through a borrow-
//! type sub-orchestrator. Each test drives the orchestrator via its
//! public methods and asserts on returned values + bus envelopes —
//! never on field layout, so the assertions hold post-reshape.
//!
//! Per the #222 plan §"PR A — characterization tests" §
//! `tests/auto_record.rs`. These are the safety net the plan calls
//! out for PR B's medium-risk state-bundling commit.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use chrono::{DateTime, TimeZone, Utc};
use heron_orchestrator::{Builder, LocalSessionOrchestrator};
use heron_session::{
    AutoRecordList, MeetingStatus, SessionOrchestrator, SetEventAutoRecordRequest,
};
use heron_vault::{CalendarError, CalendarEvent, CalendarReader};
use tempfile::TempDir;

/// Test calendar reader that always returns the same canned events
/// regardless of the requested window. Mirrors `vault_reads.rs`'s
/// `FakeCalendar` so test setups stay symmetric, with one extension:
/// the events list is held under a `Mutex` so a test can mutate it
/// between ticks if needed (e.g. simulate "user toggled off").
struct FakeCalendar {
    events: Mutex<Vec<CalendarEvent>>,
}

impl FakeCalendar {
    fn new(events: Vec<CalendarEvent>) -> Self {
        Self {
            events: Mutex::new(events),
        }
    }
}

impl CalendarReader for FakeCalendar {
    fn read_window(
        &self,
        _start_utc: DateTime<Utc>,
        _end_utc: DateTime<Utc>,
    ) -> Result<Option<Vec<CalendarEvent>>, CalendarError> {
        Ok(Some(self.events.lock().unwrap().clone()))
    }
}

/// Compose the synthetic `calendar_event_id` `list_upcoming_calendar`
/// stamps onto each event. The id is `synth_<start.bits>_<end.bits>_<title>`
/// — tests need it to correlate `list_upcoming_calendar` output with
/// `list_auto_record_events` / `set_event_auto_record` payloads.
fn synth_id(start: f64, end: f64, title: &str) -> String {
    format!("synth_{}_{}_{}", start.to_bits(), end.to_bits(), title)
}

/// Convert a `chrono::DateTime<Utc>` into the f64-epoch-seconds shape
/// `CalendarEvent` carries on the wire.
fn to_epoch_secs(dt: DateTime<Utc>) -> f64 {
    dt.timestamp() as f64
}

/// Build an orchestrator with a tempdir-backed vault root (so the
/// auto-record registry persists across `set_event_auto_record` calls
/// inside a single test) and a custom calendar reader.
fn build_orch(events: Vec<CalendarEvent>) -> (LocalSessionOrchestrator, TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let cal: Arc<dyn CalendarReader> = Arc::new(FakeCalendar::new(events));
    let orch = Builder::default()
        .vault_root(dir.path().to_path_buf())
        .calendar(cal)
        .build();
    (orch, dir)
}

// ── auto_record_tick ──────────────────────────────────────────────────

/// Pins behavior of #222 plan §PR A `auto_record.rs` for PR B's
/// `AutoRecordState`/`AutoRecord<'a>` extraction (commit 6). Two
/// successive ticks within `AUTO_RECORD_DEDUP_TTL` (12h) for the same
/// calendar event must result in a single capture starting — the
/// dedup map (`auto_record_fired`) is the contract being pinned.
/// Without this guarantee the scheduler would re-fire on every
/// 30s-cadence tick inside the 5-minute start window, producing
/// duplicate captures + duplicate vault notes.
#[tokio::test]
async fn auto_record_tick_dedups_within_ttl() {
    // Anchor `now` at a fixed wall clock so the test is independent
    // of the host system clock. Pick an event that starts 1 minute
    // after `now` — well inside the 5-minute start window.
    let now = Utc.with_ymd_and_hms(2026, 6, 1, 9, 0, 0).unwrap();
    let event_start = now + chrono::Duration::minutes(1);
    let event_end = now + chrono::Duration::minutes(31);
    let title = "Standup";

    let (orch, _dir) = build_orch(vec![CalendarEvent {
        title: title.to_owned(),
        start: to_epoch_secs(event_start),
        end: to_epoch_secs(event_end),
        attendees: Vec::new(),
    }]);

    // Enable auto-record on the event so the tick will fire it.
    let id = synth_id(to_epoch_secs(event_start), to_epoch_secs(event_end), title);
    orch.set_event_auto_record(SetEventAutoRecordRequest {
        calendar_event_id: id.clone(),
        enabled: true,
    })
    .await
    .expect("enable auto-record");

    // First tick: fires once.
    let fired = orch.auto_record_tick(now).await;
    assert_eq!(
        fired, 1,
        "first tick within window must fire exactly one capture"
    );

    // Second tick at the same `now` (well within the 12h dedup TTL):
    // the fired map keeps the event suppressed, so zero new fires.
    let fired = orch.auto_record_tick(now).await;
    assert_eq!(
        fired, 0,
        "second tick within dedup TTL must not re-fire the same event",
    );

    // The first tick's capture is still active in the orchestrator;
    // clean it up so the test's drop path is deterministic.
    let active_id = orch
        .list_meetings(heron_session::ListMeetingsQuery {
            status: Some(MeetingStatus::Recording),
            ..Default::default()
        })
        .await
        .expect("list active")
        .items
        .first()
        .map(|m| m.id);
    if let Some(id) = active_id {
        let _ = orch.end_meeting(&id).await;
    }
}

/// Pins behavior of #222 plan §PR A for PR B's `AutoRecordState`
/// extraction. Events whose `start` is outside `[now, now +
/// AUTO_RECORD_START_WINDOW]` (5 min) must NOT fire — even when
/// auto-record is enabled. Otherwise the scheduler would arm a
/// recording an hour early. This test pins the window-bounds check
/// inside `auto_record_tick`; PR B's `AutoRecord::tick()` body must
/// preserve it.
#[tokio::test]
async fn auto_record_tick_skips_outside_start_window() {
    let now = Utc.with_ymd_and_hms(2026, 6, 1, 9, 0, 0).unwrap();
    // Event starts 1 hour from now — well past the 5-minute window.
    let event_start = now + chrono::Duration::hours(1);
    let event_end = event_start + chrono::Duration::minutes(30);
    let title = "Future Meeting";

    let (orch, _dir) = build_orch(vec![CalendarEvent {
        title: title.to_owned(),
        start: to_epoch_secs(event_start),
        end: to_epoch_secs(event_end),
        attendees: Vec::new(),
    }]);

    let id = synth_id(to_epoch_secs(event_start), to_epoch_secs(event_end), title);
    orch.set_event_auto_record(SetEventAutoRecordRequest {
        calendar_event_id: id,
        enabled: true,
    })
    .await
    .expect("enable auto-record");

    let fired = orch.auto_record_tick(now).await;
    assert_eq!(
        fired, 0,
        "events outside the start window must not fire even with auto-record on",
    );

    // Defensive: no capture started.
    let active = orch
        .list_meetings(heron_session::ListMeetingsQuery {
            status: Some(MeetingStatus::Recording),
            ..Default::default()
        })
        .await
        .expect("list");
    assert!(
        active.items.is_empty(),
        "no Recording meetings should exist when tick skipped",
    );
}

// ── set_event_auto_record / list_auto_record_events ──────────────────

/// Pins behavior of #222 plan §PR A for PR B's `AutoRecordState`
/// extraction. The toggle round-trips through the registry: a
/// `set_event_auto_record(true)` followed by `list_auto_record_events`
/// must surface the calendar event id in the returned list, and a
/// subsequent `set_event_auto_record(false)` must remove it. The
/// list output is sorted (per the registry's `list()` impl) for a
/// stable wire shape — pin that too.
#[tokio::test]
async fn set_event_auto_record_then_list_auto_record_events_round_trips() {
    // No vault root — the registry runs in in-memory mode, which is
    // the substrate behavior the desktop falls back to when no
    // vault is configured. A separate vault_reads.rs test pins the
    // on-disk persistence path; this one pins the in-memory round
    // trip the public API surfaces.
    let orch = LocalSessionOrchestrator::new();

    // Enable two events in non-sorted insertion order; the list
    // must come back sorted.
    orch.set_event_auto_record(SetEventAutoRecordRequest {
        calendar_event_id: "evt_zulu".into(),
        enabled: true,
    })
    .await
    .expect("set zulu");
    orch.set_event_auto_record(SetEventAutoRecordRequest {
        calendar_event_id: "evt_alpha".into(),
        enabled: true,
    })
    .await
    .expect("set alpha");

    let listed: AutoRecordList = orch.list_auto_record_events().await.expect("list");
    assert_eq!(
        listed.event_ids,
        vec!["evt_alpha".to_owned(), "evt_zulu".to_owned()],
        "list must surface enabled ids sorted for byte-stable wire shape",
    );

    // Disable one — list shrinks accordingly.
    orch.set_event_auto_record(SetEventAutoRecordRequest {
        calendar_event_id: "evt_zulu".into(),
        enabled: false,
    })
    .await
    .expect("disable zulu");

    let listed = orch
        .list_auto_record_events()
        .await
        .expect("list after disable");
    assert_eq!(listed.event_ids, vec!["evt_alpha".to_owned()]);
}
