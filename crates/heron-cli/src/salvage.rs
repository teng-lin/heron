//! `heron salvage` — the recovery CLI per §14.3 done-when bullet
//! "Simulated SIGKILL during recording → salvage list on next launch."
//!
//! Walks the cache root via [`heron_types::discover_unfinished`] and
//! produces a human-readable list of unfinished sessions plus a JSON
//! mode for the Tauri shell to surface a banner.
//!
//! Exit codes — chosen so a launch script can short-circuit cleanly:
//! - `0` — no unfinished sessions
//! - `3` — one or more unfinished sessions found (distinct from the
//!   shared `2` IO-error code so callers can tell "broken" from "you
//!   have salvage candidates")
//! - `2` — IO error walking the cache

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use clap::ValueEnum;
use heron_types::recovery::{SessionPhase, SessionStateRecord, discover_unfinished};

/// Output format the user picks via `--format`. JSON is one record
/// per line so the Tauri shell can stream it.
///
/// Derives `clap::ValueEnum` so the CLI parser handles `human`/`json`
/// (and tab-completion) without a bespoke parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SalvageFormat {
    Human,
    Json,
}

/// Exit code constants. Kept as a public type so consumers (the CLI
/// binary, integration tests) don't have to hardcode the numbers.
pub mod exit_code {
    pub const CLEAN: i32 = 0;
    pub const IO_ERROR: i32 = 2;
    pub const HAS_CANDIDATES: i32 = 3;
}

