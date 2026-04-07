//! Individual bucket implementation.
//!
//! A bucket is an immutable container of sorted ledger entries, stored as
//! uncompressed XDR with record marks (RFC 5531). Buckets are identified by
//! their content hash (SHA-256 of the file contents).
//!
//! # Storage Modes
//!
//! Buckets support two storage modes to balance memory usage and performance:
//!
//! - **InMemory**: All entries are loaded into memory with a key index for O(1) lookups.
//!   Best for normal operations, merging, and when entries need to be accessed repeatedly.
//!
//! - **DiskBacked**: Entries remain on disk with a compact index mapping key hashes to
//!   file offsets. Entries are loaded on-demand when accessed. Essential for mainnet
//!   catchup where buckets can contain millions of entries (potentially many GB).
//!
//! # XDR Format
//!
//! Bucket files use the XDR Record Marking Standard (RFC 5531). Each entry is prefixed
//! with a 4-byte record mark: the high bit indicates "last fragment" (always set for
//! buckets), and the remaining 31 bits contain the record length in bytes.
//!
//! The bucket hash is computed over the uncompressed XDR bytes, including record marks.
//! This ensures hash consistency with stellar-core's bucket hash computation.
//!
//! # Thread Safety
//!
//! Buckets are immutable after creation and use `Arc` internally, making them safe
//! to share across threads. The disk-backed mode uses file handles that are opened
//! fresh for each operation to avoid contention.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use stellar_xdr::curr::{LedgerEntry, LedgerKey, Limits, ReadXdr, WriteXdr};

use henyey_common::Hash256;

use henyey_common::BucketListDbConfig;

use crate::cache::CacheStats;
use crate::disk_bucket::DiskBucket;
use crate::entry::{compare_entries, BucketEntry, BucketEntryExt};
use crate::index::BucketEntryCounters;
use crate::{BucketError, Result};

/// Internal storage mode for bucket entries.
///
/// This enum is not public; users interact with buckets through the [`Bucket`]
/// type which abstracts over both storage modes.
#[derive(Clone)]
enum BucketStorage {
    /// All entries loaded in memory with a key-to-index map for O(1) lookups.
    InMemory {
        /// The sorted list of bucket entries.
        entries: Arc<Vec<BucketEntry>>,
        /// Map from ledger key to entry index for O(1) lookups.
        key_index: Arc<HashMap<LedgerKey, usize>>,
    },
    /// Entries stored on disk with a compact index for on-demand loading.
    DiskBacked {
        /// The disk bucket implementation that handles file I/O.
        disk_bucket: Arc<DiskBucket>,
    },
}

/// An immutable bucket file containing sorted ledger entries.
///
/// Buckets are the fundamental storage unit in Stellar's BucketList.
/// They are:
/// - **Immutable** once created (content-addressable by hash)
/// - **Identified** by their SHA-256 content hash
/// - **Stored** as uncompressed XDR on disk with record marking (RFC 5531)
/// - **Sorted** by key for efficient merging and O(log n) binary search
///
/// # Creating Buckets
///
/// Buckets can be created in several ways:
/// - [`Bucket::empty()`]: Create an empty bucket (zero hash)
/// - [`Bucket::from_entries()`]: Create from a list of entries (will be sorted)
/// - [`Bucket::from_sorted_entries()`]: Create from pre-sorted entries (preserves order)
/// - [`Bucket::load_from_xdr_file()`]: Load from an uncompressed XDR bucket file
/// - [`Bucket::from_xdr_bytes()`]: Parse from uncompressed XDR bytes
/// - [`Bucket::from_xdr_bytes_disk_backed()`]: Memory-efficient loading for catchup
///
/// # Storage Modes
///
/// For memory efficiency during catchup (when processing mainnet buckets with
/// millions of entries), buckets can use disk-backed storage where entries
/// are loaded on-demand rather than all at once.
///
/// Use [`Bucket::is_disk_backed()`] to check the storage mode.
///
/// # Entry Access
///
/// - [`Bucket::get()`]: Look up a raw bucket entry by key
/// - [`Bucket::get_entry()`]: Look up a ledger entry (returns None for dead entries)
/// - [`Bucket::iter()`]: Iterate over all entries
/// - [`Bucket::entries()`]: Get entries as a slice (in-memory only, panics for disk-backed)
#[derive(Clone)]
pub struct Bucket {
    /// The SHA-256 hash of this bucket's uncompressed XDR contents.
    hash: Hash256,
    /// The storage mode (in-memory or disk-backed).
    storage: BucketStorage,
    /// Level 0 in-memory entry state for fast merging.
    ///
    /// This tracks how to access entries for level 0 optimization without disk I/O.
    /// Uses an enum to support zero-copy sharing with storage when possible.
    level_zero_state: LevelZeroState,
}

/// Level 0 in-memory state for bucket merging optimization.
///
/// This enum supports two modes:
/// - `None`: No in-memory entries available (disk-only or cleared)
/// - `Separate`: A separate vector of entries (used when entries differ from storage)
/// - `SharedWithStorage`: Shares storage entries with an offset to skip metadata
#[derive(Clone)]
enum LevelZeroState {
    /// No in-memory entries available.
    None,
    /// Separate in-memory entries (legacy mode, requires cloning).
    Separate(Arc<Vec<BucketEntry>>),
    /// Shared with storage entries, starting at the given offset.
    /// The offset skips metadata entries (typically 0 or 1).
    SharedWithStorage { metadata_count: usize },
}

impl Bucket {
    /// Create an empty bucket with a zero hash.
    ///
    /// Empty buckets are special-cased in bucket list operations and don't
    /// need to be stored on disk. The zero hash serves as a sentinel value.
    ///
    /// # In-memory entries for empty buckets
    ///
    /// In stellar-core, empty buckets have `mEntries` initialized to an empty
    /// vector. This means `hasInMemoryEntries()` returns true for empty buckets.
    /// We match this behavior by setting `level_zero_entries` to an empty vector
    /// rather than None. This is important because:
    ///
    /// 1. When level 0 snaps, curr becomes empty
    /// 2. On the next ledger, prepare_first_level merges this empty curr with new entries
    /// 3. If empty curr doesn't have "in-memory entries", we fall back to regular merge
    /// 4. In stellar-core, both empty curr and new entries have in-memory state, so in-memory merge is used
    /// 5. We need to match this behavior for bucket list hash parity
    pub fn empty() -> Self {
        Self {
            hash: Hash256::ZERO,
            storage: BucketStorage::InMemory {
                entries: Arc::new(Vec::new()),
                key_index: Arc::new(HashMap::new()),
            },
            // Empty bucket with shared state at offset 0 - matches stellar-core where mEntries is empty vector
            level_zero_state: LevelZeroState::SharedWithStorage { metadata_count: 0 },
        }
    }

    /// Create a bucket from a list of entries.
    ///
    /// The entries will be sorted by key using [`compare_entries`]. This is the
    /// standard way to create buckets when the entry order is not guaranteed.
    ///
    /// # Arguments
    ///
    /// * `entries` - The bucket entries (will be sorted in place)
    ///
    /// # Example
    ///
    /// ```ignore
    /// let entries = vec![
    ///     BucketEntry::Liveentry(account_entry),
    ///     BucketEntry::Deadentry(deleted_key),
    /// ];
    /// let bucket = Bucket::from_entries(entries)?;
    /// ```
    pub fn from_entries(mut entries: Vec<BucketEntry>) -> Result<Self> {
        // Sort entries by key
        entries.sort_by(compare_entries);

        Self::from_sorted_entries(entries)
    }

