//! BucketManager - manages bucket files on disk.
//!
//! The `BucketManager` is responsible for the lifecycle of bucket files,
//! providing a high-level interface for bucket operations.
//!
//! # Responsibilities
//!
//! - **Creating** buckets from entries (sort, serialize, compress, write to disk)
//! - **Loading** buckets by content hash (from cache or disk)
//! - **Caching** frequently accessed buckets in memory
//! - **Merging** buckets and storing the results
//! - **Garbage collecting** unused bucket files
//!
//! # File Layout
//!
//! Bucket files are stored in a configurable directory with names derived
//! from their content hash:
//!
//! ```text
//! <bucket_dir>/
//!   <hash1>.bucket.xdr
//!   <hash2>.bucket.xdr
//!   ...
//! ```
//!
//! All bucket files use the single canonical `.bucket.xdr` extension.
//! Files are uncompressed XDR with record marks (RFC 5531). The hash is
//! computed from these contents. This format supports random-access seeks
//! for efficient disk-backed indexing and streaming iteration.
//!
//! # Caching
//!
//! The manager maintains an in-memory cache of recently accessed buckets.
//! The cache uses a simple eviction policy (random eviction when full).
//! For production use, consider a more sophisticated LRU policy.
//!
//! # Thread Safety
//!
//! The manager uses `RwLock` for the cache, making it safe for concurrent
//! reads with exclusive writes. File operations are atomic (write to temp,
//! then rename).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use stellar_xdr::curr::{LedgerEntry, LedgerKey, Limits, WriteXdr};

use henyey_common::fs_utils::durable_rename;
use henyey_common::Hash256;

use crate::bucket::Bucket;
use crate::entry::BucketEntry;
use crate::merge::{merge_buckets_to_file, DeadEntryPolicy, InitEntryPolicy, MergeOptions};
use crate::merge_map::BucketMergeMap;
use crate::{BucketError, Result};

use std::sync::atomic::{AtomicU64, Ordering};

/// Global counter for generating unique temp file names.
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Return the canonical bucket filename for a given hash.
///
/// All bucket files use the single `.bucket.xdr` extension: uncompressed XDR
/// with record marks (RFC 5531).  The hash is the hex-encoded SHA-256 of the
/// file contents.
pub fn canonical_bucket_filename(hash: &Hash256) -> String {
    format!("{}.bucket.xdr", hash.to_hex())
}

/// Generate a unique temporary file path in the given directory.
pub(crate) fn temp_merge_path(bucket_dir: &Path) -> PathBuf {
    let id = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    bucket_dir.join(format!("merge-tmp-{}-{}.xdr", std::process::id(), id))
}

/// Promote a temporary merge output file to its permanent canonical path.
///
/// Given a `temp_path` containing a freshly-merged bucket and the expected
/// `hash`, this function attempts to place the file at `{dir}/{hash}.bucket.xdr`.
///
/// - If no file exists at the permanent path, renames temp → permanent.
/// - If a file already exists, validates its hash. If valid, removes temp.
///   If corrupt/unreadable, replaces it with the temp file.
/// - Returns a disk-backed `Bucket` pointing to the final file location.
///
/// This is used by `AsyncMergeHandle::start_merge`, `perform_merge`, and
/// `BucketManager::merge` to avoid duplicating the same promote-and-validate
/// logic.
pub(crate) fn promote_temp_to_canonical(
    temp_path: &Path,
    dir: &Path,
    hash: &Hash256,
    label: &str,
) -> crate::Result<Bucket> {
    let permanent_path = dir.join(canonical_bucket_filename(hash));
    if !permanent_path.exists() {
        match durable_rename(temp_path, &permanent_path) {
            Ok(()) => Bucket::from_xdr_file_disk_backed(&permanent_path),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    temp = %temp_path.display(),
                    dest = %permanent_path.display(),
                    "{label}: failed to rename merge output to permanent path, using temp path"
                );
                Bucket::from_xdr_file_disk_backed(temp_path)
            }
        }
    } else {
        match Bucket::from_xdr_file_disk_backed(&permanent_path) {
            Ok(existing) if existing.hash() == *hash => {
                let _ = std::fs::remove_file(temp_path);
                Ok(existing)
            }
            Ok(existing) => {
                tracing::warn!(
                    expected_hash = %hash.to_hex(),
                    actual_hash = %existing.hash().to_hex(),
                    path = %permanent_path.display(),
                    "{label}: permanent bucket file has wrong hash, replacing"
                );
                match durable_rename(temp_path, &permanent_path) {
                    Ok(()) => Bucket::from_xdr_file_disk_backed(&permanent_path),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            temp = %temp_path.display(),
                            dest = %permanent_path.display(),
                            "{label}: failed to replace corrupt permanent file, using temp path"
                        );
                        Bucket::from_xdr_file_disk_backed(temp_path)
                    }
                }
            }
            Err(load_err) => {
                tracing::warn!(
                    error = %load_err,
                    path = %permanent_path.display(),
                    "{label}: failed to load permanent bucket file, replacing"
                );
                match durable_rename(temp_path, &permanent_path) {
                    Ok(()) => Bucket::from_xdr_file_disk_backed(&permanent_path),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            temp = %temp_path.display(),
                            dest = %permanent_path.display(),
                            "{label}: failed to replace unreadable permanent file, using temp path"
                        );
                        Bucket::from_xdr_file_disk_backed(temp_path)
                    }
                }
            }
        }
    }
}

/// Manager for bucket files on disk.
///
/// The `BucketManager` provides the main interface for working with buckets
/// on disk. It handles all the complexity of serialization, compression,
/// caching, and file management.
///
/// # Example
///
/// ```ignore
/// use henyey_bucket::{BucketManager, BucketEntry};
///
/// // Create a manager
/// let manager = BucketManager::new("/path/to/buckets".into())?;
///
/// // Create a bucket from entries
/// let entries = vec![BucketEntry::Liveentry(some_entry)];
/// let bucket = manager.create_bucket(entries)?;
///
/// // Later, load the bucket by hash
/// let loaded = manager.load_bucket(&bucket.hash())?;
///
/// // Garbage collect unused buckets
/// let active_hashes = bucket_list.all_bucket_hashes();
/// manager.retain_buckets(&active_hashes)?;
/// ```
///
/// # Cache Behavior
///
/// The manager caches buckets in memory up to `max_cache_size`. When the
/// cache is full, a random entry is evicted. All cache operations are
/// thread-safe via `RwLock`.
pub struct BucketManager {
    /// Directory where bucket files are stored.
    bucket_dir: PathBuf,
    /// Cache of loaded buckets, keyed by content hash.
    cache: RwLock<HashMap<Hash256, Arc<Bucket>>>,
    /// Maximum number of buckets to keep in cache.
    max_cache_size: usize,
    /// Whether to persist/load disk indexes alongside bucket files.
    persist_index: bool,
    /// Tracks completed merges for deduplication/reattachment.
    finished_merges: RwLock<BucketMergeMap>,
}

impl BucketManager {
    /// Default maximum cache size.
    pub const DEFAULT_MAX_CACHE_SIZE: usize = 100;

    /// File size threshold (in bytes) above which buckets are loaded as DiskBacked
    /// instead of InMemory. Files larger than this threshold are accessed via
    /// random disk I/O with an in-memory index, avoiding loading the full file.
    ///
    /// Default: 10 MB. Files under this size are small enough to load entirely
    /// into memory without significant memory pressure.
    pub const DISK_BACKED_THRESHOLD: u64 = 10 * 1024 * 1024;

    fn build(bucket_dir: PathBuf, max_cache_size: usize, persist_index: bool) -> Result<Self> {
        std::fs::create_dir_all(&bucket_dir)?;

        Ok(Self {
            bucket_dir,
            cache: RwLock::new(HashMap::new()),
            max_cache_size,
            persist_index,
            finished_merges: RwLock::new(BucketMergeMap::new()),
        })
    }

    fn cached_bucket(&self, hash: &Hash256) -> Option<Arc<Bucket>> {
        self.cache.read().unwrap().get(hash).cloned()
    }

    /// Create a new BucketManager with the given directory.
    pub fn new(bucket_dir: PathBuf) -> Result<Self> {
        Self::build(bucket_dir, Self::DEFAULT_MAX_CACHE_SIZE, false)
    }

    /// Create a new BucketManager with a custom cache size.
    pub fn with_cache_size(bucket_dir: PathBuf, max_cache_size: usize) -> Result<Self> {
        Self::build(bucket_dir, max_cache_size, false)
    }

    /// Create a new BucketManager with index persistence enabled.
    ///
    /// When `persist_index` is true, disk indexes are saved alongside bucket files
    /// and loaded on startup, avoiding expensive index rebuilds.
    pub fn with_persist_index(bucket_dir: PathBuf, persist_index: bool) -> Result<Self> {
        Self::build(bucket_dir, Self::DEFAULT_MAX_CACHE_SIZE, persist_index)
    }

    /// Returns whether index persistence is enabled.
    pub fn persist_index(&self) -> bool {
        self.persist_index
    }

