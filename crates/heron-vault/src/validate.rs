//! `validate_vault` — walk a vault root and report integrity issues.
//!
//! Used by the `validate-vault` binary that ships beside this crate.
//! Pure logic + filesystem reads; no network, no subprocesses.
//!
//! ## What it checks
//!
//! For each `.md` under `<vault>/meetings/`:
//!
//! - **Frontmatter parses** — runs the full `read_note` parser; any
//!   error becomes [`Issue::FrontmatterError`].
//! - **`recording` path exists** — frontmatter's `recording: <path>`
//!   must point to a file on disk; missing → [`Issue::MissingRecording`].
//! - **`transcript` path exists** — same shape, separate variant for
//!   clarity in the report.
//! - **`.md.bak` companion** — re-summarize uses `<note>.md.bak` as
//!   the merge `base`. A note that's been re-summarized at least once
//!   should have one. First-pass notes don't, so the validator just
//!   warns rather than errors when it's missing (per `Issue::NoBackup`).
//!
//! And across the corpus:
//!
//! - **Duplicate session paths** — two `.md` files referencing the
//!   same `recording`/`transcript` path is a sign the vault has a
//!   stray file (likely a manual copy gone wrong).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::writer::{VaultError, read_note};

/// One integrity issue. Tagged enum so the report consumer can match
/// on `kind` rather than parse prose.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Issue {
    /// `read_note` failed; frontmatter is malformed or the YAML is
    /// invalid. The wrapped error message comes from [`VaultError::Display`].
    FrontmatterError {
        note: PathBuf,
        error: String,
    },
    MissingRecording {
        note: PathBuf,
        recording: PathBuf,
    },
    MissingTranscript {
        note: PathBuf,
        transcript: PathBuf,
    },
    /// Soft warning: `<note>.md.bak` doesn't exist. First-pass notes
    /// always look like this; tooling should treat it as informational.
    NoBackup {
        note: PathBuf,
    },
    /// Two notes reference the same recording or transcript path.
    DuplicateRecording {
        recording: PathBuf,
        notes: Vec<PathBuf>,
    },
}

impl Issue {
    /// `true` for issues that mean "the vault has a real problem"
    /// (vs. soft informational warnings like `NoBackup`).
    pub fn is_error(&self) -> bool {
        !matches!(self, Issue::NoBackup { .. })
    }
}

/// Walk `<vault_root>/meetings/` and produce a flat list of issues.
/// Returns an empty Vec for a healthy vault.
///
/// Missing `<vault_root>/meetings/` is *not* an error — a fresh
/// install has no meetings yet. The function returns `Ok(vec![])`.
pub fn validate_vault(vault_root: &Path) -> Vec<Issue> {
    let mut issues = Vec::new();
    let mut recordings: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();

    let meetings_dir = vault_root.join("meetings");
    let entries = match std::fs::read_dir(&meetings_dir) {
        Ok(e) => e,
        Err(_) => return issues, // missing meetings/ is fine
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        if path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|n| n.ends_with(".md.bak"))
        {
            // .bak files are inspected via their parent .md; skip
            // them in the primary walk.
            continue;
        }

        match read_note(&path) {
            Ok((fm, _body)) => {
                if !fm.recording.as_os_str().is_empty() && !fm.recording.exists() {
                    issues.push(Issue::MissingRecording {
                        note: path.clone(),
                        recording: fm.recording.clone(),
                    });
                }
                if !fm.transcript.as_os_str().is_empty() && !fm.transcript.exists() {
                    issues.push(Issue::MissingTranscript {
                        note: path.clone(),
                        transcript: fm.transcript.clone(),
                    });
                }
                recordings
                    .entry(fm.recording.clone())
                    .or_default()
                    .push(path.clone());

                let bak = bak_path_for(&path);
                if !bak.exists() {
                    issues.push(Issue::NoBackup { note: path.clone() });
                }
            }
            Err(e) => {
                issues.push(Issue::FrontmatterError {
                    note: path.clone(),
                    error: format_vault_error(&e),
                });
            }
        }
    }

    for (recording, notes) in recordings {
        if notes.len() > 1 && !recording.as_os_str().is_empty() {
            issues.push(Issue::DuplicateRecording { recording, notes });
        }
    }

    // Stable order so test golden files don't churn on HashMap
    // iteration order.
    issues.sort_by_key(|i| serde_json::to_string(i).unwrap_or_default());
    issues
}

fn bak_path_for(note: &Path) -> PathBuf {
    let mut p = note.as_os_str().to_owned();
    p.push(".bak");
    PathBuf::from(p)
}

