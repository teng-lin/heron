//! `RecallDriver` — the first concrete [`MeetingBotDriver`] impl.
//!
//! Ports the validated Recall.ai REST surface from
//! `examples/recall-spike.rs` into a real driver. Per
//! [`docs/build-vs-buy-decision.md`](../../../../docs/build-vs-buy-decision.md),
//! Recall is the explicitly chosen v2.0 path and the spike (run on
//! 2026-04-26) exercised every operation against a live Zoom
//! meeting; see [`docs/spike-findings.md`](../../../../docs/spike-findings.md)
//! for what we learned.
//!
//! ## Module layout
//!
//! - [`client`] — narrow REST wrapper. Owns the wire contract +
//!   429/507/Retry-After handling. No FSM logic.
//! - [`projection`] — pure mapping from Recall `status_changes` codes
//!   to [`crate::BotEvent`] / [`crate::BotState`]. Easy to unit-test
//!   without spinning up a runtime.
//! - this file (`driver`) — the [`MeetingBotDriver`] impl + the
//!   per-bot polling task. Owns the in-flight bot map.
//!
//! ## What's deferred (see PR body)
//!
//! 1. **Real TTS.** The placeholder MP3 below is a 0.5s silent frame
//!    so Recall accepts the bot. Real disclosure audio + the
//!    `output_audio` POST live in a follow-up — heron has no TTS
//!    today (see `docs/build-vs-buy-decision.md` audit table).
//! 2. **Webhook receiver.** Recall pushes status_changes via webhook
//!    for low-latency updates. This driver polls every 3s — fine for
//!    the first PR; webhook endpoint lands later.
//! 3. **Real disclosure vars.** [`crate::DisclosureVars`] needs
//!    `user_name` + `meeting_title`; we use placeholders here. A
//!    follow-up plumbs these through `BotCreateArgs` (or a dedicated
//!    rendered-text field).
//! 4. **Vendor-id reverse lookup.** The map is keyed by heron
//!    `BotId`; webhooks will need a `vendor_id → BotId` reverse
//!    index. Lands with the webhook receiver.
//! 5. **Entry reaping.** Terminal entries stay in the map for the
//!    lifetime of the driver so `current_state` / `subscribe_state`
//!    answer correctly post-mortem. v2 is a singleton driver so
//!    growth is one entry per session — fine for sessions/day order
//!    of magnitude. A long-running orchestrator should add a
//!    background sweeper (e.g. evict terminal entries older than
//!    1h); not needed for the alpha.

mod client;
mod projection;

use std::collections::HashMap;
use std::env;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::Utc;
use thiserror::Error;
use tokio::sync::{broadcast, oneshot};
use tokio::task::JoinHandle;

use crate::{
    BotCreateArgs, BotError, BotEvent, BotFsm, BotId, BotState, BotStateEvent, DisclosureVars,
    DriverCapabilities, MeetingBotDriver, Platform, render_disclosure,
};
use client::{Client, ClientConfig, CreateBotArgs, HttpError};
use projection::{Projection, project_status_change};

/// 0.5s silent mono MP3 @ 22.05kHz. Embedded so the driver works
/// out-of-the-box without a sidecar file. Recall requires
/// `automatic_audio_output.in_call_recording.data` at create time
/// before `output_audio` will work later (see
/// `docs/spike-findings.md` §"Recommendations" item 5). Once real
/// TTS lands the disclosure audio replaces this fixture.
const PLACEHOLDER_AUDIO: &[u8] = include_bytes!("disclosure-placeholder.mp3");

/// Default polling interval. Recall's REST polling is documented as
/// "every few seconds is fine" — 3s is the spike's chosen middle.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(3);

/// Default subscriber-channel capacity. Most consumers (the
/// orchestrator + UI) read every event; a depth of 32 leaves slack
/// for slow drains without unbounded memory.
const SUBSCRIBER_CAPACITY: usize = 32;

/// Grace window the polling task waits before firing the synthetic
/// `Init → Joining` ladder, so a caller has time to subscribe after
/// `bot_create` returns. Per the gemini-code-assist review on PR
/// #121, the full poll interval was needlessly slow for the UX —
/// 100ms is enough to cover the typical async-yield + subscribe()
/// path. Capped via `min(poll_interval)` at the call site so test
/// configs that drop the interval to a few ms (`TEST_POLL = 50ms`)
/// still leave room.
const SUBSCRIBER_GRACE: Duration = Duration::from_millis(100);

/// Configuration for [`RecallDriver`]. Field-by-field public so the
/// orchestrator can override individual knobs (region, poll interval)
/// without having to fall back to env-only construction.
#[derive(Debug, Clone)]
pub struct RecallDriverConfig {
    pub api_key: String,
    /// e.g. `"https://us-west-2.recall.ai"` (no trailing slash).
    pub base_url: String,
    /// Per-poll interval per bot. Default [`DEFAULT_POLL_INTERVAL`].
    pub poll_interval: Duration,
    /// Display name shown to other meeting participants. Spec §4
    /// treats the roster name itself as a form of disclosure;
    /// default is `"heron"`.
    pub bot_name: String,
}

