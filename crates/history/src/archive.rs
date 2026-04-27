//! History Archive client for accessing Stellar history archives.
//!
//! This module provides the main client for interacting with Stellar history
//! archives, supporting operations like fetching the archive state, downloading
//! ledger headers, transactions, and buckets.

use henyey_common::Hash256;
use reqwest::Client;
use stellar_xdr::curr::{
    LedgerHeaderHistoryEntry, ScpHistoryEntry, TransactionHistoryEntry,
    TransactionHistoryResultEntry,
};
use tracing::debug;
use url::Url;

use crate::archive_state::HistoryArchiveState;
use crate::download::{
    create_client, decompress_gzip, download_with_retries, parse_record_marked_xdr_stream,
    DownloadConfig,
};
use crate::error::HistoryError;
use crate::paths::{bucket_path, checkpoint_path, root_has_path};
use crate::verify;

/// Client for accessing a Stellar history archive.
///
/// A history archive contains checkpoints of the Stellar network state,
/// including ledger headers, transactions, transaction results, and bucket files.
///
/// # Example
///
/// ```no_run
/// use henyey_history::archive::HistoryArchive;
///
/// # async fn example() -> Result<(), henyey_history::error::HistoryError> {
/// let archive = HistoryArchive::new("https://history.stellar.org/prd/core-testnet/core_testnet_001")?;
///
/// // Get the current archive state
/// let has = archive.fetch_root_has().await?;
/// println!("Current ledger: {}", has.current_ledger());
///
/// // Get ledger headers for a specific checkpoint
/// let headers = archive.fetch_ledger_headers(63).await?;
/// println!("Got {} ledger headers", headers.len());
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct HistoryArchive {
    /// Base URL of the archive.
    base_url: Url,
    /// HTTP client for requests.
    client: Client,
    /// Download configuration.
    config: DownloadConfig,
}

impl HistoryArchive {
    /// Create a new history archive client.
    ///
    /// # Arguments
    ///
    /// * `base_url` - The base URL of the history archive
    ///
    /// # Returns
    ///
    /// A new `HistoryArchive` client, or an error if the URL is invalid
    /// or the HTTP client cannot be created.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use henyey_history::archive::HistoryArchive;
    ///
    /// let archive = HistoryArchive::new("https://history.stellar.org/prd/core-testnet/core_testnet_001")?;
    /// # Ok::<(), henyey_history::error::HistoryError>(())
    /// ```
    pub fn new(base_url: &str) -> Result<Self, HistoryError> {
        Self::with_config(base_url, DownloadConfig::default())
    }

    /// Create a new history archive client with custom configuration.
    ///
    /// # Arguments
    ///
    /// * `base_url` - The base URL of the history archive
    /// * `config` - Download configuration (timeouts, retries, etc.)
    ///
    /// # Returns
    ///
    /// A new `HistoryArchive` client, or an error if the URL is invalid
    /// or the HTTP client cannot be created.
    pub fn with_config(base_url: &str, config: DownloadConfig) -> Result<Self, HistoryError> {
        // Parse and normalize URL (ensure trailing slash)
        let mut url = Url::parse(base_url).map_err(HistoryError::UrlParse)?;
        if !url.path().ends_with('/') {
            url.set_path(&format!("{}/", url.path()));
        }

        let client = create_client(config.timeout)?;

        Ok(Self {
            base_url: url,
            client,
            config,
        })
    }

    /// Get the base URL of this archive.
    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    /// Fetch the root History Archive State (HAS).
    ///
    /// The root HAS is located at `.well-known/stellar-history.json` and
    /// contains the current state of the archive, including the latest
    /// checkpoint ledger and bucket hashes.
    ///
    /// # Returns
    ///
    /// The parsed `HistoryArchiveState`, or an error if fetching or parsing fails.
    pub async fn fetch_root_has(&self) -> Result<HistoryArchiveState, HistoryError> {
        let url = self.make_url(root_has_path())?;
        debug!(url = %url, "Fetching root HAS");

        let bytes = download_with_retries(&self.client, url.as_str(), &self.config).await?;
        let text = String::from_utf8(bytes.to_vec())
            .map_err(|e| HistoryError::InvalidResponse(format!("Invalid UTF-8 in HAS: {}", e)))?;

        HistoryArchiveState::from_json(&text)
    }

