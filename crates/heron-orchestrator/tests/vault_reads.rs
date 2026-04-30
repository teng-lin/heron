//! Integration tests for the vault-backed read endpoints on
//! [`heron_orchestrator::LocalSessionOrchestrator`].
//!
//! Each test builds a tempdir-backed vault fixture so the
//! filesystem-walk path the daemon uses in production runs in CI.
//! Calendar reads use a fake [`CalendarReader`] so the tests run on
//! linux CI (no EventKit) and don't require macOS TCC.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{NaiveDate, TimeZone, Utc};
use heron_orchestrator::{Builder, LocalSessionOrchestrator};
use heron_session::{
    ListMeetingsQuery, MeetingStatus, Platform, SessionError, SessionOrchestrator,
};
use heron_types::{
    ActionItem, Attendee, Cost, DiarizeSource, Disclosure, DisclosureHow, Frontmatter, ItemId,
    MeetingType,
};
use heron_vault::{CalendarAttendee, CalendarReader, VaultWriter};
use serde_yaml::Mapping;
use tempfile::TempDir;

#[tokio::test]
async fn list_meetings_returns_notes_newest_first() {
    let fix = Fixture::new();
    fix.write_note(
        "standup",
        NaiveDate::from_ymd_opt(2026, 4, 25).unwrap(),
        "10:00",
        "Acme",
    );
    fix.write_note(
        "standup",
        NaiveDate::from_ymd_opt(2026, 4, 26).unwrap(),
        "09:30",
        "Acme",
    );
    fix.write_note(
        "kickoff",
        NaiveDate::from_ymd_opt(2026, 4, 26).unwrap(),
        "14:00",
        "Initech",
    );

    let orch = fix.orch();
    let page = orch
        .list_meetings(ListMeetingsQuery::default())
        .await
        .unwrap();
    assert_eq!(page.items.len(), 3);
    assert_eq!(page.items[0].title.as_deref(), Some("Initech"));
    assert_eq!(page.items[1].title.as_deref(), Some("Acme"));
    assert_eq!(page.items[2].title.as_deref(), Some("Acme"));
    assert!(page.next_cursor.is_none());
}

#[tokio::test]
async fn list_meetings_orders_slug_filenames_by_frontmatter_time() {
    let fix = Fixture::new();
    let old_date = NaiveDate::from_ymd_opt(2026, 4, 25).unwrap();
    let new_date = NaiveDate::from_ymd_opt(2026, 4, 26).unwrap();
    fix.write_note("z-old", old_date, "10:00", "old");
    fix.write_note("a-new", new_date, "10:00", "new");

    let meetings_dir = fix.vault_root().join("meetings");
    std::fs::rename(
        meetings_dir.join(note_filename(old_date, "10:00", "z-old")),
        meetings_dir.join("z-old.md"),
    )
    .unwrap();
    std::fs::rename(
        meetings_dir.join(note_filename(new_date, "10:00", "a-new")),
        meetings_dir.join("a-new.md"),
    )
    .unwrap();

    let page = fix
        .orch()
        .list_meetings(ListMeetingsQuery::default())
        .await
        .unwrap();

    assert_eq!(page.items.len(), 2);
    assert_eq!(page.items[0].title.as_deref(), Some("new"));
    assert_eq!(page.items[1].title.as_deref(), Some("old"));
}

#[tokio::test]
async fn list_meetings_filters_by_since() {
    let fix = Fixture::new();
    fix.write_note(
        "a",
        NaiveDate::from_ymd_opt(2026, 4, 25).unwrap(),
        "10:00",
        "old",
    );
    fix.write_note(
        "b",
        NaiveDate::from_ymd_opt(2026, 4, 27).unwrap(),
        "10:00",
        "new",
    );

    let cutoff = Utc.with_ymd_and_hms(2026, 4, 26, 0, 0, 0).unwrap();
    let page = fix
        .orch()
        .list_meetings(ListMeetingsQuery {
            since: Some(cutoff),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].title.as_deref(), Some("new"));
}