impl Default for RecallDriverConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            base_url: "https://us-east-1.recall.ai".into(),
            poll_interval: DEFAULT_POLL_INTERVAL,
            bot_name: "heron".into(),
        }
    }
}

/// Errors construction-time configuration can raise. Distinct from
/// [`BotError`] because they fire before the driver exists — a caller
/// that hits these has a misconfigured env, not a misbehaving meeting.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("RECALL_API_KEY is unset")]
    MissingApiKey,
    #[error("RECALL_API_KEY is empty")]
    EmptyApiKey,
    #[error(
        "RECALL_REGION={region:?} not recognized; \
         use one of us-west-2, us-east-1, eu-central-1, ap-northeast-1, \
         or override RECALL_BASE_URL"
    )]
    UnknownRegion { region: String },
    #[error("recall http client build failed: {0}")]
    Http(String),
}

impl RecallDriverConfig {
    /// Read the API key + region from the environment. Mirrors the
    /// spike harness's `Client::from_env` so deployment instructions
    /// stay aligned.
    pub fn from_env() -> Result<Self, ConfigError> {
        let api_key = env::var("RECALL_API_KEY").map_err(|_| ConfigError::MissingApiKey)?;
        if api_key.trim().is_empty() {
            return Err(ConfigError::EmptyApiKey);
        }
        let base_url = match env::var("RECALL_BASE_URL") {
            Ok(s) if !s.trim().is_empty() => s,
            _ => resolve_region_url(env::var("RECALL_REGION").ok().as_deref())?,
        };
        Ok(Self {
            api_key,
            base_url,
            ..Self::default()
        })
    }
}

/// Map `RECALL_REGION` → base URL. Mirrors the spike's resolver byte-
/// for-byte so the driver and harness can read the same `.env`.
fn resolve_region_url(region: Option<&str>) -> Result<String, ConfigError> {
    let r = region.map(str::trim).filter(|s| !s.is_empty());
    let host = match r {
        None => "us-east-1",
        Some("us-west-2" | "us_west_2") => "us-west-2",
        Some("us-east-1" | "us_east_1") => "us-east-1",
        Some("eu-central-1" | "eu_central_1" | "eu") => "eu-central-1",
        Some("ap-northeast-1" | "ap_northeast_1" | "apac") => "ap-northeast-1",
        Some(other) => {
            return Err(ConfigError::UnknownRegion {
                region: other.to_string(),
            });
        }
    };
    Ok(format!("https://{host}.recall.ai"))
}

// ── per-bot tracking ───────────────────────────────────────────────────

/// Everything the driver tracks per in-flight bot. The polling task
/// owns a clone of `tx` + cancellation receiver; the driver methods
/// own the map mutation.
struct BotEntry {
    /// Recall's id for this bot. Distinct from the heron `BotId`
    /// (Invariant 4): heron mints its own id at `bot_create` time
    /// and keeps the vendor id private.
    vendor_id: String,
    /// Latest known FSM state. The polling task writes; driver
    /// methods read.
    state: BotState,
    /// Echoed `BotCreateArgs::metadata`. Carried verbatim onto every
    /// `BotStateEvent` so subscribers can correlate without a
    /// separate lookup.
    metadata: serde_json::Value,
    /// Per-bot broadcast channel for [`BotStateEvent`].
    tx: broadcast::Sender<BotStateEvent>,
    /// Cancellation handle for the polling task. Dropped on terminal
    /// transition or when the driver explicitly fires it.
    cancel: Option<oneshot::Sender<()>>,
    /// Polling task handle. Kept so `Drop` can abort it if the
    /// driver itself is dropped mid-meeting (orphaned bots are a
    /// real cost concern per spike findings).
    poll_task: Option<JoinHandle<()>>,
}

impl BotEntry {
    fn is_active(&self) -> bool {
        !is_terminal_state(&self.state)
    }
}

fn is_terminal_state(state: &BotState) -> bool {
    matches!(
        state,
        BotState::Completed
            | BotState::Failed { .. }
            | BotState::Ejected { .. }
            | BotState::HostEnded
    )
}

// ── driver ─────────────────────────────────────────────────────────────

/// Concrete [`MeetingBotDriver`] for Recall.ai. Cheap to clone — the
/// inner state lives behind an `Arc`.
#[derive(Clone)]
pub struct RecallDriver {
    inner: Arc<DriverInner>,
}

struct DriverInner {
    client: Client,
    /// `std::sync::Mutex` rather than `tokio::sync::Mutex` because
    /// every critical section against this map is short and *never*
    /// holds the lock across an `.await` (Rust's send-checker enforces
    /// this — `std::sync::MutexGuard` is `!Send`). The synchronous
    /// trait methods (`current_state`, `subscribe_state`) need a
    /// non-async lock that returns a real result rather than the
    /// `try_lock`-only fallback an async mutex forces. Poisoning is
    /// recovered transparently — see [`DriverInner::lock_bots`].
    bots: Mutex<HashMap<BotId, BotEntry>>,
    /// Serializes `bot_create` calls so the singleton invariant
    /// (Spec §12 #7) holds even when two creates race. We *can't*
    /// hold the `bots` mutex across the create-bot HTTP call (sync
    /// guard is `!Send`); a separate async mutex covers the
    /// "check + insert" window.
    create_lock: tokio::sync::Mutex<()>,
    poll_interval: Duration,
    bot_name: String,
}

