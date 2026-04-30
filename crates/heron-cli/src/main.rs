//! `heron` CLI scaffold.
//!
//! Subcommands per `docs/archives/implementation.md` weeks 9–13. Each command
//! returns `Err(anyhow::anyhow!("not yet implemented"))` until the
//! corresponding crate's real implementation lands; the scaffolding
//! is here so the user can already run `heron --help` and the
//! Tauri shell (week 11, §13) can wire CLI invocations against
//! these flags without churn.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use heron_cli::daemon::{ClientConfig, DEFAULT_BASE_URL, DEFAULT_TIMEOUT, DaemonClient};
use heron_cli::record_delegate::{
    DelegateConfig, drive_delegated_session, wait_for_stop as wait_for_record_stop,
};
use heron_cli::salvage::{
    SalvageFormat, default_cache_root, exit_code as salvage_exit, print_salvage_list,
};
use heron_cli::session;
use heron_cli::summarize;
use heron_cli::synthesize::{SynthOptions, synthesize_fixture};
use heron_session::{MeetingId, Platform, StartCaptureArgs};

#[derive(Debug, Parser)]
#[command(
    name = "heron",
    version,
    about = "Private, on-device, agent-friendly meeting note-taker.",
    long_about = "heron records native meeting calls, transcribes locally, \
                  attributes speakers via the meeting app's accessibility \
                  surface, and writes a markdown summary into your Obsidian \
                  vault. See docs/architecture.md for the current architecture."
)]
struct Cli {
    /// Increase log verbosity. `-v` = debug, `-vv` = trace.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    /// Path to the Obsidian vault (overrides $HERON_VAULT).
    #[arg(long, env = "HERON_VAULT", global = true)]
    vault: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Start recording a session for the foreground meeting app.
    Record(RecordArgs),
    /// Re-summarize an existing meeting note from its transcript.
    Summarize(SummarizeArgs),
    /// Print component health (TCC permissions, AX availability,
    /// vault path, ringbuffer status).
    Status,
    /// Verify that an m4a archival encode matches its source
    /// recording (used by the §12.3 ringbuffer purge logic).
    VerifyM4a(VerifyM4aArgs),
    /// Write a stub fixture directory (silent PCM + canned events)
    /// for offline regression of the aligner / STT / partial writer
    /// without committing real recordings to the repo.
    Synthesize(SynthesizeArgs),
    /// List unfinished sessions in the cache root (per §14.3 crash
    /// recovery). Exits 3 when any are found, 0 when clean, 2 on IO
    /// errors so a launch script can branch on the code without
    /// parsing stdout.
    Salvage(SalvageArgs),
    /// Dump the AX tree of a running app (e.g. Zoom) to stdout as
    /// JSON. Used during the `docs/archives/plan.md` §3.3 spike to capture the
    /// speaker-indicator `(role, subrole, identifier)` triple — diff
    /// two dumps (one with someone speaking, one with everyone muted)
    /// to identify the indicator element.
    AxDump(AxDumpArgs),
    /// v2 escape hatch: delegate session-control commands to the
    /// localhost `herond` daemon over its OpenAPI surface. The legacy
    /// `record` / `status` subcommands run an in-process orchestrator;
    /// `daemon …` reaches the same daemon the desktop shell drives, so
    /// CLI + GUI poke a single source of truth.
    #[command(subcommand)]
    Daemon(DaemonCommand),
}

#[derive(Debug, Subcommand)]
enum DaemonCommand {
    /// Show daemon health (`GET /v1/health`).
    Status(DaemonStatusArgs),
    /// Meeting-lifecycle commands.
    #[command(subcommand)]
    Meeting(DaemonMeetingCommand),
    /// Tail the daemon event bus (SSE projection of `/v1/events`).
    /// Defaults to a streaming follow; pass `--once` to print the
    /// replay window and exit.
    Events(DaemonEventsArgs),
}

#[derive(Debug, clap::Args)]
struct DaemonStatusArgs {
    /// Reuses the same override flags every other daemon subcommand
    /// accepts (see [`DaemonCommonArgs`]) — keeps the help output
    /// uniform across `daemon status` / `daemon meeting *` /
    /// `daemon events` and avoids two places to edit when a new
    /// shared flag (e.g. an `--insecure` for self-signed prod
    /// daemons) gets added.
    #[command(flatten)]
    common: DaemonCommonArgs,
}