#[tokio::test]
async fn list_meetings_paginates_via_cursor() {
    let fix = Fixture::new();
    for d in 20..=25 {
        fix.write_note(
            "x",
            NaiveDate::from_ymd_opt(2026, 4, d).unwrap(),
            "10:00",
            &format!("day{d}"),
        );
    }
    let orch = fix.orch();
    let first = orch
        .list_meetings(ListMeetingsQuery {
            limit: Some(2),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(first.items.len(), 2);
    let cursor = first
        .next_cursor
        .expect("cursor present when more pages remain");

    let second = orch
        .list_meetings(ListMeetingsQuery {
            limit: Some(2),
            cursor: Some(cursor),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(second.items.len(), 2);
    let first_ids: Vec<_> = first.items.iter().map(|m| m.id).collect();
    for m in &second.items {
        assert!(
            !first_ids.contains(&m.id),
            "cursor failed to skip first page"
        );
    }
}

#[tokio::test]
async fn get_meeting_resolves_id_to_note() {
    let fix = Fixture::new();
    fix.write_note(
        "alpha",
        NaiveDate::from_ymd_opt(2026, 4, 26).unwrap(),
        "09:00",
        "alpha-co",
    );
    let orch = fix.orch();

    let listed = orch
        .list_meetings(ListMeetingsQuery::default())
        .await
        .unwrap();
    let id = listed.items[0].id;

    let got = orch.get_meeting(&id).await.unwrap();
    assert_eq!(got.id, id);
    assert_eq!(got.title.as_deref(), Some("alpha-co"));
    assert_eq!(got.status, MeetingStatus::Done);
    assert_eq!(got.platform, Platform::Zoom);
}

#[tokio::test]
async fn list_and_get_meeting_surface_frontmatter_tags() {
    // Pin the bridge end-to-end:
    //   * `list_meetings` returns the tags in writer order
    //   * `get_meeting` returns the same tags (separate code path
    //     through `meeting_from_note`)
    //   * a note WITHOUT tags surfaces an empty list (the
    //     `#[serde(default)]` contract).
    let fix = Fixture::new();
    let with_tags_date = NaiveDate::from_ymd_opt(2026, 4, 26).unwrap();
    fix.write_note_with_tags(
        "alpha",
        with_tags_date,
        "09:00",
        "alpha-co",
        &["acme", "pricing"],
    );
    fix.write_note(
        "no-tags",
        NaiveDate::from_ymd_opt(2026, 4, 25).unwrap(),
        "09:00",
        "beta-co",
    );

    let orch = fix.orch();
    let listed = orch
        .list_meetings(ListMeetingsQuery::default())
        .await
        .unwrap();
    let tagged = listed
        .items
        .iter()
        .find(|m| m.title.as_deref() == Some("alpha-co"))
        .expect("tagged note must appear in list");
    assert_eq!(
        tagged.tags,
        vec!["acme".to_owned(), "pricing".to_owned()],
        "tags must flow through list_meetings in writer order",
    );

    let untagged = listed
        .items
        .iter()
        .find(|m| m.title.as_deref() == Some("beta-co"))
        .expect("untagged note must appear in list");
    assert!(
        untagged.tags.is_empty(),
        "note with no tags should surface as empty (not missing): {:?}",
        untagged.tags,
    );

    // Same contract via `get_meeting`, which goes through a distinct
    // code path (`meeting_from_note` from `find_note_path_by_id`
    // rather than the listing scan). Pin both so a future refactor
    // that diverges them gets caught.
    let by_id = orch.get_meeting(&tagged.id).await.unwrap();
    assert_eq!(by_id.tags, tagged.tags);
}

#[tokio::test]
async fn meeting_processing_surfaces_persisted_cost() {
    // Tier 0 #2: a vault note carrying populated `Frontmatter.cost`
    // must surface as `Meeting.processing` on the wire. This is the
    // round-trip that powers the Review right-rail "Processing"
    // panel — vault YAML → `read_note` → `meeting_from_note` →
    // `Meeting.processing`.
    let fix = Fixture::new();
    fix.write_note_with_cost(
        "with-cost",
        NaiveDate::from_ymd_opt(2026, 4, 28).unwrap(),
        "11:00",
        "Acme",
        Cost {
            summary_usd: 0.0421,
            tokens_in: 14_231,
            tokens_out: 612,
            model: "claude-sonnet-4-6".to_owned(),
        },
    );
    let orch = fix.orch();
    let listed = orch
        .list_meetings(ListMeetingsQuery::default())
        .await
        .unwrap();
    let id = listed.items[0].id;
    let got = orch.get_meeting(&id).await.unwrap();
    let processing = got.processing.as_ref().expect("processing populated");
    assert_eq!(processing.tokens_in, 14_231);
    assert_eq!(processing.tokens_out, 612);
    assert_eq!(processing.model, "claude-sonnet-4-6");
    assert!((processing.summary_usd - 0.0421).abs() < 1e-9);
}

#[tokio::test]
async fn meeting_processing_is_none_for_unpopulated_cost() {
    // Pre-Tier-0-#2 vault notes (and the existing test fixtures'
    // default `Cost { 0, 0, 0, "" }`) write zero tokens / empty
    // model. Surfacing those as `Some(MeetingProcessing)` would
    // render a misleading "$0.00 by `<empty>`" row in the Review
    // panel; pin the bridge's "treat fully-zero cost as None"
    // behavior here.
    let fix = Fixture::new();
    fix.write_note(
        "no-cost",
        NaiveDate::from_ymd_opt(2026, 4, 28).unwrap(),
        "12:00",
        "Acme",
    );
    let orch = fix.orch();
    let listed = orch
        .list_meetings(ListMeetingsQuery::default())
        .await
        .unwrap();
    assert!(listed.items[0].processing.is_none());
}

#[tokio::test]
async fn get_meeting_unknown_id_returns_not_found() {
    let fix = Fixture::new();
    let unknown = heron_session::MeetingId::now_v7();
    let err = fix.orch().get_meeting(&unknown).await.unwrap_err();
    assert!(
        matches!(err, SessionError::NotFound { .. }),
        "expected NotFound, got {err:?}"
    );
}

#[tokio::test]
async fn read_transcript_maps_jsonl_turns_to_segments() {
    let fix = Fixture::new();
    let date = NaiveDate::from_ymd_opt(2026, 4, 26).unwrap();
    fix.write_note_with_transcript(
        "team",
        date,
        "10:00",
        "Acme",
        &[
            r#"{"t0":0.0,"t1":1.5,"text":"Hello","channel":"mic_clean","speaker":"Teng","speaker_source":"self","confidence":0.92}"#,
            r#"{"t0":1.6,"t1":3.0,"text":"Hi back","channel":"tap","speaker":"Alice","speaker_source":"ax","confidence":0.85}"#,
        ],
    );
    let orch = fix.orch();
    let listed = orch
        .list_meetings(ListMeetingsQuery::default())
        .await
        .unwrap();
    let id = listed.items[0].id;

    let transcript = orch.read_transcript(&id).await.unwrap();
    assert_eq!(transcript.meeting_id, id);
    assert_eq!(transcript.segments.len(), 2);
    assert_eq!(transcript.segments[0].text, "Hello");
    assert!(transcript.segments[0].speaker.is_user);
    assert_eq!(transcript.segments[1].speaker.display_name, "Alice");
    assert!(!transcript.segments[1].speaker.is_user);
    for seg in &transcript.segments {
        assert!(seg.is_final);
    }
}

#[tokio::test]
async fn read_summary_returns_body_and_action_items() {
    let fix = Fixture::new();
    let date = NaiveDate::from_ymd_opt(2026, 4, 26).unwrap();
    fix.write_note_with_actions(
        "team",
        date,
        "10:00",
        "Acme",
        "## Decisions\n\n- Ship v1\n",
        &[("Teng", "Write the doc", Some("2026-05-01"))],
    );
    let orch = fix.orch();
    let listed = orch
        .list_meetings(ListMeetingsQuery::default())
        .await
        .unwrap();
    let id = listed.items[0].id;

    let summary = orch
        .read_summary(&id)
        .await
        .unwrap()
        .expect("summary present");
    assert_eq!(summary.meeting_id, id);
    assert!(summary.text.contains("Ship v1"), "body: {}", summary.text);
    assert_eq!(summary.action_items.len(), 1);
    assert_eq!(summary.action_items[0].text, "Write the doc");
    assert_eq!(summary.action_items[0].owner.as_deref(), Some("Teng"));
    assert_eq!(
        summary.action_items[0].due,
        Some(NaiveDate::from_ymd_opt(2026, 5, 1).unwrap())
    );
}

#[tokio::test]
async fn list_meetings_carries_structured_action_items_with_stable_ids() {
    // Tier 0 #3 of the UX redesign: structured rows on the wire so
    // the desktop's Review/Actions tab can replace the regex bullet
    // extractor with a typed read. Pin the projection here:
    //
    // - `Frontmatter.action_items[i].id` (UUIDv7) survives onto
    //   `Meeting.action_items[i].id` so React lists / future
    //   checkbox state can key on a stable identifier across
    //   re-summarize cycles.
    // - Empty `owner` strings collapse to `None` on the wire.
    // - ISO-formatted `due` strings parse to `NaiveDate`.
    // - Reading list_meetings + get_meeting projects the same shape.
    let fix = Fixture::new();
    let date = NaiveDate::from_ymd_opt(2026, 4, 27).unwrap();
    let id_a = ItemId::now_v7();
    let id_b = ItemId::now_v7();
    fix.write_note_with_action_ids(
        "team",
        date,
        "10:00",
        "Acme",
        "Body.\n",
        &[
            (id_a, "Teng", "Write the doc", Some("2026-05-01")),
            (id_b, "", "Pick a reviewer", None),
        ],
    );

    let orch = fix.orch();
    let listed = orch
        .list_meetings(ListMeetingsQuery::default())
        .await
        .unwrap();
    let meeting = &listed.items[0];
    assert_eq!(meeting.action_items.len(), 2);
    assert_eq!(meeting.action_items[0].id, id_a);
    assert_eq!(meeting.action_items[0].text, "Write the doc");
    assert_eq!(meeting.action_items[0].owner.as_deref(), Some("Teng"));
    assert_eq!(
        meeting.action_items[0].due,
        Some(NaiveDate::from_ymd_opt(2026, 5, 1).unwrap()),
    );
    // Empty owner on disk → `None` on the wire.
    assert_eq!(meeting.action_items[1].id, id_b);
    assert_eq!(meeting.action_items[1].owner, None);
    assert_eq!(meeting.action_items[1].due, None);

    // get_meeting must surface the same projection.
    let fetched = orch.get_meeting(&meeting.id).await.unwrap();
    assert_eq!(fetched.action_items.len(), 2);
    assert_eq!(fetched.action_items[0].id, id_a);
    assert_eq!(fetched.action_items[1].id, id_b);
}

#[tokio::test]
async fn audio_path_returns_recording_when_present() {
    let fix = Fixture::new();
    let date = NaiveDate::from_ymd_opt(2026, 4, 26).unwrap();
    let rec_rel = "audio/2026-04-26-1000 Acme.m4a";
    let rec_abs = fix.vault_root().join(rec_rel);
    std::fs::create_dir_all(rec_abs.parent().unwrap()).unwrap();
    std::fs::write(&rec_abs, b"fake m4a").unwrap();
    fix.write_note_with_recording("team", date, "10:00", "Acme", PathBuf::from(rec_rel));
    let orch = fix.orch();
    let listed = orch
        .list_meetings(ListMeetingsQuery::default())
        .await
        .unwrap();
    let id = listed.items[0].id;

    let path = orch.audio_path(&id).await.unwrap();
    assert_eq!(
        path.canonicalize().unwrap(),
        rec_abs.canonicalize().unwrap()
    );
}

#[tokio::test]
async fn audio_path_returns_not_found_when_recording_missing() {
    let fix = Fixture::new();
    let date = NaiveDate::from_ymd_opt(2026, 4, 26).unwrap();
    fix.write_note_with_recording(
        "team",
        date,
        "10:00",
        "Acme",
        PathBuf::from("audio/missing.m4a"),
    );
    let orch = fix.orch();
    let listed = orch
        .list_meetings(ListMeetingsQuery::default())
        .await
        .unwrap();
    let id = listed.items[0].id;

    let err = orch.audio_path(&id).await.unwrap_err();
    assert!(
        matches!(err, SessionError::NotFound { .. }),
        "expected NotFound, got {err:?}"
    );
}

#[tokio::test]
async fn read_transcript_rejects_absolute_path_in_frontmatter() {
    let fix = Fixture::new();
    let date = NaiveDate::from_ymd_opt(2026, 4, 26).unwrap();
    fix.write_note_with_evil_transcript(
        "team",
        date,
        "10:00",
        "Acme",
        PathBuf::from("/etc/passwd"),
    );
    let orch = fix.orch();
    let listed = orch
        .list_meetings(ListMeetingsQuery::default())
        .await
        .unwrap();
    let id = listed.items[0].id;

    let err = orch.read_transcript(&id).await.unwrap_err();
    assert!(
        matches!(err, SessionError::Validation { .. }),
        "expected Validation, got {err:?}"
    );
}

#[tokio::test]
async fn read_transcript_rejects_parent_dir_traversal() {
    let fix = Fixture::new();
    let date = NaiveDate::from_ymd_opt(2026, 4, 26).unwrap();
    fix.write_note_with_evil_transcript(
        "team",
        date,
        "10:00",
        "Acme",
        PathBuf::from("../../etc/passwd"),
    );
    let orch = fix.orch();
    let listed = orch
        .list_meetings(ListMeetingsQuery::default())
        .await
        .unwrap();
    let id = listed.items[0].id;

    let err = orch.read_transcript(&id).await.unwrap_err();
    assert!(
        matches!(err, SessionError::Validation { .. }),
        "expected Validation, got {err:?}"
    );
}

#[tokio::test]
async fn audio_path_rejects_absolute_recording_in_frontmatter() {
    let fix = Fixture::new();
    let date = NaiveDate::from_ymd_opt(2026, 4, 26).unwrap();
    fix.write_note_with_recording("team", date, "10:00", "Acme", PathBuf::from("/etc/hosts"));
    let orch = fix.orch();
    let listed = orch
        .list_meetings(ListMeetingsQuery::default())
        .await
        .unwrap();
    let id = listed.items[0].id;

    let err = orch.audio_path(&id).await.unwrap_err();
    assert!(
        matches!(err, SessionError::Validation { .. }),
        "expected Validation, got {err:?}"
    );
}

#[tokio::test]
async fn meeting_status_reflects_missing_transcript_file() {
    let fix = Fixture::new();
    let date = NaiveDate::from_ymd_opt(2026, 4, 26).unwrap();
    fix.write_note("orphan", date, "10:00", "Acme");
    let orch = fix.orch();
    let listed = orch
        .list_meetings(ListMeetingsQuery::default())
        .await
        .unwrap();
    assert_eq!(
        listed.items[0].transcript_status,
        heron_session::TranscriptLifecycle::Failed
    );
}

#[tokio::test]
async fn platform_inferred_from_source_app() {
    let fix = Fixture::new();
    let date = NaiveDate::from_ymd_opt(2026, 4, 26).unwrap();
    fix.write_note_with_source_app("team", date, "10:00", "Acme", "us.zoom.xos");
    fix.write_note_with_source_app(
        "g",
        date,
        "11:00",
        "Foo",
        "com.google.Chrome.app.meet.google.com",
    );
    fix.write_note_with_source_app("t", date, "12:00", "Bar", "com.microsoft.teams2");
    fix.write_note_with_source_app("w", date, "13:00", "Baz", "com.webex.meetingmanager");

    let listed = fix
        .orch()
        .list_meetings(ListMeetingsQuery::default())
        .await
        .unwrap();
    assert_eq!(listed.items[0].platform, Platform::Webex);
    assert_eq!(listed.items[1].platform, Platform::MicrosoftTeams);
    assert_eq!(listed.items[2].platform, Platform::GoogleMeet);
    assert_eq!(listed.items[3].platform, Platform::Zoom);
}

#[tokio::test]
async fn list_upcoming_calendar_uses_injected_reader() {
    let fix = Fixture::new();
    let cal: Arc<dyn CalendarReader> = Arc::new(FakeCalendar {
        events: vec![heron_vault::CalendarEvent {
            title: "1:1".into(),
            start: 1745660400.0,
            end: 1745664000.0,
            attendees: vec![CalendarAttendee {
                name: "Alice".into(),
                email: "alice@example.com".into(),
            }],
        }],
    });
    let orch = fix.orch_with_calendar(cal);

    let events = orch.list_upcoming_calendar(None, None, None).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].title, "1:1");
    assert_eq!(events[0].attendees.len(), 1);
    assert_eq!(events[0].attendees[0].name, "Alice");
}

#[tokio::test]
async fn list_upcoming_calendar_translates_denied_to_permission_missing() {
    let fix = Fixture::new();
    let cal: Arc<dyn CalendarReader> = Arc::new(DenyingCalendar);
    let orch = fix.orch_with_calendar(cal);

    let err = orch
        .list_upcoming_calendar(None, None, None)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            SessionError::PermissionMissing {
                permission: "calendar"
            }
        ),
        "expected PermissionMissing, got {err:?}"
    );
}