impl DriverInner {
    /// Acquire the bot map. A panic in another driver method can
    /// poison the mutex; we recover the inner data because the map
    /// is just `(BotId, BotEntry)` pairs — none of the entries hold
    /// invariants that a panicking writer could have left half-set.
    fn lock_bots(&self) -> std::sync::MutexGuard<'_, HashMap<BotId, BotEntry>> {
        match self.bots.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl RecallDriver {
    pub fn new(config: RecallDriverConfig) -> Result<Self, ConfigError> {
        let RecallDriverConfig {
            api_key,
            base_url,
            poll_interval,
            bot_name,
        } = config;
        let client = Client::new(ClientConfig::new(api_key, base_url))
            .map_err(|e| ConfigError::Http(e.to_string()))?;
        Ok(Self {
            inner: Arc::new(DriverInner {
                client,
                bots: Mutex::new(HashMap::new()),
                create_lock: tokio::sync::Mutex::new(()),
                poll_interval,
                bot_name,
            }),
        })
    }

    /// Test-only constructor that takes a pre-built [`Client`] so a
    /// wiremock test can wire its own `reqwest::Client` (e.g. with a
    /// reduced timeout).
    #[cfg(test)]
    fn from_client(client: Client, poll_interval: Duration, bot_name: String) -> Self {
        Self {
            inner: Arc::new(DriverInner {
                client,
                bots: Mutex::new(HashMap::new()),
                create_lock: tokio::sync::Mutex::new(()),
                poll_interval,
                bot_name,
            }),
        }
    }

    /// Gracefully shut down the driver: ask Recall to leave every
    /// active bot, then drain the polling tasks. After this returns,
    /// no in-flight bot is orphaned on the vendor side.
    ///
    /// Per the gemini-code-assist review on PR #121: [`Drop`] can
    /// only `abort()` the polling tasks (it cannot `await`), which
    /// leaves the bots running on Recall's side and accruing cost.
    /// `shutdown` is the explicit async path the orchestrator should
    /// call on its graceful-exit flow.
    ///
    /// Idempotent — a second call is a no-op once the first drained
    /// every entry. Vendor errors during the leave-call sweep are
    /// logged via `tracing::warn!` and never propagated; the goal of
    /// shutdown is "make best effort and exit cleanly," not surface
    /// individual failures to the caller (the orchestrator's exit
    /// path has nothing useful to do with them). Errors per-bot are
    /// already published on each bot's broadcast channel by the
    /// underlying `bot_leave` path, so subscribers still observe
    /// outcomes if they're listening.
    pub async fn shutdown(&self) {
        // Snapshot active bot ids under the sync lock; release before
        // awaiting (lock guard is `!Send` and `bot_leave` calls
        // through to async HTTP).
        let active_ids: Vec<BotId> = {
            let bots = self.inner.lock_bots();
            bots.iter()
                .filter_map(|(id, entry)| entry.is_active().then_some(*id))
                .collect()
        };

        for id in active_ids {
            if let Err(e) = self.bot_leave(id).await {
                tracing::warn!(
                    ?id,
                    error = %e,
                    "shutdown: bot_leave failed; falling back to polling-task abort",
                );
            }
        }

        // Drain any polling tasks the leave sweep didn't already
        // collect (e.g. bots that were already terminal but whose
        // task had not yet observed cancellation). Take the handles
        // under the lock, then await outside it so a slow task
        // doesn't block driver methods on other threads.
        let handles: Vec<JoinHandle<()>> = {
            let mut bots = self.inner.lock_bots();
            bots.values_mut()
                .filter_map(|entry| entry.poll_task.take())
                .collect()
        };
        for handle in handles {
            // `abort` first so a still-polling task exits at its next
            // poll point; then await the JoinError-or-Ok so we don't
            // return before the runtime has reaped the task.
            handle.abort();
            let _ = handle.await;
        }
    }
}

#[async_trait]
impl MeetingBotDriver for RecallDriver {
    async fn bot_create(&self, args: BotCreateArgs) -> Result<BotId, BotError> {
        // Invariant 6: empty disclosure → reject. Trim so a template
        // that is whitespace-only is treated as empty.
        if args.disclosure.text_template.trim().is_empty() {
            return Err(BotError::NoDisclosureProfile);
        }
        // Invariant 8: persona required. The trait surface uses
        // `PersonaId::nil()` as the sentinel "missing"; a valid
        // persona never serializes as the all-zero UUID.
        if args.persona_id.0.is_nil() {
            return Err(BotError::Vendor("persona_id required".into()));
        }
        // Validate the disclosure template up-front so a template
        // error surfaces as a vendor error before we hit the wire.
        // The rendered text is unused today — TTS / `output_audio`
        // playback is a separate gap (see module preamble) — but
        // failing fast here means a malformed template can't reach
        // Recall's billing surface. Real user/title vars are
        // deferred; we pass meaningful placeholders so the template
        // at least exercises both substitutions.
        render_disclosure(
            &args.disclosure.text_template,
            &DisclosureVars {
                user_name: "the user",
                meeting_title: &args.meeting_url,
            },
        )
        .map_err(|e| BotError::Vendor(format!("disclosure template render failed: {e}")))?;

        // Invariant 7: singleton check. The sync `bots` mutex can't
        // be held across the HTTP create call (its guard is `!Send`),
        // so an async `create_lock` covers the
        // "check + HTTP + insert" window — that's the only call site
        // that contends, so a reader's `lock_bots()` stays fast.
        let _create_guard = self.inner.create_lock.lock().await;
        if let Some((existing, _)) = self.inner.lock_bots().iter().find(|(_, e)| e.is_active()) {
            return Err(BotError::BotAlreadyActive {
                existing: *existing,
            });
        }

        let placeholder_b64 = B64.encode(PLACEHOLDER_AUDIO);
        let recall_args = CreateBotArgs {
            meeting_url: &args.meeting_url,
            bot_name: &self.inner.bot_name,
            placeholder_audio_b64: &placeholder_b64,
            metadata: &args.metadata,
            metadata_header: None,
        };
        let detail = match self
            .inner
            .client
            .create_bot(recall_args, args.idempotency_key)
            .await
        {
            Ok(d) => d,
            Err(e) => return Err(map_http_error(e)),
        };

        // Mint a fresh heron BotId (Invariant 4) and stash the
        // vendor id privately. The polling task drives the FSM via
        // synthetic `Create / PersonaLoaded / TtsReady` events
        // immediately so the state lands in `Joining`, matching
        // Recall's first-status `joining_call`.
        let bot_id = BotId::now_v7();
        // `broadcast::Sender::new` allocates the channel; the
        // initial receiver is dropped immediately because subscribers
        // join later via [`subscribe_state`].
        let (tx, _rx0) = broadcast::channel(SUBSCRIBER_CAPACITY);
        let (cancel_tx, cancel_rx) = oneshot::channel();

        // Spawn the polling task BEFORE inserting so we can capture
        // its `JoinHandle` and stash it on the entry atomically with
        // the insert. This closes the race Codex flagged: previously
        // we inserted with `poll_task: None`, spawned, then re-locked
        // to set the handle — a `bot_leave` between those two steps
        // could fire `cancel` but couldn't `abort` because the handle
        // wasn't there yet.
        let poll_handle = spawn_poll_task(
            self.inner.clone(),
            bot_id,
            detail.id.clone(),
            args.metadata.clone(),
            cancel_rx,
        );
        let entry = BotEntry {
            vendor_id: detail.id.clone(),
            state: BotState::Init,
            metadata: args.metadata.clone(),
            tx,
            cancel: Some(cancel_tx),
            poll_task: Some(poll_handle),
        };
        self.inner.lock_bots().insert(bot_id, entry);

        Ok(bot_id)
    }

