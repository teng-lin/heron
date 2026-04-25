//! `heron-bridge` — audio I/O bridge between `heron-bot` and `heron-realtime`.
//!
//! Layer 2 of the four-layer v2 architecture per
//! [`docs/api-design-spec.md`](../../../docs/api-design-spec.md) §1.
//! The hardest engineering surface in v2: getting 16kHz PCM out of
//! whatever the driver exposes (Recall WebSocket bytes, Attendee live
//! PCM, native callbacks) into the realtime backend's input stream,
//! and getting synthesized TTS bytes back into the bot's outbound
//! audio sink — without echo cancellation hell.
//!
//! The audio bridge does NOT make product decisions (that's
//! `heron-policy`) and does NOT talk to the LLM (that's
//! `heron-realtime`). It is purely mechanical: resample, mix, AEC,
//! jitter buffer, route.
//!
//! Vendor-neutral. The driver pushes frames in; the realtime backend
//! pulls frames out. Same in reverse for TTS playback.

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::mpsc;

/// Single audio chunk crossing a layer boundary. 16kHz mono i16 by
/// convention; the bridge resamples on input/output as needed.
#[derive(Debug, Clone)]
pub struct PcmFrame {
    pub samples: Vec<i16>,
    /// Capture timestamp at the source (driver). Lets the realtime
    /// layer reason about latency without needing to query the bridge.
    pub captured_at_micros: u64,
    /// Channel hint. Realtime backends that support diarization use
    /// this; others ignore.
    pub channel: AudioChannel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioChannel {
    /// Audio from the meeting (other participants).
    MeetingIn,
    /// Audio synthesized by us (TTS), being played into the meeting.
    /// Tagged so the bridge can subtract from MeetingIn for echo
    /// cancellation.
    AgentOut,
}

#[derive(Debug, Error)]
pub enum BridgeError {
    #[error("not yet implemented")]
    NotYetImplemented,
    #[error("resample failed: {0}")]
    Resample(String),
    #[error("upstream channel closed")]
    Closed,
    /// AEC was unable to track the agent's outbound audio (e.g. driver
    /// dropped the AgentOut tap). Higher layers should mute the agent
    /// to avoid feedback.
    #[error("acoustic echo cancellation tracking lost")]
    AecLost,
}

/// The bridge's contract. Implementations: `WebRtcAecBridge` (uses
/// `webrtc-audio-processing` from workspace deps), or a passthrough
/// `NaiveBridge` for tests.
#[async_trait]
pub trait AudioBridge: Send + Sync {
    /// Sender the driver pushes captured PCM into. The bridge fans
    /// out to `realtime_in()` (after AEC + resample) and to vault
    /// recording.
    fn meeting_in_sink(&self) -> mpsc::Sender<PcmFrame>;

    /// Sender the realtime backend / TTS pushes synthesized PCM into.
    /// The bridge fans out to driver playback AND to its own AEC
    /// reference channel.
    fn agent_out_sink(&self) -> mpsc::Sender<PcmFrame>;

    /// Receiver the realtime backend pulls cleaned audio from.
    /// Already AEC'd, resampled to 16kHz mono.
    fn realtime_in(&self) -> mpsc::Receiver<PcmFrame>;

    /// Receiver the driver pulls outbound audio from to push back into
    /// the meeting. Resampled to whatever the driver expects.
    fn driver_out(&self) -> mpsc::Receiver<PcmFrame>;

    /// Bridge health. Spec §7: AEC tracking loss is a soft failure —
    /// higher layers can choose to mute or to keep speaking with
    /// degraded quality.
    fn health(&self) -> BridgeHealth;
}

#[derive(Debug, Clone, Copy)]
pub struct BridgeHealth {
    pub aec_tracking: bool,
    pub jitter_ms: f32,
    /// Frames dropped at the boundary in the last second. >0 means
    /// the consumer is too slow.
    pub recent_drops: u32,
}