#[tokio::test]
async fn list_upcoming_calendar_marks_primed_for_staged_event_ids() {
    // The rail's "primed" indicator reads from `CalendarEvent.primed`.
    // The orchestrator must mirror `pending_contexts.contains_key(id)`
    // onto each event so the indicator survives a refetch — without
    // this, the rail would lose the badge every time the calendar
    // store re-runs `ensureFresh` past the TTL.
    let fix = Fixture::new();
    let cal: Arc<dyn CalendarReader> = Arc::new(FakeCalendar {
        events: vec![
            heron_vault::CalendarEvent {
                title: "1:1 with Alex".into(),
                start: 1745660400.0,
                end: 1745664000.0,
                attendees: Vec::new(),
            },
            heron_vault::CalendarEvent {
                title: "Team standup".into(),
                start: 1745670000.0,
                end: 1745671800.0,
                attendees: Vec::new(),
            },
        ],
    });
    let orch = fix.orch_with_calendar(cal);

    // Before any priming: both events come back un-primed.
    let events = orch
        .list_upcoming_calendar(None, None, None)
        .await
        .expect("list");
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|e| !e.primed), "no entry staged yet");

    // Prime the first event by id; second stays cold.
    let primed_id = events[0].id.clone();
    orch.prepare_context(heron_session::PrepareContextRequest {
        calendar_event_id: primed_id.clone(),
        attendees: Vec::new(),
    })
    .await
    .expect("prepare");

    let events = orch
        .list_upcoming_calendar(None, None, None)
        .await
        .expect("list after prepare");
    let primed: Vec<&str> = events
        .iter()
        .filter(|e| e.primed)
        .map(|e| e.id.as_str())
        .collect();
    assert_eq!(primed, vec![primed_id.as_str()]);
}

