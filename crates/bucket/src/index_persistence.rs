//! Index persistence for BucketListDB.
//!
//! This module provides serialization and deserialization of `DiskIndex`
//! structures to `.index` files, enabling fast startup without rebuilding
//! indexes from bucket files.
//!
//! # File Format
//!
//! Index files use bincode serialization with a version header:
//!
//! ```text
//! {hash}.bucket.index
//! ├── header
//! │   ├── version: u32 (BUCKET_INDEX_VERSION)
//! │   └── page_size: u64
//! └── data
//!     ├── pages: Vec<(SerializableRangeEntry, u64)>
//!     ├── bloom_data: Option<BloomFilterData>
//!     ├── counters: SerializableCounters
//!     └── type_ranges: HashMap<u32, (u64, u64)>
//! ```
//!
//! # Version Compatibility
//!
//! If the stored version doesn't match `BUCKET_INDEX_VERSION`, the index
//! file is discarded and rebuilt from the bucket file.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use henyey_common::Hash256;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use stellar_xdr::curr::{LedgerEntryType, LedgerKey, Limits, PoolId, ReadXdr, WriteXdr};
use xorf::BinaryFuse16;

use crate::bloom_filter::BucketBloomFilter;
use crate::index::{AssetPoolIdMap, BucketEntryCounters, DiskIndex, RangeEntry, TypeRange};
use henyey_common::fs_utils::durable_rename;

use crate::BucketError;

/// Current version of the index file format.
///
/// Increment this when making breaking changes to the serialization format.
/// Version 2: Added bloom filter and asset-to-pool-id map persistence.
/// Version 3: Page size changed from entry-count to byte-offset semantics.
/// Version 4: Added entry_type_sizes to counters.
/// Version 5: Added bucket_hash, bucket_file_size, and data_checksum for integrity.
pub const BUCKET_INDEX_VERSION: u32 = 5;

// ============================================================================
// Serializable Types
// ============================================================================

/// Header for index files, used for version and integrity checking.
#[derive(Serialize, Deserialize, Debug)]
struct IndexHeader {
    version: u32,
    page_size: u64,
    /// SHA-256 hash of the bucket file this index was built from.
    bucket_hash: [u8; 32],
    /// Size in bytes of the bucket file this index was built from.
    bucket_file_size: u64,
}

/// Serializable version of RangeEntry.
///
/// LedgerKey doesn't implement Serialize, so we store it as XDR bytes.
#[derive(Serialize, Deserialize, Debug)]
struct SerializableRangeEntry {
    lower_bound_xdr: Vec<u8>,
    upper_bound_xdr: Vec<u8>,
}

impl SerializableRangeEntry {
    fn from_range_entry(entry: &RangeEntry) -> Result<Self, BucketError> {
        let lower_bound_xdr = entry.lower_bound.to_xdr(Limits::none()).map_err(|e| {
            BucketError::Serialization(format!("Failed to serialize lower bound: {}", e))
        })?;
        let upper_bound_xdr = entry.upper_bound.to_xdr(Limits::none()).map_err(|e| {
            BucketError::Serialization(format!("Failed to serialize upper bound: {}", e))
        })?;
        Ok(Self {
            lower_bound_xdr,
            upper_bound_xdr,
        })
    }

    fn to_range_entry(&self) -> Result<RangeEntry, BucketError> {
        let lower_bound =
            LedgerKey::from_xdr(&self.lower_bound_xdr, Limits::none()).map_err(|e| {
                BucketError::Serialization(format!("Failed to deserialize lower bound: {}", e))
            })?;
        let upper_bound =
            LedgerKey::from_xdr(&self.upper_bound_xdr, Limits::none()).map_err(|e| {
                BucketError::Serialization(format!("Failed to deserialize upper bound: {}", e))
            })?;
        Ok(RangeEntry::new(lower_bound, upper_bound))
    }
}

/// Serializable wrapper for the asset-to-pool-id mapping.
///
/// Converts `HashMap<[u8;32], HashSet<PoolId>>` to/from
/// `HashMap<[u8;32], Vec<[u8;32]>>` for serde compatibility.
#[derive(Serialize, Deserialize, Debug, Default)]
struct SerializableAssetPoolIdMap {
    /// Maps asset hash to list of pool ID bytes.
    entries: HashMap<[u8; 32], Vec<[u8; 32]>>,
}

impl SerializableAssetPoolIdMap {
    fn from_asset_pool_map(map: &AssetPoolIdMap) -> Self {
        let entries = map
            .raw_map()
            .iter()
            .map(|(asset_hash, pool_ids)| {
                let pool_bytes: Vec<[u8; 32]> = pool_ids.iter().map(|pid| pid.0 .0).collect();
                (asset_hash.0, pool_bytes)
            })
            .collect();
        Self { entries }
    }

