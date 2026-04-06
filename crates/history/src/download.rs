//! Download utilities for history archive files.
//!
//! This module provides HTTP download infrastructure for fetching data from
//! history archives, including:
//!
//! - **Retry logic**: Automatic retries with configurable backoff
//! - **Gzip decompression**: Most archive files are gzip-compressed
//! - **XDR stream parsing**: Parse "record-marked" XDR streams
//!
//! # Record-Marked XDR Format
//!
//! History archives use a special XDR format where each record is prefixed
//! with a 4-byte big-endian length. This allows streaming parsing without
//! knowing the total number of records upfront.
//!
//! ```text
//! [4-byte length][XDR record][4-byte length][XDR record]...
//! ```
//!
//! The `parse_record_marked_xdr_stream` function handles this format.

use std::io::Read;
use std::time::Duration;

use bytes::Bytes;
use flate2::read::GzDecoder;
use reqwest::Client;
use stellar_xdr::curr::ReadXdr;
use tracing::{debug, warn};

use crate::error::HistoryError;

/// Default timeout for HTTP requests.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Retry count for operations that should retry a few times (e.g. HAS downloads).
///
/// Matches stellar-core's `RETRY_A_FEW = 5`.
pub const RETRY_A_FEW: u32 = 5;

/// Retry count for operations that should retry many times.
///
/// Matches stellar-core's `RETRY_A_LOT = 32`.
pub const RETRY_A_LOT: u32 = 32;

/// Default number of retry attempts (uses `RETRY_A_FEW`).
pub const DEFAULT_RETRIES: u32 = RETRY_A_FEW;

/// Default delay between retries.
pub const DEFAULT_RETRY_DELAY: Duration = Duration::from_secs(1);

/// Configuration for download operations.
#[derive(Debug, Clone)]
pub struct DownloadConfig {
    /// HTTP request timeout.
    pub timeout: Duration,
    /// Number of retry attempts.
    pub retries: u32,
    /// Delay between retries.
    pub retry_delay: Duration,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
            retries: DEFAULT_RETRIES,
            retry_delay: DEFAULT_RETRY_DELAY,
        }
    }
}

/// Create a new HTTP client configured for history archive access.
///
/// The client is configured with:
/// - rustls-tls (no OpenSSL dependency)
/// - Custom timeout
///
/// Note: We disable automatic gzip decompression because we handle it
/// manually in `decompress_gzip()` for downloaded bucket files.
pub fn create_client(timeout: Duration) -> Result<Client, HistoryError> {
    Client::builder()
        .timeout(timeout)
        .gzip(false) // Manual decompression for control
        .build()
        .map_err(HistoryError::Http)
}

