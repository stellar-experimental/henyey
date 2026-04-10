//! CDP Data Lake client for fetching LedgerCloseMeta.
//!
//! This module implements [SEP-0054] for reading ledger metadata from
//! Stellar's Composable Data Platform (CDP) data lakes stored in cloud
//! object storage (S3, GCS, etc.).
//!
//! # Overview
//!
//! The CDP provides streaming access to `LedgerCloseMeta` - comprehensive
//! metadata about each closed ledger including:
//!
//! - Transaction envelopes and results in execution order
//! - Detailed ledger entry changes (TransactionMeta)
//! - Evicted keys and upgrade metadata
//!
//! This is more detailed than what traditional history archives provide,
//! making it useful for:
//!
//! - Indexers that need full transaction metadata
//! - Analytics pipelines processing ledger changes
//! - Replay with complete TransactionMeta verification
//!
//! # Data Organization
//!
//! CDP data is organized by date partition and ledger range:
//!
//! ```text
//! {base_url}/{date}/
//!   {inverted_start}--{start}-{end}/  # Partition (64000 ledgers)
//!     {inverted_seq}--{seq}.xdr.zst   # Single ledger (zstd compressed)
//! ```
//!
//! The inverted prefix ensures lexicographic ordering matches chronological
//! ordering when listing objects in descending order.
//!
//! # Example
//!
//! ```no_run
//! use henyey_history::cdp::CdpDataLake;
//!
//! # async fn example() -> Result<(), henyey_history::HistoryError> {
//! let cdp = CdpDataLake::new(
//!     "https://aws-public-blockchain.s3.us-east-2.amazonaws.com/v1.1/stellar/ledgers/testnet",
//!     "2025-01-07",
//! );
//!
//! let meta = cdp.fetch_ledger_close_meta(310079).await?;
//! let header = henyey_history::cdp::extract_ledger_header(&meta);
//! println!("Ledger {} closed at {}", header.ledger_seq, header.scp_value.close_time.0);
//! # Ok(())
//! # }
//! ```
//!
//! [SEP-0054]: https://github.com/stellar/stellar-protocol/blob/master/ecosystem/sep-0054.md

use crate::{HistoryError, Result};
use std::io::Read;
use stellar_xdr::curr::{Hash, LedgerCloseMeta, Limits, ReadXdr, WriteXdr};

/// CDP data lake client for fetching `LedgerCloseMeta` from cloud object storage.
///
/// This client fetches ledger metadata from S3-compatible storage following the
/// SEP-0054 specification. Each ledger's metadata is stored as a zstd-compressed
/// XDR file containing a `LedgerCloseMetaBatch`.
#[derive(Debug, Clone)]
pub struct CdpDataLake {
    /// Base URL for the data lake.
    ///
    /// Example: `https://aws-public-blockchain.s3.us-east-2.amazonaws.com/v1.1/stellar/ledgers/testnet`
    base_url: String,

    /// HTTP client for fetching data.
    client: reqwest::Client,

    /// Date partition to query (format: `YYYY-MM-DD`).
    ///
    /// CDP data is partitioned by date for efficient querying and data lifecycle management.
    date_partition: String,
}

/// Number of ledgers per CDP partition directory.
const CDP_PARTITION_SIZE: u32 = 64_000;

