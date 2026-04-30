//! Re-summarize + `.md.bak` rollback backend per §15 PR-ε (phase 67),
//! plus the §15 v1.1 diff-modal preview surface added by PR-ξ (phase 76).
//!
//! Wraps [`heron_cli::summarize::re_summarize_in_vault`] for the Tauri
//! Re-summarize button and adds two companion commands —
//! [`check_backup`] and [`restore_backup`] — that surface the
//! `<note>.md.bak` rotation [`heron_vault::VaultWriter::re_summarize`]
//! creates so the Review UI can offer a one-click rollback. The
//! [`resummarize_preview`] entry point added in PR-ξ runs the full
//! summarize + §10.3 merge + render pipeline **without** rotating or
//! writing, so the renderer can show a side-by-side diff modal before
//! the user commits.
//!
//! ## Why a module separate from `notes.rs`?
//!
//! `notes.rs` is the read/write/list surface for plain markdown; it
//! stays free of LLM dependencies so its tests run without the
//! `heron-cli` + `heron-llm` build cost. The summarizer-driven flow
//! lives here so the `notes` shape remains lightweight.
//!
//! ## Path policy
//!
//! Every command routes through [`crate::notes::resolve_note_path`] /
//! [`crate::notes::resolve_vault_path`] so the same basename-only,
//! canonicalize-then-containment-check guard `notes::read_note` and
//! friends use protects this surface too. `.md.bak` lives next to the
//! note inside the vault — the writer never escapes it.
//!
//! ## Errors
//!
//! All entry points return `Result<_, String>` so the React side
//! can render the failure as a Sonner toast verbatim, matching the
//! `notes::read_note` / `settings::*` convention.

use std::path::{Path, PathBuf};

use serde::Serialize;
use tokio::fs;

use heron_cli::session::{Orchestrator, SessionConfig, SessionError};
use heron_cli::summarize::re_summarize_in_vault_with_persona;
use heron_llm::{
    Preference, Summarizer, parse_settings_backend, select_summarizer_with_user_choice,
};
use heron_types::{Frontmatter, Persona};
use heron_vault::{MergeInputs, merge, read_note as vault_read_note, render_note};

use crate::default_settings_path;
use crate::keychain_resolver::EnvThenKeychainResolver;
use crate::notes::{
    canonicalize_meetings_within, meetings_dir, resolve_note_path, resolve_vault_path,
    validated_basename,
};
use crate::settings::read_settings;

/// Resolve the `<vault>/meetings/<basename>.md.bak` path the renderer
/// is allowed to touch. Validation mirrors [`crate::notes::resolve_note_path`]
/// (basename allowlist, canonicalize the vault, ensure containment) —
/// only the file extension differs. `<basename>` strips the `mtg_`
/// wire-form prefix so the `.bak` lives next to the bare-uuid `.md`
/// the vault writer rotates.
async fn resolve_bak_path(vault: &Path, session_id: &str) -> Result<PathBuf, String> {
    let basename = validated_basename(session_id)?;
    let canonical_vault = resolve_vault_path(vault).await?;
    let meetings = meetings_dir(&canonical_vault);
    // Canonicalize `meetings/` and confirm it's still inside the
    // vault — otherwise a symlinked `meetings/` would let a
    // `.md.bak` read or delete escape. If `meetings/` is missing
    // (pre-capture state), the `.md.bak` can't exist either; the
    // lexical path under `canonical_vault` is safe because the
    // basename is validated, and the subsequent metadata / read call
    // surfaces NotFound which `check_backup` / `restore_backup`
    // translate into "no backup" / a clear error.
    let parent = canonicalize_meetings_within(&meetings, &canonical_vault)
        .await?
        .unwrap_or(meetings);
    Ok(parent.join(format!("{basename}.md.bak")))
}