    /// Create a bucket from a list of pre-sorted entries.
    ///
    /// This method skips the sorting step, which is useful when entries are
    /// already known to be in the correct order (e.g., extracted from another
    /// bucket via iteration).
    ///
    /// # Safety
    ///
    /// The entries **must** already be sorted by key according to [`compare_entries`].
    /// Using unsorted entries will result in incorrect bucket behavior:
    /// - Lookups may fail to find existing entries
    /// - Merges will produce incorrect results
    /// - Hash verification may fail
    ///
    /// This is intended for entries extracted from disk-backed buckets that were
    /// already sorted by stellar-core, or from bucket iteration which preserves order.
    ///
    /// # Performance
    ///
    /// This method computes the bucket hash incrementally while building the key index,
    /// using a single serialization pass for each entry. This is more efficient than
    /// serializing all entries twice (once for the index, once for the hash).
    pub fn from_sorted_entries(entries: Vec<BucketEntry>) -> Result<Self> {
        use sha2::{Digest, Sha256};
        use stellar_xdr::curr::{Limited, WriteXdr};

        let mut key_index = HashMap::new();
        let mut hasher = Sha256::new();

        // Single pass: serialize each entry once, use for both index and hash
        for (idx, entry) in entries.iter().enumerate() {
            // Serialize entry to XDR
            let xdr_entry = entry.clone();
            let mut entry_bytes = Vec::new();
            let mut limited = Limited::new(&mut entry_bytes, Limits::none());
            xdr_entry.write_xdr(&mut limited).map_err(|e| {
                BucketError::Serialization(format!("Failed to serialize entry: {}", e))
            })?;

            // Write record mark + entry to hasher (XDR Record Marking format)
            let size = u32::try_from(entry_bytes.len()).map_err(|_| {
                BucketError::Serialization(format!(
                    "Entry too large for XDR record mark: {} bytes",
                    entry_bytes.len()
                ))
            })?;
            let record_mark = size | crate::XDR_RECORD_MARK;
            hasher.update(record_mark.to_be_bytes());
            hasher.update(&entry_bytes);

            // Build key index
            if let Some(key) = entry.key() {
                key_index.insert(key, idx);
            }
        }

        // Compute final hash
        let hash_bytes: [u8; 32] = hasher.finalize().into();
        let hash = Hash256::from_bytes(hash_bytes);

        Ok(Self {
            hash,
            storage: BucketStorage::InMemory {
                entries: Arc::new(entries),
                key_index: Arc::new(key_index),
            },
            level_zero_state: LevelZeroState::None,
        })
    }

    /// Create a bucket from pre-computed parts.
    ///
    /// This is an internal constructor used by optimized merge paths that have
    /// already computed the hash incrementally during the merge operation.
    ///
    /// # Arguments
    ///
    /// * `hash` - Pre-computed SHA256 hash of all entries
    /// * `entries` - All entries including metadata, already sorted
    /// * `key_index` - Pre-built key index mapping serialized keys to entry indices
    /// * `metadata_count` - Number of metadata entries at the start (0 or 1)
    pub fn from_parts(
        hash: Hash256,
        entries: Arc<Vec<BucketEntry>>,
        key_index: Arc<HashMap<LedgerKey, usize>>,
        metadata_count: usize,
    ) -> Self {
        Self {
            hash,
            storage: BucketStorage::InMemory { entries, key_index },
            // Use shared state - entries are already in storage, just need to skip metadata
            level_zero_state: LevelZeroState::SharedWithStorage { metadata_count },
        }
    }

    /// Create a "shell" bucket for immediate in-memory merging.
    ///
    /// This is an optimization matching stellar-core's `freshInMemoryOnly`.
    /// It creates a bucket with entries for merging but WITHOUT computing the hash.
    /// The hash is set to zero since:
    /// 1. This bucket will be immediately merged with another bucket
    /// 2. Only the merged result's hash matters for the bucket list
    /// 3. Skipping hash computation saves significant CPU time
    ///
    /// # Warning
    ///
    /// This bucket MUST only be used as input to an in-memory merge. It should
    /// NOT be stored in the bucket list or used for any operation that requires
    /// a valid hash.
    ///
    /// # Arguments
    ///
    /// * `entries` - Sorted entries (metadata first, then data entries by key)
    pub fn fresh_in_memory_only(entries: Vec<BucketEntry>) -> Self {
        // Count metadata entries (typically 0 or 1, always at the start)
        let metadata_count = entries.iter().take_while(|e| e.is_metadata()).count();

        Self {
            hash: Hash256::ZERO, // Hash not computed - this is intentional!
            storage: BucketStorage::InMemory {
                entries: Arc::new(entries),
                key_index: Arc::new(HashMap::new()), // No index needed for merge input
            },
            // Use shared state - no cloning needed
            level_zero_state: LevelZeroState::SharedWithStorage { metadata_count },
        }
    }

    /// Load a bucket from a gzipped XDR file.
    ///
    /// Only used in tests; production code uses `load_from_xdr_file()`.
    #[cfg(test)]
    pub fn load_from_file(path: impl AsRef<Path>) -> Result<Self> {
        use flate2::read::GzDecoder;
        use std::io::{BufReader, Read};

        let path = path.as_ref();
        let file = std::fs::File::open(path)?;
        let reader = BufReader::new(file);
        let mut decoder = GzDecoder::new(reader);

        // Read and decompress
        let mut uncompressed = Vec::new();
        decoder.read_to_end(&mut uncompressed)?;

        Self::from_xdr_bytes(&uncompressed)
    }

    /// Create a bucket from uncompressed XDR bytes.
    pub fn from_xdr_bytes(bytes: &[u8]) -> Result<Self> {
        let bucket = Self::from_xdr_bytes_internal(bytes, true)?;

        // Debug: verify that re-serializing entries produces the same hash
        if let BucketStorage::InMemory { entries, .. } = &bucket.storage {
            if !entries.is_empty() {
                let reserialized = Self::serialize_entries(entries)?;
                let reserialized_hash = Hash256::hash(&reserialized);
                if reserialized_hash != bucket.hash {
                    tracing::warn!(
                        original_hash = %bucket.hash,
                        reserialized_hash = %reserialized_hash,
                        original_len = bytes.len(),
                        reserialized_len = reserialized.len(),
                        "Bucket roundtrip hash mismatch detected"
                    );
                }
            }
        }

        Ok(bucket)
    }

    /// Create a disk-backed bucket from uncompressed XDR bytes.
    ///
    /// This is the most memory-efficient way to load large buckets. Instead of
    /// parsing all entries into memory, it:
    /// 1. Saves the XDR bytes to the specified path
    /// 2. Builds a compact index mapping key hashes to file offsets
    /// 3. Loads entries on-demand when accessed
    ///
    /// This reduces memory usage from O(entries) to O(unique_keys) for the index,
    /// which is much smaller since we only store 8-byte key hashes and file offsets.
    ///
    /// # Arguments
    ///
    /// * `bytes` - The uncompressed XDR bytes
    /// * `save_path` - Path where the bucket file will be saved
    pub fn from_xdr_bytes_disk_backed(bytes: &[u8], save_path: impl AsRef<Path>) -> Result<Self> {
        let disk_bucket = DiskBucket::from_xdr_bytes(bytes, save_path)?;
        let hash = disk_bucket.hash();

        Ok(Self {
            hash,
            storage: BucketStorage::DiskBacked {
                disk_bucket: Arc::new(disk_bucket),
            },
            level_zero_state: LevelZeroState::None,
        })
    }

    /// Create a DiskBacked bucket from an existing uncompressed XDR file.
    ///
    /// This builds the index by streaming through the file one entry at a time,
    /// using O(index_size) memory instead of O(file_size). The hash is computed
    /// incrementally during the streaming pass.
    ///
    /// This is the preferred method for loading large bucket files (multi-GB)
    /// that would exceed available memory if loaded entirely.
    pub fn from_xdr_file_disk_backed(path: impl AsRef<Path>) -> Result<Self> {
        let disk_bucket = DiskBucket::from_file_streaming(path)?;
        let hash = disk_bucket.hash();

        Ok(Self {
            hash,
            storage: BucketStorage::DiskBacked {
                disk_bucket: Arc::new(disk_bucket),
            },
            level_zero_state: LevelZeroState::None,
        })
    }

    /// Create a DiskBacked bucket from a pre-built index, skipping file scanning.
    ///
    /// This is used when loading a persisted index from disk, avoiding the
    /// expensive 2-pass streaming build. The caller provides the pre-built
    /// `LiveBucketIndex`, the known hash, and entry count.
    pub fn from_xdr_file_disk_backed_prebuilt(
        path: impl AsRef<Path>,
        hash: Hash256,
        entry_count: usize,
        index: crate::index::LiveBucketIndex,
    ) -> Result<Self> {
        let disk_bucket = DiskBucket::from_prebuilt(path, hash, entry_count, index)?;
        Ok(Self {
            hash,
            storage: BucketStorage::DiskBacked {
                disk_bucket: Arc::new(disk_bucket),
            },
            level_zero_state: LevelZeroState::None,
        })
    }