    /// Returns a reference to the finished merges map.
    ///
    /// This can be used to record completed merges (for deduplication)
    /// and to check if a merge was previously completed before starting
    /// a new one.
    pub fn finished_merges(&self) -> &RwLock<BucketMergeMap> {
        &self.finished_merges
    }

    /// Get the bucket directory path.
    pub fn bucket_dir(&self) -> &Path {
        &self.bucket_dir
    }

    /// Get the file path for a bucket with the given hash.
    ///
    /// Returns the path using the canonical `.bucket.xdr` extension
    /// (uncompressed XDR with record marks).
    pub fn bucket_path(&self, hash: &Hash256) -> PathBuf {
        self.bucket_dir.join(canonical_bucket_filename(hash))
    }

    /// Create a bucket from entries.
    ///
    /// This will:
    /// 1. Sort entries by key
    /// 2. Create the bucket
    /// 3. Save to disk as uncompressed XDR
    /// 4. Add to cache
    pub fn create_bucket(&self, entries: Vec<BucketEntry>) -> Result<Arc<Bucket>> {
        if entries.is_empty() {
            return Ok(Arc::new(Bucket::empty()));
        }

        // Create bucket (entries will be sorted)
        let bucket = Bucket::from_entries(entries)?;
        let hash = bucket.hash();

        // Check if already cached
        if let Some(cached) = self.cached_bucket(&hash) {
            return Ok(cached);
        }

        // Save to disk as uncompressed XDR
        let path = self.bucket_path(&hash);
        bucket.save_to_xdr_file(&path)?;

        // Add to cache
        let bucket = Arc::new(bucket);
        self.add_to_cache(hash, Arc::clone(&bucket));

        Ok(bucket)
    }

    /// Create a bucket from live and dead entries.
    pub fn create_bucket_from_ledger_entries(
        &self,
        live_entries: Vec<LedgerEntry>,
        dead_entries: Vec<LedgerKey>,
    ) -> Result<Arc<Bucket>> {
        let mut entries: Vec<BucketEntry> = live_entries
            .into_iter()
            .map(BucketEntry::Liveentry)
            .collect();

        entries.extend(dead_entries.into_iter().map(BucketEntry::Deadentry));

        self.create_bucket(entries)
    }

    /// Load a bucket by its hash.
    ///
    /// First checks the cache, then loads from disk if not cached.
    /// The bucket must exist as a `.bucket.xdr` file in the bucket directory.
    ///
    /// Files larger than `DISK_BACKED_THRESHOLD` are loaded as DiskBacked
    /// buckets (only the index is in memory); smaller files are loaded entirely
    /// into memory for faster access.
    pub fn load_bucket(&self, hash: &Hash256) -> Result<Arc<Bucket>> {
        // Check if it's the empty bucket
        if hash.is_zero() {
            return Ok(Arc::new(Bucket::empty()));
        }

        // Check cache first
        if let Some(bucket) = self.cached_bucket(hash) {
            return Ok(bucket);
        }

        // Load from canonical .bucket.xdr path
        let xdr_path = self.bucket_path(hash);
        if !xdr_path.exists() {
            return Err(BucketError::NotFound(hash.to_hex()));
        }

        // Load from .bucket.xdr based on file size
        let file_size = std::fs::metadata(&xdr_path)?.len();
        let bucket = if file_size > Self::DISK_BACKED_THRESHOLD {
            // Large file: use DiskBacked
            tracing::debug!(
                hash = %hash,
                file_size,
                "Loading bucket as DiskBacked (file exceeds threshold)"
            );

            // Try loading a persisted index first (skips both file-scanning passes)
            if let Some(disk_index) =
                self.try_load_index_for_bucket(hash, crate::index::DEFAULT_PAGE_SIZE)?
            {
                let entry_count = disk_index.counters().total() as usize;
                let live_index = crate::index::LiveBucketIndex::Disk(disk_index);
                tracing::debug!(
                    hash = %hash,
                    entry_count,
                    "Loaded bucket with persisted index (skipped streaming build)"
                );
                Bucket::from_xdr_file_disk_backed_prebuilt(
                    &xdr_path,
                    *hash,
                    entry_count,
                    live_index,
                )?
            } else {
                // No persisted index: do full streaming build (2-pass)
                let bucket = Bucket::from_xdr_file_disk_backed(&xdr_path)?;

                // Save index for next time (only for DiskIndex, not InMemoryIndex)
                if let Some(crate::index::LiveBucketIndex::Disk(ref disk_idx)) = bucket.live_index()
                {
                    if let Err(e) = self.save_index_for_bucket(hash, disk_idx) {
                        tracing::warn!(
                            hash = %hash,
                            error = %e,
                            "Failed to persist bucket index"
                        );
                    }
                }

                bucket
            }
        } else {
            // Small file: load entirely into memory for fast access
            Bucket::load_from_xdr_file(&xdr_path)?
        };

        // Verify hash matches
        if bucket.hash() != *hash {
            return Err(BucketError::HashMismatch {
                expected: hash.to_hex(),
                actual: bucket.hash().to_hex(),
            });
        }

        // Add to cache
        let bucket = Arc::new(bucket);
        self.add_to_cache(*hash, Arc::clone(&bucket));

        Ok(bucket)
    }

    /// Load a hot archive bucket by its hash.
    ///
    /// Hot archive buckets contain `HotArchiveBucketEntry` instead of `BucketEntry`.
    /// This method loads and parses the bucket file with the correct entry type.
    /// The bucket must exist as a `.bucket.xdr` file in the bucket directory.
    ///
    /// For files larger than `DISK_BACKED_THRESHOLD`, creates a DiskBacked bucket
    /// that only holds an index in memory (matching stellar-core behavior for hot archive).
    ///
    /// Note: Hot archive buckets are not cached (they use a different entry type).
    pub fn load_hot_archive_bucket(
        &self,
        hash: &Hash256,
    ) -> Result<crate::hot_archive::HotArchiveBucket> {
        // Check if it's the empty bucket
        if hash.is_zero() {
            return Ok(crate::hot_archive::HotArchiveBucket::empty());
        }

        // Load from canonical .bucket.xdr path
        let xdr_path = self.bucket_path(hash);
        if !xdr_path.exists() {
            return Err(BucketError::NotFound(hash.to_hex()));
        }

        // Load based on file size: DiskBacked for large files, InMemory for small
        let file_size = std::fs::metadata(&xdr_path)?.len();
        let bucket = if file_size > Self::DISK_BACKED_THRESHOLD {
            crate::hot_archive::HotArchiveBucket::from_xdr_file_disk_backed(&xdr_path)?
        } else {
            crate::hot_archive::HotArchiveBucket::load_from_xdr_file(&xdr_path)?
        };

        // Verify hash matches
        if bucket.hash() != *hash {
            return Err(BucketError::HashMismatch {
                expected: hash.to_hex(),
                actual: bucket.hash().to_hex(),
            });
        }

        Ok(bucket)
    }

    /// Check if a bucket exists (in cache or on disk).
    pub fn bucket_exists(&self, hash: &Hash256) -> bool {
        if hash.is_zero() {
            return true; // Empty bucket always "exists"
        }

        // Check cache
        if self.cached_bucket(hash).is_some() {
            return true;
        }

        // Check disk (canonical path only)
        self.bucket_path(hash).exists()
    }

    /// Merge two buckets and create a new bucket.
    ///
    /// Uses disk-backed streaming merge to avoid loading all entries into memory.
    /// The merge output is written directly to disk and the resulting bucket is
    /// DiskBacked, keeping memory usage O(index_size) instead of O(data_size).
    pub fn merge(
        &self,
        old: &Bucket,
        new: &Bucket,
        max_protocol_version: u32,
    ) -> Result<Arc<Bucket>> {
        if old.is_empty() && new.is_empty() {
            return Ok(Arc::new(Bucket::empty()));
        }

        // Write merge output to a temp file
        let temp_path = temp_merge_path(&self.bucket_dir);
        let (hash, entry_count) = merge_buckets_to_file(
            old,
            new,
            &temp_path,
            &MergeOptions {
                keep_dead_entries: DeadEntryPolicy::Keep,
                max_protocol_version,
                normalize_init_entries: InitEntryPolicy::NormalizeToLive,
                ..Default::default()
            },
        )?;

        if hash.is_zero() || entry_count == 0 {
            let _ = std::fs::remove_file(&temp_path);
            return Ok(Arc::new(Bucket::empty()));
        }

        // Check if already cached
        if let Some(cached) = self.cached_bucket(&hash) {
            let _ = std::fs::remove_file(&temp_path);
            return Ok(cached);
        }

        // Promote temp file to canonical path and load as disk-backed bucket
        let bucket =
            promote_temp_to_canonical(&temp_path, &self.bucket_dir, &hash, "BucketManager::merge")?;

        // Persist the index for next time (only for DiskIndex, not InMemoryIndex)
        if let Some(crate::index::LiveBucketIndex::Disk(ref disk_idx)) = bucket.live_index() {
            if let Err(e) = self.save_index_for_bucket(&hash, disk_idx) {
                tracing::warn!(
                    hash = %hash,
                    error = %e,
                    "Failed to persist merged bucket index"
                );
            }
        }

        let bucket = Arc::new(bucket);
        self.add_to_cache(hash, Arc::clone(&bucket));

        Ok(bucket)
    }

