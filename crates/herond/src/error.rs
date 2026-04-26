//! Wire-shape error envelope.
//!
//! Mirrors the OpenAPI `Error` schema (`docs/api-desktop-openapi.yaml`
//! `components.schemas.Error`) which itself adopts the MeetingBaaS
//! envelope per spec §11. Every non-2xx response from the daemon
//! carries this body.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use heron_session::SessionError;
use serde::Serialize;

/// The wire envelope. Field names use mixed casing because that's
/// what the OpenAPI declares (`statusCode`, `success`); rename via
/// serde rather than fight the spec.
#[derive(Debug, Serialize)]
pub struct WireError {
    /// Always `false`. Kept as a literal-typed field so the
    /// generated TS / Python client gets a discriminated union with
    /// the success-shape responses.
    pub success: bool,
    /// PascalCase machine-readable error name. Stable across versions.
    pub error: &'static str,
    /// Human-readable explanation. May change between versions.
    pub message: String,
    /// `^HERON_E_[A-Z0-9_]+$`.
    pub code: &'static str,
    #[serde(rename = "statusCode")]
    pub status_code: u16,
    /// Free-form structured details. Empty for most errors; carries
    /// `provider` for `LlmProviderFailed`, `current_state` for
    /// `InvalidState`, etc.
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    pub details: serde_json::Value,
}

impl WireError {
    /// Construct from the literal fields. Caller is responsible for
    /// matching the `code` to a `HERON_E_*` literal — the type
    /// system can't enforce that, but [`From<SessionError>`] does
    /// for the orchestrator-error path.
    pub fn new(
        error: &'static str,
        code: &'static str,
        status: StatusCode,
        message: impl Into<String>,
    ) -> Self {
        Self {
            success: false,
            error,
            code,
            status_code: status.as_u16(),
            message: message.into(),
            details: serde_json::Value::Null,
        }
    }

    /// Builder: attach a structured `details` payload. Use for
    /// per-variant context (`current_state`, `provider`, etc.).
    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = details;
        self
    }
}

impl IntoResponse for WireError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.status_code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, Json(self)).into_response()
    }
}

/// Map a [`SessionError`] to its corresponding HTTP status. Pinned
/// against the OpenAPI's per-endpoint response codes (see
/// `MeetingBaaS-shape error envelope` §11).
pub fn status_for(err: &SessionError) -> StatusCode {
    match err {
        SessionError::NotYetImplemented => StatusCode::NOT_IMPLEMENTED,
        SessionError::NotFound { .. } => StatusCode::NOT_FOUND,
        SessionError::InvalidState { .. } => StatusCode::CONFLICT,
        SessionError::CaptureInProgress { .. } => StatusCode::CONFLICT,
        SessionError::VaultLocked { .. } => StatusCode::LOCKED,
        SessionError::LlmProviderFailed { .. } => StatusCode::FAILED_DEPENDENCY,
        SessionError::TooEarly => StatusCode::TOO_EARLY,
        SessionError::PermissionMissing { .. } => StatusCode::SERVICE_UNAVAILABLE,
        SessionError::Validation { .. } => StatusCode::UNPROCESSABLE_ENTITY,
    }
}

impl From<SessionError> for WireError {
    fn from(err: SessionError) -> Self {
        let code = err.code();
        let status = status_for(&err);
        let (name, details) = match &err {
            SessionError::NotYetImplemented => ("NotYetImplemented", serde_json::Value::Null),
            SessionError::NotFound { what } => ("NotFound", serde_json::json!({ "what": what })),
            SessionError::InvalidState { current_state } => (
                "InvalidState",
                serde_json::json!({ "current_state": current_state }),
            ),
            SessionError::CaptureInProgress { platform } => (
                "CaptureInProgress",
                serde_json::json!({ "platform": platform }),
            ),
            SessionError::VaultLocked { detail } => {
                ("VaultLocked", serde_json::json!({ "detail": detail }))
            }
            SessionError::LlmProviderFailed { provider, detail } => (
                "LlmProviderFailed",
                serde_json::json!({ "provider": provider, "detail": detail }),
            ),
            SessionError::TooEarly => ("TooEarly", serde_json::Value::Null),
            SessionError::PermissionMissing { permission } => (
                "PermissionMissing",
                serde_json::json!({ "permission": permission }),
            ),
            SessionError::Validation { detail } => {
                ("Validation", serde_json::json!({ "detail": detail }))
            }
        };
        WireError::new(name, code, status, err.to_string()).with_details(details)
    }
}
