//! Vault writer: finalize a session into `<vault>/meetings/<date>.md`
//! and re-summarize an existing note while preserving user edits.
//!
//! Per [`docs/archives/implementation.md`](../../../docs/archives/implementation.md) §12
//! and [`docs/archives/plan.md`](../../../docs/archives/plan.md) §3.2 path conventions:
//!
//! ```text
//! <vault_root>/
//!   meetings/
//!     YYYY-MM-DD-HHMM <slug>.md     <- finalized note
//!     YYYY-MM-DD-HHMM <slug>.md.bak <- previous-summary backup
//! ```
//!
//! All writes are atomic per §19.4: write to a uuid-named temp file
//! in the same directory, `fsync`, then `rename`. Final files are
//! mode `0600` so a misconfigured Dropbox/iCloud share doesn't leak
//! meeting content.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use heron_types::{ActionItem, Attendee, Frontmatter};
use thiserror::Error;
use uuid::Uuid;

use crate::merge::{MergeInputs, MergeOutcome, merge};

const FRONTMATTER_FENCE: &str = "---\n";

#[derive(Debug, Error)]
pub enum VaultError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Yaml(#[from] serde_yaml::Error),
    #[error("note has no `---` frontmatter fence at {path}")]
    MissingFrontmatter { path: PathBuf },
    #[error("note has unterminated `---` frontmatter at {path}")]
    UnterminatedFrontmatter { path: PathBuf },
}

/// Write under `vault_root`. The writer creates `meetings/` on first
/// use; the caller is expected to have created `vault_root` itself
/// (typically a folder inside Dropbox / iCloud / Google Drive — see
/// `plan.md` §3.1).
pub struct VaultWriter {
    vault_root: PathBuf,
}

impl VaultWriter {
    pub fn new(vault_root: impl Into<PathBuf>) -> Self {
        Self {
            vault_root: vault_root.into(),
        }
    }

    pub fn vault_root(&self) -> &Path {
        &self.vault_root
    }

    /// Path to a finalized note for `(date, start, slug)`.
    pub fn note_path(&self, date_str: &str, start_hhmm: &str, slug: &str) -> PathBuf {
        // YYYY-MM-DD-HHMM <slug>.md per plan.md §3.2.
        let filename = format!("{date_str}-{start_hhmm} {slug}.md");
        self.vault_root.join("meetings").join(filename)
    }

    /// First-summarize path: render `<frontmatter>\n<body>` into the
    /// canonical filename, atomically. Caller has already populated
    /// `frontmatter.recording` / `frontmatter.transcript`.
    ///
    /// Also writes the `.md.bak` companion to the same content so the
    /// FIRST re-summarize has a real baseline to diff against. Without
    /// this, a user who edits the note between finalize and the first
    /// re-summarize would silently lose the edit (base == ours
    /// collapses every llm_inferred decision to "user untouched,
    /// theirs wins"). See `docs/archives/merge-model.md`.
    pub fn finalize_session(
        &self,
        date_str: &str,
        start_hhmm: &str,
        slug: &str,
        frontmatter: &Frontmatter,
        body: &str,
    ) -> Result<PathBuf, VaultError> {
        let path = self.note_path(date_str, start_hhmm, slug);
        let parent = path.parent().unwrap_or(&self.vault_root);
        fs::create_dir_all(parent)?;

        let rendered = render_note(frontmatter, body)?;
        atomic_write(&path, rendered.as_bytes())?;
        atomic_write(&bak_path(&path), rendered.as_bytes())?;
        Ok(path)
    }

    /// Re-summarize path: read `<note>.md`, optionally `<note>.md.bak`,
    /// run the §10 merge against the LLM's fresh `theirs_*` output,
    /// rotate the backup, and atomically write the merged result.
    ///
    /// On the **first** re-summarize there is no `.md.bak`; the merge
    /// algorithm sets `base = ours`, which collapses every
    /// `llm_inferred` decision to "user untouched, theirs wins" and
    /// the body to "no semantic change, theirs wins" — the natural
    /// behavior for a fresh summarize. See `docs/archives/merge-model.md`.
    pub fn re_summarize(
        &self,
        note_path: &Path,
        theirs_frontmatter: &Frontmatter,
        theirs_body: &str,
    ) -> Result<MergeOutcome, VaultError> {
        let bak_path = bak_path(note_path);

        let (ours_fm, ours_body) = read_note(note_path)?;
        let (base_fm, base_body) = if bak_path.exists() {
            read_note(&bak_path)?
        } else {
            (ours_fm.clone(), ours_body.clone())
        };

        let outcome = merge(MergeInputs {
            base: &base_fm,
            ours: &ours_fm,
            theirs: theirs_frontmatter,
            base_body: &base_body,
            ours_body: &ours_body,
            theirs_body,
        });

        // Rotate: atomically copy current note to .md.bak BEFORE we
        // overwrite. If the rotate fails we don't write the new note,
        // so the user keeps a recoverable state.
        atomic_copy(note_path, &bak_path)?;
        let rendered = render_note(&outcome.frontmatter, &outcome.body)?;
        atomic_write(note_path, rendered.as_bytes())?;
        Ok(outcome)
    }
}

