//! Stub [`SessionOrchestrator`] used by the production binary until
//! the `LocalSessionOrchestrator` consolidation lands.
//!
//! Hardcodes a degraded `Health` (every component reports
//! `permission_missing`, since no real subsystem is wired), owns a
//! [`heron_event::EventBus`] that nothing publishes to, and returns
//! [`SessionError::NotYetImplemented`] for every actual operation.
//!
//! The point isn't usefulness — the point is shape: a real
//! orchestrator drops in by replacing `Arc<dyn …>` in
//! [`crate::AppState`], no router rewiring. That's the trait surface
//! validating itself.

use std::path::PathBuf;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use heron_event::EventBus;
use heron_session::{
    CalendarEvent, ComponentState, EventPayload, Health, HealthComponent, HealthComponents,
    HealthStatus, ListMeetingsPage, ListMeetingsQuery, Meeting, MeetingId,
    PreMeetingContextRequest, PrepareContextRequest, SessionError, SessionEventBus,
    SessionOrchestrator, StartCaptureArgs, Summary, Transcript,
};

/// Fixed bus capacity for the stub. 1024 covers any vertical-slice
/// integration test without lag.
const STUB_BUS_CAPACITY: usize = 1024;

pub struct StubOrchestrator {
    bus: SessionEventBus,
}

impl StubOrchestrator {
    pub fn new() -> Self {
        Self {
            bus: EventBus::new(STUB_BUS_CAPACITY),
        }
    }
}

impl Default for StubOrchestrator {
    fn default() -> Self {
        Self::new()
    }
}

fn permission_missing(reason: &str) -> HealthComponent {
    HealthComponent {
        state: ComponentState::PermissionMissing,
        message: Some(reason.to_owned()),
        last_check: None,
    }
}

#[async_trait]
impl SessionOrchestrator for StubOrchestrator {
    async fn list_meetings(&self, _q: ListMeetingsQuery) -> Result<ListMeetingsPage, SessionError> {
        Err(SessionError::NotYetImplemented)
    }

    async fn get_meeting(&self, _id: &MeetingId) -> Result<Meeting, SessionError> {
        Err(SessionError::NotYetImplemented)
    }

    async fn start_capture(&self, _args: StartCaptureArgs) -> Result<Meeting, SessionError> {
        Err(SessionError::NotYetImplemented)
    }

    async fn end_meeting(&self, _id: &MeetingId) -> Result<(), SessionError> {
        Err(SessionError::NotYetImplemented)
    }

    async fn pause_capture(&self, _id: &MeetingId) -> Result<(), SessionError> {
        Err(SessionError::NotYetImplemented)
    }

    async fn resume_capture(&self, _id: &MeetingId) -> Result<(), SessionError> {
        Err(SessionError::NotYetImplemented)
    }

    async fn read_transcript(&self, _id: &MeetingId) -> Result<Transcript, SessionError> {
        Err(SessionError::NotYetImplemented)
    }

    async fn read_summary(&self, _id: &MeetingId) -> Result<Option<Summary>, SessionError> {
        Err(SessionError::NotYetImplemented)
    }

    async fn audio_path(&self, _id: &MeetingId) -> Result<PathBuf, SessionError> {
        Err(SessionError::NotYetImplemented)
    }

    async fn list_upcoming_calendar(
        &self,
        _from: Option<DateTime<Utc>>,
        _to: Option<DateTime<Utc>>,
        _limit: Option<u32>,
    ) -> Result<Vec<CalendarEvent>, SessionError> {
        Err(SessionError::NotYetImplemented)
    }

    async fn attach_context(&self, _req: PreMeetingContextRequest) -> Result<(), SessionError> {
        Err(SessionError::NotYetImplemented)
    }

    async fn prepare_context(&self, _req: PrepareContextRequest) -> Result<(), SessionError> {
        Err(SessionError::NotYetImplemented)
    }

    async fn health(&self) -> Health {
        Health {
            status: HealthStatus::Degraded,
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            components: HealthComponents {
                capture: permission_missing("orchestrator stub: capture not wired"),
                whisperkit: permission_missing("orchestrator stub: whisperkit not wired"),
                vault: permission_missing("orchestrator stub: vault not wired"),
                eventkit: permission_missing("orchestrator stub: eventkit not wired"),
                llm: permission_missing("orchestrator stub: llm not wired"),
            },
        }
    }

    fn event_bus(&self) -> SessionEventBus {
        self.bus.clone()
    }
}

#[allow(dead_code)]
fn _payload_type_imported(_p: EventPayload) {}