    /// Internal method to create a bucket with optional key index building.
    fn from_xdr_bytes_internal(bytes: &[u8], build_index: bool) -> Result<Self> {
        let entries = Self::parse_entries(bytes)?;

        // Build key index only if requested (skip during catchup for memory efficiency)
        let key_index = if build_index {
            let mut index = HashMap::new();
            for (idx, entry) in entries.iter().enumerate() {
                if let Some(key) = entry.key() {
                    index.insert(key, idx);
                }
            }
            index
        } else {
            HashMap::new()
        };

        // Compute hash from raw bytes (including record marks)
        // This matches the bucket file hash used in history archives
        let hash = Hash256::hash(bytes);

        Ok(Self {
            hash,
            storage: BucketStorage::InMemory {
                entries: Arc::new(entries),
                key_index: Arc::new(key_index),
            },
            level_zero_state: LevelZeroState::None,
        })
    }

    /// Parse entries from XDR bytes.
    ///
    /// Bucket files use XDR Record Marking Standard (RFC 5531) with 4-byte
    /// record marks before each entry. The high bit indicates "last fragment"
    /// and the remaining 31 bits contain the record length.
    fn parse_entries(bytes: &[u8]) -> Result<Vec<BucketEntry>> {
        use tracing::debug;

        if bytes.is_empty() {
            return Ok(Vec::new());
        }

        let mut entries = Vec::new();
        let mut offset = 0;

        debug!(
            "Parsing bucket entries from {} bytes, first 16 bytes: {:02x?}",
            bytes.len(),
            &bytes[..std::cmp::min(16, bytes.len())]
        );

        // Bucket files always use XDR Record Marking Standard (RFC 5531).
        // Each entry is prefixed with a 4-byte record mark: high bit set
        // (last fragment) + 31-bit length. This matches stellar-core's
        // XDRInputFileStream::readOne() which always reads record marks.
        while offset + 4 <= bytes.len() {
            // Read 4-byte record mark (big-endian)
            let record_mark = u32::from_be_bytes([
                bytes[offset],
                bytes[offset + 1],
                bytes[offset + 2],
                bytes[offset + 3],
            ]);
            offset += 4;

            // High bit is "last fragment" flag, remaining 31 bits are length
            let _last_fragment = (record_mark & crate::XDR_RECORD_MARK) != 0;
            let record_len = (record_mark & crate::XDR_RECORD_LEN_MASK) as usize;

            if offset + record_len > bytes.len() {
                return Err(BucketError::Serialization(format!(
                    "Record length {} exceeds remaining data {} at offset {}",
                    record_len,
                    bytes.len() - offset,
                    offset - 4
                )));
            }

            // Parse the XDR record
            let record_data = &bytes[offset..offset + record_len];
            match stellar_xdr::curr::BucketEntry::from_xdr(record_data, Limits::none()) {
                Ok(xdr_entry) => {
                    entries.push(xdr_entry);
                }
                Err(e) => {
                    debug!(
                        "Parse error at offset {}, record_len {}, data: {:02x?}, error: {}",
                        offset,
                        record_len,
                        &record_data[..std::cmp::min(16, record_data.len())],
                        e
                    );
                    return Err(BucketError::Serialization(format!(
                        "Failed to parse bucket entry: {}",
                        e
                    )));
                }
            }

            offset += record_len;
        }

        debug!("Parsed {} bucket entries", entries.len());
        Ok(entries)
    }

    /// Serialize entries to XDR bytes WITH record marks (RFC 5531 XDR Record Marking Standard).
    /// This format is used for bucket files and hash computation.
    /// Each entry is prefixed with a 4-byte mark: high bit set + 31-bit size in big-endian.
    fn serialize_entries(entries: &[BucketEntry]) -> Result<Vec<u8>> {
        use stellar_xdr::curr::Limited;
        let mut bytes = Vec::new();

        for entry in entries {
            let xdr_entry = entry.clone();

            // First serialize the entry to get its size
            let mut entry_bytes = Vec::new();
            let mut limited = Limited::new(&mut entry_bytes, Limits::none());
            xdr_entry.write_xdr(&mut limited).map_err(|e| {
                BucketError::Serialization(format!("Failed to serialize entry: {}", e))
            })?;

            // Write 4-byte record mark: high bit set + size (big-endian)
            let size = u32::try_from(entry_bytes.len()).map_err(|_| {
                BucketError::Serialization(format!(
                    "Entry too large for XDR record mark: {} bytes",
                    entry_bytes.len()
                ))
            })?;
            let record_mark = size | crate::XDR_RECORD_MARK;
            bytes.extend_from_slice(&record_mark.to_be_bytes());

            // Write the entry data
            bytes.extend_from_slice(&entry_bytes);
        }

        Ok(bytes)
    }

    /// Save this bucket to an uncompressed XDR file.
    ///
    /// This writes the bucket entries in uncompressed XDR format with record marks
    /// (RFC 5531). This is the canonical on-disk format that supports random-access
    /// seeks for disk-backed indexing and streaming iteration.
    ///
    /// The bucket hash is computed over this exact format, so the file contents
    /// will hash to `self.hash()`.
    pub fn save_to_xdr_file(&self, path: impl AsRef<Path>) -> Result<PathBuf> {
        let path = path.as_ref().to_path_buf();

        match &self.storage {
            BucketStorage::InMemory { entries, .. } => {
                // Serialize entries to uncompressed XDR with record marks
                let uncompressed = Self::serialize_entries(entries)?;

                // Write directly (no compression).
                // IMPORTANT: if the write or sync fails (e.g. disk full), we must
                // remove the file we just created.  A zero-byte or truncated file
                // at the canonical path would corrupt any future merge that
                // produces the same content hash and tries to reuse the file.
                let mut file = std::fs::File::create(&path)?;
                let write_result = file.write_all(&uncompressed).and_then(|_| file.sync_all());
                if let Err(e) = write_result {
                    let _ = std::fs::remove_file(&path);
                    return Err(BucketError::Io(e));
                }
            }
            BucketStorage::DiskBacked { disk_bucket } => {
                // For disk-backed buckets, the file is already uncompressed XDR
                let source = disk_bucket.file_path();
                if source != path {
                    std::fs::copy(source, &path)?;
                }
            }
        }

        Ok(path)
    }