#[tokio::test]
async fn set_event_auto_record_persists_and_mirrors_to_calendar_event() {
    // The rail's per-event toggle reads `CalendarEvent.auto_record`,
    // which `list_upcoming_calendar` mirrors from the orchestrator's
    // registry. Pin both ends of the round trip: a `POST /auto-record`
    // call should make the next list response report the event as
    // armed.
    let fix = Fixture::new();
    let cal: Arc<dyn CalendarReader> = Arc::new(FakeCalendar {
        events: vec![heron_vault::CalendarEvent {
            title: "Investor sync".into(),
            start: 1745660400.0,
            end: 1745664000.0,
            attendees: Vec::new(),
        }],
    });
    let orch = fix.orch_with_calendar(cal);

    let events = orch
        .list_upcoming_calendar(None, None, None)
        .await
        .expect("list");
    assert_eq!(events.len(), 1);
    assert!(!events[0].auto_record, "starts off");
    let id = events[0].id.clone();

    orch.set_event_auto_record(heron_session::SetEventAutoRecordRequest {
        calendar_event_id: id.clone(),
        enabled: true,
    })
    .await
    .expect("set");

    let listed = orch.list_auto_record_events().await.expect("list registry");
    assert_eq!(listed.event_ids, vec![id.clone()]);

    let events = orch
        .list_upcoming_calendar(None, None, None)
        .await
        .expect("list after set");
    assert!(
        events[0].auto_record,
        "registry membership mirrors onto event"
    );

    // Disable round-trips back through the same path.
    orch.set_event_auto_record(heron_session::SetEventAutoRecordRequest {
        calendar_event_id: id.clone(),
        enabled: false,
    })
    .await
    .expect("set off");
    let events = orch
        .list_upcoming_calendar(None, None, None)
        .await
        .expect("list after disable");
    assert!(!events[0].auto_record);
}