    fn to_asset_pool_map(&self) -> AssetPoolIdMap {
        use stellar_xdr::curr::Hash;
        let asset_to_pools = self
            .entries
            .iter()
            .map(|(asset_hash, pool_bytes)| {
                let pool_ids: std::collections::HashSet<PoolId> = pool_bytes
                    .iter()
                    .map(|bytes| PoolId(Hash(*bytes)))
                    .collect();
                (Hash256(*asset_hash), pool_ids)
            })
            .collect();
        AssetPoolIdMap::from_raw(asset_to_pools)
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Serializable entry counters.
#[derive(Serialize, Deserialize, Debug, Default)]
struct SerializableCounters {
    /// Count of live entries by entry type (u32 discriminant).
    live_entries: HashMap<u32, u64>,
    /// Count of dead entries by entry type.
    dead_entries: HashMap<u32, u64>,
    /// Count of init entries by entry type.
    init_entries: HashMap<u32, u64>,
    /// XDR byte sizes per entry type.
    #[serde(default)]
    entry_type_sizes: HashMap<u32, u64>,
    /// Count of persistent Soroban entries.
    persistent_soroban_entries: u64,
    /// Count of temporary Soroban entries.
    temporary_soroban_entries: u64,
}

impl SerializableCounters {
    fn from_counters(counters: &BucketEntryCounters) -> Self {
        Self {
            live_entries: counters
                .live_entries
                .iter()
                .map(|(k, v)| (entry_type_to_u32(*k), *v))
                .collect(),
            dead_entries: counters
                .dead_entries
                .iter()
                .map(|(k, v)| (entry_type_to_u32(*k), *v))
                .collect(),
            init_entries: counters
                .init_entries
                .iter()
                .map(|(k, v)| (entry_type_to_u32(*k), *v))
                .collect(),
            entry_type_sizes: counters
                .entry_type_sizes
                .iter()
                .map(|(k, v)| (entry_type_to_u32(*k), *v))
                .collect(),
            persistent_soroban_entries: counters.persistent_soroban_entries,
            temporary_soroban_entries: counters.temporary_soroban_entries,
        }
    }

    fn to_counters(&self) -> BucketEntryCounters {
        BucketEntryCounters {
            live_entries: self
                .live_entries
                .iter()
                .filter_map(|(k, v)| u32_to_entry_type(*k).map(|t| (t, *v)))
                .collect(),
            dead_entries: self
                .dead_entries
                .iter()
                .filter_map(|(k, v)| u32_to_entry_type(*k).map(|t| (t, *v)))
                .collect(),
            init_entries: self
                .init_entries
                .iter()
                .filter_map(|(k, v)| u32_to_entry_type(*k).map(|t| (t, *v)))
                .collect(),
            entry_type_sizes: self
                .entry_type_sizes
                .iter()
                .filter_map(|(k, v)| u32_to_entry_type(*k).map(|t| (t, *v)))
                .collect(),
            persistent_soroban_entries: self.persistent_soroban_entries,
            temporary_soroban_entries: self.temporary_soroban_entries,
        }
    }
}

/// Full serializable index data.
#[derive(Serialize, Deserialize, Debug)]
struct IndexData {
    pages: Vec<(SerializableRangeEntry, u64)>,
    bloom_seed: [u8; 16],
    /// Bloom filter (BinaryFuse16 is directly serializable via xorf's serde feature).
    bloom_filter: Option<BinaryFuse16>,
    counters: SerializableCounters,
    /// Type ranges stored as (entry_type_u32, (start, end)).
    type_ranges: HashMap<u32, (u64, u64)>,
    /// Asset-to-pool-id mapping.
    asset_pool_map: Option<SerializableAssetPoolIdMap>,
}

// ============================================================================
// Helper Functions
// ============================================================================

fn entry_type_to_u32(entry_type: LedgerEntryType) -> u32 {
    entry_type as u32
}

fn u32_to_entry_type(value: u32) -> Option<LedgerEntryType> {
    LedgerEntryType::try_from(value as i32).ok()
}

// ============================================================================
// Persistence Functions
// ============================================================================

/// Compute the index file path for a bucket.
pub fn index_path_for_bucket(bucket_path: &Path) -> PathBuf {
    let mut index_path = bucket_path.to_path_buf();
    index_path.set_extension("index");
    index_path
}

/// Save a DiskIndex to disk.
///
/// Uses atomic write via temp file + rename to prevent corruption.
///
/// # Arguments
///
/// * `index` - The DiskIndex to save
/// * `bucket_path` - Path to the bucket file (used to derive index path)
///
/// # Returns
///
/// `Ok(())` on success, `Err` on I/O or serialization failure.
pub fn save_disk_index(
    index: &DiskIndex,
    bucket_path: &Path,
    bucket_hash: &Hash256,
) -> Result<(), BucketError> {
    let index_path = index_path_for_bucket(bucket_path);
    let tmp_path = index_path.with_extension("index.tmp");

    // Derive bucket file size from the actual file
    let bucket_file_size = std::fs::metadata(bucket_path)?.len();

    // Serialize index data
    let pages: Result<Vec<_>, _> = index
        .pages_iter()
        .map(|(range, offset)| {
            SerializableRangeEntry::from_range_entry(range).map(|r| (r, *offset))
        })
        .collect();
    let pages = pages?;

    let type_ranges: HashMap<u32, (u64, u64)> = index
        .type_ranges_iter()
        .map(|(entry_type, range)| {
            (
                entry_type_to_u32(*entry_type),
                (range.start_offset, range.end_offset),
            )
        })
        .collect();

    // Serialize bloom filter if present
    let bloom_filter = index.bloom_filter().map(|bf| bf.inner_filter().clone());

    // Serialize asset pool map if non-empty
    let asset_pool_map = {
        let map = SerializableAssetPoolIdMap::from_asset_pool_map(index.asset_to_pool_id());
        if map.is_empty() {
            None
        } else {
            Some(map)
        }
    };

    let data = IndexData {
        pages,
        bloom_seed: index.bloom_seed(),
        bloom_filter,
        counters: SerializableCounters::from_counters(index.counters()),
        type_ranges,
        asset_pool_map,
    };

    let header = IndexHeader {
        version: BUCKET_INDEX_VERSION,
        page_size: index.page_size(),
        bucket_hash: *bucket_hash.as_bytes(),
        bucket_file_size,
    };

    // Serialize data to bytes first so we can compute a checksum (Layer 3)
    let data_bytes = bincode::serialize(&data)
        .map_err(|e| BucketError::Serialization(format!("Failed to serialize data: {}", e)))?;

    let mut hasher = sha2::Sha256::new();
    hasher.update(&data_bytes);
    let data_checksum: [u8; 32] = hasher.finalize().into();

    // Write to temp file: header + checksum + data
    {
        let file = File::create(&tmp_path)?;
        let mut writer = BufWriter::new(file);

        bincode::serialize_into(&mut writer, &header).map_err(|e| {
            BucketError::Serialization(format!("Failed to serialize header: {}", e))
        })?;
        bincode::serialize_into(&mut writer, &data_checksum).map_err(|e| {
            BucketError::Serialization(format!("Failed to serialize checksum: {}", e))
        })?;
        writer.write_all(&data_bytes)?;

        writer.flush()?;
    }

    // Durable atomic rename (with directory fsync for crash safety)
    match durable_rename(&tmp_path, &index_path) {
        Ok(()) => {}
        Err(_) => {
            // Retry after short delay (race condition workaround)
            std::thread::sleep(Duration::from_millis(100));
            durable_rename(&tmp_path, &index_path)?;
        }
    }

    tracing::debug!(
        path = %index_path.display(),
        "Saved bucket index"
    );

    Ok(())
}

/// Load a DiskIndex from disk.
///
/// Returns `None` if:
/// - Index file doesn't exist
/// - Version mismatch
/// - PageSize mismatch
/// - Bucket hash or file size mismatch (stale/mispaired index)
/// - Data checksum mismatch (corrupted index data)
/// - Deserialization error
///
/// # Arguments
///
/// * `bucket_path` - Path to the bucket file
/// * `expected_page_size` - Expected page size (triggers rebuild if different)
/// * `expected_hash` - Expected SHA-256 hash of the bucket file
///
/// # Returns
///
/// `Some(DiskIndex)` if successfully loaded, `None` if should rebuild.
pub fn load_disk_index(
    bucket_path: &Path,
    expected_page_size: u64,
    expected_hash: &Hash256,
) -> Result<Option<DiskIndex>, BucketError> {
    let index_path = index_path_for_bucket(bucket_path);

    if !index_path.exists() {
        return Ok(None);
    }

    let file = match File::open(&index_path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut reader = BufReader::new(file);

    // Read and validate header
    let header: IndexHeader = match bincode::deserialize_from(&mut reader) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(
                path = %index_path.display(),
                error = %e,
                "Failed to read index header, will rebuild"
            );
            let _ = std::fs::remove_file(&index_path);
            return Ok(None);
        }
    };

    // Version check
    if header.version != BUCKET_INDEX_VERSION {
        tracing::info!(
            path = %index_path.display(),
            stored_version = header.version,
            expected_version = BUCKET_INDEX_VERSION,
            "Index version mismatch, will rebuild"
        );
        // Delete outdated file
        let _ = std::fs::remove_file(&index_path);
        return Ok(None);
    }

    // PageSize check
    if header.page_size != expected_page_size {
        tracing::info!(
            path = %index_path.display(),
            stored_page_size = header.page_size,
            expected_page_size = expected_page_size,
            "Index page size mismatch, will rebuild"
        );
        let _ = std::fs::remove_file(&index_path);
        return Ok(None);
    }

    // Bucket hash check (Layer 2: detect stale/mispaired indexes)
    if header.bucket_hash != *expected_hash.as_bytes() {
        tracing::info!(
            path = %index_path.display(),
            stored_hash = %Hash256::from(header.bucket_hash).to_hex(),
            expected_hash = %expected_hash.to_hex(),
            "Index bucket hash mismatch, will rebuild"
        );
        let _ = std::fs::remove_file(&index_path);
        return Ok(None);
    }

    // Bucket file size check (Layer 2: detect truncated/replaced files)
    let actual_file_size = std::fs::metadata(bucket_path)?.len();
    if header.bucket_file_size != actual_file_size {
        tracing::info!(
            path = %index_path.display(),
            stored_size = header.bucket_file_size,
            actual_size = actual_file_size,
            "Index bucket file size mismatch, will rebuild"
        );
        let _ = std::fs::remove_file(&index_path);
        return Ok(None);
    }

    // Read data checksum (Layer 3)
    let stored_checksum: [u8; 32] = match bincode::deserialize_from(&mut reader) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                path = %index_path.display(),
                error = %e,
                "Failed to read data checksum, will rebuild"
            );
            let _ = std::fs::remove_file(&index_path);
            return Ok(None);
        }
    };

    // Read remaining data bytes for checksum verification
    let mut data_bytes = Vec::new();
    std::io::Read::read_to_end(&mut reader, &mut data_bytes)?;

    // Verify data checksum (Layer 3: detect corrupted index data)
    let mut hasher = sha2::Sha256::new();
    hasher.update(&data_bytes);
    let computed_checksum: [u8; 32] = hasher.finalize().into();
    if stored_checksum != computed_checksum {
        tracing::warn!(
            path = %index_path.display(),
            "Index data checksum mismatch, will rebuild"
        );
        let _ = std::fs::remove_file(&index_path);
        return Ok(None);
    }

    // Deserialize the verified data
    let data: IndexData = match bincode::deserialize(&data_bytes) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(
                path = %index_path.display(),
                error = %e,
                "Failed to deserialize index, will rebuild"
            );
            let _ = std::fs::remove_file(&index_path);
            return Ok(None);
        }
    };

    // Convert to DiskIndex
    let pages: Result<Vec<_>, _> = data
        .pages
        .into_iter()
        .map(|(range, offset)| range.to_range_entry().map(|r| (r, offset)))
        .collect();
    let pages = pages?;

    let type_ranges: HashMap<LedgerEntryType, TypeRange> = data
        .type_ranges
        .into_iter()
        .filter_map(|(k, (start, end))| {
            u32_to_entry_type(k).map(|t| (t, TypeRange::new(start, end)))
        })
        .collect();

    // Restore bloom filter
    let bloom_filter = data
        .bloom_filter
        .map(|bf| BucketBloomFilter::from_parts(bf, data.bloom_seed));

    // Restore asset pool map
    let asset_pool_map = data.asset_pool_map.map(|m| m.to_asset_pool_map());

    let index = DiskIndex::from_persisted(
        header.page_size,
        pages,
        data.bloom_seed,
        data.counters.to_counters(),
        type_ranges,
        bloom_filter,
        asset_pool_map,
    );

    tracing::debug!(
        path = %index_path.display(),
        "Loaded bucket index from disk"
    );

    Ok(Some(index))
}