/// Metadata about a `<note>.md.bak` file the Review UI can show next
/// to the editor as a "Backup from <timestamp>" pill.
///
/// `created_at` is an ISO-8601 / RFC-3339 string in the system local
/// timezone offset — the renderer formats it for display via
/// `Intl.DateTimeFormat`. We surface the modification time rather than
/// inode-creation time because macOS / Linux disagree on what "created"
/// means for files renamed-into-place by `atomic_write`, and `mtime`
/// is what every POSIX tool reports.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BackupInfo {
    pub created_at: String,
}

/// Build a real summarizer + run [`Orchestrator::re_summarize_note`]
/// against `<vault>/<session_id>.md`, returning the rendered post-merge
/// note (frontmatter + body) the renderer should display.
///
/// **Read-only**: the helper reads the current note, the optional
/// `.md.bak`, and the transcript, but never writes to disk and never
/// rotates the backup. [`resummarize_preview`] is the only caller —
/// the apply path stays on [`re_summarize_in_vault`] because that
/// function bundles the rotate + atomic_write through
/// `heron_vault`'s private `atomic_copy` helper, which we'd otherwise
/// have to duplicate at the desktop layer. Going through
/// [`heron_vault::render_note`] + [`heron_vault::merge`] keeps the
/// preview byte-identical to what `re_summarize_in_vault` would
/// write modulo non-determinism in the LLM's output (the LLM is
/// re-invoked on Apply; see PR-ξ for that trade-off rationale).
///
/// The merge runs against `(base = .md.bak ?? ours, ours = current,
/// theirs = LLM output)` per §10.3 so the rendered preview reflects the
/// same four-bucket ownership model the writer enforces — user-edited
/// fields survive, untouched fields refresh.
async fn summarize_body(vault: &Path, session_id: &str) -> Result<String, String> {
    let note_path = resolve_note_path(vault, session_id, true).await?;
    let canonical_vault = resolve_vault_path(vault).await?;

    // Tier 4 #18 / #21: read the user's persona + strip-names toggle
    // from `Settings` and thread them through `run_summarize`. A
    // missing settings.json (first-run state) yields `Settings::default()`
    // — persona empty, strip_names = false — so the prompt path stays
    // byte-identical to pre-Tier-4 unless the user has explicitly
    // opted in via the Settings pane.
    //
    // Read first so the same snapshot drives both backend selection
    // (Tier 2 #14: `settings.llm_backend` honored as the user's
    // explicit choice) and the persona / strip-names knobs below — a
    // settings change that races us is fine in either direction
    // because each field is independently consistent on disk.
    let settings =
        read_settings(&default_settings_path()).map_err(|e| format!("read settings: {e}"))?;

    // Build a real summarizer honoring `settings.llm_backend` when the
    // user picked one explicitly; fall back to `Preference::Auto`
    // otherwise. PR-μ / phase 74: the desktop crate threads its
    // `EnvThenKeychainResolver` through so a user who only pasted
    // their key into Settings → Summarizer (PR-θ) gets the API
    // backend selected — env var still wins when both are set so CI
    // / docker workflows are unaffected. Errors surface verbatim so
    // the Review UI can render an actionable toast ("set
    // ANTHROPIC_API_KEY or paste a key in Settings → Summarizer",
    // "install claude-code", etc).
    let resolver = EnvThenKeychainResolver::new();
    let user_choice = parse_settings_backend(&settings.llm_backend);
    let (summarizer, _backend, _reason) =
        select_summarizer_with_user_choice(user_choice, Preference::Auto, &resolver)
            .map_err(|e| format!("LLM backend: {e}"))?;

    let (ours_fm, ours_body) =
        vault_read_note(&note_path).map_err(|e| format!("read {}: {}", note_path.display(), e))?;

    // `Path::join` returns the pushed path unchanged when it's
    // absolute, so this single call covers both the vault-relative
    // (default) and hand-edited-absolute (escape hatch) cases — same
    // policy `heron_cli::summarize::re_summarize_in_vault` applies.
    let transcript = canonical_vault.join(&ours_fm.transcript);
    if !transcript.exists() {
        return Err(format!(
            "transcript {} not found (frontmatter.transcript = {}); \
             the note's recorded transcript path no longer resolves on disk",
            transcript.display(),
            ours_fm.transcript.display()
        ));
    }

    let persona = persona_from_settings(&settings);
    let output = run_summarize(
        summarizer.as_ref(),
        &canonical_vault,
        &note_path,
        ours_fm.meeting_type,
        &transcript,
        persona.as_ref(),
        settings.strip_names_before_summarization,
    )
    .await?;

    // Build `theirs_frontmatter` for the §10.3 merge identical to
    // `re_summarize_in_vault`'s overlay: keep heron-managed fields
    // (date / start / duration / source_app / recording / transcript /
    // diarize_source / disclosed / extra) intact from the current
    // note, and overlay the fields the LLM is authoritative for.
    let theirs_fm = Frontmatter {
        company: output.company,
        meeting_type: output.meeting_type,
        tags: output.tags,
        action_items: output.action_items,
        attendees: output.attendees,
        cost: output.cost,
        ..ours_fm.clone()
    };

    // Read the optional `.md.bak` baseline for the 3-way merge. When
    // none exists (first re-summarize) `base = ours` per the merge-
    // model contract — every llm_inferred decision then collapses to
    // "user untouched, theirs wins", matching what the vault writer
    // does on the same input.
    let bak_path = resolve_bak_path(vault, session_id).await?;
    let (base_fm, base_body) = if bak_path.exists() {
        vault_read_note(&bak_path).map_err(|e| format!("read {}: {}", bak_path.display(), e))?
    } else {
        (ours_fm.clone(), ours_body.clone())
    };

    let outcome = merge(MergeInputs {
        base: &base_fm,
        ours: &ours_fm,
        theirs: &theirs_fm,
        base_body: &base_body,
        ours_body: &ours_body,
        theirs_body: &output.body,
    });

    // Routing through `heron_vault::render_note` (the same renderer
    // `VaultWriter::re_summarize` calls) guarantees the preview the
    // user approves in the diff modal is byte-identical to what
    // [`resummarize`] eventually writes — even if a future heron-vault
    // change tweaks the YAML serializer or fence convention.
    render_note(&outcome.frontmatter, &outcome.body).map_err(|e| format!("render preview: {e}"))
}