    /// Merge two buckets asynchronously.
    pub async fn merge_async(
        &self,
        old: &Bucket,
        new: &Bucket,
        max_protocol_version: u32,
    ) -> Result<Arc<Bucket>> {
        // For now, just call the sync version
        // In the future, this could use tokio::task::spawn_blocking
        self.merge(old, new, max_protocol_version)
    }

    /// Save a disk index for a bucket.
    ///
    /// Only effective when `persist_index` is true. The index is saved as a
    /// `.index` file alongside the bucket file.
    ///
    /// # Arguments
    ///
    /// * `hash` - The bucket hash (used to derive file paths)
    /// * `index` - The DiskIndex to persist
    pub fn save_index_for_bucket(
        &self,
        hash: &Hash256,
        index: &crate::index::DiskIndex,
    ) -> Result<()> {
        if !self.persist_index {
            return Ok(());
        }
        let bucket_path = self.bucket_path(hash);
        crate::index_persistence::save_disk_index(index, &bucket_path)
    }

    /// Try to load a persisted disk index for a bucket.
    ///
    /// Returns `None` if persistence is disabled, the file doesn't exist,
    /// or the stored version/page-size doesn't match.
    ///
    /// # Arguments
    ///
    /// * `hash` - The bucket hash
    /// * `expected_page_size` - Expected page size for validation
    pub fn try_load_index_for_bucket(
        &self,
        hash: &Hash256,
        expected_page_size: u64,
    ) -> Result<Option<crate::index::DiskIndex>> {
        if !self.persist_index {
            return Ok(None);
        }
        let bucket_path = self.bucket_path(hash);
        crate::index_persistence::load_disk_index(&bucket_path, expected_page_size)
    }

    /// Add a bucket to the cache.
    fn add_to_cache(&self, hash: Hash256, bucket: Arc<Bucket>) {
        let mut cache = self.cache.write().unwrap();

        // Evict if cache is full (simple LRU would be better)
        if cache.len() >= self.max_cache_size {
            // Remove a random entry (not ideal, but simple)
            if let Some(key) = cache.keys().next().cloned() {
                cache.remove(&key);
            }
        }

        cache.insert(hash, bucket);
    }

    /// Clear the bucket cache.
    pub fn clear_cache(&self) {
        let mut cache = self.cache.write().unwrap();
        cache.clear();
    }

    /// Get the number of cached buckets.
    pub fn cache_size(&self) -> usize {
        let cache = self.cache.read().unwrap();
        cache.len()
    }

    /// List all bucket files in the directory.
    pub fn list_buckets(&self) -> Result<Vec<Hash256>> {
        let mut hashes = Vec::new();

        for entry in std::fs::read_dir(&self.bucket_dir)? {
            let entry = entry?;
            let path = entry.path();

            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if let Some(hash_str) = name.strip_suffix(".bucket.xdr") {
                    if let Ok(hash) = Hash256::from_hex(hash_str) {
                        hashes.push(hash);
                    }
                }
            }
        }

        Ok(hashes)
    }

    /// Delete a bucket file.
    ///
    /// This also removes the bucket from the cache and deletes the
    /// associated `.index` file when `persist_index` is true.
    pub fn delete_bucket(&self, hash: &Hash256) -> Result<()> {
        // Remove from cache
        {
            let mut cache = self.cache.write().unwrap();
            cache.remove(hash);
        }

        // Delete canonical .bucket.xdr file
        let xdr_path = self.bucket_path(hash);
        if xdr_path.exists() {
            // Delete associated index file
            if self.persist_index {
                let _ = crate::index_persistence::delete_index(&xdr_path);
            }
            std::fs::remove_file(&xdr_path)?;
        }

        Ok(())
    }

    /// Delete all bucket files not in the given set of hashes.
    ///
    /// This is useful for garbage collection. When `persist_index` is enabled,
    /// also cleans up orphaned `.index` files.
    pub fn retain_buckets(&self, keep: &[Hash256]) -> Result<usize> {
        let keep_set: std::collections::HashSet<_> = keep.iter().collect();
        let all_buckets = self.list_buckets()?;
        let mut deleted = 0;

        for hash in all_buckets {
            if !keep_set.contains(&hash) {
                self.delete_bucket(&hash)?;
                deleted += 1;
            }
        }

        // Clean up merge map entries whose outputs are no longer retained
        {
            let keep_hash_set: std::collections::HashSet<Hash256> = keep.iter().copied().collect();
            let mut merge_map = self.finished_merges.write().unwrap();
            merge_map.retain_outputs(&keep_hash_set);
        }

        // Clean up any orphaned index files
        if self.persist_index {
            let _ = crate::index_persistence::cleanup_orphaned_indexes(&self.bucket_dir);
        }

        Ok(deleted)
    }

    /// Get statistics about the bucket manager.
    pub fn stats(&self) -> BucketManagerStats {
        let cached = self.cache_size();
        let on_disk = self.list_buckets().map(|v| v.len()).unwrap_or(0);

        BucketManagerStats {
            cached_buckets: cached,
            disk_buckets: on_disk,
            bucket_dir: self.bucket_dir.clone(),
        }
    }

    /// Import a bucket from raw XDR bytes.
    ///
    /// Saves the raw bytes to disk first, then loads via the threshold-aware
    /// path (DiskBacked for files > 10 MB, InMemory for smaller files).
    /// This avoids loading all entries into memory for large buckets.
    pub fn import_bucket(&self, xdr_bytes: &[u8]) -> Result<Arc<Bucket>> {
        // Compute hash directly from raw bytes (no entry parsing needed)
        let hash = Hash256::hash(xdr_bytes);

        // Check cache
        if let Some(cached) = self.cached_bucket(&hash) {
            return Ok(cached);
        }

        // Save raw XDR bytes to disk if not already there
        let path = self.bucket_path(&hash);
        if !path.exists() {
            use std::io::Write;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut file = std::fs::File::create(&path)?;
            file.write_all(xdr_bytes)?;
            file.sync_all()?;
        }

        // Load via threshold-aware path (DiskBacked for large, InMemory for small)
        self.load_bucket(&hash)
    }

    /// Export a bucket to raw XDR bytes.
    pub fn export_bucket(&self, hash: &Hash256) -> Result<Vec<u8>> {
        let bucket = self.load_bucket(hash)?;
        bucket.to_xdr_bytes()
    }

    /// Visits all ledger entries in buckets matching the filter criteria.
    ///
    /// This method iterates through bucket entries, applying a filter function
    /// to determine which entries to process, and an accept function to handle
    /// matching entries.
    ///
    /// # Arguments
    ///
    /// * `buckets` - Iterator over buckets to scan
    /// * `filter_entry` - Function that returns `true` for entries to consider
    /// * `accept_entry` - Function called for each matching entry; return `false` to stop
    /// * `min_ledger` - Optional minimum ledger sequence to filter entries
    ///
    /// # Returns
    ///
    /// `true` if iteration completed, `false` if stopped early by `accept_entry`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// manager.visit_ledger_entries(
    ///     &bucket_hashes,
    ///     |entry| matches!(entry.data, LedgerEntryData::Account(_)),
    ///     |entry| {
    ///         println!("Found account: {:?}", entry);
    ///         true // continue
    ///     },
    ///     Some(1000), // only entries modified after ledger 1000
    /// );
    /// ```
    pub fn visit_ledger_entries<F, A>(
        &self,
        bucket_hashes: &[Hash256],
        filter_entry: F,
        mut accept_entry: A,
        min_ledger: Option<u32>,
    ) -> Result<bool>
    where
        F: Fn(&LedgerEntry) -> bool,
        A: FnMut(&LedgerEntry) -> bool,
    {
        use std::collections::HashSet;

        // Track seen keys to avoid processing duplicates
        let mut seen_keys: HashSet<LedgerKey> = HashSet::new();

        for hash in bucket_hashes {
            if hash.is_zero() {
                continue;
            }

            let bucket = self.load_bucket(hash)?;

            for entry_result in bucket.iter()? {
                let entry = entry_result?;
                match entry {
                    crate::BucketEntry::Liveentry(ref ledger_entry)
                    | crate::BucketEntry::Initentry(ref ledger_entry) => {
                        // Check min_ledger filter
                        if let Some(min) = min_ledger {
                            if ledger_entry.last_modified_ledger_seq < min {
                                continue;
                            }
                        }

                        // Check filter
                        if !filter_entry(ledger_entry) {
                            continue;
                        }

                        // Check if already seen
                        let key = henyey_common::entry_to_key(ledger_entry);
                        if seen_keys.contains(&key) {
                            continue;
                        }
                        seen_keys.insert(key);

                        // Accept the entry
                        if !accept_entry(ledger_entry) {
                            return Ok(false);
                        }
                    }
                    crate::BucketEntry::Deadentry(ref key) => {
                        // Mark key as seen (it's deleted)
                        seen_keys.insert(key.clone());
                    }
                    crate::BucketEntry::Metaentry(_) => {
                        // Skip metadata entries
                    }
                }
            }
        }

        Ok(true)
    }

    /// Visits all ledger entries of a specific type in the given buckets.
    ///
    /// This is a convenience wrapper around `visit_ledger_entries` that
    /// filters by entry type.
    pub fn visit_ledger_entries_of_type<A>(
        &self,
        bucket_hashes: &[Hash256],
        entry_type: stellar_xdr::curr::LedgerEntryType,
        accept_entry: A,
        min_ledger: Option<u32>,
    ) -> Result<bool>
    where
        A: FnMut(&LedgerEntry) -> bool,
    {
        self.visit_ledger_entries(
            bucket_hashes,
            |entry| crate::entry::ledger_entry_data_type(&entry.data) == entry_type,
            accept_entry,
            min_ledger,
        )
    }
}

