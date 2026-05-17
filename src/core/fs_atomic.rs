//! Atomic filesystem helpers.
//!
//! Both [`write_atomic`] and [`temp_sibling`] place the temp file in the same
//! directory as the target so that `rename(2)` stays on a single filesystem and
//! is therefore atomic on Unix. A crash mid-write leaves either the original
//! target untouched (best case) or a leftover `.tmp.*` file (no corruption).
//!
//! The helpers do not call `fsync` — they protect against partial writes, not
//! against power loss between the write and the directory entry being persisted.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Atomically write `contents` to `path` by writing to a sibling temp file and
/// renaming it into place. On any failure, the temp file is removed and the
/// original `path` (if it existed) is left untouched.
pub fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    let tmp = temp_sibling(path)?;

    if let Err(e) = std::fs::write(&tmp, contents) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    Ok(())
}

/// Compute a sibling temp path of the form `<path>.tmp.<pid>.<nanos>`.
///
/// Living in the same directory as the target is required so that the eventual
/// `rename` stays atomic (same filesystem). PID + nanos is enough to avoid
/// collisions in normal operation; a real attacker on the same FS could still
/// race, so callers should keep their target directory under access control.
pub fn temp_sibling(path: &Path) -> io::Result<PathBuf> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "path has no parent directory")
    })?;
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "path has no file name")
    })?;

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();

    let mut tmp_name = file_name.to_os_string();
    tmp_name.push(format!(".tmp.{}.{}", pid, nanos));
    Ok(parent.join(tmp_name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn write_atomic_creates_new_file() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("cover.jpg");

        write_atomic(&target, b"new bytes").unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"new bytes");
        assert_no_leftover_tmp(dir.path(), "cover.jpg");
    }

    #[test]
    fn write_atomic_replaces_existing_file() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("cover.jpg");
        std::fs::write(&target, b"old bytes").unwrap();

        write_atomic(&target, b"new bytes").unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"new bytes");
        assert_no_leftover_tmp(dir.path(), "cover.jpg");
    }

    #[test]
    fn write_atomic_no_partial_write_on_failure() {
        // Target the parent directory itself: rename onto a non-empty directory
        // fails, so write_atomic must clean up and leave nothing behind.
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("keep.txt"), b"keep me").unwrap();

        let result = write_atomic(&nested, b"junk");
        assert!(result.is_err(), "renaming over a non-empty dir must fail");

        // Original directory and its content survive intact.
        assert!(nested.is_dir());
        assert_eq!(std::fs::read(nested.join("keep.txt")).unwrap(), b"keep me");

        // No `.tmp.*` lingering at the parent level.
        assert_no_leftover_tmp(dir.path(), "nested");
    }

    #[test]
    fn temp_sibling_lives_in_same_dir() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("file.bin");

        let tmp = temp_sibling(&target).unwrap();

        assert_eq!(tmp.parent(), Some(dir.path()));
        let name = tmp.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("file.bin.tmp."),
            "unexpected temp name: {}",
            name
        );
    }

    #[test]
    fn temp_sibling_unique_per_call() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("file.bin");

        let a = temp_sibling(&target).unwrap();
        // Force at least one nanosecond of progress.
        std::thread::sleep(std::time::Duration::from_nanos(1));
        let b = temp_sibling(&target).unwrap();

        assert_ne!(a, b);
    }

    fn assert_no_leftover_tmp(dir: &Path, target_name: &str) {
        let prefix = format!("{}.tmp.", target_name);
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            assert!(
                !name.starts_with(&prefix),
                "leftover temp file: {}",
                name
            );
        }
    }
}
