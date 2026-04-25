//! Capability-fallback strategies per spec §9.
//!
//! [`crate::RealtimeCapabilities`] is a `bool` matrix: each backend
//! reports which primitives it supports natively. Callers in
//! `heron-policy` then have to write the same defensive
//! `if caps.atomic_response_cancel { native } else { emulate }`
//! branch every time they want to invoke a primitive.
//!
//! This module folds that branch into a single planner call. Each
//! [`crate::RealtimeBackend`] primitive maps to a [`Strategy`] enum
//! (Native | EmulateVia* | NoOp) that the controller can match
//! exhaustively. The strategies don't *do* anything — they describe
//! what to do — so the planner stays a pure / synchronous library.
//!
//! ## Why an intermediate enum and not a closure / dyn fn
//!
//! - Closures hide the choice from logs; an enum value can be
//!   formatted into the audit log so an operator sees "we routed
//!   `cancel` via `EmulateViaTruncate` because the backend's
//!   `atomic_response_cancel = false`."
//! - Tests can assert `Strategy::Native` vs `Strategy::Emulate*`
//!   without spinning up a real backend.
//! - Future fallback policies (e.g., feature-flag the emulator off
//!   for safety-critical sessions) plug in by mapping to the same
//!   enum.

use crate::RealtimeCapabilities;

/// How the controller should fulfill a `cancel`-shaped request.
///
/// `#[non_exhaustive]` so adding a `NoOp` variant for a future
/// session-less backend (raw TTS without `truncate`) is non-breaking;
/// today's `RealtimeBackend` trait requires `truncate_current`, so
/// every concrete backend supports at least the truncate emulation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CancelStrategy {
    /// Backend supports `response.cancel` atomically. Send the
    /// native API call.
    Native,
    /// Backend has no atomic cancel. Emulate via
    /// `conversation.item.truncate` + a model-side
    /// "stop speaking" instruction. Audible-cut quality is worse
    /// than Native but the agent stops promptly.
    EmulateViaTruncate,
}

/// How the controller should fulfill a barge-in event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BargeInStrategy {
    /// Backend has server-side VAD with `interrupt_response`. The
    /// backend itself fires barge-in; the controller just forwards
    /// the resulting `Cancelled` event.
    ServerSideVad,
    /// Backend has VAD but no auto-interrupt. Controller listens
    /// for `InputSpeechStarted` and fires `cancel` itself, routed
    /// through [`CancelStrategy`].
    ClientSideCancel,
    /// Backend has no VAD. Controller runs its own VAD against the
    /// inbound PCM frames and decides when to fire `cancel`.
    LocalVad,
}

/// How the controller should fulfill a tool-result injection.
///
/// `#[non_exhaustive]` so adding a `NotSupported` variant for a
/// future fail-fast path (a backend that explicitly forbids tool
/// calls) is non-breaking. Today's planner always returns one of
/// the two real variants because every backend that exposes a
/// session model supports synthetic-conversation-turn emulation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ToolResultStrategy {
    /// Backend supports tool calls natively. Send the native API
    /// payload.
    Native,
    /// Backend has no tool surface. Controller must inject the
    /// result as a synthetic conversation turn (the
    /// "function-call-emulation" pattern); ergonomics are worse
    /// because the model loses the structured-call contract.
    EmulateViaConversationTurn,
}

/// How the controller surfaces text deltas to the diagnostics tab
/// (transcripts, partial-response display).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TextDeltaStrategy {
    /// Backend emits `ResponseTextDelta`. Forward verbatim.
    Native,
    /// Backend emits audio only. Controller may run its own STT
    /// over the synthesized audio if a transcript is required.
    EmulateViaSttPassthrough,
    /// Diagnostics tab gets no per-token text; only final
    /// transcripts after the response completes.
    OnlyAtCompletion,
}

/// One-stop planner: turn the capability matrix into a per-primitive
/// strategy bundle. Pure / synchronous so the orchestrator can call
/// it once at session start and cache the result for the session's
/// lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StrategyPlan {
    pub cancel: CancelStrategy,
    pub barge_in: BargeInStrategy,
    pub tool_result: ToolResultStrategy,
    pub text_delta: TextDeltaStrategy,
}

