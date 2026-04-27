//! Hot Archive Bucket List for recently evicted Soroban entries.
//!
//! The HotArchiveBucketList stores recently evicted persistent Soroban entries.
//! When persistent entries (ContractData with persistent durability or ContractCode)
//! expire, they are moved from the live BucketList to the HotArchiveBucketList.
//!
//! # Entry Types
//!
//! Hot archive buckets contain `HotArchiveBucketEntry` which has three variants:
//!
//! - `Metaentry`: Metadata about the bucket (protocol version)
//! - `Archived`: A recently archived persistent entry (full LedgerEntry)
//! - `Live`: A restored entry marker (LedgerKey only, indicates the entry was restored)
//!
//! # Merge Semantics
//!
//! Hot archive bucket merging follows these rules:
//!
//! - `Archived + Live` = Annihilate (entry was restored, remove from archive)
//! - `Live + Archived` = Keep `Archived` (re-archived after restoration)
//! - `Archived + Archived` = Keep newer `Archived`
//! - At level 10: `Live` entries are dropped (tombstones not needed)
//!
//! # Protocol Support
//!
//! Hot archive is only supported from Protocol 23+ (Soroban state archival).

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use stellar_xdr::curr::{
    BucketListType, BucketMetadata, BucketMetadataExt, HotArchiveBucketEntry, LedgerEntry,
    LedgerKey, Limits, ReadXdr, WriteXdr,
};

use henyey_common::Hash256;

use crate::bucket_list::{
    bl_keep_tombstone_entries, bl_level_half, bl_level_should_spill, bl_round_down,
    bl_should_merge_with_empty_curr, BucketListStats, HasNextState, HAS_NEXT_STATE_INPUTS,
    HAS_NEXT_STATE_OUTPUT,
};
use crate::{BucketError, Result};
use henyey_common::protocol::{
    protocol_version_is_before, protocol_version_starts_from, ProtocolVersion,
};

/// Number of levels in the HotArchiveBucketList (same as live bucket list).
pub const HOT_ARCHIVE_BUCKET_LIST_LEVELS: usize = 11;

/// Storage backend for a hot archive bucket.
#[derive(Clone, Debug)]
enum HotArchiveStorage {
    /// All entries in memory.
    InMemory {
        /// Entries indexed by key (for lookups).
        entries: BTreeMap<Vec<u8>, HotArchiveBucketEntry>,
        /// Entries in original/sorted order (for hash computation).
        /// This preserves stellar-core's entry order which uses semantic comparison,
        /// not XDR byte comparison.
        ordered_entries: Vec<HotArchiveBucketEntry>,
    },
    /// Entries stored on disk in uncompressed XDR format.
    DiskBacked {
        /// Path to the uncompressed `.bucket.xdr` file.
        path: PathBuf,
        /// Index mapping key XDR bytes → file offset of the record mark.
        /// Wrapped in OnceLock for thread-safe initialization.
        index: OnceLock<BTreeMap<Vec<u8>, u64>>,
        /// Number of entries in the bucket (including metadata).
        entry_count: usize,
    },
}

/// A hot archive bucket containing archived persistent Soroban entries.
#[derive(Clone, Debug)]
pub struct HotArchiveBucket {
    /// Storage backend.
    storage: HotArchiveStorage,
    /// Hash of the bucket contents.
    hash: Hash256,
}

impl HotArchiveBucket {
    /// Build the BTreeMap index for a disk-backed bucket by streaming through the file.
    ///
    /// Ensure the disk-backed index is built, building it if needed.
    ///
    /// Returns a reference to the index. For non-DiskBacked storage, panics.
    fn ensure_index(&self) -> Result<&BTreeMap<Vec<u8>, u64>> {
        match &self.storage {
            HotArchiveStorage::DiskBacked { index, .. } => index.get().ok_or_else(|| {
                BucketError::Serialization(
                    "disk-backed hot archive index was not initialized".into(),
                )
            }),
            _ => Err(BucketError::Serialization(
                "ensure_index called on non-DiskBacked storage".into(),
            )),
        }
    }

    /// Create an empty hot archive bucket.
    pub fn empty() -> Self {
        Self {
            storage: HotArchiveStorage::InMemory {
                entries: BTreeMap::new(),
                ordered_entries: Vec::new(),
            },
            hash: Hash256::from_bytes([0u8; 32]),
        }
    }

    /// Create a hot archive bucket from entries.
    ///
    /// **Important**: The entries MUST be pre-sorted in stellar-core order
    /// (using LedgerEntryIdCmp comparison). The entries are stored in the
    /// provided order for hash computation.
    pub fn from_entries(entries: Vec<HotArchiveBucketEntry>) -> Result<Self> {
        let mut entry_map = BTreeMap::new();

        for entry in &entries {
            let key = hot_archive_entry_to_key(entry)?;
            entry_map.insert(key, entry.clone());
        }

        let mut bucket = Self {
            storage: HotArchiveStorage::InMemory {
                entries: entry_map,
                ordered_entries: entries,
            },
            hash: Hash256::from_bytes([0u8; 32]),
        };

        bucket.hash = bucket.compute_hash()?;
        Ok(bucket)
    }

    /// Create a fresh hot archive bucket from archived and restored entries.
    ///
    /// This is the primary way to create a new hot archive bucket when entries
    /// are evicted from the live bucket list.
    ///
    /// Even when there are no data entries, a metaentry-only bucket is created
    /// (matching stellar-core behavior where BucketOutputIterator always writes metaentry first).
    pub fn fresh(
        protocol_version: u32,
        archived_entries: Vec<LedgerEntry>,
        restored_keys: Vec<LedgerKey>,
    ) -> Result<Self> {
        // In stellar-core, BucketOutputIterator constructor always writes a metaentry first,
        // so fresh buckets always have at least a metaentry (even with no data entries).
        // mObjectsPut is 1 after writing metaentry, so getBucket() creates a bucket file.
        let mut entries = Vec::with_capacity(1 + archived_entries.len() + restored_keys.len());

        // Add metadata - hot archive buckets always use V1 with BucketListType::HotArchive
        // Metadata is only included when there are actual entries to add
        entries.push(HotArchiveBucketEntry::Metaentry(BucketMetadata {
            ledger_version: protocol_version,
            ext: BucketMetadataExt::V1(BucketListType::HotArchive),
        }));

        // Add archived entries
        for entry in archived_entries {
            entries.push(HotArchiveBucketEntry::Archived(entry));
        }

        // Add restored keys (Live markers)
        for key in restored_keys {
            entries.push(HotArchiveBucketEntry::Live(key));
        }

        // Sort entries using stellar-core's comparison order
        // This matches BucketEntryIdCmp<HotArchiveBucket>
        entries.sort_by(compare_hot_archive_entries);

        let bucket = Self::from_entries(entries)?;

        Ok(bucket)
    }

    /// Get the hash of this bucket.
    pub fn hash(&self) -> Hash256 {
        self.hash
    }

    /// Check if the bucket is empty.
    pub fn is_empty(&self) -> bool {
        match &self.storage {
            HotArchiveStorage::InMemory { entries, .. } => entries.is_empty(),
            HotArchiveStorage::DiskBacked { entry_count, .. } => *entry_count == 0,
        }
    }

    /// Get the number of entries in the bucket.
    pub fn len(&self) -> usize {
        match &self.storage {
            HotArchiveStorage::InMemory { entries, .. } => entries.len(),
            HotArchiveStorage::DiskBacked { entry_count, .. } => *entry_count,
        }
    }

    /// Get the backing file path for disk-backed buckets.
    ///
    /// Returns `Some(path)` for disk-backed buckets, `None` for in-memory buckets.
    /// This is used for garbage collection - tracking which files are still referenced.
    pub fn backing_file_path(&self) -> Option<&Path> {
        match &self.storage {
            HotArchiveStorage::DiskBacked { path, .. } => Some(path.as_path()),
            HotArchiveStorage::InMemory { .. } => None,
        }
    }

    /// Get the protocol version (bucket version) from the metadata entry.
    ///
    /// This matches stellar-core's `getBucketVersion()` method.
    /// Returns the ledger_version from the bucket's metadata entry, or 0 if no metadata.
    pub fn get_protocol_version(&self) -> Result<u32> {
        match &self.storage {
            HotArchiveStorage::InMemory { entries, .. } => {
                // Metadata entry is stored with empty key
                if let Some(HotArchiveBucketEntry::Metaentry(meta)) = entries.get(&Vec::new()) {
                    return Ok(meta.ledger_version);
                }
                Ok(0)
            }
            HotArchiveStorage::DiskBacked { path, .. } => {
                // Try to read metadata entry (stored at offset for empty key)
                let index = self.ensure_index()?;
                if let Some(&offset) = index.get(&Vec::new()) {
                    match Self::read_entry_at_offset(path, offset)? {
                        HotArchiveBucketEntry::Metaentry(meta) => return Ok(meta.ledger_version),
                        _ => {
                            return Err(BucketError::Serialization(format!(
                                "hot archive metadata offset {} did not contain metadata",
                                offset
                            )))
                        }
                    }
                }
                Ok(0)
            }
        }
    }

    /// Look up an entry by key.
    pub fn get(&self, key: &LedgerKey) -> Result<Option<HotArchiveBucketEntry>> {
        let key_bytes = key.to_xdr(Limits::none()).map_err(|e| {
            BucketError::Serialization(format!("failed to serialize ledger key: {}", e))
        })?;
        self.get_by_bytes(&key_bytes)
    }

    /// Look up an entry by pre-serialized key bytes.
    ///
    /// This avoids redundant XDR serialization when the caller already has the
    /// serialized key (e.g., when searching across multiple buckets).
    pub fn get_by_bytes(&self, key_bytes: &[u8]) -> Result<Option<HotArchiveBucketEntry>> {
        match &self.storage {
            HotArchiveStorage::InMemory { entries, .. } => Ok(entries.get(key_bytes).cloned()),
            HotArchiveStorage::DiskBacked { path, .. } => {
                let index = self.ensure_index()?;
                if let Some(&offset) = index.get(key_bytes) {
                    let entry = Self::read_entry_at_offset(path, offset)?;
                    Ok(Some(entry))
                } else {
                    Ok(None)
                }
            }
        }
    }