    async fn bot_leave(&self, id: BotId) -> Result<(), BotError> {
        // Idempotent: an unknown id (already-completed bot whose
        // entry was reaped, or a never-existed id) succeeds silently
        // per the trait doc.
        let snapshot = {
            let bots = self.inner.lock_bots();
            bots.get(&id)
                .map(|e| (e.vendor_id.clone(), e.state.clone()))
        };
        let (vendor_id, current_state) = match snapshot {
            Some(v) => v,
            None => return Ok(()),
        };

        // Already terminal — idempotent success, no double-publish.
        if is_terminal_state(&current_state) {
            return Ok(());
        }

        // Two paths through Recall depending on FSM position:
        //
        // * `InMeeting` / `Reconnecting` (or `Disclosing` — bot is
        //   admitted but pre-disclosure): POST `/leave_call/`. The
        //   FSM has a clean `LeaveRequested → LeaveFinalized` ladder
        //   for these states.
        //
        // * Pre-meeting (`Init`/`LoadingPersona`/`TtsWarming`/
        //   `Joining`): the bot has no presence to leave gracefully.
        //   Recall's `DELETE /bot/{id}/` is the only way to stop it
        //   pre-join (per spec §3 + spike findings). We route the
        //   leave to terminate-style semantics rather than synthesize
        //   an FSM-illegal `Leaving` from `Joining`. The trait doc
        //   notes leave is "graceful leave: bot speaks goodbye…"
        //   which assumes InMeeting; the pre-meeting reroute matches
        //   user intent ("end this bot now") without violating the
        //   FSM's transition table.
        let route_via_delete = matches!(
            current_state,
            BotState::Init | BotState::LoadingPersona | BotState::TtsWarming | BotState::Joining,
        );

        if route_via_delete {
            match self.inner.client.delete_bot(&vendor_id).await {
                Ok(()) | Err(HttpError::NotFound) => {}
                Err(e) => return Err(map_http_error(e)),
            }
        } else {
            match self.inner.client.leave_call(&vendor_id).await {
                Ok(()) | Err(HttpError::NotFound) => {}
                Err(e) => return Err(map_http_error(e)),
            }
        }

        // Drive the FSM and capture the publishing channel + states.
        // Lock is released before publishing so a slow subscriber
        // doesn't block other driver methods.
        let publish = {
            let mut bots = self.inner.lock_bots();
            let Some(entry) = bots.get_mut(&id) else {
                return Ok(());
            };
            if is_terminal_state(&entry.state) {
                // The polling task could have raced us to a terminal
                // state between the snapshot read and now. Honor it.
                return Ok(());
            }
            cancel_polling(entry);

            // Build a fresh FSM at the entry's current state and
            // attempt the canonical `Leaving → Completed` ladder.
            // For pre-meeting states the FSM has no `LeaveRequested`
            // transition; we synthesize `Completed` directly because
            // we already DELETEd the vendor bot — the lifecycle is
            // factually over even if the FSM has no event for it.
            let mut fsm = fsm_at(&entry.state);
            let mut steps: Vec<BotState> = Vec::new();
            if fsm.on_event(BotEvent::LeaveRequested).is_ok() {
                steps.push(fsm.state().clone());
                if fsm.on_event(BotEvent::LeaveFinalized).is_ok() {
                    steps.push(fsm.state().clone());
                } else {
                    steps.push(BotState::Completed);
                }
            } else {
                // Pre-meeting reroute: skip straight to Completed.
                // No spurious `Leaving` event because the bot never
                // had a meeting to leave.
                steps.push(BotState::Completed);
            }
            entry.state = steps.last().cloned().unwrap_or(BotState::Completed);
            (entry.tx.clone(), entry.metadata.clone(), steps)
        };
        let (tx, metadata, steps) = publish;
        for state in steps {
            let _ = tx.send(BotStateEvent {
                bot_id: id,
                at: Utc::now(),
                state,
                metadata: metadata.clone(),
            });
        }
        Ok(())
    }