impl BucketManager {
    /// Loads the complete ledger state from a list of bucket hashes.
    ///
    /// This iterates through all buckets from oldest to newest (reverse order),
    /// building a map of all live entries. Dead entries shadow older versions.
    /// The result is a complete view of ledger state at the bucket list snapshot.
    ///
    /// This is the Rust equivalent of stellar-core `loadCompleteLedgerState`.
    ///
    /// # Arguments
    ///
    /// * `bucket_hashes` - List of bucket hashes in order from level 10 snap to level 0 curr
    ///   (oldest to newest is correct order for last-write-wins)
    ///
    /// # Returns
    ///
    /// A vector of all live ledger entries, with newer entries shadowing older ones.
    pub fn load_complete_ledger_state(
        &self,
        bucket_hashes: &[Hash256],
    ) -> Result<Vec<LedgerEntry>> {
        use std::collections::BTreeMap;

        // Use BTreeMap to maintain sorted order by key
        let mut state: BTreeMap<Vec<u8>, LedgerEntry> = BTreeMap::new();

        // Process buckets from oldest to newest (so newer entries overwrite older)
        // bucket_hashes should be in order: level 10 snap, level 10 curr, ..., level 0 snap, level 0 curr
        // We iterate in that order so newest entries win
        for hash in bucket_hashes {
            if hash.is_zero() {
                continue;
            }

            let bucket = self.load_bucket(hash)?;

            for entry_result in bucket.iter()? {
                let entry = entry_result?;
                match entry {
                    crate::BucketEntry::Liveentry(ref ledger_entry)
                    | crate::BucketEntry::Initentry(ref ledger_entry) => {
                        let key = henyey_common::entry_to_key(ledger_entry);
                        let key_bytes = key.to_xdr(Limits::none()).map_err(|e| {
                            BucketError::Serialization(format!(
                                "failed to serialize ledger key: {}",
                                e
                            ))
                        })?;
                        state.insert(key_bytes, ledger_entry.clone());
                    }
                    crate::BucketEntry::Deadentry(ref key) => {
                        let key_bytes = key.to_xdr(Limits::none()).map_err(|e| {
                            BucketError::Serialization(format!(
                                "failed to serialize ledger key: {}",
                                e
                            ))
                        })?;
                        // Remove the entry if it exists (dead shadows live)
                        state.remove(&key_bytes);
                    }
                    crate::BucketEntry::Metaentry(_) => {
                        // Skip metadata entries
                    }
                }
            }
        }

        Ok(state.into_values().collect())
    }

    /// Loads the complete hot archive state from a list of hot archive bucket hashes.
    ///
    /// This iterates through all hot archive buckets from oldest to newest,
    /// building a map of all archived entries. `Live` entries (restore markers)
    /// shadow older `Archived` entries, removing them from the result.
    ///
    /// This is the Rust equivalent of stellar-core `loadCompleteHotArchiveState`.
    /// Note: stellar-core defines this function but it has zero callers even in v25;
    /// it is purely diagnostic/admin. We implement it for completeness.
    ///
    /// # Arguments
    ///
    /// * `bucket_hashes` - Hot archive bucket hashes in order from level 10 snap
    ///   to level 0 curr (oldest to newest, so newer entries win)
    ///
    /// # Returns
    ///
    /// A vector of all archived ledger entries, with restore markers applied.
    pub fn load_complete_hot_archive_state(
        &self,
        bucket_hashes: &[Hash256],
    ) -> Result<Vec<LedgerEntry>> {
        use std::collections::BTreeMap;

        // Use BTreeMap to maintain sorted order by key
        let mut state: BTreeMap<Vec<u8>, LedgerEntry> = BTreeMap::new();

        // Process buckets from oldest to newest (so newer entries overwrite older)
        for hash in bucket_hashes {
            if hash.is_zero() {
                continue;
            }

            let bucket = self.load_hot_archive_bucket(hash)?;

            for entry in bucket.iter() {
                match entry {
                    stellar_xdr::curr::HotArchiveBucketEntry::Archived(ref ledger_entry) => {
                        let key = henyey_common::entry_to_key(ledger_entry);
                        let key_bytes = key.to_xdr(Limits::none()).map_err(|e| {
                            BucketError::Serialization(format!(
                                "failed to serialize ledger key: {}",
                                e
                            ))
                        })?;
                        state.insert(key_bytes, ledger_entry.clone());
                    }
                    stellar_xdr::curr::HotArchiveBucketEntry::Live(ref key) => {
                        // Live = restore marker (tombstone) — remove from archive
                        let key_bytes = key.to_xdr(Limits::none()).map_err(|e| {
                            BucketError::Serialization(format!(
                                "failed to serialize ledger key: {}",
                                e
                            ))
                        })?;
                        state.remove(&key_bytes);
                    }
                    stellar_xdr::curr::HotArchiveBucketEntry::Metaentry(_) => {
                        // Skip metadata entries
                    }
                }
            }
        }

        Ok(state.into_values().collect())
    }

    /// Merges all buckets in a bucket list into a single "super bucket".
    ///
    /// This creates a consolidated bucket containing all live entries from
    /// the bucket list. Useful for creating offline archives or testing.
    ///
    /// This is the Rust equivalent of stellar-core `mergeBuckets` (the standalone function
    /// that merges an entire bucket list).
    ///
    /// # Arguments
    ///
    /// * `bucket_hashes` - List of bucket hashes from the bucket list
    /// * `protocol_version` - Protocol version for the output bucket
    ///
    /// # Returns
    ///
    /// A single bucket containing all live entries.
    pub fn merge_all_buckets(
        &self,
        bucket_hashes: &[Hash256],
        protocol_version: u32,
    ) -> Result<Arc<Bucket>> {
        use stellar_xdr::curr::BucketMetadata;

        // Load complete state
        let entries = self.load_complete_ledger_state(bucket_hashes)?;

        if entries.is_empty() {
            return Ok(Arc::new(Bucket::empty()));
        }

        // Create bucket entries with metadata
        let mut bucket_entries: Vec<crate::BucketEntry> = Vec::with_capacity(entries.len() + 1);

        // Add metadata
        bucket_entries.push(crate::BucketEntry::Metaentry(BucketMetadata {
            ledger_version: protocol_version,
            ext: stellar_xdr::curr::BucketMetadataExt::V0,
        }));

        // Add all entries as LIVE (not INIT since these are resolved entries)
        bucket_entries.extend(entries.into_iter().map(crate::BucketEntry::Liveentry));

        // Create and save the merged bucket
        self.create_bucket(bucket_entries)
    }

    /// Verifies that all referenced bucket files exist on disk.
    ///
    /// This is a simple synchronous check. For background verification with
    /// hash validation, use `verify_bucket_hashes`.
    ///
    /// # Arguments
    ///
    /// * `bucket_hashes` - List of bucket hashes to verify
    ///
    /// # Returns
    ///
    /// A list of missing bucket hashes, or empty if all exist.
    pub fn verify_buckets_exist(&self, bucket_hashes: &[Hash256]) -> Vec<Hash256> {
        bucket_hashes
            .iter()
            .filter(|hash| !hash.is_zero() && !self.bucket_exists(hash))
            .copied()
            .collect()
    }

