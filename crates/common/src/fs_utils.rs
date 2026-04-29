//! Filesystem utilities for crash-safe operations.
//!
//! This module provides durable filesystem operations that match stellar-core's
//! crash-safety semantics, ensuring data survives both application crashes and
//! OS/power-loss crashes.
//!
//! # Durability Guarantees
//!
//! - [`durable_rename`] performs a rename followed by an `fsync` on the parent
//!   directory, ensuring the directory entry update is persisted to stable storage.
//!   This matches stellar-core's `durableRename()` in `Fs.cpp`.
//!
//! - [`atomic_write_with`] and [`atomic_write_bytes`] write data to a temp file,
//!   fsync, then durably rename to the final path. This guarantees the final path
//!   is never left in a partial state after a crash.
//!
//! - [`atomic_copy`] and [`atomic_gzip_copy`] are file-to-file conveniences
//!   built on [`atomic_write_with`]: they stream `from` into a temp file
//!   (optionally through a gzip encoder), then durably rename. Both treat
//!   `from == to` (path-equality, not canonicalized) as a no-op.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Atomically rename a file and fsync the parent directory.
///
/// This ensures the rename is durable even in the face of an OS crash or
/// power loss. Without the directory fsync, the rename could be lost even
/// though `rename()` is atomic at the filesystem level — the directory entry
/// update may only be in the kernel's page cache.
///
/// Matches stellar-core's `durableRename()` in `Fs.cpp`.
///
/// # Errors
///
/// Returns an error if:
/// - The rename fails (e.g., source doesn't exist, permission denied)
/// - The parent directory cannot be opened or fsynced
pub fn durable_rename(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)?;

    // Fsync the parent directory to ensure the rename is durable.
    // On POSIX systems, rename is atomic but the directory entry update
    // may not be flushed to disk until the directory is fsynced.
    if let Some(parent) = to.parent() {
        let dir = fs::File::open(parent)?;
        dir.sync_all()?;
    }

    Ok(())
}

/// Monotonic counter for generating unique temp file names.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a unique temp file path in the same directory as `final_path`.
///
/// Uses PID + atomic counter to guarantee uniqueness across threads and
/// process restarts. The temp file should be created with
/// `OpenOptions::new().write(true).create_new(true)` to detect stale collisions.
pub fn temp_path(final_path: &Path) -> PathBuf {
    final_path.with_file_name(format!(
        "{}.tmp.{}.{}",
        final_path.file_name().unwrap().to_string_lossy(),
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed),
    ))
}

/// Atomically write to `final_path` via a temp file + durable rename.
///
/// Calls `write_fn` with a fresh, exclusively-created temp file handle.
/// After `write_fn` returns Ok, fsyncs the file and durably renames it
/// to `final_path`. On any error before the rename, the temp file is
/// cleaned up and the final path is never modified.
///
/// # Error contract
///
/// - **Pre-rename errors** (temp creation, write_fn, sync_all): temp file
///   is removed, final path is untouched. Safe to retry.
/// - **Post-rename errors** (parent dir fsync in `durable_rename`): the
///   rename succeeded so `final_path` contains the new complete data, but
///   durability is not guaranteed on power loss. The error is propagated
///   so callers can decide whether to retry.
///
/// # Platform notes
///
/// Uses POSIX `rename(2)` semantics: atomically replaces any existing
/// file at `final_path`. This is the target platform (Linux).
pub fn atomic_write_with<F>(final_path: &Path, write_fn: F) -> io::Result<()>
where
    F: FnOnce(&mut fs::File) -> io::Result<()>,
{
    let tmp = temp_path(final_path);

    // Phase 1: write to temp file (pre-rename — cleanup on any error)
    let result = (|| -> io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        write_fn(&mut file)?;
        file.sync_all()?;
        Ok(())
    })();

    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }

    // Phase 2: atomic rename (post-rename errors may leave final_path visible)
    if let Err(e) = durable_rename(&tmp, final_path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }

    Ok(())
}

/// Atomically write `data` to `final_path` via a temp file + durable rename.
///
/// Convenience wrapper around [`atomic_write_with`]. See its documentation
/// for error contract and platform notes.
pub fn atomic_write_bytes(final_path: &Path, data: &[u8]) -> io::Result<()> {
    atomic_write_with(final_path, |file| file.write_all(data))
}

