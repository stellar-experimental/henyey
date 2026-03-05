//! URL path generation for history archive files.
//!
//! History archives use a hierarchical path structure with hex-encoded
//! ledger sequences for sharding across directories.

use std::sync::OnceLock;

use henyey_common::Hash256;

/// Default checkpoint frequency (production networks).
pub const DEFAULT_CHECKPOINT_FREQUENCY: u32 = 64;

/// Accelerated checkpoint frequency (for `ARTIFICIALLY_ACCELERATE_TIME_FOR_TESTING`).
pub const ACCELERATED_CHECKPOINT_FREQUENCY: u32 = 8;

/// Global checkpoint frequency, initialized once at startup.
///
/// Defaults to 64 for production networks. Set to 8 when
/// `ARTIFICIALLY_ACCELERATE_TIME_FOR_TESTING=true`.
static CHECKPOINT_FREQ: OnceLock<u32> = OnceLock::new();

/// Set the global checkpoint frequency. Must be called once at startup before
/// any checkpoint math is performed. Subsequent calls are ignored (first write wins).
pub fn set_checkpoint_frequency(freq: u32) {
    let _ = CHECKPOINT_FREQ.set(freq);
}

/// Get the current checkpoint frequency.
#[inline]
pub fn checkpoint_frequency() -> u32 {
    *CHECKPOINT_FREQ
        .get()
        .unwrap_or(&DEFAULT_CHECKPOINT_FREQUENCY)
}

/// Legacy constant alias for compatibility — use `checkpoint_frequency()` for
/// runtime-configurable behavior.
pub const CHECKPOINT_FREQUENCY: u32 = 64;

/// Calculate the checkpoint ledger for a given sequence.
///
/// Checkpoint ledgers are of the form `(n * 64) + 63`, i.e., 63, 127, 191, etc.
/// This function rounds a ledger sequence to its corresponding checkpoint.
///
/// # Examples
///
/// ```
/// use henyey_history::paths::checkpoint_ledger;
///
/// assert_eq!(checkpoint_ledger(0), 63);
/// assert_eq!(checkpoint_ledger(63), 63);
/// assert_eq!(checkpoint_ledger(64), 127);
/// assert_eq!(checkpoint_ledger(127), 127);
/// assert_eq!(checkpoint_ledger(128), 191);
/// ```
#[inline]
pub fn checkpoint_ledger(seq: u32) -> u32 {
    let freq = checkpoint_frequency();
    (seq / freq) * freq + (freq - 1)
}

/// Check if a ledger sequence is a checkpoint ledger.
///
/// # Examples
///
/// ```
/// use henyey_history::paths::is_checkpoint_ledger;
///
/// assert!(is_checkpoint_ledger(63));
/// assert!(is_checkpoint_ledger(127));
/// assert!(!is_checkpoint_ledger(64));
/// assert!(!is_checkpoint_ledger(100));
/// ```
#[inline]
pub fn is_checkpoint_ledger(seq: u32) -> bool {
    (seq + 1) % checkpoint_frequency() == 0
}

/// Generate the path for a checkpoint file.
///
/// The path format is: `{category}/{xx}/{yy}/{zz}/{category}-{hex}.{ext}`
/// where `{xx}/{yy}/{zz}` are the first three bytes of the hex-encoded checkpoint ledger.
///
/// # Arguments
///
/// * `category` - The file category (e.g., "history", "ledger", "transactions", "results", "scp")
/// * `ledger` - The ledger sequence (will be rounded to checkpoint)
/// * `ext` - The file extension (e.g., "json", "xdr.gz")
///
/// # Examples
///
/// ```
/// use henyey_history::paths::checkpoint_path;
///
/// // Ledger 63 (first checkpoint)
/// assert_eq!(
///     checkpoint_path("ledger", 63, "xdr.gz"),
///     "ledger/00/00/00/ledger-0000003f.xdr.gz"
/// );
///
/// // Ledger 127 (second checkpoint)
/// assert_eq!(
///     checkpoint_path("history", 127, "json"),
///     "history/00/00/00/history-0000007f.json"
/// );
///
/// // Any ledger in a checkpoint gets rounded
/// assert_eq!(
///     checkpoint_path("transactions", 100, "xdr.gz"),
///     "transactions/00/00/00/transactions-0000007f.xdr.gz"
/// );
/// ```
pub fn checkpoint_path(category: &str, ledger: u32, ext: &str) -> String {
    let checkpoint = checkpoint_ledger(ledger);
    let hex = format!("{:08x}", checkpoint);

    format!(
        "{}/{}/{}/{}/{}-{}.{}",
        category,
        &hex[0..2],
        &hex[2..4],
        &hex[4..6],
        category,
        hex,
        ext
    )
}

