/**
 * Domain entity types mirroring `crates/heron-session/src/lib.rs`.
 *
 * Hand-rolled rather than generated. Each declaration cites the Rust
 * `file:line` it mirrors so a future ts-rs adoption (recommended by
 * the UI-revamp plan but deferred for cost reasons) has a clean
 * reconciliation pass. Wire shape follows serde's defaults: structs
 * become objects with snake_case fields; enums use lowercase
 * `serde(rename_all = "snake_case")` strings.
 */

/**
 * Stripe-style prefixed UUIDv7 like `mtg_01902a8e-7c4f-7000-...`.
 * Mirrors `crates/heron-types/src/lib.rs:49` (`prefixed_id! { pub MeetingId, "mtg" }`).
 * Carried as an opaque string on the JS side; the daemon validates the
 * prefix on the way in / out.
 */
export type MeetingId = string;

/** Mirrors `crates/heron-session/src/lib.rs:79`. */
export type Platform = "zoom" | "google_meet" | "microsoft_teams" | "webex";

/** Mirrors `crates/heron-session/src/lib.rs:94`. */
export type MeetingStatus =
  | "detected"
  | "armed"
  | "recording"
  | "ended"
  | "done"
  | "failed";

/** Mirrors `crates/heron-session/src/lib.rs:174`. */
export type TranscriptLifecycle = "pending" | "partial" | "complete" | "failed";

/** Mirrors `crates/heron-session/src/lib.rs:185`. */
export type SummaryLifecycle = "pending" | "ready" | "failed";

/** Mirrors `crates/heron-session/src/lib.rs:118`. */
export type Confidence = "high" | "low";

/** Mirrors `crates/heron-session/src/lib.rs:128`. */
export type IdentifierKind = "ax_tree" | "webrtc_track" | "mic" | "fallback";

/** Mirrors `crates/heron-session/src/lib.rs:138`. */
export interface Participant {
  display_name: string;
  identifier_kind: IdentifierKind;
  is_user: boolean;
}

/** Mirrors `crates/heron-session/src/lib.rs:152`. */
export interface Meeting {
  id: MeetingId;
  status: MeetingStatus;
  platform: Platform;
  title: string | null;
  /** EventKit identifier, when calendar correlation succeeded. */
  calendar_event_id: string | null;
  /** RFC 3339 UTC timestamp. */
  started_at: string;
  /** RFC 3339 UTC timestamp. */
  ended_at: string | null;
  duration_secs: number | null;
  participants: Participant[];
  transcript_status: TranscriptLifecycle;
  summary_status: SummaryLifecycle;
  /**
   * LLM-inferred topic tags lifted from the note's
   * `Frontmatter.tags`. Empty for active captures (no summary yet)
   * and for any meeting whose summarizer omitted them. Optional on
   * the wire so a payload from an older daemon (no `tags` field
   * emitted, since the Rust side uses `#[serde(default)]`) still
   * deserializes — callers should treat a missing `tags` as `[]`,
   * typically via `meeting.tags ?? []` at the consumption site.
   * Mirrors `crates/heron-session/src/lib.rs:Meeting.tags`.
   */
  tags?: string[];
}

/** Mirrors `crates/heron-session/src/lib.rs:482`. */
export interface ListMeetingsQuery {
  /** RFC 3339 timestamp. Filter to meetings whose `started_at >= since`. */
  since?: string;
  status?: MeetingStatus;
  platform?: Platform;
  limit?: number;
  cursor?: string;
}

/** Mirrors `crates/heron-session/src/lib.rs:491`. */
export interface ListMeetingsPage {
  items: Meeting[];
  next_cursor: string | null;
}

/** Mirrors `crates/heron-session/src/lib.rs:216`. */
export interface ActionItem {
  text: string;
  owner: string | null;
  /** ISO date (`YYYY-MM-DD`); `null` when no due date. */
  due: string | null;
}

/** Mirrors `crates/heron-session/src/lib.rs:226`. */
export interface Summary {
  meeting_id: MeetingId;
  /** RFC 3339 UTC timestamp. */
  generated_at: string;
  /** Markdown body. */
  text: string;
  action_items: ActionItem[];
  llm_provider: string | null;
  llm_model: string | null;
}

/** Mirrors `crates/heron-session/src/lib.rs:205`. */
export interface Transcript {
  meeting_id: MeetingId;
  status: TranscriptLifecycle;
  language: string | null;
  segments: TranscriptSegment[];
}

/**
 * Daemon-side outcome for `heron_list_meetings` /
 * `heron_meeting_summary`. The Tauri command returns one of these so
 * the frontend can switch into a degraded UI on transport failure
 * without parsing error strings.
 */
export type DaemonResult<T> =
  | { kind: "ok"; data: T }
  | { kind: "unavailable"; detail: string };

/** Mirrors `crates/heron-session/src/lib.rs:193`. */
export interface TranscriptSegment {
  speaker: Participant;
  text: string;
  start_secs: number;
  end_secs: number;
  confidence: Confidence;
  /** `false` segments are subject to revision in a later partial. */
  is_final: boolean;
}

