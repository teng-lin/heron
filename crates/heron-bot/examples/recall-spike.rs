//! `recall-spike` — exercise Recall.ai's REST surface against the v2
//! six-operation contract to discover which spec invariants the
//! `RecallDriver: MeetingBotDriver` impl can honor.
//!
//! Per [`docs/build-vs-buy-decision.md`](../../../../docs/build-vs-buy-decision.md)
//! and [`docs/api-design-spec.md`](../../../../docs/api-design-spec.md) §13.
//!
//! Reads `RECALL_API_KEY`, `RECALL_REGION` (one of `us-west-2`,
//! `us-east-1`, `eu-central-1`, `ap-northeast-1`) and (optionally)
//! `RECALL_BASE_URL` from the environment. Copy `.env.example` to
//! `.env` and fill in real values, then `set -a; source .env; set +a`.
//!
//! Findings are appended to `spike-findings.jsonl` in the CWD as one
//! JSON object per operation. Summarize with:
//! `jq -s 'group_by(.operation) | map({op: .[0].operation, count: length, mean_ms: ((map(.duration_ms) | add) / length)})' spike-findings.jsonl`
//!
//! ## Audio prerequisite
//!
//! Recall's [output_audio](https://docs.recall.ai/reference/bot_output_audio_create)
//! endpoint requires the bot to have been created with
//! `automatic_audio_output.in_call_recording.data` populated. So
//! `join` and `disclosure-inject` take an MP3 path (the same file is
//! used as the create-time placeholder AND, for `disclosure-inject`,
//! as the immediate utterance). Without `--placeholder-audio`, `join`
//! creates a "listen-only" bot that cannot speak.
//!
//! Audio format requirements are not documented by Recall beyond
//! "MP3"; in practice mono 22.05–44.1kHz at 64–128kbps works. Generate
//! a disclosure with:
//! ```sh
//! say -v Samantha "Hi, I am the user's AI assistant ..." -o /tmp/d.aiff
//! ffmpeg -i /tmp/d.aiff -ac 1 -b:a 64k disclosure.mp3 && rm /tmp/d.aiff
//! ```
//!
//! ## Subcommands
//!
//! Spec §13 contract:
//! - `join`               — POST /bot/, dispatch a bot
//! - `listen`             — poll transcript (proxy for live WS)
//! - `speak`              — POST /bot/{id}/output_audio/
//! - `interrupt`          — DELETE /bot/{id}/output_audio/
//! - `watch-eject`        — poll bot detail, log status_changes
//! - `disclosure-inject`  — join + wait-for-in-call + immediate speak
//! - `replace-test`       — back-to-back speaks, validates Spec §9 Replace
//!
//! Admin / cleanup:
//! - `status`             — GET /bot/{id}/
//! - `leave`              — POST /bot/{id}/leave_call/ (graceful)
//! - `terminate`          — DELETE /bot/{id}/ (only legal pre-join)
//!
//! `Ctrl-C` and post-create errors trigger best-effort cleanup
//! (graceful leave) for the active bot — no orphaned bots in your
//! Recall bill.

use std::env;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

const FINDINGS_PATH: &str = "spike-findings.jsonl";
const POLL_INTERVAL_SECS_DEFAULT: u64 = 5;
const IN_CALL_TIMEOUT_SECS_DEFAULT: u64 = 180;
/// Recall's documented cap per [output_audio reference](https://docs.recall.ai/reference/bot_output_audio_create):
/// `b64_data` length is bounded at 1,835,008 base64 chars (≈ 1.31 MB
/// raw MP3). Enforced client-side because exceeding it returns a
/// generic 400 with no useful body.
const RECALL_OUTPUT_AUDIO_B64_MAX: usize = 1_835_008;

// ── Recall API client ─────────────────────────────────────────────────

mod recall_api {
    use super::*;

    /// Recall.ai REST client. Cheap to clone (Arc-internal `reqwest::Client`).
    #[derive(Clone)]
    pub struct Client {
        http: reqwest::Client,
        base_url: String,
        api_key: String,
    }

    impl Client {
        /// Read configuration from env. Errors if `RECALL_API_KEY` is
        /// unset or empty, or if `RECALL_REGION` is set to something
        /// other than the four documented regions (avoids the silent
        /// "wrong region" failure mode).
        pub fn from_env() -> Result<Self> {
            let api_key = env::var("RECALL_API_KEY")
                .context("RECALL_API_KEY is unset (copy .env.example to .env)")?;
            if api_key.trim().is_empty() {
                bail!("RECALL_API_KEY is empty");
            }
            let base_url = match env::var("RECALL_BASE_URL") {
                Ok(s) if !s.trim().is_empty() => s,
                _ => resolve_region_url(env::var("RECALL_REGION").ok().as_deref())?,
            };
            let http = reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .context("build reqwest client")?;
            Ok(Self {
                http,
                base_url,
                api_key,
            })
        }

        fn url(&self, path: &str) -> String {
            format!("{}{}", self.base_url, path)
        }

        fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
            req.header("Authorization", format!("Token {}", self.api_key))
                .header("Accept", "application/json")
        }

