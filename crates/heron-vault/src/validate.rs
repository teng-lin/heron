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
/// on `kind` rather than parse prose. Derives `Ord` so the validator
/// can sort the report deterministically without round-tripping
/// through JSON.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Issue {
    /// `read_note` failed; frontmatter is malformed or the YAML is
    /// invalid. The wrapped error message is the `Display` form of
    /// [`VaultError`].
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
    /// Two notes reference the same recording path.
    DuplicateRecording {
        recording: PathBuf,
        notes: Vec<PathBuf>,
    },
    /// Two notes reference the same transcript path. Same shape as
    /// `DuplicateRecording`; the variant split lets the consumer
    /// surface the right copy in the UI.
    DuplicateTranscript {
        transcript: PathBuf,
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
///
/// `recording` and `transcript` paths in frontmatter are interpreted
/// relative to `vault_root` when they aren't already absolute, so a
/// note that records `recording: meetings/2026/foo.m4a` checks the
/// right spot regardless of `cwd`.
pub fn validate_vault(vault_root: &Path) -> Vec<Issue> {
    let mut issues = Vec::new();
    let mut recordings: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    let mut transcripts: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();

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
                let recording_check = resolve_relative(vault_root, &fm.recording);
                let transcript_check = resolve_relative(vault_root, &fm.transcript);

                if !fm.recording.as_os_str().is_empty() && !recording_check.exists() {
                    issues.push(Issue::MissingRecording {
                        note: path.clone(),
                        recording: fm.recording.clone(),
                    });
                }
                if !fm.transcript.as_os_str().is_empty() && !transcript_check.exists() {
                    issues.push(Issue::MissingTranscript {
                        note: path.clone(),
                        transcript: fm.transcript.clone(),
                    });
                }
                recordings
                    .entry(fm.recording.clone())
                    .or_default()
                    .push(path.clone());
                transcripts
                    .entry(fm.transcript.clone())
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
    for (transcript, notes) in transcripts {
        if notes.len() > 1 && !transcript.as_os_str().is_empty() {
            issues.push(Issue::DuplicateTranscript { transcript, notes });
        }
    }

    // Stable order so test golden files don't churn on HashMap
    // iteration order. `Issue` derives `Ord`, so a direct sort is
    // both faster and more correct than the previous
    // serialize-and-sort-strings approach.
    issues.sort();
    issues
}

/// If `p` is absolute, return it unchanged; otherwise resolve it
/// against `vault_root`. This lets frontmatter store recording paths
/// either as absolute (the v1 default; `VaultWriter` writes them this
/// way) or as paths relative to the vault root (a common pattern in
/// hand-edited / imported notes).
fn resolve_relative(vault_root: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        vault_root.join(p)
    }
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
        let warnings: Vec<_> = issues
            .iter()
            .filter(|i| matches!(i, Issue::NoBackup { .. }))
            .collect();
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

    #[test]
    fn duplicate_transcript_is_flagged_separately_from_recording() {
        // Distinct recording paths but the *same* transcript path.
        // Earlier impl only checked recordings; now we should see a
        // DuplicateTranscript issue but no DuplicateRecording.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let recording_a = tmp.path().join("rec-a.m4a");
        let recording_b = tmp.path().join("rec-b.m4a");
        let shared_tr = tmp.path().join("shared.jsonl");
        fs::write(&recording_a, b"a").expect("seed");
        fs::write(&recording_b, b"b").expect("seed");
        fs::write(&shared_tr, b"{}").expect("seed");

        let fm_a = fixture_frontmatter(recording_a, shared_tr.clone());
        let fm_b = fixture_frontmatter(recording_b, shared_tr);
        write_note(tmp.path(), "alice", fm_a);
        write_note(tmp.path(), "bob", fm_b);

        let issues = validate_vault(tmp.path());
        let dup_tr = issues
            .iter()
            .filter(|i| matches!(i, Issue::DuplicateTranscript { .. }))
            .count();
        let dup_rec = issues
            .iter()
            .filter(|i| matches!(i, Issue::DuplicateRecording { .. }))
            .count();
        assert_eq!(dup_tr, 1, "expected one DuplicateTranscript: {issues:?}");
        assert_eq!(dup_rec, 0);
    }

    #[test]
    fn relative_recording_path_is_resolved_against_vault_root() {
        // Drop a real recording at <vault>/recordings/r.m4a and a
        // note whose frontmatter says `recording: recordings/r.m4a`.
        // The validator must NOT flag it as missing — earlier impl
        // checked Path::exists() relative to cwd.
        let tmp = tempfile::TempDir::new().expect("tmp");
        let rec_dir = tmp.path().join("recordings");
        fs::create_dir_all(&rec_dir).expect("mkdir");
        let abs_recording = rec_dir.join("r.m4a");
        fs::write(&abs_recording, b"r").expect("seed rec");

        let abs_transcript = rec_dir.join("r.jsonl");
        fs::write(&abs_transcript, b"{}").expect("seed tr");

        // Record relative paths in the frontmatter.
        let fm = fixture_frontmatter(
            PathBuf::from("recordings/r.m4a"),
            PathBuf::from("recordings/r.jsonl"),
        );
        write_note(tmp.path(), "rel", fm);
        let issues = validate_vault(tmp.path());
        assert!(
            !issues.iter().any(|i| matches!(
                i,
                Issue::MissingRecording { .. } | Issue::MissingTranscript { .. }
            )),
            "relative path must resolve against vault root: {issues:?}"
        );
    }
}