/// Invoke the orchestrator's `re_summarize_note` against a real
/// summarizer, returning the LLM's `SummarizerOutput`.
///
/// Split out so the [`resummarize_preview_does_not_touch_disk`] test
/// could in principle stub the call without faking the whole vault
/// state. Today the production path drives this with a real
/// `select_summarizer` instance — the helper is a thin shim around
/// [`Orchestrator::re_summarize_note`] that fixes the `SessionConfig`
/// fields the orchestrator doesn't read on a re-summarize (cache_dir,
/// session_id) to inert defaults.
async fn run_summarize(
    summarizer: &dyn Summarizer,
    vault_root: &Path,
    note_path: &Path,
    meeting_type: heron_types::MeetingType,
    transcript: &Path,
    persona: Option<&Persona>,
    strip_names: bool,
) -> Result<heron_llm::SummarizerOutput, String> {
    let cfg = SessionConfig {
        session_id: uuid::Uuid::nil(),
        target_bundle_id: String::new(),
        cache_dir: PathBuf::new(),
        vault_root: vault_root.to_path_buf(),
        stt_backend_name: "sherpa".into(),
        // Re-summarize is a vault-side operation; STT (and therefore
        // hotwords) is never invoked, so the empty default is fine.
        hotwords: Vec::new(),
        llm_preference: Preference::Auto,
        // Re-summarize is a vault-side operation; pre-meeting context
        // never participates here.
        pre_meeting_briefing: None,
        // Re-summarize never starts a live capture, so there are no
        // AX events to bridge onto the bus.
        event_bus: None,
        // Re-summarize doesn't finalize a fresh note; pattern is unread.
        file_naming_pattern: heron_vault::FileNamingPattern::Id,
        // Tier 4 #18 / #21: forward the user's persona + strip-names
        // toggle into the LLM call so a desktop-driven re-summarize
        // honors Settings the same way a fresh capture does.
        persona: persona.cloned(),
        strip_names,
        // Re-summarize is a vault-only op — no live capture pipeline,
        // no pause flag to honor.
        pause_flag: None,
    };
    let orch = Orchestrator::new(cfg);
    orch.re_summarize_note(summarizer, note_path, meeting_type, transcript)
        .await
        .map_err(|e: SessionError| format!("re-summarize: {e}"))
}

