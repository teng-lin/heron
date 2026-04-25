//! Shared parser for the structured JSON every summarizer backend
//! is asked to emit. The shape is defined by `templates/meeting.hbs`:
//!
//! ```json
//! { "body": "...", "company": "...", "meeting_type": "client",
//!   "tags": [...], "action_items": [...], "attendees": [...] }
//! ```
//!
//! Anthropic's Messages API wraps this in `content[0].text`; the
//! Claude Code / Codex CLI backends emit it on stdout. Both call
//! [`parse_content_json`] to turn raw bytes into a
//! [`crate::SummarizerOutput`] minus the `cost` field (which the
//! caller fills with whatever billing data the backend produced).
//!
//! Tolerant parser: every optional field defaults so a backend that
//! omits e.g. `tags` doesn't fail the call. The `body` field is the
//! only hard requirement.

use heron_types::{ActionItem, Attendee, MeetingType};
use serde::Deserialize;

use crate::LlmError;

/// Optional fields in the LLM's structured output. Anything missing
/// falls back to `Default` / the caller-supplied `meeting_type`.
#[derive(Debug, Deserialize)]
pub(crate) struct ContentJson {
    pub body: String,
    #[serde(default)]
    pub company: Option<String>,
    #[serde(default)]
    pub meeting_type: Option<MeetingType>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub action_items: Option<Vec<ActionItem>>,
    #[serde(default)]
    pub attendees: Option<Vec<Attendee>>,
}

/// Parse the structured JSON the LLM was asked to emit. Returns a
/// [`LlmError::Parse`] with a short snippet of the offending text so
/// the user can tell whether the LLM ignored the format instruction
/// (e.g., wrapped the JSON in markdown fences) or whether the wire
/// got corrupted.
pub(crate) fn parse_content_json(text: &str) -> Result<ContentJson, LlmError> {
    serde_json::from_str(text).map_err(|e| {
        LlmError::Parse(format!(
            "content was not the JSON shape we asked for: {e}; \
             first 120 chars: {snippet:?}",
            snippet = text.chars().take(120).collect::<String>()
        ))
    })
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_shape() {
        let json = r#"{
            "body": "summary",
            "company": "Acme",
            "meeting_type": "client",
            "tags": ["acme"],
            "action_items": [],
            "attendees": []
        }"#;
        let parsed = parse_content_json(json).expect("parse");
        assert_eq!(parsed.body, "summary");
        assert_eq!(parsed.company.as_deref(), Some("Acme"));
        assert_eq!(parsed.meeting_type, Some(MeetingType::Client));
    }

    #[test]
    fn defaults_optional_fields_when_only_body_present() {
        let parsed = parse_content_json(r#"{"body":"x"}"#).expect("parse");
        assert!(parsed.company.is_none());
        assert!(parsed.tags.is_none());
        assert!(parsed.action_items.is_none());
    }

    #[test]
    fn surfaces_parse_error_with_snippet() {
        let err = parse_content_json("not json at all").expect_err("malformed");
        match err {
            LlmError::Parse(s) => {
                assert!(s.contains("not json"), "missing snippet: {s}");
            }
            other => panic!("expected Parse, got {other:?}"),
        }
    }

    #[test]
    fn missing_body_is_a_parse_error() {
        // `body` has no `#[serde(default)]`; absent body must error.
        let err = parse_content_json(r#"{"tags":["x"]}"#).expect_err("no body");
        assert!(matches!(err, LlmError::Parse(_)));
    }
}
