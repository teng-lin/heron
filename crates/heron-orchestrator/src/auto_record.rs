//! Per-event auto-record registry (Tier 5 #26).
//!
//! Holds the set of `calendar_event_id`s the user has marked
//! "auto-record" on the upcoming-meetings rail. Lives next to the
//! vault on disk (`<vault_root>/.heron/auto_record.json`) so the
//! choice survives daemon restarts; `None` for `path` means in-memory
//! only (substrate-only mode without a configured vault root).
//!
//! The actual auto-arming scheduler that consumes this registry lands
//! alongside it — registry membership is the *flag*; the scheduler
//! polls the calendar and fires `start_capture` when an enabled
//! event's start window opens. Today's responsibility split:
//!
//! - This module owns the *what* (membership + persistence).
//! - The orchestrator owns the *when* (scheduler tick + FSM
//!   handoff), and the orchestrator's `list_upcoming_calendar` mirrors
//!   `contains` onto each `CalendarEvent.auto_record` so the rail can
//!   render the toggle's current state without a second round trip.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::lock_or_recover;

/// File-format envelope. Versioned so a future schema change (per-event
/// metadata, expirations, etc.) can land without breaking older
/// daemon builds: an unknown `version` is treated as
/// `RegistryError::UnsupportedVersion` and the file is left intact.
#[derive(Debug, Serialize, Deserialize)]
struct OnDiskRegistry {
    version: u32,
    #[serde(default)]
    event_ids: Vec<String>,
}

const ON_DISK_VERSION: u32 = 1;

/// Filename inside the vault's `.heron/` subdirectory. Hidden so the
/// user's vault listing isn't polluted; `.heron/` mirrors how other
/// dotfile-style state directories are conventionally placed
/// alongside notes (Obsidian's `.obsidian/`, etc.).
const AUTO_RECORD_FILENAME: &str = "auto_record.json";

/// Directory inside the vault root that holds heron's per-vault
/// state (today: just this registry; future: anything else that
/// needs to ride along with the vault).
const HERON_STATE_DIR: &str = ".heron";

/// Errors specific to the registry's I/O surface. Validation errors
/// (empty / oversized id) are translated into `SessionError::Validation`
/// at the orchestrator boundary; this enum stays local to the module.
#[derive(Debug)]
pub(crate) enum RegistryError {
    Io(io::Error),
    Parse(serde_json::Error),
    UnsupportedVersion(u32),
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "auto-record registry I/O failed: {e}"),
            Self::Parse(e) => write!(f, "auto-record registry parse failed: {e}"),
            Self::UnsupportedVersion(v) => {
                write!(f, "auto-record registry has unsupported version {v}")
            }
        }
    }
}

impl std::error::Error for RegistryError {}

/// In-memory + optionally file-backed set of auto-record event ids.
/// Cheap to query (`contains` is one Mutex lock + `HashSet` lookup),
/// flushes on every mutation (`set`) so a daemon crash mid-session
/// never loses the last toggle.
#[derive(Debug)]
pub(crate) struct AutoRecordRegistry {
    inner: Mutex<HashSet<String>>,
    /// Absolute path to the on-disk registry. `None` for in-memory
    /// mode (no vault root configured); writes still succeed at the
    /// API level, they just don't survive restart.
    path: Option<PathBuf>,
}