/// Convert `Settings.persona` into the `Option<Persona>` the LLM call
/// expects: an "all empty strings" persona is the no-config sentinel,
/// so collapse it to `None` at the boundary so callers don't have to
/// repeat the `is_empty` check. Pinned by tests in `heron_types`.
fn persona_from_settings(settings: &crate::settings::Settings) -> Option<Persona> {
    if settings.persona.is_empty() {
        None
    } else {
        Some(settings.persona.clone())
    }
}

/// Re-summarize `<vault>/<session_id>.md` in place, returning the new
/// body the Review editor should render.
///
/// Wires [`heron_llm::select_summarizer(Preference::Auto)`] →
/// [`heron_cli::summarize::re_summarize_in_vault`]. The vault writer
/// rotates the prior body into `<id>.md.bak` *before* overwriting the
/// note, which is what makes the Restore button a true rollback (the
/// `.md.bak` has the user's pre-resummarize content even if the LLM
/// output later turns out worse).
///
/// Returns the rendered note (frontmatter + body) so the renderer can
/// re-mount the editor against the post-merge text. The frontend
/// strips the `---` frontmatter fences when displaying — we hand back
/// the full rendered output so the round-trip with `heron_read_note`
/// (which the frontend already uses) stays consistent.
pub async fn resummarize(vault: &Path, session_id: &str) -> Result<String, String> {
    let note_path = resolve_note_path(vault, session_id, true).await?;
    let canonical_vault = resolve_vault_path(vault).await?;

    // Tier 4 #18 / #21: read settings so the apply path uses the same
    // persona + strip-names knobs the preview path
    // (`resummarize_preview`) does. Without this, Apply would silently
    // use a different prompt than the Preview the user just clicked.
    //
    // Tier 2 #14: also drives the user's explicit `llm_backend` choice
    // into `select_summarizer_with_user_choice` so Apply picks the
    // same backend the Preview did — the snapshot is read once here
    // and reused for both decisions.
    let settings =
        read_settings(&default_settings_path()).map_err(|e| format!("read settings: {e}"))?;
    let persona = persona_from_settings(&settings);

    // Build a real summarizer honoring `settings.llm_backend` when the
    // user picked one explicitly; fall back to `Preference::Auto`
    // otherwise. PR-μ / phase 74: the desktop crate threads its
    // `EnvThenKeychainResolver` through so a user who only pasted
    // their key into Settings → Summarizer (PR-θ) gets the API
    // backend selected — env var still wins when both are set so CI
    // / docker workflows are unaffected. Errors surface verbatim so
    // the Review UI can render an actionable toast.
    let resolver = EnvThenKeychainResolver::new();
    let user_choice = parse_settings_backend(&settings.llm_backend);
    let (summarizer, _backend, _reason) =
        select_summarizer_with_user_choice(user_choice, Preference::Auto, &resolver)
            .map_err(|e| format!("LLM backend: {e}"))?;

    // `re_summarize_in_vault_with_persona` does the work:
    // 1. Reads the note's frontmatter to find the transcript path.
    // 2. Calls `Orchestrator::re_summarize_note` (§10.5 ID preservation).
    // 3. Calls `VaultWriter::re_summarize` (§10.3 merge + .md.bak rotation).
    re_summarize_in_vault_with_persona(
        summarizer.as_ref(),
        &canonical_vault,
        &note_path,
        persona.as_ref(),
        settings.strip_names_before_summarization,
    )
    .await
    .map_err(|e| format!("re-summarize: {e}"))?;

    // Read the note back so the frontend doesn't need a follow-up
    // `heron_read_note` call.
    fs::read_to_string(&note_path)
        .await
        .map_err(|e| format!("read {}: {}", note_path.display(), e))
}