fn format_vault_error(e: &VaultError) -> String {
    e.to_string()
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::fs;

    use chrono::NaiveDate;
    use heron_types::{Cost, DiarizeSource, Disclosure, DisclosureHow, Frontmatter, MeetingType};

    use super::*;
    use crate::writer::VaultWriter;

    fn fixture_frontmatter(recording: PathBuf, transcript: PathBuf) -> Frontmatter {
        Frontmatter {
            date: NaiveDate::from_ymd_opt(2026, 4, 24).expect("ymd"),
            start: "09:00".to_owned(),
            duration_min: 30,
            company: Some("Acme".to_owned()),
            attendees: vec![],
            meeting_type: MeetingType::Client,
            source_app: "us.zoom.xos".to_owned(),
            recording,
            transcript,
            diarize_source: DiarizeSource::Ax,
            disclosed: Disclosure {
                stated: true,
                when: None,
                how: DisclosureHow::PreEmail,
            },
            cost: Cost {
                summary_usd: 0.04,
                tokens_in: 10_000,
                tokens_out: 500,
                model: "claude-sonnet-4-6".to_owned(),
            },
            tags: vec!["acme".to_owned()],
            action_items: vec![],
            extra: serde_yaml::Mapping::new(),
        }
    }

    fn write_note(vault: &Path, slug: &str, fm: Frontmatter) -> PathBuf {
        fs::create_dir_all(vault.join("meetings")).expect("mkdir");
        let writer = VaultWriter::new(vault);
        writer
            .finalize_session("2026-04-24", "09:00", slug, &fm, "body content")
            .expect("finalize")
    }

    #[test]
    fn empty_vault_has_no_issues() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let issues = validate_vault(tmp.path());
        assert!(issues.is_empty());
    }

    #[test]
    fn note_finalized_through_writer_is_clean() {
        // VaultWriter::finalize_session writes both .md and .md.bak,
        // so a note created the canonical way should pass with zero
        // issues. NoBackup only fires for stray notes the user drops
        // into the vault by hand.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let recording = tmp.path().join("recording.m4a");
        let transcript = tmp.path().join("transcript.jsonl");
        fs::write(&recording, b"fake m4a").expect("seed");
        fs::write(&transcript, b"{}").expect("seed");

        let fm = fixture_frontmatter(recording, transcript);
        let note = write_note(tmp.path(), "alice", fm);
        let issues = validate_vault(tmp.path());
        assert!(issues.is_empty(), "expected clean vault, got {issues:?}");
        let _ = note;
    }

    #[test]
    fn stray_note_without_md_bak_yields_no_backup_warning() {
        // A user could drop a hand-edited .md into meetings/ without
        // a .md.bak. The validator surfaces it as a soft warning.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let meetings = tmp.path().join("meetings");
        fs::create_dir_all(&meetings).expect("mkdir");

        // Hand-write a valid frontmatter but no .bak companion.
        let recording = tmp.path().join("rec.m4a");
        let transcript = tmp.path().join("rec.jsonl");
        fs::write(&recording, b"fake").expect("seed rec");
        fs::write(&transcript, b"{}").expect("seed tr");
        let fm = fixture_frontmatter(recording, transcript);
        let body = format!(
            "---\n{}\n---\n\nstray body\n",
            serde_yaml::to_string(&fm).expect("ser")
        );
        fs::write(meetings.join("stray.md"), body).expect("seed stray");

        let issues = validate_vault(tmp.path());
        let warnings: Vec<_> =
            issues.iter().filter(|i| matches!(i, Issue::NoBackup { .. })).collect();
        assert_eq!(warnings.len(), 1);
        assert!(!warnings[0].is_error(), "NoBackup must be soft");
    }

    #[test]
    fn missing_recording_is_an_error() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let fm = fixture_frontmatter(
            tmp.path().join("does-not-exist.m4a"),
            tmp.path().join("does-not-exist.jsonl"),
        );
        write_note(tmp.path(), "ghost", fm);
        let issues = validate_vault(tmp.path());
        let errs: Vec<_> = issues.iter().filter(|i| i.is_error()).collect();
        assert!(
            errs.iter()
                .any(|i| matches!(i, Issue::MissingRecording { .. }))
        );
        assert!(
            errs.iter()
                .any(|i| matches!(i, Issue::MissingTranscript { .. }))
        );
    }

    #[test]
    fn malformed_md_yields_frontmatter_error() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let meetings = tmp.path().join("meetings");
        fs::create_dir_all(&meetings).expect("mkdir");
        fs::write(meetings.join("oops.md"), "no frontmatter at all").expect("seed");
        let issues = validate_vault(tmp.path());
        assert!(
            issues
                .iter()
                .any(|i| matches!(i, Issue::FrontmatterError { .. }))
        );
    }

    #[test]
    fn skips_md_bak_files_during_walk() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let meetings = tmp.path().join("meetings");
        fs::create_dir_all(&meetings).expect("mkdir");
        fs::write(meetings.join("stray.md.bak"), "garbage").expect("seed");
        let issues = validate_vault(tmp.path());
        // .bak walk is skipped; even though the content is garbage,
        // we don't emit a FrontmatterError for it.
        assert!(
            issues
                .iter()
                .all(|i| !matches!(i, Issue::FrontmatterError { .. }))
        );
    }

    #[test]
    fn duplicate_recording_is_flagged() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let recording = tmp.path().join("shared.m4a");
        let transcript = tmp.path().join("shared.jsonl");
        fs::write(&recording, b"fake").expect("seed");
        fs::write(&transcript, b"{}").expect("seed");

        let fm1 = fixture_frontmatter(recording.clone(), transcript.clone());
        let fm2 = fixture_frontmatter(recording.clone(), transcript.clone());
        write_note(tmp.path(), "alice", fm1);
        write_note(tmp.path(), "bob", fm2);

        let issues = validate_vault(tmp.path());
        let dup_count = issues
            .iter()
            .filter(|i| matches!(i, Issue::DuplicateRecording { .. }))
            .count();
        assert_eq!(dup_count, 1);
    }

    #[test]
    fn issue_serializes_with_kind_tag() {
        let issue = Issue::NoBackup {
            note: PathBuf::from("/x"),
        };
        let s = serde_json::to_string(&issue).expect("ser");
        assert!(s.contains(r#""kind":"no_backup""#));
    }
}