/// Resolve a [`StrategyPlan`] for `caps`. Deterministic — same
/// input maps to the same plan every time, so the audit log can
/// pin "this session's strategy bundle was X" once.
pub fn plan(caps: &RealtimeCapabilities) -> StrategyPlan {
    let cancel = if caps.atomic_response_cancel {
        CancelStrategy::Native
    } else {
        // We don't have a separate "supports truncate" capability
        // today; per spec §9, every backend that exposes a session
        // model exposes truncate too (OpenAI Realtime,
        // Gemini Live, LiveKit) — backends without truncate are
        // raw TTS pipelines that also don't have sessions.
        // Treat absent atomic-cancel as "use truncate emulation."
        CancelStrategy::EmulateViaTruncate
    };

    // Per the variant docs: ClientSideCancel requires the backend
    // to emit `InputSpeechStarted` (= server_vad), so it must NOT
    // fire when the backend has no VAD at all. Backends without VAD
    // need LocalVad (controller runs its own VAD) regardless of
    // whether they support atomic cancel.
    //
    // Within `server_vad = true`: ServerSideVad if the backend can
    // also auto-interrupt; ClientSideCancel if VAD-only and the
    // controller has to fire `cancel` itself. We use atomic_response_
    // cancel as the closest proxy for "interrupt_response auto" today;
    // the capability matrix doesn't distinguish them yet.
    let barge_in = if caps.server_vad {
        if caps.atomic_response_cancel {
            BargeInStrategy::ServerSideVad
        } else {
            BargeInStrategy::ClientSideCancel
        }
    } else {
        BargeInStrategy::LocalVad
    };

    let tool_result = if caps.tool_calling {
        ToolResultStrategy::Native
    } else {
        // Two cases collapse here: (a) backend has a conversation
        // model but no tool calls — emulate; (b) backend has neither
        // — orchestrator built the wrong session. We can't
        // distinguish from the bool alone, so default to emulation
        // and let the call-time error surface the wrong-config case.
        ToolResultStrategy::EmulateViaConversationTurn
    };

    let text_delta = if caps.text_deltas {
        TextDeltaStrategy::Native
    } else if caps.bidirectional_audio {
        // Backend has audio but no text — STT passthrough is the
        // only way to get a transcript.
        TextDeltaStrategy::EmulateViaSttPassthrough
    } else {
        // Neither; only final transcripts (if any).
        TextDeltaStrategy::OnlyAtCompletion
    };

    StrategyPlan {
        cancel,
        barge_in,
        tool_result,
        text_delta,
    }
}