    /// Verifies bucket file contents match their expected hashes.
    ///
    /// This performs a full hash verification by reading and decompressing
    /// each bucket file. This is expensive and should be used sparingly.
    ///
    /// This mirrors stellar-core's `scheduleVerifyReferencedBucketsWork`:
    /// - Verifies both live and hot archive buckets
    /// - Checks file size against `max_bucket_size` before loading
    /// - Errors on missing files (use `verify_buckets_exist` for a softer check)
    ///
    /// # Arguments
    ///
    /// * `live_hashes` - Live bucket hashes to verify
    /// * `hot_archive_hashes` - Hot archive bucket hashes to verify
    /// * `max_bucket_size` - Maximum allowed bucket file size in bytes
    ///   (use `henyey_history::archive_state::MAX_HISTORY_ARCHIVE_BUCKET_SIZE`)
    ///
    /// # Returns
    ///
    /// A list of (expected_hash, actual_hash) pairs for any mismatched buckets.
    /// Returns an error if a referenced bucket file is missing or exceeds the
    /// size limit.
    pub fn verify_referenced_bucket_hashes(
        &self,
        live_hashes: &[Hash256],
        hot_archive_hashes: &[Hash256],
        max_bucket_size: u64,
    ) -> Result<Vec<(Hash256, Hash256)>> {
        let mut mismatches = Vec::new();

        // Verify live buckets
        for expected_hash in live_hashes {
            if expected_hash.is_zero() {
                continue;
            }

            let xdr_path = self.bucket_path(expected_hash);
            if !xdr_path.exists() {
                return Err(BucketError::NotFound(format!(
                    "referenced live bucket file missing: {}",
                    expected_hash.to_hex()
                )));
            }

            // File size guard
            let file_size = std::fs::metadata(&xdr_path)?.len();
            if file_size > max_bucket_size {
                return Err(BucketError::Serialization(format!(
                    "live bucket {} exceeds max size: {} > {}",
                    expected_hash.to_hex(),
                    file_size,
                    max_bucket_size
                )));
            }

            match Bucket::load_from_xdr_file(&xdr_path) {
                Ok(bucket) => {
                    let actual_hash = bucket.hash();
                    if actual_hash != *expected_hash {
                        mismatches.push((*expected_hash, actual_hash));
                    }
                }
                Err(_) => {
                    // File exists but couldn't be loaded - treat as hash mismatch
                    mismatches.push((*expected_hash, Hash256::ZERO));
                }
            }
        }

        // Verify hot archive buckets
        for expected_hash in hot_archive_hashes {
            if expected_hash.is_zero() {
                continue;
            }

            let xdr_path = self.bucket_path(expected_hash);
            if !xdr_path.exists() {
                return Err(BucketError::NotFound(format!(
                    "referenced hot archive bucket file missing: {}",
                    expected_hash.to_hex()
                )));
            }

            // File size guard
            let file_size = std::fs::metadata(&xdr_path)?.len();
            if file_size > max_bucket_size {
                return Err(BucketError::Serialization(format!(
                    "hot archive bucket {} exceeds max size: {} > {}",
                    expected_hash.to_hex(),
                    file_size,
                    max_bucket_size
                )));
            }

            match crate::hot_archive::HotArchiveBucket::load_from_xdr_file(&xdr_path) {
                Ok(bucket) => {
                    let actual_hash = bucket.hash();
                    if actual_hash != *expected_hash {
                        mismatches.push((*expected_hash, actual_hash));
                    }
                }
                Err(_) => {
                    mismatches.push((*expected_hash, Hash256::ZERO));
                }
            }
        }

        Ok(mismatches)
    }

    /// Verifies bucket file contents match their expected hashes (live buckets only).
    ///
    /// This is a convenience wrapper that only checks live buckets. For full
    /// verification including hot archive buckets, use [`Self::verify_referenced_bucket_hashes`].
    ///
    /// **Deprecated behavior**: Missing files are skipped rather than causing an error.
    /// Prefer `verify_referenced_bucket_hashes` for stricter checking.
    pub fn verify_bucket_hashes(
        &self,
        bucket_hashes: &[Hash256],
    ) -> Result<Vec<(Hash256, Hash256)>> {
        let mut mismatches = Vec::new();

        for expected_hash in bucket_hashes {
            if expected_hash.is_zero() {
                continue;
            }

            // Only check canonical .bucket.xdr path
            let xdr_path = self.bucket_path(expected_hash);
            if !xdr_path.exists() {
                return Err(BucketError::NotFound(format!(
                    "referenced bucket file missing: {}",
                    expected_hash.to_hex()
                )));
            }

            match Bucket::load_from_xdr_file(&xdr_path) {
                Ok(bucket) => {
                    let actual_hash = bucket.hash();
                    if actual_hash != *expected_hash {
                        mismatches.push((*expected_hash, actual_hash));
                    }
                }
                Err(_) => {
                    // File exists but couldn't be loaded - treat as hash mismatch
                    mismatches.push((*expected_hash, Hash256::ZERO));
                }
            }
        }

        Ok(mismatches)
    }

    /// Ensures all referenced bucket files exist, downloading if needed.
    ///
    /// This method checks if each bucket exists locally. For missing buckets,
    /// it calls the provided fetch function to obtain the bucket data.
    ///
    /// This supports the `assumeState` flow for restoring from HistoryArchiveState.
    ///
    /// # Arguments
    ///
    /// * `bucket_hashes` - List of bucket hashes required
    /// * `fetch_bucket` - Function that fetches bucket XDR bytes by hash
    ///
    /// # Returns
    ///
    /// The number of buckets that were fetched (not already present).
    pub fn ensure_buckets_exist<F>(
        &self,
        bucket_hashes: &[Hash256],
        mut fetch_bucket: F,
    ) -> Result<usize>
    where
        F: FnMut(&Hash256) -> Result<Vec<u8>>,
    {
        let mut fetched = 0;

        for hash in bucket_hashes {
            if hash.is_zero() {
                continue;
            }

            if self.bucket_exists(hash) {
                continue;
            }

            // Fetch and import the bucket
            let xdr_bytes = fetch_bucket(hash)?;
            let bucket = self.import_bucket(&xdr_bytes)?;

            // Verify hash matches
            if bucket.hash() != *hash {
                return Err(BucketError::HashMismatch {
                    expected: hash.to_hex(),
                    actual: bucket.hash().to_hex(),
                });
            }

            fetched += 1;
        }

        Ok(fetched)
    }

    /// Clean up unreferenced merge temp files from the bucket directory.
    ///
    /// This method removes `merge-tmp-*.xdr` files that are not in the provided
    /// set of referenced file paths. This is useful for garbage collection during
    /// long-running operations like verify-execution where many merge temp files
    /// are created but only a subset remain referenced by the bucket list.
    ///
    /// # Arguments
    ///
    /// * `referenced_paths` - Set of file paths that should be kept (from bucket list's
    ///   `referenced_file_paths()` method)
    ///
    /// # Returns
    ///
    /// The number of files deleted.
    pub fn cleanup_unreferenced_files(
        &self,
        referenced_paths: &std::collections::HashSet<PathBuf>,
    ) -> Result<usize> {
        let mut deleted = 0;
        let current_pid = std::process::id();

        // List all files in bucket directory that match merge-tmp-*.xdr pattern
        for entry in std::fs::read_dir(&self.bucket_dir)? {
            let entry = entry?;
            let path = entry.path();

            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                // Only consider merge temp files
                if name.starts_with("merge-tmp-") && name.ends_with(".xdr") {
                    // Parse the process ID from the filename: merge-tmp-{pid}-{counter}.xdr
                    // Only delete files from PREVIOUS runs (different PID) to avoid race
                    // conditions with in-flight async merges whose output files may not
                    // yet be tracked in referenced_paths.
                    let is_current_process = name
                        .strip_prefix("merge-tmp-")
                        .and_then(|s| s.split('-').next())
                        .and_then(|pid_str| pid_str.parse::<u32>().ok())
                        .map(|pid| pid == current_pid)
                        .unwrap_or(false);

                    // Skip files from the current process - they may be in-flight
                    // and we can't reliably track all output files from async merges.
                    // Files from previous processes are safe to delete.
                    if is_current_process {
                        // For current process files, only delete if explicitly referenced
                        // This is conservative but prevents race conditions.
                        if !referenced_paths.contains(&path) {
                            tracing::trace!(
                                path = %path.display(),
                                "Skipping current-process temp file (may be in-flight)"
                            );
                        }
                        continue;
                    }

                    // Delete unreferenced files from previous runs
                    if !referenced_paths.contains(&path) {
                        tracing::debug!(path = %path.display(), "Deleting unreferenced merge temp file from previous run");
                        if let Err(e) = std::fs::remove_file(&path) {
                            tracing::warn!(
                                path = %path.display(),
                                error = %e,
                                "Failed to delete unreferenced merge temp file"
                            );
                        } else {
                            deleted += 1;
                        }
                    }
                }
            }
        }

        if deleted > 0 {
            tracing::info!(deleted, "Cleaned up unreferenced merge temp files");
        }

        Ok(deleted)
    }
}

impl std::fmt::Debug for BucketManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BucketManager")
            .field("bucket_dir", &self.bucket_dir)
            .field("cache_size", &self.cache_size())
            .field("max_cache_size", &self.max_cache_size)
            .finish()
    }
}

/// Statistics about a BucketManager.
///
/// Provides insight into the manager's state for monitoring and debugging.
#[derive(Debug, Clone)]
pub struct BucketManagerStats {
    /// Number of buckets currently held in the in-memory cache.
    pub cached_buckets: usize,
    /// Number of bucket files on disk (may include garbage).
    pub disk_buckets: usize,
    /// The directory where bucket files are stored.
    pub bucket_dir: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::future_bucket::MergeKey;
    use crate::merge::DeadEntryPolicy;
    use crate::BucketEntry; // Re-import to shadow XDR's BucketEntry
    use stellar_xdr::curr::*;
    use tempfile::TempDir;

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

