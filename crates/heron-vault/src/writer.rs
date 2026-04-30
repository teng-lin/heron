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

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use heron_types::{ActionItem, Attendee, Frontmatter};
use serde::{Deserialize, Serialize};
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

/// Maximum slug length, in chars, before any date prefix or
/// collision-suffix or `.md` extension is appended. Tier 4 #19 pins
/// this at 100 so the surrounding `<YYYY-MM-DD>-<slug>-NN.md`
/// envelope (date prefix + 2-digit collision suffix + ".md" = 17
/// chars worst case) leaves comfortable headroom under APFS's 255-
/// byte filename limit even after `deunicode` transliteration
/// expands a single CJK char to several ASCII chars.
const MAX_SLUG_CHARS: usize = 100;

/// Writer-level slug-strategy enum mirrored by
/// `apps/desktop/src-tauri/src/settings.rs::FileNamingPattern`.
/// Owned by `heron-vault` because the slug logic itself lives here;
/// the desktop crate's enum exists for the Settings UI's TS wire
/// shape and converts to this one before it crosses the
/// `heron-cli::pipeline` → `heron_vault::VaultWriter` boundary.
///
/// Variants serialize as snake_case strings (`"id"`, `"date_slug"`,
/// `"slug"`) so the desktop wire format stays the source of truth.
///
/// **Backward compat**: `Id` writes `<uuid>.md`, the pre-Tier-1
/// convention. Default for new installs *and* the safe fallback when
/// the user's title slugifies to nothing — see [`finalize_with_pattern`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileNamingPattern {
    /// `<uuid>.md` — original convention; preserves existing-vault
    /// behavior on upgrade.
    #[default]
    Id,
    /// `<YYYY-MM-DD>-<slug>.md` — date-prefixed slug for chronological
    /// browsing.
    DateSlug,
    /// `<slug>.md` — slug only.
    Slug,
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

    /// Tier 4 #19: pattern-driven finalize. Picks the `.md` filename per
    /// `pattern`, applies the [`slugify`] pipeline (transliteration,
    /// reserved-char strip, length cap, word-boundary trim), and
    /// reserves a unique name in `meetings/` before writing the note +
    /// `.md.bak` companion.
    ///
    /// ## Pattern semantics
    ///
    /// - [`FileNamingPattern::Id`][] — `<meeting_id>.md`. The early-return
    ///   path documents backward compat — existing vaults full of
    ///   `<uuid>.md` notes keep getting `<uuid>.md` names because the
    ///   default value for Tier 1's `Settings::file_naming_pattern` is
    ///   `Id`. No slugify step runs; the title and date are unused.
    /// - [`FileNamingPattern::Slug`][] — `<slug>.md`.
    /// - [`FileNamingPattern::DateSlug`][] — `<YYYY-MM-DD>-<slug>.md`.
    ///
    /// ## Empty-slug fallback
    ///
    /// If `title` slugifies to nothing (whitespace-only, all reserved
    /// chars, transliteration drops every char), the writer falls back
    /// to `Id` semantics for *that* meeting and emits a `tracing::warn`.
    /// The user's pattern preference is unchanged on disk; the next
    /// session with a real title gets the configured pattern.
    ///
    /// ## Collision handling
    ///
    /// Two meetings whose titles produce the same slug (or two same-day
    /// runs of the same template under `DateSlug`) are disambiguated by
    /// appending `-2`, `-3`, … to the second / third arrival. The
    /// reservation uses [`OpenOptions::create_new`] inside the
    /// `meetings/` directory so the check-and-claim is one atomic
    /// syscall — closing the TOCTOU window an `exists()` pre-check
    /// would open. The collision suffix does **not** count toward
    /// [`MAX_SLUG_CHARS`].
    pub fn finalize_with_pattern(
        &self,
        pattern: FileNamingPattern,
        meeting_id: Uuid,
        title: &str,
        date: NaiveDate,
        frontmatter: &Frontmatter,
        body: &str,
    ) -> Result<PathBuf, VaultError> {
        let meetings_dir = self.vault_root.join("meetings");
        fs::create_dir_all(&meetings_dir)?;

        // `Id` is the early return: no slugify, no collision dance.
        // Two `<uuid>.md` writes for the same id either belong to the
        // same session (the legacy `finalize_session` re-entry covered
        // by `finalize_session_uses_bak_when_re_summarize_skipped_first`
        // test) or — vanishingly unlikely — to a uuid-v7 collision
        // we'd prefer to surface as an in-place overwrite rather than
        // mint a `<uuid>-2.md` that the read-side path-derived
        // `MeetingId` could never resurface.
        if pattern == FileNamingPattern::Id {
            let path = meetings_dir.join(format!("{}.md", meeting_id));
            let rendered = render_note(frontmatter, body)?;
            atomic_write(&path, rendered.as_bytes())?;
            atomic_write(&bak_path(&path), rendered.as_bytes())?;
            return Ok(path);
        }

        // Slug / DateSlug paths. Empty-slug → fall back to `Id` for
        // this meeting only, so a calendar-less ad-hoc capture still
        // produces a writable note instead of erroring out.
        let Some(slug) = slugify(title) else {
            tracing::warn!(
                meeting_id = %meeting_id,
                "title slugified to empty; falling back to Id pattern for this meeting",
            );
            return self.finalize_with_pattern(
                FileNamingPattern::Id,
                meeting_id,
                title,
                date,
                frontmatter,
                body,
            );
        };

        // Date prefix lives outside the slug-length budget per spec —
        // the 100-char cap is on the meaningful "title" portion only.
        let prefix = match pattern {
            FileNamingPattern::DateSlug => format!("{}-", date.format("%Y-%m-%d")),
            FileNamingPattern::Slug => String::new(),
            FileNamingPattern::Id => unreachable!("Id handled above"),
        };

        // Render BEFORE reserving the placeholder so a YAML serialize
        // error doesn't leak an empty `.md` into the vault. The
        // reservation is the last fallible step before the actual
        // write, narrowing the "crashed mid-write leaves an empty
        // file" window to the (rare) `atomic_write` failure path.
        let rendered = render_note(frontmatter, body)?;

        // Reserve a collision-free filename. `create_new(true)` is an
        // atomic check-and-claim so two writers racing to the same slug
        // don't both pick `<slug>.md` and clobber. The empty placeholder
        // is then atomically replaced by `atomic_write` (rename over
        // the path) — keeps the §19.4 "never half-written note"
        // invariant intact.
        let path = reserve_unique_path(&meetings_dir, &prefix, &slug)?;

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

/// Tier 4 #19 slug pipeline. Returns `None` when the input slugifies
/// to nothing — the caller (typically [`VaultWriter::finalize_with_pattern`])
/// uses that signal to fall back to the `Id` pattern for that meeting.
///
/// Pipeline (in order):
///
/// 1. **ASCII transliteration** via `deunicode`. `Café résumé 中文` →
///    `Cafe resume Zhong Wen `. The crate ships an internal lookup
///    table; output is always plain ASCII so subsequent ops can be
///    byte-oriented without UTF-8 boundary fuss.
/// 2. **Reserved-char strip**. `/`, `\`, `:`, `*`, `?`, `"`, `<`, `>`,
///    `|`, NUL, ASCII control bytes, and stray dots all get replaced
///    with a single space. Per the Windows-portable filename charset
///    (also a strict superset of macOS HFS+ / APFS forbidden bytes).
/// 3. **Lowercase + non-alphanumeric → `-`**. Spaces, punctuation, and
///    any leftover non-`[a-z0-9]` char collapse into single `-`s. The
///    output is the conventional URL-style slug shape every other
///    PKM tool produces, so cross-tool grep / wikilinks behave.
/// 4. **Trim leading/trailing `-`**. After the substitution above an
///    input like `". v2 ."` ends up `--v2--`; the trim sheds the
///    runners.
/// 5. **Length cap at [`MAX_SLUG_CHARS`] chars** (counted *after*
///    transliteration so a 200-char Chinese title doesn't sneak past
///    the cap). The cap is applied at the last `-` boundary inside
///    the budget when one exists, so we don't slice a word in half;
///    falls back to a hard cut + a final trim of trailing `-` when no
///    boundary fits.
///
/// Returns `Some(slug)` only when the result is non-empty. Empty title,
/// reserved-char-only title, or transliteration that produces only
/// whitespace all return `None`.
pub fn slugify(title: &str) -> Option<String> {
    // Step 1: transliterate to ASCII.
    let ascii = deunicode::deunicode(title);

    // Step 2 + 3: byte-walk; map reserved bytes / non-alnum to a marker
    // ('-'). Lowercasing happens here too so a single linear pass
    // produces the final character set.
    let mut buf = String::with_capacity(ascii.len());
    for ch in ascii.chars() {
        // ASCII control codes (incl. NUL, BEL, BS, TAB, LF, CR, etc.)
        // never belong in a filename. Reserved chars (`/\:*?"<>|`) get
        // the same treatment. Stray ASCII bytes outside `[A-Za-z0-9]`
        // also collapse to '-' — punctuation, dots, and spaces all
        // produce the same separator.
        if ch.is_ascii_alphanumeric() {
            buf.push(ch.to_ascii_lowercase());
        } else {
            buf.push('-');
        }
    }

    // Collapse runs of `-` and trim. Single linear pass: skip
    // consecutive `-` so `"a---b"` → `"a-b"`. Leading / trailing `-`
    // dropped at the end via `trim_matches`.
    let mut collapsed = String::with_capacity(buf.len());
    let mut prev_dash = false;
    for ch in buf.chars() {
        if ch == '-' {
            if !prev_dash {
                collapsed.push('-');
                prev_dash = true;
            }
        } else {
            collapsed.push(ch);
            prev_dash = false;
        }
    }
    let trimmed = collapsed.trim_matches('-');
    if trimmed.is_empty() {
        return None;
    }

    // Step 5: length cap. Budget is in chars; transliteration
    // produces ASCII so chars == bytes here.
    if trimmed.chars().count() <= MAX_SLUG_CHARS {
        return Some(trimmed.to_owned());
    }

    // Word-boundary trim. Walk back from the cap to the last `-` so
    // we don't slice a word; if no boundary lands inside the budget,
    // hard-cut at the cap and strip any trailing `-`.
    let chars: Vec<char> = trimmed.chars().collect();
    let mut cut = MAX_SLUG_CHARS;
    while cut > 0 && chars[cut - 1] != '-' {
        cut -= 1;
    }
    let cut = if cut == 0 {
        // No `-` inside the budget — keep the first MAX_SLUG_CHARS
        // chars, the resulting suffix-trim handles trailing dashes.
        MAX_SLUG_CHARS
    } else {
        // Stop at the `-` (don't include it).
        cut - 1
    };
    let head: String = chars[..cut].iter().collect();
    let head = head.trim_end_matches('-');
    if head.is_empty() {
        // Pathological: cap landed before the first non-`-` char.
        // Shouldn't be reachable (`trimmed` had no leading `-`), but
        // be defensive.
        return None;
    }
    Some(head.to_owned())
}

/// Reserve a collision-free filename inside `meetings_dir` for
/// `<prefix><slug><-N?>.md`. Returns the reserved path; an empty
/// placeholder file at that path is left in place, and the caller's
/// `atomic_write` rename overwrites it on the actual content write.
///
/// The reservation uses `OpenOptions::create_new(true)` so the "does
/// this name already exist?" check and the claim are one atomic kernel
/// operation. An `exists()` pre-check + `create()` would race two
/// concurrent writers picking the same `-N` suffix.
fn reserve_unique_path(
    meetings_dir: &Path,
    prefix: &str,
    slug: &str,
) -> Result<PathBuf, VaultError> {
    let mut suffix: u32 = 1;
    loop {
        let name = if suffix == 1 {
            format!("{prefix}{slug}.md")
        } else {
            format!("{prefix}{slug}-{suffix}.md")
        };
        let candidate = meetings_dir.join(&name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(_file) => {
                // Placeholder created. The subsequent `atomic_write`
                // tmp+rename overwrites the placeholder atomically;
                // dropping `_file` closes the descriptor, but the
                // entry stays in the directory until the rename
                // replaces it. A crash before `atomic_write` leaves
                // an empty `.md` in the vault, which the next run
                // skips by claiming `-2` — same lossy-on-crash
                // semantics the legacy `finalize_session` had.
                return Ok(candidate);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Collision. Bump the suffix and retry. The cap
                // protects against pathological cases (a directory
                // with thousands of identical-slug notes); 9999 is
                // far beyond any realistic vault.
                suffix = suffix.saturating_add(1);
                if suffix > 9999 {
                    return Err(VaultError::Io(std::io::Error::other(format!(
                        "exhausted 9999 collision suffixes for slug {slug:?}",
                    ))));
                }
            }
            Err(e) => return Err(VaultError::Io(e)),
        }
    }
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

    // ----- Tier 4 #19: file-naming pattern -----

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("date")
    }

    /// `Id` pattern produces `<uuid>.md`, ignoring title and date.
    /// The early-return path documents backward compat with the
    /// pre-Tier-1 `<uuid>.md` convention.
    #[test]
    fn finalize_with_pattern_id_writes_uuid_filename() {
        let tmp = TempDir::new().expect("tmp");
        let writer = VaultWriter::new(tmp.path());
        let id = Uuid::from_u128(0x0123_4567_89AB_CDEF_FEDC_BA98_7654_3210);
        let path = writer
            .finalize_with_pattern(
                FileNamingPattern::Id,
                id,
                "Acme sync",
                date(2026, 4, 24),
                &baseline(),
                "Body.\n",
            )
            .expect("finalize");

        assert!(
            path.ends_with(format!("meetings/{id}.md")),
            "path = {}",
            path.display(),
        );
        let (_, body) = read_note(&path).expect("read");
        assert_eq!(body, "Body.\n");
        // .md.bak companion is written alongside the .md.
        assert!(bak_path(&path).exists());
    }

    /// `Slug` pattern produces `<slug>.md` with no date prefix.
    #[test]
    fn finalize_with_pattern_slug_writes_slug_only() {
        let tmp = TempDir::new().expect("tmp");
        let writer = VaultWriter::new(tmp.path());
        let path = writer
            .finalize_with_pattern(
                FileNamingPattern::Slug,
                Uuid::nil(),
                "Acme weekly sync",
                date(2026, 4, 24),
                &baseline(),
                "Body.\n",
            )
            .expect("finalize");

        assert!(path.ends_with("meetings/acme-weekly-sync.md"));
    }

    /// `DateSlug` prefixes with `YYYY-MM-DD-`.
    #[test]
    fn finalize_with_pattern_date_slug_prefixes_date() {
        let tmp = TempDir::new().expect("tmp");
        let writer = VaultWriter::new(tmp.path());
        let path = writer
            .finalize_with_pattern(
                FileNamingPattern::DateSlug,
                Uuid::nil(),
                "Acme weekly sync",
                date(2026, 4, 24),
                &baseline(),
                "Body.\n",
            )
            .expect("finalize");

        assert!(path.ends_with("meetings/2026-04-24-acme-weekly-sync.md"));
    }

    /// Two meetings with the same slug get `-2`, `-3`, … appended in
    /// arrival order. The collision suffix does NOT count toward the
    /// 100-char `MAX_SLUG_CHARS` budget.
    #[test]
    fn finalize_with_pattern_appends_collision_suffix() {
        let tmp = TempDir::new().expect("tmp");
        let writer = VaultWriter::new(tmp.path());

        let p1 = writer
            .finalize_with_pattern(
                FileNamingPattern::Slug,
                Uuid::nil(),
                "Acme sync",
                date(2026, 4, 24),
                &baseline(),
                "v1\n",
            )
            .expect("p1");
        let p2 = writer
            .finalize_with_pattern(
                FileNamingPattern::Slug,
                Uuid::nil(),
                "Acme sync",
                date(2026, 4, 25),
                &baseline(),
                "v2\n",
            )
            .expect("p2");
        let p3 = writer
            .finalize_with_pattern(
                FileNamingPattern::Slug,
                Uuid::nil(),
                "Acme sync",
                date(2026, 4, 26),
                &baseline(),
                "v3\n",
            )
            .expect("p3");

        assert!(p1.ends_with("meetings/acme-sync.md"));
        assert!(p2.ends_with("meetings/acme-sync-2.md"));
        assert!(p3.ends_with("meetings/acme-sync-3.md"));
        // First file's content survived (the second/third didn't
        // overwrite it via the TOCTOU window).
        let (_, b1) = read_note(&p1).expect("read p1");
        assert_eq!(b1, "v1\n");
    }

    /// Non-ASCII titles transliterate to ASCII. `Café résumé` →
    /// `cafe-resume`. CJK scripts get the deunicode word-by-word
    /// transliteration so the slug is greppable.
    #[test]
    fn slugify_transliterates_non_ascii() {
        // Latin diacritics.
        assert_eq!(slugify("Café résumé"), Some("cafe-resume".to_owned()),);
        // CJK transliteration. Exact form depends on `deunicode`'s
        // table; assert the slug is non-empty pure-ASCII alphanumeric/-.
        let cjk = slugify("Café résumé 中文").expect("non-empty");
        assert!(
            cjk.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "slug should be ascii lowercase/-/digits, got {cjk:?}",
        );
        assert!(cjk.starts_with("cafe-resume"));
    }

    /// Reserved chars (`/\:*?"<>|`), control chars, and stray dots all
    /// collapse to `-`. The trim drops leading/trailing runs.
    #[test]
    fn slugify_strips_reserved_characters() {
        assert_eq!(
            slugify("foo/bar:baz*qux?"),
            Some("foo-bar-baz-qux".to_owned()),
        );
        assert_eq!(slugify("\\\"<test>\"|"), Some("test".to_owned()),);
        assert_eq!(slugify("..weird..title.."), Some("weird-title".to_owned()));
        // Embedded NUL / control bytes drop cleanly.
        assert_eq!(slugify("foo\0\x01bar"), Some("foo-bar".to_owned()),);
    }

    /// Slug is capped at 100 chars (post-transliteration). The cap is
    /// applied at the last `-` boundary inside the budget so words
    /// aren't sliced; trailing `-` from the cut is stripped.
    #[test]
    fn slugify_caps_at_max_chars_on_word_boundary() {
        // 30 'aaa' words separated by spaces → ~119 chars after slugify.
        // Should cap at 100 on a `-` boundary.
        let words: Vec<&str> = std::iter::repeat_n("aaa", 30).collect();
        let title = words.join(" ");
        let slug = slugify(&title).expect("non-empty");
        assert!(
            slug.chars().count() <= MAX_SLUG_CHARS,
            "slug len {} > cap {}; slug = {slug:?}",
            slug.chars().count(),
            MAX_SLUG_CHARS,
        );
        // Cap landed on a word boundary — no trailing `-`, no half-word.
        assert!(!slug.ends_with('-'));
        assert!(slug.starts_with("aaa-aaa"));
    }

    /// Pathological case: one long word with no `-` boundary. Falls
    /// back to a hard cut at MAX_SLUG_CHARS.
    #[test]
    fn slugify_hard_cuts_when_no_word_boundary_in_budget() {
        let title = "a".repeat(200);
        let slug = slugify(&title).expect("non-empty");
        assert_eq!(slug.chars().count(), MAX_SLUG_CHARS);
        assert!(slug.chars().all(|c| c == 'a'));
    }

    /// Empty-input or all-reserved-chars titles produce no slug; the
    /// caller must fall back to `Id`.
    #[test]
    fn slugify_returns_none_for_empty_input() {
        assert_eq!(slugify(""), None);
        assert_eq!(slugify("   \t\n"), None);
        assert_eq!(slugify("///\\\\:::"), None);
        assert_eq!(slugify("..."), None);
    }

    /// Empty-slug fallback: when the title slugifies to nothing, the
    /// writer falls back to `Id` for that meeting and writes
    /// `<uuid>.md` instead of erroring.
    #[test]
    fn finalize_with_pattern_falls_back_to_id_on_empty_slug() {
        let tmp = TempDir::new().expect("tmp");
        let writer = VaultWriter::new(tmp.path());
        let id = Uuid::from_u128(0xDEAD_BEEF_DEAD_BEEF_DEAD_BEEF_DEAD_BEEF);
        let path = writer
            .finalize_with_pattern(
                FileNamingPattern::DateSlug,
                id,
                "///",
                date(2026, 4, 24),
                &baseline(),
                "Body.\n",
            )
            .expect("finalize");

        assert!(
            path.ends_with(format!("meetings/{id}.md")),
            "fallback should produce <uuid>.md regardless of pattern; got {}",
            path.display(),
        );
    }

    /// Meeting type schema check on the .md.bak: pattern doesn't change
    /// the round-trip semantics. Cheap pin so a future writer change
    /// that forgets the .md.bak companion under the new code path
    /// fails loudly.
    #[test]
    fn finalize_with_pattern_round_trips_frontmatter() {
        let tmp = TempDir::new().expect("tmp");
        let writer = VaultWriter::new(tmp.path());
        let path = writer
            .finalize_with_pattern(
                FileNamingPattern::DateSlug,
                Uuid::nil(),
                "Acme",
                date(2026, 4, 24),
                &baseline(),
                "Body.\n",
            )
            .expect("finalize");
        let (fm, body) = read_note(&path).expect("read");
        assert_eq!(fm.duration_min, 47);
        assert_eq!(body, "Body.\n");
        let (bak_fm, _) = read_note(&bak_path(&path)).expect("read bak");
        assert_eq!(bak_fm.duration_min, fm.duration_min);
    }

    /// `Id` pattern with the same uuid is idempotent — the second
    /// write overwrites in place rather than picking `<uuid>-2.md`.
    /// This pins the early-return contract: collisions are only
    /// disambiguated for the slug-based patterns.
    #[test]
    fn finalize_with_pattern_id_is_idempotent_per_uuid() {
        let tmp = TempDir::new().expect("tmp");
        let writer = VaultWriter::new(tmp.path());
        let id = Uuid::from_u128(0x1234);
        let p1 = writer
            .finalize_with_pattern(
                FileNamingPattern::Id,
                id,
                "v1",
                date(2026, 4, 24),
                &baseline(),
                "v1\n",
            )
            .expect("p1");
        let p2 = writer
            .finalize_with_pattern(
                FileNamingPattern::Id,
                id,
                "v2",
                date(2026, 4, 24),
                &baseline(),
                "v2\n",
            )
            .expect("p2");
        assert_eq!(p1, p2);
        let (_, body) = read_note(&p1).expect("read");
        assert_eq!(body, "v2\n");
    }

    /// `FileNamingPattern` serializes to the snake_case strings the TS
    /// `Settings` interface in `lib/invoke.ts` declares. The desktop
    /// crate's `settings::FileNamingPattern` mirrors these — keep the
    /// vault-side wire format aligned so a future "remove the
    /// settings-side enum" refactor doesn't silently break the IPC
    /// contract.
    #[test]
    fn file_naming_pattern_serializes_to_snake_case() {
        assert_eq!(
            serde_json::to_value(FileNamingPattern::Id).expect("ser"),
            "id",
        );
        assert_eq!(
            serde_json::to_value(FileNamingPattern::DateSlug).expect("ser"),
            "date_slug",
        );
        assert_eq!(
            serde_json::to_value(FileNamingPattern::Slug).expect("ser"),
            "slug",
        );
    }
}