#[tokio::test]
async fn auto_record_registry_persists_across_orchestrator_rebuild() {
    // The registry is the user's "auto-record this on the next
    // occurrence" choice — it must survive daemon restart, otherwise
    // the toggle silently resets every time the daemon updates. Pin
    // the on-disk persistence path by toggling on one orchestrator,
    // dropping it, and rebuilding against the same vault root.
    let fix = Fixture::new();
    {
        let orch = fix.orch();
        orch.set_event_auto_record(heron_session::SetEventAutoRecordRequest {
            calendar_event_id: "evt_persistent".to_owned(),
            enabled: true,
        })
        .await
        .expect("set");
    } // orch dropped — simulates daemon shutdown
    let orch2 = fix.orch();
    let listed = orch2.list_auto_record_events().await.expect("list");
    assert_eq!(
        listed.event_ids,
        vec!["evt_persistent"],
        "registry must rehydrate from <vault_root>/.heron/auto_record.json",
    );
}

#[tokio::test]
async fn set_event_auto_record_rejects_empty_calendar_event_id() {
    let fix = Fixture::new();
    let orch = fix.orch();
    let err = orch
        .set_event_auto_record(heron_session::SetEventAutoRecordRequest {
            calendar_event_id: "   ".to_owned(),
            enabled: true,
        })
        .await
        .expect_err("blank id must be rejected");
    assert!(
        matches!(err, SessionError::Validation { .. }),
        "expected Validation, got {err:?}",
    );
}