    async fn bot_terminate(&self, id: BotId) -> Result<(), BotError> {
        let snapshot = {
            let bots = self.inner.lock_bots();
            bots.get(&id)
                .map(|e| (e.vendor_id.clone(), e.state.clone()))
        };
        let (vendor_id, current_state) = match snapshot {
            // Idempotent: unknown id → success.
            None => return Ok(()),
            Some(v) => v,
        };

        // Already terminal — idempotent success. Returning Ok(()) for
        // a bot that's already done matches `bot_leave`'s behavior
        // and the trait's "hard kill" intent: the bot is dead,
        // mission accomplished.
        if is_terminal_state(&current_state) {
            return Ok(());
        }

        // Trait surface: only legal in `Init | LoadingPersona |
        // TtsWarming | Joining`. Any "live" state (Disclosing,
        // InMeeting, Reconnecting) must reject. The error variant
        // is named `NotInMeeting` for historical reasons but the
        // trait surface owns the variants — we re-use it as "wrong
        // state for terminate."
        if !matches!(
            current_state,
            BotState::Init | BotState::LoadingPersona | BotState::TtsWarming | BotState::Joining,
        ) {
            return Err(BotError::NotInMeeting { current_state });
        }

        match self.inner.client.delete_bot(&vendor_id).await {
            Ok(()) | Err(HttpError::NotFound) => {}
            Err(e) => return Err(map_http_error(e)),
        }

        let publish = {
            let mut bots = self.inner.lock_bots();
            let Some(entry) = bots.get_mut(&id) else {
                return Ok(());
            };
            if is_terminal_state(&entry.state) {
                return Ok(());
            }
            cancel_polling(entry);
            // `Failed` carries the operator-visible reason; we use it
            // (rather than `Completed`) because terminate cuts a
            // pre-meeting bot short — no graceful close, no
            // disclosure spoken, nothing for the dashboard to claim
            // succeeded.
            entry.state = BotState::Failed {
                error: "terminated by caller before join".into(),
            };
            (
                entry.tx.clone(),
                entry.metadata.clone(),
                entry.state.clone(),
            )
        };
        let (tx, metadata, state) = publish;
        let _ = tx.send(BotStateEvent {
            bot_id: id,
            at: Utc::now(),
            state,
            metadata,
        });
        Ok(())
    }

    fn current_state(&self, id: BotId) -> Option<BotState> {
        // `std::sync::Mutex` keeps the synchronous trait signature
        // honest: critical sections never hold the lock across an
        // `.await`, so a brief block-on-contention is bounded.
        self.inner.lock_bots().get(&id).map(|e| e.state.clone())
    }

    fn subscribe_state(&self, id: BotId) -> broadcast::Receiver<BotStateEvent> {
        // Per trait: unknown bot → freshly-created closed channel.
        // We construct a small channel and immediately drop the
        // sender so `recv()` returns `Closed`.
        match self.inner.lock_bots().get(&id) {
            Some(entry) => entry.tx.subscribe(),
            None => {
                let (tx, rx) = broadcast::channel(1);
                drop(tx);
                rx
            }
        }
    }

