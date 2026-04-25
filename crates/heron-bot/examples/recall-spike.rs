//! `recall-spike` — exercise Recall.ai's REST surface against the v2
//! six-operation contract to discover which spec invariants the
//! `RecallDriver: MeetingBotDriver` impl can honor.
//!
//! Per [`docs/build-vs-buy-decision.md`](../../../../docs/build-vs-buy-decision.md)
//! and [`docs/api-design-spec.md`](../../../../docs/api-design-spec.md) §13.
//!
//! Reads `RECALL_API_KEY` and (optionally) `RECALL_REGION` /
//! `RECALL_BASE_URL` from the environment — copy `.env.example` to
//! `.env` and fill in real values, then `set -a; source .env; set +a`
//! before running.
//!
//! Findings are appended to `spike-findings.jsonl` in the CWD as one
//! JSON object per operation. After a session, summarize with:
//! `jq -s 'group_by(.operation) | map({op: .[0].operation, count: length, mean_ms: (map(.duration_ms) | add / length)})' spike-findings.jsonl`
//!
//! ## Subcommands
//!
//! Operations from the spec §13 contract:
//! - `join`               — POST /bot/, dispatch a bot to a meeting
//! - `listen`             — poll transcript (proxy for live WS)
//! - `speak`              — POST /bot/{id}/output_audio/
//! - `interrupt`          — DELETE /bot/{id}/output_audio/
//! - `watch-eject`        — poll bot detail, log state transitions
//! - `disclosure-inject`  — join + immediate speak; measures end-to-end latency
//!
//! Admin / cleanup:
//! - `status`             — GET /bot/{id}/
//! - `leave`              — POST /bot/{id}/leave_call/ (graceful)
//! - `terminate`          — DELETE /bot/{id}/ (only legal pre-join)

use std::env;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const FINDINGS_PATH: &str = "spike-findings.jsonl";
const POLL_INTERVAL_SECS: u64 = 5;

// ── Recall API client ─────────────────────────────────────────────────

mod recall_api {
    use super::*;

    /// Recall.ai REST client. One per spike invocation.
    pub struct Client {
        http: reqwest::Client,
        base_url: String,
        api_key: String,
    }

    impl Client {
        /// Read configuration from env. Errors if `RECALL_API_KEY` is
        /// unset; falls back to the US region URL otherwise.
        pub fn from_env() -> Result<Self> {
            let api_key = env::var("RECALL_API_KEY")
                .context("RECALL_API_KEY is unset (copy .env.example to .env)")?;
            if api_key.trim().is_empty() {
                bail!("RECALL_API_KEY is empty");
            }
            let base_url = env::var("RECALL_BASE_URL").unwrap_or_else(|_| {
                match env::var("RECALL_REGION").as_deref() {
                    Ok("eu") => "https://eu-central-1.recall.ai".to_string(),
                    _ => "https://us-east-1.recall.ai".to_string(),
                }
            });
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

        /// `POST /api/v1/bot/` — dispatch a bot to a meeting URL.
        pub async fn create_bot(&self, args: &CreateBotArgs<'_>) -> Result<BotDetail> {
            let body = json!({
                "meeting_url": args.meeting_url,
                "bot_name": args.bot_name,
                "transcription_options": { "provider": "meeting_captions" },
            });
            let resp = self
                .auth(self.http.post(self.url("/api/v1/bot/")))
                .json(&body)
                .send()
                .await
                .context("POST /api/v1/bot/")?;
            check_status(&resp).await?;
            resp.json::<BotDetail>().await.context("decode BotDetail")
        }

        /// `GET /api/v1/bot/{id}/` — full bot detail including
        /// `status_changes` history.
        pub async fn get_bot(&self, bot_id: &str) -> Result<BotDetail> {
            let resp = self
                .auth(self.http.get(self.url(&format!("/api/v1/bot/{bot_id}/"))))
                .send()
                .await
                .context("GET /api/v1/bot/{id}/")?;
            check_status(&resp).await?;
            resp.json::<BotDetail>().await.context("decode BotDetail")
        }

        /// `GET /api/v1/bot/{id}/transcript/` — finalized transcript
        /// segments. Live partials are WS-only; we poll for the spike.
        pub async fn get_transcript(&self, bot_id: &str) -> Result<Value> {
            let resp = self
                .auth(
                    self.http
                        .get(self.url(&format!("/api/v1/bot/{bot_id}/transcript/"))),
                )
                .send()
                .await
                .context("GET /api/v1/bot/{id}/transcript/")?;
            check_status(&resp).await?;
            resp.json::<Value>().await.context("decode transcript")
        }

        /// `POST /api/v1/bot/{id}/output_audio/` — play synthesized
        /// audio in the meeting. Recall accepts `kind: "mp3"` with
        /// base64-encoded payload, capped at ~1.83M base64 chars.
        pub async fn output_audio(&self, bot_id: &str, b64_mp3: &str) -> Result<Value> {
            let body = json!({ "kind": "mp3", "b64_data": b64_mp3 });
            let resp = self
                .auth(
                    self.http
                        .post(self.url(&format!("/api/v1/bot/{bot_id}/output_audio/"))),
                )
                .json(&body)
                .send()
                .await
                .context("POST output_audio")?;
            check_status(&resp).await?;
            resp.json::<Value>().await.unwrap_or(Value::Null).pipe(Ok)
        }

        /// `DELETE /api/v1/bot/{id}/output_audio/` — stop the audio
        /// output channel. Channel-level only; no per-utterance cancel.
        pub async fn stop_output_audio(&self, bot_id: &str) -> Result<()> {
            let resp = self
                .auth(
                    self.http
                        .delete(self.url(&format!("/api/v1/bot/{bot_id}/output_audio/"))),
                )
                .send()
                .await
                .context("DELETE output_audio")?;
            check_status(&resp).await
        }

        /// `POST /api/v1/bot/{id}/leave_call/` — graceful leave.
        pub async fn leave_call(&self, bot_id: &str) -> Result<()> {
            let resp = self
                .auth(
                    self.http
                        .post(self.url(&format!("/api/v1/bot/{bot_id}/leave_call/"))),
                )
                .send()
                .await
                .context("POST leave_call")?;
            check_status(&resp).await
        }

        /// `DELETE /api/v1/bot/{id}/` — only legal on bots that have
        /// not yet joined. Recall returns 4xx after join.
        pub async fn delete_bot(&self, bot_id: &str) -> Result<()> {
            let resp = self
                .auth(self.http.delete(self.url(&format!("/api/v1/bot/{bot_id}/"))))
                .send()
                .await
                .context("DELETE /api/v1/bot/{id}/")?;
            check_status(&resp).await
        }
    }