#[tokio::test]
async fn attach_context_persists_against_vault_orchestrator() {
    // The vault-backed orchestrator stages context the same way the
    // vault-less one does: in-memory, keyed by `calendar_event_id`,
    // independent of the vault root. Pin that here so a future vault
    // refactor doesn't accidentally route this through disk.
    let fix = Fixture::new();
    let orch = fix.orch();

    orch.attach_context(heron_session::PreMeetingContextRequest {
        calendar_event_id: "synth_x".into(),
        context: heron_session::PreMeetingContext {
            agenda: Some("kickoff".into()),
            ..Default::default()
        },
    })
    .await
    .expect("attach_context");

    let staged = orch.pending_context("synth_x").expect("staged context");
    assert_eq!(staged.agenda.as_deref(), Some("kickoff"));
}

#[tokio::test]
async fn health_reports_vault_ok_when_root_exists() {
    let fix = Fixture::new();
    let h = fix.orch().health().await;
    assert!(matches!(
        h.components.vault.state,
        heron_session::ComponentState::Ok
    ));
    // Capture is wired when a vault root exists; remaining component
    // states depend on local platform/tool availability and are covered
    // by unit tests in `src/lib.rs`.
    assert!(matches!(
        h.components.capture.state,
        heron_session::ComponentState::Ok
    ));
}

// ── fixture ───────────────────────────────────────────────────────────