    fn make_account_key(bytes: [u8; 32]) -> LedgerKey {
        LedgerKey::Account(LedgerKeyAccount {
            account_id: make_account_id(bytes),
        })
    }

    fn create_manager() -> (TempDir, BucketManager) {
        let temp_dir = TempDir::new().unwrap();
        let manager = BucketManager::new(temp_dir.path().to_path_buf()).unwrap();
        (temp_dir, manager)
    }

    #[test]
    fn test_create_bucket() {
        let (_temp_dir, manager) = create_manager();

        let entries = vec![
            BucketEntry::Liveentry(make_account_entry([1u8; 32], 100)),
            BucketEntry::Liveentry(make_account_entry([2u8; 32], 200)),
        ];

        let bucket = manager.create_bucket(entries).unwrap();
        assert_eq!(bucket.len(), 2);
        assert!(!bucket.hash().is_zero());
    }

    #[test]
    fn test_load_bucket() {
        let (_temp_dir, manager) = create_manager();

        let entries = vec![BucketEntry::Liveentry(make_account_entry([1u8; 32], 100))];

        let bucket = manager.create_bucket(entries).unwrap();
        let hash = bucket.hash();

        // Clear cache to force disk load
        manager.clear_cache();

        let loaded = manager.load_bucket(&hash).unwrap();
        assert_eq!(loaded.hash(), hash);
        assert_eq!(loaded.len(), 1);
    }

    #[test]
    fn test_bucket_caching() {
        let (_temp_dir, manager) = create_manager();

        let entries = vec![BucketEntry::Liveentry(make_account_entry([1u8; 32], 100))];

        let bucket = manager.create_bucket(entries).unwrap();
        let hash = bucket.hash();

        assert_eq!(manager.cache_size(), 1);

        // Loading again should use cache
        let loaded = manager.load_bucket(&hash).unwrap();
        assert!(Arc::ptr_eq(&bucket, &loaded));
    }

    #[test]
    fn test_bucket_exists() {
        let (_temp_dir, manager) = create_manager();

        let entries = vec![BucketEntry::Liveentry(make_account_entry([1u8; 32], 100))];

        let bucket = manager.create_bucket(entries).unwrap();
        let hash = bucket.hash();

        assert!(manager.bucket_exists(&hash));
        assert!(!manager.bucket_exists(&Hash256::hash(b"nonexistent")));
        assert!(manager.bucket_exists(&Hash256::ZERO)); // Empty bucket
    }

    #[test]
    fn test_merge_buckets() {
        let (_temp_dir, manager) = create_manager();

        let old_entries = vec![BucketEntry::Liveentry(make_account_entry([1u8; 32], 100))];
        let new_entries = vec![BucketEntry::Liveentry(make_account_entry([1u8; 32], 200))];

        let old_bucket = Bucket::from_entries(old_entries).unwrap();
        let new_bucket = Bucket::from_entries(new_entries).unwrap();

        let merged = manager.merge(&old_bucket, &new_bucket, 0).unwrap();
        assert_eq!(merged.len(), 1);

        let key = make_account_key([1u8; 32]);
        let entry = merged.get_entry(&key).unwrap().unwrap();
        if let LedgerEntryData::Account(account) = &entry.data {
            assert_eq!(account.balance, 200);
        }
    }

    #[test]
    fn test_list_and_delete_buckets() {
        let (_temp_dir, manager) = create_manager();

        let entries1 = vec![BucketEntry::Liveentry(make_account_entry([1u8; 32], 100))];
        let entries2 = vec![BucketEntry::Liveentry(make_account_entry([2u8; 32], 200))];

        let bucket1 = manager.create_bucket(entries1).unwrap();
        let bucket2 = manager.create_bucket(entries2).unwrap();

        let buckets = manager.list_buckets().unwrap();
        assert_eq!(buckets.len(), 2);

        manager.delete_bucket(&bucket1.hash()).unwrap();

        let buckets = manager.list_buckets().unwrap();
        assert_eq!(buckets.len(), 1);
        assert!(buckets.contains(&bucket2.hash()));
    }

    #[test]
    fn test_retain_buckets() {
        let (_temp_dir, manager) = create_manager();

        let entries1 = vec![BucketEntry::Liveentry(make_account_entry([1u8; 32], 100))];
        let entries2 = vec![BucketEntry::Liveentry(make_account_entry([2u8; 32], 200))];
        let entries3 = vec![BucketEntry::Liveentry(make_account_entry([3u8; 32], 300))];

        let bucket1 = manager.create_bucket(entries1).unwrap();
        let bucket2 = manager.create_bucket(entries2).unwrap();
        let _bucket3 = manager.create_bucket(entries3).unwrap();

        // Keep only bucket1 and bucket2
        let deleted = manager
            .retain_buckets(&[bucket1.hash(), bucket2.hash()])
            .unwrap();
        assert_eq!(deleted, 1);

        let buckets = manager.list_buckets().unwrap();
        assert_eq!(buckets.len(), 2);
    }

    #[test]
    fn test_import_export_bucket() {
        let (_temp_dir, manager) = create_manager();

        let entries = vec![
            BucketEntry::Liveentry(make_account_entry([1u8; 32], 100)),
            BucketEntry::Liveentry(make_account_entry([2u8; 32], 200)),
        ];

        let original = manager.create_bucket(entries).unwrap();
        let hash = original.hash();

        // Export
        let xdr_bytes = manager.export_bucket(&hash).unwrap();

        // Import (create new manager)
        let temp_dir2 = TempDir::new().unwrap();
        let manager2 = BucketManager::new(temp_dir2.path().to_path_buf()).unwrap();

        let imported = manager2.import_bucket(&xdr_bytes).unwrap();
        assert_eq!(imported.hash(), hash);
        assert_eq!(imported.len(), 2);
    }

    #[test]
    fn test_empty_bucket() {
        let (_temp_dir, manager) = create_manager();

        let bucket = manager.create_bucket(vec![]).unwrap();
        assert!(bucket.is_empty());
        assert_eq!(bucket.hash(), Hash256::ZERO);

        // Loading empty bucket should work
        let loaded = manager.load_bucket(&Hash256::ZERO).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn test_stats() {
        let (_temp_dir, manager) = create_manager();

        let entries = vec![BucketEntry::Liveentry(make_account_entry([1u8; 32], 100))];
        manager.create_bucket(entries).unwrap();

        let stats = manager.stats();
        assert_eq!(stats.cached_buckets, 1);
        assert_eq!(stats.disk_buckets, 1);
    }

    #[test]
    fn test_load_complete_ledger_state() {
        let (_temp_dir, manager) = create_manager();

        // Create two buckets - older has entry1, newer has entry2
        let bucket1 = manager
            .create_bucket(vec![BucketEntry::Liveentry(make_account_entry(
                [1u8; 32], 100,
            ))])
            .unwrap();
        let bucket2 = manager
            .create_bucket(vec![BucketEntry::Liveentry(make_account_entry(
                [2u8; 32], 200,
            ))])
            .unwrap();

        // Load complete state (bucket1 is older, bucket2 is newer)
        let entries = manager
            .load_complete_ledger_state(&[bucket1.hash(), bucket2.hash()])
            .unwrap();

        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn test_load_complete_ledger_state_with_updates() {
        let (_temp_dir, manager) = create_manager();

        // Create older bucket with initial entry
        let bucket1 = manager
            .create_bucket(vec![BucketEntry::Liveentry(make_account_entry(
                [1u8; 32], 100,
            ))])
            .unwrap();

        // Create newer bucket with updated entry
        let bucket2 = manager
            .create_bucket(vec![BucketEntry::Liveentry(make_account_entry(
                [1u8; 32], 500,
            ))])
            .unwrap();

        // Load complete state - newer entry should win
        let entries = manager
            .load_complete_ledger_state(&[bucket1.hash(), bucket2.hash()])
            .unwrap();

        assert_eq!(entries.len(), 1);
        if let LedgerEntryData::Account(account) = &entries[0].data {
            assert_eq!(account.balance, 500);
        } else {
            panic!("Expected Account entry");
        }
    }

    #[test]
    fn test_load_complete_ledger_state_with_deletes() {
        let (_temp_dir, manager) = create_manager();

        // Create older bucket with entry
        let bucket1 = manager
            .create_bucket(vec![BucketEntry::Liveentry(make_account_entry(
                [1u8; 32], 100,
            ))])
            .unwrap();

        // Create newer bucket with deletion
        let bucket2 = manager
            .create_bucket(vec![BucketEntry::Deadentry(make_account_key([1u8; 32]))])
            .unwrap();

        // Load complete state - entry should be deleted
        let entries = manager
            .load_complete_ledger_state(&[bucket1.hash(), bucket2.hash()])
            .unwrap();

        assert!(entries.is_empty());
    }

    #[test]
    fn test_merge_all_buckets() {
        let (_temp_dir, manager) = create_manager();

        // Create buckets with different entries
        let bucket1 = manager
            .create_bucket(vec![
                BucketEntry::Liveentry(make_account_entry([1u8; 32], 100)),
                BucketEntry::Liveentry(make_account_entry([2u8; 32], 200)),
            ])
            .unwrap();
        let bucket2 = manager
            .create_bucket(vec![
                BucketEntry::Liveentry(make_account_entry([3u8; 32], 300)),
                BucketEntry::Liveentry(make_account_entry([1u8; 32], 150)), // Update entry 1
            ])
            .unwrap();

        // Merge all buckets
        let merged = manager
            .merge_all_buckets(&[bucket1.hash(), bucket2.hash()], 25)
            .unwrap();

        // Should have 3 entries (entry 1 updated, entries 2 and 3)
        assert_eq!(merged.len(), 4); // 1 metadata + 3 entries

        // Check that entry 1 has the updated value
        let key1 = make_account_key([1u8; 32]);
        let entry1 = merged.get_entry(&key1).unwrap().unwrap();
        if let LedgerEntryData::Account(account) = &entry1.data {
            assert_eq!(account.balance, 150);
        } else {
            panic!("Expected Account entry");
        }
    }

    #[test]
    fn test_verify_buckets_exist() {
        let (_temp_dir, manager) = create_manager();

        let bucket = manager
            .create_bucket(vec![BucketEntry::Liveentry(make_account_entry(
                [1u8; 32], 100,
            ))])
            .unwrap();

        let nonexistent = Hash256::hash(b"does not exist");

        // Should find the nonexistent bucket as missing
        let missing = manager.verify_buckets_exist(&[bucket.hash(), nonexistent, Hash256::ZERO]);

        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0], nonexistent);
    }