#[derive(Debug, Subcommand)]
enum DaemonMeetingCommand {
    /// `POST /v1/meetings` — start a manual capture.
    Start(DaemonMeetingStartArgs),
    /// `POST /v1/meetings/{id}/end` — gracefully end a capture.
    End(DaemonMeetingEndArgs),
    /// `GET /v1/meetings` — list recent captures.
    List(DaemonMeetingListArgs),
    /// `GET /v1/meetings/{id}` — fetch a single meeting.
    Get(DaemonMeetingGetArgs),
}

#[derive(Debug, clap::Args)]
struct DaemonCommonArgs {
    /// Override the daemon base URL. Defaults to
    /// `http://127.0.0.1:7384/v1`.
    #[arg(long, env = "HERON_DAEMON_URL", global = false)]
    url: Option<String>,
    /// Override the bearer-token file path. Defaults to
    /// `~/.heron/cli-token`.
    #[arg(long, env = "HERON_DAEMON_TOKEN_FILE", global = false)]
    token_file: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
struct DaemonMeetingStartArgs {
    /// Native client to bind. v1 only serves `zoom`; the others are
    /// reserved for v1.1+. The CLI value is forwarded verbatim.
    #[arg(long, default_value = "zoom")]
    platform: PlatformArg,
    /// Optional free-form hint forwarded to the orchestrator (e.g.
    /// window title, meeting URL). Not a primary identifier.
    #[arg(long)]
    hint: Option<String>,
    /// Optional EventKit calendar event id to correlate this capture
    /// with a previously `attach_context`-supplied
    /// `PreMeetingContext`. When set and a context is pending for
    /// this id, the daemon consumes it as part of session
    /// materialization. Resolver-input shape per Invariant 4 — never
    /// a heron primary key.
    #[arg(long)]
    calendar_event_id: Option<String>,
    #[command(flatten)]
    common: DaemonCommonArgs,
}

#[derive(Debug, clap::Args)]
struct DaemonMeetingEndArgs {
    /// Meeting ID returned by `daemon meeting start` or seen on the
    /// `/v1/events` stream.
    meeting_id: String,
    #[command(flatten)]
    common: DaemonCommonArgs,
}

#[derive(Debug, clap::Args)]
struct DaemonMeetingListArgs {
    /// Optional platform filter.
    #[arg(long)]
    platform: Option<PlatformArg>,
    /// Page size. The daemon caps this server-side.
    #[arg(long)]
    limit: Option<u32>,
    #[command(flatten)]
    common: DaemonCommonArgs,
}

#[derive(Debug, clap::Args)]
struct DaemonMeetingGetArgs {
    meeting_id: String,
    #[command(flatten)]
    common: DaemonCommonArgs,
}

#[derive(Debug, clap::Args)]
struct DaemonEventsArgs {
    /// Resume after this `evt_*` ID. Maps to the spec's
    /// `?since_event_id` query param.
    #[arg(long)]
    since_event_id: Option<String>,
    /// Cap the number of events to print before exiting. `0` (the
    /// default) means follow indefinitely until the user hits Ctrl-C.
    #[arg(long, default_value_t = 0)]
    limit: u32,
    #[command(flatten)]
    common: DaemonCommonArgs,
}

/// Wraps [`heron_session::Platform`] so clap can derive `ValueEnum`
/// over it without forcing the upstream enum to depend on clap.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum PlatformArg {
    Zoom,
    GoogleMeet,
    MicrosoftTeams,
    Webex,
}

impl From<PlatformArg> for Platform {
    fn from(p: PlatformArg) -> Self {
        match p {
            PlatformArg::Zoom => Platform::Zoom,
            PlatformArg::GoogleMeet => Platform::GoogleMeet,
            PlatformArg::MicrosoftTeams => Platform::MicrosoftTeams,
            PlatformArg::Webex => Platform::Webex,
        }
    }
}

