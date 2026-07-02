//! Atomic file write via write-to-temp-then-rename on the same filesystem.
//!
//! The [`write_atomic`] function writes bytes to a temporary file in the
//! target's parent directory, fsyncs the data, then renames the temp file
//! over the target.  This guarantees crash-safe atomic replacement on the
//! same filesystem (the rename is atomic on POSIX when source and target are
//! on the same mount).
//!
//! # Errors
//!
//! Returns [`io::Error`] (never panics) when:
//! - The parent directory does not exist or is inaccessible.
//! - A write, fsync, or rename fails.
//!
//! On failure the temporary file is removed as best-effort cleanup.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonically increasing counter used to make temp file names unique per
/// call within this process.  Combined with the process ID, this prevents
/// collisions between concurrent writers to different targets and across
/// process restarts.
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Writes `bytes` to `path` atomically.
///
/// A temporary file is created alongside `path` (in the parent directory),
/// the data is written and synced to disk, and then the temp file is renamed
/// over the target — a crash-safe atomic replacement on the same filesystem.
///
/// The parent directory **must** already exist; if it does not, an [`io::Error`]
/// of kind [`NotFound`](io::ErrorKind::NotFound) is returned.
///
/// On failure the temporary file is removed before the error is propagated
/// (best-effort — a failed removal is silently ignored).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "write_atomic: path has no parent directory",
        )
    })?;

    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "write_atomic: path has no file name",
        )
    })?;

    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);

    // Build a temp-file name that is unique per call (pid + monotonic counter).
    let mut tmp_name = OsString::from(".");
    tmp_name.push(file_name);
    tmp_name.push(".tmp.");
    tmp_name.push(std::process::id().to_string());
    tmp_name.push(".");
    tmp_name.push(counter.to_string());
    let tmp_path = parent.join(tmp_name);

    // Inner closure so we can do best-effort cleanup on any failure
    // after the temp file was created.
    let result = (|| -> io::Result<()> {
        let mut file = File::create(&tmp_path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp_path, path)?;
        Ok(())
    })();

    match result {
        Ok(()) => Ok(()),
        Err(e) => {
            // Best-effort cleanup: remove the temp file if it exists.
            let _ = fs::remove_file(&tmp_path);
            Err(e)
        }
    }
}

// --- tests ---

#[cfg(test)]
mod tests {
    #[test]
    fn write_atomic_creates_new_file_with_correct_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("test.bin");

        let data = b"hello world, this is the persisted content";
        let result = super::write_atomic(&target, data);

        assert!(result.is_ok(), "write_atomic should succeed on a fresh path");
        assert!(target.exists(), "the target file should exist after write_atomic");
        assert_eq!(
            std::fs::read(&target).unwrap(),
            data,
            "the file content should match what was written",
        );

        // No stray temp files — the only entry is the target file.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            entries.len(),
            1,
            "expected exactly one entry (the target file) after write_atomic",
        );
    }

    #[test]
    fn write_atomic_replaces_existing_file_content() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("existing.bin");

        // Pre-write existing content.
        std::fs::write(&target, b"original content that should be replaced").unwrap();

        let new_data = b"replacement content that is longer/shorter than the original";
        let result = super::write_atomic(&target, new_data);

        assert!(
            result.is_ok(),
            "write_atomic should succeed when replacing an existing file",
        );
        assert_eq!(
            std::fs::read(&target).unwrap(),
            new_data,
            "the file content should be entirely replaced",
        );

        // No stray temp files — the only entry is the target file.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            entries.len(),
            1,
            "expected exactly one entry (the target file) after replacing",
        );
    }

    #[test]
    fn write_atomic_returns_error_when_parent_dir_does_not_exist() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nonexistent").join("test.bin");

        // This must return Err — it must NOT panic.
        let result = super::write_atomic(&target, b"data");

        assert!(
            result.is_err(),
            "write_atomic should return Err when the parent directory does not exist",
        );
    }

    #[test]
    fn write_atomic_leaves_no_stray_temp_files_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("clean.bin");

        let result = super::write_atomic(&target, b"some data");

        assert!(result.is_ok(), "write_atomic should succeed");
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            entries.len(),
            1,
            "expected exactly one entry; stray temp files: {:?}",
            entries.iter().map(|e| e.file_name()).collect::<Vec<_>>(),
        );
    }
}
