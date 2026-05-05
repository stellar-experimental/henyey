//! Disk-backed bucket implementation for memory-efficient storage.
//!
//! This module provides a bucket implementation that stores entries on disk
//! and uses a compact index for efficient lookups. This is essential for
//! processing mainnet buckets during catchup, where buckets can contain
//! millions of entries that would require many GB of RAM if loaded entirely.
//!
//! # How It Works
//!
//! Instead of loading all entries into memory, the disk bucket:
//!
//! 1. **Stores** the raw XDR bucket file on disk (uncompressed)
//! 2. **Builds** a `LiveBucketIndex` by streaming entries from the file
//! 3. **Reads** entries on-demand from disk when accessed via mmap
//!
//! # Index Types
//!
//! The index is automatically selected based on bucket size:
//!
//! - **Small buckets** (< 10K entries): `InMemoryIndex` with per-key offsets
//! - **Large buckets** (>= 10K entries): `DiskIndex` with page-based ranges
//!   and bloom filter (~148 MB for 60M keys vs ~960 MB with a flat index)
//!
//! # Trade-offs
//!
//! - **Slower lookups**: Each lookup requires disk I/O (mitigated by mmap)
//! - **No in-memory slice access**: Must use iteration instead
//!
//! # Usage
//!
//! Disk buckets are created via [`Bucket::from_xdr_bytes_disk_backed`] and are
//! transparent to most bucket operations. Check [`Bucket::is_disk_backed`] to
//! determine the storage mode.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use memmap2::Mmap;

use sha2::{Digest, Sha256};
use stellar_xdr::curr::{LedgerEntryType, LedgerKey, Limits, ReadXdr};

use henyey_common::{BucketListDbConfig, Hash256};

use crate::bloom_filter::HashSeed;
use crate::cache::RandomEvictionCache;
use crate::entry::{BucketEntry, BucketEntryExt};
use crate::index::LiveBucketIndex;
use crate::{BucketError, Result};

/// Minimum number of entries required to build a bloom filter.
/// Smaller buckets don't benefit enough from bloom filter lookups to justify the overhead.
const BLOOM_FILTER_MIN_ENTRIES: usize = 2;

/// Default hash seed for bloom filter construction.
/// This is used when no custom seed is provided.
pub const DEFAULT_BLOOM_SEED: HashSeed = [0u8; 16];

/// A disk-backed bucket that stores entries on disk with an in-memory index.
///
/// This implementation is designed for memory efficiency when processing
/// large buckets during catchup. Instead of loading all entries into memory,
/// it maintains a compact index and reads entries on-demand.
///
/// # Index Type
///
/// Uses `LiveBucketIndex` which automatically selects the appropriate strategy:
///
/// - **Small buckets** (< 10K entries): `InMemoryIndex` with per-key offsets
/// - **Large buckets** (≥ 10K entries): `DiskIndex` with page-based ranges
///   and bloom filter, reducing memory from ~960 MB to ~148 MB for 60M keys
///
/// # Bloom Filter
///
/// The index includes bloom filters for fast negative lookups,
/// allowing `get()` to quickly reject keys that are definitely
/// not present (avoiding disk I/O).
///
/// # File Access Pattern
///
/// Lookups use memory-mapped I/O for lock-free, zero-syscall reads.
/// This is critical for performance on mainnet where thousands of
/// lookups per ledger are needed.
///
/// # Index Construction
///
/// The index is built eagerly via streaming I/O when loading a bucket
/// from disk. The `OnceLock` wrapper provides thread-safe initialization
/// and supports the `from_prebuilt` path where a persisted index is
/// loaded directly.
pub struct DiskBucket {
    /// The SHA-256 hash of this bucket's contents (for verification).
    hash: Hash256,
    /// Path to the bucket file on disk (uncompressed XDR).
    file_path: PathBuf,
    /// Total number of entries in this bucket.
    entry_count: usize,
    /// Bloom filter seed used for index construction.
    bloom_seed: HashSeed,
    /// Index for key lookups (eagerly built or verified by construction).
    index: Box<LiveBucketIndex>,
    /// Memory-mapped file for lock-free reads.
    mmap: OnceLock<Arc<Mmap>>,
    /// Per-bucket entry cache for DiskIndex buckets.
    /// Initialized lazily via `maybe_initialize_cache()`.
    /// Only caches ACCOUNT entries (matching stellar-core).
    cache: OnceLock<Arc<RandomEvictionCache>>,
}

impl Clone for DiskBucket {
    fn clone(&self) -> Self {
        Self {
            hash: self.hash,
            file_path: self.file_path.clone(),
            entry_count: self.entry_count,
            bloom_seed: self.bloom_seed,
            index: self.index.clone(),
            mmap: self.mmap.clone(),
            cache: self.cache.clone(),
        }
    }
}

/// Iterator that reads `(BucketEntry, offset)` pairs one at a time from an
/// uncompressed XDR bucket file. Each call to `next()` reads and parses a
/// single record, keeping only O(1) memory (one entry + reusable buffer).
///
/// Optionally computes SHA-256 hash incrementally as records are read,
/// merging what was previously a separate `count_and_hash` pass into the
/// index-building pass (single-pass over the file).
struct StreamingXdrEntryIterator {
    records: crate::record::RecordMarkedReader<BufReader<File>>,
    /// Optional hasher for computing bucket hash during iteration.
    hasher: Option<Sha256>,
    /// Validates that entries are strictly sorted with no duplicates.
    validator: crate::entry::StreamingSortedValidator,
    /// Number of records read (including those that failed to parse).
    record_count: usize,
    failed: bool,
}