impl AutoRecordRegistry {
    /// Construct a registry, hydrating from `<vault_root>/.heron/auto_record.json`
    /// when `vault_root` is `Some`. A missing file is treated as an
    /// empty registry — callers shouldn't have to special-case the
    /// first-ever boot. A *malformed* file is treated as fatal so a
    /// schema regression surfaces loudly rather than silently dropping
    /// the user's saved choices.
    pub(crate) fn load(vault_root: Option<&std::path::Path>) -> Result<Self, RegistryError> {
        let Some(root) = vault_root else {
            return Ok(Self {
                inner: Mutex::new(HashSet::new()),
                path: None,
            });
        };
        let path = root.join(HERON_STATE_DIR).join(AUTO_RECORD_FILENAME);
        let event_ids = match fs::read(&path) {
            Ok(bytes) => {
                let parsed: OnDiskRegistry =
                    serde_json::from_slice(&bytes).map_err(RegistryError::Parse)?;
                if parsed.version != ON_DISK_VERSION {
                    return Err(RegistryError::UnsupportedVersion(parsed.version));
                }
                parsed.event_ids.into_iter().collect()
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => HashSet::new(),
            Err(err) => return Err(RegistryError::Io(err)),
        };
        Ok(Self {
            inner: Mutex::new(event_ids),
            path: Some(path),
        })
    }

    /// Resilient hydration: same as `load`, but on a parse / version
    /// failure the bad file is renamed aside (`.corrupt.<unix-ts>`)
    /// and an empty registry is returned instead. Used at orchestrator
    /// startup so a truncated write or hand-edit doesn't brick boot —
    /// the user just loses the few toggles in the bad file rather than
    /// having the daemon panic until they fix it manually.
    ///
    /// I/O errors (permission denied, disk gone) still propagate —
    /// silently returning an empty registry there would happily mask
    /// a misconfigured vault path.
    pub(crate) fn load_or_quarantine(
        vault_root: Option<&std::path::Path>,
    ) -> Result<Self, RegistryError> {
        match Self::load(vault_root) {
            Ok(reg) => Ok(reg),
            Err(err @ (RegistryError::Parse(_) | RegistryError::UnsupportedVersion(_))) => {
                if let Some(root) = vault_root {
                    let path = root.join(HERON_STATE_DIR).join(AUTO_RECORD_FILENAME);
                    let stamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let quarantine = path.with_extension(format!("json.corrupt.{stamp}"));
                    match fs::rename(&path, &quarantine) {
                        Ok(()) => tracing::warn!(
                            error = %err,
                            quarantined_to = %quarantine.display(),
                            "auto-record registry corrupt; quarantined and starting empty",
                        ),
                        Err(rename_err) => tracing::warn!(
                            error = %err,
                            rename_error = %rename_err,
                            "auto-record registry corrupt; quarantine rename failed, starting empty in-memory",
                        ),
                    }
                    Ok(Self {
                        inner: Mutex::new(HashSet::new()),
                        path: Some(path),
                    })
                } else {
                    // No vault root means there was no on-disk file
                    // to corrupt in the first place; surface the
                    // (impossible) error rather than swallow it.
                    Err(err)
                }
            }
            Err(err) => Err(err),
        }
    }

    /// Constructor for substrate-only tests that don't need a vault.
    /// Real callers go through [`Self::load`].
    #[cfg(test)]
    pub(crate) fn in_memory() -> Self {
        Self {
            inner: Mutex::new(HashSet::new()),
            path: None,
        }
    }

    /// Whether `event_id` is enabled for auto-record. Used by
    /// `list_upcoming_calendar` to mirror the flag onto each event.
    pub(crate) fn contains(&self, event_id: &str) -> bool {
        lock_or_recover(&self.inner).contains(event_id)
    }

    /// Snapshot of the current set, sorted for stable wire shape.
    /// Sorting in the snapshot (not at insert time) keeps the hot
    /// `contains` path on a `HashSet`; the read-side allocation is
    /// per-API-call and cheap.
    pub(crate) fn list(&self) -> Vec<String> {
        let mut out: Vec<String> = lock_or_recover(&self.inner).iter().cloned().collect();
        out.sort();
        out
    }

    /// Add or remove `event_id`. Returns `true` when the membership
    /// actually changed (so callers can no-op the flush on duplicates).
    /// Persists to disk on every change when a path is configured —
    /// the registry is small, the user's toggle clicks are infrequent,
    /// and the alternative (debounced flush) opens a window where a
    /// crash drops the last choice silently.
    pub(crate) fn set(&self, event_id: String, enabled: bool) -> Result<bool, RegistryError> {
        let mut g = lock_or_recover(&self.inner);
        if g.contains(&event_id) == enabled {
            return Ok(false);
        }
        let mut next = g.clone();
        if enabled {
            next.insert(event_id);
        } else {
            next.remove(&event_id);
        }
        let mut snapshot = next.iter().cloned().collect::<Vec<_>>();
        snapshot.sort();
        if let Some(path) = self.path.as_ref() {
            flush(path, &snapshot)?;
        }
        *g = next;
        Ok(true)
    }
}

/// Serialize + atomic-rename write to keep partial writes from
/// surviving a crash. The temp path lives in the same directory as
/// the target so `rename` stays a single inode-table swap (cross-
/// device rename is a copy, which defeats the atomicity).
fn flush(path: &std::path::Path, event_ids: &[String]) -> Result<(), RegistryError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(RegistryError::Io)?;
    }
    let body = serde_json::to_vec_pretty(&OnDiskRegistry {
        version: ON_DISK_VERSION,
        event_ids: event_ids.to_vec(),
    })
    .map_err(RegistryError::Parse)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, body).map_err(RegistryError::Io)?;
    fs::rename(&tmp, path).map_err(RegistryError::Io)?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn empty_registry_when_file_missing() {
        let dir = tempdir().expect("tempdir");
        let reg = AutoRecordRegistry::load(Some(dir.path())).expect("load");
        assert!(!reg.contains("evt_anything"));
        assert!(reg.list().is_empty());
    }

    #[test]
    fn set_persists_across_load() {
        let dir = tempdir().expect("tempdir");
        let reg = AutoRecordRegistry::load(Some(dir.path())).expect("load");
        assert!(
            reg.set("evt_alpha".to_owned(), true)
                .expect("set should succeed")
        );
        // Re-load from disk in a fresh registry to prove persistence.
        let reload = AutoRecordRegistry::load(Some(dir.path())).expect("reload");
        assert!(reload.contains("evt_alpha"));
        assert_eq!(reload.list(), vec!["evt_alpha"]);
    }

    #[test]
    fn set_disable_removes_and_persists() {
        let dir = tempdir().expect("tempdir");
        let reg = AutoRecordRegistry::load(Some(dir.path())).expect("load");
        reg.set("evt_alpha".to_owned(), true).expect("set on");
        reg.set("evt_alpha".to_owned(), false).expect("set off");
        let reload = AutoRecordRegistry::load(Some(dir.path())).expect("reload");
        assert!(!reload.contains("evt_alpha"));
        assert!(reload.list().is_empty());
    }

    #[test]
    fn set_returns_false_on_duplicate() {
        let dir = tempdir().expect("tempdir");
        let reg = AutoRecordRegistry::load(Some(dir.path())).expect("load");
        assert!(reg.set("evt_alpha".to_owned(), true).expect("first"));
        assert!(
            !reg.set("evt_alpha".to_owned(), true)
                .expect("duplicate should not error"),
            "duplicate enable must return Ok(false) so callers can skip the flush",
        );
    }

    #[test]
    fn list_is_sorted_for_stable_wire_shape() {
        let dir = tempdir().expect("tempdir");
        let reg = AutoRecordRegistry::load(Some(dir.path())).expect("load");
        for id in ["evt_charlie", "evt_alpha", "evt_bravo"] {
            reg.set(id.to_owned(), true).expect("set");
        }
        assert_eq!(
            reg.list(),
            vec!["evt_alpha", "evt_bravo", "evt_charlie"],
            "snapshot must be sorted so the wire payload is byte-stable",
        );
    }

    #[test]
    fn unsupported_version_fails_loudly() {
        // A future schema bump must not silently drop the user's
        // saved choices — surface UnsupportedVersion so the daemon
        // can decline to start (or migrate) rather than re-key the
        // registry from scratch.
        let dir = tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(HERON_STATE_DIR)).expect("mkdir");
        std::fs::write(
            dir.path().join(HERON_STATE_DIR).join(AUTO_RECORD_FILENAME),
            br#"{"version": 99, "event_ids": ["evt_x"]}"#,
        )
        .expect("write");
        let err = AutoRecordRegistry::load(Some(dir.path()))
            .expect_err("future-version registry must surface as an error");
        assert!(
            matches!(err, RegistryError::UnsupportedVersion(99)),
            "expected UnsupportedVersion(99), got {err:?}",
        );
    }

    #[test]
    fn in_memory_mode_works_without_path() {
        // Substrate-only callers (no vault root) still get a usable
        // registry — toggles persist for the daemon's lifetime; a
        // restart resets the set.
        let reg = AutoRecordRegistry::in_memory();
        assert!(reg.set("evt_alpha".to_owned(), true).expect("set"));
        assert!(reg.contains("evt_alpha"));
    }

    #[test]
    fn load_or_quarantine_starts_empty_when_file_is_garbage() {
        // A truncated write or hand-edit must not brick the daemon —
        // `load_or_quarantine` renames the bad file aside and returns
        // an empty registry so startup can proceed. Subsequent writes
        // re-create a clean file at the original path.
        let dir = tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(HERON_STATE_DIR)).expect("mkdir");
        std::fs::write(
            dir.path().join(HERON_STATE_DIR).join(AUTO_RECORD_FILENAME),
            b"{not even close to json",
        )
        .expect("write");

        let reg = AutoRecordRegistry::load_or_quarantine(Some(dir.path()))
            .expect("quarantine variant must not propagate parse errors");
        assert!(
            reg.list().is_empty(),
            "starts with no entries after quarantine"
        );

        // Bad file is no longer at the canonical path.
        let canonical = dir.path().join(HERON_STATE_DIR).join(AUTO_RECORD_FILENAME);
        assert!(
            !canonical.exists(),
            "the corrupt file must be moved aside so the next write doesn't reparse it",
        );
        // And a sibling `.corrupt.<ts>` quarantine file should now exist.
        let entries: Vec<_> = std::fs::read_dir(dir.path().join(HERON_STATE_DIR))
            .expect("readdir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            entries
                .iter()
                .any(|name| name.starts_with("auto_record.json.corrupt.")),
            "expected a quarantine file alongside the registry, got {entries:?}",
        );

        // New writes flush back to the canonical path.
        reg.set("evt_alpha".to_owned(), true)
            .expect("set after quarantine");
        let reload = AutoRecordRegistry::load(Some(dir.path())).expect("reload");
        assert!(reload.contains("evt_alpha"));
    }

    #[test]
    fn load_or_quarantine_passes_through_when_file_is_fine() {
        let dir = tempdir().expect("tempdir");
        let seed = AutoRecordRegistry::load(Some(dir.path())).expect("seed");
        seed.set("evt_alpha".to_owned(), true).expect("set");

        let reg = AutoRecordRegistry::load_or_quarantine(Some(dir.path())).expect("loads cleanly");
        assert!(reg.contains("evt_alpha"));
    }
}
