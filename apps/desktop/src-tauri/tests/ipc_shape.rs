//! IPC payload snapshot tests (issue #186).
//!
//! Locks the JSON wire format of the high-traffic Tauri command
//! request / response payloads so a serde-shape regression — most
//! notably a future `#[serde(rename = "...")]` that changes the wire
//! key without changing the Rust ident — lands as a visible snapshot
//! diff during review instead of a silent break in the renderer's
//! `invoke()` parsing.
//!
//! Sister test to `ipc_contract.rs`: that one pins the *set* of
//! command names against `invoke.ts::HeronCommands`; this one pins the
//! *shape* of each command's request / response body.
//!
//! ## Targets
//!
//! Mirrors the issue spec / `docs/testing-roadmap.md` deferred list:
//!
//! - `heron_meeting_summary` (issue's "heron_summarize") —
//!   `Summary` + `DaemonOutcome<Summary>` (Ok + Unavailable variants).
//! - `heron_update_action_item` — `ActionItemPatch` request + the
//!   `ActionItemView` response.
//! - `heron_get_meeting` — `Meeting` + `DaemonOutcome<Meeting>`.
//! - `heron_write_settings` — `Settings` request body.
//! - `heron_prepare_context` — `PrepareContextRequest` body +
//!   `AttachContextAck` response.
//!
//! ## Why round-trip?
//!
//! Each shape that derives both `Serialize` + `Deserialize` is also
//! round-tripped — `to_value -> from_value -> to_value` — so a
//! serde-shape change that decodes lossily (e.g. `#[serde(skip)]` on a
//! field carrying real data) fails the round-trip assertion before it
//! ever touches a snapshot. `DaemonOutcome` is serialize-only, so the
//! snapshot is the only check.
//!
//! ## Why deserialize-from-partial fixtures too?
//!
//! Snapshotting fully-populated structs locks the *wire-out* shape but
//! cannot catch regressions in `#[serde(default)]` — dropping the
//! attribute would break decoding of older settings.json files, but
//! the snapshot of a fully-populated value would be unaffected. The
//! `from_partial_json_uses_defaults_*` tests below decode a
//! deliberately-thin JSON object and assert the missing fields fell
//! through to their defaults; a future PR that drops `#[serde(default)]`
//! makes those tests fail at `from_value` time.
//!
//! ## How to update
//!
//! 1. Run `cargo test -p heron-desktop --test ipc_shape`. Failing
//!    snapshots leave a `.snap.new` next to the existing `.snap`.
//! 2. `cargo install cargo-insta` (one-time).
//! 3. `cargo insta review` — interactive accept / reject for each
//!    drift; reviewed `.snap` files replace the originals. Commit the
//!    accepted snaps in the same PR as the wire-format change.
//!
//! Snapshots live under `apps/desktop/src-tauri/tests/snapshots/` and
//! are checked into the repo so the diff shows up at PR-review time.

#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::str::FromStr;

use chrono::{NaiveDate, TimeZone, Utc};
use heron_desktop_lib::Settings;
use heron_desktop_lib::action_items::ActionItemView;
use heron_desktop_lib::meetings::{AttachContextAck, DaemonOutcome};
use heron_desktop_lib::settings::{ActiveMode, FileNamingPattern, Persona};
use heron_session::{
    ActionItem, AttendeeContext, IdentifierKind, Meeting, MeetingId, MeetingProcessing,
    MeetingStatus, Participant, Platform, PrepareContextRequest, Summary, SummaryLifecycle,
    TranscriptLifecycle,
};
use heron_vault::ActionItemPatch;
use insta::assert_json_snapshot;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use uuid::Uuid;

/// Stable `MeetingId` for fixtures: `mtg_<all-zeroes>`. UUIDv7 minting
/// is non-deterministic, so we hand-pin so the snapshot bytes are
/// reproducible across machines.
fn fixture_meeting_id() -> MeetingId {
    MeetingId::nil()
}

/// Stable item-id UUIDs for fixtures. Hand-pinned so the snapshot is
/// reproducible. The two values differ so a future "id field collapse"
/// regression (one UUID accidentally written into both rows) is
/// visible in the diff.
fn fixture_item_id_a() -> Uuid {
    Uuid::from_str("11111111-1111-7111-8111-111111111111").expect("valid uuid")
}

fn fixture_item_id_b() -> Uuid {
    Uuid::from_str("22222222-2222-7222-8222-222222222222").expect("valid uuid")
}