impl StreamingXdrEntryIterator {
    fn with_hasher(path: &Path, file_len: u64, compute_hash: bool) -> Result<Self> {
        let file = File::open(path)?;
        Ok(Self {
            records: crate::record::RecordMarkedReader::new(BufReader::new(file), file_len),
            hasher: if compute_hash {
                Some(Sha256::new())
            } else {
                None
            },
            validator: crate::entry::StreamingSortedValidator::new(),
            record_count: 0,
            failed: false,
        })
    }

    fn position(&self) -> u64 {
        self.records.position()
    }

    /// Finalize the iterator, returning (record_count, hash).
    /// Hash is only available if the iterator was created with `compute_hash=true`.
    fn finalize(self) -> (usize, Option<Hash256>) {
        let hash = self.hasher.map(|h| {
            let hash_bytes: [u8; 32] = h.finalize().into();
            Hash256::from_bytes(hash_bytes)
        });
        (self.record_count, hash)
    }
}

impl Iterator for StreamingXdrEntryIterator {
    type Item = Result<(BucketEntry, u64)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }

        loop {
            let record = match self.records.next_record() {
                Ok(Some(record)) => record,
                Ok(None) => return None,
                Err(e) => {
                    self.failed = true;
                    return Some(Err(e));
                }
            };

            // Feed raw bytes to hasher before parsing
            if let Some(ref mut hasher) = self.hasher {
                hasher.update(record.mark_bytes);
                hasher.update(record.body);
            }
            self.record_count += 1;

            let xdr_entry =
                match stellar_xdr::curr::BucketEntry::from_xdr(record.body, Limits::none()) {
                    Ok(entry) => entry,
                    Err(e) => {
                        self.failed = true;
                        return Some(Err(BucketError::Serialization(format!(
                            "Failed to parse bucket entry at offset {}: {}",
                            record.offset, e
                        ))));
                    }
                };

            // Validate sorted-unique invariant before filtering metadata
            if let Err(e) = self.validator.validate(&xdr_entry) {
                self.failed = true;
                return Some(Err(e));
            }

            if xdr_entry.key().is_some() {
                return Some(Ok((xdr_entry, record.offset)));
            }
        }
    }
}

impl DiskBucket {
    /// Create a memory-mapped file for lock-free reads.
    /// Uses MADV_RANDOM to optimize for point lookups (no readahead waste).
    fn create_mmap(path: &Path) -> Result<Arc<Mmap>> {
        let file = File::open(path)?;
        // SAFETY: The file is opened read-only, and the mmap is used for read-only access.
        // The bucket file is not modified while the mmap is active.
        let mmap = unsafe { Mmap::map(&file)? };
        #[cfg(unix)]
        {
            mmap.advise(memmap2::Advice::Random)
                .unwrap_or_else(|e| tracing::warn!("madvise(RANDOM) failed: {}", e));
        }
        Ok(Arc::new(mmap))
    }

    /// Compute SHA-256 hash of a file's content using sequential buffered I/O.
    ///
    /// Uses a separate file read (not mmap) to get optimal sequential access
    /// patterns for the kernel's readahead logic.
    fn compute_file_hash(path: &Path) -> Result<Hash256> {
        let file = File::open(path)?;
        let mut reader = BufReader::with_capacity(256 * 1024, file);
        let mut hasher = Sha256::new();
        let mut buf = [0u8; 256 * 1024];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(Hash256::from(<[u8; 32]>::from(hasher.finalize())))
    }

    /// Ensure the index is initialized.
    ///
    /// The index is always built eagerly during construction, so this simply
    /// returns the existing index. The `get_or_init` closure is a safety net
    /// that should never execute in practice.
    fn ensure_index(&self) -> &LiveBucketIndex {
        &self.index
    }

    /// Ensure the mmap is initialized, creating it if needed.
    fn ensure_mmap(&self) -> &Arc<Mmap> {
        self.mmap
            .get_or_init(|| Self::create_mmap(&self.file_path).expect("failed to mmap bucket file"))
    }

    /// Create a disk bucket from an uncompressed XDR file using streaming I/O.
    ///
    /// This builds the index by reading entries one at a time from the file,
    /// computing the hash incrementally. Memory usage is O(index_size), not
    /// O(file_size), making it suitable for very large bucket files (multi-GB).
    ///
    /// The file must use XDR record marking format (high bit set in record marks).
    pub fn from_file_streaming(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_file_streaming_with_seed(path, DEFAULT_BLOOM_SEED)
    }