    fn capabilities(&self) -> DriverCapabilities {
        DriverCapabilities {
            // Recall publicly supports Zoom, Google Meet, Teams,
            // Webex; Webex is excluded here because the spike didn't
            // exercise it and the spec wants only what's been
            // validated end-to-end. Adding Webex is a one-line
            // change after a Webex spike.
            platforms: &[
                Platform::Zoom,
                Platform::GoogleMeet,
                Platform::MicrosoftTeams,
            ],
            // Recall pushes partials over WebSocket. The current
            // RecallDriver consumes only the polling REST surface,
            // but the capability flag describes the vendor's
            // capability, not what this driver currently uses
            // (heron-policy gates feature exposure on it).
            live_partial_transcripts: true,
            granular_eject_reasons: true,
            raw_pcm_access: false,
        }
    }
}

impl Drop for DriverInner {
    fn drop(&mut self) {
        // Best-effort: abort polling tasks so they don't outlive the
        // driver. We don't hit Recall here — that would require
        // blocking in a Drop, which can deadlock the runtime. The
        // orchestrator should call `bot_leave` for every active bot
        // before dropping the driver to avoid orphaning a paid bot.
        //
        // `lock_bots()` recovers from poisoning, so a panic in
        // another driver method doesn't block clean shutdown. The
        // exclusive `&mut self` here means no other thread holds
        // the mutex.
        let mut bots = self.lock_bots();
        for entry in bots.values_mut() {
            if let Some(handle) = entry.poll_task.take() {
                handle.abort();
            }
        }
    }
}

// ── helpers ────────────────────────────────────────────────────────────

fn map_http_error(e: HttpError) -> BotError {
    match e {
        HttpError::Network(s) => BotError::Network(s),
        HttpError::NotFound => BotError::Vendor("recall: bot not found".into()),
        HttpError::RateLimited { retry_after_secs } => BotError::RateLimited { retry_after_secs },
        HttpError::CapacityExhausted { retry_after_secs } => {
            BotError::CapacityExhausted { retry_after_secs }
        }
        HttpError::Vendor { status, body } => BotError::Vendor(format!("status {status}: {body}")),
        HttpError::Build(s) | HttpError::Decode(s) => BotError::Vendor(s),
    }
}

fn cancel_polling(entry: &mut BotEntry) {
    if let Some(cancel) = entry.cancel.take() {
        let _ = cancel.send(());
    }
    if let Some(handle) = entry.poll_task.take() {
        handle.abort();
    }
}

/// Build a fresh [`BotFsm`] aligned to a known [`BotState`]. The FSM
/// has no public "set state" so we replay the canonical event ladder
/// up to `state` — used only for short hops by the leave path.
fn fsm_at(state: &BotState) -> BotFsm {
    let mut fsm = BotFsm::new();
    let ladder: &[BotEvent] = match state {
        BotState::Init => &[],
        BotState::LoadingPersona => &[BotEvent::Create],
        BotState::TtsWarming => &[BotEvent::Create, BotEvent::PersonaLoaded],
        BotState::Joining => &[
            BotEvent::Create,
            BotEvent::PersonaLoaded,
            BotEvent::TtsReady,
        ],
        BotState::Disclosing => &[
            BotEvent::Create,
            BotEvent::PersonaLoaded,
            BotEvent::TtsReady,
            BotEvent::JoinAccepted,
        ],
        BotState::InMeeting => &[
            BotEvent::Create,
            BotEvent::PersonaLoaded,
            BotEvent::TtsReady,
            BotEvent::JoinAccepted,
            BotEvent::DisclosureAcked,
        ],
        BotState::Reconnecting => &[
            BotEvent::Create,
            BotEvent::PersonaLoaded,
            BotEvent::TtsReady,
            BotEvent::JoinAccepted,
            BotEvent::DisclosureAcked,
            BotEvent::ConnectionLost,
        ],
        BotState::Leaving => &[
            BotEvent::Create,
            BotEvent::PersonaLoaded,
            BotEvent::TtsReady,
            BotEvent::JoinAccepted,
            BotEvent::DisclosureAcked,
            BotEvent::LeaveRequested,
        ],
        // Terminal states — the FSM rejects all further events. The
        // caller should be checking `is_terminal_state` first; we
        // return a fresh FSM as a safe fallback.
        BotState::Completed
        | BotState::Failed { .. }
        | BotState::Ejected { .. }
        | BotState::HostEnded => &[],
    };
    for ev in ladder {
        // Replay should always succeed by construction; if it doesn't,
        // we silently break and surface a fresh FSM rather than
        // panicking. The leave path tolerates this.
        if fsm.on_event(ev.clone()).is_err() {
            return BotFsm::new();
        }
    }
    fsm
}

/// Spawn the per-bot polling task. Loops on the configured interval,
/// fetches `GET /api/v1/bot/{vendor_id}/`, diffs `status_changes`
/// against last-seen length, projects each new entry, and publishes
/// state transitions.
fn spawn_poll_task(
    inner: Arc<DriverInner>,
    bot_id: BotId,
    vendor_id: String,
    metadata: serde_json::Value,
    mut cancel_rx: oneshot::Receiver<()>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut seen: usize = 0;
        // Two-stage initial wait (gemini-code-assist on PR #121):
        //
        //   1. A short [`SUBSCRIBER_GRACE`] window so callers have
        //      time to invoke `subscribe_state(bot_id)` after
        //      `bot_create` returns. Without it, the synthetic
        //      ladder would fire on a broadcast channel with no
        //      subscribers and the events would be dropped.
        //   2. The remainder of `poll_interval` so the first GET
        //      lands close to when Recall's `joining_call` status
        //      typically appears (~700ms after dispatch per the
        //      spike). The publish ladder fires between the two
        //      stages so the UI sees `Joining` after ~100ms instead
        //      of after the full poll interval.
        //
        // Cancellation via `select!` so an immediate `bot_leave` /
        // `bot_terminate` after `bot_create` exits before any
        // synthetic event lands.
        let grace = SUBSCRIBER_GRACE.min(inner.poll_interval);
        tokio::select! {
            _ = tokio::time::sleep(grace) => {}
            _ = &mut cancel_rx => {
                tracing::debug!(?bot_id, vendor_id, "polling task cancelled before initial ladder");
                return;
            }
        }

        // Re-check terminal state before publishing the ladder. A
        // `bot_leave` / `bot_terminate` could have completed during
        // the sleep without firing the cancel oneshot in time (the
        // oneshot is fire-and-forget from the driver's side). Reading
        // state under the sync lock is cheap.
        if entry_is_terminal(&inner, bot_id) {
            tracing::debug!(
                ?bot_id,
                "polling task exiting before initial ladder; entry already terminal"
            );
            return;
        }

        // Drive the synthetic pre-flight ladder so the FSM lands in
        // `Joining` quickly after `bot_create` returns. Recall's
        // first status code is `joining_call`; matching the FSM up
        // to it before the first poll keeps published transitions
        // monotonic.
        publish_initial_ladder(&inner, bot_id, &metadata);

        // Now sleep the rest of `poll_interval` before the first
        // network poll. Re-uses the cancel branch.
        let remainder = inner.poll_interval.saturating_sub(grace);
        if !remainder.is_zero() {
            tokio::select! {
                _ = tokio::time::sleep(remainder) => {}
                _ = &mut cancel_rx => {
                    tracing::debug!(?bot_id, vendor_id, "polling task cancelled before first poll");
                    return;
                }
            }
        }

        loop {
            // Fast-fail on cancellation. `try_recv` never blocks; if
            // the sender already dropped (driver gone), we still
            // exit cleanly.
            if cancel_rx.try_recv() != Err(oneshot::error::TryRecvError::Empty) {
                tracing::debug!(?bot_id, vendor_id, "polling task cancelled");
                return;
            }

            // Single poll pass — encapsulated so the `?`-style early
            // returns don't risk skipping the inter-iteration sleep
            // (a regression that landed during code review: a `continue`
            // mid-loop spun the CPU on a network failure).
            let should_exit = poll_once(&inner, bot_id, &vendor_id, &metadata, &mut seen).await;
            if should_exit {
                return;
            }

            // Sleep between iterations OR exit early on cancel.
            // `select!` polls both branches; the cancel branch is a
            // mutable reference borrow rather than a move so the
            // receiver stays alive across loop iterations.
            tokio::select! {
                _ = tokio::time::sleep(inner.poll_interval) => {}
                _ = &mut cancel_rx => {
                    tracing::debug!(?bot_id, vendor_id, "polling task cancelled mid-sleep");
                    return;
                }
            }
        }
    })
}