impl CdpDataLake {
    /// Create a new CDP data lake client.
    ///
    /// # Arguments
    /// * `base_url` - Base URL for the data lake (e.g., `https://aws-public-blockchain.s3.us-east-2.amazonaws.com/v1.1/stellar/ledgers/testnet`)
    /// * `date_partition` - Date partition to use (e.g., "2025-12-18")
    pub fn new(base_url: &str, date_partition: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
            date_partition: date_partition.to_string(),
        }
    }

    /// Calculate the partition directory name for a ledger sequence.
    ///
    /// Partitions are [`CDP_PARTITION_SIZE`] ledgers each.
    /// Format: `{inverted_start}--{start}-{end}/`
    fn partition_for_ledger(&self, ledger_seq: u32) -> String {
        let partition_size = CDP_PARTITION_SIZE;
        let partition_start = (ledger_seq / partition_size) * partition_size;
        let partition_end = partition_start + partition_size - 1;
        let inverted = u32::MAX - partition_start;
        format!("{:08X}--{}-{}", inverted, partition_start, partition_end)
    }

    /// Calculate the batch file name for a ledger sequence.
    ///
    /// For single-ledger batches: `{inverted}--{ledger}.xdr.zst`
    fn batch_filename(&self, ledger_seq: u32) -> String {
        let inverted = u32::MAX - ledger_seq;
        format!("{:08X}--{}.xdr.zst", inverted, ledger_seq)
    }

    /// Build the full URL for a ledger's metadata file.
    fn url_for_ledger(&self, ledger_seq: u32) -> String {
        let partition = self.partition_for_ledger(ledger_seq);
        let filename = self.batch_filename(ledger_seq);
        if self.date_partition.is_empty() {
            format!("{}/{}/{}", self.base_url, partition, filename)
        } else {
            format!(
                "{}/{}/{}/{}",
                self.base_url, self.date_partition, partition, filename
            )
        }
    }

    /// Fetch LedgerCloseMeta for a single ledger.
    pub async fn fetch_ledger_close_meta(&self, ledger_seq: u32) -> Result<LedgerCloseMeta> {
        let url = self.url_for_ledger(ledger_seq);
        tracing::debug!(ledger_seq = ledger_seq, url = %url, "Fetching LedgerCloseMeta from CDP");

        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| HistoryError::DownloadFailed(format!("CDP fetch failed: {}", e)))?;

        if !response.status().is_success() {
            return Err(HistoryError::HttpStatus {
                url: url.clone(),
                status: response.status().as_u16(),
            });
        }

        let compressed_data: bytes::Bytes = response
            .bytes()
            .await
            .map_err(|e| HistoryError::DownloadFailed(format!("CDP read failed: {}", e)))?;

        // Decompress zstd
        let decompressed = self.decompress_zstd(&compressed_data)?;

        // Parse the LedgerCloseMetaBatch XDR
        // The batch format is: startSequence (u32) + endSequence (u32) + LedgerCloseMeta[]
        self.parse_ledger_close_meta_batch(&decompressed, ledger_seq)
    }

    /// Fetch LedgerCloseMeta for a range of ledgers.
    pub async fn fetch_ledger_close_metas(
        &self,
        start_seq: u32,
        end_seq: u32,
    ) -> Result<Vec<LedgerCloseMeta>> {
        let mut metas = Vec::with_capacity((end_seq - start_seq + 1) as usize);

        for seq in start_seq..=end_seq {
            let meta = self.fetch_ledger_close_meta(seq).await?;
            metas.push(meta);
        }

        Ok(metas)
    }

    /// Decompress zstd-compressed data.
    fn decompress_zstd(&self, data: &[u8]) -> Result<Vec<u8>> {
        let mut decoder = zstd::Decoder::new(data)
            .map_err(|e| HistoryError::XdrParsing(format!("zstd init failed: {}", e)))?;

        let mut decompressed = Vec::new();
        decoder
            .read_to_end(&mut decompressed)
            .map_err(|e| HistoryError::XdrParsing(format!("zstd decompress failed: {}", e)))?;

        Ok(decompressed)
    }

    /// Parse LedgerCloseMetaBatch XDR and extract the LedgerCloseMeta for the requested ledger.
    ///
    /// The batch format according to SEP-0054:
    /// ```xdr
    /// struct LedgerCloseMetaBatch {
    ///     uint32 startSequence;
    ///     uint32 endSequence;
    ///     LedgerCloseMeta ledgerCloseMetas<>;
    /// }
    /// ```
    fn parse_ledger_close_meta_batch(
        &self,
        data: &[u8],
        requested_ledger: u32,
    ) -> Result<LedgerCloseMeta> {
        if data.len() < 8 {
            return Err(HistoryError::XdrParsing(
                "LedgerCloseMetaBatch too short".to_string(),
            ));
        }

        // Read startSequence and endSequence (big-endian u32)
        let start_seq = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        let end_seq = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);

        tracing::debug!(
            start_seq = start_seq,
            end_seq = end_seq,
            requested = requested_ledger,
            "Parsing LedgerCloseMetaBatch"
        );

        if requested_ledger < start_seq || requested_ledger > end_seq {
            return Err(HistoryError::XdrParsing(format!(
                "Ledger {} not in batch range [{}, {}]",
                requested_ledger, start_seq, end_seq
            )));
        }

        // Read the count of LedgerCloseMetas (XDR array length)
        if data.len() < 12 {
            return Err(HistoryError::XdrParsing(
                "LedgerCloseMetaBatch missing array length".to_string(),
            ));
        }
        let count = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);

        // For single-ledger batches, just parse the one LedgerCloseMeta
        if count == 1 && start_seq == end_seq {
            let meta = LedgerCloseMeta::from_xdr(&data[12..], Limits::none())
                .map_err(|e| HistoryError::XdrParsing(format!("XDR parse failed: {}", e)))?;
            return Ok(meta);
        }

        // For multi-ledger batches, we need to iterate to find the right one
        // This is less common since the testnet uses 1 ledger per batch
        let mut offset = 12;
        for i in 0..count {
            let current_ledger = start_seq + i;

            // Parse this LedgerCloseMeta
            let meta = LedgerCloseMeta::from_xdr(&data[offset..], Limits::none())
                .map_err(|e| HistoryError::XdrParsing(format!("XDR parse failed: {}", e)))?;

            if current_ledger == requested_ledger {
                return Ok(meta);
            }

            // Skip to next entry (need to calculate XDR size)
            // A proper implementation would calculate the exact size
            offset += meta
                .to_xdr(Limits::none())
                .map_err(|e| HistoryError::XdrParsing(format!("XDR size calc failed: {}", e)))?
                .len();
        }

        Err(HistoryError::XdrParsing(format!(
            "Ledger {} not found in batch",
            requested_ledger
        )))
    }
}