#[derive(Debug, clap::Args)]
struct AxDumpArgs {
    /// Bundle ID of the running app to walk (e.g. `us.zoom.xos`).
    #[arg(long, default_value = "us.zoom.xos")]
    bundle: String,
    /// Hard cap on visited nodes. `0` uses the bridge's internal
    /// default (4096) — large enough for a fully-populated Zoom
    /// gallery but bounded so a pathological tree can't hang.
    #[arg(long, default_value_t = 0)]
    max_nodes: i32,
    /// Optional output file. When omitted, the JSON is printed to
    /// stdout (pipe to `jq` to filter).
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
struct SalvageArgs {
    /// Cache root to walk. Defaults to the platform path under
    /// Application Support.
    #[arg(long)]
    cache_root: Option<PathBuf>,
    /// Output format: `human` (default) or `json` (one record per
    /// line, machine-parsable for the Tauri shell).
    #[arg(long, value_enum, default_value_t = SalvageFormat::Human)]
    format: SalvageFormat,
}

#[derive(Debug, clap::Args)]
struct SynthesizeArgs {
    /// Output directory. Refuses to overwrite a non-empty dir.
    out: PathBuf,
    /// Length of each `.wav` in seconds (max 300).
    #[arg(long, default_value_t = 30)]
    duration_secs: u32,
    /// Number of AX speaker events to spread across the duration.
    #[arg(long, default_value_t = 6)]
    ax_events: u32,
    /// Number of ground-truth turns to emit.
    #[arg(long, default_value_t = 6)]
    turns: u32,
}

#[derive(Debug, clap::Args)]
struct RecordArgs {
    /// Bundle ID of the meeting app to tap (e.g. `us.zoom.xos`).
    #[arg(long, default_value = "us.zoom.xos")]
    app: String,
    /// Where to spill the disk ringbuffer + final outputs. Defaults
    /// to `~/Library/Application Support/com.heronnote.heron`.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Hard cap on session duration; the session ends regardless of
    /// the meeting app's state when this expires.
    #[arg(long)]
    duration: Option<duration::Duration>,
    /// Inject a synthetic STT lag for the §7.4 backpressure spike.
    /// `0` disables.
    #[arg(long, default_value = "0", hide = true)]
    fake_stt_lag: f64,
    /// Drive the FSM through a no-op happy path without invoking any
    /// backend. Used by CI runners that lack TCC permissions for live
    /// audio capture; the real `record` flow needs Mic + Screen
    /// Recording + Accessibility grants.
    #[arg(long)]
    no_op: bool,
    /// v2 escape hatch: delegate session control to the localhost
    /// `herond` daemon over its OpenAPI surface (`POST /v1/meetings`,
    /// `/v1/events`, `POST /v1/meetings/{id}/end`) instead of running
    /// the v1 in-process pipeline. Used to exercise the same code
    /// path the desktop shell drives so CLI + GUI converge on one
    /// session-control surface.
    #[arg(long)]
    daemon: bool,
    /// Daemon-mode platform (forwarded as the `platform` field on
    /// `POST /v1/meetings`). Ignored unless `--daemon` is set.
    #[arg(long, value_enum, default_value_t = PlatformArg::Zoom)]
    platform: PlatformArg,
    /// Daemon-mode free-form hint (e.g. window title). Ignored unless
    /// `--daemon` is set.
    #[arg(long)]
    hint: Option<String>,
    /// Daemon-mode EventKit calendar event id to attach. Ignored
    /// unless `--daemon` is set.
    #[arg(long)]
    calendar_event_id: Option<String>,
    /// Daemon-mode override flags (`--url`, `--token-file`). Ignored
    /// unless `--daemon` is set.
    #[command(flatten)]
    daemon_common: DaemonCommonArgs,
}

#[derive(Debug, clap::Args)]
struct SummarizeArgs {
    /// Path to a `<note>.md` to re-summarize.
    note: PathBuf,
    /// Backend to call: `anthropic` (default), `claude-code`, `codex`.
    #[arg(long, default_value = "anthropic")]
    backend: String,
}

#[derive(Debug, clap::Args)]
struct VerifyM4aArgs {
    /// Path to the m4a to verify.
    path: PathBuf,
    /// Expected duration in seconds.
    #[arg(long)]
    duration_sec: f64,
}

mod duration {
    //! Tiny shim so `--duration 30m` parses without pulling in the
    //! full `humantime` crate. Accepts `<n>[smh]` or bare seconds.
    use std::time::Duration as StdDuration;

    /// Wraps `std::time::Duration`. The inner value is unread until
    /// the orchestrator wires `--duration` into the session-end
    /// timer (next phase); allow dead_code in the meantime.
    #[derive(Debug, Clone, Copy)]
    #[allow(dead_code)]
    pub struct Duration(pub StdDuration);

