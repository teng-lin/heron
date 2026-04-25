//! `validate-vault` — walk a vault and report integrity issues.
//!
//! See `heron_vault::validate::validate_vault` for the rules; this
//! binary is a thin CLI wrapper that prints one JSON line per issue
//! and exits non-zero if any *error*-level issue is present.
//! `NoBackup` is informational and does not affect the exit status.

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use heron_vault::validate_vault;

fn usage() -> ExitCode {
    eprintln!("Usage: validate-vault <vault-root>");
    eprintln!();
    eprintln!("Walks <vault-root>/meetings/ and prints one JSON line per integrity issue.");
    eprintln!(
        "Exits 0 on a clean vault, 1 if any error-level issue is reported, 2 on usage error."
    );
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.len() != 1 {
        return usage();
    }
    let root = PathBuf::from(&args[0]);
    if !root.is_dir() {
        eprintln!("error: {} is not a directory", root.display());
        return ExitCode::from(2);
    }
    let issues = validate_vault(&root);
    let mut any_error = false;
    for issue in &issues {
        if issue.is_error() {
            any_error = true;
        }
        match serde_json::to_string(issue) {
            Ok(line) => println!("{line}"),
            Err(e) => eprintln!("serialize error: {e}"),
        }
    }
    if any_error {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}