    /// Create a disk bucket from an uncompressed XDR file using streaming I/O
    /// with a custom bloom filter seed.
    ///
    /// Uses the advanced `LiveBucketIndex`:
    /// - Small buckets (< 10K entries): `InMemoryIndex` (per-key O(log n) lookup)
    /// - Large buckets (≥ 10K entries): `DiskIndex` (page-based, ~10 MB for 60M keys)
    pub fn from_file_streaming_with_seed(
        path: impl AsRef<Path>,
        bloom_seed: HashSeed,
    ) -> Result<Self> {
        let path = path.as_ref();
        let file_len = std::fs::metadata(path)?.len();

        // Single pass: build index and compute hash simultaneously.
        // The iterator feeds raw record bytes to a SHA-256 hasher while also
        // parsing entries for the index, eliminating the previous two-pass approach.
        let iter = StreamingXdrEntryIterator::with_hasher(path, file_len, true)?;
        let (live_index, iter) =
            LiveBucketIndex::try_from_entries_default_with_iter(iter, bloom_seed, file_len)?;
        if iter.position() != file_len {
            return Err(BucketError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!(
                    "bucket stream ended at byte {} before file length {}",
                    iter.position(),
                    file_len
                ),
            )));
        }
        let (entry_count, hash) = iter.finalize();
        let hash = hash.expect("hasher was enabled");

        tracing::debug!(
            entry_count,
            file_size = file_len,
            index_type = if live_index.is_in_memory() {
                "InMemory"
            } else {
                "DiskIndex"
            },
            "Built disk bucket index via single-pass streaming"
        );

        let mmap = OnceLock::new();
        mmap.set(Self::create_mmap(path)?)
            .expect("just initialized");

        Ok(Self {
            hash,
            file_path: path.to_path_buf(),
            entry_count,
            bloom_seed,
            index: Box::new(live_index),
            mmap,
            cache: OnceLock::new(),
        })
    }

    /// Create a disk bucket from a pre-built index, skipping file scanning.
    ///
    /// This is used when loading a persisted index from disk, avoiding the
    /// expensive 2-pass streaming build. The caller is responsible for ensuring
    /// that the index matches the bucket file contents.
    ///
    /// # Trust model
    ///
    /// This path trusts the persisted index's semantic correctness (key ordering,
    /// offset accuracy). It verifies the bucket file's content hash against the
    /// expected hash, confirming the file hasn't changed since it was first loaded
    /// by `from_file_streaming_with_seed` (which validates sort order). If the
    /// persisted index was built from a valid file and the file hash still matches,
    /// the bucket data is consistent. The persisted index file itself is
    /// integrity-checked during deserialization (see `index_persistence.rs`).
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the uncompressed XDR bucket file
    /// * `hash` - The known SHA-256 hash of the bucket contents
    /// * `entry_count` - Total number of entries in the bucket
    /// * `index` - Pre-built LiveBucketIndex (from persistence or prior build)
    pub fn from_prebuilt(
        path: impl AsRef<Path>,
        hash: Hash256,
        entry_count: usize,
        prebuilt_index: LiveBucketIndex,
    ) -> Result<Self> {
        let path = path.as_ref();

        // Verify the bucket file's content hash matches the expected hash.
        // The prebuilt path trusts the persisted index for the hash value,
        // so we must independently verify by hashing the file content.
        // Use a sequential file read (not the mmap which uses MADV_RANDOM).
        let actual_hash = Self::compute_file_hash(path)?;
        if actual_hash != hash {
            return Err(BucketError::HashMismatch {
                expected: hash.to_hex(),
                actual: actual_hash.to_hex(),
            });
        }

        let mmap = OnceLock::new();
        mmap.set(Self::create_mmap(path)?)
            .expect("just initialized");

        Ok(Self {
            hash,
            file_path: path.to_path_buf(),
            entry_count,
            bloom_seed: DEFAULT_BLOOM_SEED,
            index: Box::new(prebuilt_index),
            mmap,
            cache: OnceLock::new(),
        })
    }

    /// Create a disk bucket from raw XDR bytes, saving to the specified path.
    pub fn from_xdr_bytes(bytes: &[u8], save_path: impl AsRef<Path>) -> Result<Self> {
        Self::from_xdr_bytes_with_seed(bytes, save_path, DEFAULT_BLOOM_SEED)
    }

    /// Create a disk bucket from raw XDR bytes with a custom bloom filter seed.
    ///
    /// Writes the bytes to disk, then builds the index by streaming.
    pub fn from_xdr_bytes_with_seed(
        bytes: &[u8],
        save_path: impl AsRef<Path>,
        bloom_seed: HashSeed,
    ) -> Result<Self> {
        let save_path = save_path.as_ref();

        // Save to disk atomically first
        henyey_common::fs_utils::atomic_write_bytes(save_path, bytes)?;

        // Build index by streaming the saved file. If validation fails, remove
        // the just-written file so malformed caller-provided bytes are not left
        // behind as a bucket artifact.
        match Self::from_file_streaming_with_seed(save_path, bloom_seed) {
            Ok(bucket) => Ok(bucket),
            Err(e) => {
                let _ = std::fs::remove_file(save_path);
                Err(e)
            }
        }
    }

    /// Get the hash of this bucket.
    pub fn hash(&self) -> Hash256 {
        self.hash
    }

    /// Check if this bucket is empty.
    pub fn is_empty(&self) -> bool {
        self.entry_count == 0 || self.hash.is_zero()
    }

    /// Get the number of entries in this bucket.
    pub fn len(&self) -> usize {
        self.entry_count
    }

    /// Get the path to the bucket file.
    pub fn file_path(&self) -> &Path {
        &self.file_path
    }

    /// Returns a reference to the live bucket index.
    /// Lazily initializes the index if it hasn't been built yet.
    pub fn live_index(&self) -> &LiveBucketIndex {
        self.ensure_index()
    }

    /// Returns true if this bucket has a bloom filter for fast negative lookups.
    pub fn has_bloom_filter(&self) -> bool {
        self.entry_count >= BLOOM_FILTER_MIN_ENTRIES
    }

    /// Returns the size of the bloom filter in bytes, or 0 if no filter exists.
    pub fn bloom_filter_size_bytes(&self) -> usize {
        self.ensure_index().bloom_filter_size_bytes()
    }

    /// Returns the hash seed used for the bloom filter.
    pub fn bloom_seed(&self) -> HashSeed {
        self.bloom_seed
    }

    /// Returns the per-bucket cache, if initialized.
    pub fn cache(&self) -> Option<&Arc<RandomEvictionCache>> {
        self.cache.get()
    }

    /// Returns the estimated heap bytes used by this bucket's index.
    pub fn index_heap_bytes(&self) -> usize {
        self.ensure_index().estimated_heap_bytes()
    }

    /// Returns the size of the mmap'd region in bytes, or 0 if not yet mapped.
    pub fn mmap_bytes(&self) -> usize {
        self.mmap.get().map(|m| m.len()).unwrap_or(0)
    }

    /// Initializes the per-bucket entry cache with proportional sizing.
    ///
    /// Matches stellar-core's `LiveBucketIndex::maybeInitializeCache`:
    /// - Only DiskIndex buckets get caches (InMemory already has everything in memory)
    /// - Cache size is proportional to this bucket's share of total account bytes
    /// - When total account bytes fit in the budget, the full bucket is cached
    pub fn maybe_initialize_cache(
        &self,
        total_bucket_list_account_size_bytes: u64,
        config: &BucketListDbConfig,
    ) {
        if self.cache.get().is_some() {
            return;
        }

        let index = self.ensure_index();

        // Only DiskIndex buckets get caches (InMemory already has everything)
        let LiveBucketIndex::Disk(disk_idx) = index else {
            return;
        };

        let counters = disk_idx.counters();
        let accounts_in_bucket = counters.count_for_type(LedgerEntryType::Account);
        let max_cache_bytes = config.memory_for_caching_mb as u64 * 1024 * 1024;

        if accounts_in_bucket == 0 || max_cache_bytes == 0 {
            return;
        }

        let account_bytes = counters.size_for_type(LedgerEntryType::Account);
        let cache_entries = if total_bucket_list_account_size_bytes <= max_cache_bytes {
            // Can cache the entire bucket
            accounts_in_bucket as usize
        } else {
            // Proportional allocation (stellar-core formula)
            let fraction = account_bytes as f64 / total_bucket_list_account_size_bytes as f64;
            let bytes_for_bucket = max_cache_bytes as f64 * fraction;
            let avg_size = account_bytes as f64 / accounts_in_bucket as f64;
            (bytes_for_bucket / avg_size) as usize
        };

        if cache_entries == 0 {
            return;
        }

        let cache = RandomEvictionCache::with_limits(usize::MAX, cache_entries);
        cache.activate();
        let _ = self.cache.set(Arc::new(cache));
    }

    /// Look up an entry by key.
    ///
    /// This reads from disk using the index. The bloom filter is checked first
    /// to quickly reject keys that are definitely not present (avoiding disk I/O).
    ///
    /// On first call, this lazily initializes the index (Pass 2) and mmap.
    pub fn get(&self, key: &LedgerKey) -> Result<Option<BucketEntry>> {
        // Check per-bucket cache first
        if let Some(cache) = self.cache.get() {
            if let Some(cached) = cache.get(key) {
                return Ok(Some((*cached).clone()));
            }
        }

        let index = self.ensure_index();

        // Check bloom filter (built into the index)
        if !index.may_contain(key) {
            return Ok(None);
        }

        let result = match index {
            LiveBucketIndex::InMemory(idx) => {
                // Exact offset lookup
                if let Some(offset) = idx.get_offset(key) {
                    let entry = self.read_entry_at(offset)?;
                    if let Some(entry_key) = entry.key() {
                        if &entry_key == key {
                            Some(entry)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            LiveBucketIndex::Disk(disk_idx) => {
                // Page-based lookup: find candidate page, scan within it
                if let Some(page_offset) = disk_idx.find_page_for_key(key) {
                    self.scan_page_for_key(page_offset, key, disk_idx.page_size())?
                } else {
                    None
                }
            }
        };

        // Populate cache on miss
        if let Some(ref entry) = result {
            if let Some(cache) = self.cache.get() {
                cache.insert(key.clone(), entry.clone());
            }
        }

        Ok(result)
    }

    /// Look up an entry using pre-serialized key bytes to avoid redundant serialization.
    ///
    /// The `key` is needed for final verification (hash collisions), while `key_bytes`
    /// is used for bloom filter checks and index lookups.
    pub fn get_by_key_bytes(
        &self,
        key: &LedgerKey,
        key_bytes: &[u8],
    ) -> Result<Option<BucketEntry>> {
        // Check per-bucket cache first
        if let Some(cache) = self.cache.get() {
            if let Some(cached) = cache.get(key) {
                return Ok(Some((*cached).clone()));
            }
        }

        let index = self.ensure_index();

        if !index.may_contain_bytes(key_bytes) {
            return Ok(None);
        }

        let result = match index {
            LiveBucketIndex::InMemory(idx) => {
                if let Some(offset) = idx.get_offset_by_key_bytes(key_bytes) {
                    let entry = self.read_entry_at(offset)?;
                    if let Some(entry_key) = entry.key() {
                        if &entry_key == key {
                            Some(entry)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            LiveBucketIndex::Disk(disk_idx) => {
                // Disk-based index uses key comparison for page search,
                // fall back to regular method
                if let Some(page_offset) = disk_idx.find_page_for_key(key) {
                    self.scan_page_for_key(page_offset, key, disk_idx.page_size())?
                } else {
                    None
                }
            }
        };

        // Populate cache on miss
        if let Some(ref entry) = result {
            if let Some(cache) = self.cache.get() {
                cache.insert(key.clone(), entry.clone());
            }
        }

        Ok(result)
    }

    /// Read a single entry from the mmap at the given offset.
    /// No syscalls, no locks — direct memory access through the mmap.
    fn read_entry_at(&self, offset: u64) -> Result<BucketEntry> {
        let offset = offset as usize;
        let data = &**self.ensure_mmap();
        let mut records = crate::record::RecordMarkedSliceIter::at(data, offset);
        let record = records.next_record()?.ok_or_else(|| {
            BucketError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("no XDR record at offset {}", offset),
            ))
        })?;
        let xdr_entry = stellar_xdr::curr::BucketEntry::from_xdr(record.body, Limits::none())
            .map_err(|e| {
                BucketError::Serialization(format!(
                    "Failed to parse entry at offset {}: {}",
                    record.offset, e
                ))
            })?;

        Ok(xdr_entry)
    }

    /// Scan a page starting at `page_offset` for a key, reading up to `page_size` bytes.
    ///
    /// The scan terminates when the position exceeds `page_offset + page_size` or
    /// when an entry with a key greater than the target is found (entries are sorted).
    /// Entries straddling the page boundary are still read completely (mmap allows this).
    fn scan_page_for_key(
        &self,
        page_offset: u64,
        key: &LedgerKey,
        page_size: u64,
    ) -> Result<Option<BucketEntry>> {
        let data = &**self.ensure_mmap();
        let start = usize::try_from(page_offset).map_err(|_| {
            BucketError::Serialization(format!("page offset {} does not fit usize", page_offset))
        })?;
        let page_size = usize::try_from(page_size).map_err(|_| {
            BucketError::Serialization(format!("page size {} does not fit usize", page_size))
        })?;
        let page_end = start.checked_add(page_size).ok_or_else(|| {
            BucketError::Serialization(format!(
                "page offset {} plus size {} overflows",
                start, page_size
            ))
        })?;
        let mut records = crate::record::RecordMarkedSliceIter::at(data, start);

        while records.position() < page_end {
            let Some(record) = records.next_record()? else {
                break;
            };
            let entry = stellar_xdr::curr::BucketEntry::from_xdr(record.body, Limits::none())
                .map_err(|e| {
                    BucketError::Serialization(format!(
                        "Failed to parse entry at offset {} during page scan: {}",
                        record.offset, e
                    ))
                })?;

            if let Some(entry_key) = entry.key() {
                if &entry_key == key {
                    return Ok(Some(entry));
                }
                if crate::entry::compare_keys(&entry_key, key) == std::cmp::Ordering::Greater {
                    return Ok(None);
                }
            }
        }

        Ok(None)
    }

    /// Iterate over all entries in this bucket.
    ///
    /// This streams entries from disk sequentially using buffered I/O,
    /// holding only one entry plus a read buffer (~8 KB) in memory at a time.
    /// This is O(1) memory regardless of file size.
    pub fn iter(&self) -> Result<DiskBucketIter> {
        let file = File::open(&self.file_path)?;
        let file_len = file.metadata()?.len();
        Ok(DiskBucketIter {
            records: crate::record::RecordMarkedReader::new(BufReader::new(file), file_len),
            failed: false,
        })
    }

    /// Iterate over entries starting from a byte offset, yielding record sizes.
    ///
    /// This is optimized for the eviction scan: it seeks directly to
    /// `start_offset` in the file (avoiding reading/skipping millions of
    /// entries), and returns the on-disk record size with each entry (avoiding
    /// expensive XDR re-serialization just to compute byte sizes).
    ///
    /// Each item is `(BucketEntry, record_size)` where `record_size` includes
    /// the 4-byte record mark.
    pub fn iter_from_offset_with_sizes(&self, start_offset: u64) -> Result<DiskBucketOffsetIter> {
        let file = File::open(&self.file_path)?;
        let file_len = file.metadata()?.len();
        let mut reader = BufReader::new(file);

        if start_offset > 0 && start_offset < file_len {
            reader.seek(SeekFrom::Start(start_offset))?;
        }

        Ok(DiskBucketOffsetIter {
            records: crate::record::RecordMarkedReader::new_at(
                reader,
                file_len,
                if start_offset < file_len {
                    start_offset
                } else {
                    file_len
                },
            ),
            failed: false,
        })
    }
}

impl std::fmt::Debug for DiskBucket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskBucket")
            .field("hash", &self.hash.to_hex())
            .field("entries", &self.entry_count)
            .field("file", &self.file_path)
            .finish()
    }
}

/// Iterator over entries in a disk bucket.
///
/// This iterator streams entries from disk using buffered I/O, reading one
/// entry at a time. Memory usage is O(1) — only the current entry and a
/// small read buffer (~8 KB) are held in memory, regardless of file size.
///
/// This matches the stellar-core `BucketInputIterator` behavior.
///
/// # Format Detection
///
/// The iterator automatically detects whether the bucket uses XDR record
/// marking (RFC 5531) or raw XDR format, handling both transparently.
///
/// # Error Handling
///
/// Parse errors for individual entries are returned as `Result` items.
/// Callers should handle or filter these appropriately.
pub struct DiskBucketIter {
    records: crate::record::RecordMarkedReader<BufReader<File>>,
    failed: bool,
}

impl Iterator for DiskBucketIter {
    type Item = Result<BucketEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        let record = match self.records.next_record() {
            Ok(Some(record)) => record,
            Ok(None) => return None,
            Err(e) => {
                self.failed = true;
                return Some(Err(e));
            }
        };
        match stellar_xdr::curr::BucketEntry::from_xdr(record.body, Limits::none()) {
            Ok(xdr_entry) => Some(Ok(xdr_entry)),
            Err(e) => {
                self.failed = true;
                Some(Err(BucketError::Serialization(format!(
                    "Failed to parse entry at offset {}: {}",
                    record.offset, e
                ))))
            }
        }
    }
}

/// Iterator over disk bucket entries that yields record sizes alongside entries.
///
/// This iterator is designed for the eviction scan where byte offsets and
/// record sizes are needed. It starts from an arbitrary file offset (seeking
/// directly, not iterating from the start) and yields `(BucketEntry, u64)`
/// tuples where the `u64` is the total on-disk record size (4-byte record
/// mark + XDR data), eliminating the need for expensive XDR re-serialization
/// just to compute entry byte sizes.
///
/// All bucket files at this point use XDR record marks (RFC 5531 format).
pub struct DiskBucketOffsetIter {
    records: crate::record::RecordMarkedReader<BufReader<File>>,
    failed: bool,
}

impl Iterator for DiskBucketOffsetIter {
    /// (entry, total_record_size_including_mark)
    type Item = Result<(BucketEntry, u64)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        let record = match self.records.next_record() {
            Ok(Some(record)) => record,
            Ok(None) => return None,
            Err(e) => {
                self.failed = true;
                return Some(Err(e));
            }
        };
        let total_record_size = record.next_offset - record.offset;
        match stellar_xdr::curr::BucketEntry::from_xdr(record.body, Limits::none()) {
            Ok(xdr_entry) => Some(Ok((xdr_entry, total_record_size))),
            Err(e) => {
                self.failed = true;
                Some(Err(BucketError::Serialization(format!(
                    "Failed to parse entry at offset {}: {}",
                    record.offset, e
                ))))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::BucketError;
    use stellar_xdr::curr::*;
    use tempfile::tempdir;

    fn make_test_bucket_bytes() -> Vec<u8> {
        use stellar_xdr::curr::WriteXdr;

        let mut bytes = Vec::new();

        // Create a simple account entry
        let account = AccountEntry {
            account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32]))),
            balance: 100,
            seq_num: SequenceNumber(1),
            num_sub_entries: 0,
            inflation_dest: None,
            flags: 0,
            home_domain: String32::default(),
            thresholds: Thresholds([1, 0, 0, 0]),
            signers: Vec::new().try_into().unwrap(),
            ext: AccountEntryExt::V0,
        };

        let entry = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Account(account),
            ext: LedgerEntryExt::V0,
        };

        let bucket_entry = stellar_xdr::curr::BucketEntry::Liveentry(entry);
        let entry_bytes = bucket_entry.to_xdr(Limits::none()).unwrap();

        // Write with record mark
        let record_mark = (entry_bytes.len() as u32) | crate::XDR_RECORD_MARK;
        bytes.extend_from_slice(&record_mark.to_be_bytes());
        bytes.extend_from_slice(&entry_bytes);

        bytes
    }

    #[test]
    fn test_disk_bucket_creation() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.bucket");

        let bytes = make_test_bucket_bytes();
        let bucket = DiskBucket::from_xdr_bytes(&bytes, &path).unwrap();

        assert!(!bucket.is_empty());
        assert_eq!(bucket.len(), 1);
        assert!(path.exists());
    }

    #[test]
    fn test_streaming_loader_rejects_trailing_partial_mark() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.bucket");
        let valid = make_test_bucket_bytes();
        let valid_bucket =
            DiskBucket::from_xdr_bytes(&valid, dir.path().join("valid.bucket")).unwrap();

        let mut corrupt = valid.clone();
        corrupt.extend_from_slice(&[0xde, 0xad, 0xbe]);
        std::fs::write(&path, corrupt).unwrap();

        assert!(DiskBucket::from_file_streaming(&path).is_err());
        assert_eq!(valid_bucket.hash(), Hash256::hash(&valid));
    }

    #[test]
    fn test_streaming_loader_rejects_truncated_body() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("truncated.bucket");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(100u32 | crate::XDR_RECORD_MARK).to_be_bytes());
        bytes.extend_from_slice(&[1, 2, 3]);
        std::fs::write(&path, bytes).unwrap();

        assert!(DiskBucket::from_file_streaming(&path).is_err());
    }

    #[test]
    fn test_streaming_loader_rejects_undecodable_full_record() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("invalid.bucket");
        let body = [0xff, 0xff, 0xff, 0xff];
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&((body.len() as u32) | crate::XDR_RECORD_MARK).to_be_bytes());
        bytes.extend_from_slice(&body);
        std::fs::write(&path, bytes).unwrap();

        assert!(
            DiskBucket::from_file_streaming(&path).is_err(),
            "complete but invalid XDR records must not be skipped"
        );
    }

    #[test]
    fn test_disk_iterators_emit_one_terminal_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("iter.bucket");
        let mut bytes = make_test_bucket_bytes();
        bytes.push(0xff);
        std::fs::write(&path, &bytes).unwrap();

        let file = File::open(&path).unwrap();
        let file_len = file.metadata().unwrap().len();
        let mut iter = DiskBucketIter {
            records: crate::record::RecordMarkedReader::new(BufReader::new(file), file_len),
            failed: false,
        };
        assert!(iter.next().unwrap().is_ok());
        assert!(iter.next().unwrap().is_err());
        assert!(iter.next().is_none());

        let file = File::open(&path).unwrap();
        let mut sized_iter = DiskBucketOffsetIter {
            records: crate::record::RecordMarkedReader::new(BufReader::new(file), file_len),
            failed: false,
        };
        assert!(sized_iter.next().unwrap().is_ok());
        assert!(sized_iter.next().unwrap().is_err());
        assert!(sized_iter.next().is_none());
    }

    #[test]
    fn test_disk_iterator_rejects_undecodable_full_record() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("invalid-iter.bucket");
        let body = [0xff, 0xff, 0xff, 0xff];
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&((body.len() as u32) | crate::XDR_RECORD_MARK).to_be_bytes());
        bytes.extend_from_slice(&body);
        std::fs::write(&path, &bytes).unwrap();

        let file = File::open(&path).unwrap();
        let file_len = file.metadata().unwrap().len();
        let mut iter = DiskBucketIter {
            records: crate::record::RecordMarkedReader::new(BufReader::new(file), file_len),
            failed: false,
        };
        assert!(iter.next().unwrap().is_err());
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_scan_page_for_key_rejects_truncated_record() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("scan.bucket");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(100u32 | crate::XDR_RECORD_MARK).to_be_bytes());
        bytes.extend_from_slice(&[1, 2, 3]);
        std::fs::write(&path, bytes).unwrap();

        let bucket = DiskBucket {
            hash: Hash256::ZERO,
            file_path: path,
            entry_count: 0,
            bloom_seed: DEFAULT_BLOOM_SEED,
            index: Box::new(LiveBucketIndex::from_entries_default(
                std::iter::empty(),
                DEFAULT_BLOOM_SEED,
                0,
            )),
            mmap: OnceLock::new(),
            cache: OnceLock::new(),
        };
        let key = LedgerKey::Account(LedgerKeyAccount {
            account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([9u8; 32]))),
        });
        assert!(bucket.scan_page_for_key(0, &key, 4096).is_err());
    }

    #[test]
    fn test_disk_bucket_lookup() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.bucket");

        let bytes = make_test_bucket_bytes();
        let bucket = DiskBucket::from_xdr_bytes(&bytes, &path).unwrap();

        let key = LedgerKey::Account(LedgerKeyAccount {
            account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32]))),
        });

        let entry = bucket.get(&key).unwrap();
        assert!(entry.is_some());
    }

    fn make_multi_entry_bucket_bytes(count: usize) -> Vec<u8> {
        use stellar_xdr::curr::WriteXdr;

        let mut bytes = Vec::new();

        for i in 0..count {
            let mut id = [0u8; 32];
            id[0] = i as u8;

            let account = AccountEntry {
                account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(id))),
                balance: i as i64 * 100,
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: Vec::new().try_into().unwrap(),
                ext: AccountEntryExt::V0,
            };

            let entry = LedgerEntry {
                last_modified_ledger_seq: 1,
                data: LedgerEntryData::Account(account),
                ext: LedgerEntryExt::V0,
            };

            let bucket_entry = stellar_xdr::curr::BucketEntry::Liveentry(entry);
            let entry_bytes = bucket_entry.to_xdr(Limits::none()).unwrap();

            // Write with record mark
            let record_mark = (entry_bytes.len() as u32) | crate::XDR_RECORD_MARK;
            bytes.extend_from_slice(&record_mark.to_be_bytes());
            bytes.extend_from_slice(&entry_bytes);
        }

        bytes
    }

    #[test]
    fn test_disk_bucket_no_bloom_filter_with_single_entry() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.bucket");

        let bytes = make_test_bucket_bytes(); // Only 1 entry
        let bucket = DiskBucket::from_xdr_bytes(&bytes, &path).unwrap();

        // Single entry bucket should not have a bloom filter
        assert!(!bucket.has_bloom_filter());
        assert_eq!(bucket.bloom_filter_size_bytes(), 0);
    }

    #[test]
    fn test_disk_bucket_bloom_filter_with_multiple_entries() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.bucket");

        let bytes = make_multi_entry_bucket_bytes(10);
        let bucket = DiskBucket::from_xdr_bytes(&bytes, &path).unwrap();

        assert_eq!(bucket.len(), 10);
        // Multiple entries should have a bloom filter
        assert!(bucket.has_bloom_filter());
        assert!(bucket.bloom_filter_size_bytes() > 0);
    }

    #[test]
    fn test_disk_bucket_bloom_filter_no_false_negatives() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.bucket");

        let count = 50;
        let bytes = make_multi_entry_bucket_bytes(count);
        let bucket = DiskBucket::from_xdr_bytes(&bytes, &path).unwrap();

        assert!(bucket.has_bloom_filter());

        // All keys that are in the bucket must be found
        for i in 0..count {
            let mut id = [0u8; 32];
            id[0] = i as u8;
            let key = LedgerKey::Account(LedgerKeyAccount {
                account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(id))),
            });

            let entry = bucket.get(&key).unwrap();
            assert!(entry.is_some(), "Entry {} should be found", i);
        }
    }

    #[test]
    fn test_disk_bucket_bloom_filter_rejects_missing_keys() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.bucket");

        // Create bucket with entries 0-9
        let bytes = make_multi_entry_bucket_bytes(10);
        let bucket = DiskBucket::from_xdr_bytes(&bytes, &path).unwrap();

        assert!(bucket.has_bloom_filter());

        // Keys 100-199 should not be found
        for i in 100..200 {
            let mut id = [0u8; 32];
            id[0] = i as u8;
            let key = LedgerKey::Account(LedgerKeyAccount {
                account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(id))),
            });

            let entry = bucket.get(&key).unwrap();
            assert!(entry.is_none(), "Entry {} should not be found", i);
        }
    }

    #[test]
    fn test_disk_bucket_with_custom_bloom_seed() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.bucket");

        let custom_seed: HashSeed = [
            0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE,
            0xFF, 0x00,
        ];

        let bytes = make_multi_entry_bucket_bytes(10);
        let bucket = DiskBucket::from_xdr_bytes_with_seed(&bytes, &path, custom_seed).unwrap();

        assert!(bucket.has_bloom_filter());
        assert_eq!(bucket.bloom_seed(), custom_seed);

        // Should still find all entries
        for i in 0..10 {
            let mut id = [0u8; 32];
            id[0] = i as u8;
            let key = LedgerKey::Account(LedgerKeyAccount {
                account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(id))),
            });

            let entry = bucket.get(&key).unwrap();
            assert!(entry.is_some(), "Entry {} should be found", i);
        }
    }

    #[test]
    fn test_from_prebuilt_rejects_hash_mismatch() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.bucket");

        let bytes = make_test_bucket_bytes();
        let bucket = DiskBucket::from_xdr_bytes(&bytes, &path).unwrap();

        let real_hash = bucket.hash();
        let wrong_hash = Hash256::hash(b"wrong");
        assert_ne!(wrong_hash, real_hash, "Test requires distinct hashes");

        let index = bucket.live_index().clone();
        let entry_count = bucket.len();

        let result = DiskBucket::from_prebuilt(&path, wrong_hash, entry_count, index);

        match result {
            Err(BucketError::HashMismatch { expected, actual }) => {
                assert_eq!(expected, wrong_hash.to_hex());
                assert_eq!(actual, real_hash.to_hex());
            }
            Err(other) => panic!("Expected HashMismatch, got: {:?}", other),
            Ok(_) => panic!("Expected error, got Ok"),
        }
    }

    /// Build raw XDR bytes for a single account entry with the given key byte.
    fn make_entry_bytes(key_byte: u8) -> Vec<u8> {
        use stellar_xdr::curr::WriteXdr;

        let account = AccountEntry {
            account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([key_byte; 32]))),
            balance: 100,
            seq_num: SequenceNumber(1),
            num_sub_entries: 0,
            inflation_dest: None,
            flags: 0,
            home_domain: String32::default(),
            thresholds: Thresholds([1, 0, 0, 0]),
            signers: Vec::new().try_into().unwrap(),
            ext: AccountEntryExt::V0,
        };
        let entry = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Account(account),
            ext: LedgerEntryExt::V0,
        };
        let bucket_entry = stellar_xdr::curr::BucketEntry::Liveentry(entry);
        let entry_bytes = bucket_entry.to_xdr(Limits::none()).unwrap();

        let mut bytes = Vec::new();
        let record_mark = (entry_bytes.len() as u32) | crate::XDR_RECORD_MARK;
        bytes.extend_from_slice(&record_mark.to_be_bytes());
        bytes.extend_from_slice(&entry_bytes);
        bytes
    }

    #[test]
    fn test_streaming_loader_rejects_unsorted_entries() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("unsorted.bucket");

        // Write entries out of order: key [2] then key [1]
        let mut file_bytes = Vec::new();
        file_bytes.extend(make_entry_bytes(2));
        file_bytes.extend(make_entry_bytes(1));
        std::fs::write(&path, &file_bytes).unwrap();

        let result = DiskBucket::from_file_streaming(&path);
        assert!(
            result.is_err(),
            "streaming loader should reject unsorted entries"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("corruption"),
            "Expected corruption error, got: {err_msg}"
        );
    }

    #[test]
    fn test_streaming_loader_rejects_duplicate_keys() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("duplicates.bucket");

        // Write duplicate keys: key [1] twice
        let mut file_bytes = Vec::new();
        file_bytes.extend(make_entry_bytes(1));
        file_bytes.extend(make_entry_bytes(1));
        std::fs::write(&path, &file_bytes).unwrap();

        let result = DiskBucket::from_file_streaming(&path);
        assert!(
            result.is_err(),
            "streaming loader should reject duplicate keys"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("corruption"),
            "Expected corruption error, got: {err_msg}"
        );
    }

    #[test]
    fn test_streaming_loader_accepts_sorted_entries() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sorted.bucket");

        // Write entries in correct order: key [1] then key [2]
        let mut file_bytes = Vec::new();
        file_bytes.extend(make_entry_bytes(1));
        file_bytes.extend(make_entry_bytes(2));
        std::fs::write(&path, &file_bytes).unwrap();

        let result = DiskBucket::from_file_streaming(&path);
        assert!(
            result.is_ok(),
            "streaming loader should accept sorted entries"
        );
        assert_eq!(result.unwrap().len(), 2);
    }
}