    impl std::str::FromStr for Duration {
        type Err = String;
        fn from_str(s: &str) -> Result<Self, Self::Err> {
            let s = s.trim();
            // Tolerate "30 m" (whitespace) and "30M" (uppercase) per
            // gemini's PR-14 comment — common CLI typos that
            // shouldn't surface a parse error.
            let (num, unit) = match s.chars().last() {
                Some(c) if c.is_ascii_alphabetic() => {
                    (s[..s.len() - 1].trim(), c.to_ascii_lowercase())
                }
                _ => (s, 's'),
            };
            let n: u64 = num
                .parse()
                .map_err(|e| format!("not a number: {num:?} ({e})"))?;
            let secs = match unit {
                's' => n,
                'm' => n.saturating_mul(60),
                'h' => n.saturating_mul(3600),
                _ => return Err(format!("unknown unit {unit:?} in {s:?}")),
            };
            Ok(Duration(StdDuration::from_secs(secs)))
        }
    }

    #[cfg(test)]
    #[allow(clippy::expect_used)]
    mod tests {
        use super::*;
        use std::str::FromStr;

        #[test]
        fn parses_bare_seconds() {
            let d = Duration::from_str("30").expect("30");
            assert_eq!(d.0, StdDuration::from_secs(30));
        }
        #[test]
        fn parses_with_unit() {
            assert_eq!(
                Duration::from_str("30s").expect("s").0,
                StdDuration::from_secs(30)
            );
            assert_eq!(
                Duration::from_str("5m").expect("m").0,
                StdDuration::from_secs(300)
            );
            assert_eq!(
                Duration::from_str("2h").expect("h").0,
                StdDuration::from_secs(7200)
            );
        }
        #[test]
        fn tolerates_whitespace_and_uppercase() {
            assert_eq!(
                Duration::from_str(" 30 m ").expect("ws").0,
                StdDuration::from_secs(1800)
            );
            assert_eq!(
                Duration::from_str("2H").expect("upper").0,
                StdDuration::from_secs(7200)
            );
        }
        #[test]
        fn rejects_unknown_unit() {
            assert!(Duration::from_str("5d").is_err());
        }
    }
}

fn main() {
    let cli = Cli::parse();
    install_tracing(cli.verbose);

    let exit = match cli.command {
        // Salvage uses bespoke exit codes per §14.3; the others
        // collapse to anyhow's default 0/1.
        Commands::Salvage(args) => cmd_salvage(args),
        cmd => match dispatch(cmd, cli.vault) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("error: {e:#}");
                1
            }
        },
    };
    std::process::exit(exit);
}

fn dispatch(cmd: Commands, vault: Option<PathBuf>) -> Result<()> {
    match cmd {
        Commands::Record(args) => cmd_record(args, vault),
        Commands::Summarize(args) => cmd_summarize(args, vault),
        Commands::Status => cmd_status(vault),
        Commands::VerifyM4a(args) => cmd_verify_m4a(args),
        Commands::Synthesize(args) => cmd_synthesize(args),
        Commands::AxDump(args) => cmd_ax_dump(args),
        Commands::Daemon(cmd) => cmd_daemon(cmd),
        // Handled by `main` directly so it can pick its own exit
        // code; this arm is unreachable but keeps the match
        // exhaustive without a wildcard.
        Commands::Salvage(_) => unreachable!("salvage handled in main"),
    }
}

fn cmd_daemon(cmd: DaemonCommand) -> Result<()> {
    // The daemon client is async (reqwest). Each top-level invocation
    // spins up a fresh runtime — these commands are short-lived and
    // share no state across calls, so keeping the runtime per-
    // invocation is simpler than threading a global through `main`.
    // `current_thread` is the right shape for a CLI: a single
    // request/response (or one streaming SSE) at a time, and no
    // need to pay for a multi-thread scheduler's worker pool.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow::anyhow!("tokio runtime: {e}"))?;
    runtime.block_on(async move {
        match cmd {
            DaemonCommand::Status(args) => daemon_status(args).await,
            DaemonCommand::Meeting(meeting) => match meeting {
                DaemonMeetingCommand::Start(args) => daemon_meeting_start(args).await,
                DaemonMeetingCommand::End(args) => daemon_meeting_end(args).await,
                DaemonMeetingCommand::List(args) => daemon_meeting_list(args).await,
                DaemonMeetingCommand::Get(args) => daemon_meeting_get(args).await,
            },
            DaemonCommand::Events(args) => daemon_events(args).await,
        }
    })
}