/// CDP data lake client with disk caching and parallel prefetching.
///
/// This wrapper around `CdpDataLake` adds:
/// - Disk-based caching of downloaded ledger metadata
/// - Parallel prefetching of upcoming ledgers
/// - Significant performance improvement for sequential access patterns
///
/// # Cache Structure
///
/// Cached files are stored as:
/// ```text
/// {cache_dir}/cdp/{network}/{date}/{ledger_seq}.xdr.zst
/// ```
///
/// # Prefetching
///
/// When fetching a ledger, this client can prefetch the next N ledgers
/// in parallel, reducing latency for sequential access patterns like
/// catchup and verification.
#[derive(Debug)]
pub struct CachedCdpDataLake {
    /// Inner CDP client for network requests.
    inner: CdpDataLake,
    /// Cache directory for storing downloaded metadata.
    cache_dir: std::path::PathBuf,
    /// Number of ledgers to prefetch ahead.
    prefetch_count: usize,
}

impl CachedCdpDataLake {
    /// Create a new cached CDP data lake client.
    ///
    /// # Arguments
    /// * `base_url` - Base URL for the CDP data lake
    /// * `date_partition` - Date partition (e.g., "2025-12-18")
    /// * `cache_dir` - Directory for caching downloaded metadata
    /// * `network` - Network identifier for cache organization
    pub fn new(
        base_url: &str,
        date_partition: &str,
        cache_dir: impl AsRef<std::path::Path>,
        network: &str,
    ) -> std::io::Result<Self> {
        let cache_path = if date_partition.is_empty() {
            cache_dir.as_ref().join("cdp").join(network)
        } else {
            cache_dir
                .as_ref()
                .join("cdp")
                .join(network)
                .join(date_partition)
        };
        std::fs::create_dir_all(&cache_path)?;

        Ok(Self {
            inner: CdpDataLake::new(base_url, date_partition),
            cache_dir: cache_path,
            prefetch_count: 16, // Default prefetch count
        })
    }

    /// Set the number of ledgers to prefetch.
    pub fn with_prefetch_count(mut self, count: usize) -> Self {
        self.prefetch_count = count;
        self
    }

    /// Get the cache file path for a ledger.
    fn cache_path(&self, ledger_seq: u32) -> std::path::PathBuf {
        self.cache_dir.join(format!("{}.xdr.zst", ledger_seq))
    }

    /// Check if a ledger is cached.
    pub fn is_cached(&self, ledger_seq: u32) -> bool {
        self.cache_path(ledger_seq).exists()
    }

    /// Delete a cached ledger file to free up disk space.
    ///
    /// This is useful for streaming access patterns where ledgers are processed
    /// sequentially and don't need to be retained after processing.
    ///
    /// Returns `true` if the file was deleted, `false` if it didn't exist.
    pub fn delete_cached(&self, ledger_seq: u32) -> bool {
        let path = self.cache_path(ledger_seq);
        if path.exists() {
            if let Err(e) = std::fs::remove_file(&path) {
                tracing::warn!(ledger_seq, error = %e, "Failed to delete cached CDP file");
                false
            } else {
                tracing::trace!(ledger_seq, "Deleted cached CDP file");
                true
            }
        } else {
            false
        }
    }

    /// Delete all cached ledgers in a range.
    ///
    /// Returns the number of files successfully deleted.
    pub fn delete_cached_range(&self, start: u32, end: u32) -> usize {
        (start..=end).filter(|&seq| self.delete_cached(seq)).count()
    }

    /// Get count of cached ledgers in a range.
    pub fn cached_count(&self, start: u32, end: u32) -> usize {
        (start..=end).filter(|seq| self.is_cached(*seq)).count()
    }

    /// Fetch a ledger, using cache if available.
    pub async fn fetch_ledger_close_meta(&self, ledger_seq: u32) -> Result<LedgerCloseMeta> {
        let cache_path = self.cache_path(ledger_seq);

        // Check cache first
        if cache_path.exists() {
            tracing::debug!(ledger_seq, "Loading LedgerCloseMeta from cache");
            let compressed = std::fs::read(&cache_path)?;
            return self.inner.decompress_and_parse(&compressed, ledger_seq);
        }

        // Fetch from network and cache
        self.fetch_and_cache(ledger_seq).await
    }

    /// Fetch a ledger from network and cache it.
    async fn fetch_and_cache(&self, ledger_seq: u32) -> Result<LedgerCloseMeta> {
        let url = self.inner.url_for_ledger(ledger_seq);
        tracing::debug!(ledger_seq, url = %url, "Fetching LedgerCloseMeta from CDP");

        let response = self
            .inner
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| HistoryError::DownloadFailed(format!("CDP fetch failed: {}", e)))?;

        if !response.status().is_success() {
            return Err(HistoryError::HttpStatus {
                url,
                status: response.status().as_u16(),
            });
        }

        let compressed_data: bytes::Bytes = response
            .bytes()
            .await
            .map_err(|e| HistoryError::DownloadFailed(format!("CDP read failed: {}", e)))?;

        // Cache the compressed data
        let cache_path = self.cache_path(ledger_seq);
        if let Err(e) = std::fs::write(&cache_path, &compressed_data) {
            tracing::warn!(ledger_seq, error = %e, "Failed to cache CDP data");
        }