/** Local file source returned by `heron_meeting_audio`. */
export interface DaemonAudioSource {
  path: string;
  content_type: string | null;
}

/** Mirrors `crates/heron-session/src/lib.rs:263`. */
export interface AttendeeContext {
  name: string;
  email: string | null;
  last_seen_in: MeetingId | null;
  relationship: string | null;
  notes: string | null;
}

/** Mirrors `crates/heron-session/src/lib.rs:245`. */
export interface CalendarEvent {
  /** EventKit identifier — passed back to `attach_context`. */
  id: string;
  title: string;
  /** RFC 3339 UTC. */
  start: string;
  /** RFC 3339 UTC. */
  end: string;
  attendees: AttendeeContext[];
  meeting_url: string | null;
  related_meetings: MeetingId[];
}

/** Mirrors herond's `CalendarPage` wire shape (serialize-only daemon-side). */
export interface CalendarPage {
  items: CalendarEvent[];
}

/** Mirrors `crates/heron-session/src/lib.rs:285`. */
export interface PriorDecision {
  text: string;
  source_meeting_id: MeetingId;
}

/** Mirrors `crates/heron-session/src/lib.rs:271`. */
export interface PreMeetingContext {
  agenda: string | null;
  attendees_known: AttendeeContext[];
  /** Vault-relative paths. */
  related_notes: string[];
  prior_decisions: PriorDecision[];
  user_briefing: string | null;
}

/** Mirrors `crates/heron-session/src/lib.rs:294`. */
export interface PreMeetingContextRequest {
  calendar_event_id: string;
  context: PreMeetingContext;
}

/** Synthetic ack for a successful `PUT /v1/context` (daemon emits 204). */
export interface AttachContextAck {
  calendar_event_id: string;
}

/** Query params for `heron_list_calendar_upcoming`. All RFC 3339 / numeric. */
export interface CalendarQuery {
  /** RFC 3339 UTC. Default: now. */
  from?: string;
  /** RFC 3339 UTC. Default: from + 7 days. */
  to?: string;
  /** Capped at 100 by the daemon. */
  limit?: number;
}

/** Mirrors `crates/heron-session/src/lib.rs:434`. */
export type MeetingOutcome =
  | "success"
  | "failed"
  | "aborted"
  | "permission_revoked";

/** Mirrors `crates/heron-session/src/lib.rs:427`. */
export interface MeetingCompletedData {
  meeting: Meeting;
  outcome: MeetingOutcome;
  failure_reason: string | null;
}

/** Mirrors `crates/heron-session/src/lib.rs:443`. */
export interface ActionItemsReadyData {
  items: ActionItem[];
}

/** Mirrors `crates/heron-session/src/lib.rs:448`. */
export interface DoctorWarningData {
  component: string;
  message: string;
}

/** Mirrors `crates/heron-session/src/lib.rs:468`. */
export interface DaemonErrorData {
  /** `HERON_E_*` machine-readable error code. */
  code: string;
  message: string;
  recoverable: boolean;
}

/**
 * Mirrors `crates/heron-session/src/lib.rs:358` — the discriminated
 * union of all daemon → frontend events. Serde framing is
 * `tag: event_type, content: data`, so the wire shape is
 * `{ event_type: "transcript.partial", data: TranscriptSegment }`.
 */
export type EventPayload =
  | { event_type: "meeting.detected"; data: Meeting }
  | { event_type: "meeting.armed"; data: Meeting }
  | { event_type: "meeting.started"; data: Meeting }
  | { event_type: "meeting.ended"; data: Meeting }
  | { event_type: "meeting.completed"; data: MeetingCompletedData }
  | { event_type: "meeting.participant_joined"; data: Participant }
  | { event_type: "transcript.partial"; data: TranscriptSegment }
  | { event_type: "transcript.final"; data: TranscriptSegment }
  | { event_type: "summary.ready"; data: Summary }
  | { event_type: "action_items.ready"; data: ActionItemsReadyData }
  | { event_type: "doctor.warning"; data: DoctorWarningData }
  | { event_type: "daemon.error"; data: DaemonErrorData };

/**
 * Wire envelope for every event flowing on the daemon → frontend bus.
 * Mirrors `crates/heron-event/src/lib.rs:84` (`Envelope<EventPayload>`).
 * The `EventPayload` is `#[serde(flatten)]` into the envelope on the
 * wire, so the top-level JSON looks like:
 *
 * ```
 * {
 *   "event_id": "evt_<uuid>",
 *   "api_version": "2026-04-25",
 *   "created_at": "2026-04-27T12:34:56Z",
 *   "meeting_id": "mtg_<uuid>" | null,
 *   "event_type": "transcript.partial",
 *   "data": { ... }
 * }
 * ```
 */
export type EventEnvelope = {
  event_id: string;
  api_version: string;
  /** RFC 3339 UTC. */
  created_at: string;
  meeting_id: string | null;
} & EventPayload;
