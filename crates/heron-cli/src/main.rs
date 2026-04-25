//! `heron` CLI scaffold.
//!
//! Subcommands per `docs/implementation.md` weeks 9–13. Each command
//! returns `Err(anyhow::anyhow!("not yet implemented"))` until the
//! corresponding crate's real implementation lands; the scaffolding
//! is here so the user can already run `heron --help` and the
//! Tauri shell (week 11, §13) can wire CLI invocations against
//! these flags without churn.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use heron_cli::salvage::{
    SalvageFormat, default_cache_root, exit_code as salvage_exit, print_salvage_list,
};
use heron_cli::session;
use heron_cli::synthesize::{SynthOptions, synthesize_fixture};

#[derive(Debug, Parser)]
#[command(
    name = "heron",
    version,
    about = "Private, on-device, agent-friendly meeting note-taker.",
    long_about = "heron records native meeting calls, transcribes locally, \
                  attributes speakers via the meeting app's accessibility \
                  surface, and writes a markdown summary into your Obsidian \
                  vault. See docs/plan.md for the full architecture."
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
        // Handled by `main` directly so it can pick its own exit
        // code; this arm is unreachable but keeps the match
        // exhaustive without a wildcard.
        Commands::Salvage(_) => unreachable!("salvage handled in main"),
    }
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

    // Wire the orchestrator skeleton: even though every backend
    // returns NotYetImplemented today, going through Orchestrator
    // exercises the FSM + selection logic so a "real audio yet?"
    // smoke test boils down to running this command.
    let cache = args
        .out
        .clone()
        .unwrap_or_else(|| PathBuf::from("/tmp/heron-cli-cache"));
    let vault_root = vault.unwrap_or_else(|| PathBuf::from("/tmp/heron-cli-vault"));
    let cfg = session::SessionConfig {
        session_id: heron_types::SessionId::nil(),
        target_bundle_id: args.app.clone(),
        cache_dir: cache,
        vault_root,
        stt_backend_name: "sherpa".into(),
        llm_backend: heron_llm::Backend::Anthropic,
    };
    let orch = session::Orchestrator::new(cfg);
    let (stt, ax, _llm) = orch
        .backends()
        .map_err(|e| anyhow::anyhow!("backend wiring: {e}"))?;
    tracing::info!(stt = stt.name(), ax = ax.name(), "backends resolved");

    Err(anyhow::anyhow!(
        "record: orchestrator wired but audio capture is stubbed \
         (NotYetImplemented). Real recording arrives once the §6 \
         capture pipeline lands. Use the Tauri shell once §13 \
         ships for the full UX."
    ))
}

fn cmd_summarize(args: SummarizeArgs, _vault: Option<PathBuf>) -> Result<()> {
    tracing::info!(?args, "summarize requested");
    Err(anyhow::anyhow!(
        "summarize: not yet implemented (arrives week 9 per §11). \
         heron-llm trait surface + meeting.hbs template are in place; \
         this command needs the orchestrator wiring to call them."
    ))
}

fn cmd_status(_vault: Option<PathBuf>) -> Result<()> {
    println!("heron CLI scaffold — orchestrator not yet wired (§13, week 11).");
    println!();
    println!("crates committed:");
    println!("  ✓ heron-types     §5.2 + §5.3 SessionClock");
    println!("  ✓ heron-audio     §6.2 surface + §7.2 ringbuffer + §7.4 backpressure");
    println!("  ✓ heron-speech    §8.1 trait surface (stub backends)");
    println!("  ✓ heron-zoom      §9.1 AxBackend trait + §9.3 aligner");
    println!("  ✓ heron-llm       §11.1 surface + §11.2 meeting.hbs");
    println!("  ✓ heron-vault     §10 merge + §12 writer + §11.3 encode");
    println!("  ⏳ heron-session  orchestrator (next phase)");
    Ok(())
}

fn cmd_verify_m4a(args: VerifyM4aArgs) -> Result<()> {
    tracing::info!(?args, "verify_m4a requested");
    Err(anyhow::anyhow!(
        "verify-m4a: heron-vault::verify_m4a is in place but heron-cli \
         doesn't yet depend on heron-vault directly; that wiring lands \
         alongside the orchestrator (next phase)."
    ))
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