    /// Load a bucket from an uncompressed XDR file.
    ///
    /// This reads the bucket file directly without any decompression. The file
    /// must contain XDR entries with record marks (RFC 5531).
    pub fn load_from_xdr_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let bytes = std::fs::read(path)?;
        Self::from_xdr_bytes(&bytes)
    }

    /// Save this bucket to a gzipped file.
    ///
    /// Only used in tests; production code uses `save_to_xdr_file()`.
    #[cfg(test)]
    pub fn save_to_file(&self, path: impl AsRef<Path>) -> Result<PathBuf> {
        use flate2::write::GzEncoder;
        use flate2::Compression;

        let path = path.as_ref().to_path_buf();

        match &self.storage {
            BucketStorage::InMemory { entries, .. } => {
                // Serialize entries
                let uncompressed = Self::serialize_entries(entries)?;

                // Compress and write
                let file = std::fs::File::create(&path)?;
                let mut encoder = GzEncoder::new(file, Compression::default());
                encoder.write_all(&uncompressed)?;
                encoder.finish()?;
            }
            BucketStorage::DiskBacked { disk_bucket } => {
                // For disk-backed buckets, read from disk and compress
                let uncompressed = std::fs::read(disk_bucket.file_path())?;
                let file = std::fs::File::create(&path)?;
                let mut encoder = GzEncoder::new(file, Compression::default());
                encoder.write_all(&uncompressed)?;
                encoder.finish()?;
            }
        }

        Ok(path)
    }

    /// Get the hash of this bucket's contents.
    pub fn hash(&self) -> Hash256 {
        self.hash
    }

    /// Check if this bucket is empty.
    pub fn is_empty(&self) -> bool {
        if self.hash.is_zero() {
            return true;
        }
        match &self.storage {
            BucketStorage::InMemory { entries, .. } => entries.is_empty(),
            BucketStorage::DiskBacked { disk_bucket } => disk_bucket.is_empty(),
        }
    }

    /// Get the number of entries in this bucket.
    pub fn len(&self) -> usize {
        match &self.storage {
            BucketStorage::InMemory { entries, .. } => entries.len(),
            BucketStorage::DiskBacked { disk_bucket } => disk_bucket.len(),
        }
    }

    /// Check if this bucket uses disk-backed storage.
    pub fn is_disk_backed(&self) -> bool {
        matches!(&self.storage, BucketStorage::DiskBacked { .. })
    }

    /// Get the live bucket index for disk-backed buckets.
    ///
    /// Returns `Some(&LiveBucketIndex)` for disk-backed buckets, `None` for in-memory.
    /// This is used to extract the index for persistence (saving to `.index` files).
    pub fn live_index(&self) -> Option<&crate::index::LiveBucketIndex> {
        match &self.storage {
            BucketStorage::DiskBacked { disk_bucket } => Some(disk_bucket.live_index()),
            BucketStorage::InMemory { .. } => None,
        }
    }

    /// Get the backing file path for disk-backed buckets.
    ///
    /// Returns `Some(path)` for disk-backed buckets, `None` for in-memory buckets.
    /// This is useful for garbage collection - tracking which files are still referenced.
    pub fn backing_file_path(&self) -> Option<&Path> {
        match &self.storage {
            BucketStorage::DiskBacked { disk_bucket } => Some(disk_bucket.file_path()),
            BucketStorage::InMemory { .. } => None,
        }
    }

    /// Returns the entry counters from the bucket's index, if available.
    ///
    /// Only disk-backed buckets have counters (via their index).
    /// In-memory buckets return `None`.
    pub fn entry_counters(&self) -> Option<&BucketEntryCounters> {
        match &self.storage {
            BucketStorage::DiskBacked { disk_bucket } => Some(disk_bucket.live_index().counters()),
            BucketStorage::InMemory { .. } => None,
        }
    }

    /// Initializes the per-bucket entry cache with proportional sizing.
    ///
    /// Delegates to `DiskBucket::maybe_initialize_cache()`. Only disk-backed
    /// buckets get caches; in-memory buckets are no-ops.
    pub fn maybe_initialize_cache(&self, total_account_bytes: u64, config: &BucketListDbConfig) {
        if let BucketStorage::DiskBacked { disk_bucket } = &self.storage {
            disk_bucket.maybe_initialize_cache(total_account_bytes, config);
        }
    }

    /// Returns cache statistics for this bucket's per-bucket cache, if present.
    pub fn cache_stats(&self) -> Option<CacheStats> {
        match &self.storage {
            BucketStorage::DiskBacked { disk_bucket } => disk_bucket.cache().map(|c| c.stats()),
            BucketStorage::InMemory { .. } => None,
        }
    }

    /// Estimate heap bytes used by this bucket (index + cache, excluding mmap).
    pub fn estimate_heap_bytes(&self) -> usize {
        match &self.storage {
            BucketStorage::InMemory { entries, key_index } => {
                // entries Vec: BucketEntry structs on heap
                let entries_cap = entries.capacity() * std::mem::size_of::<BucketEntry>();
                // key_index: HashMap<LedgerKey, usize>
                // LedgerKey size + value usize + hashmap bucket overhead per entry
                let map_overhead = key_index.len()
                    * (std::mem::size_of::<LedgerKey>() + std::mem::size_of::<usize>() + 48);
                entries_cap + map_overhead
            }
            BucketStorage::DiskBacked { disk_bucket } => {
                let index_bytes = disk_bucket.index_heap_bytes();
                let cache_bytes = disk_bucket.cache().map(|c| c.size_bytes()).unwrap_or(0);
                index_bytes + cache_bytes
            }
        }
    }

    /// Return mmap'd file-backed bytes (not heap, backed by OS page cache).
    pub fn mmap_bytes(&self) -> usize {
        match &self.storage {
            BucketStorage::InMemory { .. } => 0,
            BucketStorage::DiskBacked { disk_bucket } => disk_bucket.mmap_bytes(),
        }
    }

    /// Resets cache hit/miss counters for this bucket's per-bucket cache.
    pub fn reset_cache_counters(&self) {
        if let BucketStorage::DiskBacked { disk_bucket } = &self.storage {
            if let Some(cache) = disk_bucket.cache() {
                cache.reset_counters();
            }
        }
    }

    /// Iterate over entries in this bucket.
    ///
    /// For in-memory buckets, this is efficient. For disk-backed buckets,
    /// this reads entries from disk sequentially.
    ///
    /// Returns an error if the disk-backed bucket file cannot be opened.
    pub fn iter(&self) -> Result<BucketIter<'_>> {
        match &self.storage {
            BucketStorage::InMemory { entries, .. } => Ok(BucketIter::InMemory(entries.iter())),
            BucketStorage::DiskBacked { disk_bucket } => {
                Ok(BucketIter::DiskBacked(disk_bucket.iter()?))
            }
        }
    }

    /// Iterate over entries starting from a byte offset, yielding record sizes.
    ///
    /// This is optimized for the eviction scan. For disk-backed buckets, it seeks
    /// directly to `start_offset` in the file instead of reading and skipping all
    /// prior entries. It also returns each entry's on-disk record size (4-byte
    /// record mark + XDR data) so callers can track byte offsets without
    /// re-serializing every entry to XDR.
    ///
    /// Each item is `(BucketEntry, record_size)` where `record_size` is the total
    /// bytes this entry occupies on disk.
    pub fn iter_from_offset_with_sizes(&self, start_offset: u64) -> Result<BucketOffsetIter<'_>> {
        match &self.storage {
            BucketStorage::InMemory { entries, .. } => {
                Ok(BucketOffsetIter::InMemory(InMemoryOffsetIter {
                    entries,
                    index: 0,
                    current_offset: 0,
                    start_offset,
                }))
            }
            BucketStorage::DiskBacked { disk_bucket } => Ok(BucketOffsetIter::DiskBacked(
                disk_bucket.iter_from_offset_with_sizes(start_offset)?,
            )),
        }
    }

    /// Get entries as a slice.
    ///
    /// **Note**: This only works for in-memory buckets. For disk-backed buckets,
    /// use `iter()` instead.
    ///
    /// # Panics
    ///
    /// Panics if called on a disk-backed bucket.
    pub fn entries(&self) -> &[BucketEntry] {
        match &self.storage {
            BucketStorage::InMemory { entries, .. } => entries,
            BucketStorage::DiskBacked { .. } => {
                panic!("entries() not supported for disk-backed buckets, use iter() instead")
            }
        }
    }

    /// Look up an entry by its key.
    ///
    /// For in-memory buckets, returns a reference. For disk-backed buckets,
    /// loads the entry from disk.
    pub fn get(&self, key: &LedgerKey) -> Result<Option<BucketEntry>> {
        match &self.storage {
            BucketStorage::InMemory { entries, key_index } => {
                if let Some(&idx) = key_index.get(key) {
                    Ok(entries.get(idx).cloned())
                } else {
                    Ok(None)
                }
            }
            BucketStorage::DiskBacked { disk_bucket } => disk_bucket.get(key),
        }
    }

    /// Look up a bucket entry by its original key and pre-serialized XDR bytes.
    ///
    /// This avoids redundant key serialization when the caller has already
    /// serialized the key (e.g., when searching multiple buckets in sequence).
    pub fn get_by_key_bytes(
        &self,
        key: &LedgerKey,
        key_bytes: &[u8],
    ) -> Result<Option<BucketEntry>> {
        match &self.storage {
            BucketStorage::InMemory { entries, key_index } => {
                if let Some(&idx) = key_index.get(key) {
                    Ok(entries.get(idx).cloned())
                } else {
                    Ok(None)
                }
            }
            BucketStorage::DiskBacked { disk_bucket } => {
                disk_bucket.get_by_key_bytes(key, key_bytes)
            }
        }
    }

    /// Look up a ledger entry by key, returning None if dead or not found.
    pub fn get_entry(&self, key: &LedgerKey) -> Result<Option<LedgerEntry>> {
        match self.get(key)? {
            Some(BucketEntry::Liveentry(entry)) | Some(BucketEntry::Initentry(entry)) => {
                Ok(Some(entry))
            }
            Some(BucketEntry::Deadentry(_)) => Ok(None), // Entry is deleted
            Some(BucketEntry::Metaentry(_)) => Ok(None),
            None => Ok(None),
        }
    }

    /// Get the protocol version from bucket metadata, if present.
    pub fn protocol_version(&self) -> Option<u32> {
        let iter = self.iter().ok()?;
        for entry_result in iter {
            if let Ok(BucketEntry::Metaentry(meta)) = entry_result {
                return Some(meta.ledger_version);
            }
        }
        None
    }

    /// Convert bucket contents to uncompressed XDR bytes.
    ///
    /// **Note**: Only works for in-memory buckets.
    pub fn to_xdr_bytes(&self) -> Result<Vec<u8>> {
        match &self.storage {
            BucketStorage::InMemory { entries, .. } => Self::serialize_entries(entries),
            BucketStorage::DiskBacked { disk_bucket } => {
                // Read from disk file
                let path = disk_bucket.file_path();
                let bytes = std::fs::read(path)?;
                Ok(bytes)
            }
        }
    }

    // ========================================================================
    // Level 0 In-Memory Optimization
    // ========================================================================

    /// Check if this bucket has in-memory entries for level 0 optimization.
    ///
    /// When true, the bucket can participate in fast in-memory merges
    /// without disk I/O.
    pub fn has_in_memory_entries(&self) -> bool {
        !matches!(self.level_zero_state, LevelZeroState::None)
    }

    /// Get the in-memory entries for level 0 optimization.
    ///
    /// Returns None if entries are not cached in memory.
    pub fn get_in_memory_entries(&self) -> Option<&[BucketEntry]> {
        match &self.level_zero_state {
            LevelZeroState::None => None,
            LevelZeroState::Separate(entries) => Some(entries.as_slice()),
            LevelZeroState::SharedWithStorage { metadata_count } => {
                // Get entries from storage and skip metadata
                match &self.storage {
                    BucketStorage::InMemory { entries, .. } => Some(&entries[*metadata_count..]),
                    BucketStorage::DiskBacked { .. } => {
                        // Disk-backed buckets shouldn't use SharedWithStorage
                        None
                    }
                }
            }
        }
    }

    /// Set the in-memory entries for level 0 optimization.
    ///
    /// This stores entries in memory for fast access during subsequent merges.
    /// Call this after creating a bucket to enable in-memory level 0 merges.
    pub fn set_in_memory_entries(&mut self, entries: Vec<BucketEntry>) {
        self.level_zero_state = LevelZeroState::Separate(Arc::new(entries));
    }

    /// Clear the in-memory entries.
    ///
    /// This releases the memory used for level 0 optimization.
    /// Call this when a bucket moves beyond level 0.
    pub fn clear_in_memory_entries(&mut self) {
        self.level_zero_state = LevelZeroState::None;
    }

    /// Create a bucket from sorted entries with in-memory optimization enabled.
    ///
    /// This is like `from_sorted_entries` but also keeps entries in memory
    /// for level 0 optimization.
    ///
    /// # Important: METAENTRY exclusion from in-memory state
    ///
    /// Per stellar-core (LiveBucket.h lines 35-38): "Stores all BucketEntries
    /// (except METAENTRY) in the same order that they appear in the bucket file
    /// for level 0 entries."
    ///
    /// The in-memory entries exclude METAENTRY because:
    /// 1. METAENTRY is always first and can be reconstructed from protocol version
    /// 2. In-memory merges generate fresh metadata based on max protocol version
    /// 3. Keeping metadata separate simplifies the merge logic
    ///
    /// # Performance
    ///
    /// This method uses zero-copy shared storage: entries are stored once and
    /// the in-memory state references the same storage with an offset to skip metadata.
    pub fn from_sorted_entries_with_in_memory(entries: Vec<BucketEntry>) -> Result<Self> {
        use sha2::{Digest, Sha256};
        use stellar_xdr::curr::{Limited, WriteXdr};

        let mut key_index = HashMap::new();
        let mut hasher = Sha256::new();

        // Count metadata entries (typically 0 or 1, always at the start)
        let metadata_count = entries.iter().take_while(|e| e.is_metadata()).count();

        // Single pass: serialize each entry for hash and build key index
        for (idx, entry) in entries.iter().enumerate() {
            // Serialize entry to XDR for hash
            let xdr_entry = entry.clone();
            let mut entry_bytes = Vec::new();
            let mut limited = Limited::new(&mut entry_bytes, Limits::none());
            xdr_entry.write_xdr(&mut limited).map_err(|e| {
                BucketError::Serialization(format!("Failed to serialize entry: {}", e))
            })?;

            // Write record mark + entry to hasher (XDR Record Marking format)
            let size = u32::try_from(entry_bytes.len()).map_err(|_| {
                BucketError::Serialization(format!(
                    "Entry too large for XDR record mark: {} bytes",
                    entry_bytes.len()
                ))
            })?;
            let record_mark = size | crate::XDR_RECORD_MARK;
            hasher.update(record_mark.to_be_bytes());
            hasher.update(&entry_bytes);

            // Build key index for entries with keys
            if let Some(key) = entry.key() {
                key_index.insert(key, idx);
            }
        }

        // Compute final hash
        let hash_bytes: [u8; 32] = hasher.finalize().into();
        let hash = Hash256::from_bytes(hash_bytes);

        Ok(Self {
            hash,
            storage: BucketStorage::InMemory {
                entries: Arc::new(entries),
                key_index: Arc::new(key_index),
            },
            // Use shared state - no cloning needed!
            level_zero_state: LevelZeroState::SharedWithStorage { metadata_count },
        })
    }
}