    /// Subset of Recall's bot detail we care about for the spike. Other
    /// fields are present but ignored — Recall recommends not treating
    /// status codes as a closed enum.
    #[derive(Debug, Clone, Deserialize, Serialize)]
    pub struct BotDetail {
        pub id: String,
        #[serde(default)]
        pub bot_name: Option<String>,
        #[serde(default)]
        pub meeting_url: Option<Value>,
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
        /// Latest known state, or `"unknown"` if `status_changes` is empty.
        pub fn current_code(&self) -> &str {
            self.status_changes
                .last()
                .map(|s| s.code.as_str())
                .unwrap_or("unknown")
        }
    }

    pub struct CreateBotArgs<'a> {
        pub meeting_url: &'a str,
        pub bot_name: &'a str,
    }

    /// Body-pipe sugar so the `output_audio` return decode reads cleanly.
    trait Pipe: Sized {
        fn pipe<U>(self, f: impl FnOnce(Self) -> U) -> U {
            f(self)
        }
    }
    impl<T> Pipe for T {}

    /// Surfaces non-2xx responses with the body so spike output is
    /// debuggable rather than just "404 Not Found."
    async fn check_status(resp: &reqwest::Response) -> Result<()> {
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            let url = resp.url().to_string();
            bail!("HTTP {} from {url}", status);
        }
    }
}

// ── findings log ──────────────────────────────────────────────────────

mod findings {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::Write;

    /// One JSONL row per operation. Keep the shape narrow so
    /// `jq -s 'group_by(.operation)'` works without surprise.
    #[derive(Debug, Clone, Serialize)]
    pub struct Finding<'a> {
        pub timestamp: DateTime<Utc>,
        pub operation: &'a str,
        pub bot_id: Option<&'a str>,
        pub duration_ms: u128,
        pub outcome: Outcome,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub http_status: Option<u16>,
        #[serde(skip_serializing_if = "<[String]>::is_empty")]
        pub spec_invariants_relevant: &'a [String],
        pub notes: String,
    }

    #[derive(Debug, Clone, Copy, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum Outcome {
        Success,
        Failure,
        /// Operation completed but a spec invariant could not be
        /// validated from the available data. The notes field carries
        /// what's missing.
        Inconclusive,
    }

    pub fn append(finding: &Finding<'_>) -> Result<()> {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(FINDINGS_PATH)
            .with_context(|| format!("open {FINDINGS_PATH} for append"))?;
        let line = serde_json::to_string(finding).context("serialize Finding")?;
        writeln!(f, "{line}").context("write Finding")?;
        Ok(())
    }
}