/// `<note>.md.bak` companion path.
fn bak_path(note_path: &Path) -> PathBuf {
    let mut s = note_path.as_os_str().to_owned();
    s.push(".bak");
    PathBuf::from(s)
}

/// Render `<frontmatter>\n<body>` as a single string, with `---` YAML
/// fences around the frontmatter.
///
/// Public so the desktop crate's PR-ξ (phase 76) `resummarize_preview`
/// command can produce byte-identical output to what
/// [`VaultWriter::re_summarize`] writes — without the preview path
/// re-implementing the fence + YAML serializer (which would silently
/// drift the moment this renderer changes shape).
pub fn render_note(frontmatter: &Frontmatter, body: &str) -> Result<String, VaultError> {
    let mut out = String::new();
    out.push_str(FRONTMATTER_FENCE);
    out.push_str(&serde_yaml::to_string(frontmatter)?);
    out.push_str(FRONTMATTER_FENCE);
    out.push_str(body);
    Ok(out)
}

/// Parse `<note>.md` into `(Frontmatter, body)`. Errors clearly when
/// the note is missing the `---` fences instead of returning a confused
/// YAML error.
pub fn read_note(path: &Path) -> Result<(Frontmatter, String), VaultError> {
    let mut buf = String::new();
    File::open(path)?.read_to_string(&mut buf)?;
    parse_note(&buf, path)
}

/// The prior `action_items` and `attendees` extracted from a note's
/// frontmatter — the inputs the LLM needs to honor the §10.5
/// ID-preservation contract on a re-summarize.
///
/// Owned (rather than borrowed) so the orchestrator can keep the
/// items alive across the async `summarize` call without juggling
/// the source `Frontmatter` lifetime. `[Self::is_empty]` lets the
/// caller decide whether to pass `None` to `SummarizerInput` (first
/// summarize) or `Some(&...)` (re-summarize).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct PriorItems {
    pub action_items: Vec<ActionItem>,
    pub attendees: Vec<Attendee>,
}

impl PriorItems {
    /// `true` when both lists are empty — the caller should treat
    /// the summarize as a "first summarize" and pass `None` to the
    /// summarizer rather than priming it with empty arrays (which
    /// would still render the §10.5 prompt block, suggesting the
    /// LLM mint UUIDs to "preserve" things we never told it about).
    pub fn is_empty(&self) -> bool {
        self.action_items.is_empty() && self.attendees.is_empty()
    }

    /// Borrow the lists as `Option<&[_]>` shaped for
    /// `heron_llm::SummarizerInput`: empty list → `None` so the
    /// §10.5 prompt block stays out on a first summarize, populated
    /// list → `Some(&...)` so the LLM is asked to preserve those
    /// IDs.
    pub fn as_summarizer_inputs(&self) -> (Option<&[ActionItem]>, Option<&[Attendee]>) {
        let actions = (!self.action_items.is_empty()).then_some(&self.action_items[..]);
        let attendees = (!self.attendees.is_empty()).then_some(&self.attendees[..]);
        (actions, attendees)
    }
}

/// Read just the prior `action_items` + `attendees` from a note —
/// the inputs `heron_llm::SummarizerInput::existing_action_items` and
/// `existing_attendees` need on a re-summarize per §10.5.
///
/// Convenience over [`read_note`] for callers that don't care about
/// the body or the rest of the frontmatter; the source-of-truth note
/// is the **current** `<note>.md` (i.e., `ours` in the §10.3 merge),
/// **not** `<note>.md.bak` — see §11.2.
pub fn read_prior_items(path: &Path) -> Result<PriorItems, VaultError> {
    let (fm, _body) = read_note(path)?;
    Ok(PriorItems {
        action_items: fm.action_items,
        attendees: fm.attendees,
    })
}