/// Build a [`DaemonClient`] from the shared override flags. Resolves
/// the bearer-token file (defaulting to `~/.heron/cli-token`) and
/// surfaces the typed `DaemonError` directly so the CLI prints the
/// actionable "is the daemon running?" message rather than the raw
/// reqwest error chain.
fn build_daemon_client(common: &DaemonCommonArgs) -> Result<DaemonClient> {
    let token_path = match &common.token_file {
        Some(p) => p.clone(),
        None => heron_cli::daemon::default_token_path()
            .map_err(|e| anyhow::anyhow!("resolving token path: {e}"))?,
    };
    let bearer = heron_cli::daemon::load_bearer(&token_path).map_err(|e| anyhow::anyhow!("{e}"))?;
    let base_url = common
        .url
        .clone()
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
    let client = DaemonClient::new(ClientConfig {
        bearer,
        base_url,
        timeout: DEFAULT_TIMEOUT,
    })
    .map_err(|e| anyhow::anyhow!("building daemon client: {e}"))?;
    Ok(client)
}

async fn daemon_status(args: DaemonStatusArgs) -> Result<()> {
    let client = build_daemon_client(&args.common)?;
    let health = client.health().await.map_err(|e| anyhow::anyhow!("{e}"))?;
    let body = serde_json::to_string_pretty(&health)
        .map_err(|e| anyhow::anyhow!("encoding health: {e}"))?;
    println!("{body}");
    Ok(())
}

async fn daemon_meeting_start(args: DaemonMeetingStartArgs) -> Result<()> {
    let client = build_daemon_client(&args.common)?;
    let meeting = client
        .start_capture(StartCaptureArgs {
            platform: args.platform.into(),
            hint: args.hint,
            calendar_event_id: args.calendar_event_id,
        })
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let body = serde_json::to_string_pretty(&meeting)
        .map_err(|e| anyhow::anyhow!("encoding meeting: {e}"))?;
    println!("{body}");
    Ok(())
}

async fn daemon_meeting_end(args: DaemonMeetingEndArgs) -> Result<()> {
    let id = parse_meeting_id(&args.meeting_id)?;
    let client = build_daemon_client(&args.common)?;
    client
        .end_meeting(&id)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("ok");
    Ok(())
}

async fn daemon_meeting_list(args: DaemonMeetingListArgs) -> Result<()> {
    let client = build_daemon_client(&args.common)?;
    let page = client
        .list_meetings(args.platform.map(Into::into), args.limit)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let body =
        serde_json::to_string_pretty(&page).map_err(|e| anyhow::anyhow!("encoding page: {e}"))?;
    println!("{body}");
    Ok(())
}

async fn daemon_meeting_get(args: DaemonMeetingGetArgs) -> Result<()> {
    let id = parse_meeting_id(&args.meeting_id)?;
    let client = build_daemon_client(&args.common)?;
    let meeting = client
        .get_meeting(&id)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let body = serde_json::to_string_pretty(&meeting)
        .map_err(|e| anyhow::anyhow!("encoding meeting: {e}"))?;
    println!("{body}");
    Ok(())
}

async fn daemon_events(args: DaemonEventsArgs) -> Result<()> {
    let client = build_daemon_client(&args.common)?;
    let mut stream = client
        .events(args.since_event_id.as_deref())
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut printed: u32 = 0;
    while let Some(item) = stream.next().await {
        match item {
            Ok(env) => {
                let line = serde_json::to_string(&env)
                    .unwrap_or_else(|e| format!("{{\"error\":\"encoding envelope: {e}\"}}"));
                println!("{line}");
                printed = printed.saturating_add(1);
                if args.limit > 0 && printed >= args.limit {
                    break;
                }
            }
            Err(e) => {
                eprintln!("event stream error: {e}");
                return Err(anyhow::anyhow!("{e}"));
            }
        }
    }
    Ok(())
}

fn parse_meeting_id(s: &str) -> Result<MeetingId> {
    // `prefixed_id!` macro stamps `FromStr` on every prefixed ID
    // type, so the CLI inherits whatever validation `heron-types`
    // enforces — no need to round-trip through serde just to reuse
    // the same checker.
    s.parse::<MeetingId>()
        .map_err(|e| anyhow::anyhow!("invalid meeting id {s:?}: {e}"))
}