struct Fixture {
    dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("meetings")).unwrap();
        Self { dir }
    }

    fn vault_root(&self) -> &Path {
        self.dir.path()
    }

    fn orch(&self) -> LocalSessionOrchestrator {
        // Default to a denying calendar so a stray
        // list_upcoming_calendar in a test that didn't inject one
        // surfaces PermissionMissing rather than calling out to
        // EventKit (which would hang on a CI runner without TCC).
        self.orch_with_calendar(Arc::new(DenyingCalendar))
    }

    fn orch_with_calendar(&self, cal: Arc<dyn CalendarReader>) -> LocalSessionOrchestrator {
        Builder::default()
            .vault_root(self.vault_root().to_path_buf())
            .calendar(cal)
            .build()
    }

    fn write_note(&self, slug: &str, date: NaiveDate, start: &str, company: &str) {
        self.write_note_inner(slug, date, start, company, "Body.\n", &[], None, &[]);
    }

    /// Write a note and stamp a populated `Cost` block onto its
    /// frontmatter. The default fixture's `Cost { 0, 0, 0, "" }` is
    /// the "unpopulated" sentinel the orchestrator collapses to
    /// `Meeting.processing = None`; this helper opts out of that so
    /// tests can assert the populated wire shape.
    ///
    /// Unlike the other `write_note_with_*` helpers in this fixture,
    /// this one builds the `Frontmatter` directly and finalizes once
    /// rather than going through the `write_note_inner`
    /// → `read_note` → mutate → `atomic_write` round-trip — `cost` is
    /// the only thing diverging from the baseline, so a one-shot write
    /// is both shorter and avoids the read/parse churn the others
    /// inherit. The other helpers' read-modify-write shape is harder
    /// to migrate (they each diverge a different field) and out of
    /// scope here.
    fn write_note_with_cost(
        &self,
        slug: &str,
        date: NaiveDate,
        start: &str,
        company: &str,
        cost: Cost,
    ) {
        let writer = VaultWriter::new(self.vault_root().to_path_buf());
        let frontmatter = Frontmatter {
            date,
            start: start.to_owned(),
            duration_min: 30,
            company: Some(company.to_owned()),
            attendees: Vec::new(),
            meeting_type: MeetingType::Internal,
            source_app: "zoom.us".to_owned(),
            recording: PathBuf::from(format!(
                "audio/{}-{} {slug}.m4a",
                date,
                start.replace(':', ""),
            )),
            transcript: PathBuf::from(format!(
                "transcripts/{}-{} {slug}.jsonl",
                date,
                start.replace(':', ""),
            )),
            diarize_source: DiarizeSource::Ax,
            disclosed: Disclosure {
                stated: true,
                when: None,
                how: DisclosureHow::Verbal,
            },
            cost,
            action_items: Vec::new(),
            tags: Vec::new(),
            extra: Mapping::new(),
        };
        writer
            .finalize_session(
                &date.to_string(),
                &start.replace(':', ""),
                slug,
                &frontmatter,
                "Body.\n",
            )
            .expect("finalize");
    }

    fn write_note_with_transcript(
        &self,
        slug: &str,
        date: NaiveDate,
        start: &str,
        company: &str,
        turns_jsonl: &[&str],
    ) {
        let transcript_rel = format!(
            "transcripts/{}-{} {slug}.jsonl",
            date,
            start.replace(':', ""),
        );
        let transcript_abs = self.vault_root().join(&transcript_rel);
        std::fs::create_dir_all(transcript_abs.parent().unwrap()).unwrap();
        let body = turns_jsonl.join("\n");
        std::fs::write(&transcript_abs, body).unwrap();
        self.write_note_inner(
            slug,
            date,
            start,
            company,
            "Body.\n",
            &[],
            Some(PathBuf::from(transcript_rel)),
            &[],
        );
    }

    fn write_note_with_actions(
        &self,
        slug: &str,
        date: NaiveDate,
        start: &str,
        company: &str,
        body: &str,
        actions: &[(&str, &str, Option<&str>)],
    ) {
        let items: Vec<ActionItem> = actions
            .iter()
            .map(|(owner, text, due)| ActionItem {
                id: ItemId::nil(),
                owner: (*owner).to_owned(),
                text: (*text).to_owned(),
                due: due.map(str::to_owned),
            })
            .collect();
        self.write_note_inner(slug, date, start, company, body, &items, None, &[]);
    }

    fn write_note_with_action_ids(
        &self,
        slug: &str,
        date: NaiveDate,
        start: &str,
        company: &str,
        body: &str,
        actions: &[(ItemId, &str, &str, Option<&str>)],
    ) {
        let items: Vec<ActionItem> = actions
            .iter()
            .map(|(id, owner, text, due)| ActionItem {
                id: *id,
                owner: (*owner).to_owned(),
                text: (*text).to_owned(),
                due: due.map(str::to_owned),
            })
            .collect();
        self.write_note_inner(slug, date, start, company, body, &items, None, &[]);
    }

    fn write_note_with_recording(
        &self,
        slug: &str,
        date: NaiveDate,
        start: &str,
        company: &str,
        recording: PathBuf,
    ) {
        self.write_note_inner(slug, date, start, company, "Body.\n", &[], None, &[]);
        let path = note_filename(date, start, slug);
        let abs = self.vault_root().join("meetings").join(&path);
        let (mut fm, body) = heron_vault::read_note(&abs).unwrap();
        fm.recording = recording;
        let rendered = heron_vault::render_note(&fm, &body).unwrap();
        std::fs::write(&abs, rendered).unwrap();
    }

    fn write_note_with_evil_transcript(
        &self,
        slug: &str,
        date: NaiveDate,
        start: &str,
        company: &str,
        transcript: PathBuf,
    ) {
        self.write_note_inner(slug, date, start, company, "Body.\n", &[], None, &[]);
        let path = note_filename(date, start, slug);
        let abs = self.vault_root().join("meetings").join(&path);
        let (mut fm, body) = heron_vault::read_note(&abs).unwrap();
        fm.transcript = transcript;
        let rendered = heron_vault::render_note(&fm, &body).unwrap();
        std::fs::write(&abs, rendered).unwrap();
    }

    fn write_note_with_source_app(
        &self,
        slug: &str,
        date: NaiveDate,
        start: &str,
        company: &str,
        source_app: &str,
    ) {
        self.write_note_inner(slug, date, start, company, "Body.\n", &[], None, &[]);
        let path = note_filename(date, start, slug);
        let abs = self.vault_root().join("meetings").join(&path);
        let (mut fm, body) = heron_vault::read_note(&abs).unwrap();
        fm.source_app = source_app.to_owned();
        let rendered = heron_vault::render_note(&fm, &body).unwrap();
        std::fs::write(&abs, rendered).unwrap();
    }

    /// Write a default note, then patch in `tags`. Mirrors the
    /// edit-and-rewrite pattern of [`Self::write_note_with_recording`]
    /// so the test fixture stays additive — tags are llm-inferred,
    /// so a happy-path note ships them via the same path the real
    /// summarizer→merge pipeline writes them through.
    fn write_note_with_tags(
        &self,
        slug: &str,
        date: NaiveDate,
        start: &str,
        company: &str,
        tags: &[&str],
    ) {
        self.write_note_inner(slug, date, start, company, "Body.\n", &[], None, &[]);
        let path = note_filename(date, start, slug);
        let abs = self.vault_root().join("meetings").join(&path);
        let (mut fm, body) = heron_vault::read_note(&abs).unwrap();
        fm.tags = tags.iter().map(|s| (*s).to_owned()).collect();
        let rendered = heron_vault::render_note(&fm, &body).unwrap();
        std::fs::write(&abs, rendered).unwrap();
    }

    #[allow(clippy::too_many_arguments)]
    fn write_note_inner(
        &self,
        slug: &str,
        date: NaiveDate,
        start: &str,
        company: &str,
        body: &str,
        action_items: &[ActionItem],
        transcript: Option<PathBuf>,
        attendees: &[Attendee],
    ) {
        let writer = VaultWriter::new(self.vault_root().to_path_buf());
        let frontmatter = Frontmatter {
            date,
            start: start.to_owned(),
            duration_min: 30,
            company: Some(company.to_owned()),
            attendees: attendees.to_vec(),
            meeting_type: MeetingType::Internal,
            source_app: "zoom.us".to_owned(),
            recording: PathBuf::from(format!(
                "audio/{}-{} {slug}.m4a",
                date,
                start.replace(':', ""),
            )),
            transcript: transcript.unwrap_or_else(|| {
                PathBuf::from(format!(
                    "transcripts/{}-{} {slug}.jsonl",
                    date,
                    start.replace(':', ""),
                ))
            }),
            diarize_source: DiarizeSource::Ax,
            disclosed: Disclosure {
                stated: true,
                when: None,
                how: DisclosureHow::Verbal,
            },
            cost: Cost {
                summary_usd: 0.0,
                tokens_in: 0,
                tokens_out: 0,
                model: String::new(),
            },
            action_items: action_items.to_vec(),
            tags: Vec::new(),
            extra: Mapping::new(),
        };
        writer
            .finalize_session(
                &date.to_string(),
                &start.replace(':', ""),
                slug,
                &frontmatter,
                body,
            )
            .expect("finalize");
    }
}

fn note_filename(date: NaiveDate, start: &str, slug: &str) -> String {
    format!("{}-{} {slug}.md", date, start.replace(':', ""))
}

// ── calendar reader fakes ─────────────────────────────────────────────

struct FakeCalendar {
    events: Vec<heron_vault::CalendarEvent>,
}

impl CalendarReader for FakeCalendar {
    fn read_window(
        &self,
        _start_utc: chrono::DateTime<chrono::Utc>,
        _end_utc: chrono::DateTime<chrono::Utc>,
    ) -> Result<Option<Vec<heron_vault::CalendarEvent>>, heron_vault::CalendarError> {
        Ok(Some(self.events.clone()))
    }
}

struct DenyingCalendar;

impl CalendarReader for DenyingCalendar {
    fn read_window(
        &self,
        _start_utc: chrono::DateTime<chrono::Utc>,
        _end_utc: chrono::DateTime<chrono::Utc>,
    ) -> Result<Option<Vec<heron_vault::CalendarEvent>>, heron_vault::CalendarError> {
        Err(heron_vault::CalendarError::Denied)
    }
}