/// Round-trip a value through JSON and assert the re-encoded form
/// matches the original. Catches lossy fields (e.g. `#[serde(skip)]`
/// on a real wire field) that a snapshot alone wouldn't notice — the
/// snapshot would happily drift to the new lossy shape on `cargo
/// insta accept`, but the round-trip would fail loudly first.
fn assert_round_trips<T>(value: &T) -> Value
where
    T: Serialize + DeserializeOwned,
{
    let first = serde_json::to_value(value).expect("serialize");
    let decoded: T = serde_json::from_value(first.clone()).expect("deserialize");
    let second = serde_json::to_value(&decoded).expect("re-serialize");
    assert_eq!(
        first, second,
        "round-trip drift: re-encoded JSON differs from initial encoding",
    );
    first
}

// ── heron_write_settings ──────────────────────────────────────────────

/// Build a fully-populated `Settings` with every optional field set so
/// the snapshot covers all serde keys, including the
/// `#[serde(default)]`-decorated fields that would silently disappear
/// from a snapshot built from `Settings::default()` alone.
fn fixture_settings() -> Settings {
    Settings {
        stt_backend: "whisperkit".to_owned(),
        llm_backend: "anthropic".to_owned(),
        auto_summarize: true,
        vault_root: "/Users/example/Vault".to_owned(),
        record_hotkey: "CmdOrCtrl+Shift+R".to_owned(),
        remind_interval_secs: 30,
        recover_on_launch: true,
        min_free_disk_mib: 2048,
        session_logging: true,
        crash_telemetry: false,
        audio_retention_days: Some(14),
        onboarded: true,
        target_bundle_ids: vec!["us.zoom.xos".to_owned(), "com.microsoft.teams2".to_owned()],
        active_mode: ActiveMode::Clio,
        hotwords: vec!["Heron".to_owned(), "Athena".to_owned()],
        persona: Persona {
            name: "Alex".to_owned(),
            role: "Engineer".to_owned(),
            working_on: "Tauri IPC snapshots".to_owned(),
        },
        file_naming_pattern: FileNamingPattern::DateSlug,
        summary_retention_days: Some(90),
        strip_names_before_summarization: true,
        show_tray_indicator: true,
        auto_detect_meeting_app: true,
        openai_model: "gpt-4o-mini".to_owned(),
        shortcuts: BTreeMap::from([
            (
                "toggle_recording".to_owned(),
                "CmdOrCtrl+Shift+R".to_owned(),
            ),
            ("summarize_now".to_owned(), "CmdOrCtrl+Shift+S".to_owned()),
        ]),
    }
}

#[test]
fn settings_request_shape_is_stable() {
    let settings = fixture_settings();
    let value = assert_round_trips(&settings);
    assert_json_snapshot!("heron_write_settings__request", value);
}

// ── heron_update_action_item ──────────────────────────────────────────

/// `ActionItemPatch` covers four wire shapes:
/// - `text`: `Option<String>`
/// - `owner`: `Option<Option<String>>` (RFC 7396 double-option: `null`
///   clears, missing leaves untouched, value sets)
/// - `due`: `Option<Option<String>>`
/// - `done`: `Option<bool>`
///
/// We pin one snapshot per *combination class* so a regression in any
/// of the three states (missing / null / set) is visible.

#[test]
fn update_action_item_patch_set_fields_shape_is_stable() {
    let patch = ActionItemPatch {
        text: Some("Send the recap email".to_owned()),
        owner: Some(Some("Alex".to_owned())),
        due: Some(Some("2026-05-15".to_owned())),
        done: Some(true),
    };
    let value = assert_round_trips(&patch);
    assert_json_snapshot!("heron_update_action_item__patch_set_fields", value);
}

#[test]
fn update_action_item_patch_clear_fields_shape_is_stable() {
    // `Some(None)` for owner / due is the RFC-7396 "clear" signal —
    // the wire form is JSON `null`, which the custom
    // `deserialize_double_option` distinguishes from "missing".
    let patch = ActionItemPatch {
        text: None,
        owner: Some(None),
        due: Some(None),
        done: Some(false),
    };
    let value = assert_round_trips(&patch);
    assert_json_snapshot!("heron_update_action_item__patch_clear_fields", value);
}