fn cmd_ax_dump(args: AxDumpArgs) -> Result<()> {
    tracing::info!(?args, "ax-dump requested");
    let json = heron_zoom::ax_dump_tree(&args.bundle, args.max_nodes)
        .map_err(|e| anyhow::anyhow!("ax-dump: {e}"))?;
    match args.out {
        Some(path) => {
            std::fs::write(&path, &json)
                .map_err(|e| anyhow::anyhow!("write {}: {e}", path.display()))?;
            println!("wrote AX tree dump to {}", path.display());
        }
        None => {
            println!("{json}");
        }
    }
    Ok(())
}

fn cmd_salvage(args: SalvageArgs) -> i32 {
    let root = args.cache_root.unwrap_or_else(default_cache_root);
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    match print_salvage_list(&mut handle, &root, args.format) {
        Ok(0) => salvage_exit::CLEAN,
        Ok(_) => salvage_exit::HAS_CANDIDATES,
        Err(e) => {
            // `{e:#}` walks the source chain so the user sees the
            // actual IO cause (e.g. permission denied), not just the
            // outer "walking cache root failed" wrapper.
            eprintln!("salvage: {e:#}");
            // Surface the source via tracing too — heron-doctor
            // tails this for anomalies.
            tracing::warn!(error = %e, "salvage walk failed");
            salvage_exit::IO_ERROR
        }
    }
}

fn cmd_synthesize(args: SynthesizeArgs) -> Result<()> {
    tracing::info!(?args, "synthesize requested");
    let opts = SynthOptions {
        duration_secs: args.duration_secs,
        ax_events: args.ax_events,
        turns: args.turns,
    };
    synthesize_fixture(&args.out, &opts).map_err(|e| anyhow::anyhow!("synthesize: {e}"))?;
    println!("wrote stub fixture to {}", args.out.display());
    Ok(())
}

fn cmd_record(args: RecordArgs, vault: Option<PathBuf>) -> Result<()> {
    tracing::info!(?args, "record requested");

    if args.daemon {
        // The v1 `--no-op` flag walks the in-process FSM; it has no
        // analogue on the daemon HTTP surface, so refuse rather than
        // silently ignoring it. `--app` / `--out` / `--fake-stt-lag`
        // are similarly v1-only — accept them for backwards-compat
        // (so existing scripts still parse) but they have no effect
        // on the delegated path.
        if args.no_op {
            return Err(anyhow::anyhow!(
                "--no-op cannot be combined with --daemon (the no-op path \
                 walks the v1 in-process FSM; the daemon has no equivalent). \
                 Drop one of the flags."
            ));
        }
        return cmd_record_via_daemon(args);
    }

    let cache = args
        .out
        .clone()
        .unwrap_or_else(|| PathBuf::from("/tmp/heron-cli-cache"));
    let vault_root = vault.unwrap_or_else(|| PathBuf::from("/tmp/heron-cli-vault"));
    // Each invocation gets a fresh v7 UUID so the cache + transcript
    // + recording paths don't collide across runs. Tests that need a
    // deterministic id construct `SessionConfig` directly.
    let cfg = session::SessionConfig {
        session_id: uuid::Uuid::now_v7(),
        target_bundle_id: args.app.clone(),
        cache_dir: cache,
        vault_root,
        stt_backend_name: "sherpa".into(),
        // The `heron record` CLI doesn't read Tauri Settings; it ships
        // with no hotwords by default. Daemon callers (Tauri / herond)
        // populate this from `Settings::hotwords` instead.
        hotwords: Vec::new(),
        llm_preference: heron_llm::Preference::Auto,
        // CLI captures never stage pre-meeting context — that path is
        // a daemon-only concern (`attach_context` -> `start_capture`).
        pre_meeting_briefing: None,
        // CLI captures have no SSE consumer listening, so the AX
        // bridge stays an offline-aligner-only feed.
        event_bus: None,
        // Tier 4: CLI captures don't read the desktop's `Settings.persona`
        // / `Settings.strip_names_before_summarization` — leave both off
        // so the prompt path stays byte-identical to pre-Tier-4 here.
        persona: None,
        strip_names: false,
        // CLI captures have no pause UI; the pipeline treats `None`
        // as "never paused" (see `pause_flag` doc on SessionConfig).
        pause_flag: None,
    };

    if args.no_op {
        tracing::info!("--no-op: walking FSM without invoking backends");
        let mut orch = session::Orchestrator::new(cfg);
        let outcome = orch
            .run_no_op(heron_types::SummaryOutcome::Done)
            .map_err(|e| anyhow::anyhow!("no-op session: {e}"))?;
        println!("no-op session complete: {outcome:?}");
        return Ok(());
    }

    let mut orch = session::Orchestrator::new(cfg);
    let (stt, ax, _llm, _cal) = orch
        .backends()
        .map_err(|e| anyhow::anyhow!("backend wiring: {e}"))?;
    tracing::info!(stt = stt.name(), ax = ax.name(), "backends resolved");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow::anyhow!("tokio runtime: {e}"))?;
    runtime.block_on(async move {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        // Ctrl-C handler: signals the orchestrator to drain + finalize
        // rather than aborting mid-write. A second Ctrl-C is treated
        // by the OS — we don't trap it, so the user can still escape
        // a stuck pipeline.
        tokio::spawn(async move {
            if let Err(e) = tokio::signal::ctrl_c().await {
                tracing::warn!(error = %e, "ctrl_c handler failed; orchestrator will run until duration cap");
                return;
            }
            let _ = stop_tx.send(());
            eprintln!("\nstop signal received; finalizing session...");
        });
        let outcome = orch
            .run(stop_rx)
            .await
            .map_err(|e| anyhow::anyhow!("session run: {e}"))?;
        match outcome.note_path {
            Some(p) => println!("session complete: {}", p.display()),
            None => println!("session complete: no note written ({:?})", outcome.last_idle_reason),
        }
        Ok::<_, anyhow::Error>(())
    })
}