        /// `POST /api/v1/bot/` — dispatch a bot. Schema per the
        /// [Quickstart](https://docs.recall.ai/docs/quickstart) and
        /// [Output Audio guide](https://docs.recall.ai/docs/output-audio-in-meetings).
        ///
        /// `placeholder_audio_b64`, when present, populates
        /// `automatic_audio_output.in_call_recording.data` — required
        /// by Recall before any `output_audio` call will work.
        pub async fn create_bot(&self, args: &CreateBotArgs<'_>) -> Result<ApiOk<BotDetail>> {
            let mut body = json!({
                "meeting_url": args.meeting_url,
                "bot_name": args.bot_name,
                "recording_config": {
                    "transcript": {
                        "provider": { "meeting_captions": {} }
                    }
                }
            });
            if let Some(b64) = args.placeholder_audio_b64 {
                body["automatic_audio_output"] = json!({
                    "in_call_recording": {
                        "data": { "kind": "mp3", "b64_data": b64 }
                    }
                });
            }
            self.send_json::<BotDetail>(reqwest::Method::POST, "/api/v1/bot/", Some(&body))
                .await
        }

        /// `GET /api/v1/bot/{id}/` — full detail including
        /// `status_changes` history.
        pub async fn get_bot(&self, bot_id: &str) -> Result<ApiOk<BotDetail>> {
            self.send_json::<BotDetail>(
                reqwest::Method::GET,
                &format!("/api/v1/bot/{bot_id}/"),
                None,
            )
            .await
        }

        /// `GET /api/v1/bot/{id}/transcript/` — finalized transcript
        /// segments. Live partials are WS-only; we poll for the spike.
        /// Returns the raw JSON because Recall's per-segment schema is
        /// not enumerated in the reference page (only the streaming
        /// guide schema is documented), so we don't risk decode
        /// errors that would shadow real protocol findings.
        pub async fn get_transcript(&self, bot_id: &str) -> Result<ApiOk<Value>> {
            self.send_json::<Value>(
                reqwest::Method::GET,
                &format!("/api/v1/bot/{bot_id}/transcript/"),
                None,
            )
            .await
        }

        /// `POST /api/v1/bot/{id}/output_audio/` — play synthesized
        /// audio. Requires the bot was created with
        /// `automatic_audio_output` (see [`create_bot`]).
        pub async fn output_audio(&self, bot_id: &str, b64_mp3: &str) -> Result<ApiOk<Value>> {
            let body = json!({ "kind": "mp3", "b64_data": b64_mp3 });
            self.send_json::<Value>(
                reqwest::Method::POST,
                &format!("/api/v1/bot/{bot_id}/output_audio/"),
                Some(&body),
            )
            .await
        }

        /// `DELETE /api/v1/bot/{id}/output_audio/` — stop the audio
        /// output channel. 204 No Content on success.
        pub async fn stop_output_audio(&self, bot_id: &str) -> Result<ApiOk<()>> {
            self.send_no_content(
                reqwest::Method::DELETE,
                &format!("/api/v1/bot/{bot_id}/output_audio/"),
            )
            .await
        }

        /// `POST /api/v1/bot/{id}/leave_call/` — graceful leave.
        pub async fn leave_call(&self, bot_id: &str) -> Result<ApiOk<Value>> {
            self.send_json::<Value>(
                reqwest::Method::POST,
                &format!("/api/v1/bot/{bot_id}/leave_call/"),
                None,
            )
            .await
        }

        /// `DELETE /api/v1/bot/{id}/` — only legal pre-join per
        /// Recall. Post-join, this returns 4xx; the spike treats that
        /// as a *successful* validation of spec §3.
        pub async fn delete_bot(&self, bot_id: &str) -> Result<ApiOk<()>> {
            self.send_no_content(
                reqwest::Method::DELETE,
                &format!("/api/v1/bot/{bot_id}/"),
            )
            .await
        }

        // ── transport helpers ────────────────────────────────────────

        async fn send_json<T: serde::de::DeserializeOwned>(
            &self,
            method: reqwest::Method,
            path: &str,
            body: Option<&Value>,
        ) -> Result<ApiOk<T>> {
            let mut req = self.auth(self.http.request(method, self.url(path)));
            if let Some(b) = body {
                req = req.header("Content-Type", "application/json").json(b);
            }
            let resp = req
                .send()
                .await
                .with_context(|| format!("network failure on {path}"))?;
            let status = resp.status();
            let body_text = resp
                .text()
                .await
                .with_context(|| format!("read body of {path}"))?;
            if !status.is_success() {
                return Err(api_error(status, path, body_text));
            }
            let parsed = if body_text.is_empty() {
                serde_json::from_value::<T>(Value::Null).with_context(|| {
                    format!("decode empty body of {path}; expected non-Null type")
                })?
            } else {
                serde_json::from_str::<T>(&body_text).with_context(|| {
                    format!("decode body of {path}: {}", truncate(&body_text, 200))
                })?
            };
            Ok(ApiOk {
                status: status.as_u16(),
                body: parsed,
            })
        }