/// Atomically copy `from` to `to` via a temp file + durable rename.
///
/// Reads `from` and streams it through `std::io::copy` into a temp file
/// in `to`'s parent directory, then `fsync`s and durably renames the
/// temp into place. Composes [`atomic_write_with`] with `File::open` +
/// `io::copy`; the durability and error contract of the underlying
/// primitive applies unchanged.
///
/// `to` is replaced atomically (POSIX `rename(2)`).
///
/// If `from` and `to` refer to the same path (path-equality, *not*
/// canonicalized — symlinks and different spellings of the same file
/// are not detected), this returns `Ok(())` without touching disk.
/// Callers that need same-inode detection must canonicalize before
/// calling.
///
/// # Errors
///
/// Inherits [`atomic_write_with`]'s contract:
/// - Pre-rename errors (open `from`, write to temp, sync_all): temp
///   file is removed; `to` is untouched.
/// - Post-rename errors (parent dir fsync): rename succeeded but
///   durability is not guaranteed; error is propagated.
pub fn atomic_copy(from: &Path, to: &Path) -> io::Result<()> {
    if from == to {
        return Ok(());
    }
    atomic_write_with(to, |file| {
        let mut src = fs::File::open(from)?;
        io::copy(&mut src, file)?;
        Ok(())
    })
}