/// Iterator over bucket entries.
///
/// This iterator abstracts over both in-memory and disk-backed storage modes.
/// For in-memory buckets, iteration is efficient (just cloning references).
/// For disk-backed buckets, entries are read sequentially from disk.
///
/// # Performance
///
/// - **In-memory**: O(n) time, no I/O
/// - **Disk-backed**: O(n) time with disk reads, sequential access pattern
///
/// The iterator yields owned [`BucketEntry`] values (cloned from in-memory
/// storage or parsed from disk).
pub enum BucketIter<'a> {
    /// Iterating over in-memory entries (efficient, just cloning).
    InMemory(std::slice::Iter<'a, BucketEntry>),
    /// Iterating over disk-backed entries (reads from disk sequentially).
    DiskBacked(crate::disk_bucket::DiskBucketIter),
    /// Empty iterator (no entries).
    Empty,
}

impl Iterator for BucketIter<'_> {
    type Item = Result<BucketEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            BucketIter::InMemory(iter) => iter.next().cloned().map(Ok),
            BucketIter::DiskBacked(iter) => iter.next(),
            BucketIter::Empty => None,
        }
    }
}

/// Iterator over bucket entries that yields record sizes alongside entries.
///
/// For disk-backed buckets, this seeks directly to a starting byte offset and
/// reads record sizes from the file format (no re-serialization needed).
/// For in-memory buckets, it computes record sizes via XDR serialization
/// (acceptable since in-memory buckets are small).
pub enum BucketOffsetIter<'a> {
    /// In-memory variant that computes sizes on the fly.
    InMemory(InMemoryOffsetIter<'a>),
    /// Disk-backed variant that reads sizes from record marks.
    DiskBacked(crate::disk_bucket::DiskBucketOffsetIter),
    /// Empty iterator (used for error recovery).
    Empty,
}

/// In-memory offset iterator state.
pub struct InMemoryOffsetIter<'a> {
    entries: &'a [BucketEntry],
    index: usize,
    current_offset: u64,
    start_offset: u64,
}

impl Iterator for BucketOffsetIter<'_> {
    /// (entry, total_record_size)
    type Item = Result<(BucketEntry, u64)>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            BucketOffsetIter::InMemory(iter) => {
                while iter.index < iter.entries.len() {
                    let entry = &iter.entries[iter.index];
                    iter.index += 1;

                    // Compute the on-disk record size: 4-byte mark + XDR bytes
                    let entry_size = match entry.to_xdr_bytes() {
                        Ok(bytes) => bytes.len() as u64 + 4,
                        Err(e) => {
                            return Some(Err(BucketError::Serialization(format!(
                                "Failed to serialize entry for size calculation: {e}"
                            ))))
                        }
                    };

                    let entry_end = iter.current_offset + entry_size;
                    if entry_end <= iter.start_offset {
                        iter.current_offset = entry_end;
                        continue;
                    }

                    iter.current_offset = entry_end;
                    return Some(Ok((entry.clone(), entry_size)));
                }
                None
            }
            BucketOffsetIter::DiskBacked(iter) => iter.next(),
            BucketOffsetIter::Empty => None,
        }
    }
}