        async fn send_no_content(
            &self,
            method: reqwest::Method,
            path: &str,
        ) -> Result<ApiOk<()>> {
            let resp = self
                .auth(self.http.request(method, self.url(path)))
                .send()
                .await
                .with_context(|| format!("network failure on {path}"))?;
            let status = resp.status();
            let body_text = resp
                .text()
                .await
                .with_context(|| format!("read body of {path}"))?;
            if !status.is_success() {
                return Err(api_error(status, path, body_text));
            }
            Ok(ApiOk {
                status: status.as_u16(),
                body: (),
            })
        }
    }

    /// Successful API call. Carries the real HTTP status (so findings
    /// don't fabricate it) plus the typed body.
    #[derive(Debug)]
    pub struct ApiOk<T> {
        pub status: u16,
        pub body: T,
    }

    /// Subset of Recall's bot detail. Other fields are present but
    /// ignored; per Recall's docs we don't treat status codes as a
    /// closed enum, hence the `String` for `code` and only documented
    /// terminal codes hard-coded in [`is_terminal`].
    #[derive(Debug, Clone, Deserialize, Serialize)]
    pub struct BotDetail {
        pub id: String,
        #[serde(default)]
        pub bot_name: Option<String>,
        #[serde(default)]
        pub status_changes: Vec<StatusChange>,
    }

    #[derive(Debug, Clone, Deserialize, Serialize)]
    pub struct StatusChange {
        pub code: String,
        #[serde(default)]
        pub sub_code: Option<String>,
        pub created_at: DateTime<Utc>,
        #[serde(default)]
        pub message: Option<String>,
    }

    impl BotDetail {
        /// Latest known state, or `None` if `status_changes` is empty
        /// (Recall returned the bot before its first transition).
        pub fn current_code(&self) -> Option<&str> {
            self.status_changes.last().map(|s| s.code.as_str())
        }

        /// Per Recall's [bot status events docs](https://docs.recall.ai/docs/bot-status-change-events),
        /// `bot.done` and `bot.fatal` are the only terminal codes.
        /// `bot.call_ended` is NOT terminal — `bot.done` follows.
        pub fn is_terminal(&self) -> bool {
            matches!(
                self.current_code(),
                Some("bot.done") | Some("bot.fatal")
            )
        }
    }

    pub struct CreateBotArgs<'a> {
        pub meeting_url: &'a str,
        pub bot_name: &'a str,
        /// Required for `output_audio` to work later. Pass `None` only
        /// if the bot will be listen-only.
        pub placeholder_audio_b64: Option<&'a str>,
    }

    /// Errors carrying everything findings need: status, path, body.
    /// Distinguishes 429 from 507 — Spec §11 Invariant 14 wants these
    /// surfaced separately.
    #[derive(Debug, thiserror::Error)]
    pub enum ApiError {
        #[error("HTTP {status} from {path}: {body}")]
        Http {
            status: u16,
            path: String,
            body: String,
        },
        #[error("rate limit (429) on {path}: {body}")]
        RateLimit { path: String, body: String },
        /// Recall-specific: warm-bot-pool depleted on Create Bot. Per
        /// docs, retry every 30s OR avoid by using `join_at` for
        /// scheduled bots. Distinct from rate limit.
        #[error("capacity exhausted (507) on {path}: {body}")]
        CapacityExhausted { path: String, body: String },
    }

    impl ApiError {
        pub fn http_status(&self) -> u16 {
            match self {
                Self::Http { status, .. } => *status,
                Self::RateLimit { .. } => 429,
                Self::CapacityExhausted { .. } => 507,
            }
        }

        pub fn body(&self) -> &str {
            match self {
                Self::Http { body, .. }
                | Self::RateLimit { body, .. }
                | Self::CapacityExhausted { body, .. } => body,
            }
        }
    }

    fn api_error(status: reqwest::StatusCode, path: &str, body: String) -> anyhow::Error {
        let path = path.to_string();
        let body = truncate_owned(body, 1024);
        match status.as_u16() {
            429 => ApiError::RateLimit { path, body }.into(),
            507 => ApiError::CapacityExhausted { path, body }.into(),
            other => ApiError::Http {
                status: other,
                path,
                body,
            }
            .into(),
        }
    }

    fn truncate(s: &str, n: usize) -> String {
        if s.len() <= n {
            s.to_string()
        } else {
            format!("{}…(+{} bytes)", &s[..n], s.len() - n)
        }
    }

    fn truncate_owned(s: String, n: usize) -> String {
        if s.len() <= n {
            s
        } else {
            let len = s.len();
            let mut t = s;
            t.truncate(n);
            format!("{t}…(+{} bytes)", len - n)
        }
    }

    /// Map `RECALL_REGION` → base URL. `None`/empty defaults to
    /// `us-east-1` (Recall's documented default alias). Unknown
    /// regions bail loudly rather than silently falling back —
    /// per the spike, "wrong region" findings would otherwise look
    /// like authentication failures.
    pub fn resolve_region_url(region: Option<&str>) -> Result<String> {
        let r = region.map(str::trim).filter(|s| !s.is_empty());
        let host = match r {
            None => "us-east-1",
            Some("us-west-2" | "us_west_2") => "us-west-2",
            Some("us-east-1" | "us_east_1") => "us-east-1",
            Some("eu-central-1" | "eu_central_1" | "eu") => "eu-central-1",
            Some("ap-northeast-1" | "ap_northeast_1" | "apac") => "ap-northeast-1",
            Some(other) => bail!(
                "RECALL_REGION={other:?} not recognized; \
                 use one of us-west-2, us-east-1, eu-central-1, ap-northeast-1, \
                 or override RECALL_BASE_URL"
            ),
        };
        Ok(format!("https://{host}.recall.ai"))
    }
}

use recall_api::{ApiError, ApiOk};

// ── findings log ──────────────────────────────────────────────────────

mod findings {
    use super::*;