/// Format and print the salvage list to `out`. Returns the number of
/// candidates written so the caller can pick the exit code.
///
/// `BrokenPipe` from the writer (e.g. `heron salvage | head -1`) is
/// treated as a clean stop, not an error — the stream-of-records
/// shape explicitly invites pipelines that close early.
pub fn print_salvage_list<W: Write>(
    out: &mut W,
    cache_root: &Path,
    format: SalvageFormat,
) -> Result<usize, SalvageError> {
    let mut unfinished = discover_unfinished(cache_root).map_err(SalvageError::Walk)?;
    // `discover_unfinished` sorts by `started_at`. Tie-break on the
    // session_id so two records with identical timestamps render in
    // the same order on every machine — important for golden tests
    // and diff-friendly output.
    unfinished.sort_by(|a, b| {
        a.started_at
            .cmp(&b.started_at)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });

    let result = match format {
        SalvageFormat::Human => write_human(out, cache_root, &unfinished),
        SalvageFormat::Json => write_json(out, &unfinished),
    };
    match result {
        Ok(()) => Ok(unfinished.len()),
        Err(SalvageError::Io(e)) if e.kind() == io::ErrorKind::BrokenPipe => Ok(unfinished.len()),
        Err(e) => Err(e),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SalvageError {
    #[error("walking cache root failed: {0}")]
    Walk(#[source] heron_types::recovery::RecoveryError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("serializing salvage record failed: {0}")]
    Serialize(#[from] serde_json::Error),
}

fn write_human<W: Write>(
    out: &mut W,
    cache_root: &Path,
    records: &[SessionStateRecord],
) -> Result<(), SalvageError> {
    if records.is_empty() {
        writeln!(out, "no unfinished sessions in {}", cache_root.display())?;
        return Ok(());
    }
    writeln!(
        out,
        "{} unfinished session(s) in {}:",
        records.len(),
        cache_root.display()
    )?;
    for r in records {
        let dur = r.last_updated.signed_duration_since(r.started_at);
        writeln!(out, "  - {sid}", sid = r.session_id)?;
        writeln!(
            out,
            "      phase:        {phase} {hint}",
            phase = phase_token(r.phase),
            hint = phase_hint(r.phase),
        )?;
        writeln!(out, "      app:          {app}", app = r.source_app)?;
        writeln!(
            out,
            "      cache_dir:    {dir}",
            dir = r.cache_dir.display(),
        )?;
        writeln!(
            out,
            "      started_at:   {start}",
            start = r.started_at.to_rfc3339(),
        )?;
        writeln!(
            out,
            "      last_updated: {last} ({dur} after start)",
            last = r.last_updated.to_rfc3339(),
            dur = humanize_duration(dur),
        )?;
        writeln!(
            out,
            "      mic_bytes:    {mic}, tap_bytes: {tap}, turns: {turns}",
            mic = r.mic_bytes_written,
            tap = r.tap_bytes_written,
            turns = r.turns_finalized,
        )?;
    }
    Ok(())
}

fn write_json<W: Write>(out: &mut W, records: &[SessionStateRecord]) -> Result<(), SalvageError> {
    for r in records {
        let line = serde_json::to_string(r)?;
        writeln!(out, "{line}")?;
    }
    Ok(())
}

/// Lower-case token matching the JSON wire form of [`SessionPhase`].
/// Using the serde-tagged form (rather than `Debug`) guarantees the
/// human and `--format json` outputs agree on phase spelling, which
/// matters for the Tauri shell rendering both.
fn phase_token(phase: SessionPhase) -> &'static str {
    match phase {
        SessionPhase::Armed => "armed",
        SessionPhase::Recording => "recording",
        SessionPhase::Transcribing => "transcribing",
        SessionPhase::Summarizing => "summarizing",
        SessionPhase::Done => "done",
        // `SessionPhase` is `#[non_exhaustive]`; keep the human
        // output stable for unknown phases instead of panicking.
        _ => "unknown",
    }
}

/// One-word hint added after the phase line so the user can tell at a
/// glance whether the session has audio worth recovering.
fn phase_hint(phase: SessionPhase) -> &'static str {
    match phase {
        SessionPhase::Armed => "(no audio yet — nothing to recover)",
        SessionPhase::Recording => "(recoverable — audio captured)",
        SessionPhase::Transcribing => "(recoverable — audio captured, STT in flight)",
        SessionPhase::Summarizing => "(recoverable — transcript ready, summarize in flight)",
        SessionPhase::Done => "(complete — would not appear in salvage list)",
        _ => "",
    }
}

/// Render a [`chrono::Duration`] like `12s`, `4m 03s`, `1h 02m`. Used
/// only in the human-readable salvage list.
fn humanize_duration(d: chrono::Duration) -> String {
    let total_secs = d.num_seconds().max(0);
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

/// Default cache-root location per `docs/archives/plan.md` §3 + §11.3 (the
/// ringbuffer lives under Application Support; per-session state is
/// inside that). Falls back to the current working directory when
/// `HOME` is unset (CI / sandbox), mirroring `heron-doctor`'s same
/// fallback so the binary is still usable without env state.
pub fn default_cache_root() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME").filter(|s| !s.is_empty()) {
        PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("com.heronnote.heron")
            .join("sessions")
    } else {
        PathBuf::from(".").join("heron-sessions")
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use chrono::Utc;
    use heron_types::SessionId;
    use heron_types::recovery::{SessionPhase, SessionStateRecord, write_state};

    use super::*;

    fn make_rec(cache_dir: PathBuf, phase: SessionPhase, age_secs: i64) -> SessionStateRecord {
        let now = Utc::now() - chrono::Duration::seconds(age_secs);
        SessionStateRecord {
            state_version: heron_types::recovery::STATE_VERSION,
            session_id: SessionId::now_v7(),
            started_at: now - chrono::Duration::seconds(60),
            last_updated: now,
            source_app: "us.zoom.xos".into(),
            cache_dir,
            phase,
            mic_bytes_written: 1_024,
            tap_bytes_written: 4_096,
            turns_finalized: 7,
        }
    }

    fn tmproot() -> tempfile::TempDir {
        tempfile::tempdir().expect("tmpdir")
    }

    #[test]
    fn empty_cache_root_emits_clean_human_message() {
        let dir = tmproot();
        let mut buf: Vec<u8> = Vec::new();
        let count = print_salvage_list(&mut buf, dir.path(), SalvageFormat::Human).expect("print");
        assert_eq!(count, 0);
        let s = String::from_utf8(buf).expect("utf8");
        assert!(s.contains("no unfinished sessions"));
    }

    #[test]
    fn empty_cache_root_emits_zero_lines_in_json() {
        let dir = tmproot();
        let mut buf: Vec<u8> = Vec::new();
        let count = print_salvage_list(&mut buf, dir.path(), SalvageFormat::Json).expect("print");
        assert_eq!(count, 0);
        assert!(buf.is_empty(), "json mode emits nothing for empty list");
    }

    #[test]
    fn missing_cache_root_is_treated_as_empty() {
        let dir = tmproot();
        let phantom = dir.path().join("does-not-exist");
        let mut buf: Vec<u8> = Vec::new();
        let count = print_salvage_list(&mut buf, &phantom, SalvageFormat::Human).expect("print");
        assert_eq!(count, 0);
    }

    #[test]
    fn finds_unfinished_sessions_and_skips_done() {
        let root = tmproot();
        let s1 = root.path().join("a");
        let s2 = root.path().join("b");
        let done = root.path().join("done");
        for d in [&s1, &s2, &done] {
            std::fs::create_dir_all(d).expect("mkdir");
        }
        write_state(&make_rec(s1.clone(), SessionPhase::Recording, 60)).expect("s1");
        write_state(&make_rec(s2.clone(), SessionPhase::Transcribing, 30)).expect("s2");
        write_state(&make_rec(done.clone(), SessionPhase::Done, 600)).expect("done");

        let mut buf: Vec<u8> = Vec::new();
        let count = print_salvage_list(&mut buf, root.path(), SalvageFormat::Human).expect("ok");
        assert_eq!(count, 2);
        let s = String::from_utf8(buf).expect("utf8");
        // Lower-case form (matches the JSON wire shape):
        assert!(
            s.contains("phase:        recording"),
            "missing recording: {s}"
        );
        assert!(
            s.contains("phase:        transcribing"),
            "missing transcribing: {s}"
        );
        // `done` is the only phase salvage filters out.
        assert!(
            !s.contains("phase:        done"),
            "should not list done: {s}"
        );
        // Recoverable hint surfaces alongside the phase line.
        assert!(s.contains("(recoverable"), "missing recoverable hint: {s}");
    }

    #[test]
    fn json_format_emits_one_line_per_record() {
        let root = tmproot();
        let s1 = root.path().join("a");
        let s2 = root.path().join("b");
        std::fs::create_dir_all(&s1).expect("mkdir");
        std::fs::create_dir_all(&s2).expect("mkdir");
        write_state(&make_rec(s1, SessionPhase::Recording, 30)).expect("s1");
        write_state(&make_rec(s2, SessionPhase::Summarizing, 10)).expect("s2");

        let mut buf: Vec<u8> = Vec::new();
        let count = print_salvage_list(&mut buf, root.path(), SalvageFormat::Json).expect("ok");
        assert_eq!(count, 2);
        let s = String::from_utf8(buf).expect("utf8");
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in lines {
            let _: SessionStateRecord = serde_json::from_str(line).expect("each line parses");
        }
    }

    #[test]
    fn human_output_includes_phase_app_and_paths() {
        let root = tmproot();
        let s1 = root.path().join("a");
        std::fs::create_dir_all(&s1).expect("mkdir");
        let rec = make_rec(s1.clone(), SessionPhase::Recording, 90);
        write_state(&rec).expect("s1");

        let mut buf: Vec<u8> = Vec::new();
        print_salvage_list(&mut buf, root.path(), SalvageFormat::Human).expect("ok");
        let s = String::from_utf8(buf).expect("utf8");
        assert!(s.contains("us.zoom.xos"), "missing app: {s}");
        assert!(s.contains(&s1.display().to_string()), "missing path: {s}");
        assert!(s.contains(&rec.session_id.to_string()), "missing sid: {s}");
        // mic/tap/turns rendered:
        assert!(s.contains("1024"));
        assert!(s.contains("4096"));
        assert!(s.contains("turns: 7"));
    }

    #[test]
    fn broken_pipe_from_writer_is_clean_exit_not_error() {
        // A consumer like `heron salvage | head -1` will close the
        // pipe after reading the first record. The CLI must NOT
        // surface that as an IO error / exit 2 — phase 34 explicitly
        // sets up `--format json` for streaming consumers.
        struct PipeAfter(usize);
        impl Write for PipeAfter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                if self.0 == 0 {
                    return Err(io::Error::new(io::ErrorKind::BrokenPipe, "test pipe"));
                }
                self.0 -= 1;
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let root = tmproot();
        let s1 = root.path().join("a");
        std::fs::create_dir_all(&s1).expect("mkdir");
        write_state(&make_rec(s1, SessionPhase::Recording, 30)).expect("s1");

        let mut writer = PipeAfter(0); // first write fails
        let res = print_salvage_list(&mut writer, root.path(), SalvageFormat::Json);
        assert!(res.is_ok(), "BrokenPipe must not surface: {res:?}");
    }

    #[test]
    fn other_io_errors_still_surface() {
        // Sanity: BrokenPipe is the only kind we swallow. A different
        // IO failure (e.g. WouldBlock) must still bubble out so the
        // caller gets exit code 2.
        struct FailingWriter;
        impl Write for FailingWriter {
            fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::WouldBlock, "would block"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let root = tmproot();
        let s1 = root.path().join("a");
        std::fs::create_dir_all(&s1).expect("mkdir");
        write_state(&make_rec(s1, SessionPhase::Recording, 30)).expect("s1");

        let mut writer = FailingWriter;
        let res = print_salvage_list(&mut writer, root.path(), SalvageFormat::Json);
        assert!(matches!(res, Err(SalvageError::Io(_))));
    }

    #[test]
    fn json_record_order_is_deterministic_via_session_id_tiebreak() {
        // Two records with identical `started_at` (synthetic clock)
        // must render in the same order on every machine; the
        // tiebreaker is `session_id`. Without it the order would
        // depend on `read_dir` enumeration which is FS-specific.
        let root = tmproot();
        let s1 = root.path().join("d1");
        let s2 = root.path().join("d2");
        std::fs::create_dir_all(&s1).expect("mkdir");
        std::fs::create_dir_all(&s2).expect("mkdir");

        let now = Utc::now();
        let mut a = make_rec(s1.clone(), SessionPhase::Recording, 0);
        let mut b = make_rec(s2.clone(), SessionPhase::Recording, 0);
        a.started_at = now;
        b.started_at = now;
        // Force a > b by session_id so tiebreak deterministically
        // places `b` first.
        let lo = SessionId::from_u128(1);
        let hi = SessionId::from_u128(2);
        a.session_id = hi;
        b.session_id = lo;
        write_state(&a).expect("a");
        write_state(&b).expect("b");

        let mut buf: Vec<u8> = Vec::new();
        print_salvage_list(&mut buf, root.path(), SalvageFormat::Json).expect("ok");
        let lines: Vec<&str> = std::str::from_utf8(&buf).expect("utf8").lines().collect();
        assert_eq!(lines.len(), 2);
        let first: SessionStateRecord = serde_json::from_str(lines[0]).expect("first parse");
        let second: SessionStateRecord = serde_json::from_str(lines[1]).expect("second parse");
        assert_eq!(first.session_id, lo);
        assert_eq!(second.session_id, hi);
    }

    #[test]
    fn phase_token_matches_the_serde_wire_form() {
        // The human format and the JSON output must agree on phase
        // spelling so a user grepping one against the other works.
        // `phase_token` returns a static str; `serde_json` encodes
        // the variant; both should produce the same byte sequence.
        for p in [
            SessionPhase::Armed,
            SessionPhase::Recording,
            SessionPhase::Transcribing,
            SessionPhase::Summarizing,
            SessionPhase::Done,
        ] {
            let json = serde_json::to_string(&p).expect("ser");
            // `serde_json::to_string` wraps the token in quotes.
            let stripped = json.trim_matches('"');
            assert_eq!(phase_token(p), stripped, "drift on {p:?}");
        }
    }

    #[test]
    fn humanize_duration_handles_each_band() {
        use chrono::Duration as Cd;
        assert_eq!(humanize_duration(Cd::seconds(0)), "0s");
        assert_eq!(humanize_duration(Cd::seconds(45)), "45s");
        assert_eq!(humanize_duration(Cd::seconds(125)), "2m 05s");
        assert_eq!(humanize_duration(Cd::seconds(3725)), "1h 02m");
        // negative inputs (clock went backwards) clamp to 0.
        assert_eq!(humanize_duration(Cd::seconds(-30)), "0s");
    }

    #[test]
    fn default_cache_root_with_home_set_lands_under_home() {
        // SAFETY: this test temporarily sets HOME for the current
        // process; we restore it before returning. Run in series
        // (no parallel test sets HOME the same way) — serial-test
        // would harden this further but is currently not in deps.
        let saved = std::env::var_os("HOME");
        // SAFETY: set_var is unsafe in nightly; on stable it's safe.
        // The deprecation only fires under a specific edition flag.
        unsafe {
            std::env::set_var("HOME", "/Users/example");
        }
        let p = default_cache_root();
        // SAFETY: restoring HOME to its prior value; uses the same
        // unsafe set_var on the path that requires it.
        unsafe {
            match saved {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
        assert!(p.starts_with("/Users/example"));
        assert!(p.ends_with("sessions"));
    }
}