/// One poll cycle. Returns `true` when the polling task should
/// terminate (bot evicted or reached a terminal state). Network
/// failures + empty diffs both return `false` so the outer loop
/// proceeds to its mandatory sleep — without that, a string of
/// failed polls would burn CPU + the rate-limit budget.
async fn poll_once(
    inner: &DriverInner,
    bot_id: BotId,
    vendor_id: &str,
    metadata: &serde_json::Value,
    seen: &mut usize,
) -> bool {
    let detail = match inner.client.get_bot(vendor_id).await {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(
                ?bot_id, vendor_id, error = %e,
                "recall poll failed; will retry on next interval",
            );
            return false;
        }
    };

    let total = detail.status_changes.len();
    if total > *seen {
        let new_changes = &detail.status_changes[*seen..];
        for change in new_changes {
            process_change(inner, bot_id, metadata, change);
        }
        *seen = total;
    }

    let bots = inner.lock_bots();
    let Some(entry) = bots.get(&bot_id) else {
        tracing::debug!(?bot_id, "bot evicted; polling task exiting");
        return true;
    };
    if is_terminal_state(&entry.state) {
        tracing::debug!(?bot_id, state = ?entry.state, "polling task exiting on terminal");
        return true;
    }
    false
}

/// Cheap snapshot read used by the polling task to decide whether to
/// exit before publishing more events. Returns `true` if the bot is
/// missing or already in a terminal state — both mean "stop work."
fn entry_is_terminal(inner: &DriverInner, bot_id: BotId) -> bool {
    inner
        .lock_bots()
        .get(&bot_id)
        .map(|e| is_terminal_state(&e.state))
        .unwrap_or(true)
}