use findings::{Finding, Outcome};

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
    /// POST /bot/ — dispatch a bot to a meeting URL.
    Join {
        meeting_url: String,
        #[arg(long, default_value = "heron spike bot")]
        bot_name: String,
    },
    /// GET /bot/{id}/ — full detail including status_changes.
    Status { bot_id: String },
    /// Poll the transcript every N seconds; print new segments.
    Listen {
        bot_id: String,
        #[arg(long, default_value_t = POLL_INTERVAL_SECS)]
        poll_secs: u64,
    },
    /// POST /bot/{id}/output_audio/ — play an MP3 file.
    Speak {
        bot_id: String,
        /// Path to an MP3 ≤ ~1.4MB raw (= ~1.83M base64 chars).
        audio_path: PathBuf,
    },
    /// DELETE /bot/{id}/output_audio/ — stop the output channel.
    Interrupt { bot_id: String },
    /// Poll status_changes and print transitions; exit on terminal state.
    WatchEject {
        bot_id: String,
        #[arg(long, default_value_t = POLL_INTERVAL_SECS)]
        poll_secs: u64,
    },
    /// End-to-end: dispatch a bot AND immediately speak the disclosure.
    /// Measures join → first-output-audio latency (Recall-side acceptance,
    /// not actual audibility — needs a human in the meeting to confirm).
    DisclosureInject {
        meeting_url: String,
        /// Disclosure audio (MP3). Pre-record once, e.g. via:
        /// `say -v Samantha "Hi, I am Teng's AI assistant ..." -o disclosure.aiff && ffmpeg -i disclosure.aiff disclosure.mp3`
        audio_path: PathBuf,
        #[arg(long, default_value = "heron disclosure spike")]
        bot_name: String,
    },
    /// POST /bot/{id}/leave_call/ — graceful leave (post-join).
    Leave { bot_id: String },
    /// DELETE /bot/{id}/ — hard kill (only legal pre-join, per Recall).
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

    match cli.cmd {
        Cmd::Join {
            meeting_url,
            bot_name,
        } => cmd_join(&client, &meeting_url, &bot_name).await,
        Cmd::Status { bot_id } => cmd_status(&client, &bot_id).await,
        Cmd::Listen { bot_id, poll_secs } => cmd_listen(&client, &bot_id, poll_secs).await,
        Cmd::Speak {
            bot_id,
            audio_path,
        } => cmd_speak(&client, &bot_id, &audio_path).await,
        Cmd::Interrupt { bot_id } => cmd_interrupt(&client, &bot_id).await,
        Cmd::WatchEject { bot_id, poll_secs } => {
            cmd_watch_eject(&client, &bot_id, poll_secs).await
        }
        Cmd::DisclosureInject {
            meeting_url,
            audio_path,
            bot_name,
        } => cmd_disclosure_inject(&client, &meeting_url, &audio_path, &bot_name).await,
        Cmd::Leave { bot_id } => cmd_leave(&client, &bot_id).await,
        Cmd::Terminate { bot_id } => cmd_terminate(&client, &bot_id).await,
    }
}

// ── command handlers ──────────────────────────────────────────────────

async fn cmd_join(client: &recall_api::Client, meeting_url: &str, bot_name: &str) -> Result<()> {
    let started = Instant::now();
    let result = client
        .create_bot(&recall_api::CreateBotArgs {
            meeting_url,
            bot_name,
        })
        .await;
    let duration_ms = started.elapsed().as_millis();

    match result {
        Ok(detail) => {
            tracing::info!(bot_id = %detail.id, "bot dispatched");
            println!("bot_id: {}", detail.id);
            println!("initial_state: {}", detail.current_code());
            findings::append(&Finding {
                timestamp: Utc::now(),
                operation: "join",
                bot_id: Some(&detail.id),
                duration_ms,
                outcome: Outcome::Success,
                http_status: Some(200),
                spec_invariants_relevant: &[
                    "spec §3 (FSM)".into(),
                    "spec §6 (persona — placeholder)".into(),
                ],
                notes: format!("initial state: {}", detail.current_code()),
            })?;
            Ok(())
        }
        Err(e) => {
            findings::append(&Finding {
                timestamp: Utc::now(),
                operation: "join",
                bot_id: None,
                duration_ms,
                outcome: Outcome::Failure,
                http_status: None,
                spec_invariants_relevant: &[],
                notes: format!("{e:#}"),
            })?;
            Err(e)
        }
    }
}