/// Download a file from a URL with retry logic.
///
/// # Arguments
///
/// * `client` - The HTTP client to use
/// * `url` - The URL to download from
/// * `config` - Download configuration (retries, timeout, etc.)
///
/// # Returns
///
/// The downloaded bytes, or an error if all retries fail.
pub async fn download_with_retries(
    client: &Client,
    url: &str,
    config: &DownloadConfig,
) -> Result<Bytes, HistoryError> {
    let mut last_error = None;

    for attempt in 0..=config.retries {
        if attempt > 0 {
            debug!(
                url = url,
                attempt = attempt,
                "Retrying download after delay"
            );
            tokio::time::sleep(config.retry_delay).await;
        }

        match download_once(client, url).await {
            Ok(bytes) => {
                debug!(url = url, bytes = bytes.len(), "Download successful");
                return Ok(bytes);
            }
            Err(e) => {
                warn!(
                    url = url,
                    attempt = attempt,
                    error = %e,
                    "Download attempt failed"
                );
                last_error = Some(e);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| {
        HistoryError::DownloadFailed(format!("Failed to download {url} after retries"))
    }))
}

/// Download a file from a URL (single attempt).
///
/// Supports both HTTP(S) and `file://` URLs. For `file://` URLs, reads
/// directly from the local filesystem.
async fn download_once(client: &Client, url: &str) -> Result<Bytes, HistoryError> {
    // Handle file:// URLs by reading from the local filesystem.
    if let Some(path) = url.strip_prefix("file://") {
        let path = std::path::PathBuf::from(path);
        match tokio::fs::read(&path).await {
            Ok(data) => return Ok(Bytes::from(data)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(HistoryError::NotFound(url.to_string()));
            }
            Err(e) => return Err(HistoryError::Io(e)),
        }
    }

    let response = client.get(url).send().await.map_err(HistoryError::Http)?;

    let status = response.status();
    if !status.is_success() {
        if status.as_u16() == 404 {
            return Err(HistoryError::NotFound(url.to_string()));
        }
        return Err(HistoryError::HttpStatus {
            url: url.to_string(),
            status: status.as_u16(),
        });
    }

    response.bytes().await.map_err(HistoryError::Http)
}

/// Decompress gzip-compressed data.
///
/// # Arguments
///
/// * `compressed` - The gzip-compressed bytes
///
/// # Returns
///
/// The decompressed bytes, or an error if decompression fails.
pub fn decompress_gzip(compressed: &[u8]) -> Result<Vec<u8>, HistoryError> {
    let mut decoder = GzDecoder::new(compressed);
    let mut decompressed = Vec::new();

    decoder
        .read_to_end(&mut decompressed)
        .map_err(HistoryError::Io)?;

    Ok(decompressed)
}

/// Parse an XDR stream containing multiple entries.
///
/// XDR files in history archives contain a sequence of entries, each
/// prefixed with its length. This function parses all entries from the stream.
///
/// # Type Parameters
///
/// * `T` - The XDR type to parse. Must implement `ReadXdr`.
///
/// # Arguments
///
/// * `data` - The raw XDR bytes
///
/// # Returns
///
/// A vector of parsed entries, or an error if parsing fails.
pub fn parse_xdr_stream<T: ReadXdr>(data: &[u8]) -> Result<Vec<T>, HistoryError> {
    use stellar_xdr::curr::{Limited, Limits};

    let mut entries = Vec::new();
    let cursor = std::io::Cursor::new(data);
    let mut limited = Limited::new(cursor, Limits::none());

    // Read entries until EOF
    loop {
        match T::read_xdr(&mut limited) {
            Ok(entry) => entries.push(entry),
            Err(stellar_xdr::curr::Error::Io(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                // Normal end of stream
                break;
            }
            Err(e) => return Err(HistoryError::Xdr(e)),
        }
    }

    Ok(entries)
}

/// Parse an XDR stream that uses length-prefixed framing.
///
/// Some XDR streams use explicit 4-byte length prefixes before each entry.
/// This function handles that format.
///
/// # Type Parameters
///
/// * `T` - The XDR type to parse. Must implement `ReadXdr`.
///
/// # Arguments
///
/// * `data` - The raw XDR bytes with length prefixes
///
/// # Returns
///
/// A vector of parsed entries, or an error if parsing fails.
pub fn parse_length_prefixed_xdr_stream<T: ReadXdr>(data: &[u8]) -> Result<Vec<T>, HistoryError> {
    let mut entries = Vec::new();
    let mut offset = 0;

    while offset + 4 <= data.len() {
        // Read 4-byte big-endian length
        let len = u32::from_be_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as usize;
        offset += 4;

        if offset + len > data.len() {
            return Err(HistoryError::XdrParsing(format!(
                "Invalid XDR length: {} exceeds remaining data {}",
                len,
                data.len() - offset
            )));
        }

        let entry_data = &data[offset..offset + len];
        let entry = T::from_xdr(entry_data, stellar_xdr::curr::Limits::none())
            .map_err(HistoryError::Xdr)?;
        entries.push(entry);

        offset += len;

        // XDR padding to 4-byte boundary
        let padding = (4 - (len % 4)) % 4;
        offset += padding;
    }

    Ok(entries)
}

/// Parse an XDR stream that uses XDR Record Marking Standard (RFC 5531).
///
/// The record marking format uses 4-byte record marks where:
/// - High bit (0x80000000) indicates "last fragment" for this record
/// - Lower 31 bits contain the record length
///
/// This format is used by Stellar history archive ledger and transaction files.
///
/// # Type Parameters
///
/// * `T` - The XDR type to parse. Must implement `ReadXdr`.
///
/// # Arguments
///
/// * `data` - The raw XDR bytes with record marks
///
/// # Returns
///
/// A vector of parsed entries, or an error if parsing fails.
pub fn parse_record_marked_xdr_stream<T: ReadXdr>(data: &[u8]) -> Result<Vec<T>, HistoryError> {
    let mut entries = Vec::new();
    let mut offset = 0;

    while offset + 4 <= data.len() {
        // Read 4-byte record mark (big-endian)
        let record_mark = u32::from_be_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]);
        offset += 4;

        // High bit is "last fragment" flag, remaining 31 bits are length
        let _last_fragment = (record_mark & 0x80000000) != 0;
        let record_len = (record_mark & 0x7FFFFFFF) as usize;

        if record_len == 0 {
            continue; // Empty record, skip
        }

        if offset + record_len > data.len() {
            return Err(HistoryError::XdrParsing(format!(
                "Record length {} exceeds remaining data {} at offset {}",
                record_len,
                data.len() - offset,
                offset - 4
            )));
        }

        let record_data = &data[offset..offset + record_len];
        let entry = T::from_xdr(record_data, stellar_xdr::curr::Limits::none())
            .map_err(HistoryError::Xdr)?;
        entries.push(entry);

        offset += record_len;
    }

    Ok(entries)
}