/// Delete an index file.
pub fn delete_index(bucket_path: &Path) -> Result<(), BucketError> {
    let index_path = index_path_for_bucket(bucket_path);
    if index_path.exists() {
        std::fs::remove_file(&index_path)?;
    }
    Ok(())
}

/// Clean up orphaned index files (indexes without corresponding bucket files).
pub fn cleanup_orphaned_indexes(bucket_dir: &Path) -> Result<usize, BucketError> {
    let mut removed_count = 0;

    for entry in std::fs::read_dir(bucket_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.extension().map(|e| e == "index").unwrap_or(false) {
            // Check if corresponding bucket file exists
            let mut bucket_path = path.clone();
            bucket_path.set_extension("xdr");

            if !bucket_path.exists() {
                tracing::info!(
                    path = %path.display(),
                    "Removing orphaned index file"
                );
                std::fs::remove_file(&path)?;
                removed_count += 1;
            }
        }
    }

    Ok(removed_count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::DEFAULT_PAGE_SIZE;
    use henyey_common::LIQUIDITY_POOL_FEE_V18;
    use stellar_xdr::curr::*;
    use tempfile::tempdir;

    /// Create a dummy bucket file for testing and return its SHA-256 hash.
    fn create_dummy_bucket_file(bucket_path: &Path) -> Hash256 {
        let data = b"dummy bucket data for index persistence tests";
        std::fs::write(bucket_path, data).unwrap();
        Hash256::hash(data)
    }

    fn make_account_key(byte: u8) -> LedgerKey {
        LedgerKey::Account(LedgerKeyAccount {
            account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([byte; 32]))),
        })
    }

    #[test]
    fn test_serializable_range_entry_roundtrip() {
        let range = RangeEntry::new(make_account_key(1), make_account_key(10));
        let serializable = SerializableRangeEntry::from_range_entry(&range).unwrap();
        let restored = serializable.to_range_entry().unwrap();

        // Verify keys match
        let orig_lower_bytes = range.lower_bound.to_xdr(Limits::none()).unwrap();
        let restored_lower_bytes = restored.lower_bound.to_xdr(Limits::none()).unwrap();
        assert_eq!(orig_lower_bytes, restored_lower_bytes);
    }

    #[test]
    fn test_entry_type_conversion() {
        let types = vec![
            LedgerEntryType::Account,
            LedgerEntryType::Trustline,
            LedgerEntryType::Offer,
            LedgerEntryType::ContractData,
            LedgerEntryType::Ttl,
        ];

        for entry_type in types {
            let u32_val = entry_type_to_u32(entry_type);
            let restored = u32_to_entry_type(u32_val);
            assert_eq!(restored, Some(entry_type));
        }
    }

    #[test]
    fn test_index_path_for_bucket() {
        let bucket_path = Path::new("/tmp/buckets/bucket-abc123.xdr");
        let index_path = index_path_for_bucket(bucket_path);
        assert_eq!(
            index_path.to_str().unwrap(),
            "/tmp/buckets/bucket-abc123.index"
        );
    }

    #[test]
    fn test_load_nonexistent_index() {
        let temp_dir = tempdir().unwrap();
        let bucket_path = temp_dir.path().join("bucket-test.xdr");

        let result = load_disk_index(&bucket_path, DEFAULT_PAGE_SIZE, &Hash256::ZERO).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_serializable_counters_roundtrip() {
        let mut counters = BucketEntryCounters::default();
        counters.live_entries.insert(LedgerEntryType::Account, 100);
        counters.dead_entries.insert(LedgerEntryType::Offer, 50);
        counters.persistent_soroban_entries = 25;

        let serializable = SerializableCounters::from_counters(&counters);
        let restored = serializable.to_counters();

        assert_eq!(
            restored.live_entries.get(&LedgerEntryType::Account),
            Some(&100)
        );
        assert_eq!(
            restored.dead_entries.get(&LedgerEntryType::Offer),
            Some(&50)
        );
        assert_eq!(restored.persistent_soroban_entries, 25);
    }

    #[test]
    fn test_save_load_disk_index_roundtrip() {
        use crate::entry::BucketEntry;
        use crate::index::DiskIndex;

        fn make_account_entry(byte: u8) -> LedgerEntry {
            LedgerEntry {
                last_modified_ledger_seq: 1,
                data: LedgerEntryData::Account(AccountEntry {
                    account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([byte; 32]))),
                    balance: 100,
                    seq_num: SequenceNumber(1),
                    num_sub_entries: 0,
                    inflation_dest: None,
                    flags: 0,
                    home_domain: String32::default(),
                    thresholds: Thresholds([1, 0, 0, 0]),
                    signers: vec![].try_into().unwrap(),
                    ext: AccountEntryExt::V0,
                }),
                ext: LedgerEntryExt::V0,
            }
        }

        let temp_dir = tempdir().unwrap();
        let bucket_path = temp_dir.path().join("bucket-test.xdr");
        let bucket_hash = create_dummy_bucket_file(&bucket_path);

        // Create a DiskIndex with entries
        let entries: Vec<(BucketEntry, u64)> = (0..100u8)
            .map(|i| {
                (
                    BucketEntry::Liveentry(make_account_entry(i)),
                    i as u64 * 100,
                )
            })
            .collect();

        let bloom_seed = [42u8; 16];
        let page_size = 10u64;
        let original = DiskIndex::from_entries(entries.into_iter(), bloom_seed, page_size);

        // Save the index
        save_disk_index(&original, &bucket_path, &bucket_hash).unwrap();

        // Verify index file was created
        let index_path = index_path_for_bucket(&bucket_path);
        assert!(index_path.exists());

        // Load the index back
        let loaded = load_disk_index(&bucket_path, page_size, &bucket_hash)
            .unwrap()
            .expect("Index should load successfully");

        // Verify properties match
        assert_eq!(loaded.page_size(), original.page_size());
        assert_eq!(loaded.num_pages(), original.num_pages());
        assert_eq!(loaded.bloom_seed(), original.bloom_seed());

        // Verify counters match
        assert_eq!(
            loaded.counters().total_live(),
            original.counters().total_live()
        );
        assert_eq!(
            loaded
                .counters()
                .live_entries
                .get(&LedgerEntryType::Account),
            original
                .counters()
                .live_entries
                .get(&LedgerEntryType::Account)
        );

        // Verify type ranges match
        let orig_range = original.type_range(LedgerEntryType::Account);
        let loaded_range = loaded.type_range(LedgerEntryType::Account);
        assert!(orig_range.is_some());
        assert!(loaded_range.is_some());
        assert_eq!(
            orig_range.unwrap().start_offset,
            loaded_range.unwrap().start_offset
        );

        // Verify bloom filter survives persistence
        assert!(
            original.bloom_filter().is_some(),
            "Original should have a bloom filter"
        );
        assert!(
            loaded.bloom_filter().is_some(),
            "Loaded index should have bloom filter restored"
        );

        // Verify bloom filter no false negatives: all original keys found
        for i in 0..100u8 {
            let key = make_account_key(i);
            assert!(
                loaded.may_contain(&key),
                "bloom filter false negative for key {}",
                i
            );
        }

        // Verify page lookup works on loaded index
        let key = make_account_key(55);
        let orig_page = original.find_page_for_key(&key);
        let loaded_page = loaded.find_page_for_key(&key);
        assert!(orig_page.is_some());
        assert_eq!(orig_page, loaded_page);
    }

    #[test]
    fn test_load_with_wrong_page_size_returns_none() {
        use crate::entry::BucketEntry;
        use crate::index::DiskIndex;

        fn make_account_entry(byte: u8) -> LedgerEntry {
            LedgerEntry {
                last_modified_ledger_seq: 1,
                data: LedgerEntryData::Account(AccountEntry {
                    account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([byte; 32]))),
                    balance: 100,
                    seq_num: SequenceNumber(1),
                    num_sub_entries: 0,
                    inflation_dest: None,
                    flags: 0,
                    home_domain: String32::default(),
                    thresholds: Thresholds([1, 0, 0, 0]),
                    signers: vec![].try_into().unwrap(),
                    ext: AccountEntryExt::V0,
                }),
                ext: LedgerEntryExt::V0,
            }
        }

        let temp_dir = tempdir().unwrap();
        let bucket_path = temp_dir.path().join("bucket-test.xdr");
        let bucket_hash = create_dummy_bucket_file(&bucket_path);

        // Create and save with page_size = 10
        let entries: Vec<(BucketEntry, u64)> = (0..50u8)
            .map(|i| {
                (
                    BucketEntry::Liveentry(make_account_entry(i)),
                    i as u64 * 100,
                )
            })
            .collect();

        let index = DiskIndex::from_entries(entries.into_iter(), [0u8; 16], 10);
        save_disk_index(&index, &bucket_path, &bucket_hash).unwrap();

        // Try to load with different page_size = 20
        let result = load_disk_index(&bucket_path, 20, &bucket_hash).unwrap();
        assert!(
            result.is_none(),
            "Should return None for page size mismatch"
        );

        // Index file should be deleted
        let index_path = index_path_for_bucket(&bucket_path);
        assert!(
            !index_path.exists(),
            "Mismatched index file should be deleted"
        );
    }

    #[test]
    fn test_delete_index() {
        let temp_dir = tempdir().unwrap();
        let bucket_path = temp_dir.path().join("bucket-test.xdr");
        let index_path = index_path_for_bucket(&bucket_path);

        // Create a dummy index file
        std::fs::write(&index_path, b"dummy").unwrap();
        assert!(index_path.exists());

        // Delete it
        delete_index(&bucket_path).unwrap();
        assert!(!index_path.exists());

        // Deleting again should be a no-op
        delete_index(&bucket_path).unwrap();
    }

    #[test]
    fn test_cleanup_orphaned_indexes() {
        let temp_dir = tempdir().unwrap();

        // Create some bucket files and their indexes
        let bucket1 = temp_dir.path().join("bucket-aaa.xdr");
        let bucket2 = temp_dir.path().join("bucket-bbb.xdr");
        std::fs::write(&bucket1, b"bucket1").unwrap();
        std::fs::write(&bucket2, b"bucket2").unwrap();
        std::fs::write(index_path_for_bucket(&bucket1), b"index1").unwrap();
        std::fs::write(index_path_for_bucket(&bucket2), b"index2").unwrap();

        // Create an orphaned index (no bucket file)
        let orphaned_index = temp_dir.path().join("bucket-orphan.index");
        std::fs::write(&orphaned_index, b"orphan").unwrap();

        // Cleanup
        let removed = cleanup_orphaned_indexes(temp_dir.path()).unwrap();
        assert_eq!(removed, 1);

        // Verify orphaned was removed but others remain
        assert!(!orphaned_index.exists());
        assert!(index_path_for_bucket(&bucket1).exists());
        assert!(index_path_for_bucket(&bucket2).exists());
    }

    // Helper to create a DiskIndex with entries
    fn make_disk_index(num_entries: u8, bloom_seed: [u8; 16], page_size: u64) -> DiskIndex {
        use crate::entry::BucketEntry;

        fn make_account_entry_for(byte: u8) -> LedgerEntry {
            LedgerEntry {
                last_modified_ledger_seq: 1,
                data: LedgerEntryData::Account(AccountEntry {
                    account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([byte; 32]))),
                    balance: 100,
                    seq_num: SequenceNumber(1),
                    num_sub_entries: 0,
                    inflation_dest: None,
                    flags: 0,
                    home_domain: String32::default(),
                    thresholds: Thresholds([1, 0, 0, 0]),
                    signers: vec![].try_into().unwrap(),
                    ext: AccountEntryExt::V0,
                }),
                ext: LedgerEntryExt::V0,
            }
        }

        let entries: Vec<(BucketEntry, u64)> = (0..num_entries)
            .map(|i| {
                (
                    BucketEntry::Liveentry(make_account_entry_for(i)),
                    i as u64 * 100,
                )
            })
            .collect();

        DiskIndex::from_entries(entries.into_iter(), bloom_seed, page_size)
    }

    #[test]
    fn test_save_load_with_bloom_filter() {
        let temp_dir = tempdir().unwrap();
        let bucket_path = temp_dir.path().join("bucket-bloom.xdr");
        let bucket_hash = create_dummy_bucket_file(&bucket_path);

        let bloom_seed = [7u8; 16];
        let page_size = 10u64;
        let original = make_disk_index(100, bloom_seed, page_size);

        // Verify original has bloom filter
        assert!(original.bloom_filter().is_some());

        // Save
        save_disk_index(&original, &bucket_path, &bucket_hash).unwrap();

        // Load
        let loaded = load_disk_index(&bucket_path, page_size, &bucket_hash)
            .unwrap()
            .expect("Should load successfully");

        // Verify bloom filter is restored
        assert!(loaded.bloom_filter().is_some());

        // Verify no false negatives: every key in the original must pass
        for i in 0..100u8 {
            let key = make_account_key(i);
            assert!(
                loaded.may_contain(&key),
                "bloom filter false negative for key {}",
                i
            );
        }
    }

    #[test]
    fn test_save_load_with_asset_pool_map() {
        use stellar_xdr::curr::{
            AlphaNum4, Asset, AssetCode4, Hash, LiquidityPoolConstantProductParameters,
            LiquidityPoolEntryBody, LiquidityPoolEntryConstantProduct,
        };

        let temp_dir = tempdir().unwrap();
        let bucket_path = temp_dir.path().join("bucket-pool.xdr");
        let bucket_hash = create_dummy_bucket_file(&bucket_path);

        // Build a DiskIndex that includes liquidity pool entries
        // We'll use from_entries with pool entries to populate the map
        let pool_id = PoolId(Hash([1u8; 32]));
        let asset_a = Asset::Native;
        let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([99u8; 32]))),
        });

        // Create entries with a liquidity pool
        let pool_entry = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::LiquidityPool(stellar_xdr::curr::LiquidityPoolEntry {
                liquidity_pool_id: pool_id.clone(),
                body: LiquidityPoolEntryBody::LiquidityPoolConstantProduct(
                    LiquidityPoolEntryConstantProduct {
                        params: LiquidityPoolConstantProductParameters {
                            asset_a: asset_a.clone(),
                            asset_b: asset_b.clone(),
                            fee: LIQUIDITY_POOL_FEE_V18,
                        },
                        reserve_a: 1000,
                        reserve_b: 2000,
                        total_pool_shares: 1000,
                        pool_shares_trust_line_count: 1,
                    },
                ),
            }),
            ext: LedgerEntryExt::V0,
        };

        // Build a small account entry first, then the pool entry
        let account_entry = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32]))),
                balance: 100,
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: vec![].try_into().unwrap(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        };

        use crate::entry::BucketEntry;

        let entries = vec![
            (BucketEntry::Liveentry(account_entry), 0u64),
            (BucketEntry::Liveentry(pool_entry), 100u64),
        ];

        let bloom_seed = [0u8; 16];
        let page_size = 10u64;
        let original = DiskIndex::from_entries(entries.into_iter(), bloom_seed, page_size);

        // Verify original has pool data
        let pools_native = original.asset_to_pool_id().get_pools_for_asset(&asset_a);
        assert_eq!(pools_native.len(), 1);
        assert_eq!(pools_native[0], pool_id);

        // Save
        save_disk_index(&original, &bucket_path, &bucket_hash).unwrap();

        // Load
        let loaded = load_disk_index(&bucket_path, page_size, &bucket_hash)
            .unwrap()
            .expect("Should load successfully");

        // Verify pool map is restored
        let loaded_pools_native = loaded.asset_to_pool_id().get_pools_for_asset(&asset_a);
        assert_eq!(loaded_pools_native.len(), 1);
        assert_eq!(loaded_pools_native[0], pool_id);

        let loaded_pools_usd = loaded.asset_to_pool_id().get_pools_for_asset(&asset_b);
        assert_eq!(loaded_pools_usd.len(), 1);
        assert_eq!(loaded_pools_usd[0], pool_id);
    }

    #[test]
    fn test_load_rejects_legacy_header_and_deletes_index() {
        let temp_dir = tempdir().unwrap();
        let bucket_path = temp_dir.path().join("bucket-v1.xdr");
        let bucket_hash = create_dummy_bucket_file(&bucket_path);
        let index_path = index_path_for_bucket(&bucket_path);

        // Manually write an index file with an old version header.
        // The old format lacks bucket_hash/bucket_file_size fields, so
        // deserialization of the full header will fail gracefully.
        {
            let file = File::create(&index_path).unwrap();
            let mut writer = BufWriter::new(file);
            // Write just version + page_size (old format)
            bincode::serialize_into(&mut writer, &1u32).unwrap(); // version
            bincode::serialize_into(&mut writer, &10u64).unwrap(); // page_size
            writer.flush().unwrap();
        }

        assert!(index_path.exists());

        // Try to load — should return None due to header deserialization failure
        // (old format lacks bucket_hash and bucket_file_size fields)
        let result = load_disk_index(&bucket_path, 10, &bucket_hash).unwrap();
        assert!(result.is_none(), "Old format index should be rejected");

        // Stale index file should be deleted
        assert!(
            !index_path.exists(),
            "Malformed header index should be deleted"
        );
    }

    #[test]
    fn test_load_rejects_truncated_header_and_deletes_index() {
        let temp_dir = tempdir().unwrap();
        let bucket_path = temp_dir.path().join("bucket-truncated.xdr");
        let bucket_hash = create_dummy_bucket_file(&bucket_path);
        let index_path = index_path_for_bucket(&bucket_path);

        // Write a truncated 2-byte file that cannot deserialize into IndexHeader
        std::fs::write(&index_path, &[0xDE, 0xAD]).unwrap();
        assert!(index_path.exists());

        let result = load_disk_index(&bucket_path, 10, &bucket_hash).unwrap();
        assert!(
            result.is_none(),
            "Truncated header index should be rejected"
        );

        // Stale index file should be deleted
        assert!(
            !index_path.exists(),
            "Truncated header index should be deleted"
        );
    }

    #[test]
    fn test_load_rejects_wrong_bucket_hash() {
        let temp_dir = tempdir().unwrap();
        let bucket_path = temp_dir.path().join("bucket-hash.xdr");
        let bucket_hash = create_dummy_bucket_file(&bucket_path);

        let page_size = 10u64;
        let original = make_disk_index(20, [0u8; 16], page_size);
        save_disk_index(&original, &bucket_path, &bucket_hash).unwrap();

        // Load with a different expected hash — should be rejected
        let wrong_hash = Hash256::hash(b"wrong data");
        let result = load_disk_index(&bucket_path, page_size, &wrong_hash).unwrap();
        assert!(
            result.is_none(),
            "Should reject index with wrong bucket hash"
        );

        // Index file should be deleted
        let index_path = index_path_for_bucket(&bucket_path);
        assert!(
            !index_path.exists(),
            "Mismatched hash index should be deleted"
        );
    }

    #[test]
    fn test_load_rejects_wrong_file_size() {
        let temp_dir = tempdir().unwrap();
        let bucket_path = temp_dir.path().join("bucket-size.xdr");
        let bucket_hash = create_dummy_bucket_file(&bucket_path);

        let page_size = 10u64;
        let original = make_disk_index(20, [0u8; 16], page_size);
        save_disk_index(&original, &bucket_path, &bucket_hash).unwrap();

        // Change the bucket file size (append data)
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&bucket_path)
                .unwrap();
            f.write_all(b"extra data").unwrap();
        }

        // Load should reject because file size changed
        let result = load_disk_index(&bucket_path, page_size, &bucket_hash).unwrap();
        assert!(
            result.is_none(),
            "Should reject index with changed file size"
        );

        // Index file should be deleted
        let index_path = index_path_for_bucket(&bucket_path);
        assert!(
            !index_path.exists(),
            "Mismatched file-size index should be deleted"
        );
    }

    #[test]
    fn test_load_rejects_corrupted_data() {
        let temp_dir = tempdir().unwrap();
        let bucket_path = temp_dir.path().join("bucket-corrupt.xdr");
        let bucket_hash = create_dummy_bucket_file(&bucket_path);

        let page_size = 10u64;
        let original = make_disk_index(20, [0u8; 16], page_size);
        save_disk_index(&original, &bucket_path, &bucket_hash).unwrap();

        // Corrupt the index file data section (flip bits after header + checksum)
        let index_path = index_path_for_bucket(&bucket_path);
        let mut bytes = std::fs::read(&index_path).unwrap();
        // Corrupt bytes near the end of the file (in the data section)
        let len = bytes.len();
        if len > 10 {
            bytes[len - 5] ^= 0xFF;
            bytes[len - 3] ^= 0xFF;
        }
        std::fs::write(&index_path, &bytes).unwrap();

        // Load should reject because data checksum doesn't match
        let result = load_disk_index(&bucket_path, page_size, &bucket_hash).unwrap();
        assert!(result.is_none(), "Should reject index with corrupted data");

        // Index file should be deleted
        let index_path = index_path_for_bucket(&bucket_path);
        assert!(
            !index_path.exists(),
            "Corrupted index file should be deleted"
        );
    }
}