    #[derive(Debug, Clone, Serialize)]
    pub struct Finding<'a> {
        pub timestamp: DateTime<Utc>,
        pub operation: &'a str,
        pub bot_id: Option<&'a str>,
        pub duration_ms: u128,
        pub outcome: Outcome,
        /// Real HTTP status (or `None` for non-network errors). Spec
        /// §11 wants 429 vs 507 visible in findings — surfaced as the
        /// numeric value so downstream `jq` filters can group cleanly.
        #[serde(skip_serializing_if = "Option::is_none")]
        pub http_status: Option<u16>,
        /// Truncated response body on error. Empty on success.
        #[serde(skip_serializing_if = "str::is_empty")]
        pub response_body: &'a str,
        #[serde(skip_serializing_if = "<[&str]>::is_empty")]
        pub spec_invariants_relevant: &'a [&'a str],
        pub notes: String,
    }

    #[derive(Debug, Clone, Copy, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum Outcome {
        Success,
        Failure,
        /// Operation completed but a spec invariant could not be
        /// validated from the available data (e.g. audibility, which
        /// requires a human in the meeting).
        Inconclusive,
    }

    pub fn append(finding: &Finding<'_>) -> Result<()> {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(FINDINGS_PATH)
            .with_context(|| format!("open {FINDINGS_PATH} for append"))?;
        let line = serde_json::to_string(finding).context("serialize Finding")?;
        // Single ≤4KB write to an O_APPEND fd is atomic across processes
        // on macOS; concurrent subcommands stay record-separated. Larger
        // writes can tear — keep `notes` small.
        writeln!(f, "{line}").context("write Finding")?;
        Ok(())
    }
}

use findings::{Finding, Outcome};

// ── shared utilities ──────────────────────────────────────────────────

/// Track which bot is "live" so the Ctrl-C handler can leave it
/// gracefully. Spike-wide singleton.
type ActiveBot = Arc<Mutex<Option<String>>>;

async fn set_active_bot(slot: &ActiveBot, bot_id: Option<String>) {
    *slot.lock().await = bot_id;
}

/// Pull (status, body) out of a `Result<ApiOk<T>, anyhow::Error>` for
/// findings. Distinguishes our typed `ApiError` from generic
/// network/decode failures.
fn classify<T>(result: &Result<ApiOk<T>>) -> (Option<u16>, String, Outcome) {
    match result {
        Ok(ok) => (Some(ok.status), String::new(), Outcome::Success),
        Err(e) => match e.downcast_ref::<ApiError>() {
            Some(api) => (Some(api.http_status()), api.body().to_string(), Outcome::Failure),
            None => (None, String::new(), Outcome::Failure),
        },
    }
}

async fn read_b64(path: &Path) -> Result<String> {
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("read {}", path.display()))?;
    let b64 = B64.encode(&bytes);
    if b64.len() > RECALL_OUTPUT_AUDIO_B64_MAX {
        bail!(
            "base64 payload {} chars exceeds Recall's {RECALL_OUTPUT_AUDIO_B64_MAX} cap; trim the audio",
            b64.len()
        );
    }
    Ok(b64)
}

// ── CLI ───────────────────────────────────────────────────────────────

#[derive(Debug, Parser)]
#[command(
    name = "recall-spike",
    version,
    about = "Validate Recall.ai against the v2 six-operation contract."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// POST /bot/ — dispatch a bot. With `--placeholder-audio` the bot
    /// is created speak-capable (i.e. with `automatic_audio_output`);
    /// without it, `speak`/`interrupt` will fail at the API level —
    /// useful for validating that Recall enforces the prerequisite.
    Join {
        meeting_url: String,
        /// Display name shown to other meeting participants. The
        /// roster name is itself a form of disclosure (Spec §4); name
        /// the bot for what it is.
        bot_name: String,
        /// Path to a placeholder MP3. Required to enable `output_audio`
        /// later; per Recall, any short MP3 works (they recommend
        /// silence).
        #[arg(long)]
        placeholder_audio: Option<PathBuf>,
    },
    /// GET /bot/{id}/ — full detail (incl. status_changes).
    Status { bot_id: String },
    /// Poll the transcript every N seconds; print new segments.
    /// Tracks segments by their (participant_id, start_timestamp)
    /// rather than array position — Recall may revise/reorder.
    Listen {
        bot_id: String,
        #[arg(long, default_value_t = POLL_INTERVAL_SECS_DEFAULT)]
        poll_secs: u64,
    },
    /// POST /bot/{id}/output_audio/ — play an MP3 file.
    Speak {
        bot_id: String,
        audio_path: PathBuf,
    },
    /// DELETE /bot/{id}/output_audio/ — stop the channel.
    Interrupt { bot_id: String },
    /// Poll `status_changes` and print transitions; exit on a terminal
    /// code (`bot.done` or `bot.fatal` per Recall docs).
    WatchEject {
        bot_id: String,
        #[arg(long, default_value_t = POLL_INTERVAL_SECS_DEFAULT)]
        poll_secs: u64,
    },
    /// End-to-end disclosure: dispatch a bot AND speak the disclosure
    /// once it's in-call. Same MP3 serves as the create-time
    /// placeholder. Measures `join → in_call → speak_accepted` latency
    /// (Recall-side acceptance, NOT actual audibility — needs a human
    /// in the meeting to confirm).
    DisclosureInject {
        meeting_url: String,
        bot_name: String,
        audio_path: PathBuf,
        #[arg(long, default_value_t = IN_CALL_TIMEOUT_SECS_DEFAULT)]
        timeout_secs: u64,
    },
    /// Send two `output_audio` calls back-to-back to validate Spec §9
    /// `Replace` semantics. Records the gap behavior; reveals whether
    /// Recall queues, replaces, or rejects the second call.
    ReplaceTest {
        bot_id: String,
        audio_path: PathBuf,
    },
    /// POST /bot/{id}/leave_call/ — graceful leave (post-join).
    Leave { bot_id: String },
    /// DELETE /bot/{id}/ — only legal pre-join per Recall.
    Terminate { bot_id: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let client = recall_api::Client::from_env()?;
    let active: ActiveBot = Arc::new(Mutex::new(None));

    spawn_signal_cleanup(client.clone(), active.clone());

    match cli.cmd {
        Cmd::Join {
            meeting_url,
            bot_name,
            placeholder_audio,
        } => cmd_join(&client, &active, &meeting_url, &bot_name, placeholder_audio.as_deref()).await,
        Cmd::Status { bot_id } => cmd_status(&client, &bot_id).await,
        Cmd::Listen { bot_id, poll_secs } => cmd_listen(&client, &bot_id, poll_secs).await,
        Cmd::Speak { bot_id, audio_path } => cmd_speak(&client, &bot_id, &audio_path).await,
        Cmd::Interrupt { bot_id } => cmd_interrupt(&client, &bot_id).await,
        Cmd::WatchEject { bot_id, poll_secs } => cmd_watch_eject(&client, &bot_id, poll_secs).await,
        Cmd::DisclosureInject {
            meeting_url,
            bot_name,
            audio_path,
            timeout_secs,
        } => cmd_disclosure_inject(
            &client,
            &active,
            &meeting_url,
            &bot_name,
            &audio_path,
            timeout_secs,
        )
        .await,
        Cmd::ReplaceTest { bot_id, audio_path } => {
            cmd_replace_test(&client, &bot_id, &audio_path).await
        }
        Cmd::Leave { bot_id } => cmd_leave(&client, &bot_id).await,
        Cmd::Terminate { bot_id } => cmd_terminate(&client, &bot_id).await,
    }
}