impl StrategyPlan {
    /// `true` if every primitive resolves to a "native" path. Used
    /// by the diagnostics tab to surface "you're on the fast path"
    /// vs "you're on a degraded fallback for X." Tool-result not
    /// supported is treated as not-fast (forces config attention).
    pub fn all_native(&self) -> bool {
        matches!(self.cancel, CancelStrategy::Native)
            && matches!(self.barge_in, BargeInStrategy::ServerSideVad)
            && matches!(self.tool_result, ToolResultStrategy::Native)
            && matches!(self.text_delta, TextDeltaStrategy::Native)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn caps(audio: bool, vad: bool, cancel: bool, tools: bool, text: bool) -> RealtimeCapabilities {
        RealtimeCapabilities {
            bidirectional_audio: audio,
            server_vad: vad,
            atomic_response_cancel: cancel,
            tool_calling: tools,
            text_deltas: text,
        }
    }

    #[test]
    fn fully_capable_backend_resolves_all_native() {
        let p = plan(&caps(true, true, true, true, true));
        assert_eq!(p.cancel, CancelStrategy::Native);
        assert_eq!(p.barge_in, BargeInStrategy::ServerSideVad);
        assert_eq!(p.tool_result, ToolResultStrategy::Native);
        assert_eq!(p.text_delta, TextDeltaStrategy::Native);
        assert!(p.all_native());
    }

    #[test]
    fn fully_incapable_backend_falls_back_on_every_primitive() {
        let p = plan(&caps(false, false, false, false, false));
        assert_eq!(p.cancel, CancelStrategy::EmulateViaTruncate);
        assert_eq!(p.barge_in, BargeInStrategy::LocalVad);
        assert_eq!(
            p.tool_result,
            ToolResultStrategy::EmulateViaConversationTurn
        );
        assert_eq!(p.text_delta, TextDeltaStrategy::OnlyAtCompletion);
        assert!(!p.all_native());
    }

    #[test]
    fn server_vad_with_cancel_picks_server_side_vad() {
        // Phase-52 fix: ServerSideVad requires server_vad=true AND
        // atomic_response_cancel=true (auto-interrupt). The
        // pre-fix logic returned ServerSideVad for any
        // server_vad=true case regardless of cancel.
        let p = plan(&caps(true, true, true, true, true));
        assert_eq!(p.barge_in, BargeInStrategy::ServerSideVad);
    }

    #[test]
    fn server_vad_without_cancel_picks_client_side_cancel() {
        // Phase-52 fix: ClientSideCancel is "backend has VAD but
        // controller fires cancel itself" — that requires
        // server_vad=true (so we get InputSpeechStarted events) +
        // atomic_response_cancel=false.
        let p = plan(&caps(true, true, false, true, true));
        assert_eq!(p.barge_in, BargeInStrategy::ClientSideCancel);
    }

    #[test]
    fn no_vad_picks_local_vad_regardless_of_cancel() {
        // Phase-52 fix: LocalVad is the only valid choice when the
        // backend doesn't emit InputSpeechStarted events. The pre-
        // fix logic incorrectly mapped (no VAD, has cancel) to
        // ClientSideCancel, which would have left the controller
        // listening for events that never arrive.
        for cancel in [false, true] {
            let p = plan(&caps(true, false, cancel, true, true));
            assert_eq!(
                p.barge_in,
                BargeInStrategy::LocalVad,
                "no VAD must always pick LocalVad (cancel={cancel})"
            );
        }
    }

    #[test]
    fn missing_atomic_cancel_emulates_via_truncate() {
        let p = plan(&caps(true, true, false, true, true));
        assert_eq!(p.cancel, CancelStrategy::EmulateViaTruncate);
    }

    #[test]
    fn missing_tool_calling_emulates_via_conversation_turn() {
        let p = plan(&caps(true, true, true, false, true));
        assert_eq!(
            p.tool_result,
            ToolResultStrategy::EmulateViaConversationTurn
        );
    }

    #[test]
    fn missing_text_deltas_with_audio_emulates_via_stt() {
        // Audio-only backend (raw TTS pipeline that also captures
        // input audio) — STT passthrough is the only transcript
        // path.
        let p = plan(&caps(true, true, true, true, false));
        assert_eq!(p.text_delta, TextDeltaStrategy::EmulateViaSttPassthrough);
    }

    #[test]
    fn missing_text_deltas_and_audio_falls_to_only_at_completion() {
        // No audio AND no text deltas — only final transcripts
        // (the "request-response" backend shape).
        let p = plan(&caps(false, true, true, true, false));
        assert_eq!(p.text_delta, TextDeltaStrategy::OnlyAtCompletion);
    }

    #[test]
    fn all_native_only_true_when_every_primitive_is_native() {
        // Verify the predicate flips on a single non-native
        // primitive at a time.
        let mut full = caps(true, true, true, true, true);
        assert!(plan(&full).all_native());

        // One non-native primitive is enough to flip the predicate.
        for flip in 0..5 {
            let mut c = full;
            match flip {
                0 => c.atomic_response_cancel = false,
                1 => c.server_vad = false,
                2 => c.tool_calling = false,
                3 => c.text_deltas = false,
                _ => c.bidirectional_audio = false,
            }
            // bidirectional_audio alone doesn't flip cancel/vad/etc.
            // Expect all_native true unless one of the four
            // strategy-driving caps flipped.
            let p = plan(&c);
            if flip == 4 {
                // bidirectional_audio false + text_deltas true =
                // text strategy stays Native, all four still
                // native. Pin the carve-out.
                assert!(p.all_native(), "flip {flip} should keep all-native");
            } else {
                assert!(!p.all_native(), "flip {flip} should break all-native");
            }
            full = caps(true, true, true, true, true);
        }
    }

    #[test]
    fn plan_is_deterministic_for_same_input() {
        // Audit log + diagnostics depend on this. Same caps in →
        // same StrategyPlan out, every call.
        let c = caps(true, false, true, true, false);
        let p1 = plan(&c);
        let p2 = plan(&c);
        let p3 = plan(&c);
        assert_eq!(p1, p2);
        assert_eq!(p2, p3);
    }

    #[test]
    fn openai_realtime_profile_resolves_all_native() {
        // Approximate OpenAI Realtime's published capability matrix
        // as of writing: bidirectional audio, server VAD, atomic
        // cancel, tool calling, text deltas. Pin so a future
        // change to the planner that broke the OpenAI fast path
        // surfaces here.
        let p = plan(&caps(true, true, true, true, true));
        assert!(p.all_native());
    }

    #[test]
    fn raw_tts_profile_resolves_to_emulation_everywhere() {
        // A raw TTS pipeline: audio-only, no VAD, no atomic cancel,
        // no tools, no text. Pin the worst case so a future
        // planner change doesn't accidentally upgrade emulation
        // paths.
        let p = plan(&caps(true, false, false, false, false));
        assert_eq!(p.cancel, CancelStrategy::EmulateViaTruncate);
        assert_eq!(p.barge_in, BargeInStrategy::LocalVad);
        assert_eq!(
            p.tool_result,
            ToolResultStrategy::EmulateViaConversationTurn
        );
        assert_eq!(p.text_delta, TextDeltaStrategy::EmulateViaSttPassthrough);
    }
}