/// Drive `Init → LoadingPersona → TtsWarming → Joining` so subscribers
/// see the synthetic pre-flight transitions even on platforms where
/// Recall's first observable state is `joining_call`. Splits the
/// trait's three "warm-up" hops into individual broadcast events so
/// the orchestrator's dashboard tracks them.
///
/// Holds the bot mutex once for the whole ladder. Per Gemini's
/// "atomicity in publish_initial_ladder" suggestion: re-acquiring
/// per event was wasted overhead and let `bot_leave` interleave
/// midway through the ladder. The single critical section here is
/// pure synchronous Rust.
fn publish_initial_ladder(inner: &DriverInner, bot_id: BotId, metadata: &serde_json::Value) {
    let mut bots = inner.lock_bots();
    let Some(entry) = bots.get_mut(&bot_id) else {
        return;
    };
    // Bail if the entry was driven terminal between the polling
    // task's pre-ladder check and now — readers of the FSM see a
    // monotonic state stream.
    if is_terminal_state(&entry.state) {
        return;
    }
    let mut fsm = fsm_at(&entry.state);
    for ev in [
        BotEvent::Create,
        BotEvent::PersonaLoaded,
        BotEvent::TtsReady,
    ] {
        let Ok(_) = fsm.on_event(ev.clone()) else {
            tracing::warn!(?bot_id, ?ev, state = ?entry.state, "synthetic ladder event rejected by FSM");
            continue;
        };
        entry.state = fsm.state().clone();
        let _ = entry.tx.send(BotStateEvent {
            bot_id,
            at: Utc::now(),
            state: entry.state.clone(),
            metadata: metadata.clone(),
        });
    }
}

/// Process one Recall `status_changes` entry. Drives the FSM via the
/// projection; on illegal transitions, logs + ignores rather than
/// panicking. Synthetic terminals bypass the FSM (Recall collapses
/// states the FSM models as multi-hop, e.g. `fatal/bot_kicked` while
/// still pre-meeting).
fn process_change(
    inner: &DriverInner,
    bot_id: BotId,
    metadata: &serde_json::Value,
    change: &client::StatusChange,
) {
    let projection = project_status_change(
        &change.code,
        change.sub_code.as_deref(),
        change.message.as_deref().unwrap_or(""),
    );
    let mut bots = inner.lock_bots();
    let Some(entry) = bots.get_mut(&bot_id) else {
        return;
    };
    // Don't republish over a terminal — `bot_leave` / `bot_terminate`
    // may have already driven us there while a poll was in flight.
    if is_terminal_state(&entry.state) {
        return;
    }
    match projection {
        Projection::Ignore => {
            tracing::debug!(
                ?bot_id, code = %change.code, sub_code = ?change.sub_code,
                "recall status change ignored",
            );
        }
        Projection::Event(ev) => {
            // For `JoinAccepted` we also want to synthesize the
            // `DisclosureAcked` step — the polling driver doesn't
            // host TTS, and the disclosure-text round trip is a
            // separate gap. Without this, the FSM would sit at
            // `Disclosing` forever.
            let mut transitions: Vec<BotEvent> = vec![ev.clone()];
            if matches!(ev, BotEvent::JoinAccepted) {
                transitions.push(BotEvent::DisclosureAcked);
            }
            let mut fsm = fsm_at(&entry.state);
            for transition in transitions {
                match fsm.on_event(transition.clone()) {
                    Ok(_) => {
                        entry.state = fsm.state().clone();
                        let _ = entry.tx.send(BotStateEvent {
                            bot_id,
                            at: Utc::now(),
                            state: entry.state.clone(),
                            metadata: metadata.clone(),
                        });
                    }
                    Err(err) => {
                        tracing::warn!(
                            ?bot_id, code = %change.code, ?err,
                            "FSM rejected projected event; staying put",
                        );
                        break;
                    }
                }
            }
        }
        Projection::Terminal(state) => {
            // Bypass the FSM — Recall collapses transitions the FSM
            // would require multiple events for (e.g. fatal during
            // pre-meeting). Synthesize the terminal directly and
            // publish.
            entry.state = state.clone();
            let _ = entry.tx.send(BotStateEvent {
                bot_id,
                at: Utc::now(),
                state,
                metadata: metadata.clone(),
            });
            // Don't loop again — the polling task's outer loop will
            // see `is_terminal_state` and exit.
        }
    }
}

/// Re-export tests at the module root so wiremock + projection +
/// client tests live under `cargo test -p heron-bot recall::`.
#[cfg(test)]
mod tests;