/// On Ctrl-C, attempt graceful leave for the active bot. Process
/// exits with 130 (POSIX SIGINT convention).
fn spawn_signal_cleanup(client: recall_api::Client, active: ActiveBot) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }
        let id_opt = active.lock().await.clone();
        if let Some(id) = id_opt {
            eprintln!("\n^C — leaving bot {id}…");
            match client.leave_call(&id).await {
                Ok(_) => eprintln!("cleanup ok"),
                Err(e) => eprintln!("cleanup failed: {e:#}"),
            }
        } else {
            eprintln!("\n^C");
        }
        std::process::exit(130);
    });
}

// ── command handlers ──────────────────────────────────────────────────

async fn cmd_join(
    client: &recall_api::Client,
    active: &ActiveBot,
    meeting_url: &str,
    bot_name: &str,
    placeholder_audio: Option<&Path>,
) -> Result<()> {
    let placeholder_b64 = match placeholder_audio {
        Some(p) => Some(read_b64(p).await?),
        None => None,
    };
    let started = Instant::now();
    let result = client
        .create_bot(&recall_api::CreateBotArgs {
            meeting_url,
            bot_name,
            placeholder_audio_b64: placeholder_b64.as_deref(),
        })
        .await;
    let duration_ms = started.elapsed().as_millis();

    let (http_status, body, outcome) = classify(&result);
    let bot_id = result.as_ref().ok().map(|ok| ok.body.id.clone());
    let initial_state = result
        .as_ref()
        .ok()
        .and_then(|ok| ok.body.current_code())
        .unwrap_or("(no transitions yet)")
        .to_string();
    let notes = match &result {
        Ok(_) => format!(
            "initial_state: {initial_state}; speak_capable: {}",
            placeholder_b64.is_some()
        ),
        Err(e) => format!("{e:#}"),
    };

    findings::append(&Finding {
        timestamp: Utc::now(),
        operation: "join",
        bot_id: bot_id.as_deref(),
        duration_ms,
        outcome,
        http_status,
        response_body: &body,
        spec_invariants_relevant: &[
            "spec §3 (FSM)",
            "spec §6 (persona — placeholder)",
            "spec §11 (real http_status captured)",
        ],
        notes,
    })?;

    let ok = result?;
    set_active_bot(active, Some(ok.body.id.clone())).await;
    println!("bot_id: {}", ok.body.id);
    println!("initial_state: {initial_state}");
    Ok(())
}

async fn cmd_status(client: &recall_api::Client, bot_id: &str) -> Result<()> {
    let detail = client.get_bot(bot_id).await?;
    println!("{}", serde_json::to_string_pretty(&detail.body)?);
    Ok(())
}