    /// Iterate over all entries in sorted order.
    ///
    /// For InMemory buckets, iterates over the BTreeMap values.
    /// For DiskBacked buckets, streams entries from the file sequentially.
    pub fn iter(&self) -> crate::Result<HotArchiveIter<'_>> {
        self.try_iter()
    }

    /// Like [`iter`](Self::iter) but returns an error if the backing file
    /// cannot be opened, instead of silently returning an empty iterator.
    pub fn try_iter(&self) -> crate::Result<HotArchiveIter<'_>> {
        match &self.storage {
            HotArchiveStorage::InMemory {
                ordered_entries, ..
            } => Ok(HotArchiveIter::InMemory {
                inner: ordered_entries.iter(),
            }),
            HotArchiveStorage::DiskBacked { path, .. } => {
                let file = std::fs::File::open(path).map_err(|e| {
                    crate::BucketError::Io(std::io::Error::new(
                        e.kind(),
                        format!(
                            "Failed to open hot archive bucket {}: {}",
                            path.display(),
                            e
                        ),
                    ))
                })?;
                let file_len = file.metadata()?.len();
                Ok(HotArchiveIter::DiskBacked {
                    records: crate::record::RecordMarkedReader::new(BufReader::new(file), file_len),
                    failed: false,
                })
            }
        }
    }

    /// Compute the hash of the bucket contents.
    ///
    /// This must match stellar-core's bucket hashing:
    /// Each entry is written with an XDR record mark (4-byte size prefix with high bit set),
    /// and the hash is computed over the entire serialized content including record marks.
    ///
    /// **Important**: We iterate over `ordered_entries` which preserves the original
    /// entry order from the file or from fresh() sorting. This is critical because
    /// stellar-core uses semantic comparison (LedgerEntryIdCmp) which differs from XDR byte order.
    fn compute_hash(&self) -> Result<Hash256> {
        match &self.storage {
            HotArchiveStorage::InMemory {
                ordered_entries, ..
            } => {
                if ordered_entries.is_empty() {
                    return Ok(Hash256::from_bytes([0u8; 32]));
                }

                let mut hasher = Sha256::new();
                for entry in ordered_entries {
                    let bytes = entry.to_xdr(Limits::none()).map_err(|e| {
                        BucketError::Serialization(format!("failed to serialize entry: {}", e))
                    })?;

                    // Write XDR record mark: 4-byte size (big-endian) with high bit set
                    let sz = bytes.len() as u32;
                    let record_mark: [u8; 4] = [
                        ((sz >> 24) & 0xFF) as u8 | 0x80,
                        ((sz >> 16) & 0xFF) as u8,
                        ((sz >> 8) & 0xFF) as u8,
                        (sz & 0xFF) as u8,
                    ];
                    hasher.update(record_mark);
                    hasher.update(&bytes);
                }

                Ok(Hash256::from_sha256(hasher))
            }
            HotArchiveStorage::DiskBacked { .. } => {
                // For disk-backed, hash was computed during construction
                Ok(self.hash)
            }
        }
    }

    /// Read a single entry from the XDR file at a given offset.
    fn read_entry_at_offset(path: &Path, offset: u64) -> Result<HotArchiveBucketEntry> {
        let file = std::fs::File::open(path)?;
        let file_len = file.metadata()?.len();
        let mut reader = BufReader::new(file);
        reader.seek(SeekFrom::Start(offset))?;
        let mut records = crate::record::RecordMarkedReader::new_at(reader, file_len, offset);
        let record = records.next_record()?.ok_or_else(|| {
            BucketError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("no hot archive record at offset {}", offset),
            ))
        })?;

        HotArchiveBucketEntry::from_xdr(record.body, Limits::none()).map_err(|e| {
            BucketError::Serialization(format!(
                "failed to parse hot archive entry at offset {}: {}",
                record.offset, e
            ))
        })
    }

    /// Load a hot archive bucket from a gzipped XDR file.
    ///
    /// This parses a bucket file containing `HotArchiveBucketEntry` values.
    pub fn load_from_file(path: impl AsRef<std::path::Path>) -> Result<Self> {
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

    /// Create a hot archive bucket from uncompressed XDR bytes.
    ///
    /// Parses bucket files using XDR Record Marking Standard (RFC 5531).
    /// Preserves the original file order for hash computation.
    pub fn from_xdr_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() {
            return Ok(Self::empty());
        }

        let mut entries = BTreeMap::new();
        let mut ordered_entries = Vec::new();
        let mut records = crate::record::RecordMarkedSliceIter::new(bytes);

        while let Some(record) = records.next_record()? {
            match HotArchiveBucketEntry::from_xdr(record.body, Limits::none()) {
                Ok(entry) => {
                    let key = hot_archive_entry_to_key(&entry)?;
                    entries.insert(key, entry.clone());
                    ordered_entries.push(entry);
                }
                Err(e) => {
                    return Err(BucketError::Serialization(format!(
                        "Failed to parse hot archive bucket entry at offset {}: {}",
                        record.offset, e
                    )));
                }
            }
        }

        // Compute hash from raw bytes (including record marks)
        let hash = Hash256::hash(bytes);

        Ok(Self {
            storage: HotArchiveStorage::InMemory {
                entries,
                ordered_entries,
            },
            hash,
        })
    }

    /// Load a hot archive bucket from an uncompressed XDR file.
    ///
    /// This reads the bucket file directly without any decompression.
    pub fn load_from_xdr_file(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let path = path.as_ref();
        let bytes = std::fs::read(path)?;
        Self::from_xdr_bytes(&bytes)
    }

    /// Serialize this bucket's entries to uncompressed XDR bytes with record marks.
    pub fn to_xdr_bytes(&self) -> Result<Vec<u8>> {
        match &self.storage {
            HotArchiveStorage::InMemory {
                ordered_entries, ..
            } => {
                let mut bytes = Vec::new();
                for entry in ordered_entries {
                    let entry_bytes = entry.to_xdr(Limits::none()).map_err(|e| {
                        BucketError::Serialization(format!("failed to serialize entry: {}", e))
                    })?;
                    let sz = entry_bytes.len() as u32;
                    let record_mark = sz | crate::XDR_RECORD_MARK;
                    bytes.extend_from_slice(&record_mark.to_be_bytes());
                    bytes.extend_from_slice(&entry_bytes);
                }
                Ok(bytes)
            }
            HotArchiveStorage::DiskBacked { path, .. } => {
                // Read the entire file
                std::fs::read(path).map_err(|e| e.into())
            }
        }
    }

    /// Save this bucket to an uncompressed XDR file.
    ///
    /// For InMemory buckets, serializes entries with record marks.
    /// For DiskBacked buckets, copies the existing file.
    /// The resulting file will hash to `self.hash()`.
    pub fn save_to_xdr_file(&self, path: impl AsRef<Path>) -> Result<PathBuf> {
        let path = path.as_ref().to_path_buf();

        match &self.storage {
            HotArchiveStorage::InMemory { .. } => {
                let bytes = self.to_xdr_bytes()?;
                let mut file = std::fs::File::create(&path)?;
                use std::io::Write;
                file.write_all(&bytes)?;
                file.sync_all()?;
            }
            HotArchiveStorage::DiskBacked {
                path: source_path, ..
            } => {
                if source_path != &path {
                    std::fs::copy(source_path, &path)?;
                }
            }
        }

        Ok(path)
    }

    /// Create a disk-backed hot archive bucket from an existing uncompressed XDR file.
    ///
    /// Streams through the file to build an index without loading all entries into memory.
    /// Hash is computed during the streaming pass. The index is built eagerly.
    pub fn from_xdr_file_disk_backed(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = std::fs::File::open(&path)?;
        let file_len = file.metadata()?.len();
        let mut records = crate::record::RecordMarkedReader::new(BufReader::new(file), file_len);

        let mut built_index = BTreeMap::new();
        let mut hasher = Sha256::new();
        let mut entry_count = 0;

        while let Some(record) = records.next_record()? {
            // Hash: include record mark + data
            hasher.update(record.mark_bytes);
            hasher.update(record.body);

            // Parse entry to extract key for index
            let entry =
                HotArchiveBucketEntry::from_xdr(record.body, Limits::none()).map_err(|e| {
                    BucketError::Serialization(format!(
                        "failed to parse hot archive entry at offset {}: {}",
                        record.offset, e
                    ))
                })?;

            let key = hot_archive_entry_to_key(&entry)?;
            built_index.insert(key, record.offset);
            entry_count += 1;
        }

        let hash = if entry_count == 0 {
            Hash256::from_bytes([0u8; 32])
        } else {
            Hash256::from_sha256(hasher)
        };

        let index = OnceLock::new();
        let _ = index.set(built_index);

        Ok(Self {
            storage: HotArchiveStorage::DiskBacked {
                path,
                index,
                entry_count,
            },
            hash,
        })
    }

    /// Estimate heap bytes used by this bucket.
    pub fn estimate_heap_bytes(&self) -> usize {
        match &self.storage {
            HotArchiveStorage::InMemory {
                entries,
                ordered_entries,
            } => {
                // BTreeMap: key Vec<u8> heap + entry overhead
                let key_data: usize = entries.keys().map(|k| k.len()).sum();
                let entry_overhead =
                    entries.len() * (std::mem::size_of::<HotArchiveBucketEntry>() + 64);
                // ordered_entries Vec
                let ordered =
                    ordered_entries.capacity() * std::mem::size_of::<HotArchiveBucketEntry>();
                key_data + entry_overhead + ordered
            }
            HotArchiveStorage::DiskBacked { index, .. } => {
                // Index BTreeMap<Vec<u8>, u64> if initialized
                if let Some(idx) = index.get() {
                    let key_data: usize = idx.keys().map(|k| k.len()).sum();
                    idx.len() * (8 + 64) + key_data
                } else {
                    0
                }
            }
        }
    }

    /// Return mmap'd / disk-backed file size in bytes.
    pub fn mmap_bytes(&self) -> usize {
        match &self.storage {
            HotArchiveStorage::InMemory { .. } => 0,
            HotArchiveStorage::DiskBacked { path, .. } => std::fs::metadata(path)
                .map(|m| m.len() as usize)
                .unwrap_or(0),
        }
    }
}

/// Iterator over hot archive bucket entries.
///
/// Supports both in-memory (reference) and disk-backed (streaming) iteration.
/// Returns owned entries in both cases for a uniform interface.
pub enum HotArchiveIter<'a> {
    /// In-memory iteration over ordered entries (preserves hash-consistent order).
    InMemory {
        inner: std::slice::Iter<'a, HotArchiveBucketEntry>,
    },
    /// Disk-backed streaming iteration.
    DiskBacked {
        records: crate::record::RecordMarkedReader<BufReader<std::fs::File>>,
        failed: bool,
    },
}

impl Iterator for HotArchiveIter<'_> {
    type Item = Result<HotArchiveBucketEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            HotArchiveIter::InMemory { inner } => inner.next().cloned().map(Ok),
            HotArchiveIter::DiskBacked { records, failed } => {
                if *failed {
                    return None;
                }
                let record = match records.next_record() {
                    Ok(Some(record)) => record,
                    Ok(None) => return None,
                    Err(e) => {
                        *failed = true;
                        return Some(Err(e));
                    }
                };
                match HotArchiveBucketEntry::from_xdr(record.body, Limits::none()) {
                    Ok(entry) => Some(Ok(entry)),
                    Err(e) => {
                        *failed = true;
                        Some(Err(BucketError::Serialization(format!(
                            "failed to parse hot archive entry at offset {}: {}",
                            record.offset, e
                        ))))
                    }
                }
            }
        }
    }
}