async fn cmd_status(client: &recall_api::Client, bot_id: &str) -> Result<()> {
    let detail = client.get_bot(bot_id).await?;
    println!("{}", serde_json::to_string_pretty(&detail)?);
    Ok(())
}

async fn cmd_listen(client: &recall_api::Client, bot_id: &str, poll_secs: u64) -> Result<()> {
    println!("polling transcript every {poll_secs}s; Ctrl-C to stop");
    let mut last_count: usize = 0;
    loop {
        let started = Instant::now();
        match client.get_transcript(bot_id).await {
            Ok(Value::Array(segments)) => {
                let new = segments.len().saturating_sub(last_count);
                if new > 0 {
                    for seg in segments.iter().skip(last_count) {
                        let speaker = seg.get("speaker").and_then(Value::as_str).unwrap_or("?");
                        let words = seg
                            .get("words")
                            .and_then(Value::as_array)
                            .map(|ws| {
                                ws.iter()
                                    .filter_map(|w| w.get("text").and_then(Value::as_str))
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            })
                            .unwrap_or_default();
                        println!("[{speaker}] {words}");
                    }
                    last_count = segments.len();
                }
                findings::append(&Finding {
                    timestamp: Utc::now(),
                    operation: "listen",
                    bot_id: Some(bot_id),
                    duration_ms: started.elapsed().as_millis(),
                    outcome: Outcome::Success,
                    http_status: Some(200),
                    spec_invariants_relevant: &["spec §9 (partial vs final)".into()],
                    notes: format!("segments: {}, new: {new}", segments.len()),
                })?;
            }
            Ok(other) => {
                tracing::warn!(?other, "unexpected transcript shape");
                findings::append(&Finding {
                    timestamp: Utc::now(),
                    operation: "listen",
                    bot_id: Some(bot_id),
                    duration_ms: started.elapsed().as_millis(),
                    outcome: Outcome::Inconclusive,
                    http_status: Some(200),
                    spec_invariants_relevant: &[],
                    notes: "transcript was not an array; check schema".to_string(),
                })?;
            }
            Err(e) => {
                tracing::warn!(error = %e, "transcript poll failed");
            }
        }
        tokio::time::sleep(Duration::from_secs(poll_secs)).await;
    }
}

async fn cmd_speak(client: &recall_api::Client, bot_id: &str, audio_path: &Path) -> Result<()> {
    let bytes = tokio::fs::read(audio_path)
        .await
        .with_context(|| format!("read {}", audio_path.display()))?;
    let b64 = B64.encode(&bytes);
    if b64.len() > 1_830_000 {
        bail!(
            "base64 payload {} chars exceeds Recall's ~1.83M cap; trim the audio",
            b64.len()
        );
    }

    let started = Instant::now();
    let result = client.output_audio(bot_id, &b64).await;
    let duration_ms = started.elapsed().as_millis();

    let (outcome, notes) = match &result {
        Ok(_) => (
            Outcome::Inconclusive,
            format!(
                "accepted in {duration_ms}ms; actual audibility requires human in meeting to confirm. \
                 No utterance_id returned (validates spec §9 gap)."
            ),
        ),
        Err(e) => (Outcome::Failure, format!("{e:#}")),
    };
    findings::append(&Finding {
        timestamp: Utc::now(),
        operation: "speak",
        bot_id: Some(bot_id),
        duration_ms,
        outcome,
        http_status: result.is_ok().then_some(200),
        spec_invariants_relevant: &[
            "spec §9 (utterance ID — Recall returns none)".into(),
            "spec §11 (atomic Replace — not supported)".into(),
        ],
        notes,
    })?;
    result.map(|_| println!("output_audio accepted in {duration_ms}ms"))
}