        self.inner
            .decompress_and_parse(&compressed_data, ledger_seq)
    }

    /// Prefetch multiple ledgers in parallel.
    ///
    /// Downloads and caches ledgers that aren't already cached.
    /// Returns the number of ledgers successfully prefetched.
    pub async fn prefetch(&self, start: u32, end: u32) -> usize {
        use futures::stream::{self, StreamExt};

        let to_fetch: Vec<u32> = (start..=end).filter(|seq| !self.is_cached(*seq)).collect();

        if to_fetch.is_empty() {
            return 0;
        }

        let results: Vec<Result<()>> = stream::iter(to_fetch.iter())
            .map(|&seq| async move {
                self.fetch_and_cache(seq).await?;
                Ok(())
            })
            .buffer_unordered(self.prefetch_count)
            .collect()
            .await;

        results.iter().filter(|r| r.is_ok()).count()
    }

    /// Fetch multiple ledgers with prefetching.
    ///
    /// This method efficiently fetches a range of ledgers by:
    /// 1. Using cached data when available
    /// 2. Prefetching uncached ledgers in parallel
    /// 3. Returning results in order
    pub async fn fetch_ledger_close_metas_prefetch(
        &self,
        start: u32,
        end: u32,
    ) -> Result<Vec<LedgerCloseMeta>> {
        use futures::stream::{self, StreamExt};

        // Prefetch all uncached ledgers in parallel first
        let uncached: Vec<u32> = (start..=end).filter(|seq| !self.is_cached(*seq)).collect();

        if !uncached.is_empty() {
            let total = uncached.len();
            tracing::info!(
                "Prefetching {} ledgers ({} already cached)",
                total,
                (end - start + 1) as usize - total
            );

            let downloaded = std::sync::atomic::AtomicU32::new(0);
            let _: Vec<Result<()>> = stream::iter(uncached.into_iter())
                .map(|seq| {
                    let downloaded = &downloaded;
                    async move {
                        self.fetch_and_cache(seq).await?;
                        let count =
                            downloaded.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        if count % 10 == 0 || count == total as u32 {
                            tracing::info!("  Prefetched {}/{} ledgers", count, total);
                        }
                        Ok(())
                    }
                })
                .buffer_unordered(self.prefetch_count)
                .collect()
                .await;
        }

        // Now read all from cache (which should all be populated)
        let mut metas = Vec::with_capacity((end - start + 1) as usize);
        for seq in start..=end {
            let meta = self.fetch_ledger_close_meta(seq).await?;
            metas.push(meta);
        }

        Ok(metas)
    }

    /// Get cache statistics.
    pub fn cache_stats(&self) -> CacheStats {
        let (entries, size_bytes) = std::fs::read_dir(&self.cache_dir)
            .map(|dir| {
                dir.filter_map(|e| e.ok())
                    .fold((0usize, 0u64), |(count, size), entry| {
                        let file_size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                        (count + 1, size + file_size)
                    })
            })
            .unwrap_or((0, 0));

        CacheStats {
            entries,
            size_bytes,
            cache_dir: self.cache_dir.clone(),
        }
    }
}

/// Statistics about the CDP cache.
#[derive(Debug)]
pub struct CacheStats {
    /// Number of cached ledger entries.
    pub entries: usize,
    /// Total size of cached data in bytes.
    pub size_bytes: u64,
    /// Cache directory path.
    pub cache_dir: std::path::PathBuf,
}

impl CdpDataLake {
    /// Helper to decompress and parse cached data.
    fn decompress_and_parse(&self, compressed: &[u8], ledger_seq: u32) -> Result<LedgerCloseMeta> {
        let decompressed = self.decompress_zstd(compressed)?;
        self.parse_ledger_close_meta_batch(&decompressed, ledger_seq)
    }
}

/// Map over `tx_processing` entries in any `LedgerCloseMeta` variant.
///
/// V0/V1 have `TransactionResultMeta` while V2 has `TransactionResultMetaV1`,
/// but they share `tx_apply_processing`, `result`, and `fee_processing` fields.
macro_rules! map_tx_processing {
    ($meta:expr, $f:expr) => {
        match $meta {
            LedgerCloseMeta::V0(v0) => v0.tx_processing.iter().map($f).collect(),
            LedgerCloseMeta::V1(v1) => v1.tx_processing.iter().map($f).collect(),
            LedgerCloseMeta::V2(v2) => v2.tx_processing.iter().map($f).collect(),
        }
    };
}

/// Extract transaction metadata from a `LedgerCloseMeta`.
///
/// Returns the `TransactionMeta` for each transaction in execution order.
/// This contains the detailed ledger entry changes made by each transaction,
/// which is essential for:
///
/// - Accurate state reconstruction during replay
/// - Building change feeds for indexers
/// - Debugging transaction execution
///
/// # Note
///
/// The returned metadata is in transaction **apply order**, which may differ
/// from the order in the transaction set for protocol versions with parallel
/// execution phases.
pub fn extract_transaction_metas(
    meta: &LedgerCloseMeta,
) -> Vec<stellar_xdr::curr::TransactionMeta> {
    map_tx_processing!(meta, |tp| tp.tx_apply_processing.clone())
}

/// Complete transaction processing information in apply order.
///
/// This struct combines all the data needed to fully understand a transaction's
/// execution: the original envelope, the result, and the detailed metadata
/// showing what changed.
///
/// All fields are aligned by transaction - the envelope, result, and meta at
/// index N all correspond to the same transaction.
#[derive(Debug, Clone)]
pub struct TransactionProcessingInfo {
    /// The transaction envelope containing the transaction body and signatures.
    pub envelope: stellar_xdr::curr::TransactionEnvelope,