impl Default for HotArchiveBucket {
    fn default() -> Self {
        Self::empty()
    }
}

/// A single level in the HotArchiveBucketList.
#[derive(Clone, Debug)]
pub struct HotArchiveBucketLevel {
    /// The current bucket.
    pub(crate) curr: Arc<HotArchiveBucket>,
    /// The snapshot bucket.
    pub(crate) snap: Arc<HotArchiveBucket>,
    /// Staged merge result.
    next: Option<HotArchiveBucket>,
    /// Level number (stored for debugging).
    _level: usize,
}

impl HotArchiveBucketLevel {
    /// Create a new empty level.
    pub fn new(level: usize) -> Self {
        Self {
            curr: Arc::new(HotArchiveBucket::empty()),
            snap: Arc::new(HotArchiveBucket::empty()),
            next: None,
            _level: level,
        }
    }

    /// Get a reference to the current bucket.
    pub fn curr(&self) -> &HotArchiveBucket {
        &self.curr
    }

    /// Get an Arc clone of the current bucket (cheap).
    pub fn curr_arc(&self) -> Arc<HotArchiveBucket> {
        Arc::clone(&self.curr)
    }

    /// Get a reference to the snapshot bucket.
    pub fn snap_bucket(&self) -> &HotArchiveBucket {
        &self.snap
    }

    /// Get an Arc clone of the snapshot bucket (cheap).
    pub fn snap_arc(&self) -> Arc<HotArchiveBucket> {
        Arc::clone(&self.snap)
    }

    /// Get the hash of this level.
    pub fn hash(&self) -> Hash256 {
        let curr_hash = self.curr.hash();
        let snap_hash = self.snap.hash();

        let mut hasher = Sha256::new();
        hasher.update(curr_hash.as_bytes());
        hasher.update(snap_hash.as_bytes());
        Hash256::from_sha256(hasher)
    }

    /// Get reference to the staged next bucket.
    pub fn next(&self) -> Option<&HotArchiveBucket> {
        self.next.as_ref()
    }

    /// Get the hash of the pending merge output, if any.
    ///
    /// Hot archive merges are always synchronous, so if there is a pending
    /// merge it is always resolved. Returns the output hash for HAS
    /// serialization (state=1).
    pub fn pending_merge_output_hash(&self) -> Option<Hash256> {
        self.next.as_ref().and_then(|bucket| {
            let h = bucket.hash();
            if h.is_zero() {
                None
            } else {
                Some(h)
            }
        })
    }

    /// Commit the staged merge.
    fn commit(&mut self) {
        if let Some(next) = self.next.take() {
            self.curr = Arc::new(next);
        }
    }

    /// Snap curr to snap and return the new snap (matches stellar-core BucketLevel::snap).
    fn snap(&mut self) -> Arc<HotArchiveBucket> {
        self.snap = std::mem::replace(&mut self.curr, Arc::new(HotArchiveBucket::empty()));
        Arc::clone(&self.snap)
    }

    /// Prepare a merge with an incoming bucket.
    ///
    /// - `use_empty_curr`: If true, use an empty bucket instead of self.curr for the merge.
    ///   This is used when the level is about to snap its curr (shouldMergeWithEmptyCurr).
    fn prepare(
        &mut self,
        protocol_version: u32,
        incoming: &HotArchiveBucket,
        keep_tombstones: bool,
        use_empty_curr: bool,
    ) -> Result<()> {
        if self.next.is_some() {
            return Err(BucketError::Merge(
                "hot archive bucket merge already in progress".to_string(),
            ));
        }

        // Choose curr or empty based on shouldMergeWithEmptyCurr
        let empty;
        let curr_for_merge: &HotArchiveBucket = if use_empty_curr {
            empty = HotArchiveBucket::empty();
            &empty
        } else {
            &self.curr
        };

        let merged =
            merge_hot_archive_buckets(curr_for_merge, incoming, protocol_version, keep_tombstones)?;
        self.next = Some(merged);
        Ok(())
    }
}

/// The complete HotArchiveBucketList.
#[derive(Clone)]
pub struct HotArchiveBucketList {
    /// The 11 levels.
    levels: Vec<HotArchiveBucketLevel>,
    /// Current ledger sequence.
    ledger_seq: u32,
}

/// Load a hot archive bucket by hash via `FnMut` closure and verify the returned hash matches.
fn load_hot_and_verify<F>(hash: &Hash256, load_bucket: &mut F) -> Result<HotArchiveBucket>
where
    F: FnMut(&Hash256) -> Result<HotArchiveBucket>,
{
    let bucket = load_bucket(hash)?;
    if bucket.hash() != *hash {
        return Err(BucketError::HashMismatch {
            expected: hash.to_hex(),
            actual: bucket.hash().to_hex(),
        });
    }
    Ok(bucket)
}

/// Load a hot archive bucket by hash via shared `Fn` closure and verify the returned hash matches.
fn load_hot_and_verify_shared<F>(hash: &Hash256, load_bucket: &F) -> Result<HotArchiveBucket>
where
    F: Fn(&Hash256) -> Result<HotArchiveBucket>,
{
    let bucket = load_bucket(hash)?;
    if bucket.hash() != *hash {
        return Err(BucketError::HashMismatch {
            expected: hash.to_hex(),
            actual: bucket.hash().to_hex(),
        });
    }
    Ok(bucket)
}

impl HotArchiveBucketList {
    /// Number of levels in the hot archive bucket list.
    pub const NUM_LEVELS: usize = HOT_ARCHIVE_BUCKET_LIST_LEVELS;

    /// Create a new empty HotArchiveBucketList.
    pub fn new() -> Self {
        let levels = (0..HOT_ARCHIVE_BUCKET_LIST_LEVELS)
            .map(HotArchiveBucketLevel::new)
            .collect();

        Self {
            levels,
            ledger_seq: 0,
        }
    }

    /// Get the hash of the entire bucket list.
    pub fn hash(&self) -> Hash256 {
        let mut hasher = Sha256::new();
        for level in self.levels.iter() {
            let level_hash = level.hash();
            hasher.update(level_hash.as_bytes());
        }

        Hash256::from_sha256(hasher)
    }

    /// Get the current ledger sequence.
    pub fn ledger_seq(&self) -> u32 {
        self.ledger_seq
    }

    /// Set the ledger sequence.
    ///
    /// This is used after restoring a bucket list from history archive hashes
    /// to set the correct ledger sequence. The `restore_from_hashes` method
    /// sets ledger_seq to 0, so callers must set it to the actual ledger
    /// sequence to ensure proper bucket list advancement behavior.
    pub fn set_ledger_seq(&mut self, ledger_seq: u32) {
        self.ledger_seq = ledger_seq;
    }

    /// Get a reference to all levels.
    pub fn levels(&self) -> &[HotArchiveBucketLevel] {
        &self.levels
    }

