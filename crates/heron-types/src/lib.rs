//! `heron-types` — shared serde types and the `Event` enum.
//!
//! Invariant: **no event types are invented outside this crate.** Any
//! new variant in any other crate is a PR against `heron-types` first.
//!
//! Surface mirrors `docs/implementation.md` §5.2 (types) and §5.3
//! (`SessionClock`).

use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

pub mod clock;
pub mod recording;
pub mod recovery;

pub use clock::SessionClock;
pub use recording::{
    ARM_COOLDOWN, IdleReason, RecordingFsm, RecordingState, SummaryOutcome, TransitionError,
};
pub use recovery::{
    MAX_STATE_FILE_BYTES, RecoveryError, STATE_FILE_NAME, STATE_VERSION, SessionPhase,
    SessionStateRecord, discover_unfinished, read_state, write_state,
};

pub type SessionId = uuid::Uuid;
/// Identifier on entries that survive merge-on-write (action items,
/// attendees). Generated with UUID v7 so insertion order is recoverable
/// without an explicit timestamp field.
pub type ItemId = uuid::Uuid;

/// Display label written into transcript JSONL.
///
/// By convention this is one of:
/// - `"me"` for the user's own voice (paired with [`SpeakerSource::Self_`])
/// - `"them"` for an unattributed remote speaker (paired with
///   [`SpeakerSource::Channel`])
/// - a real display name (e.g. `"Alice"`) for an AX-attributed turn
pub type SpeakerLabel = String;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Turn {
    pub t0: f64,
    pub t1: f64,
    pub text: String,
    pub channel: Channel,
    pub speaker: SpeakerLabel,
    pub speaker_source: SpeakerSource,
    pub confidence: Option<f64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    /// User's own voice, captured from the input device.
    Mic,
    /// Remote audio, captured via Core Audio process tap on the meeting app.
    Tap,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SpeakerSource {
    /// `channel == Mic`; speaker is the user. Trivially correct.
    #[serde(rename = "self")]
    Self_,
    /// AX-derived display name with sufficient overlap.
    Ax,
    /// Fell back to channel; AX did not fire or overlap-confidence was
    /// below threshold.
    Channel,
    /// Voice-embedding clustering result. v2-only.
    Cluster,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActionItem {
    pub id: ItemId,
    pub owner: String,
    pub text: String,
    pub due: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Attendee {
    pub id: ItemId,
    pub name: String,
    pub company: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MeetingType {
    Client,
    Internal,
    #[serde(rename = "1:1")]
    OneOnOne,
    Other,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiarizeSource {
    Ax,
    Channel,
    Hybrid,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Disclosure {
    pub stated: bool,
    /// `mm:ss` into the call when disclosure was made; `None` if
    /// pre-call (email) or written.
    pub when: Option<String>,
    pub how: DisclosureHow,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DisclosureHow {
    Verbal,
    WrittenChat,
    PreEmail,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Cost {
    pub summary_usd: f64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frontmatter {
    pub date: NaiveDate,
    pub start: String,
    pub duration_min: u32,
    pub company: Option<String>,
    pub attendees: Vec<Attendee>,
    pub meeting_type: MeetingType,
    pub source_app: String,
    pub recording: PathBuf,
    pub transcript: PathBuf,
    pub diarize_source: DiarizeSource,
    pub disclosed: Disclosure,
    pub cost: Cost,
    pub action_items: Vec<ActionItem>,
    pub tags: Vec<String>,
    /// Anything in the YAML frontmatter we don't model is preserved here
    /// verbatim so re-summarize round-trips cleanly.
    #[serde(flatten)]
    pub extra: serde_yaml::Mapping,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeakerEvent {
    /// Session-secs (relative to `SessionClock::started_at`).
    pub t: f64,
    pub name: String,
    /// `true` for the start of a speaking turn, `false` for the end.
    pub started: bool,
    pub view_mode: ViewMode,
    /// `true` when AX reports the user's own tile is the active speaker.
    pub own_tile: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ViewMode {
    ActiveSpeaker,
    Gallery,
    Paginated,
    SharedScreen,
    Other,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeviceChangeReason {
    DeviceAdded,
    DeviceRemoved,
    DefaultChanged,
    SampleRateChanged,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    SessionStarted {
        id: SessionId,
        source_app: String,
        started_at: DateTime<Utc>,
    },
    SessionEnded {
        id: SessionId,
        ended_at: DateTime<Utc>,
        duration: Duration,
    },
    MicMuted {
        id: SessionId,
        at: Duration,
    },
    MicUnmuted {
        id: SessionId,
        at: Duration,
    },
    AudioDeviceChanged {
        id: SessionId,
        at: Duration,
        reason: DeviceChangeReason,
    },
    CaptureDegraded {
        id: SessionId,
        at: Duration,
        dropped_frames: u32,
        reason: String,
    },
    SpeakerDetected {
        id: SessionId,
        event: SpeakerEvent,
    },
    AttributionDegraded {
        id: SessionId,
        at: Duration,
        reason: String,
    },
    TranscriptPartial {
        id: SessionId,
        turn: Turn,
    },
    TranscriptFinal {
        id: SessionId,
        turns_count: usize,
        path: PathBuf,
    },
    SummaryReady {
        id: SessionId,
        path: PathBuf,
        cost: Cost,
    },
    SummaryFailed {
        id: SessionId,
        error: String,
    },
    StorageCritical {
        id: SessionId,
        free_bytes: u64,
    },
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn speaker_source_self_round_trips_to_string_self() {
        // Load-bearing: transcript JSONL writes `"speaker_source": "self"`
        // (per docs/plan.md §3.4). The Rust variant has to be `Self_`
        // because `self` is a keyword.
        let json = serde_json::to_string(&SpeakerSource::Self_).expect("Self_ must serialize");
        assert_eq!(json, r#""self""#);

        let round: SpeakerSource =
            serde_json::from_str(r#""self""#).expect(r#""self" must deserialize to Self_"#);
        assert_eq!(round, SpeakerSource::Self_);
    }

    #[test]
    fn speaker_source_other_variants_snake_case() {
        for (variant, expected) in [
            (SpeakerSource::Ax, r#""ax""#),
            (SpeakerSource::Channel, r#""channel""#),
            (SpeakerSource::Cluster, r#""cluster""#),
        ] {
            let s = serde_json::to_string(&variant).expect("serialize");
            assert_eq!(s, expected, "{variant:?} should serialize to {expected}");
        }
    }

    #[test]
    fn meeting_type_one_on_one_renames_to_1_1() {
        let json = serde_json::to_string(&MeetingType::OneOnOne).expect("serialize");
        assert_eq!(json, r#""1:1""#);
        let round: MeetingType = serde_json::from_str(r#""1:1""#).expect("deserialize");
        assert_eq!(round, MeetingType::OneOnOne);
    }

    #[test]
    fn turn_round_trips_jsonl_shape() {
        // Mirrors the JSONL example in docs/plan.md §3.4.
        let line = r#"{"t0":12.4,"t1":18.9,"text":"We need...","channel":"tap","speaker":"Alice","speaker_source":"ax","confidence":0.92}"#;
        let turn: Turn = serde_json::from_str(line).expect("deserialize");
        assert_eq!(turn.t0, 12.4);
        assert_eq!(turn.channel, Channel::Tap);
        assert_eq!(turn.speaker, "Alice");
        assert_eq!(turn.speaker_source, SpeakerSource::Ax);
        assert_eq!(turn.confidence, Some(0.92));
        let back = serde_json::to_string(&turn).expect("serialize");
        assert_eq!(back, line);
    }

    #[test]
    fn channel_turn_confidence_null_round_trips() {
        // §3.4 invariant: `confidence: null` for `speaker_source: "channel"`.
        let line = r#"{"t0":22.0,"t1":25.4,"text":"Hmm...","channel":"tap","speaker":"them","speaker_source":"channel","confidence":null}"#;
        let turn: Turn = serde_json::from_str(line).expect("deserialize");
        assert!(turn.confidence.is_none());
        let back = serde_json::to_string(&turn).expect("serialize");
        assert_eq!(back, line);
    }

    #[test]
    fn event_uses_kind_tag() {
        // The serde tag attribute drives the LLM prompt contract — if
        // the tag name drifts, downstream consumers break silently.
        let id = uuid::Uuid::nil();
        let evt = Event::MicMuted {
            id,
            at: Duration::from_secs(5),
        };
        let s = serde_json::to_string(&evt).expect("serialize");
        assert!(s.contains(r#""kind":"mic_muted""#), "got: {s}");
    }

    #[test]
    fn frontmatter_preserves_unknown_yaml_keys_round_trip() {
        // Load-bearing for §10 merge-on-write: anything in the user's
        // frontmatter that we don't model must survive re-serialize.
        let yaml = r#"date: 2026-04-24
start: "14:00"
duration_min: 47
company: Acme
attendees:
  - id: 00000000-0000-0000-0000-000000000001
    name: Alice
    company: Acme
meeting_type: client
source_app: us.zoom.xos
recording: recordings/x.m4a
transcript: transcripts/x.jsonl
diarize_source: ax
disclosed:
  stated: true
  when: "00:14"
  how: verbal
cost:
  summary_usd: 0.04
  tokens_in: 14231
  tokens_out: 612
  model: claude-sonnet-4-6
action_items: []
tags: [meeting, acme]
custom_user_field: hello
custom_nested:
  inner: 42
"#;
        let fm: Frontmatter = serde_yaml::from_str(yaml).expect("frontmatter must parse");

        // Modeled fields parsed correctly.
        assert_eq!(fm.duration_min, 47);
        assert_eq!(fm.meeting_type, MeetingType::Client);

        // Unknown keys collected into `extra`.
        let extra_str = serde_yaml::to_string(&fm.extra).expect("extra serializes");
        assert!(extra_str.contains("custom_user_field"));
        assert!(extra_str.contains("hello"));
        assert!(extra_str.contains("custom_nested"));
        assert!(extra_str.contains("inner: 42"));

        // Round-trip the whole frontmatter and confirm both modeled
        // and unknown fields make it through.
        let back = serde_yaml::to_string(&fm).expect("frontmatter serializes");
        assert!(back.contains("duration_min: 47"));
        assert!(back.contains("custom_user_field: hello"));
        assert!(back.contains("custom_nested:"));
    }

    #[test]
    fn event_round_trips_with_payload() {
        let id = uuid::Uuid::nil();
        let cases = [
            Event::MicMuted {
                id,
                at: Duration::from_millis(1_500),
            },
            Event::AudioDeviceChanged {
                id,
                at: Duration::from_secs(2),
                reason: DeviceChangeReason::DefaultChanged,
            },
            Event::SummaryFailed {
                id,
                error: "rate_limited".into(),
            },
        ];
        for evt in cases {
            let json = serde_json::to_string(&evt).expect("serialize");
            let _back: Event = serde_json::from_str(&json).expect("deserialize round-trips");
        }
    }
}
