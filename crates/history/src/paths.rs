//! URL path generation for history archive files.
//!
//! History archives use a hierarchical path structure with hex-encoded
//! ledger sequences for sharding across directories.
//!
//! Checkpoint math (frequency, rounding, boundary queries) lives in
//! [`crate::checkpoint`]. This module uses those primitives to build
//! archive URL paths.

use crate::checkpoint::checkpoint_ledger;
use henyey_common::Hash256;

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

/// Generate the path for the §4.3 ledger-zero pseudo-checkpoint HAS file.
///
/// Returns the literal `"history/00/00/00/history-00000000.json"`.
///
/// This intentionally does NOT route through [`has_path`]/[`checkpoint_path`]
/// because [`crate::checkpoint::checkpoint_ledger`] rounds `0` up to the first
/// real checkpoint (`63 == 0x3f`), which would yield
/// `history-0000003f.json` — the wrong file.
///
/// The pseudo-checkpoint is written by `initialize_history_archive` alongside
/// the well-known root HAS to mark an archive as initialized. See
/// stellar-core's `PutHistoryArchiveStateWork::spawnPublishWork` and
/// `HistoryManager.h` §4.3.
pub fn pseudo_checkpoint_has_path() -> &'static str {
    "history/00/00/00/history-00000000.json"
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
    fn test_has_path() {
        let path = has_path(127);
        assert_eq!(path, "history/00/00/00/history-0000007f.json");

        let path = has_path(0xaabbcc00 + 63);
        assert!(path.starts_with("history/"));
        assert!(path.ends_with(".json"));
    }

    #[test]
    fn test_pseudo_checkpoint_has_path_literal() {
        // Regression guard: this path MUST be the literal ledger-0 file, not
        // the rounded-up `history-0000003f.json` that `has_path(0)` would
        // produce via `checkpoint_ledger(0) == 63`.
        assert_eq!(
            pseudo_checkpoint_has_path(),
            "history/00/00/00/history-00000000.json"
        );
        assert_ne!(pseudo_checkpoint_has_path(), has_path(0));
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