    /// Get the hash of each level along with curr and snap bucket hashes.
    ///
    /// Returns an iterator of (level_index, level_hash, curr_hash, snap_hash).
    /// This is useful for debugging bucket list hash mismatches.
    pub fn level_hashes(&self) -> impl Iterator<Item = (usize, Hash256, Hash256, Hash256)> + '_ {
        self.levels
            .iter()
            .enumerate()
            .map(|(idx, level)| (idx, level.hash(), level.curr.hash(), level.snap.hash()))
    }

    /// Get all file paths referenced by disk-backed buckets in this bucket list.
    ///
    /// This includes:
    /// - curr and snap buckets for all levels
    /// - pending merge outputs (next) that are disk-backed
    ///
    /// This is used for garbage collection - any files in the bucket directory
    /// that aren't in this set can be safely deleted.
    pub fn referenced_file_paths(&self) -> std::collections::HashSet<PathBuf> {
        let mut paths = std::collections::HashSet::new();

        for level in &self.levels {
            // Add curr bucket's backing file if disk-backed
            if let Some(path) = level.curr.backing_file_path() {
                paths.insert(path.to_path_buf());
            }
            // Add snap bucket's backing file if disk-backed
            if let Some(path) = level.snap.backing_file_path() {
                paths.insert(path.to_path_buf());
            }
            // Add pending merge output's backing file if disk-backed
            if let Some(ref pending) = level.next {
                if let Some(path) = pending.backing_file_path() {
                    paths.insert(path.to_path_buf());
                }
            }
        }

        paths
    }

    /// Get all bucket hashes in the hot archive bucket list.
    ///
    /// Returns hashes for curr and snap buckets at every level.
    /// Used for garbage collection to identify which bucket files are still referenced.
    pub fn all_bucket_hashes(&self) -> Vec<Hash256> {
        let mut hashes = Vec::with_capacity(self.levels.len() * 2);
        for level in &self.levels {
            hashes.push(level.curr.hash());
            hashes.push(level.snap.hash());
        }
        hashes
    }

    /// Get all bucket hashes referenced by the hot archive bucket list, including
    /// pending merge outputs. Used for garbage collection.
    pub fn all_referenced_hashes(&self) -> Vec<Hash256> {
        let mut hashes = Vec::with_capacity(self.levels.len() * 3);
        for level in &self.levels {
            hashes.push(level.curr.hash());
            hashes.push(level.snap.hash());
            if let Some(ref next) = level.next {
                hashes.push(next.hash());
            }
        }
        hashes
    }

    /// Get a reference to a level.
    pub fn level(&self, idx: usize) -> Option<&HotArchiveBucketLevel> {
        self.levels.get(idx)
    }

    /// Get a mutable reference to a level.
    pub fn level_mut(&mut self, idx: usize) -> Option<&mut HotArchiveBucketLevel> {
        self.levels.get_mut(idx)
    }

    /// Add a batch of archived and restored entries.
    ///
    /// This is called when entries are evicted from the live bucket list.
    ///
    /// - `archived_entries`: Persistent entries that have expired and are being archived
    /// - `restored_keys`: Keys of entries that were restored (previously archived)
    pub fn add_batch(
        &mut self,
        ledger_seq: u32,
        protocol_version: u32,
        archived_entries: Vec<LedgerEntry>,
        restored_keys: Vec<LedgerKey>,
    ) -> Result<()> {
        if protocol_version_is_before(protocol_version, ProtocolVersion::V23) {
            return Err(BucketError::Merge(format!(
                "hot archive not supported before protocol {}",
                ProtocolVersion::V23.as_u32()
            )));
        }

        // In stellar-core, HotArchiveBucket::fresh() always creates a bucket with a metaentry,
        // even when there are no data entries. The metaentry is written in the
        // BucketOutputIterator constructor before any put() calls. So a bucket with
        // no archived/restored entries still has mObjectsPut=1 and returns a bucket
        // with hash 95079eba... (the metaentry-only bucket).
        //
        // We match this behavior by always calling fresh(), which creates a bucket
        // with at least a metaentry.
        let new_bucket =
            HotArchiveBucket::fresh(protocol_version, archived_entries, restored_keys)?;

        self.add_batch_internal(ledger_seq, protocol_version, new_bucket)?;
        self.ledger_seq = ledger_seq;

        Ok(())
    }

    /// Advance the hot archive bucket list from its current ledger to a target ledger
    /// by applying empty batches for all intermediate ledgers.
    ///
    /// This is required because the bucket list merge algorithm depends on being
    /// called for every ledger in sequence.
    ///
    /// # Arguments
    ///
    /// * `target_ledger` - The ledger to advance to (exclusive of actual changes)
    /// * `protocol_version` - Protocol version for empty batches
    ///
    /// # Returns
    ///
    /// Ok(()) if successful, or an error if the target is not greater than current.
    pub fn advance_to_ledger(&mut self, target_ledger: u32, protocol_version: u32) -> Result<()> {
        let current = self.ledger_seq;
        if target_ledger <= current {
            // Nothing to do - we're already at or past this ledger
            return Ok(());
        }

        // Apply empty batches for each intermediate ledger
        for seq in (current + 1)..target_ledger {
            tracing::trace!(
                from_ledger = current,
                to_ledger = target_ledger,
                current_seq = seq,
                "Advancing hot archive bucket list through empty ledger"
            );
            self.add_batch(
                seq,
                protocol_version,
                Vec::new(), // empty archived entries
                Vec::new(), // empty restored keys
            )?;
        }

        Ok(())
    }

    fn add_batch_internal(
        &mut self,
        ledger_seq: u32,
        protocol_version: u32,
        new_bucket: HotArchiveBucket,
    ) -> Result<()> {
        if ledger_seq == 0 {
            return Err(BucketError::Merge(
                "ledger sequence must be > 0".to_string(),
            ));
        }

        // Process spills from highest level down
        for i in (1..HOT_ARCHIVE_BUCKET_LIST_LEVELS).rev() {
            if Self::level_should_spill(ledger_seq, i - 1) {
                let spilling_snap = self.levels[i - 1].snap();
                self.levels[i].commit();

                let keep_tombstones = Self::keep_tombstone_entries(i);
                let use_empty_curr = Self::should_merge_with_empty_curr(ledger_seq, i);
                self.levels[i].prepare(
                    protocol_version,
                    &spilling_snap,
                    keep_tombstones,
                    use_empty_curr,
                )?;
            }
        }

        // Add new entries to level 0
        // Level 0 never uses empty curr (shouldMergeWithEmptyCurr returns false for level 0)
        let keep_tombstones_0 = Self::keep_tombstone_entries(0);

        self.levels[0].prepare(protocol_version, &new_bucket, keep_tombstones_0, false)?;
        self.levels[0].commit();

        Ok(())
    }

    /// Look up an archived entry by key.
    pub fn get(&self, key: &LedgerKey) -> Result<Option<LedgerEntry>> {
        // Serialize key once, then search across all levels/buckets with pre-serialized bytes
        let key_bytes = key.to_xdr(Limits::none()).map_err(|e| {
            BucketError::Serialization(format!("failed to serialize ledger key: {}", e))
        })?;
        for level in &self.levels {
            for bucket in [&level.curr, &level.snap] {
                if let Some(entry) = bucket.get_by_bytes(&key_bytes)? {
                    match entry {
                        HotArchiveBucketEntry::Archived(e) => return Ok(Some(e)),
                        HotArchiveBucketEntry::Live(_) => return Ok(None), // Restored, not in archive
                        HotArchiveBucketEntry::Metaentry(_) => continue,
                    }
                }
            }
        }
        Ok(None)
    }

    /// Check if an entry is in the hot archive.
    pub fn contains(&self, key: &LedgerKey) -> Result<bool> {
        Ok(self.get(key)?.is_some())
    }

    /// Returns true if a level should spill at a given ledger.
    fn level_should_spill(ledger_seq: u32, level: usize) -> bool {
        bl_level_should_spill(ledger_seq, level, HOT_ARCHIVE_BUCKET_LIST_LEVELS)
    }

    /// Check if tombstone entries should be kept at a level.
    fn keep_tombstone_entries(level: usize) -> bool {
        bl_keep_tombstone_entries(level, HOT_ARCHIVE_BUCKET_LIST_LEVELS)
    }

    /// Determines whether to merge with an empty curr bucket instead of the actual curr.
    fn should_merge_with_empty_curr(ledger_seq: u32, level: usize) -> bool {
        bl_should_merge_with_empty_curr(ledger_seq, level, HOT_ARCHIVE_BUCKET_LIST_LEVELS)
    }

    /// Get statistics about the bucket list.
    pub fn stats(&self) -> BucketListStats {
        let mut total_entries = 0;
        let mut total_buckets = 0;

        for level in &self.levels {
            if !level.curr.is_empty() {
                total_entries += level.curr.len();
                total_buckets += 1;
            }
            if !level.snap.is_empty() {
                total_entries += level.snap.len();
                total_buckets += 1;
            }
        }

        BucketListStats {
            num_levels: HOT_ARCHIVE_BUCKET_LIST_LEVELS,
            total_entries,
            total_buckets,
        }
    }

    /// Estimate total heap bytes across all hot archive buckets.
    pub fn estimate_heap_bytes(&self) -> usize {
        self.levels
            .iter()
            .map(|l| l.curr.estimate_heap_bytes() + l.snap.estimate_heap_bytes())
            .sum()
    }

    /// Total mmap'd / disk-backed bytes across all hot archive buckets.
    pub fn mmap_bytes(&self) -> usize {
        self.levels
            .iter()
            .map(|l| l.curr.mmap_bytes() + l.snap.mmap_bytes())
            .sum()
    }

    /// Debug print level state.
    pub fn debug_print_levels(&self) {
        for (i, level) in self.levels.iter().enumerate() {
            let curr_hash = level.curr.hash();
            let snap_hash = level.snap.hash();
            let next_hash = level
                .next
                .as_ref()
                .map(|b| b.hash().to_hex())
                .unwrap_or_else(|| "None".to_string());
            eprintln!(
                "  HA_L{}: curr={}, snap={}, next={}",
                i,
                curr_hash.to_hex(),
                snap_hash.to_hex(),
                next_hash
            );
        }
    }

    /// Restore a hot archive bucket list from bucket hashes.
    ///
    /// This is used to restore state from a history archive checkpoint.
    ///
    /// Restore a hot archive bucket list from hashes and a bucket lookup function.
    ///
    /// This is a convenience wrapper around [`Self::restore_from_has`] for cases where
    /// you only have bucket hashes without HAS next state information. It assumes
    /// no pending merges (state=0 for all levels), which is the common case at
    /// checkpoints.
    ///
    /// # Arguments
    ///
    /// * `hashes` - Flat array of bucket hashes (curr, snap for each level, 22 total)
    /// * `load_bucket` - Function to load a HotArchiveBucket by hash
    pub fn restore_from_hashes<F>(hashes: &[Hash256], load_bucket: F) -> Result<Self>
    where
        F: FnMut(&Hash256) -> Result<HotArchiveBucket>,
    {
        if hashes.len() != HOT_ARCHIVE_BUCKET_LIST_LEVELS * 2 {
            return Err(BucketError::Serialization(format!(
                "Expected {} hot archive bucket hashes, got {}",
                HOT_ARCHIVE_BUCKET_LIST_LEVELS * 2,
                hashes.len()
            )));
        }

        // Convert flat array to (curr, snap) pairs
        let pairs: Vec<(Hash256, Hash256)> =
            hashes.chunks(2).map(|chunk| (chunk[0], chunk[1])).collect();

        // Use default next states (all state=0, no pending merges)
        let next_states = vec![HasNextState::default(); HOT_ARCHIVE_BUCKET_LIST_LEVELS];

        Self::restore_from_has(&pairs, &next_states, load_bucket)
    }

    /// Parallel variant of [`Self::restore_from_has`].
    ///
    /// Loads all levels concurrently via `std::thread::scope`. Requires `Fn + Send + Sync`
    /// instead of `FnMut`.
    ///
    /// # Arguments
    ///
    /// * `hashes` - Vec of (curr_hash, snap_hash) pairs for each level
    /// * `next_states` - Vec of HasNextState for each level
    /// * `load_bucket` - Thread-safe function to load a HotArchiveBucket by hash
    pub fn restore_from_has_parallel<F>(
        hashes: &[(Hash256, Hash256)],
        next_states: &[HasNextState],
        load_bucket: F,
    ) -> Result<Self>
    where
        F: Fn(&Hash256) -> Result<HotArchiveBucket> + Send + Sync,
    {
        if hashes.len() != HOT_ARCHIVE_BUCKET_LIST_LEVELS {
            return Err(BucketError::Serialization(format!(
                "Expected {} hot archive bucket level hashes, got {}",
                HOT_ARCHIVE_BUCKET_LIST_LEVELS,
                hashes.len()
            )));
        }
        if next_states.len() != HOT_ARCHIVE_BUCKET_LIST_LEVELS {
            return Err(BucketError::Serialization(format!(
                "Expected {} next states, got {}",
                HOT_ARCHIVE_BUCKET_LIST_LEVELS,
                next_states.len()
            )));
        }

        let load_bucket = &load_bucket;
        let levels = std::thread::scope(|s| -> Result<Vec<HotArchiveBucketLevel>> {
            let handles: Vec<_> = hashes
                .iter()
                .zip(next_states.iter())
                .enumerate()
                .map(|(i, ((curr_hash, snap_hash), state))| {
                    s.spawn(move || -> Result<HotArchiveBucketLevel> {
                        let curr = if curr_hash.is_zero() {
                            HotArchiveBucket::empty()
                        } else {
                            load_hot_and_verify_shared(curr_hash, load_bucket)?
                        };

                        let snap = if snap_hash.is_zero() {
                            HotArchiveBucket::empty()
                        } else {
                            load_hot_and_verify_shared(snap_hash, load_bucket)?
                        };

                        let next = if state.state == HAS_NEXT_STATE_OUTPUT {
                            if let Some(ref output_hash) = state.output {
                                if !output_hash.is_zero() {
                                    tracing::debug!(
                                        level = i,
                                        output_hash = %output_hash.to_hex(),
                                        "hot_archive restore_from_has_parallel: loading completed merge output"
                                    );
                                    Some(load_hot_and_verify_shared(output_hash, load_bucket)?)
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        } else {
                            None
                        };

                        let mut level = HotArchiveBucketLevel::new(i);
                        level.curr = Arc::new(curr);
                        level.snap = Arc::new(snap);
                        level.next = next;
                        Ok(level)
                    })
                })
                .collect();

            handles
                .into_iter()
                .map(|h| h.join().expect("level load thread panicked"))
                .collect()
        })?;

        Ok(Self {
            levels,
            ledger_seq: 0,
        })
    }

    /// Restore a hot archive bucket list from History Archive State with full FutureBucket support.
    ///
    /// This is the primary restoration method. It restores pending merge results when
    /// the HAS indicates a completed merge (state == HAS_NEXT_STATE_OUTPUT), which is
    /// necessary for correct bucket list hash computation at checkpoints.
    ///
    /// For convenience, [`Self::restore_from_hashes`] wraps this function with default
    /// next states (all state=0).
    ///
    /// # Arguments
    ///
    /// * `hashes` - Vec of (curr_hash, snap_hash) pairs for each level
    /// * `next_states` - Vec of HasNextState for each level
    /// * `load_bucket` - Function to load a HotArchiveBucket by hash
    pub fn restore_from_has<F>(
        hashes: &[(Hash256, Hash256)],
        next_states: &[HasNextState],
        mut load_bucket: F,
    ) -> Result<Self>
    where
        F: FnMut(&Hash256) -> Result<HotArchiveBucket>,
    {
        if hashes.len() != HOT_ARCHIVE_BUCKET_LIST_LEVELS {
            return Err(BucketError::Serialization(format!(
                "Expected {} hot archive bucket level hashes, got {}",
                HOT_ARCHIVE_BUCKET_LIST_LEVELS,
                hashes.len()
            )));
        }
        if next_states.len() != HOT_ARCHIVE_BUCKET_LIST_LEVELS {
            return Err(BucketError::Serialization(format!(
                "Expected {} next states, got {}",
                HOT_ARCHIVE_BUCKET_LIST_LEVELS,
                next_states.len()
            )));
        }

        let mut levels = Vec::with_capacity(HOT_ARCHIVE_BUCKET_LIST_LEVELS);

        for (i, (curr_hash, snap_hash)) in hashes.iter().enumerate() {
            let curr = if curr_hash.is_zero() {
                HotArchiveBucket::empty()
            } else {
                load_hot_and_verify(curr_hash, &mut load_bucket)?
            };

            let snap = if snap_hash.is_zero() {
                HotArchiveBucket::empty()
            } else {
                load_hot_and_verify(snap_hash, &mut load_bucket)?
            };

            // Check if there's a completed merge (state == HAS_NEXT_STATE_OUTPUT) for this level
            let state = &next_states[i];
            let next = if state.state == HAS_NEXT_STATE_OUTPUT {
                if let Some(ref output_hash) = state.output {
                    if !output_hash.is_zero() {
                        tracing::debug!(
                            level = i,
                            output_hash = %output_hash.to_hex(),
                            "hot_archive restore_from_has: loading completed merge output"
                        );
                        Some(load_hot_and_verify(output_hash, &mut load_bucket)?)
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            };

            let mut level = HotArchiveBucketLevel::new(i);
            level.curr = Arc::new(curr);
            level.snap = Arc::new(snap);
            level.next = next;
            levels.push(level);
        }

        Ok(Self {
            levels,
            ledger_seq: 0,
        })
    }

    /// Restart any pending merges after restoring from a History Archive State (HAS),
    /// using the stored input hashes from the HAS.
    ///
    /// This handles state 2 (HAS_NEXT_STATE_INPUTS) by restarting merges with the
    /// exact input curr and snap hashes stored in the HAS.
    ///
    /// This matches stellar-core's logic in restartMerges() for handling
    /// FutureBuckets with hasHashes() && !isLive().
    ///
    /// # Arguments
    ///
    /// * `restart_structure_based` - If true, fall back to structure-based merge restart
    ///   for levels with no stored HAS hashes (state 0). This should be true for full
    ///   startup mode but false for offline verification mode. stellar-core only
    ///   calls restartMerges in full startup mode, not for standalone offline commands.
    pub fn restart_merges_from_has<F>(
        &mut self,
        ledger: u32,
        protocol_version: u32,
        next_states: &[HasNextState],
        mut load_bucket: F,
        restart_structure_based: bool,
    ) -> Result<()>
    where
        F: FnMut(&Hash256) -> Result<HotArchiveBucket>,
    {
        tracing::debug!(
            ledger = ledger,
            "hot_archive restart_merges_from_has: restarting merges using HAS input hashes"
        );

        if next_states.len() != HOT_ARCHIVE_BUCKET_LIST_LEVELS {
            return Err(BucketError::Serialization(format!(
                "Expected {} next states, got {}",
                HOT_ARCHIVE_BUCKET_LIST_LEVELS,
                next_states.len()
            )));
        }

        for i in 1..HOT_ARCHIVE_BUCKET_LIST_LEVELS {
            // Skip if there's already a pending merge (from state 1 output)
            if self.levels[i].next.is_some() {
                tracing::trace!(
                    level = i,
                    "hot_archive restart_merges_from_has: level already has pending merge"
                );
                continue;
            }

            // Check if HAS has stored input hashes for this level (state 2)
            let state = &next_states[i];
            if state.state == HAS_NEXT_STATE_INPUTS {
                if let (Some(ref curr_hash), Some(ref snap_hash)) =
                    (&state.input_curr, &state.input_snap)
                {
                    // Load the input buckets from the stored hashes
                    let input_curr = if curr_hash.is_zero() {
                        HotArchiveBucket::empty()
                    } else {
                        load_hot_and_verify(curr_hash, &mut load_bucket)?
                    };

                    let input_snap = if snap_hash.is_zero() {
                        HotArchiveBucket::empty()
                    } else {
                        load_hot_and_verify(snap_hash, &mut load_bucket)?
                    };

                    tracing::info!(
                        level = i,
                        ledger = ledger,
                        input_curr_hash = %curr_hash.to_hex(),
                        input_snap_hash = %snap_hash.to_hex(),
                        "hot_archive restart_merges_from_has: restarting merge with HAS input hashes"
                    );

                    // Perform the merge with the exact input hashes from HAS
                    // Use the caller's protocol_version (from ledger header) as the
                    // max protocol version, matching stellar-core behavior in restartMerges
                    // where makeLive() is called with maxProtocolVersion.
                    let keep_tombstones = Self::keep_tombstone_entries(i);

                    let merged = merge_hot_archive_buckets(
                        &input_curr,
                        &input_snap,
                        protocol_version, // Use caller's protocol version, not bucket's
                        keep_tombstones,
                    )?;

                    tracing::info!(
                        level = i,
                        merged_hash = %merged.hash().to_hex(),
                        "hot_archive restart_merges_from_has: merge completed"
                    );

                    self.levels[i].next = Some(merged);
                    continue;
                }
            }
        }

        // For levels that don't have HAS input hashes (state 0 = clear),
        // fall back to regular restart_merges which examines bucket structure
        // to determine if a merge should be in progress.
        //
        // This matches stellar-core behavior: when next.isClear() for a level,
        // restartMerges() uses the previous level's snap to start a merge if needed.
        //
        // Note: stellar-core only does structure-based restarts in full startup mode, not for
        // standalone offline commands. The caller controls this via restart_structure_based.
        if restart_structure_based {
            self.restart_merges(ledger, protocol_version)
        } else {
            // Update ledger sequence even when not restarting merges
            self.ledger_seq = ledger;
            Ok(())
        }
    }

    /// Restart any pending merges after restoring from a History Archive State (HAS).
    ///
    /// When a hot archive bucket list is restored from HAS, there may be merges that should
    /// have been in progress at that checkpoint ledger. This function recreates those pending
    /// merges by examining the current and snap buckets and starting merges where appropriate.
    ///
    /// This matches stellar-core's BucketListBase::restartMerges() for hot archive.
    pub fn restart_merges(&mut self, ledger: u32, protocol_version: u32) -> Result<()> {
        tracing::debug!(
            ledger = ledger,
            "hot_archive restart_merges: restarting pending merges after HAS restore"
        );

        for i in 1..HOT_ARCHIVE_BUCKET_LIST_LEVELS {
            // Skip if there's already a pending merge
            if self.levels[i].next.is_some() {
                tracing::trace!(
                    level = i,
                    "hot_archive restart_merges: level already has pending merge"
                );
                continue;
            }

            // Clone the previous level's snap to avoid borrow conflicts
            let prev_snap = self.levels[i - 1].snap.clone();

            // If the previous level's snap is empty, this and all higher levels
            // are uninitialized (haven't received enough data yet)
            if prev_snap.is_empty() {
                tracing::debug!(
                    level = i,
                    "hot_archive restart_merges: previous level snap is empty, stopping"
                );
                break;
            }

            // Calculate the ledger when this merge would have started
            let merge_start_ledger = bl_round_down(ledger, bl_level_half(i - 1));

            tracing::debug!(
                level = i,
                merge_start_ledger = merge_start_ledger,
                prev_snap_hash = %prev_snap.hash(),
                "hot_archive restart_merges: restarting merge"
            );

            // Determine merge parameters
            let merge_protocol_version = match prev_snap.get_protocol_version()? {
                0 => protocol_version,
                version => version,
            };
            let keep_tombstones = Self::keep_tombstone_entries(i);
            let use_empty_curr = Self::should_merge_with_empty_curr(merge_start_ledger, i);

            // Start the merge with the previous level's snap
            self.levels[i].prepare(
                merge_protocol_version,
                &prev_snap,
                keep_tombstones,
                use_empty_curr,
            )?;

            tracing::debug!(
                level = i,
                "hot_archive restart_merges: merge restarted successfully"
            );
        }

        // Update the ledger sequence to the restored ledger
        self.ledger_seq = ledger;

        Ok(())
    }
}

impl Default for HotArchiveBucketList {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for HotArchiveBucketList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HotArchiveBucketList")
            .field("ledger_seq", &self.ledger_seq)
            .field("hash", &self.hash().to_hex())
            .field("stats", &self.stats())
            .finish()
    }
}

/// Extract key bytes from a hot archive bucket entry.
fn hot_archive_entry_to_key(entry: &HotArchiveBucketEntry) -> Result<Vec<u8>> {
    match entry {
        HotArchiveBucketEntry::Archived(e) => {
            let key = henyey_common::entry_to_key(e);
            key.to_xdr(Limits::none())
                .map_err(|e| BucketError::Serialization(format!("failed to serialize key: {}", e)))
        }
        HotArchiveBucketEntry::Live(key) => key
            .to_xdr(Limits::none())
            .map_err(|e| BucketError::Serialization(format!("failed to serialize key: {}", e))),
        HotArchiveBucketEntry::Metaentry(_) => {
            // Metadata uses a special key (empty)
            Ok(Vec::new())
        }
    }
}

/// Compare two hot archive bucket entries for sorting, matching stellar-core's
/// `BucketEntryIdCmp<HotArchiveBucket>`.
///
/// Comparison order:
/// 1. Metaentry always comes first
/// 2. For other entries, compare by LedgerKey using the same order as stellar-core
pub fn compare_hot_archive_entries(
    a: &HotArchiveBucketEntry,
    b: &HotArchiveBucketEntry,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    // Metaentry always sorts first
    match (a, b) {
        (HotArchiveBucketEntry::Metaentry(_), HotArchiveBucketEntry::Metaentry(_)) => {
            return Ordering::Equal;
        }
        (HotArchiveBucketEntry::Metaentry(_), _) => {
            return Ordering::Less;
        }
        (_, HotArchiveBucketEntry::Metaentry(_)) => {
            return Ordering::Greater;
        }
        _ => {}
    }

    // Get LedgerKey from each entry
    let key_a = match a {
        HotArchiveBucketEntry::Archived(e) => Some(henyey_common::entry_to_key(e)),
        HotArchiveBucketEntry::Live(k) => Some(k.clone()),
        HotArchiveBucketEntry::Metaentry(_) => unreachable!(),
    };

    let key_b = match b {
        HotArchiveBucketEntry::Archived(e) => Some(henyey_common::entry_to_key(e)),
        HotArchiveBucketEntry::Live(k) => Some(k.clone()),
        HotArchiveBucketEntry::Metaentry(_) => unreachable!(),
    };

    match (key_a, key_b) {
        (Some(ka), Some(kb)) => compare_ledger_keys(&ka, &kb),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// Compare two LedgerKeys using the same order as stellar-core's `LedgerEntryIdCmp`.
///
/// Delegates to `LedgerKey`'s derived `Ord`, which matches xdrpp's comparison:
/// discriminant first, then fields in XDR declaration order.
fn compare_ledger_keys(a: &LedgerKey, b: &LedgerKey) -> std::cmp::Ordering {
    a.cmp(b)
}

/// Check if a hot archive entry is a tombstone (Live marker).
pub fn is_hot_archive_tombstone(entry: &HotArchiveBucketEntry) -> bool {
    matches!(entry, HotArchiveBucketEntry::Live(_))
}

/// Merge two hot archive buckets.
///
/// Spec: BUCKETLISTDB_SPEC §11.4 — Hot archive merge follows strict "newest wins"
/// semantics with NO annihilation cases. The `snap` (newer/shallower) entry
/// always takes precedence over `curr` (older/deeper) when keys are equal.
/// At the bottom level, LIVE tombstones are dropped by the output iterator.
pub fn merge_hot_archive_buckets(
    curr: &HotArchiveBucket,
    snap: &HotArchiveBucket,
    protocol_version: u32,
    keep_tombstones: bool,
) -> Result<HotArchiveBucket> {
    tracing::trace!(
        curr_hash = %curr.hash().to_hex(),
        curr_len = curr.len(),
        snap_hash = %snap.hash().to_hex(),
        snap_len = snap.len(),
        protocol_version = protocol_version,
        keep_tombstones = keep_tombstones,
        "hot_archive merge: starting merge"
    );

    // NOTE: We intentionally do NOT optimize for empty buckets here.
    // stellar-core always goes through the full merge process even when
    // one input is empty. This is important because:
    // 1. The output bucket gets new metadata (protocol version)
    // 2. The bucket hash includes metadata
    // 3. Returning input unchanged would preserve old metadata and wrong hash
    //
    // The only optimization is when BOTH inputs are empty and protocol version is 0.
    // In stellar-core, even an empty merge produces a bucket with metadata.
    if snap.is_empty() && curr.is_empty() && protocol_version == 0 {
        tracing::trace!("hot_archive merge: both empty and no protocol, returning empty");
        return Ok(HotArchiveBucket::empty());
    }

    // Calculate output protocol version using max(curr, snap), matching stellar-core behavior
    // in calculateMergeProtocolVersion(). The passed protocol_version is only used as
    // a constraint (maxProtocolVersion), not as the output version.
    let output_version = curr
        .get_protocol_version()?
        .max(snap.get_protocol_version()?);

    // Validate that calculated version doesn't exceed the max
    if protocol_version > 0 && output_version > protocol_version {
        return Err(BucketError::Merge(format!(
            "hot archive bucket protocol version {} exceeds max {}",
            output_version, protocol_version
        )));
    }

    // Build result using sorted merge-join.
    //
    // Both curr and snap are already sorted by compare_hot_archive_entries.
    // We merge them in O(n + m) time with O(1) extra space per entry,
    // avoiding the HashMap + sort approach that required O(n + m) extra
    // memory for the HashMap AND a subsequent O((n+m) log(n+m)) sort.
    let mut result_entries = Vec::with_capacity(1 + curr.len() + snap.len());

    // Add metadata - hot archive buckets always use V1 with BucketListType::HotArchive
    // for Protocol 23+.
    let mut meta = BucketMetadata {
        ledger_version: output_version,
        ext: BucketMetadataExt::V0,
    };
    if protocol_version_starts_from(output_version, ProtocolVersion::V23) {
        meta.ext = BucketMetadataExt::V1(BucketListType::HotArchive);
    }
    result_entries.push(HotArchiveBucketEntry::Metaentry(meta));

    // Sorted merge-join: iterate both iterators in lock-step.
    // Snap (newer) wins on equal keys.
    let curr_entries: Vec<_> = curr
        .try_iter()?
        .filter_map(|entry| match entry {
            Ok(HotArchiveBucketEntry::Metaentry(_)) => None,
            other => Some(other),
        })
        .collect::<Result<_>>()?;
    let snap_entries: Vec<_> = snap
        .try_iter()?
        .filter_map(|entry| match entry {
            Ok(HotArchiveBucketEntry::Metaentry(_)) => None,
            other => Some(other),
        })
        .collect::<Result<_>>()?;
    let mut curr_iter = curr_entries.into_iter().peekable();
    let mut snap_iter = snap_entries.into_iter().peekable();

    loop {
        match (curr_iter.peek(), snap_iter.peek()) {
            (None, None) => break,
            (Some(_), None) => {
                let entry = curr_iter.next().unwrap();
                if keep_tombstones || !is_hot_archive_tombstone(&entry) {
                    result_entries.push(entry);
                }
            }
            (None, Some(_)) => {
                let entry = snap_iter.next().unwrap();
                if keep_tombstones || !is_hot_archive_tombstone(&entry) {
                    result_entries.push(entry);
                }
            }
            (Some(c), Some(s)) => {
                let ord = compare_hot_archive_entries(c, s);
                match ord {
                    std::cmp::Ordering::Less => {
                        let entry = curr_iter.next().unwrap();
                        if keep_tombstones || !is_hot_archive_tombstone(&entry) {
                            result_entries.push(entry);
                        }
                    }
                    std::cmp::Ordering::Greater => {
                        let entry = snap_iter.next().unwrap();
                        if keep_tombstones || !is_hot_archive_tombstone(&entry) {
                            result_entries.push(entry);
                        }
                    }
                    std::cmp::Ordering::Equal => {
                        // Snap (newer) wins — skip curr entry.
                        let _ = curr_iter.next();
                        let entry = snap_iter.next().unwrap();
                        if keep_tombstones || !is_hot_archive_tombstone(&entry) {
                            result_entries.push(entry);
                        }
                    }
                }
            }
        }
    }

    let result = HotArchiveBucket::from_entries(result_entries)?;
    tracing::trace!(
        result_hash = %result.hash().to_hex(),
        result_len = result.len(),
        "hot_archive merge: merge complete"
    );
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::*;

    fn make_contract_data_key(contract_id: [u8; 32], key_bytes: &[u8]) -> LedgerKey {
        LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(Hash(contract_id).into()),
            key: ScVal::Bytes(key_bytes.to_vec().try_into().unwrap()),
            durability: ContractDataDurability::Persistent,
        })
    }

    fn make_contract_data_entry(
        contract_id: [u8; 32],
        key_bytes: &[u8],
        value: i64,
    ) -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::ContractData(ContractDataEntry {
                ext: ExtensionPoint::V0,
                contract: ScAddress::Contract(Hash(contract_id).into()),
                key: ScVal::Bytes(key_bytes.to_vec().try_into().unwrap()),
                durability: ContractDataDurability::Persistent,
                val: ScVal::I64(value),
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    #[test]
    fn test_hot_archive_bucket_empty() {
        let bucket = HotArchiveBucket::empty();
        assert!(bucket.is_empty());
        assert_eq!(bucket.len(), 0);
        assert!(bucket.hash().is_zero());
    }

    #[test]
    fn test_hot_archive_bucket_fresh() {
        let entry = make_contract_data_entry([1u8; 32], b"key1", 100);
        let bucket = HotArchiveBucket::fresh(25, vec![entry], vec![]).unwrap();

        assert!(!bucket.is_empty());
        assert_eq!(bucket.len(), 2); // metadata + 1 entry
        assert!(!bucket.hash().is_zero());
    }

    #[test]
    fn test_hot_archive_rejects_trailing_partial_mark() {
        let bucket = HotArchiveBucket::fresh(25, vec![], vec![]).unwrap();
        let mut bytes = bucket.to_xdr_bytes().unwrap();
        bytes.extend_from_slice(&[0xaa, 0xbb]);

        assert!(HotArchiveBucket::from_xdr_bytes(&bytes).is_err());
    }

    #[test]
    fn test_hot_archive_disk_backed_rejects_truncated_body() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hot.bucket");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(100u32 | crate::XDR_RECORD_MARK).to_be_bytes());
        bytes.extend_from_slice(&[1, 2, 3]);
        std::fs::write(&path, bytes).unwrap();

        assert!(HotArchiveBucket::from_xdr_file_disk_backed(&path).is_err());
    }

    #[test]
    fn test_hot_archive_disk_backed_rejects_undecodable_full_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hot-invalid.bucket");
        let body = [0xff, 0xff, 0xff, 0xff];
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&((body.len() as u32) | crate::XDR_RECORD_MARK).to_be_bytes());
        bytes.extend_from_slice(&body);
        std::fs::write(&path, bytes).unwrap();

        assert!(
            HotArchiveBucket::from_xdr_file_disk_backed(&path).is_err(),
            "complete but invalid hot archive XDR records must be rejected"
        );
    }

    #[test]
    fn test_hot_archive_disk_iterator_reports_terminal_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hot.bucket");
        let bucket = HotArchiveBucket::fresh(25, vec![], vec![]).unwrap();
        bucket.save_to_xdr_file(&path).unwrap();
        let disk_bucket = HotArchiveBucket::from_xdr_file_disk_backed(&path).unwrap();
        let mut corrupt = std::fs::read(&path).unwrap();
        corrupt.push(0xff);
        std::fs::write(&path, corrupt).unwrap();

        assert!(disk_bucket.get_protocol_version().is_ok());
        let mut iter = disk_bucket.try_iter().unwrap();
        assert!(iter.next().unwrap().is_ok());
        assert!(iter.next().unwrap().is_err());
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_hot_archive_bucket_fresh_metaentry_only() {
        // Fresh bucket with no data entries should still have metaentry
        // This matches stellar-core behavior where BucketOutputIterator always writes metaentry first
        let bucket = HotArchiveBucket::fresh(25, vec![], vec![]).unwrap();

        // Should have 1 entry (just the metaentry)
        assert_eq!(bucket.len(), 1);
        // Should NOT be considered empty (has metaentry)
        assert!(!bucket.is_empty());
        // Should have non-zero hash
        assert!(!bucket.hash().is_zero());
        // Should have correct protocol version
        assert_eq!(bucket.get_protocol_version().unwrap(), 25);
    }

    #[test]
    fn test_hot_archive_merge_metaentry_only_buckets() {
        // Merging two metaentry-only buckets should produce a metaentry-only bucket
        // This is critical for hash consistency with stellar-core
        let bucket1 = HotArchiveBucket::fresh(25, vec![], vec![]).unwrap();
        let bucket2 = HotArchiveBucket::fresh(25, vec![], vec![]).unwrap();

        let merged = merge_hot_archive_buckets(&bucket1, &bucket2, 25, true).unwrap();

        // Should have 1 entry (just the metaentry)
        assert_eq!(merged.len(), 1);
        // Should NOT be considered empty
        assert!(!merged.is_empty());
        // Should have non-zero hash
        assert!(!merged.hash().is_zero());
        // Should have correct protocol version
        assert_eq!(merged.get_protocol_version().unwrap(), 25);
    }

    #[test]
    fn test_hot_archive_metaentry_only_hash_matches_stellar_core() {
        // This test verifies the hash of a metaentry-only bucket matches stellar-core
        // Bucket 95079eba2ff8ef53c179aa3dedb62b78acd7aa9ba5ddcc391436812c5f7084aa from testnet
        // Contains only a METAENTRY with protocol version 25
        //
        // Raw bytes (hex): 80000010 ffffffff 00000019 00000001 00000001
        // - 80000010: XDR record mark (0x10 = 16 bytes, high bit set)
        // - ffffffff: HotArchiveBucketEntryType::Metaentry = -1
        // - 00000019: ledger_version = 25
        // - 00000001: ext.v = 1
        // - 00000001: BucketListType::HotArchive = 1
        let bucket = HotArchiveBucket::fresh(25, vec![], vec![]).unwrap();

        let expected_hash = "95079eba2ff8ef53c179aa3dedb62b78acd7aa9ba5ddcc391436812c5f7084aa";
        assert_eq!(
            bucket.hash().to_hex(),
            expected_hash,
            "Metaentry-only bucket hash should match stellar-core"
        );
    }

    #[test]
    fn test_hot_archive_bucket_lookup() {
        let entry = make_contract_data_entry([1u8; 32], b"key1", 100);
        let key = make_contract_data_key([1u8; 32], b"key1");

        let bucket = HotArchiveBucket::fresh(25, vec![entry.clone()], vec![]).unwrap();
        let found = bucket.get(&key).unwrap();

        assert!(found.is_some());
        match found.unwrap() {
            HotArchiveBucketEntry::Archived(e) => {
                assert_eq!(e, entry);
            }
            _ => panic!("expected Archived entry"),
        }
    }

    #[test]
    fn test_hot_archive_bucket_list_new() {
        let list = HotArchiveBucketList::new();
        assert_eq!(list.ledger_seq(), 0);
        assert_eq!(list.stats().total_entries, 0);
    }

    #[test]
    fn test_hot_archive_bucket_list_add_batch() {
        let mut list = HotArchiveBucketList::new();
        let entry = make_contract_data_entry([1u8; 32], b"key1", 100);

        list.add_batch(1, 25, vec![entry.clone()], vec![]).unwrap();

        let key = make_contract_data_key([1u8; 32], b"key1");
        let found = list.get(&key).unwrap();
        assert!(found.is_some());
    }

    #[test]
    fn test_hot_archive_bucket_list_restoration() {
        let mut list = HotArchiveBucketList::new();
        let entry = make_contract_data_entry([1u8; 32], b"key1", 100);
        let key = make_contract_data_key([1u8; 32], b"key1");

        // Archive the entry
        list.add_batch(1, 25, vec![entry.clone()], vec![]).unwrap();
        assert!(list.contains(&key).unwrap());

        // Mark as restored
        list.add_batch(2, 25, vec![], vec![key.clone()]).unwrap();

        // Entry should not be found (restored = Live marker shadows Archived)
        assert!(!list.contains(&key).unwrap());
    }

    #[test]
    fn test_merge_newer_always_wins_archived_then_live() {
        let entry = make_contract_data_entry([1u8; 32], b"key1", 100);
        let key = make_contract_data_key([1u8; 32], b"key1");

        // curr (old) has Archived, snap (new) has Live
        // stellar-core always takes newer entry, so Live should win
        let curr = HotArchiveBucket::from_entries(vec![
            HotArchiveBucketEntry::Metaentry(BucketMetadata {
                ledger_version: 25,
                ext: BucketMetadataExt::V1(BucketListType::HotArchive),
            }),
            HotArchiveBucketEntry::Archived(entry),
        ])
        .unwrap();

        let snap = HotArchiveBucket::from_entries(vec![
            HotArchiveBucketEntry::Metaentry(BucketMetadata {
                ledger_version: 25,
                ext: BucketMetadataExt::V1(BucketListType::HotArchive),
            }),
            HotArchiveBucketEntry::Live(key.clone()),
        ])
        .unwrap();

        let merged = merge_hot_archive_buckets(&curr, &snap, 25, true).unwrap();

        // Live from snap (newer) wins - entry exists but is a tombstone
        let entry_result = merged.get(&key).unwrap();
        assert!(entry_result.is_some());
        assert!(matches!(
            entry_result.unwrap(),
            HotArchiveBucketEntry::Live(_)
        ));
    }

    #[test]
    fn test_merge_newer_always_wins_live_then_archived() {
        let entry = make_contract_data_entry([1u8; 32], b"key1", 100);
        let key = make_contract_data_key([1u8; 32], b"key1");

        // curr (old) has Live, snap (new) has Archived
        // stellar-core always takes newer entry, so Archived should win
        let curr = HotArchiveBucket::from_entries(vec![
            HotArchiveBucketEntry::Metaentry(BucketMetadata {
                ledger_version: 25,
                ext: BucketMetadataExt::V1(BucketListType::HotArchive),
            }),
            HotArchiveBucketEntry::Live(key.clone()),
        ])
        .unwrap();

        let snap = HotArchiveBucket::from_entries(vec![
            HotArchiveBucketEntry::Metaentry(BucketMetadata {
                ledger_version: 25,
                ext: BucketMetadataExt::V1(BucketListType::HotArchive),
            }),
            HotArchiveBucketEntry::Archived(entry.clone()),
        ])
        .unwrap();

        let merged = merge_hot_archive_buckets(&curr, &snap, 25, true).unwrap();

        // Archived from snap (newer) wins
        let entry_result = merged.get(&key).unwrap();
        assert!(entry_result.is_some());
        match entry_result.unwrap() {
            HotArchiveBucketEntry::Archived(e) => {
                assert_eq!(e.last_modified_ledger_seq, entry.last_modified_ledger_seq);
            }
            _ => panic!("Expected Archived entry"),
        }
    }

    #[test]
    fn test_tombstones_dropped_at_bottom() {
        let key = make_contract_data_key([1u8; 32], b"key1");

        let bucket = HotArchiveBucket::from_entries(vec![
            HotArchiveBucketEntry::Metaentry(BucketMetadata {
                ledger_version: 25,
                ext: BucketMetadataExt::V1(BucketListType::HotArchive),
            }),
            HotArchiveBucketEntry::Live(key.clone()),
        ])
        .unwrap();

        // keep_tombstones = false (bottom level)
        let merged =
            merge_hot_archive_buckets(&bucket, &HotArchiveBucket::empty(), 25, false).unwrap();

        // Live entry should be dropped
        assert!(merged.get(&key).unwrap().is_none());
    }

    // ============ Protocol Version Handling Regression Tests ============

    #[test]
    fn test_hot_archive_merge_uses_max_of_inputs() {
        // Regression test: hot archive merge uses max(curr, snap) for output version,
        // NOT the passed protocol_version which is only a constraint.
        let entry1 = make_contract_data_entry([1u8; 32], b"key1", 100);
        let entry2 = make_contract_data_entry([2u8; 32], b"key2", 200);

        // curr has protocol version 23
        let curr = HotArchiveBucket::from_entries(vec![
            HotArchiveBucketEntry::Metaentry(BucketMetadata {
                ledger_version: 23,
                ext: BucketMetadataExt::V1(BucketListType::HotArchive),
            }),
            HotArchiveBucketEntry::Archived(entry1),
        ])
        .unwrap();

        // snap has protocol version 24
        let snap = HotArchiveBucket::from_entries(vec![
            HotArchiveBucketEntry::Metaentry(BucketMetadata {
                ledger_version: 24,
                ext: BucketMetadataExt::V1(BucketListType::HotArchive),
            }),
            HotArchiveBucketEntry::Archived(entry2),
        ])
        .unwrap();

        // protocol_version = 25 (constraint from ledger header)
        let merged = merge_hot_archive_buckets(&curr, &snap, 25, true).unwrap();

        // Output should use max(23, 24) = 24, NOT 25
        assert_eq!(
            merged.get_protocol_version().unwrap(),
            24,
            "Hot archive merge should use max(curr=23, snap=24)=24, NOT max_protocol_version=25"
        );
    }

    #[test]
    fn test_hot_archive_merge_validates_constraint() {
        // Regression test: protocol_version should be validated as a constraint
        let entry = make_contract_data_entry([1u8; 32], b"key1", 100);

        // Bucket with protocol version 26 (exceeds constraint)
        let bucket = HotArchiveBucket::from_entries(vec![
            HotArchiveBucketEntry::Metaentry(BucketMetadata {
                ledger_version: 26,
                ext: BucketMetadataExt::V1(BucketListType::HotArchive),
            }),
            HotArchiveBucketEntry::Archived(entry),
        ])
        .unwrap();

        // Should fail because bucket version 26 > constraint 25
        let result = merge_hot_archive_buckets(&bucket, &HotArchiveBucket::empty(), 25, true);
        assert!(
            result.is_err(),
            "Should fail when bucket version exceeds protocol_version constraint"
        );
    }

    #[test]
    fn test_hot_archive_merge_same_version_uses_that_version() {
        // When both buckets have the same version, output should use that version
        let entry1 = make_contract_data_entry([1u8; 32], b"key1", 100);
        let entry2 = make_contract_data_entry([2u8; 32], b"key2", 200);

        let curr = HotArchiveBucket::from_entries(vec![
            HotArchiveBucketEntry::Metaentry(BucketMetadata {
                ledger_version: 24,
                ext: BucketMetadataExt::V1(BucketListType::HotArchive),
            }),
            HotArchiveBucketEntry::Archived(entry1),
        ])
        .unwrap();

        let snap = HotArchiveBucket::from_entries(vec![
            HotArchiveBucketEntry::Metaentry(BucketMetadata {
                ledger_version: 24,
                ext: BucketMetadataExt::V1(BucketListType::HotArchive),
            }),
            HotArchiveBucketEntry::Archived(entry2),
        ])
        .unwrap();

        // max_protocol_version = 25, but both inputs are 24
        let merged = merge_hot_archive_buckets(&curr, &snap, 25, true).unwrap();

        // Output should be 24 (max of inputs), not 25
        assert_eq!(
            merged.get_protocol_version().unwrap(),
            24,
            "Output should be max(24, 24)=24, not constraint=25"
        );
    }

    // ============ hash assertion tests ============

    fn make_hot_loader(
        buckets: Vec<HotArchiveBucket>,
    ) -> impl Fn(&Hash256) -> crate::Result<HotArchiveBucket> + Send + Sync {
        let map: std::collections::HashMap<Hash256, HotArchiveBucket> =
            buckets.into_iter().map(|b| (b.hash(), b)).collect();
        let map = std::sync::Arc::new(map);
        move |hash: &Hash256| {
            map.get(hash).cloned().ok_or_else(|| {
                BucketError::Serialization(format!("bucket not found: {}", hash.to_hex()))
            })
        }
    }

    #[test]
    fn test_hot_restore_from_has_hash_mismatch_curr() {
        let correct = HotArchiveBucket::fresh(
            25,
            vec![make_contract_data_entry([1u8; 32], b"k1", 1)],
            vec![],
        )
        .unwrap();
        let wrong = HotArchiveBucket::fresh(
            25,
            vec![make_contract_data_entry([2u8; 32], b"k2", 2)],
            vec![],
        )
        .unwrap();

        let mut hashes = vec![(Hash256::ZERO, Hash256::ZERO); HOT_ARCHIVE_BUCKET_LIST_LEVELS];
        hashes[0] = (correct.hash(), Hash256::ZERO);

        let next_states = vec![HasNextState::default(); HOT_ARCHIVE_BUCKET_LIST_LEVELS];

        let correct_hash = correct.hash();
        let wrong_clone = wrong.clone();
        let loader = move |hash: &Hash256| {
            if *hash == correct_hash {
                Ok(wrong_clone.clone())
            } else {
                Err(BucketError::Serialization("not found".to_string()))
            }
        };

        let result = HotArchiveBucketList::restore_from_has(&hashes, &next_states, loader);
        assert!(matches!(result, Err(BucketError::HashMismatch { .. })));
    }

    #[test]
    fn test_hot_restore_from_has_parallel_hash_mismatch_curr() {
        let correct = HotArchiveBucket::fresh(
            25,
            vec![make_contract_data_entry([1u8; 32], b"k1", 1)],
            vec![],
        )
        .unwrap();
        let wrong = HotArchiveBucket::fresh(
            25,
            vec![make_contract_data_entry([2u8; 32], b"k2", 2)],
            vec![],
        )
        .unwrap();

        let mut hashes = vec![(Hash256::ZERO, Hash256::ZERO); HOT_ARCHIVE_BUCKET_LIST_LEVELS];
        hashes[0] = (correct.hash(), Hash256::ZERO);

        let next_states = vec![HasNextState::default(); HOT_ARCHIVE_BUCKET_LIST_LEVELS];

        let correct_hash = correct.hash();
        let wrong_clone = wrong.clone();
        let loader = move |hash: &Hash256| {
            if *hash == correct_hash {
                Ok(wrong_clone.clone())
            } else {
                Err(BucketError::Serialization("not found".to_string()))
            }
        };

        let result = HotArchiveBucketList::restore_from_has_parallel(&hashes, &next_states, loader);
        assert!(matches!(result, Err(BucketError::HashMismatch { .. })));
    }

    #[test]
    fn test_hot_restore_from_has_hash_mismatch_output() {
        let bucket_curr = HotArchiveBucket::fresh(
            25,
            vec![make_contract_data_entry([1u8; 32], b"k1", 1)],
            vec![],
        )
        .unwrap();
        let bucket_out = HotArchiveBucket::fresh(
            25,
            vec![make_contract_data_entry([9u8; 32], b"k9", 9)],
            vec![],
        )
        .unwrap();
        let bucket_wrong = HotArchiveBucket::fresh(
            25,
            vec![make_contract_data_entry([7u8; 32], b"k7", 7)],
            vec![],
        )
        .unwrap();

        let hc = bucket_curr.hash();
        let ho = bucket_out.hash();

        let mut hashes = vec![(Hash256::ZERO, Hash256::ZERO); HOT_ARCHIVE_BUCKET_LIST_LEVELS];
        hashes[0] = (hc, Hash256::ZERO);

        let mut next_states = vec![HasNextState::default(); HOT_ARCHIVE_BUCKET_LIST_LEVELS];
        next_states[0] = HasNextState {
            state: HAS_NEXT_STATE_OUTPUT,
            output: Some(ho),
            ..Default::default()
        };

        let wrong_clone = bucket_wrong.clone();
        let loader = make_hot_loader(vec![bucket_curr]);
        let output_hash = ho;
        let combined_loader = move |hash: &Hash256| {
            if *hash == output_hash {
                Ok(wrong_clone.clone())
            } else {
                loader(hash)
            }
        };

        let result = HotArchiveBucketList::restore_from_has(&hashes, &next_states, combined_loader);
        assert!(matches!(result, Err(BucketError::HashMismatch { .. })));
    }

    #[test]
    fn test_hot_restore_from_has_parallel_hash_mismatch_output() {
        let bucket_curr = HotArchiveBucket::fresh(
            25,
            vec![make_contract_data_entry([1u8; 32], b"k1", 1)],
            vec![],
        )
        .unwrap();
        let bucket_out = HotArchiveBucket::fresh(
            25,
            vec![make_contract_data_entry([9u8; 32], b"k9", 9)],
            vec![],
        )
        .unwrap();
        let bucket_wrong = HotArchiveBucket::fresh(
            25,
            vec![make_contract_data_entry([7u8; 32], b"k7", 7)],
            vec![],
        )
        .unwrap();

        let hc = bucket_curr.hash();
        let ho = bucket_out.hash();

        let mut hashes = vec![(Hash256::ZERO, Hash256::ZERO); HOT_ARCHIVE_BUCKET_LIST_LEVELS];
        hashes[0] = (hc, Hash256::ZERO);

        let mut next_states = vec![HasNextState::default(); HOT_ARCHIVE_BUCKET_LIST_LEVELS];
        next_states[0] = HasNextState {
            state: HAS_NEXT_STATE_OUTPUT,
            output: Some(ho),
            ..Default::default()
        };

        let wrong_clone = bucket_wrong.clone();
        let loader = make_hot_loader(vec![bucket_curr]);
        let output_hash = ho;
        let combined_loader = move |hash: &Hash256| {
            if *hash == output_hash {
                Ok(wrong_clone.clone())
            } else {
                loader(hash)
            }
        };

        let result =
            HotArchiveBucketList::restore_from_has_parallel(&hashes, &next_states, combined_loader);
        assert!(matches!(result, Err(BucketError::HashMismatch { .. })));
    }

    #[test]
    fn test_hot_restore_from_has_next_states_under_length() {
        let hashes = vec![(Hash256::ZERO, Hash256::ZERO); HOT_ARCHIVE_BUCKET_LIST_LEVELS];
        let next_states = vec![HasNextState::default(); HOT_ARCHIVE_BUCKET_LIST_LEVELS - 1];

        let result = HotArchiveBucketList::restore_from_has(&hashes, &next_states, |_| {
            unreachable!("should not call loader")
        });
        assert!(result.is_err());

        let result = HotArchiveBucketList::restore_from_has_parallel(&hashes, &next_states, |_| {
            unreachable!("should not call loader")
        });
        assert!(result.is_err());
    }

    #[test]
    fn test_hot_restore_from_has_next_states_over_length() {
        let hashes = vec![(Hash256::ZERO, Hash256::ZERO); HOT_ARCHIVE_BUCKET_LIST_LEVELS];
        let next_states = vec![HasNextState::default(); HOT_ARCHIVE_BUCKET_LIST_LEVELS + 1];

        let result = HotArchiveBucketList::restore_from_has(&hashes, &next_states, |_| {
            unreachable!("should not call loader")
        });
        assert!(result.is_err());

        let result = HotArchiveBucketList::restore_from_has_parallel(&hashes, &next_states, |_| {
            unreachable!("should not call loader")
        });
        assert!(result.is_err());
    }

    #[test]
    fn test_hot_restart_merges_from_has_next_states_under_length() {
        let mut ha = HotArchiveBucketList::new();
        let next_states = vec![HasNextState::default(); HOT_ARCHIVE_BUCKET_LIST_LEVELS - 1];

        let result = ha.restart_merges_from_has(1, 25, &next_states, |_| unreachable!(), false);
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("Expected 11 next states, got 10"),
            "{err_msg}"
        );
    }

    #[test]
    fn test_hot_restart_merges_from_has_next_states_over_length() {
        let mut ha = HotArchiveBucketList::new();
        let next_states = vec![HasNextState::default(); HOT_ARCHIVE_BUCKET_LIST_LEVELS + 1];

        let result = ha.restart_merges_from_has(1, 25, &next_states, |_| unreachable!(), false);
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("Expected 11 next states, got 12"),
            "{err_msg}"
        );
    }

    #[test]
    fn test_hot_restart_merges_from_has_hash_mismatch_input() {
        use crate::bucket_list::{HasNextState, HAS_NEXT_STATE_INPUTS};

        let mut ha = HotArchiveBucketList::new();

        // Create a real hot archive bucket with an entry
        let entry = make_contract_data_entry([7u8; 32], b"key", 700);
        let real_bucket =
            HotArchiveBucket::from_entries(vec![HotArchiveBucketEntry::Archived(entry)]).unwrap();
        let real_hash = real_bucket.hash();
        assert!(!real_hash.is_zero());

        // Create a different bucket whose hash won't match
        let entry2 = make_contract_data_entry([8u8; 32], b"key2", 800);
        let wrong_bucket =
            HotArchiveBucket::from_entries(vec![HotArchiveBucketEntry::Archived(entry2)]).unwrap();
        let wrong_hash = wrong_bucket.hash();
        assert_ne!(real_hash, wrong_hash);

        // Build next_states with state-2 inputs using the real hash at level 1
        let mut next_states = vec![HasNextState::default(); HOT_ARCHIVE_BUCKET_LIST_LEVELS];
        next_states[1] = HasNextState {
            state: HAS_NEXT_STATE_INPUTS,
            output: None,
            input_curr: Some(real_hash),
            input_snap: Some(Hash256::ZERO),
        };

        // Loader returns the WRONG bucket for the real hash
        let result = ha.restart_merges_from_has(
            1,
            25,
            &next_states,
            |hash| {
                if hash.is_zero() {
                    return Ok(HotArchiveBucket::empty());
                }
                Ok(wrong_bucket.clone())
            },
            false,
        );

        assert!(result.is_err());
        match result.unwrap_err() {
            BucketError::HashMismatch { expected, actual } => {
                assert_eq!(expected, real_hash.to_hex());
                assert_eq!(actual, wrong_hash.to_hex());
            }
            other => panic!("Expected HashMismatch, got: {:?}", other),
        }
    }
}