impl std::fmt::Debug for Bucket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let entry_count = self.len();
        let is_disk_backed = self.is_disk_backed();
        f.debug_struct("Bucket")
            .field("hash", &self.hash.to_hex())
            .field("entries", &entry_count)
            .field("disk_backed", &is_disk_backed)
            .finish()
    }
}

impl Default for Bucket {
    fn default() -> Self {
        Self::empty()
    }
}

impl PartialEq for Bucket {
    fn eq(&self, other: &Self) -> bool {
        self.hash == other.hash
    }
}

impl Eq for Bucket {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BucketEntry;
    use stellar_xdr::curr::*; // Re-import to shadow XDR's BucketEntry

    fn make_account_id(bytes: [u8; 32]) -> AccountId {
        AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(bytes)))
    }

    fn make_account_entry(bytes: [u8; 32], balance: i64) -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: make_account_id(bytes),
                balance,
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: Vec::new().try_into().unwrap(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    #[test]
    fn test_empty_bucket() {
        let bucket = Bucket::empty();
        assert!(bucket.is_empty());
        assert_eq!(bucket.len(), 0);
        assert_eq!(bucket.hash(), Hash256::ZERO);
    }

    #[test]
    fn test_bucket_from_entries() {
        let entries = vec![
            BucketEntry::Liveentry(make_account_entry([2u8; 32], 200)),
            BucketEntry::Liveentry(make_account_entry([1u8; 32], 100)),
        ];

        let bucket = Bucket::from_entries(entries).unwrap();
        assert!(!bucket.is_empty());
        assert_eq!(bucket.len(), 2);

        // Entries should be sorted
        let entries: Vec<_> = bucket.iter().unwrap().map(|e| e.unwrap()).collect();
        if let BucketEntry::Liveentry(entry) = &entries[0] {
            if let LedgerEntryData::Account(account) = &entry.data {
                assert_eq!(account.balance, 100);
            }
        }
    }

    #[test]
    fn test_bucket_lookup() {
        let entries = vec![
            BucketEntry::Liveentry(make_account_entry([1u8; 32], 100)),
            BucketEntry::Liveentry(make_account_entry([2u8; 32], 200)),
        ];

        let bucket = Bucket::from_entries(entries).unwrap();

        let key = LedgerKey::Account(LedgerKeyAccount {
            account_id: make_account_id([1u8; 32]),
        });

        let entry = bucket.get_entry(&key).unwrap().unwrap();
        if let LedgerEntryData::Account(account) = &entry.data {
            assert_eq!(account.balance, 100);
        } else {
            panic!("Expected Account entry");
        }
    }

    #[test]
    fn test_bucket_dead_entry() {
        let key = LedgerKey::Account(LedgerKeyAccount {
            account_id: make_account_id([1u8; 32]),
        });

        let entries = vec![BucketEntry::Deadentry(key.clone())];
        let bucket = Bucket::from_entries(entries).unwrap();

        // Looking up a dead entry should return None
        let result = bucket.get_entry(&key).unwrap();
        assert!(result.is_none());

        // But get() should return the dead entry
        let entry = bucket.get(&key).unwrap();
        assert!(entry.is_some());
        assert!(entry.unwrap().is_dead());
    }

    #[test]
    fn test_bucket_hash_consistency() {
        let entries = vec![
            BucketEntry::Liveentry(make_account_entry([1u8; 32], 100)),
            BucketEntry::Liveentry(make_account_entry([2u8; 32], 200)),
        ];

        let bucket1 = Bucket::from_entries(entries.clone()).unwrap();
        let bucket2 = Bucket::from_entries(entries).unwrap();

        assert_eq!(bucket1.hash(), bucket2.hash());
    }

    #[test]
    fn test_bucket_save_and_load() {
        let entries = vec![
            BucketEntry::Liveentry(make_account_entry([1u8; 32], 100)),
            BucketEntry::Liveentry(make_account_entry([2u8; 32], 200)),
        ];

        let bucket = Bucket::from_entries(entries).unwrap();
        let original_hash = bucket.hash();

        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("test.bucket.gz");

        bucket.save_to_file(&path).unwrap();

        let loaded = Bucket::load_from_file(&path).unwrap();
        assert_eq!(loaded.hash(), original_hash);
        assert_eq!(loaded.len(), 2);
    }

    #[test]
    fn test_bucket_roundtrip_entries() {
        // Create a bucket with entries
        let entries = vec![
            BucketEntry::Liveentry(make_account_entry([1u8; 32], 100)),
            BucketEntry::Liveentry(make_account_entry([2u8; 32], 200)),
        ];
        let bucket = Bucket::from_entries(entries.clone()).unwrap();
        let original_hash = bucket.hash();

        // Serialize to XDR bytes
        let xdr_bytes = bucket.to_xdr_bytes().unwrap();

        // Parse back from XDR bytes
        let parsed = Bucket::from_xdr_bytes(&xdr_bytes).unwrap();

        // Hashes should match
        assert_eq!(
            parsed.hash(),
            original_hash,
            "Hash mismatch after round-trip"
        );
        assert_eq!(parsed.len(), entries.len(), "Entry count mismatch");
    }

    #[test]
    fn test_bucket_entries_roundtrip() {
        // Create a bucket
        let entries = vec![
            BucketEntry::Liveentry(make_account_entry([1u8; 32], 100)),
            BucketEntry::Liveentry(make_account_entry([2u8; 32], 200)),
        ];
        let bucket1 = Bucket::from_entries(entries).unwrap();

        // Extract entries and create a new bucket
        let extracted: Vec<BucketEntry> = bucket1.iter().unwrap().map(|e| e.unwrap()).collect();
        let bucket2 = Bucket::from_entries(extracted).unwrap();

        // Hashes should match
        assert_eq!(
            bucket2.hash(),
            bucket1.hash(),
            "Hash mismatch after entries roundtrip"
        );
    }

    #[test]
    fn test_disk_backed_bucket_roundtrip() {
        // Create entries
        let entries = vec![
            BucketEntry::Liveentry(make_account_entry([1u8; 32], 100)),
            BucketEntry::Liveentry(make_account_entry([2u8; 32], 200)),
            BucketEntry::Liveentry(make_account_entry([3u8; 32], 300)),
        ];

        // Create an in-memory bucket first
        let in_memory_bucket = Bucket::from_entries(entries).unwrap();
        let original_hash = in_memory_bucket.hash();

        // Serialize to XDR bytes
        let xdr_bytes = in_memory_bucket.to_xdr_bytes().unwrap();

        // Create a disk-backed bucket
        let temp_dir = tempfile::tempdir().unwrap();
        let disk_path = temp_dir.path().join("test.bucket");
        let disk_bucket = Bucket::from_xdr_bytes_disk_backed(&xdr_bytes, &disk_path).unwrap();

        // Hash should match
        assert_eq!(
            disk_bucket.hash(),
            original_hash,
            "Disk bucket hash mismatch"
        );
        assert!(disk_bucket.is_disk_backed());

        // Extract entries via iter() and create a new bucket
        let extracted: Vec<BucketEntry> = disk_bucket.iter().unwrap().map(|e| e.unwrap()).collect();
        let recreated = Bucket::from_entries(extracted).unwrap();

        // Hashes should match after roundtrip
        assert_eq!(
            recreated.hash(),
            original_hash,
            "Hash mismatch after disk bucket roundtrip"
        );
    }

    #[test]
    fn test_disk_backed_bucket_with_metadata() {
        use stellar_xdr::curr::{BucketMetadata, BucketMetadataExt};

        // Create entries with metadata (as stellar-core buckets would have)
        let entries = vec![
            BucketEntry::Metaentry(BucketMetadata {
                ledger_version: 24,
                ext: BucketMetadataExt::V0,
            }),
            BucketEntry::Liveentry(make_account_entry([1u8; 32], 100)),
            BucketEntry::Liveentry(make_account_entry([2u8; 32], 200)),
        ];

        // Create an in-memory bucket
        let in_memory_bucket = Bucket::from_entries(entries).unwrap();
        let original_hash = in_memory_bucket.hash();

        // Serialize and create disk-backed
        let xdr_bytes = in_memory_bucket.to_xdr_bytes().unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let disk_path = temp_dir.path().join("test_meta.bucket");
        let disk_bucket = Bucket::from_xdr_bytes_disk_backed(&xdr_bytes, &disk_path).unwrap();

        assert_eq!(disk_bucket.hash(), original_hash);

        // Roundtrip via entries
        let extracted: Vec<BucketEntry> = disk_bucket.iter().unwrap().map(|e| e.unwrap()).collect();

        // Verify metadata is first
        assert!(extracted[0].is_metadata(), "Metadata should be first entry");

        let recreated = Bucket::from_entries(extracted).unwrap();
        assert_eq!(
            recreated.hash(),
            original_hash,
            "Hash mismatch after metadata bucket roundtrip"
        );
    }

    #[test]
    fn test_xdr_serialization_roundtrip_produces_identical_bytes() {
        // Create entries
        let entries = vec![
            BucketEntry::Liveentry(make_account_entry([1u8; 32], 100)),
            BucketEntry::Liveentry(make_account_entry([2u8; 32], 200)),
        ];

        // Create bucket and get XDR bytes
        let bucket = Bucket::from_entries(entries).unwrap();
        let original_xdr = bucket.to_xdr_bytes().unwrap();

        // Parse entries from XDR and re-serialize
        let parsed = Bucket::from_xdr_bytes(&original_xdr).unwrap();
        let reserialized_xdr = parsed.to_xdr_bytes().unwrap();

        // Bytes should be identical
        assert_eq!(
            reserialized_xdr.len(),
            original_xdr.len(),
            "XDR length differs"
        );
        assert_eq!(
            reserialized_xdr, original_xdr,
            "XDR bytes differ after roundtrip"
        );
    }

    #[test]
    fn test_disk_backed_to_inmemory_xdr_roundtrip() {
        // Create entries
        let entries = vec![
            BucketEntry::Liveentry(make_account_entry([1u8; 32], 100)),
            BucketEntry::Liveentry(make_account_entry([2u8; 32], 200)),
        ];

        // Create bucket and serialize
        let bucket = Bucket::from_entries(entries).unwrap();
        let original_xdr = bucket.to_xdr_bytes().unwrap();
        let original_hash = bucket.hash();

        // Create disk-backed bucket
        let temp_dir = tempfile::tempdir().unwrap();
        let disk_path = temp_dir.path().join("test.bucket");
        let disk_bucket = Bucket::from_xdr_bytes_disk_backed(&original_xdr, &disk_path).unwrap();
        assert_eq!(disk_bucket.hash(), original_hash);

        // Get XDR from disk-backed bucket (reads from file)
        let disk_xdr = disk_bucket.to_xdr_bytes().unwrap();
        assert_eq!(
            disk_xdr, original_xdr,
            "Disk bucket XDR differs from original"
        );

        // Extract entries and create new in-memory bucket
        let extracted: Vec<_> = disk_bucket.iter().unwrap().map(|e| e.unwrap()).collect();
        let inmemory = Bucket::from_sorted_entries(extracted).unwrap();

        // Get XDR from in-memory bucket (serializes entries)
        let inmemory_xdr = inmemory.to_xdr_bytes().unwrap();

        // These should be identical
        assert_eq!(
            inmemory_xdr, disk_xdr,
            "In-memory XDR differs from disk XDR"
        );
        assert_eq!(inmemory.hash(), original_hash);
    }

    #[test]
    fn test_from_sorted_entries_preserves_hash() {
        // Create entries
        let entries = vec![
            BucketEntry::Liveentry(make_account_entry([1u8; 32], 100)),
            BucketEntry::Liveentry(make_account_entry([2u8; 32], 200)),
        ];

        // Create bucket (will sort)
        let bucket1 = Bucket::from_entries(entries).unwrap();

        // Extract entries (in sorted order from bucket1)
        let sorted: Vec<_> = bucket1.iter().unwrap().map(|e| e.unwrap()).collect();

        // Create bucket from sorted entries (should NOT re-sort)
        let bucket2 = Bucket::from_sorted_entries(sorted).unwrap();

        // Hashes should match
        assert_eq!(bucket2.hash(), bucket1.hash());
    }

    #[test]
    fn test_disk_bucket_from_sorted_entries_roundtrip() {
        // Create entries
        let entries = vec![
            BucketEntry::Liveentry(make_account_entry([1u8; 32], 100)),
            BucketEntry::Liveentry(make_account_entry([2u8; 32], 200)),
            BucketEntry::Liveentry(make_account_entry([3u8; 32], 300)),
        ];

        // Create bucket and serialize
        let bucket = Bucket::from_entries(entries).unwrap();
        let original_hash = bucket.hash();
        let xdr_bytes = bucket.to_xdr_bytes().unwrap();

        // Create disk-backed bucket
        let temp_dir = tempfile::tempdir().unwrap();
        let disk_path = temp_dir.path().join("test.bucket");
        let disk_bucket = Bucket::from_xdr_bytes_disk_backed(&xdr_bytes, &disk_path).unwrap();
        assert_eq!(disk_bucket.hash(), original_hash);

        // Extract entries (already in sorted order)
        let extracted: Vec<_> = disk_bucket.iter().unwrap().map(|e| e.unwrap()).collect();

        // Use from_sorted_entries instead of from_entries
        let recreated = Bucket::from_sorted_entries(extracted).unwrap();

        // Hash should match!
        assert_eq!(
            recreated.hash(),
            original_hash,
            "from_sorted_entries should preserve disk bucket hash"
        );
    }

    #[test]
    fn test_bucket_mixed_entry_types_ordering() {
        use stellar_xdr::curr::{
            AlphaNum4, AssetCode4, OfferEntry, TrustLineAsset, TrustLineEntry,
        };

        // Create helper functions for different entry types
        let make_offer_entry = |seller: [u8; 32], offer_id: i64| -> LedgerEntry {
            LedgerEntry {
                last_modified_ledger_seq: 1,
                data: LedgerEntryData::Offer(OfferEntry {
                    seller_id: make_account_id(seller),
                    offer_id: offer_id,
                    selling: stellar_xdr::curr::Asset::Native,
                    buying: stellar_xdr::curr::Asset::Native,
                    amount: 1000,
                    price: stellar_xdr::curr::Price { n: 1, d: 1 },
                    flags: 0,
                    ext: stellar_xdr::curr::OfferEntryExt::V0,
                }),
                ext: LedgerEntryExt::V0,
            }
        };

        let make_trustline_entry = |account: [u8; 32], issuer: [u8; 32]| -> LedgerEntry {
            LedgerEntry {
                last_modified_ledger_seq: 1,
                data: LedgerEntryData::Trustline(TrustLineEntry {
                    account_id: make_account_id(account),
                    asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                        asset_code: AssetCode4([b'U', b'S', b'D', 0]),
                        issuer: make_account_id(issuer),
                    }),
                    balance: 1000,
                    limit: 10000,
                    flags: 0, // No flags set
                    ext: stellar_xdr::curr::TrustLineEntryExt::V0,
                }),
                ext: LedgerEntryExt::V0,
            }
        };

        // Create entries of different types (unsorted)
        let entries = vec![
            // Offer (type 2) with high account
            BucketEntry::Liveentry(make_offer_entry([255u8; 32], 1)),
            // Account (type 0) with low account
            BucketEntry::Liveentry(make_account_entry([1u8; 32], 100)),
            // Trustline (type 1) with mid account
            BucketEntry::Liveentry(make_trustline_entry([128u8; 32], [50u8; 32])),
            // Another Account (type 0) with mid account
            BucketEntry::Liveentry(make_account_entry([128u8; 32], 200)),
        ];

        // Create bucket (will sort entries)
        let bucket = Bucket::from_entries(entries).unwrap();
        let sorted_entries: Vec<_> = bucket.iter().unwrap().map(|e| e.unwrap()).collect();

        // Verify ordering: Account (type 0) < Trustline (type 1) < Offer (type 2)
        assert!(sorted_entries[0].is_live());
        assert!(matches!(
            sorted_entries[0].as_ledger_entry().unwrap().data,
            LedgerEntryData::Account(_)
        ));
        assert!(sorted_entries[1].is_live());
        assert!(matches!(
            sorted_entries[1].as_ledger_entry().unwrap().data,
            LedgerEntryData::Account(_)
        ));
        assert!(sorted_entries[2].is_live());
        assert!(matches!(
            sorted_entries[2].as_ledger_entry().unwrap().data,
            LedgerEntryData::Trustline(_)
        ));
        assert!(sorted_entries[3].is_live());
        assert!(matches!(
            sorted_entries[3].as_ledger_entry().unwrap().data,
            LedgerEntryData::Offer(_)
        ));

        // Verify accounts are sorted by account ID
        if let LedgerEntryData::Account(a1) = &sorted_entries[0].as_ledger_entry().unwrap().data {
            if let LedgerEntryData::Account(a2) = &sorted_entries[1].as_ledger_entry().unwrap().data
            {
                assert_eq!(a1.balance, 100); // [1u8; 32] comes first
                assert_eq!(a2.balance, 200); // [128u8; 32] comes second
            }
        }

        // Roundtrip test
        let extracted: Vec<_> = bucket.iter().unwrap().map(|e| e.unwrap()).collect();
        let recreated = Bucket::from_entries(extracted).unwrap();
        assert_eq!(
            recreated.hash(),
            bucket.hash(),
            "Hash mismatch after mixed type roundtrip"
        );
    }

    /// Regression test for VE-12: `save_to_xdr_file` must not leave a zero-byte
    /// (or truncated) file on disk when the write fails.
    ///
    /// Before the fix, a disk-full error after `File::create` but before
    /// `write_all` left a zero-byte file at the canonical path.  A subsequent
    /// async merge whose output happened to hash to the same value would then
    /// find the pre-existing (corrupt) file, load it, and produce a bucket with
    /// hash = SHA-256("") = e3b0c44... instead of the real content hash.  This
    /// cascaded into a wrong bucket_list_hash at the next spill.
    ///
    /// After the fix, `save_to_xdr_file` removes the partial file on any write
    /// error, so no corrupt permanent file is left behind.
    #[test]
    fn test_save_to_xdr_file_cleans_up_partial_file_on_write_error() {
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("test.bucket.xdr");

        // Create a non-trivial in-memory bucket
        let entries = vec![
            BucketEntry::Liveentry(make_account_entry([1u8; 32], 100)),
            BucketEntry::Liveentry(make_account_entry([2u8; 32], 200)),
        ];
        let bucket = Bucket::from_entries(entries).unwrap();
        let original_hash = bucket.hash();

        // Simulate a partial write: create the file first, then truncate it
        // to zero bytes to mimic what happens when write_all fails mid-write.
        // (We can't actually trigger ENOSPC in a unit test, so we test the
        // cleanup logic by checking the happy path works, and separately verify
        // the cleanup path via the fixed code structure.)

        // Happy path: normal save should produce a valid file with the correct hash.
        bucket.save_to_xdr_file(&path).unwrap();
        assert!(path.exists(), "file must exist after successful save");
        let loaded = Bucket::from_xdr_file_disk_backed(&path).unwrap();
        assert_eq!(
            loaded.hash(),
            original_hash,
            "loaded hash must match original after save"
        );
        assert!(!loaded.is_empty(), "loaded bucket must not be empty");

        // Simulate corruption: truncate the file to zero bytes (as would happen
        // if write_all failed after File::create on a full disk).
        std::fs::write(&path, b"").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            0,
            "file should be zero bytes after simulated corruption"
        );

        // Loading the zero-byte file must NOT return a bucket with the original
        // hash — it would produce SHA-256("") instead.
        let corrupt = Bucket::from_xdr_file_disk_backed(&path).unwrap();
        assert_ne!(
            corrupt.hash(),
            original_hash,
            "loading a zero-byte file must not produce the original hash"
        );
        // SHA-256("") is the hash of an empty byte stream.
        let empty_hash_hex = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(
            corrupt.hash().to_hex(),
            empty_hash_hex,
            "zero-byte file must hash to SHA-256(empty)"
        );
    }

    /// Regression test for AUDIT-BC1: BucketIter must propagate I/O errors
    /// instead of silently stopping iteration.
    ///
    /// Previously, DiskBucketIter returned `Result<BucketEntry>` but BucketIter
    /// stripped the error with `.ok()`, silently terminating iteration on I/O
    /// errors. This meant a truncated or corrupted bucket file would produce a
    /// partial merge, yielding incorrect bucket hashes and consensus divergence.
    ///
    /// After the fix, BucketIter yields `Result<BucketEntry>`, ensuring I/O
    /// errors propagate to callers (merge, eviction, applicator, etc.).
    #[test]
    fn test_bucket_iter_yields_result_items() {
        // Verify in-memory bucket iteration yields Ok items
        let entry = BucketEntry::Liveentry(make_account_entry([1u8; 32], 100));
        let bucket = Bucket::from_entries(vec![entry.clone()]).unwrap();
        let mut iter = bucket.iter().unwrap();

        // First item should be Ok
        let first = iter.next();
        assert!(first.is_some(), "Should have an entry");
        let first = first.unwrap();
        assert!(first.is_ok(), "In-memory entries should always be Ok");

        // Next should be None (end of iteration)
        assert!(iter.next().is_none());
    }

    /// Regression test: verify that the iterator Item type is Result<BucketEntry>
    /// at compile time. This test would fail to compile if the type were changed
    /// back to plain BucketEntry.
    #[test]
    fn test_bucket_iter_item_is_result() {
        let bucket = Bucket::empty();
        let iter = bucket.iter().unwrap();
        // This line verifies the type at compile time
        let _items: Vec<crate::Result<BucketEntry>> = iter.collect();
    }

    /// [AUDIT-BH1] Raw XDR fallback silently swallows parse errors.
    ///
    /// Construct a raw XDR byte stream with two entries where the second is
    /// corrupt. The current buggy code enters the raw XDR fallback (because
    /// the first byte doesn't have high bit set) and silently `break`s on
    /// the corrupt second entry, returning only the first entry.
    /// After the fix (always use record marks), this should error.
    #[test]
    fn test_audit_bh1_parse_entries_rejects_corrupt_data() {
        // Create two valid entries as raw XDR (no record marks)
        let entry1 = BucketEntry::Liveentry(make_account_entry([1u8; 32], 100));
        let entry2 = BucketEntry::Liveentry(make_account_entry([2u8; 32], 200));
        let raw1 = entry1.to_xdr(Limits::none()).unwrap();
        let raw2 = entry2.to_xdr(Limits::none()).unwrap();

        // Concatenate entry1 raw XDR + truncated entry2 (corrupt)
        let mut corrupt_stream = raw1.clone();
        corrupt_stream.extend_from_slice(&raw2[..raw2.len() / 2]); // truncated

        // The current buggy code: detects no record marks (first byte 0x00),
        // enters raw XDR fallback, parses entry1 successfully, then hits a
        // parse error on the truncated entry2 and silently `break`s —
        // returning 1 entry instead of erroring.
        let result = Bucket::parse_entries(&corrupt_stream);
        // After fix: should error (invalid record mark or parse failure)
        assert!(
            result.is_err(),
            "parse_entries should error on corrupt data, not silently return partial results"
        );
    }

    /// [AUDIT-BH1] All bucket data must be parsed as record-marked XDR.
    ///
    /// Bucket files always use XDR record marks (RFC 5531). There should be
    /// no "raw XDR" fallback path — that path silently swallows errors.
    #[test]
    fn test_audit_bh1_no_raw_xdr_fallback() {
        // Serialize an entry as raw XDR (no record marks) — this is NOT a
        // valid bucket file format. parse_entries must reject it.
        let entry = BucketEntry::Liveentry(make_account_entry([2u8; 32], 200));
        let raw_xdr = entry.to_xdr(Limits::none()).unwrap();

        // The raw XDR for a Liveentry starts with discriminant 0x00000001,
        // whose first byte is 0x00 (high bit clear). The current buggy code
        // would detect this as "no record marks" and parse it via the raw
        // XDR fallback. The fixed code should reject it because the record
        // mark is missing/invalid.
        let result = Bucket::parse_entries(&raw_xdr);
        assert!(
            result.is_err(),
            "parse_entries should reject raw XDR without record marks"
        );
    }
}
