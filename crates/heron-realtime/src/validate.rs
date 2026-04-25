//! Validation for [`crate::SessionConfig`] before it crosses the
//! [`crate::RealtimeBackend::session_open`] boundary.
//!
//! The trait's existing error variants ([`crate::RealtimeError::PromptTooLarge`],
//! [`crate::RealtimeError::BadConfig`], [`crate::RealtimeError::InvalidToolSpec`])
//! are how a backend reports a *server-side* rejection. This module
//! catches the same failures *before* the network round-trip — so a
//! misconfigured prompt fails the orchestrator's session-start with
//! a clean local error rather than a backend-specific 400 + retry
//! storm.
//!
//! Pure / synchronous: the orchestrator can call [`validate`] on the
//! hot path without a clock or thread hop.
//!
//! ## Limits
//!
//! - **System-prompt budget**: 64 KiB (the same heuristic
//!   `heron_bot::context` uses — 16K tokens × ~4 bytes/token plus a
//!   little headroom for the persona prompt that `context::render`
//!   doesn't emit).
//! - **Tool count**: 64. OpenAI Realtime's documented cap is around
//!   128; we err conservative because Gemini Live and LiveKit
//!   typically cap lower, and 64 is more tools than any realistic
//!   meeting agent uses.
//! - **Tool name + description**: must be non-empty. A nameless
//!   tool can't be invoked; a description-less tool can't be picked
//!   by the LLM.
//! - **Tool parameters_schema**: must be a JSON object (not array,
//!   string, etc.) — JSON Schema's `type` is always object-shaped.
//! - **VAD threshold**: must be in `[0.0, 1.0]` (the OpenAI Realtime
//!   convention).
//! - **Voice**: non-empty string. Empty would be silently routed to
//!   the backend default which is rarely what the caller wanted.

use crate::{RealtimeError, SessionConfig};

/// Hard cap on `system_prompt` size in bytes. 64 KiB ≈ 16K tokens
/// at 4 bytes/token. Erring slightly higher than `heron_bot::
/// context::MAX_CONTEXT_BYTES` (48 KiB) because the system prompt
/// includes the persona prompt + tool schemas the realtime backend
/// appends, not just the rendered `PreMeetingContext`.
pub const MAX_SYSTEM_PROMPT_BYTES: usize = 64 * 1024;

/// Hard cap on tool count. Real backends accept more; capping here
/// catches the "I forgot a base case in the tool generator" bug
/// before it hits a vendor.
pub const MAX_TOOL_COUNT: usize = 64;

/// Run all validations against `config`. Returns `Ok(())` when the
/// config is safe to send to a backend's `session_open`. Errors map
/// 1:1 onto the existing [`RealtimeError`] variants so the
/// orchestrator can `?` through to a single error type.
pub fn validate(config: &SessionConfig) -> Result<(), RealtimeError> {
    if config.system_prompt.is_empty() {
        return Err(RealtimeError::BadConfig(
            "system_prompt is empty; pass a persona prompt".to_owned(),
        ));
    }
    if config.system_prompt.len() > MAX_SYSTEM_PROMPT_BYTES {
        return Err(RealtimeError::PromptTooLarge);
    }

    if config.tools.len() > MAX_TOOL_COUNT {
        return Err(RealtimeError::BadConfig(format!(
            "tools.len() = {actual} exceeds {cap}-tool cap",
            actual = config.tools.len(),
            cap = MAX_TOOL_COUNT,
        )));
    }
    for (idx, tool) in config.tools.iter().enumerate() {
        validate_tool(idx, tool)?;
    }

    let v = config.turn_detection.vad_threshold;
    if !(0.0..=1.0).contains(&v) || v.is_nan() {
        return Err(RealtimeError::BadConfig(format!(
            "turn_detection.vad_threshold = {v} must be in [0.0, 1.0]"
        )));
    }

    if config.voice.trim().is_empty() {
        return Err(RealtimeError::BadConfig(
            "voice is empty; pass a backend-specific voice ID".to_owned(),
        ));
    }

    Ok(())
}