    /// The transaction result pair containing the hash and result code.
    pub result: stellar_xdr::curr::TransactionResultPair,

    /// The transaction metadata containing all ledger entry changes.
    ///
    /// This includes changes from both the transaction body and any
    /// Soroban contract invocations.
    pub meta: stellar_xdr::curr::TransactionMeta,

    /// Fee-related ledger entry changes.
    ///
    /// These changes are applied before the transaction body and include
    /// fee deduction from the source account.
    pub fee_meta: stellar_xdr::curr::LedgerEntryChanges,

    /// Post-transaction fee processing changes.
    ///
    /// These changes are applied after the transaction body, such as Soroban refunds.
    pub post_fee_meta: stellar_xdr::curr::LedgerEntryChanges,

    /// Per-transaction base fee from the transaction set.
    ///
    /// This may differ from `header.base_fee` during surge pricing.
    /// When `None`, use the ledger header's `base_fee`.
    pub base_fee: Option<u32>,
}

/// A transaction envelope paired with its per-component base fee from a `GeneralizedTransactionSet`.
///
/// The base fee may differ from `ledger_header.base_fee` during surge pricing.
/// `None` means "use the ledger header's base_fee".
struct TxWithBaseFee<'a> {
    env: &'a stellar_xdr::curr::TransactionEnvelope,
    base_fee: Option<u32>,
}

/// Visit every transaction in a `GeneralizedTransactionSet`, yielding each
/// envelope paired with its per-component base fee.
///
/// This centralizes the deeply nested traversal of the GeneralizedTransactionSet
/// structure (phases -> V0 components / V1 parallel stages -> clusters -> txs)
/// so that consumers can operate on a flat iterator.
fn visit_generalized_tx_set(
    tx_set: &stellar_xdr::curr::GeneralizedTransactionSet,
) -> Vec<TxWithBaseFee<'_>> {
    let stellar_xdr::curr::GeneralizedTransactionSet::V1(v1) = tx_set;
    let mut result = Vec::new();

    for phase in v1.phases.iter() {
        match phase {
            stellar_xdr::curr::TransactionPhase::V0(components) => {
                for comp in components.iter() {
                    let stellar_xdr::curr::TxSetComponent::TxsetCompTxsMaybeDiscountedFee(c) = comp;
                    let base_fee = c.base_fee.and_then(|fee| u32::try_from(fee).ok());
                    for env in c.txs.iter() {
                        result.push(TxWithBaseFee { env, base_fee });
                    }
                }
            }
            stellar_xdr::curr::TransactionPhase::V1(parallel) => {
                let base_fee = parallel.base_fee.and_then(|fee| u32::try_from(fee).ok());
                for stage in parallel.execution_stages.iter() {
                    for cluster in stage.iter() {
                        for env in cluster.0.iter() {
                            result.push(TxWithBaseFee { env, base_fee });
                        }
                    }
                }
            }
        }
    }

    result
}

/// Helper to build a map from transaction hash to per-component base fee.
///
/// The `GeneralizedTransactionSet` can contain per-phase/per-component base fees
/// that differ from `header.base_fee` during surge pricing.
fn build_tx_hash_to_base_fee_map(
    tx_set: &stellar_xdr::curr::GeneralizedTransactionSet,
    network_id: &Hash,
) -> std::collections::HashMap<Hash, Option<u32>> {
    visit_generalized_tx_set(tx_set)
        .into_iter()
        .filter_map(|t| {
            let hash = compute_tx_hash(t.env, network_id)?;
            Some((hash, t.base_fee))
        })
        .collect()
}

/// Compute the network-aware transaction hash.
fn compute_tx_hash(
    env: &stellar_xdr::curr::TransactionEnvelope,
    network_id: &Hash,
) -> Option<Hash> {
    use sha2::{Digest, Sha256};
    use stellar_xdr::curr::{EnvelopeType, Limits, WriteXdr};

    let mut hasher = Sha256::new();
    hasher.update(network_id);

    let envelope_type = match env {
        stellar_xdr::curr::TransactionEnvelope::TxV0(_) => EnvelopeType::TxV0,
        stellar_xdr::curr::TransactionEnvelope::Tx(_) => EnvelopeType::Tx,
        stellar_xdr::curr::TransactionEnvelope::TxFeeBump(_) => EnvelopeType::TxFeeBump,
    };
    hasher.update((envelope_type as i32).to_be_bytes());

    match env {
        stellar_xdr::curr::TransactionEnvelope::TxV0(tx_v0) => {
            let tx_xdr = tx_v0.tx.to_xdr(Limits::none()).ok()?;
            hasher.update(&tx_xdr);
        }
        stellar_xdr::curr::TransactionEnvelope::Tx(tx_v1) => {
            let tx_xdr = tx_v1.tx.to_xdr(Limits::none()).ok()?;
            hasher.update(&tx_xdr);
        }
        stellar_xdr::curr::TransactionEnvelope::TxFeeBump(tx_bump) => {
            let tx_xdr = tx_bump.tx.to_xdr(Limits::none()).ok()?;
            hasher.update(&tx_xdr);
        }
    }

    let result: [u8; 32] = hasher.finalize().into();
    Some(Hash(result))
}