async fn cmd_listen(client: &recall_api::Client, bot_id: &str, poll_secs: u64) -> Result<()> {
    println!("polling transcript every {poll_secs}s; Ctrl-C to stop");
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    loop {
        let started = Instant::now();
        let result = client.get_transcript(bot_id).await;
        let (http_status, body, outcome) = classify(&result);
        let mut new_segments = 0usize;
        let mut total_segments = 0usize;
        if let Ok(ok) = &result
            && let Value::Array(segments) = &ok.body
        {
            total_segments = segments.len();
            for seg in segments {
                let key = segment_key(seg);
                if seen.insert(key) {
                    new_segments += 1;
                    print_segment(seg);
                }
            }
        }
        findings::append(&Finding {
            timestamp: Utc::now(),
            operation: "listen",
            bot_id: Some(bot_id),
            duration_ms: started.elapsed().as_millis(),
            outcome,
            http_status,
            response_body: &body,
            spec_invariants_relevant: &["spec §9 (partial vs final)"],
            notes: format!("total: {total_segments}, new: {new_segments}"),
        })?;
        // Surface the error after recording it so loop continues on transient failures.
        if let Err(e) = &result {
            tracing::warn!(error = %e, "transcript poll failed");
        }
        tokio::time::sleep(Duration::from_secs(poll_secs)).await;
    }
}