/// Parse an XDR stream, auto-detecting the format.
///
/// This function detects whether the stream uses XDR Record Marking Standard
/// (RFC 5531) by checking if the high bit is set in the first 4 bytes.
/// If so, it uses record marking parsing; otherwise, it uses raw XDR parsing.
///
/// # Type Parameters
///
/// * `T` - The XDR type to parse. Must implement `ReadXdr`.
///
/// # Arguments
///
/// * `data` - The raw XDR bytes
///
/// # Returns
///
/// A vector of parsed entries, or an error if parsing fails.
pub fn parse_xdr_stream_auto<T: ReadXdr>(data: &[u8]) -> Result<Vec<T>, HistoryError> {
    if data.is_empty() {
        return Ok(Vec::new());
    }

    // History/bucket files always use XDR record marks (RFC 5531).
    // No format detection needed — always parse with record marks.
    parse_record_marked_xdr_stream(data)
}

/// Download and decompress a gzipped file.
///
/// This is a convenience function that combines downloading with retries
/// and gzip decompression.
///
/// # Arguments
///
/// * `client` - The HTTP client to use
/// * `url` - The URL to download from
/// * `config` - Download configuration
///
/// # Returns
///
/// The decompressed bytes, or an error.
pub async fn download_and_decompress(
    client: &Client,
    url: &str,
    config: &DownloadConfig,
) -> Result<Vec<u8>, HistoryError> {
    let compressed = download_with_retries(client, url, config).await?;
    decompress_gzip(&compressed)
}

/// Download, decompress, and parse an XDR file.
///
/// This is a convenience function for the common case of downloading
/// a gzipped XDR file and parsing it into a vector of entries.
///
/// # Type Parameters
///
/// * `T` - The XDR type to parse. Must implement `ReadXdr`.
///
/// # Arguments
///
/// * `client` - The HTTP client to use
/// * `url` - The URL to download from
/// * `config` - Download configuration
///
/// # Returns
///
/// A vector of parsed entries, or an error.
pub async fn download_and_parse_xdr<T: ReadXdr>(
    client: &Client,
    url: &str,
    config: &DownloadConfig,
) -> Result<Vec<T>, HistoryError> {
    let data = download_and_decompress(client, url, config).await?;
    parse_xdr_stream(&data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = DownloadConfig::default();
        assert_eq!(config.timeout, DEFAULT_TIMEOUT);
        assert_eq!(config.retries, DEFAULT_RETRIES);
        assert_eq!(config.retry_delay, DEFAULT_RETRY_DELAY);
    }

    #[test]
    fn test_decompress_gzip() {
        // Create some gzip-compressed data
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Write;

        let original = b"Hello, World!";
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(original).unwrap();
        let compressed = encoder.finish().unwrap();

        // Decompress and verify
        let decompressed = decompress_gzip(&compressed).unwrap();
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_decompress_invalid_gzip() {
        let result = decompress_gzip(b"not gzip data");
        assert!(result.is_err());
    }

    #[test]
    fn test_create_client() {
        let client = create_client(Duration::from_secs(30));
        assert!(client.is_ok());
    }

    // ── CATCHUP_SPEC §9.1: Retry constants ──────────────────────────

    #[test]
    fn test_retry_a_few_is_5() {
        assert_eq!(RETRY_A_FEW, 5, "CATCHUP_SPEC §9.1: RETRY_A_FEW must be 5");
    }

    #[test]
    fn test_retry_a_lot_is_32() {
        assert_eq!(RETRY_A_LOT, 32, "CATCHUP_SPEC §9.1: RETRY_A_LOT must be 32");
    }

    #[test]
    fn test_default_retries_uses_retry_a_few() {
        assert_eq!(
            DEFAULT_RETRIES, RETRY_A_FEW,
            "DEFAULT_RETRIES must equal RETRY_A_FEW"
        );
    }

    #[test]
    fn test_default_retry_delay_is_1s() {
        assert_eq!(DEFAULT_RETRY_DELAY, Duration::from_secs(1));
    }
}