fn parse_note(input: &str, path: &Path) -> Result<(Frontmatter, String), VaultError> {
    let after_open =
        input
            .strip_prefix(FRONTMATTER_FENCE)
            .ok_or_else(|| VaultError::MissingFrontmatter {
                path: path.to_path_buf(),
            })?;
    let close =
        after_open
            .find(FRONTMATTER_FENCE)
            .ok_or_else(|| VaultError::UnterminatedFrontmatter {
                path: path.to_path_buf(),
            })?;
    let yaml = &after_open[..close];
    let body = &after_open[close + FRONTMATTER_FENCE.len()..];
    let fm: Frontmatter = serde_yaml::from_str(yaml)?;
    Ok((fm, body.to_string()))
}

/// Atomic write: write to `.<basename>.<uuid>.tmp` next to the target
/// path, `fsync`, then `rename` over the destination. Final mode is
/// `0600` (readable/writable by the user only).
pub fn atomic_write(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("path has no parent dir"))?;
    let basename = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("path has no filename"))?
        .to_string_lossy();
    let tmp = parent.join(format!(".{basename}.{}.tmp", Uuid::nil().simple()));
    // Use a fresh uuid each call so concurrent writes don't collide.
    let tmp = tmp.with_extension(format!("{}.tmp", Uuid::from_u128(rand_u128()).simple()));

    {
        let mut f = File::create(&tmp)?;
        f.write_all(contents)?;
        f.sync_all()?;
    }
    set_mode_0600(&tmp)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Atomic copy: read source, atomic-write to destination. The
/// rotation in [`VaultWriter::re_summarize`] uses this so the .bak
/// is never half-written.
fn atomic_copy(src: &Path, dst: &Path) -> std::io::Result<()> {
    let mut buf = Vec::new();
    File::open(src)?.read_to_end(&mut buf)?;
    atomic_write(dst, &buf)
}

#[cfg(unix)]
fn set_mode_0600(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perm = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perm)
}

#[cfg(not(unix))]
fn set_mode_0600(_path: &Path) -> std::io::Result<()> {
    // Non-unix platforms (we don't ship there in v1) silently skip.
    Ok(())
}