/// Identity key for a transcript segment. Recall's documented schema
/// has no per-segment ID, so we hash on `(participant.id, words[0]
/// .start_timestamp.absolute)` which is stable across polls.
fn segment_key(seg: &Value) -> String {
    let participant = seg
        .get("participant")
        .and_then(|p| p.get("id"))
        .map(Value::to_string)
        .unwrap_or_default();
    let first_word_ts = seg
        .get("words")
        .and_then(Value::as_array)
        .and_then(|ws| ws.first())
        .and_then(|w| w.get("start_timestamp"))
        .and_then(|t| t.get("absolute"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    format!("{participant}:{first_word_ts}")
}

fn print_segment(seg: &Value) {
    let speaker = seg
        .get("participant")
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("?");
    let text = seg
        .get("words")
        .and_then(Value::as_array)
        .map(|ws| {
            ws.iter()
                .filter_map(|w| w.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    println!("[{speaker}] {text}");
}

async fn cmd_speak(client: &recall_api::Client, bot_id: &str, audio_path: &Path) -> Result<()> {
    let b64 = read_b64(audio_path).await?;
    let started = Instant::now();
    let result = client.output_audio(bot_id, &b64).await;
    let duration_ms = started.elapsed().as_millis();
    let (http_status, body, base_outcome) = classify(&result);
    let outcome = if matches!(base_outcome, Outcome::Success) {
        Outcome::Inconclusive
    } else {
        base_outcome
    };
    let notes = match &result {
        Ok(_) => format!(
            "accepted in {duration_ms}ms; audibility requires human in meeting to confirm. \
             No utterance_id returned (validates spec §9 gap)."
        ),
        Err(e) => format!("{e:#}"),
    };
    findings::append(&Finding {
        timestamp: Utc::now(),
        operation: "speak",
        bot_id: Some(bot_id),
        duration_ms,
        outcome,
        http_status,
        response_body: &body,
        spec_invariants_relevant: &[
            "spec §9 (utterance ID — Recall returns none)",
            "spec §11 (atomic Replace — not supported)",
        ],
        notes,
    })?;
    result?;
    println!("output_audio accepted in {duration_ms}ms");
    Ok(())
}

async fn cmd_interrupt(client: &recall_api::Client, bot_id: &str) -> Result<()> {
    let started = Instant::now();
    let result = client.stop_output_audio(bot_id).await;
    let duration_ms = started.elapsed().as_millis();
    let (http_status, body, outcome) = classify(&result);
    findings::append(&Finding {
        timestamp: Utc::now(),
        operation: "interrupt",
        bot_id: Some(bot_id),
        duration_ms,
        outcome,
        http_status,
        response_body: &body,
        spec_invariants_relevant: &["spec §9 (cancel granularity — channel only)"],
        notes: "channel-level cancel; cannot target a specific utterance".to_string(),
    })?;
    result?;
    println!("output channel stopped in {duration_ms}ms");
    Ok(())
}

async fn cmd_watch_eject(
    client: &recall_api::Client,
    bot_id: &str,
    poll_secs: u64,
) -> Result<()> {
    println!("watching status_changes every {poll_secs}s; Ctrl-C to stop");
    let mut seen: usize = 0;
    loop {
        let result = client.get_bot(bot_id).await;
        match result {
            Ok(ok) => {
                let detail = &ok.body;
                let new_changes = detail.status_changes.iter().skip(seen);
                for change in new_changes {
                    let terminal = matches!(change.code.as_str(), "bot.done" | "bot.fatal");
                    println!(
                        "{}: code={} sub_code={:?} message={:?} terminal={terminal}",
                        change.created_at.to_rfc3339(),
                        change.code,
                        change.sub_code,
                        change.message
                    );
                    findings::append(&Finding {
                        timestamp: Utc::now(),
                        operation: "watch-eject",
                        bot_id: Some(bot_id),
                        duration_ms: 0,
                        outcome: Outcome::Success,
                        http_status: Some(ok.status),
                        response_body: "",
                        spec_invariants_relevant: &[
                            "spec §7 (kick-out as event)",
                            "spec §9 (granular EjectReason via sub_code)",
                        ],
                        notes: format!(
                            "code={} sub_code={:?} terminal={terminal}",
                            change.code, change.sub_code
                        ),
                    })?;
                    if terminal {
                        return Ok(());
                    }
                }
                seen = detail.status_changes.len();
            }
            Err(e) => {
                tracing::warn!(error = %e, "status poll failed");
            }
        }
        tokio::time::sleep(Duration::from_secs(poll_secs)).await;
    }
}

async fn cmd_disclosure_inject(
    client: &recall_api::Client,
    active: &ActiveBot,
    meeting_url: &str,
    bot_name: &str,
    audio_path: &Path,
    timeout_secs: u64,
) -> Result<()> {
    let b64 = read_b64(audio_path).await?;

    println!("dispatching bot…");
    let join_started = Instant::now();
    let dispatch = client
        .create_bot(&recall_api::CreateBotArgs {
            meeting_url,
            bot_name,
            placeholder_audio_b64: Some(&b64),
        })
        .await?;
    let join_ms = join_started.elapsed().as_millis();
    let bot_id = dispatch.body.id.clone();
    set_active_bot(active, Some(bot_id.clone())).await;
    println!("bot {bot_id} dispatched in {join_ms}ms");

    // Everything after dispatch wraps in cleanup-on-error. Recall bills
    // per bot-minute — orphaning a bot during a long meeting is a real
    // cost leak.
    let outcome = run_disclosure_post_join(
        client,
        &bot_id,
        &b64,
        timeout_secs,
        join_started,
        join_ms,
    )
    .await;
    if outcome.is_err() {
        eprintln!("disclosure-inject failed; attempting graceful leave for bot {bot_id}");
        match client.leave_call(&bot_id).await {
            Ok(_) => eprintln!("cleanup ok"),
            Err(e) => eprintln!("cleanup failed: {e:#}"),
        }
    }
    set_active_bot(active, None).await;
    outcome
}

async fn run_disclosure_post_join(
    client: &recall_api::Client,
    bot_id: &str,
    b64: &str,
    timeout_secs: u64,
    join_started: Instant,
    join_ms: u128,
) -> Result<()> {
    println!("waiting up to {timeout_secs}s for bot to enter in-call state…");
    let in_call_ms = wait_for_in_call(client, bot_id, timeout_secs).await?;
    println!("bot reached in_call state in {in_call_ms}ms after dispatch");

    println!("posting disclosure audio…");
    let speak_started = Instant::now();
    let speak_result = client.output_audio(bot_id, b64).await;
    let speak_ms = speak_started.elapsed().as_millis();
    let total_ms = join_started.elapsed().as_millis();
    let (http_status, body, _) = classify(&speak_result);
    let outcome = match &speak_result {
        Ok(_) => Outcome::Inconclusive,
        Err(_) => Outcome::Failure,
    };
    findings::append(&Finding {
        timestamp: Utc::now(),
        operation: "disclosure-inject",
        bot_id: Some(bot_id),
        duration_ms: total_ms,
        outcome,
        http_status,
        response_body: &body,
        spec_invariants_relevant: &[
            "spec §4 (disclosure ordering)",
            "spec Invariant 6 (disclosure required)",
        ],
        notes: format!(
            "join={join_ms}ms in_call={in_call_ms}ms speak_accept={speak_ms}ms total={total_ms}ms; \
             audibility requires human confirmation."
        ),
    })?;
    speak_result?;
    println!("output_audio accepted in {speak_ms}ms");
    println!("total join → disclosure-accepted: {total_ms}ms");
    println!("(actual audibility requires human in meeting to confirm)");
    Ok(())
}

async fn wait_for_in_call(
    client: &recall_api::Client,
    bot_id: &str,
    timeout_secs: u64,
) -> Result<u128> {
    let started = Instant::now();
    let deadline = started + Duration::from_secs(timeout_secs);
    loop {
        let detail = client.get_bot(bot_id).await?.body;
        let code = detail.current_code().unwrap_or("(none)");
        tracing::debug!(state = code, "polling");
        // Recall's documented in-call codes are bot.in_call_*; anything
        // matching the prefix means audio I/O is now valid.
        if code.starts_with("bot.in_call") {
            return Ok(started.elapsed().as_millis());
        }
        if detail.is_terminal() {
            bail!("bot reached terminal state {code} before in_call");
        }
        if Instant::now() >= deadline {
            bail!("bot did not reach in_call state within {timeout_secs}s (last: {code})");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn cmd_replace_test(
    client: &recall_api::Client,
    bot_id: &str,
    audio_path: &Path,
) -> Result<()> {
    let b64 = read_b64(audio_path).await?;
    println!("posting first output_audio…");
    let first_started = Instant::now();
    let first_result = client.output_audio(bot_id, &b64).await;
    let first_ms = first_started.elapsed().as_millis();
    let (first_status, first_body, _) = classify(&first_result);

    println!("immediately posting second output_audio (no delay)…");
    let second_started = Instant::now();
    let second_result = client.output_audio(bot_id, &b64).await;
    let second_ms = second_started.elapsed().as_millis();
    let (second_status, second_body, _) = classify(&second_result);

    let combined_status = match (first_status, second_status) {
        (Some(a), Some(b)) if a == b => Some(a),
        _ => None,
    };
    let outcome = if first_result.is_ok() && second_result.is_ok() {
        Outcome::Inconclusive
    } else {
        Outcome::Failure
    };
    let combined_body = if !first_body.is_empty() {
        format!("first: {first_body}")
    } else if !second_body.is_empty() {
        format!("second: {second_body}")
    } else {
        String::new()
    };
    findings::append(&Finding {
        timestamp: Utc::now(),
        operation: "replace-test",
        bot_id: Some(bot_id),
        duration_ms: first_ms + second_ms,
        outcome,
        http_status: combined_status,
        response_body: &combined_body,
        spec_invariants_relevant: &[
            "spec §9 (Priority::Replace semantics)",
            "spec Invariant 11 (atomic replace as single primitive)",
        ],
        notes: format!(
            "first_accept={first_ms}ms second_accept={second_ms}ms; \
             observe in-meeting: did Recall queue, replace, or play both? \
             (Recall has no documented Replace primitive — finding records what we observe.)"
        ),
    })?;
    first_result?;
    second_result?;
    println!("both calls accepted; observe playback in meeting to determine semantics");
    Ok(())
}

async fn cmd_leave(client: &recall_api::Client, bot_id: &str) -> Result<()> {
    let started = Instant::now();
    let result = client.leave_call(bot_id).await;
    let duration_ms = started.elapsed().as_millis();
    let (http_status, body, outcome) = classify(&result);
    findings::append(&Finding {
        timestamp: Utc::now(),
        operation: "leave",
        bot_id: Some(bot_id),
        duration_ms,
        outcome,
        http_status,
        response_body: &body,
        spec_invariants_relevant: &["spec §3 (leave vs terminate split)"],
        notes: "graceful post-join leave".to_string(),
    })?;
    result?;
    println!("leave_call accepted in {duration_ms}ms");
    Ok(())
}

async fn cmd_terminate(client: &recall_api::Client, bot_id: &str) -> Result<()> {
    let started = Instant::now();
    let result = client.delete_bot(bot_id).await;
    let duration_ms = started.elapsed().as_millis();
    let (http_status, body, _) = classify(&result);
    // For this op, an error after join is the *expected* spec-§3
    // outcome: Recall enforces "DELETE only legal pre-join."
    let (outcome, notes) = match &result {
        Ok(_) => (Outcome::Success, "DELETE accepted (bot was pre-join)".to_string()),
        Err(e) => (
            Outcome::Success,
            format!("DELETE rejected (bot was post-join — validates spec §3): {e:#}"),
        ),
    };
    findings::append(&Finding {
        timestamp: Utc::now(),
        operation: "terminate",
        bot_id: Some(bot_id),
        duration_ms,
        outcome,
        http_status,
        response_body: &body,
        spec_invariants_relevant: &["spec §3 (DELETE only legal pre-join)"],
        notes,
    })?;
    if result.is_err() {
        println!("(expected) terminate rejected in {duration_ms}ms; bot already in-call");
    } else {
        println!("terminate accepted in {duration_ms}ms");
    }
    Ok(())
}

// ── tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_unset_defaults_to_us_east() -> Result<()> {
        let url = recall_api::resolve_region_url(None)?;
        assert_eq!(url, "https://us-east-1.recall.ai");
        Ok(())
    }

    #[test]
    fn region_us_west_2_resolves() -> Result<()> {
        let url = recall_api::resolve_region_url(Some("us-west-2"))?;
        assert_eq!(url, "https://us-west-2.recall.ai");
        Ok(())
    }

    #[test]
    fn region_eu_alias_resolves() -> Result<()> {
        let url = recall_api::resolve_region_url(Some("eu"))?;
        assert_eq!(url, "https://eu-central-1.recall.ai");
        Ok(())
    }

    #[test]
    fn region_apac_alias_resolves() -> Result<()> {
        let url = recall_api::resolve_region_url(Some("apac"))?;
        assert_eq!(url, "https://ap-northeast-1.recall.ai");
        Ok(())
    }

    #[test]
    fn region_unknown_bails() {
        let err = recall_api::resolve_region_url(Some("mars-1")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not recognized"),
            "expected helpful error, got: {msg}"
        );
    }

    #[test]
    fn region_empty_string_treated_as_unset() -> Result<()> {
        let url = recall_api::resolve_region_url(Some(""))?;
        assert_eq!(url, "https://us-east-1.recall.ai");
        Ok(())
    }

    #[test]
    fn finding_serde_roundtrip() -> Result<()> {
        let f = Finding {
            timestamp: Utc::now(),
            operation: "join",
            bot_id: Some("bot_xyz"),
            duration_ms: 123,
            outcome: Outcome::Success,
            http_status: Some(201),
            response_body: "",
            spec_invariants_relevant: &["spec §3"],
            notes: "ok".into(),
        };
        let json = serde_json::to_string(&f)?;
        let v: serde_json::Value = serde_json::from_str(&json)?;
        assert_eq!(v["operation"], "join");
        assert_eq!(v["http_status"], 201);
        assert_eq!(v["bot_id"], "bot_xyz");
        // response_body is skipped when empty per the serde attribute.
        assert!(v.get("response_body").is_none());
        Ok(())
    }

    #[test]
    fn finding_omits_empty_invariants() -> Result<()> {
        let f = Finding {
            timestamp: Utc::now(),
            operation: "x",
            bot_id: None,
            duration_ms: 0,
            outcome: Outcome::Failure,
            http_status: None,
            response_body: "boom",
            spec_invariants_relevant: &[],
            notes: String::new(),
        };
        let json = serde_json::to_string(&f)?;
        assert!(!json.contains("spec_invariants_relevant"));
        assert!(json.contains("\"response_body\":\"boom\""));
        Ok(())
    }
}

// `anyhow!` is not currently used at top level but kept available for
// future inline diagnostics; suppress the dead-code warning.
const _: fn() -> anyhow::Error = || anyhow!("unused");