fn validate_tool(idx: usize, tool: &crate::ToolSpec) -> Result<(), RealtimeError> {
    if tool.name.trim().is_empty() {
        return Err(RealtimeError::InvalidToolSpec(format!(
            "tools[{idx}].name is empty"
        )));
    }
    if tool.description.trim().is_empty() {
        return Err(RealtimeError::InvalidToolSpec(format!(
            "tools[{idx}].description is empty (LLM needs description to pick the tool)"
        )));
    }
    // JSON Schema for tool arguments is always object-shaped per
    // OpenAI / Anthropic / Gemini conventions. Reject any other
    // shape so a typo (e.g., `"a string"` instead of `{"type": "object"}`)
    // surfaces locally rather than as a vendor 400.
    if !tool.parameters_schema.is_object() {
        return Err(RealtimeError::InvalidToolSpec(format!(
            "tools[{idx}].parameters_schema must be a JSON object \
             (got {shape}); use {{\"type\":\"object\",\"properties\":...}}",
            shape = json_shape(&tool.parameters_schema)
        )));
    }
    Ok(())
}

fn json_shape(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::{ToolSpec, TurnDetection};
    use serde_json::json;

    fn turn_detection() -> TurnDetection {
        TurnDetection {
            vad_threshold: 0.5,
            prefix_padding_ms: 300,
            silence_duration_ms: 500,
            interrupt_response: true,
            auto_create_response: true,
        }
    }

    fn tool(name: &str, description: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_owned(),
            description: description.to_owned(),
            parameters_schema: json!({"type": "object", "properties": {}}),
        }
    }

    fn config() -> SessionConfig {
        SessionConfig {
            system_prompt: "You are a helpful meeting assistant.".to_owned(),
            tools: vec![],
            turn_detection: turn_detection(),
            voice: "alloy".to_owned(),
        }
    }

    #[test]
    fn minimal_valid_config_passes() {
        validate(&config()).expect("valid");
    }

    #[test]
    fn empty_system_prompt_rejected() {
        let mut c = config();
        c.system_prompt.clear();
        let err = validate(&c).expect_err("empty prompt");
        assert!(matches!(err, RealtimeError::BadConfig(s) if s.contains("system_prompt is empty")));
    }

    #[test]
    fn oversize_system_prompt_returns_prompt_too_large() {
        let mut c = config();
        c.system_prompt = "x".repeat(MAX_SYSTEM_PROMPT_BYTES + 1);
        let err = validate(&c).expect_err("oversize");
        assert!(matches!(err, RealtimeError::PromptTooLarge));
    }

    #[test]
    fn at_cap_system_prompt_passes() {
        // Exactly MAX bytes is allowed; only > MAX rejects.
        let mut c = config();
        c.system_prompt = "x".repeat(MAX_SYSTEM_PROMPT_BYTES);
        validate(&c).expect("at cap");
    }

    #[test]
    fn too_many_tools_rejected() {
        let mut c = config();
        c.tools = (0..=MAX_TOOL_COUNT)
            .map(|i| tool(&format!("tool_{i}"), "test"))
            .collect();
        let err = validate(&c).expect_err("too many");
        match err {
            RealtimeError::BadConfig(s) => {
                assert!(s.contains("exceeds"), "got: {s}");
                assert!(s.contains(&format!("{MAX_TOOL_COUNT}")));
            }
            other => panic!("expected BadConfig, got {other:?}"),
        }
    }

    #[test]
    fn at_cap_tools_pass() {
        let mut c = config();
        c.tools = (0..MAX_TOOL_COUNT)
            .map(|i| tool(&format!("tool_{i}"), "test"))
            .collect();
        validate(&c).expect("at cap");
    }

    #[test]
    fn empty_tool_name_rejected_with_index() {
        let mut c = config();
        c.tools = vec![tool("ok", "fine"), tool("", "fine"), tool("ok2", "fine")];
        let err = validate(&c).expect_err("empty name");
        match err {
            RealtimeError::InvalidToolSpec(s) => {
                assert!(s.contains("tools[1]"), "should name index 1: {s}");
                assert!(s.contains("name is empty"));
            }
            other => panic!("expected InvalidToolSpec, got {other:?}"),
        }
    }

    #[test]
    fn whitespace_only_tool_name_rejected() {
        let mut c = config();
        c.tools = vec![tool("   \n\t", "ok")];
        let err = validate(&c).expect_err("whitespace name");
        assert!(matches!(err, RealtimeError::InvalidToolSpec(_)));
    }

    #[test]
    fn empty_tool_description_rejected() {
        let mut c = config();
        c.tools = vec![tool("ok", "")];
        let err = validate(&c).expect_err("empty description");
        match err {
            RealtimeError::InvalidToolSpec(s) => {
                assert!(s.contains("description is empty"));
            }
            other => panic!("expected InvalidToolSpec, got {other:?}"),
        }
    }

    #[test]
    fn non_object_parameters_schema_rejected() {
        for (shape, value) in [
            ("array", json!([1, 2, 3])),
            ("string", json!("a string")),
            ("number", json!(42)),
            ("bool", json!(true)),
            ("null", json!(null)),
        ] {
            let mut c = config();
            c.tools = vec![ToolSpec {
                name: "ok".to_owned(),
                description: "ok".to_owned(),
                parameters_schema: value,
            }];
            let err = validate(&c).expect_err(shape);
            match err {
                RealtimeError::InvalidToolSpec(s) => {
                    assert!(
                        s.contains(shape),
                        "rejection should name the offending shape ({shape}): {s}"
                    );
                }
                other => panic!("expected InvalidToolSpec for {shape}, got {other:?}"),
            }
        }
    }

    #[test]
    fn vad_threshold_below_zero_rejected() {
        let mut c = config();
        c.turn_detection.vad_threshold = -0.1;
        let err = validate(&c).expect_err("negative");
        assert!(matches!(err, RealtimeError::BadConfig(s) if s.contains("vad_threshold")));
    }

    #[test]
    fn vad_threshold_above_one_rejected() {
        let mut c = config();
        c.turn_detection.vad_threshold = 1.1;
        let err = validate(&c).expect_err("over");
        assert!(matches!(err, RealtimeError::BadConfig(_)));
    }

    #[test]
    fn vad_threshold_at_boundaries_accepted() {
        for t in [0.0, 1.0, 0.5] {
            let mut c = config();
            c.turn_detection.vad_threshold = t;
            validate(&c).expect("boundary");
        }
    }

    #[test]
    fn nan_vad_threshold_rejected() {
        let mut c = config();
        c.turn_detection.vad_threshold = f32::NAN;
        let err = validate(&c).expect_err("nan");
        assert!(matches!(err, RealtimeError::BadConfig(_)));
    }

    #[test]
    fn empty_voice_rejected() {
        let mut c = config();
        c.voice.clear();
        let err = validate(&c).expect_err("empty voice");
        assert!(matches!(err, RealtimeError::BadConfig(s) if s.contains("voice is empty")));
    }

    #[test]
    fn whitespace_only_voice_rejected() {
        let mut c = config();
        c.voice = "   ".to_owned();
        let err = validate(&c).expect_err("ws voice");
        assert!(matches!(err, RealtimeError::BadConfig(_)));
    }

    #[test]
    fn validate_is_deterministic_for_same_input() {
        let c = config();
        // Three calls; each should produce the same Ok/Err result.
        validate(&c).expect("a");
        validate(&c).expect("b");
        validate(&c).expect("c");
    }
}