    /// Fetch the History Archive State for a specific checkpoint.
    ///
    /// Each checkpoint has its own HAS file that describes the state at
    /// that checkpoint ledger.
    ///
    /// # Arguments
    ///
    /// * `ledger` - The ledger sequence (will be rounded to the checkpoint)
    ///
    /// # Returns
    ///
    /// The parsed `HistoryArchiveState` for the checkpoint.
    pub async fn fetch_checkpoint_has(
        &self,
        ledger: u32,
    ) -> Result<HistoryArchiveState, HistoryError> {
        let path = checkpoint_path("history", ledger, "json");
        let url = self.make_url(&path)?;
        debug!(url = %url, ledger = ledger, "Fetching checkpoint HAS");

        let bytes = download_with_retries(&self.client, url.as_str(), &self.config).await?;
        let text = String::from_utf8(bytes.to_vec())
            .map_err(|e| HistoryError::InvalidResponse(format!("Invalid UTF-8 in HAS: {}", e)))?;

        HistoryArchiveState::from_json(&text)
    }

    /// Download ledger headers for a checkpoint.
    ///
    /// A checkpoint contains 64 ledger headers (or fewer for early checkpoints).
    /// The headers are returned in order from oldest to newest.
    ///
    /// # Arguments
    ///
    /// * `checkpoint` - The checkpoint ledger sequence (will be rounded to checkpoint)
    ///
    /// # Returns
    ///
    /// A vector of ledger header history entries.
    pub async fn fetch_ledger_headers(
        &self,
        checkpoint: u32,
    ) -> Result<Vec<LedgerHeaderHistoryEntry>, HistoryError> {
        let path = checkpoint_path("ledger", checkpoint, "xdr.gz");
        let data = self.download_xdr_gz(&path).await?;
        parse_record_marked_xdr_stream(&data).map_err(|e| {
            // Classify any XDR parsing / runtime error during header
            // deserialization as CorruptHeader, matching stellar-core's
            // VERIFY_STATUS_ERR_CORRUPT_HEADER.
            match &e {
                HistoryError::Xdr(_) | HistoryError::XdrParsing(_) => HistoryError::CorruptHeader {
                    ledger: checkpoint,
                    detail: e.to_string(),
                },
                _ => e,
            }
        })
    }

    /// Download transactions for a checkpoint.
    ///
    /// Returns all transactions included in the ledgers of this checkpoint.
    ///
    /// # Arguments
    ///
    /// * `checkpoint` - The checkpoint ledger sequence (will be rounded to checkpoint)
    ///
    /// # Returns
    ///
    /// A vector of transaction history entries.
    pub async fn fetch_transactions(
        &self,
        checkpoint: u32,
    ) -> Result<Vec<TransactionHistoryEntry>, HistoryError> {
        let path = checkpoint_path("transactions", checkpoint, "xdr.gz");
        let data = self.download_xdr_gz(&path).await?;
        parse_record_marked_xdr_stream(&data)
    }

    /// Download transaction results for a checkpoint.
    ///
    /// Returns the results of all transactions in this checkpoint.
    ///
    /// # Arguments
    ///
    /// * `checkpoint` - The checkpoint ledger sequence (will be rounded to checkpoint)
    ///
    /// # Returns
    ///
    /// A vector of transaction result history entries.
    pub async fn fetch_results(
        &self,
        checkpoint: u32,
    ) -> Result<Vec<TransactionHistoryResultEntry>, HistoryError> {
        let path = checkpoint_path("results", checkpoint, "xdr.gz");
        let data = self.download_xdr_gz(&path).await?;
        parse_record_marked_xdr_stream(&data)
    }