/// Generate the path for a bucket file.
///
/// The path format is: `bucket/{xx}/{yy}/{zz}/bucket-{hash}.xdr.gz`
/// where `{xx}/{yy}/{zz}` are the first three bytes of the hex-encoded hash.
///
/// # Arguments
///
/// * `hash` - The bucket hash
///
/// # Examples
///
/// ```
/// use henyey_common::Hash256;
/// use henyey_history::paths::bucket_path;
///
/// let hash = Hash256::from_hex("e113f8cc5468579cb57538e3204c8d3ecce59a0cdb47f6fa7e87ab4d9d8146fd").unwrap();
/// assert_eq!(
///     bucket_path(&hash),
///     "bucket/e1/13/f8/bucket-e113f8cc5468579cb57538e3204c8d3ecce59a0cdb47f6fa7e87ab4d9d8146fd.xdr.gz"
/// );
/// ```
pub fn bucket_path(hash: &Hash256) -> String {
    let hex = hash.to_hex();

    format!(
        "bucket/{}/{}/{}/bucket-{}.xdr.gz",
        &hex[0..2],
        &hex[2..4],
        &hex[4..6],
        hex
    )
}

/// Generate the path for the root history archive state file.
pub fn root_has_path() -> &'static str {
    ".well-known/stellar-history.json"
}

/// Generate the directory path for ledger-related files at a checkpoint.
///
/// Returns: `ledger/{xx}/{yy}/{zz}`
pub fn ledger_dir(ledger: u32) -> String {
    let checkpoint = checkpoint_ledger(ledger);
    let hex = format!("{:08x}", checkpoint);

    format!("ledger/{}/{}/{}", &hex[0..2], &hex[2..4], &hex[4..6])
}

/// Generate the path for a checkpoint file without extension.
///
/// Returns: `{category}/{xx}/{yy}/{zz}/{category}-{hex}`
pub fn checkpoint_file_path(ledger: u32, file_type: &str) -> String {
    let checkpoint = checkpoint_ledger(ledger);
    let hex = format!("{:08x}", checkpoint);

    format!(
        "{}/{}/{}/{}/{}-{}",
        file_type,
        &hex[0..2],
        &hex[2..4],
        &hex[4..6],
        file_type,
        hex
    )
}

/// Generate the path for a History Archive State file.
///
/// Returns: `history/{xx}/{yy}/{zz}/history-{hex}.json`
pub fn has_path(ledger: u32) -> String {
    checkpoint_path("history", ledger, "json")
}

// ============================================================================
// Dirty file helpers for crash-safe checkpoint building
// ============================================================================

/// Generate the dirty (temporary) path for a checkpoint file.
///
/// Dirty files are used during checkpoint building for crash safety.
/// The checkpoint builder writes to `.dirty` files first, then atomically
/// renames them to final paths on commit.
///
/// # Examples
///
/// ```
/// use henyey_history::paths::checkpoint_path_dirty;
///
/// assert_eq!(
///     checkpoint_path_dirty("ledger", 63, "xdr.gz"),
///     "ledger/00/00/00/ledger-0000003f.xdr.gz.dirty"
/// );
/// ```
pub fn checkpoint_path_dirty(category: &str, ledger: u32, ext: &str) -> String {
    format!("{}.dirty", checkpoint_path(category, ledger, ext))
}

/// Check if a path is a dirty checkpoint file.
///
/// A dirty path must end with `.dirty` and have a meaningful base name
/// (i.e., not just ".dirty" by itself).
///
/// # Examples
///
/// ```
/// use henyey_history::paths::is_dirty_path;
/// use std::path::Path;
///
/// assert!(is_dirty_path(Path::new("ledger/00/00/00/ledger-0000003f.xdr.gz.dirty")));
/// assert!(!is_dirty_path(Path::new("ledger/00/00/00/ledger-0000003f.xdr.gz")));
/// assert!(!is_dirty_path(Path::new(".dirty"))); // Just ".dirty" is not valid
/// ```
pub fn is_dirty_path(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|f| f.to_str())
        .is_some_and(|name| name.ends_with(".dirty") && name.len() > 6)
}

/// Convert a dirty path to its final path by removing the `.dirty` suffix.
///
/// # Examples
///
/// ```
/// use henyey_history::paths::dirty_to_final_path;
/// use std::path::Path;
///
/// let dirty = Path::new("ledger/00/00/00/ledger-0000003f.xdr.gz.dirty");
/// let final_path = dirty_to_final_path(dirty).unwrap();
/// assert_eq!(final_path.to_str().unwrap(), "ledger/00/00/00/ledger-0000003f.xdr.gz");
/// ```
pub fn dirty_to_final_path(dirty_path: &std::path::Path) -> Option<std::path::PathBuf> {
    let s = dirty_path.to_str()?;
    s.strip_suffix(".dirty").map(std::path::PathBuf::from)
}