/// Common fields extracted from V1/V2 `TransactionResultMeta` variants.
struct TxProcessingFields {
    tx_hash: Hash,
    result: stellar_xdr::curr::TransactionResultPair,
    meta: stellar_xdr::curr::TransactionMeta,
    fee_meta: stellar_xdr::curr::LedgerEntryChanges,
    post_fee_meta: stellar_xdr::curr::LedgerEntryChanges,
}

/// Shared extraction logic for V1/V2 `LedgerCloseMeta` (generalized tx set).
fn extract_generalized_tx_processing(
    tx_set: &stellar_xdr::curr::GeneralizedTransactionSet,
    processing: impl Iterator<Item = TxProcessingFields>,
    network_id: &Hash,
    version_label: &str,
) -> Vec<TransactionProcessingInfo> {
    let txs = extract_txs_from_generalized_set(tx_set);
    let tx_map = build_tx_hash_map_with_network(&txs, network_id);
    let base_fee_map = build_tx_hash_to_base_fee_map(tx_set, network_id);
    let mut processing_count = 0usize;
    let result: Vec<_> = processing
        .inspect(|_| processing_count += 1)
        .filter_map(|fields| {
            tx_map
                .get(&fields.tx_hash)
                .map(|env| TransactionProcessingInfo {
                    envelope: env.clone(),
                    result: fields.result,
                    meta: fields.meta,
                    fee_meta: fields.fee_meta,
                    post_fee_meta: fields.post_fee_meta,
                    base_fee: base_fee_map.get(&fields.tx_hash).copied().flatten(),
                })
        })
        .collect();
    if result.len() != processing_count {
        tracing::warn!(
            processing_count = processing_count,
            result_count = result.len(),
            txs_in_set = txs.len(),
            "Some transactions were not matched in {} LedgerCloseMeta",
            version_label,
        );
    }
    result
}

/// Extract complete transaction processing info in apply order.
///
/// This function aligns transaction envelopes with their results and metadata,
/// ensuring all data for a given transaction is grouped together. This is more
/// complex than it sounds because:
///
/// 1. Transaction sets may be ordered differently than apply order
/// 2. Generalized transaction sets (protocol 20+) use phases
/// 3. Transaction hashes require network-aware computation
///
/// # Arguments
///
/// * `meta` - The `LedgerCloseMeta` to extract from
/// * `network_id` - The 32-byte network ID for hash computation
///
/// # Returns
///
/// A vector of [`TransactionProcessingInfo`] in transaction apply order.
/// If some transactions cannot be matched (e.g., hash mismatch), a warning
/// is logged and those transactions are omitted.
pub fn extract_transaction_processing(
    meta: &LedgerCloseMeta,
    network_id: &Hash,
) -> Vec<TransactionProcessingInfo> {
    match meta {
        LedgerCloseMeta::V0(v0) => {
            // V0 has a simpler structure - tx_set.txs and tx_processing should align
            // V0 doesn't have per-component base fees, use header's base_fee
            let txs = &v0.tx_set.txs;
            let processing_count = v0.tx_processing.len();
            let result: Vec<_> = v0
                .tx_processing
                .iter()
                .enumerate()
                .filter_map(|(i, tp)| {
                    txs.get(i).map(|env| TransactionProcessingInfo {
                        envelope: env.clone(),
                        result: tp.result.clone(),
                        meta: tp.tx_apply_processing.clone(),
                        fee_meta: tp.fee_processing.clone(),
                        post_fee_meta: stellar_xdr::curr::LedgerEntryChanges::default(),
                        base_fee: None, // V0 doesn't have per-tx base fees
                    })
                })
                .collect();
            if result.len() != processing_count {
                tracing::warn!(
                    processing_count = processing_count,
                    result_count = result.len(),
                    "Some transactions were not matched in V0 LedgerCloseMeta"
                );
            }
            result
        }
        LedgerCloseMeta::V1(v1) => extract_generalized_tx_processing(
            &v1.tx_set,
            v1.tx_processing.iter().map(|tp| TxProcessingFields {
                tx_hash: tp.result.transaction_hash.clone(),
                result: tp.result.clone(),
                meta: tp.tx_apply_processing.clone(),
                fee_meta: tp.fee_processing.clone(),
                post_fee_meta: stellar_xdr::curr::LedgerEntryChanges::default(),
            }),
            network_id,
            "V1",
        ),
        LedgerCloseMeta::V2(v2) => extract_generalized_tx_processing(
            &v2.tx_set,
            v2.tx_processing.iter().map(|tp| TxProcessingFields {
                tx_hash: tp.result.transaction_hash.clone(),
                result: tp.result.clone(),
                meta: tp.tx_apply_processing.clone(),
                fee_meta: tp.fee_processing.clone(),
                post_fee_meta: tp.post_tx_apply_fee_processing.clone(),
            }),
            network_id,
            "V2",
        ),
    }
}