/// `heron record --daemon` entry point. Spins up a single-threaded
/// tokio runtime (HTTP + SSE only — no audio threads to feed) and
/// dispatches to [`drive_delegated_session`]. Mirrors the runtime
/// shape of [`cmd_daemon`] so the two HTTP-driven CLI paths look the
/// same to a reader.
fn cmd_record_via_daemon(args: RecordArgs) -> Result<()> {
    let client = build_daemon_client(&args.daemon_common)?;
    let duration_cap = args.duration.map(|d| d.0);
    let config = DelegateConfig {
        start: StartCaptureArgs {
            platform: args.platform.into(),
            hint: args.hint.clone(),
            calendar_event_id: args.calendar_event_id.clone(),
        },
        duration_cap,
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow::anyhow!("tokio runtime: {e}"))?;
    runtime.block_on(async move {
        let stop = wait_for_record_stop(duration_cap);
        drive_delegated_session(&client, config, stop)
            .await
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("{e}"))
    })
}

fn cmd_summarize(args: SummarizeArgs, vault: Option<PathBuf>) -> Result<()> {
    tracing::info!(?args, "summarize requested");

    // The note's `transcript:` frontmatter field is vault-relative
    // (so notes survive the user moving the vault), and §10.3 merge
    // writes go through `VaultWriter` which is rooted at the vault.
    // Both code paths need an authoritative vault root, so require it.
    let vault_root = vault.ok_or_else(|| {
        anyhow::anyhow!(
            "summarize: --vault not set and $HERON_VAULT unset; \
             pass --vault <path> to the Obsidian vault containing the note"
        )
    })?;

    let backend = summarize::parse_backend_flag(&args.backend)?;
    let summarizer = heron_llm::build_summarizer(backend);
    tracing::info!(?backend, "summarize: using LLM backend");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow::anyhow!("tokio runtime: {e}"))?;
    let outcome = runtime
        .block_on(summarize::re_summarize_in_vault(
            &*summarizer,
            &vault_root,
            &args.note,
        ))
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    println!(
        "summarize complete: {} ({} action items, {} attendees)",
        args.note.display(),
        outcome.frontmatter.action_items.len(),
        outcome.frontmatter.attendees.len(),
    );
    Ok(())
}