#[test]
fn update_action_item_patch_default_serialized_shape_is_stable() {
    // `ActionItemPatch::default()` serializes every `Option` field as
    // JSON `null`. Important wrinkle: because of the custom
    // `deserialize_double_option` on `owner` and `due`, a present-and-
    // null value on the wire decodes to `Some(None)` — i.e. *clear* —
    // not "missing". So this serialized form is **not** a no-op patch
    // when the renderer sends it. The semantic no-op test lives in
    // [`update_action_item_patch_no_op_decodes_to_default`] below; this
    // snapshot only pins the *encoded* shape so a future migration to
    // `#[serde(skip_serializing_if = "Option::is_none")]` (which would
    // also fix the no-op semantics) lands as a visible diff.
    let patch = ActionItemPatch::default();
    let value = assert_round_trips(&patch);
    assert_json_snapshot!("heron_update_action_item__patch_default_serialized", value);
}

#[test]
fn update_action_item_patch_no_op_decodes_to_default() {
    // The renderer's "no-op patch" wire form is `{}` — every field
    // omitted. The `#[serde(default)]` on the container plus the outer
    // `Option<...>::None` per field is what makes that decode to a
    // patch that touches nothing. Pin the round-trip so a regression
    // (e.g. dropping the container-level `#[serde(default)]`) fails
    // here rather than silently widening the wire surface.
    let decoded: ActionItemPatch =
        serde_json::from_value(json!({})).expect("empty patch must decode");
    assert_eq!(decoded, ActionItemPatch::default());
    assert_eq!(decoded.text, None);
    assert_eq!(
        decoded.owner, None,
        "missing owner must be None (untouched)"
    );
    assert_eq!(decoded.due, None, "missing due must be None (untouched)");
    assert_eq!(decoded.done, None);
}

#[test]
fn update_action_item_patch_owner_null_decodes_as_clear() {
    // The RFC-7396 "clear owner" wire form: an explicit `null`. The
    // double-option deserializer must turn this into `Some(None)` so
    // the writer knows to wipe the field on disk — distinct from
    // missing-field-leaves-untouched. Regression here would silently
    // turn "clear" into "no-op" (or vice versa), which is exactly the
    // class of drift this test file exists to catch.
    let decoded: ActionItemPatch =
        serde_json::from_value(json!({ "owner": null })).expect("null owner must decode");
    assert_eq!(decoded.owner, Some(None), "null owner must decode as clear");
    assert_eq!(
        decoded.due, None,
        "missing due must remain None (untouched)"
    );
}

#[test]
fn update_action_item_response_shape_is_stable() {
    let view = ActionItemView {
        id: fixture_item_id_a(),
        text: "Send the recap email".to_owned(),
        owner: Some("Alex".to_owned()),
        due: Some("2026-05-15".to_owned()),
        done: true,
    };
    let value = assert_round_trips(&view);
    assert_json_snapshot!("heron_update_action_item__response", value);
}

#[test]
fn update_action_item_response_nullable_fields_shape_is_stable() {
    // `owner` and `due` as `None` — pin that they serialize as JSON
    // `null` (not omitted) so the renderer's discriminated-union
    // parser keeps seeing the field.
    let view = ActionItemView {
        id: fixture_item_id_b(),
        text: "Decide on launch date".to_owned(),
        owner: None,
        due: None,
        done: false,
    };
    let value = assert_round_trips(&view);
    assert_json_snapshot!("heron_update_action_item__response_nullable_fields", value,);
}

// ── heron_get_meeting ─────────────────────────────────────────────────

fn fixture_meeting() -> Meeting {
    Meeting {
        id: fixture_meeting_id(),
        status: MeetingStatus::Done,
        platform: Platform::Zoom,
        title: Some("Weekly Team Sync".to_owned()),
        calendar_event_id: Some("cal-event-abc-123".to_owned()),
        // Pinned timestamp so the snapshot is reproducible across runs.
        started_at: Utc.with_ymd_and_hms(2026, 5, 1, 14, 0, 0).unwrap(),
        ended_at: Some(Utc.with_ymd_and_hms(2026, 5, 1, 14, 30, 0).unwrap()),
        duration_secs: Some(1800),
        participants: vec![
            Participant {
                display_name: "me".to_owned(),
                identifier_kind: IdentifierKind::Mic,
                is_user: true,
            },
            Participant {
                display_name: "Alice".to_owned(),
                identifier_kind: IdentifierKind::AxTree,
                is_user: false,
            },
        ],
        transcript_status: TranscriptLifecycle::Complete,
        summary_status: SummaryLifecycle::Ready,
        tags: vec!["weekly-sync".to_owned(), "engineering".to_owned()],
        processing: Some(MeetingProcessing {
            summary_usd: 0.0125,
            tokens_in: 4_200,
            tokens_out: 350,
            model: "claude-sonnet-4-6".to_owned(),
        }),
        action_items: vec![
            ActionItem {
                id: fixture_item_id_a(),
                text: "Send the recap email".to_owned(),
                owner: Some("Alex".to_owned()),
                due: Some(NaiveDate::from_ymd_opt(2026, 5, 15).unwrap()),
            },
            ActionItem {
                id: fixture_item_id_b(),
                text: "Decide on launch date".to_owned(),
                owner: None,
                due: None,
            },
        ],
    }
}