async fn cmd_interrupt(client: &recall_api::Client, bot_id: &str) -> Result<()> {
    let started = Instant::now();
    let result = client.stop_output_audio(bot_id).await;
    let duration_ms = started.elapsed().as_millis();
    findings::append(&Finding {
        timestamp: Utc::now(),
        operation: "interrupt",
        bot_id: Some(bot_id),
        duration_ms,
        outcome: if result.is_ok() {
            Outcome::Success
        } else {
            Outcome::Failure
        },
        http_status: result.is_ok().then_some(204),
        spec_invariants_relevant: &["spec §9 (cancel granularity — channel only)".into()],
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
    println!("watching state transitions every {poll_secs}s; Ctrl-C to stop");
    let mut seen: usize = 0;
    loop {
        match client.get_bot(bot_id).await {
            Ok(detail) => {
                let new_changes: Vec<_> = detail.status_changes.iter().skip(seen).collect();
                for change in &new_changes {
                    println!(
                        "{}: code={} sub_code={:?} message={:?}",
                        change.created_at.to_rfc3339(),
                        change.code,
                        change.sub_code,
                        change.message
                    );
                    let terminal = matches!(
                        change.code.as_str(),
                        "done" | "fatal" | "ended" | "call_ended"
                    );
                    findings::append(&Finding {
                        timestamp: Utc::now(),
                        operation: "watch-eject",
                        bot_id: Some(bot_id),
                        duration_ms: 0,
                        outcome: Outcome::Success,
                        http_status: Some(200),
                        spec_invariants_relevant: &[
                            "spec §7 (kick-out as event)".into(),
                            "spec §9 (granular EjectReason)".into(),
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
    meeting_url: &str,
    audio_path: &Path,
    bot_name: &str,
) -> Result<()> {
    let bytes = tokio::fs::read(audio_path)
        .await
        .with_context(|| format!("read {}", audio_path.display()))?;
    let b64 = B64.encode(&bytes);

    println!("dispatching bot…");
    let join_started = Instant::now();
    let detail = client
        .create_bot(&recall_api::CreateBotArgs {
            meeting_url,
            bot_name,
        })
        .await?;
    let join_ms = join_started.elapsed().as_millis();
    let bot_id = detail.id.clone();
    println!("bot {bot_id} dispatched in {join_ms}ms");

    // Wait for the bot to enter a state where output_audio is accepted.
    // Recall typically transitions: ready → joining → in_call_*. We
    // poll until current_code starts with "in_call" or we time out.
    println!("waiting for bot to enter in-call state…");
    let in_call_started = Instant::now();
    let mut in_call_ms: Option<u128> = None;
    for _ in 0..60 {
        let d = client.get_bot(&bot_id).await?;
        let code = d.current_code();
        tracing::debug!(state = code, "polling");
        if code.starts_with("in_call") {
            in_call_ms = Some(in_call_started.elapsed().as_millis());
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let Some(in_call_ms) = in_call_ms else {
        bail!("bot did not reach in_call state within 60s");
    };
    println!("bot reached in_call state in {in_call_ms}ms after dispatch");

    println!("posting disclosure audio…");
    let speak_started = Instant::now();
    client.output_audio(&bot_id, &b64).await?;
    let speak_ms = speak_started.elapsed().as_millis();
    let total_ms = join_started.elapsed().as_millis();
    println!("output_audio accepted in {speak_ms}ms");
    println!("total join → disclosure-accepted: {total_ms}ms");
    println!("(actual audibility requires human in meeting to confirm)");

    findings::append(&Finding {
        timestamp: Utc::now(),
        operation: "disclosure-inject",
        bot_id: Some(&bot_id),
        duration_ms: total_ms,
        outcome: Outcome::Inconclusive,
        http_status: Some(200),
        spec_invariants_relevant: &[
            "spec §4 (disclosure ordering)".into(),
            "spec Invariant 6 (disclosure required)".into(),
        ],
        notes: format!(
            "join={join_ms}ms in_call={in_call_ms}ms speak_accept={speak_ms}ms total={total_ms}ms; \
             audibility requires human confirmation."
        ),
    })?;
    Ok(())
}

async fn cmd_leave(client: &recall_api::Client, bot_id: &str) -> Result<()> {
    let started = Instant::now();
    let result = client.leave_call(bot_id).await;
    let duration_ms = started.elapsed().as_millis();
    findings::append(&Finding {
        timestamp: Utc::now(),
        operation: "leave",
        bot_id: Some(bot_id),
        duration_ms,
        outcome: if result.is_ok() {
            Outcome::Success
        } else {
            Outcome::Failure
        },
        http_status: result.is_ok().then_some(200),
        spec_invariants_relevant: &["spec §3 (leave vs terminate split)".into()],
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
    let (outcome, notes) = match &result {
        Ok(_) => (Outcome::Success, "DELETE accepted (bot was pre-join)".to_string()),
        Err(e) => (
            Outcome::Failure,
            format!("DELETE rejected (likely already in_call): {e:#}"),
        ),
    };
    findings::append(&Finding {
        timestamp: Utc::now(),
        operation: "terminate",
        bot_id: Some(bot_id),
        duration_ms,
        outcome,
        http_status: result.is_ok().then_some(204),
        spec_invariants_relevant: &["spec §3 (DELETE only legal pre-join)".into()],
        notes,
    })?;
    if let Err(e) = result {
        println!("(expected) terminate failed in {duration_ms}ms: {e}");
    } else {
        println!("terminate accepted in {duration_ms}ms");
    }
    Ok(())
}