/// Atomically gzip-compress `from` and write the result to `to`.
///
/// Streams the source through a `flate2::write::GzEncoder` with default
/// compression into a temp file in `to`'s parent, then durably renames
/// into place. The output is standard gzip — the same format produced
/// by `gzip(1)` on the source.
///
/// If `from` and `to` are path-equal, returns `Ok(())` without touching
/// disk. As with [`atomic_copy`], this is path equality only; symlinks
/// and alternate spellings are not detected.
///
/// # Errors
///
/// Same contract as [`atomic_write_with`]: the pre-rename phase
/// (opening `from`, encoding, finishing the encoder, syncing the temp)
/// cleans up on error and leaves `to` untouched. Post-rename errors
/// (parent dir fsync) propagate but the rename has already succeeded.
pub fn atomic_gzip_copy(from: &Path, to: &Path) -> io::Result<()> {
    use flate2::write::GzEncoder;
    use flate2::Compression;

    if from == to {
        return Ok(());
    }
    atomic_write_with(to, |file| {
        let mut encoder = GzEncoder::new(&mut *file, Compression::default());
        let mut src = fs::File::open(from)?;
        io::copy(&mut src, &mut encoder)?;
        encoder.finish()?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_durable_rename_basic() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("source.txt");
        let dst = dir.path().join("dest.txt");

        fs::write(&src, b"hello").unwrap();
        assert!(src.exists());
        assert!(!dst.exists());

        durable_rename(&src, &dst).unwrap();

        assert!(!src.exists());
        assert!(dst.exists());
        assert_eq!(fs::read(&dst).unwrap(), b"hello");
    }

    #[test]
    fn test_durable_rename_overwrite() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("source.txt");
        let dst = dir.path().join("dest.txt");

        fs::write(&src, b"new content").unwrap();
        fs::write(&dst, b"old content").unwrap();

        durable_rename(&src, &dst).unwrap();

        assert!(!src.exists());
        assert_eq!(fs::read(&dst).unwrap(), b"new content");
    }

    #[test]
    fn test_durable_rename_nonexistent_source() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("nonexistent.txt");
        let dst = dir.path().join("dest.txt");

        let result = durable_rename(&src, &dst);
        assert!(result.is_err());
    }

    #[test]
    fn test_temp_path_uniqueness() {
        let base = Path::new("/tmp/test.bucket.xdr");
        let p1 = temp_path(base);
        let p2 = temp_path(base);
        assert_ne!(p1, p2, "temp_path must produce unique names");
    }

    #[test]
    fn test_atomic_write_bytes_basic() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.dat");

        atomic_write_bytes(&path, b"hello world").unwrap();

        assert!(path.exists());
        assert_eq!(fs::read(&path).unwrap(), b"hello world");

        // No temp files should remain
        let temps: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(temps.is_empty(), "temp files should be cleaned up");
    }

    #[test]
    fn test_atomic_write_bytes_overwrite() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.dat");

        fs::write(&path, b"old content").unwrap();
        atomic_write_bytes(&path, b"new content").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"new content");
    }

    #[test]
    fn test_atomic_write_with_basic() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("streamed.dat");

        atomic_write_with(&path, |file| {
            file.write_all(b"part1")?;
            file.write_all(b"part2")?;
            Ok(())
        })
        .unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"part1part2");
    }

    #[test]
    fn test_atomic_write_with_error_no_final_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("should_not_exist.dat");

        let result = atomic_write_with(&path, |_file| {
            Err(io::Error::new(io::ErrorKind::Other, "simulated failure"))
        });

        assert!(result.is_err());
        assert!(
            !path.exists(),
            "final path must not exist after write error"
        );

        // No temp files should remain
        let temps: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(temps.is_empty(), "temp files should be cleaned up on error");
    }

    #[test]
    fn test_atomic_write_with_error_preserves_existing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("existing.dat");

        fs::write(&path, b"original content").unwrap();

        let result = atomic_write_with(&path, |_file| {
            Err(io::Error::new(io::ErrorKind::Other, "simulated failure"))
        });

        assert!(result.is_err());
        assert_eq!(
            fs::read(&path).unwrap(),
            b"original content",
            "existing file must be untouched after write error"
        );
    }

    #[test]
    fn test_atomic_copy_basic() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src.dat");
        let dst = dir.path().join("dst.dat");

        fs::write(&src, b"hello copy").unwrap();
        atomic_copy(&src, &dst).unwrap();

        assert_eq!(fs::read(&dst).unwrap(), b"hello copy");
        // Source must remain readable and unchanged.
        assert_eq!(fs::read(&src).unwrap(), b"hello copy");
    }

    #[test]
    fn test_atomic_copy_overwrite() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src.dat");
        let dst = dir.path().join("dst.dat");

        fs::write(&src, b"new content").unwrap();
        fs::write(&dst, b"old content").unwrap();

        atomic_copy(&src, &dst).unwrap();

        assert_eq!(fs::read(&dst).unwrap(), b"new content");
    }

    #[test]
    fn test_atomic_copy_missing_source() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("does_not_exist.dat");
        let dst = dir.path().join("dst.dat");

        let result = atomic_copy(&src, &dst);
        assert!(result.is_err());
        assert!(!dst.exists(), "dst must not exist when src is missing");
    }

    #[test]
    fn test_atomic_copy_preserves_existing_on_missing_source() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("does_not_exist.dat");
        let dst = dir.path().join("dst.dat");

        fs::write(&dst, b"original").unwrap();
        let result = atomic_copy(&src, &dst);

        assert!(result.is_err());
        assert_eq!(
            fs::read(&dst).unwrap(),
            b"original",
            "existing dst must be untouched when src is missing"
        );
    }

    #[test]
    fn test_atomic_copy_no_temp_left_on_error() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("missing.dat");
        let dst = dir.path().join("dst.dat");

        let _ = atomic_copy(&src, &dst);

        let temps: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(temps.is_empty(), "no temp files should remain after error");
    }

    #[test]
    fn test_atomic_copy_same_path_noop() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file.dat");

        fs::write(&path, b"unchanged").unwrap();
        atomic_copy(&path, &path).unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"unchanged");

        let temps: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(
            temps.is_empty(),
            "same-path no-op must not create temp files"
        );
    }

    #[test]
    fn test_atomic_gzip_copy_roundtrip() {
        use flate2::read::GzDecoder;
        use std::io::Read;

        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src.dat");
        let dst = dir.path().join("dst.gz");

        let original: Vec<u8> = (0..1024u32).flat_map(|i| i.to_le_bytes()).collect();
        fs::write(&src, &original).unwrap();

        atomic_gzip_copy(&src, &dst).unwrap();

        let mut decoded = Vec::new();
        let gz_file = fs::File::open(&dst).unwrap();
        GzDecoder::new(gz_file).read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn test_atomic_gzip_copy_large_input() {
        use flate2::read::GzDecoder;
        use std::io::Read;

        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src.dat");
        let dst = dir.path().join("dst.gz");

        // ~200KB of pseudo-random bytes that don't compress to near-zero,
        // exercising the streaming read/write path well past the default
        // 64KB internal buffer.
        let original: Vec<u8> = (0..200_000u32)
            .map(|i| i.wrapping_mul(2654435761) as u8)
            .collect();
        fs::write(&src, &original).unwrap();

        atomic_gzip_copy(&src, &dst).unwrap();

        let mut decoded = Vec::new();
        let gz_file = fs::File::open(&dst).unwrap();
        GzDecoder::new(gz_file).read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn test_atomic_gzip_copy_same_path_noop() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("file.gz");

        fs::write(&path, b"unchanged").unwrap();
        atomic_gzip_copy(&path, &path).unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"unchanged");
    }
}
