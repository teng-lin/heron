//! Disk-space gate per
//! [`docs/archives/implementation.md`](../../../docs/archives/implementation.md) §14.1:
//! "Disk-space gate (<2GB free → record disabled)."
//!
//! The Tauri shell (week 11) calls [`free_bytes`] at app launch and
//! before every record-arm; a return below
//! [`MIN_FREE_BYTES_TO_RECORD`] disables the record button with a
//! "free up `<N>GB`" affordance.

use std::path::Path;

use thiserror::Error;

/// 2 GB. Below this on the cache volume the record button is
/// disabled (per §14.1). Tauri shell renders the threshold in the
/// disabled-state copy.
pub const MIN_FREE_BYTES_TO_RECORD: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum DiskError {
    #[error("statvfs / GetDiskFreeSpaceEx failed for {path}: {source}")]
    Probe {
        path: String,
        source: std::io::Error,
    },
}

/// Free bytes on the volume containing `path`.
///
/// Uses `libc::statvfs` on unix, returns an error otherwise — the v1
/// targets are macOS only. Cross-platform support is a phase that
/// matches the §17 Windows / Linux roadmap.
pub fn free_bytes(path: &Path) -> Result<u64, DiskError> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let cstring = CString::new(path.as_os_str().as_bytes()).map_err(|_| DiskError::Probe {
            path: path.display().to_string(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path contains a NUL byte",
            ),
        })?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: cstring lives until end of expression; statvfs writes
        // through the &mut pointer to a stack-local. Standard libc API
        // shape, available on every supported macOS version.
        let rc = unsafe { libc::statvfs(cstring.as_ptr(), &mut stat) };
        if rc != 0 {
            return Err(DiskError::Probe {
                path: path.display().to_string(),
                source: std::io::Error::last_os_error(),
            });
        }
        // f_bavail = blocks available to non-privileged user.
        // The libc binding types these as u64 on macOS and Linux;
        // suppress useless_conversion in case a future libc bump
        // shifts the field type.
        #[allow(clippy::useless_conversion)]
        let avail: u64 = stat.f_bavail.into();
        #[allow(clippy::useless_conversion)]
        let frsize: u64 = stat.f_frsize.into();
        Ok(avail.saturating_mul(frsize))
    }
    #[cfg(not(unix))]
    {
        Err(DiskError::Probe {
            path: path.display().to_string(),
            source: std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "free_bytes is unix-only in v1",
            ),
        })
    }
}

/// `true` if there's enough free space on the volume to start a
/// recording session per §14.1. Convenience wrapper for the UI.
pub fn can_record(path: &Path) -> Result<bool, DiskError> {
    Ok(free_bytes(path)? >= MIN_FREE_BYTES_TO_RECORD)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn free_bytes_reads_existing_path() {
        // / always exists on unix. Just assert we get a number back.
        let bytes = free_bytes(Path::new("/")).expect("/ probes");
        // No volume worth recording on has 0 bytes free unless we're
        // simulating the gate trigger; just check we got a number.
        assert!(bytes > 0);
    }

    #[test]
    fn free_bytes_errors_on_nul_path() {
        // A path containing a NUL byte can't be passed through CString
        // and must surface the InvalidInput error rather than panicking.
        use std::os::unix::ffi::OsStrExt;
        use std::path::PathBuf;
        let bad = PathBuf::from(std::ffi::OsStr::from_bytes(b"/foo\0bar"));
        let result = free_bytes(&bad);
        assert!(result.is_err());
    }

    #[test]
    fn can_record_uses_threshold() {
        // We can't reliably simulate "low disk" in a unit test, but
        // we can sanity-check that the threshold is the published 2GB.
        assert_eq!(MIN_FREE_BYTES_TO_RECORD, 2 * 1024 * 1024 * 1024);
    }

    #[test]
    fn free_bytes_fails_on_missing_path_with_clear_error() {
        let result = free_bytes(Path::new("/definitely-not-a-real-mountpoint-zxqv"));
        match result {
            Err(DiskError::Probe { path, .. }) => {
                assert!(path.contains("zxqv"));
            }
            Ok(b) => panic!("expected probe error, got {b}"),
        }
    }
}
