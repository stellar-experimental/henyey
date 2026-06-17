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
    /// Operator-assigned name for this archive (used in metric labels).
    name: String,
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

    /// Create a new history archive client with an explicit name.
    ///
    /// The name is used as the `archive` label value in Prometheus metrics,
    /// allowing operators to identify per-archive health.
    pub fn with_name(base_url: &str, name: impl Into<String>) -> Result<Self, HistoryError> {
        Self::with_name_and_config(base_url, name, DownloadConfig::default())
    }

    /// Create a new history archive client with an explicit name and custom config.
    pub fn with_name_and_config(
        base_url: &str,
        name: impl Into<String>,
        config: DownloadConfig,
    ) -> Result<Self, HistoryError> {
        let mut url = Url::parse(base_url).map_err(HistoryError::UrlParse)?;
        if !url.path().ends_with('/') {
            url.set_path(&format!("{}/", url.path()));
        }

        let name_str = name.into();
        let effective_name = if name_str.is_empty() {
            derive_name_from_url(&url)
        } else {
            name_str
        };

        let client = create_client(config.timeout)?;

        Ok(Self {
            name: effective_name,
            base_url: url,
            client,
            config,
        })
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

        let name = derive_name_from_url(&url);
        let client = create_client(config.timeout)?;

        Ok(Self {
            name,
            base_url: url,
            client,
            config,
        })
    }

    /// Get the operator-assigned name of this archive.
    ///
    /// Used as the `archive` label value in Prometheus metrics.
    pub fn name(&self) -> &str {
        &self.name
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
        // Skip sentinel hashes (zero hash and SHA-256("") both represent empty buckets)
        if hash.is_empty_bucket_sentinel() {
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

    /// Get the current ledger from this archive.
    ///
    /// This is a convenience method that fetches the root HAS and returns
    /// the current ledger sequence.
    pub async fn fetch_current_ledger(&self) -> Result<u32, HistoryError> {
        let has = self.fetch_root_has().await?;
        Ok(has.current_ledger())
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
}

/// Derive a short human-readable name from a URL.
///
/// Uses the last non-empty path segment (e.g., `core_testnet_001` from
/// `https://history.stellar.org/prd/core-testnet/core_testnet_001/`).
/// Falls back to `host:port` for root URLs or URLs without path segments.
fn derive_name_from_url(url: &Url) -> String {
    // Try last non-empty path segment
    let segments: Vec<&str> = url
        .path_segments()
        .map(|s| s.filter(|seg| !seg.is_empty()).collect())
        .unwrap_or_default();

    if let Some(last) = segments.last() {
        return (*last).to_string();
    }

    // Fallback: host + port (for root URLs like http://127.0.0.1:8080/)
    match (url.host_str(), url.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_string(),
        _ => "unknown".to_string(),
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

/// Fetch the root History Archive State from a URL, blocking.
///
/// Supports HTTP/HTTPS URLs, `file://` URLs, and bare local paths.
/// This is used in the synchronous publish path (`spawn_blocking`) to
/// enable differential bucket uploads.
///
/// # Arguments
///
/// * `url` - The archive base URL (e.g., `https://history.stellar.org/...`,
///   `file:///path/to/archive`, or `/path/to/archive`)
///
/// # Returns
///
/// The parsed `HistoryArchiveState`, or an error if fetching/parsing fails.
/// The caller is responsible for deciding fallback behavior on error.
pub fn fetch_root_has_blocking(url: &str) -> Result<HistoryArchiveState, HistoryError> {
    use std::time::Duration;

    let root_has = root_has_path();

    // Try to parse as a URL first.
    match Url::parse(url) {
        Ok(parsed) => match parsed.scheme() {
            "http" | "https" => {
                let mut base = parsed;
                if !base.path().ends_with('/') {
                    base.set_path(&format!("{}/", base.path()));
                }
                let full_url = base
                    .join(root_has)
                    .map_err(|e| HistoryError::DownloadFailed(e.to_string()))?;

                let client = reqwest::blocking::Client::builder()
                    .timeout(Duration::from_secs(10))
                    .build()
                    .map_err(|e| HistoryError::DownloadFailed(e.to_string()))?;

                let resp = client
                    .get(full_url.as_str())
                    .send()
                    .map_err(|e| HistoryError::DownloadFailed(e.to_string()))?;

                if !resp.status().is_success() {
                    return Err(HistoryError::DownloadFailed(format!(
                        "{}: HTTP {}",
                        full_url,
                        resp.status()
                    )));
                }

                let text = resp
                    .text()
                    .map_err(|e| HistoryError::InvalidResponse(e.to_string()))?;

                HistoryArchiveState::from_json(&text)
            }
            "file" => {
                let path = parsed.to_file_path().map_err(|_| {
                    HistoryError::DownloadFailed(format!("Invalid file URL: {}", url))
                })?;
                read_has_from_path(&path)
            }
            scheme => Err(HistoryError::DownloadFailed(format!(
                "Unsupported URL scheme: {}",
                scheme
            ))),
        },
        // Not a valid URL — treat as a bare local path.
        Err(_) => {
            let path = std::path::PathBuf::from(url);
            read_has_from_path(&path)
        }
    }
}

/// Read the root HAS from a local filesystem path.
fn read_has_from_path(base_path: &std::path::Path) -> Result<HistoryArchiveState, HistoryError> {
    let has_path = base_path.join(root_has_path());
    let text = std::fs::read_to_string(&has_path)
        .map_err(|e| HistoryError::DownloadFailed(format!("{}: {}", has_path.display(), e)))?;
    HistoryArchiveState::from_json(&text)
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

    #[tokio::test]
    async fn test_fetch_bucket_zero_hash_returns_empty() {
        let archive =
            HistoryArchive::new("https://history.stellar.org/prd/core-testnet/core_testnet_001")
                .unwrap();
        let result = archive.fetch_bucket(&Hash256::ZERO).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_fetch_bucket_empty_hash_returns_empty() {
        let archive =
            HistoryArchive::new("https://history.stellar.org/prd/core-testnet/core_testnet_001")
                .unwrap();
        let result = archive.fetch_bucket(Hash256::empty_hash()).await.unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_derive_name_from_url_path_segment() {
        let url =
            Url::parse("https://history.stellar.org/prd/core-testnet/core_testnet_001").unwrap();
        assert_eq!(derive_name_from_url(&url), "core_testnet_001");
    }

    #[test]
    fn test_derive_name_from_url_trailing_slash() {
        let url =
            Url::parse("https://history.stellar.org/prd/core-testnet/core_testnet_002/").unwrap();
        assert_eq!(derive_name_from_url(&url), "core_testnet_002");
    }

    #[test]
    fn test_derive_name_from_url_root_with_port() {
        let url = Url::parse("http://127.0.0.1:8080/").unwrap();
        assert_eq!(derive_name_from_url(&url), "127.0.0.1:8080");
    }

    #[test]
    fn test_derive_name_from_url_root_no_port() {
        let url = Url::parse("https://archive.example.com/").unwrap();
        assert_eq!(derive_name_from_url(&url), "archive.example.com");
    }

    #[test]
    fn test_with_name_uses_provided_name() {
        let archive = HistoryArchive::with_name(
            "https://history.stellar.org/prd/core-testnet/core_testnet_001",
            "my-custom-name",
        )
        .unwrap();
        assert_eq!(archive.name(), "my-custom-name");
    }

    #[test]
    fn test_with_name_empty_falls_back_to_url() {
        let archive = HistoryArchive::with_name(
            "https://history.stellar.org/prd/core-testnet/core_testnet_001",
            "",
        )
        .unwrap();
        assert_eq!(archive.name(), "core_testnet_001");
    }

    #[test]
    fn test_new_derives_name_from_url() {
        let archive =
            HistoryArchive::new("https://history.stellar.org/prd/core-live/core_live_003").unwrap();
        assert_eq!(archive.name(), "core_live_003");
    }

    /// Helper to create a minimal valid HAS JSON for testing.
    fn minimal_has_json(ledger: u32) -> String {
        let zero = "0000000000000000000000000000000000000000000000000000000000000000";
        let zero_level = format!(
            r#"{{"curr":"{}","snap":"{}","next":{{"state":0}}}}"#,
            zero, zero
        );
        let levels = std::iter::repeat(zero_level.as_str())
            .take(11)
            .collect::<Vec<_>>()
            .join(",");
        format!(
            r#"{{"version":1,"currentLedger":{},"currentBuckets":[{}]}}"#,
            ledger, levels
        )
    }

    #[test]
    fn test_fetch_root_has_blocking_bare_path() {
        let dir = tempfile::tempdir().unwrap();
        let well_known = dir.path().join(".well-known");
        std::fs::create_dir_all(&well_known).unwrap();
        std::fs::write(
            well_known.join("stellar-history.json"),
            minimal_has_json(63),
        )
        .unwrap();

        let has = fetch_root_has_blocking(dir.path().to_str().unwrap()).unwrap();
        assert_eq!(has.current_ledger(), 63);
    }

    #[test]
    fn test_fetch_root_has_blocking_file_url() {
        let dir = tempfile::tempdir().unwrap();
        let well_known = dir.path().join(".well-known");
        std::fs::create_dir_all(&well_known).unwrap();
        std::fs::write(
            well_known.join("stellar-history.json"),
            minimal_has_json(127),
        )
        .unwrap();

        let url = format!("file://{}", dir.path().display());
        let has = fetch_root_has_blocking(&url).unwrap();
        assert_eq!(has.current_ledger(), 127);
    }

    #[test]
    fn test_fetch_root_has_blocking_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let result = fetch_root_has_blocking(dir.path().to_str().unwrap());
        assert!(result.is_err());
    }
}