    #[test]
    fn test_verify_bucket_hashes() {
        let (_temp_dir, manager) = create_manager();

        let bucket = manager
            .create_bucket(vec![BucketEntry::Liveentry(make_account_entry(
                [1u8; 32], 100,
            ))])
            .unwrap();

        // Verify hash matches
        let mismatches = manager.verify_bucket_hashes(&[bucket.hash()]).unwrap();
        assert!(mismatches.is_empty());
    }

    #[test]
    fn test_ensure_buckets_exist() {
        let (_temp_dir, manager) = create_manager();

        // Create a bucket in another manager
        let temp_dir2 = TempDir::new().unwrap();
        let manager2 = BucketManager::new(temp_dir2.path().to_path_buf()).unwrap();
        let bucket = manager2
            .create_bucket(vec![BucketEntry::Liveentry(make_account_entry(
                [1u8; 32], 100,
            ))])
            .unwrap();

        let bucket_xdr = manager2.export_bucket(&bucket.hash()).unwrap();

        // Ensure it exists in manager (needs to fetch)
        let fetched = manager
            .ensure_buckets_exist(&[bucket.hash()], |_hash| Ok(bucket_xdr.clone()))
            .unwrap();

        assert_eq!(fetched, 1);

        // Second call should fetch 0 (already exists)
        let fetched = manager
            .ensure_buckets_exist(&[bucket.hash()], |_hash| panic!("Should not be called"))
            .unwrap();
        assert_eq!(fetched, 0);
    }

    #[test]
    fn test_bucket_manager_persist_index() {
        use crate::index::{DiskIndex, DEFAULT_PAGE_SIZE};
        use crate::index_persistence::index_path_for_bucket;

        let temp_dir = TempDir::new().unwrap();
        let manager =
            BucketManager::with_persist_index(temp_dir.path().to_path_buf(), true).unwrap();
        assert!(manager.persist_index());

        // Create a bucket
        let entries: Vec<BucketEntry> = (0..100u8)
            .map(|i| BucketEntry::Liveentry(make_account_entry([i; 32], i as i64 * 100)))
            .collect();
        let bucket = manager.create_bucket(entries.clone()).unwrap();
        let hash = bucket.hash();
        let bucket_path = manager.bucket_path(&hash);

        // Build and save a disk index
        let indexed_entries: Vec<(crate::entry::BucketEntry, u64)> = entries
            .into_iter()
            .enumerate()
            .map(|(i, e)| (e, i as u64 * 100))
            .collect();
        let bloom_seed = [0u8; 16];
        let index =
            DiskIndex::from_entries(indexed_entries.into_iter(), bloom_seed, DEFAULT_PAGE_SIZE);
        manager.save_index_for_bucket(&hash, &index).unwrap();

        // Verify index file exists
        let index_path = index_path_for_bucket(&bucket_path);
        assert!(index_path.exists(), "Index file should be created");

        // Load the index back
        let loaded = manager
            .try_load_index_for_bucket(&hash, DEFAULT_PAGE_SIZE)
            .unwrap();
        assert!(loaded.is_some(), "Should load persisted index");

        let loaded_idx = loaded.unwrap();
        assert_eq!(loaded_idx.page_size(), DEFAULT_PAGE_SIZE);

        // Delete the bucket — index should also be removed
        manager.delete_bucket(&hash).unwrap();
        assert!(!bucket_path.exists(), "Bucket file should be deleted");
        assert!(!index_path.exists(), "Index file should also be deleted");
    }

    #[test]
    fn test_bucket_manager_no_persist_index() {
        let temp_dir = TempDir::new().unwrap();
        let manager = BucketManager::new(temp_dir.path().to_path_buf()).unwrap();
        assert!(!manager.persist_index());

        // save/load should be no-ops when persist_index is false
        let hash = Hash256::hash(b"dummy");
        let entries: Vec<(crate::entry::BucketEntry, u64)> = vec![];
        let index = crate::index::DiskIndex::from_entries(entries.into_iter(), [0u8; 16], 10);

        manager.save_index_for_bucket(&hash, &index).unwrap();
        let loaded = manager.try_load_index_for_bucket(&hash, 10).unwrap();
        assert!(loaded.is_none());
    }

    /// Regression test: Verify that cleanup_unreferenced_files only deletes
    /// merge temp files from PREVIOUS process runs, not from the current process.
    ///
    /// This prevents a race condition where an async merge output file could be
    /// deleted before it's registered in referenced_paths. The fix uses PID-based
    /// filtering: files with the current process's PID in their name are skipped.
    #[test]
    fn test_cleanup_merge_temps_respects_current_pid() {
        let temp_dir = TempDir::new().unwrap();
        let manager = BucketManager::new(temp_dir.path().to_path_buf()).unwrap();

        let current_pid = std::process::id();
        let other_pid = current_pid.wrapping_add(12345); // Different PID

        // Create temp files simulating:
        // 1. A file from the current process (should NOT be deleted)
        // 2. A file from a previous process (should be deleted if unreferenced)
        let current_process_file = temp_dir
            .path()
            .join(format!("merge-tmp-{}-1.xdr", current_pid));
        let previous_process_file = temp_dir
            .path()
            .join(format!("merge-tmp-{}-1.xdr", other_pid));

        std::fs::write(&current_process_file, b"current process data").unwrap();
        std::fs::write(&previous_process_file, b"previous process data").unwrap();

        // Empty referenced paths - nothing is "in use"
        let referenced = std::collections::HashSet::new();

        // Run cleanup
        let deleted = manager.cleanup_unreferenced_files(&referenced).unwrap();

        // The previous process file should be deleted (1 file deleted)
        assert_eq!(
            deleted, 1,
            "Should delete exactly one file (from previous process)"
        );

        // Current process file should still exist
        assert!(
            current_process_file.exists(),
            "File from current process should NOT be deleted (may be in-flight async merge)"
        );

        // Previous process file should be gone
        assert!(
            !previous_process_file.exists(),
            "File from previous process should be deleted"
        );
    }

    /// Regression test: Verify that cleanup_unreferenced_files preserves
    /// files that are explicitly referenced, regardless of PID.
    #[test]
    fn test_cleanup_merge_temps_preserves_referenced_files() {
        let temp_dir = TempDir::new().unwrap();
        let manager = BucketManager::new(temp_dir.path().to_path_buf()).unwrap();

        let other_pid = std::process::id().wrapping_add(99999);

        // Create temp files from a "previous process"
        let referenced_file = temp_dir
            .path()
            .join(format!("merge-tmp-{}-1.xdr", other_pid));
        let unreferenced_file = temp_dir
            .path()
            .join(format!("merge-tmp-{}-2.xdr", other_pid));

        std::fs::write(&referenced_file, b"referenced").unwrap();
        std::fs::write(&unreferenced_file, b"unreferenced").unwrap();

        // Mark one file as referenced
        let mut referenced = std::collections::HashSet::new();
        referenced.insert(referenced_file.clone());

        // Run cleanup
        let deleted = manager.cleanup_unreferenced_files(&referenced).unwrap();

        assert_eq!(deleted, 1, "Should delete exactly one unreferenced file");
        assert!(
            referenced_file.exists(),
            "Referenced file should be preserved"
        );
        assert!(
            !unreferenced_file.exists(),
            "Unreferenced file should be deleted"
        );
    }

    // ============ P2-1: Bucket Manager Ownership / GC Lifecycle ============
    //
    // stellar-core: BucketManagerTests.cpp "bucketmanager ownership"
    // Tests Arc reference counting and cleanup of bucket files.