/// Compute the post-merge note **without** rotating `.md.bak` or
/// writing to disk — the data the diff modal's right pane shows.
///
/// PR-ξ (phase 76) lifts the diff-view-before-accepting checkbox from
/// `plan.md` §15 v1.1 forward. The flow is:
///
/// 1. Renderer fires [`resummarize_preview`] from the confirmation
///    dialog's onConfirm.
/// 2. Spinner shows while the summarizer runs (5–30s on a real LLM).
/// 3. Modal renders the current `<id>.md` body on the left and the
///    string returned here on the right.
/// 4. On Apply: renderer fires [`resummarize`] which rotates + writes.
/// 5. On Cancel: renderer drops the preview; nothing changes on disk.
///
/// Disk safety: this command **must not** write `<id>.md`,
/// **must not** create `<id>.md.bak`, and **must not** mutate any
/// existing file under the vault. The `resummarize_preview_does_not_touch_disk`
/// test anchors all three invariants.
pub async fn resummarize_preview(vault: &Path, session_id: &str) -> Result<String, String> {
    summarize_body(vault, session_id).await
}

/// Return the `.md.bak`'s modification time as an ISO-8601 string,
/// or `None` when no backup exists.
///
/// "No backup" is `Ok(None)` rather than an error: the renderer mounts
/// the Review page on every navigation and a missing `.md.bak` is the
/// common (steady-state) case. Surfacing it as `Err` would force every
/// caller to pattern-match on a substring of the error message.
pub async fn check_backup(vault: &Path, session_id: &str) -> Result<Option<BackupInfo>, String> {
    let bak = resolve_bak_path(vault, session_id).await?;
    match fs::metadata(&bak).await {
        Ok(meta) => {
            let mtime = meta
                .modified()
                .map_err(|e| format!("mtime {}: {}", bak.display(), e))?;
            let dt: chrono::DateTime<chrono::Local> = mtime.into();
            Ok(Some(BackupInfo {
                created_at: dt.to_rfc3339(),
            }))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("stat {}: {}", bak.display(), e)),
    }
}