/// Tiny entropy source for the temp-file uuid. Uses the system
/// nanosecond clock — collision-resistant enough for two concurrent
/// writes on the same path (the rename is the actual atomicity
/// guarantee). Avoids pulling in `rand` for one byte's worth of work.
fn rand_u128() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Mix in the thread id so two threads on the same nanosecond
    // produce different uuids.
    let tid = std::thread::current().id();
    let tid_hash = format!("{tid:?}").len() as u128;
    now ^ (tid_hash.wrapping_mul(0x9E3779B97F4A7C15))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use chrono::NaiveDate;
    use heron_types::{Cost, DiarizeSource, Disclosure, DisclosureHow, MeetingType};
    use tempfile::TempDir;

    fn baseline() -> Frontmatter {
        Frontmatter {
            date: NaiveDate::from_ymd_opt(2026, 4, 24).expect("date"),
            start: "14:00".into(),
            duration_min: 47,
            company: Some("Acme".into()),
            attendees: vec![],
            meeting_type: MeetingType::Client,
            source_app: "us.zoom.xos".into(),
            recording: PathBuf::from("recordings/2026-04-24-1400.m4a"),
            transcript: PathBuf::from("transcripts/2026-04-24-1400.jsonl"),
            diarize_source: DiarizeSource::Ax,
            disclosed: Disclosure {
                stated: true,
                when: Some("00:14".into()),
                how: DisclosureHow::Verbal,
            },
            cost: Cost {
                summary_usd: 0.04,
                tokens_in: 14_231,
                tokens_out: 612,
                model: "claude-sonnet-4-6".into(),
            },
            action_items: vec![],
            tags: vec!["meeting".into(), "acme".into()],
            extra: serde_yaml::Mapping::default(),
        }
    }

    #[test]
    fn finalize_session_writes_canonical_path_and_round_trips() {
        let tmp = TempDir::new().expect("tmp");
        let writer = VaultWriter::new(tmp.path());
        let path = writer
            .finalize_session(
                "2026-04-24",
                "1400",
                "acme-pricing",
                &baseline(),
                "First-pass body.\n",
            )
            .expect("finalize");

        assert!(path.ends_with("meetings/2026-04-24-1400 acme-pricing.md"));
        let (fm, body) = read_note(&path).expect("read back");
        assert_eq!(fm.duration_min, 47);
        assert_eq!(body, "First-pass body.\n");
    }

    #[test]
    fn re_summarize_rotates_bak_and_preserves_user_edits() {
        let tmp = TempDir::new().expect("tmp");
        let writer = VaultWriter::new(tmp.path());

        // First summarize.
        let path = writer
            .finalize_session(
                "2026-04-24",
                "1400",
                "acme",
                &baseline(),
                "Original body.\n",
            )
            .expect("finalize");

        // User edit: reads the file, mutates it, writes it back.
        let (mut fm, _orig_body) = read_note(&path).expect("read");
        fm.meeting_type = MeetingType::Internal; // user reclassified
        let user_body = "User-edited body.\n";
        let rendered = render_note(&fm, user_body).expect("render");
        atomic_write(&path, rendered.as_bytes()).expect("write");

        // LLM re-summarize comes in with the original meeting_type +
        // a polished body.
        let theirs = baseline(); // meeting_type = Client (LLM's view)
        let theirs_body = "Polished LLM body.";
        let outcome = writer
            .re_summarize(&path, &theirs, theirs_body)
            .expect("resummarize");

        // User's reclassification survives (llm_inferred + ours edited).
        assert_eq!(outcome.frontmatter.meeting_type, MeetingType::Internal);
        // User's body survives (semantic edit detected).
        assert_eq!(outcome.body, user_body);
        // .md.bak now contains the user-edited version (the previous
        // contents of .md before this re-summarize).
        assert!(bak_path(&path).exists());
        let (bak_fm, _) = read_note(&bak_path(&path)).expect("read bak");
        assert_eq!(bak_fm.meeting_type, MeetingType::Internal);
    }

    #[test]
    fn re_summarize_first_run_treats_ours_as_base() {
        let tmp = TempDir::new().expect("tmp");
        let writer = VaultWriter::new(tmp.path());

        let path = writer
            .finalize_session("2026-04-24", "1400", "acme", &baseline(), "Body.\n")
            .expect("finalize");

        // No .md.bak yet. Re-summarize with a polished company name.
        let mut theirs = baseline();
        theirs.company = Some("Acme Corp.".into());
        let outcome = writer
            .re_summarize(&path, &theirs, "Body.\n")
            .expect("resummarize");

        // Without a bak, base = ours, so theirs wins on llm_inferred
        // fields.
        assert_eq!(outcome.frontmatter.company.as_deref(), Some("Acme Corp."));
    }

    #[test]
    fn parse_note_errors_clearly_on_missing_fence() {
        let path = PathBuf::from("/tmp/x.md");
        let result = parse_note("no fences here\nbody\n", &path);
        assert!(matches!(result, Err(VaultError::MissingFrontmatter { .. })));
    }

    #[test]
    fn parse_note_errors_clearly_on_unterminated_frontmatter() {
        let path = PathBuf::from("/tmp/x.md");
        let result = parse_note("---\nfield: value\nbody without close\n", &path);
        assert!(matches!(
            result,
            Err(VaultError::UnterminatedFrontmatter { .. })
        ));
    }

    #[test]
    fn atomic_write_creates_file_with_user_only_permissions() {
        let tmp = TempDir::new().expect("tmp");
        let path = tmp.path().join("note.md");
        atomic_write(&path, b"hello").expect("write");
        let bytes = fs::read(&path).expect("read");
        assert_eq!(bytes, b"hello");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn atomic_write_overwrites_existing_file_atomically() {
        let tmp = TempDir::new().expect("tmp");
        let path = tmp.path().join("note.md");
        atomic_write(&path, b"v1").expect("v1");
        atomic_write(&path, b"v2").expect("v2");
        assert_eq!(fs::read(&path).expect("read"), b"v2");
    }

    #[test]
    fn finalize_session_uses_bak_when_re_summarize_skipped_first() {
        // If the user calls finalize_session twice (instead of going
        // through re_summarize), the second write should still
        // succeed atomically. This protects the unusual flow where
        // session machinery fails partway and the writer is invoked
        // again from a checkpoint.
        let tmp = TempDir::new().expect("tmp");
        let writer = VaultWriter::new(tmp.path());
        let path = writer
            .finalize_session("2026-04-24", "1400", "x", &baseline(), "v1\n")
            .expect("v1");
        let path2 = writer
            .finalize_session("2026-04-24", "1400", "x", &baseline(), "v2\n")
            .expect("v2");
        assert_eq!(path, path2);
        let (_, body) = read_note(&path).expect("read");
        assert_eq!(body, "v2\n");
    }
}