/// Build a map from transaction hash to envelope for matching.
/// This version uses the network-aware hash (network_id || ENVELOPE_TYPE || tx).
pub fn build_tx_hash_map_with_network(
    txs: &[stellar_xdr::curr::TransactionEnvelope],
    network_id: &Hash,
) -> std::collections::HashMap<Hash, stellar_xdr::curr::TransactionEnvelope> {
    txs.iter()
        .filter_map(|env| {
            let hash = compute_tx_hash(env, network_id)?;
            Some((hash, env.clone()))
        })
        .collect()
}

/// Extract ledger header from LedgerCloseMeta.
pub fn extract_ledger_header(meta: &LedgerCloseMeta) -> stellar_xdr::curr::LedgerHeader {
    match meta {
        LedgerCloseMeta::V0(v0) => v0.ledger_header.header.clone(),
        LedgerCloseMeta::V1(v1) => v1.ledger_header.header.clone(),
        LedgerCloseMeta::V2(v2) => v2.ledger_header.header.clone(),
    }
}

/// Extract transaction envelopes from LedgerCloseMeta.
pub fn extract_transaction_envelopes(
    meta: &LedgerCloseMeta,
) -> Vec<stellar_xdr::curr::TransactionEnvelope> {
    match meta {
        LedgerCloseMeta::V0(v0) => v0.tx_set.txs.to_vec(),
        LedgerCloseMeta::V1(v1) => extract_txs_from_generalized_set(&v1.tx_set),
        LedgerCloseMeta::V2(v2) => extract_txs_from_generalized_set(&v2.tx_set),
    }
}

/// Helper to extract transactions from a GeneralizedTransactionSet.
fn extract_txs_from_generalized_set(
    tx_set: &stellar_xdr::curr::GeneralizedTransactionSet,
) -> Vec<stellar_xdr::curr::TransactionEnvelope> {
    visit_generalized_tx_set(tx_set)
        .into_iter()
        .map(|t| t.env.clone())
        .collect()
}

/// Extract transaction result pairs from LedgerCloseMeta.
pub fn extract_transaction_results(
    meta: &LedgerCloseMeta,
) -> Vec<stellar_xdr::curr::TransactionResultPair> {
    map_tx_processing!(meta, |tp| tp.result.clone())
}

/// Extract the transaction set from LedgerCloseMeta.
///
/// Returns either a Classic TransactionSet (V0) or Generalized set (V1/V2).
/// This is useful for passing to `close_ledger` when replaying from CDP.
pub fn extract_transaction_set(meta: &LedgerCloseMeta) -> henyey_ledger::TransactionSetVariant {
    use henyey_ledger::TransactionSetVariant;
    match meta {
        LedgerCloseMeta::V0(v0) => TransactionSetVariant::Classic(v0.tx_set.clone()),
        LedgerCloseMeta::V1(v1) => TransactionSetVariant::Generalized(v1.tx_set.clone()),
        LedgerCloseMeta::V2(v2) => TransactionSetVariant::Generalized(v2.tx_set.clone()),
    }
}

/// Create `LedgerCloseData` from `LedgerCloseMeta` for replay.
///
/// This extracts all the information needed to call `close_ledger`:
/// - Transaction set
/// - Close time
/// - Previous ledger hash
/// - Protocol upgrades
/// - StellarValue extension (Basic or Signed)
///
/// # Arguments
/// * `meta` - The LedgerCloseMeta from CDP
/// * `prev_ledger_hash` - Hash of the previous ledger (from history archive headers)
///
/// # Example
/// ```ignore
/// let close_data = extract_ledger_close_data(&cdp_meta, prev_hash);
/// let result = ledger_manager.close_ledger(close_data)?;
/// ```
pub fn extract_ledger_close_data(
    meta: &LedgerCloseMeta,
    prev_ledger_hash: henyey_common::Hash256,
) -> henyey_ledger::LedgerCloseData {
    use henyey_ledger::LedgerCloseData;
    use stellar_xdr::curr::{LedgerUpgrade, Limits, ReadXdr};

    let header = extract_ledger_header(meta);
    let tx_set = extract_transaction_set(meta);

    // Decode upgrades from raw bytes
    let upgrades: Vec<LedgerUpgrade> = header
        .scp_value
        .upgrades
        .iter()
        .filter_map(|upgrade_bytes| {
            LedgerUpgrade::from_xdr(upgrade_bytes.0.as_slice(), Limits::none()).ok()
        })
        .collect();

    LedgerCloseData::new(
        header.ledger_seq.into(),
        tx_set,
        header.scp_value.close_time.0,
        prev_ledger_hash,
    )
    .with_upgrades(upgrades)
    .with_stellar_value_ext(header.scp_value.ext.clone())
}

/// Extract evicted ledger keys from LedgerCloseMeta (V2 only).
/// These are entries that were evicted from the live bucket list.
pub fn extract_evicted_keys(meta: &LedgerCloseMeta) -> Vec<stellar_xdr::curr::LedgerKey> {
    match meta {
        LedgerCloseMeta::V0(_) | LedgerCloseMeta::V1(_) => Vec::new(),
        LedgerCloseMeta::V2(v2) => v2.evicted_keys.to_vec(),
    }
}