/// Restore `<vault>/<session_id>.md` from `<vault>/<session_id>.md.bak`,
/// then delete the `.bak` so the Review UI's pill goes away.
///
/// Returns the restored body so the editor can re-mount immediately
/// (mirrors [`resummarize`]'s "return-the-new-body" contract). Atomic
/// over-write goes through [`crate::notes::write_note_atomic`] which
/// uses the same temp-file + fsync + rename recipe everything else in
/// the desktop crate writes with.
///
/// On a successful overwrite we best-effort delete the `.bak`. If the
/// delete fails the next render of the page will still see the old
/// `.bak` and offer a Restore again — idempotent rollback.
pub async fn restore_backup(vault: &Path, session_id: &str) -> Result<String, String> {
    let bak = resolve_bak_path(vault, session_id).await?;
    // Single read serves as both the existence check and the data
    // fetch; we map ENOENT to a clear "does not exist" message so a
    // double-click race surfaces a useful toast rather than the raw
    // OS error string. Skipping the upfront `try_exists` also closes
    // a small TOCTOU window where the file could vanish between the
    // probe and the read.
    let body = fs::read_to_string(&bak).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            format!("{} does not exist", bak.display())
        } else {
            format!("read {}: {}", bak.display(), e)
        }
    })?;
    crate::notes::write_note_atomic(vault, session_id, &body).await?;

    // Best-effort cleanup. If the unlink races with another writer the
    // worst case is the user sees the Restore pill on the next page
    // load and clicks Restore again — same body, no data loss.
    let _ = fs::remove_file(&bak).await;
    Ok(body)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::notes::note_basename;
    use tokio::fs;

    /// Path to the on-disk note (or backup) for a session, mirroring
    /// the writer's `<vault>/meetings/<basename>.md` layout. Test-only
    /// helper — production reads go through `resolve_note_path`.
    fn test_note_path(vault: &Path, session_id: &str, ext: &str) -> PathBuf {
        meetings_dir(vault).join(format!("{}{ext}", note_basename(session_id)))
    }

    /// Helper: seed a vault with a note. The body alone (no
    /// frontmatter) is enough for `check_backup` / `restore_backup`
    /// since they don't parse the markdown.
    async fn seed_note(vault: &Path, session_id: &str, body: &str) {
        let path = test_note_path(vault, session_id, ".md");
        fs::create_dir_all(path.parent().expect("parent"))
            .await
            .expect("mkdir meetings");
        fs::write(&path, body).await.expect("seed note");
    }

    async fn seed_bak(vault: &Path, session_id: &str, body: &str) {
        let path = test_note_path(vault, session_id, ".md.bak");
        fs::create_dir_all(path.parent().expect("parent"))
            .await
            .expect("mkdir meetings");
        fs::write(&path, body).await.expect("seed bak");
    }

    /// `check_backup` returns `None` when no `.md.bak` exists. The
    /// steady-state / common-case path on every page load — must not
    /// surface as an error.
    #[tokio::test]
    async fn check_backup_returns_none_when_no_backup() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let vault = tmp.path();
        seed_note(vault, "note", "body").await;

        let result = check_backup(vault, "note").await.expect("check");
        assert!(result.is_none(), "expected None, got {result:?}");
    }

    /// `check_backup` returns the mtime as an RFC-3339 string when a
    /// `.md.bak` is present.
    #[tokio::test]
    async fn check_backup_returns_some_with_timestamp() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let vault = tmp.path();
        seed_note(vault, "note", "current").await;
        seed_bak(vault, "note", "previous").await;

        let result = check_backup(vault, "note").await.expect("check");
        let info = result.expect("expected Some(BackupInfo)");
        // Don't pin the exact value (depends on host clock); assert
        // the shape is RFC-3339-ish (contains `T` separator + a `:`
        // in the time portion). chrono's `to_rfc3339` always emits
        // these, so the assertion is stable.
        assert!(info.created_at.contains('T'), "got: {}", info.created_at);
    }

    /// Path-traversal attempts are rejected before any filesystem
    /// operation. Anchors the same shared validator that `notes.rs`
    /// uses on its read/write surface.
    #[tokio::test]
    async fn check_backup_rejects_traversal() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let vault = tmp.path();
        for evil in ["..", ".", "../etc/passwd", "foo/bar"] {
            let err = check_backup(vault, evil)
                .await
                .expect_err(&format!("must reject {evil}"));
            assert!(!err.is_empty());
        }
    }

    /// `restore_backup` overwrites the note with the `.md.bak` body
    /// AND deletes the `.bak` — the common rollback flow.
    #[tokio::test]
    async fn restore_backup_overwrites_note_and_deletes_bak() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let vault = tmp.path();
        seed_note(vault, "note", "current").await;
        seed_bak(vault, "note", "previous").await;

        let restored = restore_backup(vault, "note").await.expect("restore");
        assert_eq!(restored, "previous");

        // Note now contains the backup body.
        let on_disk = fs::read_to_string(test_note_path(vault, "note", ".md"))
            .await
            .expect("read note");
        assert_eq!(on_disk, "previous");

        // .bak is gone.
        let bak = test_note_path(vault, "note", ".md.bak");
        assert!(!bak.exists(), "expected .md.bak to be deleted");

        // Subsequent check_backup returns None.
        let again = check_backup(vault, "note").await.expect("check again");
        assert!(again.is_none());
    }

    /// `restore_backup` errors clearly when there's no `.bak` to
    /// restore from. The Review UI should never call this without a
    /// pill being visible, but a stale double-click while the first
    /// restore is in flight could race; the error message is the
    /// safety net.
    #[tokio::test]
    async fn restore_backup_errors_when_no_bak() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let vault = tmp.path();
        seed_note(vault, "note", "body").await;

        let err = restore_backup(vault, "note").await.expect_err("must error");
        assert!(err.contains("does not exist"), "got: {err}");
    }

    /// On unix, the restored note keeps the same 0600 permissions
    /// `notes::write_note_atomic` enforces. Anchors that the rollback
    /// path doesn't accidentally widen the posture vs. a normal save.
    #[cfg(unix)]
    #[tokio::test]
    async fn restore_backup_preserves_user_only_perms() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().expect("tmp");
        let vault = tmp.path();
        seed_note(vault, "note", "current").await;
        seed_bak(vault, "note", "previous").await;
        restore_backup(vault, "note").await.expect("restore");

        let mode = std::fs::metadata(test_note_path(vault, "note", ".md"))
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// `resolve_bak_path` rejects the same traversal payloads that
    /// `notes::resolve_note_path` rejects, end-to-end. Without this
    /// guard a renderer bug supplying `..` could read or write a
    /// `.md.bak` outside the vault — the rollback surface must keep
    /// parity with the read/write surface.
    #[tokio::test]
    async fn resolve_bak_path_rejects_traversal() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let vault = tmp.path();
        for evil in ["..", ".", "foo/bar", "a\\b", ""] {
            let err = resolve_bak_path(vault, evil)
                .await
                .expect_err(&format!("must reject {evil}"));
            assert!(!err.is_empty());
        }
    }

    /// A symlinked `meetings/` must not let a `.md.bak` escape the
    /// vault. The lexical-join precursor to this test failed silently
    /// — `check_backup`/`restore_backup` would happily stat / read /
    /// delete a file outside the vault. Anchors the canonicalize +
    /// `starts_with` defense in `resolve_bak_path`.
    #[cfg(unix)]
    #[tokio::test]
    async fn resolve_bak_path_rejects_symlinked_meetings_dir() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::TempDir::new().expect("tmp");
        let vault = tmp.path().join("vault");
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&vault).expect("mkdir vault");
        std::fs::create_dir(&outside).expect("mkdir outside");
        // Plant a `.md.bak` we'd be able to read if the escape worked.
        std::fs::write(outside.join("note.md.bak"), b"secret").expect("seed");
        symlink(&outside, vault.join("meetings")).expect("symlink");

        let err = resolve_bak_path(&vault, "note")
            .await
            .expect_err("symlinked meetings/ must be rejected");
        assert!(err.contains("escapes vault"), "got: {err}");
    }

    /// PR-ξ (phase 76): `resummarize_preview` rejects the same
    /// traversal payloads `resolve_note_path` rejects. Defense in
    /// depth — the validator runs identically on the preview path
    /// and the write path so a renderer bug can't only break one
    /// side. We don't need a vault to be stood up for this test;
    /// the validator is the first thing each call hits.
    #[tokio::test]
    async fn resummarize_preview_rejects_traversal() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let vault = tmp.path();
        for evil in ["..", ".", "../etc/passwd", "foo/bar", ""] {
            let err = resummarize_preview(vault, evil)
                .await
                .expect_err(&format!("must reject {evil}"));
            assert!(!err.is_empty());
        }
    }

    /// PR-ξ (phase 76): the preview command surfaces a clear error
    /// when the note is empty / has no frontmatter, instead of
    /// hanging or returning a confusing summarize-pipeline error.
    /// This is the precondition the Review UI's empty-result branch
    /// relies on — without the upfront read, an empty note would
    /// surface only after the summarizer ran (5–30s wasted).
    #[tokio::test]
    async fn resummarize_preview_errors_on_missing_frontmatter() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let vault = tmp.path();
        // Note exists but has no `---` fence — `vault_read_note`
        // surfaces `MissingFrontmatter`, which we wrap into the
        // "read <path>: ..." string the renderer toasts.
        seed_note(vault, "note", "no frontmatter here").await;
        let err = resummarize_preview(vault, "note")
            .await
            .expect_err("malformed note must error");
        assert!(!err.is_empty());
    }

    /// PR-ξ (phase 76) core invariant: the preview path **never**
    /// writes to disk. Even when the LLM call fails (the common case
    /// in CI without `ANTHROPIC_API_KEY`), the original `<id>.md`
    /// must be byte-identical to its pre-call contents and no
    /// `<id>.md.bak` may have been created.
    ///
    /// We can't easily inject a real summarizer in a unit test
    /// without a live backend, so this asserts the disk invariant
    /// across the failure paths the CI runner actually hits — every
    /// failure mode in the preview pipeline runs **before** any
    /// filesystem mutation, which is the property the test pins.
    #[tokio::test]
    async fn resummarize_preview_does_not_touch_disk() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let vault = tmp.path();
        // Seed a real note + transcript so the preview call gets as
        // far as the summarizer. The body+frontmatter are valid YAML
        // so `vault_read_note` succeeds; the summarizer will then
        // either fail (no API key + no CLI on PATH) or run, and in
        // either case the disk must not change.
        let original_body = "---\n\
date: 2025-01-01\n\
start: \"09:00\"\n\
duration_min: 30\n\
source_app: us.zoom.xos\n\
recording: rec.m4a\n\
transcript: transcript.txt\n\
diarize_source: ax_observer\n\
disclosed: false\n\
company: null\n\
meeting_type: other\n\
tags: []\n\
action_items: []\n\
attendees: []\n\
cost: null\n\
---\n# Body\n";
        seed_note(vault, "note", original_body).await;
        fs::write(vault.join("transcript.txt"), "stub transcript")
            .await
            .expect("seed transcript");

        // The call may succeed (live backend) or fail (no API key,
        // no CLI on PATH); either way, the disk invariants below
        // must hold.
        let _ = resummarize_preview(vault, "note").await;

        // `<id>.md` must be byte-identical — no rotate, no merge-
        // and-write side effect.
        let on_disk = fs::read_to_string(test_note_path(vault, "note", ".md"))
            .await
            .expect("read note");
        assert_eq!(on_disk, original_body, "preview must not modify <id>.md",);

        // `<id>.md.bak` must not have been created.
        let bak = test_note_path(vault, "note", ".md.bak");
        assert!(
            !bak.exists(),
            "preview must not create <id>.md.bak (found {})",
            bak.display(),
        );
    }

    /// Tier 4 #18: `persona_from_settings` collapses the all-empty
    /// `Persona` (the default after `Settings::default()`) to `None`.
    /// The boundary collapse is the single switch the rest of the
    /// pipeline depends on for the "no-persona prompt is byte-identical
    /// to pre-Tier-4" contract — the LLM-side logic in
    /// `render_meeting_prompt` re-applies the same `is_empty()` check
    /// (belt-and-suspenders), but breaking either side independently
    /// could silently turn a no-config user's prompt into a different
    /// shape, so pin both ends of the contract.
    #[test]
    fn persona_from_settings_collapses_empty_persona_to_none() {
        let settings = crate::settings::Settings::default();
        assert!(persona_from_settings(&settings).is_none());
    }

    #[test]
    fn persona_from_settings_returns_some_when_any_field_set() {
        let mut settings = crate::settings::Settings::default();
        settings.persona.name = "Alice".into();
        let persona = persona_from_settings(&settings).expect("Some");
        assert_eq!(persona.name, "Alice");
        assert_eq!(persona.role, "");
        assert_eq!(persona.working_on, "");
    }
}