    /// Download SCP history for a checkpoint.
    ///
    /// Returns SCP envelopes and quorum sets for the checkpoint.
    pub async fn fetch_scp_history(
        &self,
        checkpoint: u32,
    ) -> Result<Vec<ScpHistoryEntry>, HistoryError> {
        let path = checkpoint_path("scp", checkpoint, "xdr.gz");
        let data = self.download_xdr_gz(&path).await?;
        parse_record_marked_xdr_stream(&data)
    }

    /// Download a bucket file by hash.
    ///
    /// Bucket files contain the state entries for the BucketList at a
    /// particular point in time.
    ///
    /// # Arguments
    ///
    /// * `hash` - The SHA-256 hash of the bucket file
    ///
    /// # Returns
    ///
    /// The raw (decompressed) bucket data.
    pub async fn fetch_bucket(&self, hash: &Hash256) -> Result<Vec<u8>, HistoryError> {
        // Skip zero hash (empty bucket)
        if hash.is_zero() {
            return Ok(Vec::new());
        }

        let path = bucket_path(hash);
        self.download_xdr_gz(&path).await
    }

    /// Download and decompress a gzipped XDR file.
    async fn download_xdr_gz(&self, path: &str) -> Result<Vec<u8>, HistoryError> {
        let url = self.make_url(path)?;
        debug!(url = %url, "Downloading XDR file");

        let compressed = download_with_retries(&self.client, url.as_str(), &self.config).await?;
        decompress_gzip(&compressed)
    }

    /// Build a full URL from a path.
    fn make_url(&self, path: &str) -> Result<Url, HistoryError> {
        self.base_url.join(path).map_err(HistoryError::UrlParse)
    }

    /// Check if the archive is accessible by fetching the root HAS.
    ///
    /// # Returns
    ///
    /// `Ok(())` if the archive is accessible, or an error otherwise.
    pub async fn check_accessible(&self) -> Result<(), HistoryError> {
        self.fetch_root_has().await?;
        Ok(())
    }

    /// Get the current ledger from this archive.
    ///
    /// This is a convenience method that fetches the root HAS and returns
    /// the current ledger sequence.
    pub async fn fetch_current_ledger(&self) -> Result<u32, HistoryError> {
        let has = self.fetch_root_has().await?;
        Ok(has.current_ledger())
    }

    /// Download a single ledger header by sequence.
    ///
    /// This downloads the checkpoint containing the ledger and extracts
    /// the specific header. For bulk downloads, use `fetch_ledger_headers`.
    ///
    /// # Arguments
    ///
    /// * `seq` - The ledger sequence number
    ///
    /// # Returns
    ///
    /// The ledger header for the specified sequence.
    pub async fn fetch_ledger_header(
        &self,
        seq: u32,
    ) -> Result<stellar_xdr::curr::LedgerHeader, HistoryError> {
        let (header, _hash) = self.fetch_ledger_header_with_hash(seq).await?;
        Ok(header)
    }

    /// Download a single ledger header with its pre-computed hash by sequence.
    ///
    /// This downloads the checkpoint containing the ledger and extracts the
    /// specific header along with its verified hash.
    ///
    /// # Arguments
    ///
    /// * `seq` - The ledger sequence number
    ///
    /// # Returns
    ///
    /// A tuple of (header, hash) for the specified sequence.
    pub async fn fetch_ledger_header_with_hash(
        &self,
        seq: u32,
    ) -> Result<(stellar_xdr::curr::LedgerHeader, Hash256), HistoryError> {
        let headers = self.fetch_ledger_headers(seq).await?;

        for entry in headers {
            if entry.header.ledger_seq == seq {
                let hash = verify::verify_ledger_header_history_entry(&entry)?;
                return Ok((entry.header, hash));
            }
        }

        Err(HistoryError::NotFound(format!(
            "Ledger header {} not found in checkpoint",
            seq
        )))
    }