#[test]
fn get_meeting_response_shape_is_stable() {
    let meeting = fixture_meeting();
    let value = assert_round_trips(&meeting);
    assert_json_snapshot!("heron_get_meeting__response_meeting", value);
}

#[test]
fn get_meeting_outcome_ok_shape_is_stable() {
    // The full Tauri-side wire shape: a tagged-union envelope around
    // the daemon's `Meeting`. `DaemonOutcome` is serialize-only, so we
    // skip the round-trip and snapshot the encoding directly.
    let outcome = DaemonOutcome::Ok {
        data: fixture_meeting(),
    };
    let value = serde_json::to_value(&outcome).expect("serialize");
    assert_json_snapshot!("heron_get_meeting__outcome_ok", value);
}

#[test]
fn get_meeting_outcome_unavailable_shape_is_stable() {
    // The degraded-UI path: a 404 / 401 / connect error from the
    // daemon collapses to this variant. Pinning the wire shape keeps
    // the renderer's discriminator-on-`kind` parsing safe.
    let outcome: DaemonOutcome<Meeting> = DaemonOutcome::Unavailable {
        detail: "connect refused".to_owned(),
    };
    let value = serde_json::to_value(&outcome).expect("serialize");
    assert_json_snapshot!("heron_get_meeting__outcome_unavailable", value);
}

#[test]
fn get_meeting_minimal_shape_is_stable() {
    // Active-recording variant: pre-summary, so `processing` is `None`
    // and `action_items` / `tags` are empty. Catches a regression in
    // `Meeting.processing`'s `#[serde(skip_serializing_if = "Option::is_none")]`
    // — the field MUST be omitted from the wire when None, not encoded
    // as `"processing": null`. The renderer's TS type is `processing?:
    // MeetingProcessing` (optional), and a shift to `null` would turn
    // the destructure into a runtime crash.
    let meeting = Meeting {
        id: fixture_meeting_id(),
        status: MeetingStatus::Recording,
        platform: Platform::Zoom,
        title: None,
        calendar_event_id: None,
        started_at: Utc.with_ymd_and_hms(2026, 5, 1, 14, 0, 0).unwrap(),
        ended_at: None,
        duration_secs: None,
        participants: Vec::new(),
        transcript_status: TranscriptLifecycle::Partial,
        summary_status: SummaryLifecycle::Pending,
        tags: Vec::new(),
        processing: None,
        action_items: Vec::new(),
    };
    let value = assert_round_trips(&meeting);
    let object = value
        .as_object()
        .expect("Meeting must serialize as JSON object");
    assert!(
        !object.contains_key("processing"),
        "processing: None must be omitted from the wire (skip_serializing_if), \
         got: {value:#}",
    );
    assert_json_snapshot!("heron_get_meeting__response_meeting_minimal", value);
}

// ── heron_meeting_summary (issue's "heron_summarize") ─────────────────

fn fixture_summary() -> Summary {
    Summary {
        meeting_id: fixture_meeting_id(),
        generated_at: Utc.with_ymd_and_hms(2026, 5, 1, 14, 35, 0).unwrap(),
        text: "## Decisions\n\n- Ship the IPC snapshots PR.\n".to_owned(),
        action_items: vec![ActionItem {
            id: fixture_item_id_a(),
            text: "Send the recap email".to_owned(),
            owner: Some("Alex".to_owned()),
            due: Some(NaiveDate::from_ymd_opt(2026, 5, 15).unwrap()),
        }],
        llm_provider: Some("anthropic".to_owned()),
        llm_model: Some("claude-sonnet-4-6".to_owned()),
    }
}

#[test]
fn meeting_summary_response_shape_is_stable() {
    let summary = fixture_summary();
    let value = assert_round_trips(&summary);
    assert_json_snapshot!("heron_meeting_summary__response_summary", value);
}

