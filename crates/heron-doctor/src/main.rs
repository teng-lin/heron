//! `heron-doctor` CLI binary entry point.

use std::path::PathBuf;
use std::process::ExitCode;

use chrono::Local;
use clap::{Parser, Subcommand};
use heron_doctor::{
    SessionSummaryRecord, Thresholds, count_unknown_versions, detect_anomalies,
    read_session_summaries,
};

#[derive(Debug, Parser)]
#[command(
    name = "heron-doctor",
    version,
    about = "Offline diagnosis of heron session logs",
    long_about = None,
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Read every `kind: session_summary` line from the given log
    /// file and print one JSON object per parsed record.
    Dump {
        /// Path to a heron daily log file. Defaults to today's file
        /// under `~/Library/Logs/heron/<YYYY-MM-DD>.log`.
        #[arg(default_value_os_t = default_log_path())]
        log_path: PathBuf,
    },
    /// Same input as `dump`, but only print sessions that hit one or
    /// more anomaly thresholds. Exits 0 if nothing is flagged, 1 if
    /// anything is flagged (so a CI hook can treat anomalies as a
    /// non-zero status).
    Anomalies {
        #[arg(default_value_os_t = default_log_path())]
        log_path: PathBuf,
        /// USD per session above which we flag cost as high.
        #[arg(long, default_value_t = Thresholds::default().max_cost_usd)]
        max_cost_usd: f64,
        /// AX hit rate (0.0–1.0) below which the session is flagged.
        #[arg(long, default_value_t = Thresholds::default().min_ax_hit_pct)]
        min_ax_hit_pct: f64,
        /// `low_conf_turns / turns_total` above which the session is
        /// flagged.
        #[arg(long, default_value_t = Thresholds::default().max_low_conf_ratio)]
        max_low_conf_ratio: f64,
        /// Suppress the dropped-frames flag.
        #[arg(long)]
        no_dropped_frames: bool,
        /// Suppress the device-changes flag.
        #[arg(long)]
        no_device_changes: bool,
    },
}

/// Resolves to `$HOME/Library/Logs/heron/<YYYY-MM-DD>.log` using
/// today's local date. Falls back to `./<date>.log` if `HOME` isn't
/// set or is empty (CI sandboxes, minimal containers).
fn default_log_path() -> PathBuf {
    let date = Local::now().format("%Y-%m-%d").to_string();
    if let Some(home) = std::env::var_os("HOME").filter(|s| !s.is_empty()) {
        PathBuf::from(home)
            .join("Library")
            .join("Logs")
            .join("heron")
            .join(format!("{date}.log"))
    } else {
        PathBuf::from(format!("{date}.log"))
    }
}

fn warn_on_unknown_versions(records: &[SessionSummaryRecord]) {
    let count = count_unknown_versions(records);
    if count > 0 {
        eprintln!(
            "warning: {count} record(s) have a non-1 log_version; \
             schema may have drifted (see docs/observability.md §field-stability)"
        );
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Dump { log_path } => match read_session_summaries(&log_path) {
            Ok(records) => {
                warn_on_unknown_versions(&records);
                print_records(&records);
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error reading {}: {e}", log_path.display());
                ExitCode::from(2)
            }
        },
        Cmd::Anomalies {
            log_path,
            max_cost_usd,
            min_ax_hit_pct,
            max_low_conf_ratio,
            no_dropped_frames,
            no_device_changes,
        } => {
            let thresholds = Thresholds {
                max_cost_usd,
                min_ax_hit_pct,
                max_low_conf_ratio,
                flag_dropped_frames: !no_dropped_frames,
                flag_device_changes: !no_device_changes,
            };
            match read_session_summaries(&log_path) {
                Ok(records) => {
                    warn_on_unknown_versions(&records);
                    let anomalies = detect_anomalies(&records, &thresholds);
                    let any = !anomalies.is_empty();
                    print_records(&anomalies);
                    if any {
                        ExitCode::from(1)
                    } else {
                        ExitCode::SUCCESS
                    }
                }
                Err(e) => {
                    eprintln!("error reading {}: {e}", log_path.display());
                    ExitCode::from(2)
                }
            }
        }
    }
}

/// Print one JSON line per item to stdout. Serialization failures go
/// to stderr but don't fail the run — they indicate a programmer bug
/// in `Serialize`, not a user error.
fn print_records<T: serde::Serialize>(items: &[T]) {
    for item in items {
        match serde_json::to_string(item) {
            Ok(line) => println!("{line}"),
            Err(e) => eprintln!("serialize error: {e}"),
        }
    }
}