    /// Download a transaction set for a specific ledger.
    ///
    /// This downloads the checkpoint containing the ledger and extracts
    /// the transactions for that specific ledger.
    ///
    /// # Arguments
    ///
    /// * `seq` - The ledger sequence number
    ///
    /// # Returns
    ///
    /// The transaction set for the specified ledger.
    pub async fn fetch_transaction_set(
        &self,
        seq: u32,
    ) -> Result<stellar_xdr::curr::TransactionSet, HistoryError> {
        let transactions = self.fetch_transactions(seq).await?;

        // Find the transaction set with the matching ledger sequence
        for entry in transactions {
            if entry.ledger_seq == seq {
                return Ok(entry.tx_set);
            }
        }

        // Return empty transaction set if no transactions for this ledger
        Ok(crate::make_empty_tx_set())
    }
}

/// Testnet archive URLs.
pub mod testnet {
    /// Available testnet history archive URLs.
    pub const ARCHIVE_URLS: &[&str] = &[
        "https://history.stellar.org/prd/core-testnet/core_testnet_001",
        "https://history.stellar.org/prd/core-testnet/core_testnet_002",
        "https://history.stellar.org/prd/core-testnet/core_testnet_003",
    ];

    /// Testnet network passphrase.
    pub const NETWORK_PASSPHRASE: &str = "Test SDF Network ; September 2015";
}

/// Mainnet archive URLs.
pub mod mainnet {
    /// Available mainnet history archive URLs.
    pub const ARCHIVE_URLS: &[&str] = &[
        "https://history.stellar.org/prd/core-live/core_live_001",
        "https://history.stellar.org/prd/core-live/core_live_002",
        "https://history.stellar.org/prd/core-live/core_live_003",
    ];

    /// Mainnet network passphrase.
    pub const NETWORK_PASSPHRASE: &str = "Public Global Stellar Network ; September 2015";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_archive() {
        let archive =
            HistoryArchive::new("https://history.stellar.org/prd/core-testnet/core_testnet_001");
        assert!(archive.is_ok());
    }

    #[test]
    fn test_new_archive_trailing_slash() {
        let archive =
            HistoryArchive::new("https://history.stellar.org/prd/core-testnet/core_testnet_001/")
                .unwrap();
        assert!(archive.base_url().path().ends_with('/'));
    }

    #[test]
    fn test_new_archive_no_trailing_slash() {
        let archive =
            HistoryArchive::new("https://history.stellar.org/prd/core-testnet/core_testnet_001")
                .unwrap();
        // Should have trailing slash added
        assert!(archive.base_url().path().ends_with('/'));
    }

    #[test]
    fn test_new_archive_invalid_url() {
        let archive = HistoryArchive::new("not a valid url");
        assert!(archive.is_err());
    }

    #[test]
    fn test_make_url() {
        let archive =
            HistoryArchive::new("https://history.stellar.org/prd/core-testnet/core_testnet_001")
                .unwrap();

        let url = archive
            .make_url(".well-known/stellar-history.json")
            .unwrap();
        assert_eq!(
            url.as_str(),
            "https://history.stellar.org/prd/core-testnet/core_testnet_001/.well-known/stellar-history.json"
        );

        let url = archive
            .make_url("ledger/00/00/00/ledger-0000003f.xdr.gz")
            .unwrap();
        assert_eq!(
            url.as_str(),
            "https://history.stellar.org/prd/core-testnet/core_testnet_001/ledger/00/00/00/ledger-0000003f.xdr.gz"
        );
    }

    #[test]
    fn test_testnet_constants() {
        assert_eq!(testnet::ARCHIVE_URLS.len(), 3);
        assert!(testnet::ARCHIVE_URLS[0].contains("core_testnet"));
        assert_eq!(
            testnet::NETWORK_PASSPHRASE,
            "Test SDF Network ; September 2015"
        );
    }

    #[test]
    fn test_mainnet_constants() {
        assert_eq!(mainnet::ARCHIVE_URLS.len(), 3);
        assert!(mainnet::ARCHIVE_URLS[0].contains("core_live"));
        assert_eq!(
            mainnet::NETWORK_PASSPHRASE,
            "Public Global Stellar Network ; September 2015"
        );
    }

    // Integration tests that require network access would go in tests/ directory
}