#[test]
fn meeting_summary_outcome_ok_shape_is_stable() {
    let outcome = DaemonOutcome::Ok {
        data: fixture_summary(),
    };
    let value = serde_json::to_value(&outcome).expect("serialize");
    assert_json_snapshot!("heron_meeting_summary__outcome_ok", value);
}

#[test]
fn meeting_summary_outcome_unavailable_shape_is_stable() {
    // Symmetric to `heron_get_meeting`'s Unavailable variant — pinning
    // both rules out a future generic-parameter-aware serde change
    // that flipped the tag layout for one inner type but not another.
    let outcome: DaemonOutcome<Summary> = DaemonOutcome::Unavailable {
        detail: "404 Not Found".to_owned(),
    };
    let value = serde_json::to_value(&outcome).expect("serialize");
    assert_json_snapshot!("heron_meeting_summary__outcome_unavailable", value);
}

// ── heron_prepare_context ─────────────────────────────────────────────

#[test]
fn prepare_context_request_shape_is_stable() {
    let request = PrepareContextRequest {
        calendar_event_id: "cal-event-abc-123".to_owned(),
        attendees: vec![
            AttendeeContext {
                name: "Alice".to_owned(),
                email: Some("alice@example.com".to_owned()),
                last_seen_in: Some(fixture_meeting_id()),
                relationship: Some("teammate".to_owned()),
                notes: Some("PM on launch".to_owned()),
            },
            AttendeeContext {
                name: "Bob".to_owned(),
                email: None,
                last_seen_in: None,
                relationship: None,
                notes: None,
            },
        ],
    };
    let value = assert_round_trips(&request);
    assert_json_snapshot!("heron_prepare_context__request", value);
}

#[test]
fn prepare_context_request_empty_attendees_shape_is_stable() {
    // `attendees` carries `#[serde(default)]`; pinning the shape with
    // an empty vec catches a future migration to `skip_serializing_if`
    // that would silently drop the field from the wire.
    let request = PrepareContextRequest {
        calendar_event_id: "cal-event-empty".to_owned(),
        attendees: Vec::new(),
    };
    let value = assert_round_trips(&request);
    assert_json_snapshot!("heron_prepare_context__request_empty_attendees", value);
}

#[test]
fn prepare_context_response_shape_is_stable() {
    // Synthetic ack the desktop crate fabricates from a `204 No Content`
    // daemon response — locking it down catches the case where the
    // ack's field name drifts away from `calendar_event_id` (which the
    // renderer's optimistic-UI clear path keys on).
    let ack = AttachContextAck {
        calendar_event_id: "cal-event-abc-123".to_owned(),
    };
    let value = assert_round_trips(&ack);
    assert_json_snapshot!("heron_prepare_context__response_ack", value);
}

#[test]
fn prepare_context_outcome_ok_shape_is_stable() {
    let outcome = DaemonOutcome::Ok {
        data: AttachContextAck {
            calendar_event_id: "cal-event-abc-123".to_owned(),
        },
    };
    let value = serde_json::to_value(&outcome).expect("serialize");
    assert_json_snapshot!("heron_prepare_context__outcome_ok", value);
}

// ── deserialize-from-partial: pin `#[serde(default)]` regression class ─

#[test]
fn from_partial_json_uses_defaults_for_settings() {
    // A pre-PR-71 settings.json (no `onboarded`, no `target_bundle_ids`,
    // no `active_mode`, no Tier-4 fields) must still decode and fill
    // every missing field with `Settings::default()`'s value. Dropping
    // the container-level `#[serde(default)]` would break this — and
    // would silently force every existing user back through onboarding.
    let decoded: Settings =
        serde_json::from_value(json!({})).expect("partial settings.json must decode");
    assert_eq!(
        decoded,
        Settings::default(),
        "an empty settings.json must decode equal to Settings::default()",
    );
}

#[test]
fn from_partial_json_uses_defaults_for_prepare_context_request() {
    // `attendees` carries `#[serde(default)]`; a missing-attendees
    // request from a future renderer (or replayed cached payload from
    // an older build) must decode as an empty vec, not error.
    let decoded: PrepareContextRequest =
        serde_json::from_value(json!({ "calendar_event_id": "evt-1" }))
            .expect("partial prepare-context request must decode");
    assert_eq!(decoded.calendar_event_id, "evt-1");
    assert!(
        decoded.attendees.is_empty(),
        "missing attendees must default to empty vec",
    );
}