/// Extract restored ledger keys from transaction metadata.
/// These are entries that were restored from the hot archive back to the live bucket list.
/// For the hot archive, these become "Live" markers indicating the entry was restored.
///
/// **Important**: Only CONTRACT_DATA and CONTRACT_CODE keys are recorded in the hot archive
/// bucket list. TTL keys are NOT included, matching stellar-core behavior from LedgerManagerImpl.cpp:
/// ```cpp
/// // TTL keys are not recorded in the hot archive BucketList
/// if (key.type() == CONTRACT_DATA || key.type() == CONTRACT_CODE)
/// {
///     restoredHotArchiveKeys.push_back(key);
/// }
/// ```
pub fn extract_restored_keys(
    tx_metas: &[stellar_xdr::curr::TransactionMeta],
) -> Vec<stellar_xdr::curr::LedgerKey> {
    use stellar_xdr::curr::{LedgerEntryChange, LedgerKey, TransactionMeta};

    let mut restored_keys = Vec::new();

    fn process_change(
        change: &LedgerEntryChange,
        restored_keys: &mut Vec<stellar_xdr::curr::LedgerKey>,
    ) {
        if let LedgerEntryChange::Restored(entry) = change {
            let key = henyey_common::entry_to_key(entry);
            // Only CONTRACT_DATA and CONTRACT_CODE keys go to hot archive.
            // TTL keys are NOT recorded in the hot archive bucket list,
            // matching stellar-core behavior in LedgerManagerImpl.cpp.
            match &key {
                LedgerKey::ContractData(_) | LedgerKey::ContractCode(_) => {
                    restored_keys.push(key);
                }
                _ => {
                    // Skip TTL keys and any other key types
                }
            }
        }
    }

    /// Process tx_changes_before, operations, and tx_changes_after for V2+.
    macro_rules! process_v2_plus {
        ($v:expr, $out:expr) => {{
            for change in $v.tx_changes_before.iter() {
                process_change(change, $out);
            }
            for op_changes in $v.operations.iter() {
                for change in op_changes.changes.iter() {
                    process_change(change, $out);
                }
            }
            for change in $v.tx_changes_after.iter() {
                process_change(change, $out);
            }
        }};
    }

    for meta in tx_metas {
        match meta {
            TransactionMeta::V0(operations) => {
                for op_meta in operations.iter() {
                    for change in op_meta.changes.iter() {
                        process_change(change, &mut restored_keys);
                    }
                }
            }
            TransactionMeta::V1(v1) => {
                for change in v1.tx_changes.iter() {
                    process_change(change, &mut restored_keys);
                }
                for op_changes in v1.operations.iter() {
                    for change in op_changes.changes.iter() {
                        process_change(change, &mut restored_keys);
                    }
                }
            }
            TransactionMeta::V2(v) => process_v2_plus!(v, &mut restored_keys),
            TransactionMeta::V3(v) => process_v2_plus!(v, &mut restored_keys),
            TransactionMeta::V4(v) => process_v2_plus!(v, &mut restored_keys),
        }
    }

    restored_keys
}

/// Extract upgrade changes from LedgerCloseMeta.
/// These are ledger entry changes from protocol upgrades (not from transactions).
pub fn extract_upgrade_metas(meta: &LedgerCloseMeta) -> Vec<stellar_xdr::curr::UpgradeEntryMeta> {
    match meta {
        LedgerCloseMeta::V0(v0) => v0.upgrades_processing.to_vec(),
        LedgerCloseMeta::V1(v1) => v1.upgrades_processing.to_vec(),
        LedgerCloseMeta::V2(v2) => v2.upgrades_processing.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_partition_calculation() {
        let cdp = CdpDataLake::new("https://example.com/stellar/ledgers/testnet", "2025-12-18");

        // Ledger 310079 should be in partition 256000-319999
        assert_eq!(
            cdp.partition_for_ledger(310079u32),
            "FFFC17FF--256000-319999"
        );

        // Ledger 0 should be in partition 0-63999
        assert_eq!(cdp.partition_for_ledger(0u32), "FFFFFFFF--0-63999");

        // Ledger 64000 should be in partition 64000-127999
        assert_eq!(cdp.partition_for_ledger(64000u32), "FFFF05FF--64000-127999");
    }

    #[test]
    fn test_batch_filename() {
        let cdp = CdpDataLake::new("https://example.com/stellar/ledgers/testnet", "2025-12-18");

        // Ledger 310079 -> inverted = 0xFFFB44C0
        assert_eq!(cdp.batch_filename(310079u32), "FFFB44C0--310079.xdr.zst");

        // Ledger 0 -> inverted = 0xFFFFFFFF
        assert_eq!(cdp.batch_filename(0u32), "FFFFFFFF--0.xdr.zst");
    }

    #[test]
    fn test_url_construction() {
        let cdp = CdpDataLake::new(
            "https://aws-public-blockchain.s3.us-east-2.amazonaws.com/v1.1/stellar/ledgers/testnet",
            "2025-12-18",
        );

        let url = cdp.url_for_ledger(310079u32);
        assert_eq!(
            url,
            "https://aws-public-blockchain.s3.us-east-2.amazonaws.com/v1.1/stellar/ledgers/testnet/2025-12-18/FFFC17FF--256000-319999/FFFB44C0--310079.xdr.zst"
        );
    }
}