    #[test]
    fn test_bucket_arc_reference_counting() {
        let (_temp_dir, manager) = create_manager();

        // Create a bucket - manager caches it
        let entries = vec![
            BucketEntry::Liveentry(make_account_entry([1u8; 32], 100)),
            BucketEntry::Liveentry(make_account_entry([2u8; 32], 200)),
        ];
        let b1 = manager.create_bucket(entries).unwrap();
        let hash = b1.hash();

        // b1 + cache = 2 strong references
        assert_eq!(Arc::strong_count(&b1), 2);

        // Create another reference
        let b2 = Arc::clone(&b1);
        assert_eq!(Arc::strong_count(&b1), 3);

        // Load from cache creates another reference
        let b3 = manager.load_bucket(&hash).unwrap();
        assert!(Arc::ptr_eq(&b1, &b3));
        assert_eq!(Arc::strong_count(&b1), 4);

        // Drop b2, b3 - count should decrease
        drop(b2);
        drop(b3);
        assert_eq!(Arc::strong_count(&b1), 2); // b1 + cache

        // Clear cache - only b1 remains
        manager.clear_cache();
        assert_eq!(Arc::strong_count(&b1), 1);
    }

    #[test]
    fn test_bucket_gc_lifecycle() {
        let (_temp_dir, manager) = create_manager();

        // Create 3 buckets
        let b1 = manager
            .create_bucket(vec![BucketEntry::Liveentry(make_account_entry(
                [1u8; 32], 100,
            ))])
            .unwrap();
        let b2 = manager
            .create_bucket(vec![BucketEntry::Liveentry(make_account_entry(
                [2u8; 32], 200,
            ))])
            .unwrap();
        let b3 = manager
            .create_bucket(vec![BucketEntry::Liveentry(make_account_entry(
                [3u8; 32], 300,
            ))])
            .unwrap();

        let h1 = b1.hash();
        let h2 = b2.hash();
        let h3 = b3.hash();

        // All 3 exist on disk
        assert_eq!(manager.list_buckets().unwrap().len(), 3);

        // Retain only b1 and b2
        let deleted = manager.retain_buckets(&[h1, h2]).unwrap();
        assert_eq!(deleted, 1);

        // b3 should be deleted from disk
        assert!(manager.bucket_exists(&h1));
        assert!(manager.bucket_exists(&h2));
        assert!(!manager.bucket_path(&h3).exists());

        // Retain only b1
        let deleted = manager.retain_buckets(&[h1]).unwrap();
        assert_eq!(deleted, 1);
        assert!(manager.bucket_exists(&h1));
        assert!(!manager.bucket_path(&h2).exists());
    }

    #[test]
    fn test_bucket_deduplication() {
        let (_temp_dir, manager) = create_manager();

        // Creating the same entries twice should return the same bucket (same hash)
        let entries1 = vec![BucketEntry::Liveentry(make_account_entry([1u8; 32], 100))];
        let entries2 = vec![BucketEntry::Liveentry(make_account_entry([1u8; 32], 100))];

        let b1 = manager.create_bucket(entries1).unwrap();
        let b2 = manager.create_bucket(entries2).unwrap();

        assert_eq!(b1.hash(), b2.hash());
        // Should point to the same cached instance
        assert!(Arc::ptr_eq(&b1, &b2));
    }

    // ============ P2-2: Missing Bucket Startup Failure ============
    //
    // stellar-core: BucketManagerTests.cpp "bucketmanager missing buckets fail"
    // Tests that verify_buckets_exist correctly detects missing bucket files.

    #[test]
    fn test_missing_bucket_detected() {
        let (_temp_dir, manager) = create_manager();

        let bucket = manager
            .create_bucket(vec![BucketEntry::Liveentry(make_account_entry(
                [1u8; 32], 100,
            ))])
            .unwrap();
        let hash = bucket.hash();

        // Delete the bucket file from disk and clear cache
        let path = manager.bucket_path(&hash);
        std::fs::remove_file(&path).unwrap();
        drop(bucket);
        manager.clear_cache();

        // verify_buckets_exist should detect the missing file
        let missing = manager.verify_buckets_exist(&[hash]);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0], hash);
    }

    #[test]
    fn test_load_missing_bucket_fails() {
        let (_temp_dir, manager) = create_manager();

        let bucket = manager
            .create_bucket(vec![BucketEntry::Liveentry(make_account_entry(
                [1u8; 32], 100,
            ))])
            .unwrap();
        let hash = bucket.hash();

        // Delete file and clear cache
        let path = manager.bucket_path(&hash);
        std::fs::remove_file(&path).unwrap();
        manager.clear_cache();

        // Loading should fail
        let result = manager.load_bucket(&hash);
        assert!(result.is_err(), "Loading missing bucket should fail");
    }

    // ============ P2-3: Merge Reattach to Finished Merge ============
    //
    // stellar-core: BucketManagerTests.cpp "bucketmanager reattach to finished merge"
    // Tests FutureBucket snapshot serialization roundtrip.

    #[test]
    fn test_merge_result_persists_on_disk() {
        let (_temp_dir, manager) = create_manager();

        // Create two buckets and merge them
        let old = manager
            .create_bucket(vec![BucketEntry::Liveentry(make_account_entry(
                [1u8; 32], 100,
            ))])
            .unwrap();
        let new = manager
            .create_bucket(vec![BucketEntry::Liveentry(make_account_entry(
                [2u8; 32], 200,
            ))])
            .unwrap();

        let merged = manager.merge(&old, &new, 25).unwrap();
        let merged_hash = merged.hash();

        // Clear cache - should still be loadable from disk
        manager.clear_cache();
        let reloaded = manager.load_bucket(&merged_hash).unwrap();
        assert_eq!(reloaded.hash(), merged_hash);
        assert_eq!(reloaded.len(), merged.len());
    }

    #[test]
    fn test_merge_replaces_corrupt_existing_canonical_file() {
        let (_temp_dir, manager) = create_manager();

        let old = manager
            .create_bucket(vec![BucketEntry::Liveentry(make_account_entry(
                [1u8; 32], 100,
            ))])
            .unwrap();
        let new = manager
            .create_bucket(vec![BucketEntry::Liveentry(make_account_entry(
                [2u8; 32], 200,
            ))])
            .unwrap();

        // First merge creates a valid canonical file.
        let first = manager.merge(&old, &new, 25).unwrap();
        let expected_hash = first.hash();
        let canonical_path = manager.bucket_path(&expected_hash);
        assert!(canonical_path.exists());

        // Corrupt the canonical file to simulate prior partial write.
        std::fs::write(&canonical_path, b"").unwrap();
        assert_eq!(std::fs::metadata(&canonical_path).unwrap().len(), 0);

        // A second merge with the same inputs should replace corrupt file.
        manager.clear_cache();
        let second = manager.merge(&old, &new, 25).unwrap();
        assert_eq!(second.hash(), expected_hash);

        manager.clear_cache();
        let reloaded = manager.load_bucket(&expected_hash).unwrap();
        assert_eq!(reloaded.hash(), expected_hash);
        assert!(reloaded.len() > 0);
    }

    #[test]
    fn test_merge_map_records_and_retrieves() {
        let (_temp_dir, manager) = create_manager();

        let old = manager
            .create_bucket(vec![BucketEntry::Liveentry(make_account_entry(
                [1u8; 32], 100,
            ))])
            .unwrap();
        let new = manager
            .create_bucket(vec![BucketEntry::Liveentry(make_account_entry(
                [2u8; 32], 200,
            ))])
            .unwrap();
        let old_hash = old.hash();
        let new_hash = new.hash();

        // Merge the buckets
        let result = manager.merge(&old, &new, 25).unwrap();
        let result_hash = result.hash();

        // Record the merge in the merge map
        let merge_key = MergeKey::new(DeadEntryPolicy::Keep, old_hash, new_hash);
        manager
            .finished_merges()
            .write()
            .unwrap()
            .record_merge(merge_key.clone(), result_hash);

        // Verify the merge map returns the output
        let output = manager
            .finished_merges()
            .read()
            .unwrap()
            .get_output(&merge_key)
            .copied();
        assert_eq!(output, Some(result_hash));
    }

    #[test]
    fn test_retain_buckets_cleans_merge_map() {
        let (_temp_dir, manager) = create_manager();

        let old = manager
            .create_bucket(vec![BucketEntry::Liveentry(make_account_entry(
                [1u8; 32], 100,
            ))])
            .unwrap();
        let new = manager
            .create_bucket(vec![BucketEntry::Liveentry(make_account_entry(
                [2u8; 32], 200,
            ))])
            .unwrap();
        let result = manager.merge(&old, &new, 25).unwrap();

        let merge_key = MergeKey::new(DeadEntryPolicy::Keep, old.hash(), new.hash());
        manager
            .finished_merges()
            .write()
            .unwrap()
            .record_merge(merge_key.clone(), result.hash());

        // Retain only the input buckets (not the merged output)
        manager.retain_buckets(&[old.hash(), new.hash()]).unwrap();

        // Merge map entry should be cleaned up since the output was deleted
        let output = manager
            .finished_merges()
            .read()
            .unwrap()
            .get_output(&merge_key)
            .copied();
        assert_eq!(output, None);
    }
}