fn cmd_status(vault: Option<PathBuf>) -> Result<()> {
    println!("heron CLI status — environment preflight");
    println!();

    // Tooling probe: ffmpeg / ffprobe presence is a §0.1 prereq.
    println!("system tools:");
    let ffmpeg = check_on_path("ffmpeg");
    let ffprobe = check_on_path("ffprobe");
    print_tool("ffmpeg", &ffmpeg);
    print_tool("ffprobe", &ffprobe);

    // Vault probe: if a vault is configured (env or flag), report
    // whether it's readable + whether validate_vault surfaces issues.
    println!();
    println!("vault:");
    match vault {
        Some(path) => {
            if !path.exists() {
                println!("  ✘ {} — does not exist", path.display());
            } else if !path.is_dir() {
                println!("  ✘ {} — not a directory", path.display());
            } else {
                println!("  ✓ {}", path.display());
                let issues = heron_vault::validate_vault(&path);
                // `Issue::is_error()` is the canonical predicate so a
                // future warning variant added to heron-vault is
                // counted as a warning here without requiring this
                // call site to be updated.
                let issue_count = issues.iter().filter(|i| i.is_error()).count();
                let warn_count = issues.len() - issue_count;
                if issue_count == 0 && warn_count == 0 {
                    println!("    no issues");
                } else {
                    println!(
                        "    {issue_count} issue(s), {warn_count} warning(s) — \
                         run `validate-vault` for details"
                    );
                }
            }
        }
        None => println!("  · no vault configured ($HERON_VAULT or --vault to set)"),
    }

    println!();
    println!("crates committed (this binary):");
    println!("  ✓ heron-types     §5.2 + §5.3 SessionClock + §14.3 recovery");
    println!("  ✓ heron-audio     §6.2 surface + §7.2 ringbuffer + §7.4 backpressure");
    println!("  ✓ heron-speech    §8.1 trait surface (stub backends)");
    println!("  ✓ heron-zoom      §9.1 AxBackend trait + §9.3 aligner");
    println!("  ✓ heron-llm       §11.1 surface + §11.2 meeting.hbs + §11.4 cost");
    println!("  ✓ heron-vault     §10 merge + §12 writer + §11.3 encode + verify");
    println!("  ⏳ heron-session  orchestrator (next phase)");
    Ok(())
}

#[derive(Debug)]
enum ToolStatus {
    Present(PathBuf),
    Missing,
}

fn check_on_path(name: &str) -> ToolStatus {
    let Some(paths) = std::env::var_os("PATH") else {
        return ToolStatus::Missing;
    };
    for dir in std::env::split_paths(&paths) {
        let candidate = dir.join(name);
        // One `metadata` call per candidate covers both is-file and
        // (on unix) the executable-bit check; previously `is_file()`
        // and `metadata()` each called stat(2) for the same file.
        let Ok(meta) = std::fs::metadata(&candidate) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        // Match the executable-bit check `heron_vault::encode::is_on_path`
        // applies. A non-executable file at PATH/ffmpeg shouldn't
        // surface as "present" — running it would fail anyway.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o111 == 0 {
                continue;
            }
        }
        return ToolStatus::Present(candidate);
    }
    ToolStatus::Missing
}

fn print_tool(name: &str, status: &ToolStatus) {
    match status {
        ToolStatus::Present(p) => println!("  ✓ {name} at {}", p.display()),
        ToolStatus::Missing => println!("  ✘ {name} not on PATH (brew install ffmpeg)"),
    }
}

fn cmd_verify_m4a(args: VerifyM4aArgs) -> Result<()> {
    tracing::info!(?args, "verify_m4a requested");
    if !args.path.exists() {
        return Err(anyhow::anyhow!(
            "verify-m4a: file not found: {}",
            args.path.display()
        ));
    }
    if !args.path.is_file() {
        return Err(anyhow::anyhow!(
            "verify-m4a: not a regular file: {}",
            args.path.display()
        ));
    }
    if args.duration_sec <= 0.0 || !args.duration_sec.is_finite() {
        return Err(anyhow::anyhow!(
            "verify-m4a: --duration-sec must be a positive finite number; got {}",
            args.duration_sec
        ));
    }
    let ok = heron_vault::verify_m4a(&args.path, args.duration_sec)
        .map_err(|e| anyhow::anyhow!("verify-m4a: {e}"))?;
    if ok {
        println!(
            "verify-m4a: OK ({} matches expected duration {:.3}s within ±1%)",
            args.path.display(),
            args.duration_sec
        );
        Ok(())
    } else {
        // Distinct exit so a launch script can branch — but use
        // anyhow::anyhow! since the rest of the dispatch flow
        // collapses non-zero to exit 1; the README/§12.3 callers
        // only need "ok or not".
        Err(anyhow::anyhow!(
            "verify-m4a: {} does not match expected duration {:.3}s (per §12.3 \
             ringbuffer purge gate; ringbuffer would be retained).",
            args.path.display(),
            args.duration_sec
        ))
    }
}

fn install_tracing(verbose: u8) {
    use tracing_subscriber::{EnvFilter, fmt};

    let default = match verbose {
        0 => "heron=info,warn",
        1 => "heron=debug,info",
        _ => "heron=trace,debug",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));

    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}