/// Convert a final path to its dirty path by adding the `.dirty` suffix.
pub fn final_to_dirty_path(final_path: &std::path::Path) -> std::path::PathBuf {
    let mut dirty = final_path.to_path_buf();
    let file_name = dirty.file_name().unwrap().to_str().unwrap();
    dirty.set_file_name(format!("{}.dirty", file_name));
    dirty
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_checkpoint_ledger() {
        // First checkpoint
        assert_eq!(checkpoint_ledger(0), 63);
        assert_eq!(checkpoint_ledger(1), 63);
        assert_eq!(checkpoint_ledger(63), 63);

        // Second checkpoint
        assert_eq!(checkpoint_ledger(64), 127);
        assert_eq!(checkpoint_ledger(100), 127);
        assert_eq!(checkpoint_ledger(127), 127);

        // Third checkpoint
        assert_eq!(checkpoint_ledger(128), 191);
        assert_eq!(checkpoint_ledger(191), 191);

        // Large checkpoints
        assert_eq!(checkpoint_ledger(1000000), 1000063);
    }

    #[test]
    fn test_is_checkpoint_ledger() {
        assert!(is_checkpoint_ledger(63));
        assert!(is_checkpoint_ledger(127));
        assert!(is_checkpoint_ledger(191));
        assert!(is_checkpoint_ledger(1000063));

        assert!(!is_checkpoint_ledger(0));
        assert!(!is_checkpoint_ledger(64));
        assert!(!is_checkpoint_ledger(100));
        assert!(!is_checkpoint_ledger(1000000));
    }

    #[test]
    fn test_checkpoint_path() {
        // First checkpoint (ledger 63 = 0x3f)
        assert_eq!(
            checkpoint_path("ledger", 63, "xdr.gz"),
            "ledger/00/00/00/ledger-0000003f.xdr.gz"
        );

        // Second checkpoint (ledger 127 = 0x7f)
        assert_eq!(
            checkpoint_path("history", 127, "json"),
            "history/00/00/00/history-0000007f.json"
        );

        // Rounds to checkpoint
        assert_eq!(
            checkpoint_path("transactions", 100, "xdr.gz"),
            "transactions/00/00/00/transactions-0000007f.xdr.gz"
        );

        // Large checkpoint (ledger 16777215 = 0xffffff)
        assert_eq!(
            checkpoint_path("results", 16777215, "xdr.gz"),
            "results/00/ff/ff/results-00ffffff.xdr.gz"
        );

        // Very large checkpoint
        assert_eq!(
            checkpoint_path("scp", 0x12345678 + 63, "xdr.gz"),
            "scp/12/34/56/scp-123456bf.xdr.gz"
        );
    }

    #[test]
    fn test_bucket_path() {
        let hash =
            Hash256::from_hex("e113f8cc5468579cb57538e3204c8d3ecce59a0cdb47f6fa7e87ab4d9d8146fd")
                .unwrap();

        assert_eq!(
            bucket_path(&hash),
            "bucket/e1/13/f8/bucket-e113f8cc5468579cb57538e3204c8d3ecce59a0cdb47f6fa7e87ab4d9d8146fd.xdr.gz"
        );

        // Zero hash
        assert_eq!(
            bucket_path(&Hash256::ZERO),
            "bucket/00/00/00/bucket-0000000000000000000000000000000000000000000000000000000000000000.xdr.gz"
        );
    }

    #[test]
    fn test_root_has_path() {
        assert_eq!(root_has_path(), ".well-known/stellar-history.json");
    }

    #[test]
    fn test_checkpoint_path_dirty() {
        assert_eq!(
            checkpoint_path_dirty("ledger", 63, "xdr.gz"),
            "ledger/00/00/00/ledger-0000003f.xdr.gz.dirty"
        );
        assert_eq!(
            checkpoint_path_dirty("transactions", 127, "xdr.gz"),
            "transactions/00/00/00/transactions-0000007f.xdr.gz.dirty"
        );
    }

    #[test]
    fn test_is_dirty_path() {
        use std::path::Path;

        assert!(is_dirty_path(Path::new(
            "ledger/00/00/00/ledger-0000003f.xdr.gz.dirty"
        )));
        assert!(is_dirty_path(Path::new("foo.dirty")));

        assert!(!is_dirty_path(Path::new(
            "ledger/00/00/00/ledger-0000003f.xdr.gz"
        )));
        assert!(!is_dirty_path(Path::new("foo.txt")));
        assert!(!is_dirty_path(Path::new(".dirty")));
    }

    #[test]
    fn test_dirty_to_final_path() {
        use std::path::Path;

        let dirty = Path::new("ledger/00/00/00/ledger-0000003f.xdr.gz.dirty");
        let final_path = dirty_to_final_path(dirty).unwrap();
        assert_eq!(
            final_path.to_str().unwrap(),
            "ledger/00/00/00/ledger-0000003f.xdr.gz"
        );

        // Non-dirty path returns None
        assert!(dirty_to_final_path(Path::new("foo.txt")).is_none());
    }

    #[test]
    fn test_final_to_dirty_path() {
        use std::path::Path;

        let final_path = Path::new("ledger/00/00/00/ledger-0000003f.xdr.gz");
        let dirty = final_to_dirty_path(final_path);
        assert_eq!(
            dirty.to_str().unwrap(),
            "ledger/00/00/00/ledger-0000003f.xdr.gz.dirty"
        );
    }
}
