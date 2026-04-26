//! Configuration types for henyey.
//!
//! This module defines the configuration schema for stellar-core nodes.
//! Configuration is typically loaded from a TOML file and includes settings
//! for networking, database, consensus, history archives, and logging.
//!
//! # Example Configuration (TOML)
//!
//! ```toml
//! [network]
//! passphrase = "Test SDF Network ; September 2015"
//! peer_port = 11625
//! known_peers = ["core-testnet1.stellar.org:11625"]
//!
//! [database]
//! path = "stellar.db"
//!
//! [node]
//! is_validator = false
//! ```
//!
//! # Loading Configuration
//!
//! ```rust,no_run
//! use henyey_common::Config;
//! use std::path::Path;
//!
//! // Load from file
//! let config = Config::from_file(Path::new("config.toml")).unwrap();
//!
//! // Or use the testnet defaults
//! let testnet_config = Config::testnet();
//! ```

use serde::{Deserialize, Deserializer, Serialize};
use std::path::PathBuf;

/// Log levels for filtering log output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    #[default]
    Info,
    Warn,
    Error,
}

/// Log output formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    #[default]
    Text,
    Json,
}

/// A threshold percentage value constrained to 0-100.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ThresholdPercent(u32);

impl ThresholdPercent {
    /// Create a new threshold percentage, clamping to 0-100.
    pub fn new(value: u32) -> Self {
        Self(value.min(100))
    }

    /// Get the percentage value.
    pub fn value(&self) -> u32 {
        self.0
    }
}

impl Default for ThresholdPercent {
    fn default() -> Self {
        Self(67)
    }
}

impl std::fmt::Display for ThresholdPercent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl PartialEq<u32> for ThresholdPercent {
    fn eq(&self, other: &u32) -> bool {
        self.0 == *other
    }
}

impl PartialOrd<u32> for ThresholdPercent {
    fn partial_cmp(&self, other: &u32) -> Option<std::cmp::Ordering> {
        self.0.partial_cmp(other)
    }
}

impl From<u32> for ThresholdPercent {
    fn from(value: u32) -> Self {
        Self::new(value)
    }
}

impl From<ThresholdPercent> for u32 {
    fn from(value: ThresholdPercent) -> Self {
        value.0
    }
}

impl<'de> Deserialize<'de> for ThresholdPercent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = u32::deserialize(deserializer)?;
        if value > 100 {
            return Err(serde::de::Error::custom(
                "threshold_percent must be between 0 and 100",
            ));
        }
        Ok(Self(value))
    }
}

/// Main configuration for henyey.
///
/// This is the top-level configuration struct that encompasses all settings
/// needed to run a stellar-core node. It can be loaded from a TOML file
/// using [`Config::from_file`] or created programmatically.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Network configuration (passphrase, peers, ports).
    pub network: NetworkConfig,

    /// Database configuration (storage path).
    pub database: DatabaseConfig,

    /// Node configuration (validator settings, quorum).
    pub node: NodeConfig,

    /// History archive configuration for catchup and publishing.
    #[serde(default)]
    pub history: HistoryConfig,

    /// Logging configuration (level and format).
    #[serde(default)]
    pub logging: LoggingConfig,

    /// BucketListDB configuration (indexing and caching).
    #[serde(default)]
    pub bucket_list_db: BucketListDbConfig,
}

/// Network-related configuration.
///
/// Defines the network identity (via passphrase), peer connections,
/// and port settings for the node.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfig {
    /// Network passphrase that uniquely identifies the Stellar network.
    ///
    /// Standard values:
    /// - Testnet: `"Test SDF Network ; September 2015"`
    /// - Mainnet: `"Public Global Stellar Network ; September 2015"`
    pub passphrase: String,

    /// Port to listen on for peer-to-peer connections.
    ///
    /// Default: 11625
    #[serde(default = "default_peer_port")]
    pub peer_port: u16,

    /// Port for the HTTP admin interface.
    ///
    /// Default: 11626
    #[serde(default = "default_http_port")]
    pub http_port: u16,

    /// Initial peers to connect to on startup.
    ///
    /// Format: `"hostname:port"` (e.g., `"core-testnet1.stellar.org:11625"`)
    #[serde(default)]
    pub known_peers: Vec<String>,

    /// Preferred peers that the node always tries to maintain connections with.
    ///
    /// These peers are prioritized over regular peers.
    #[serde(default)]
    pub preferred_peers: Vec<String>,

    /// Maximum number of peer connections allowed.
    ///
    /// Default: 25
    #[serde(default = "default_max_peers")]
    pub max_peer_connections: usize,

    /// Target number of peer connections to maintain.
    ///
    /// The node will actively try to maintain at least this many connections.
    /// Default: 8
    #[serde(default = "default_target_peers")]
    pub target_peer_connections: usize,
}

/// Database configuration.
///
/// Currently only SQLite is supported.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    /// Path to the SQLite database file.
    ///
    /// The file will be created if it does not exist.
    pub path: PathBuf,
}

/// Node identity and consensus configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    /// Whether this node participates in consensus as a validator.
    ///
    /// Validators require a `node_seed` and properly configured `quorum_set`.
    /// Non-validators (watchers) only observe and do not vote.
    #[serde(default)]
    pub is_validator: bool,

    /// Node seed (secret key) for signing consensus messages.
    ///
    /// Required for validators. Format: Stellar secret seed (starts with 'S').
    pub node_seed: Option<String>,

    /// Quorum set configuration for Stellar Consensus Protocol (SCP).
    ///
    /// Defines which validators this node trusts and the threshold requirements.
    #[serde(default)]
    pub quorum_set: QuorumSetConfig,
}

/// Quorum set configuration for Stellar Consensus Protocol.
///
/// A quorum set defines a set of validators and a threshold that must agree
/// for the node to consider a statement valid. Quorum sets can be nested
/// to create hierarchical trust structures.
///
/// # Example
///
/// A simple quorum set requiring 2 of 3 validators:
///
/// ```toml
/// [node.quorum_set]
/// threshold_percent = 67
/// validators = ["GA...", "GB...", "GC..."]
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuorumSetConfig {
    /// Threshold percentage (0-100) of validators/inner sets that must agree.
    ///
    /// For example, 67 means at least 67% must agree. Default: 67
    #[serde(default)]
    pub threshold_percent: ThresholdPercent,

    /// Public keys of validators in this quorum set.
    ///
    /// Format: Stellar public keys (start with 'G').
    #[serde(default)]
    pub validators: Vec<String>,

    /// Nested quorum sets for hierarchical trust.
    ///
    /// Each inner set is treated as a single member for threshold calculation.
    #[serde(default)]
    pub inner_sets: Vec<QuorumSetConfig>,
}

/// History archive configuration.
///
/// History archives store historical ledger data and are used for:
/// - Catchup: Syncing a new node to the current ledger state
/// - Audit: Verifying the complete history of the network
/// - Publishing: Validators publish their view of history
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryConfig {
    /// History archive configurations for reading (catchup).
    ///
    /// Multiple archives can be configured for redundancy.
    #[serde(default)]
    pub get_commands: Vec<HistoryArchiveConfig>,

    /// History archive configurations for writing (validators only).
    ///
    /// Validators should publish their history to at least one archive.
    #[serde(default)]
    pub put_commands: Vec<HistoryArchiveConfig>,
}

/// Configuration for a single history archive.
///
/// History archives use command templates to fetch/store files.
/// The templates support placeholders:
/// - `{0}` - Remote path (e.g., `history/00/00/00/00/ledger-00000000.xdr.gz`)
/// - `{1}` - Local path (e.g., `/tmp/stellar/ledger-00000000.xdr.gz`)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryArchiveConfig {
    /// Human-readable name for this archive.
    pub name: String,

    /// Command template for fetching files from the archive.
    ///
    /// Example: `"curl -sf https://history.stellar.org/{0} -o {1}"`
    pub get: String,

    /// Command template for uploading files to the archive (validators only).
    ///
    /// Example: `"aws s3 cp {1} s3://my-bucket/{0}"`
    #[serde(default)]
    pub put: Option<String>,

    /// Command template for creating directories in the archive (validators only).
    ///
    /// Example: `"aws s3api put-object --bucket my-bucket --key {0}/"`
    #[serde(default)]
    pub mkdir: Option<String>,
}

/// BucketListDB configuration.
///
/// Controls the indexing and caching behavior of the disk-backed BucketListDB.
/// These settings match stellar-core's `BUCKETLIST_DB_*` config options.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BucketListDbConfig {
    /// Exponent for the index page size in bytes: page_size = 1 << exponent.
    ///
    /// Default: 14 (16384 bytes = 16 KB per page), matching stellar-core.
    /// Must be a power-of-two page size. Larger pages mean fewer index entries
    /// but longer per-page scans.
    #[serde(default = "default_index_page_size_exponent")]
    pub index_page_size_exponent: u32,

    /// File size threshold (in MB) below which buckets use InMemoryIndex.
    ///
    /// Buckets with file size below this threshold use a full in-memory index
    /// (per-key offset map), while larger buckets use a page-based DiskIndex
    /// with bloom filter. Default: 20 MB, matching stellar-core's
    /// `BUCKETLIST_DB_INDEX_CUTOFF`.
    #[serde(default = "default_index_cutoff_mb")]
    pub index_cutoff_mb: usize,

    /// Memory budget (in MB) for per-bucket entry caching. 0 = disabled.
    ///
    /// When non-zero, each DiskIndex bucket gets a proportional share of this
    /// budget for caching Account entries, reducing disk I/O during lookups.
    /// Default: 1024.
    #[serde(default = "default_memory_for_caching_mb")]
    pub memory_for_caching_mb: usize,

    /// Whether to persist disk indexes alongside bucket files.
    ///
    /// When true, indexes are saved as `.index` files and loaded on startup
    /// instead of being rebuilt from bucket files. Default: true.
    #[serde(default = "default_persist_index")]
    pub persist_index: bool,
}

impl Default for BucketListDbConfig {
    fn default() -> Self {
        Self {
            index_page_size_exponent: default_index_page_size_exponent(),
            index_cutoff_mb: default_index_cutoff_mb(),
            memory_for_caching_mb: default_memory_for_caching_mb(),
            persist_index: default_persist_index(),
        }
    }
}

impl BucketListDbConfig {
    /// Returns the page size in bytes: `1 << index_page_size_exponent`.
    pub fn page_size_bytes(&self) -> u64 {
        1u64 << self.index_page_size_exponent
    }

    /// Returns the index cutoff in bytes: `index_cutoff_mb * 1024 * 1024`.
    pub fn index_cutoff_bytes(&self) -> u64 {
        self.index_cutoff_mb as u64 * 1024 * 1024
    }
}

fn default_index_page_size_exponent() -> u32 {
    14 // 16384 bytes — matches stellar-core BUCKET_INDEX_PAGE_SIZE_EXPONENT
}

fn default_index_cutoff_mb() -> usize {
    20 // matches stellar-core BUCKETLIST_DB_INDEX_CUTOFF (20 MB)
}

fn default_memory_for_caching_mb() -> usize {
    1024 // 1 GB — matches stellar-core BUCKETLIST_DB_PERSIST_INDEX_CACHE_SIZE
}

fn default_persist_index() -> bool {
    true
}

/// Logging configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfig {
    /// Log level filter.
    ///
    /// Default: `Info`
    #[serde(default)]
    pub level: LogLevel,

    /// Log output format.
    ///
    /// Default: `Text`
    #[serde(default)]
    pub format: LogFormat,
}

fn default_peer_port() -> u16 {
    11625 // stellar network default peer port
}

fn default_http_port() -> u16 {
    11626 // stellar network default HTTP admin port
}

fn default_max_peers() -> usize {
    25 // matches stellar-core MAX_ADDITIONAL_PEER_COUNT + TARGET_PEER_COUNT
}

fn default_target_peers() -> usize {
    8 // matches stellar-core DEFAULT_PEER_NUM_FAILURES_TO_BACKOFF
}

impl Config {
    /// Load configuration from a TOML file.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or contains invalid TOML.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use henyey_common::Config;
    /// use std::path::Path;
    ///
    /// let config = Config::from_file(Path::new("/etc/stellar/config.toml"))?;
    /// # Ok::<(), henyey_common::Error>(())
    /// ```
    pub fn from_file(path: &std::path::Path) -> Result<Self, crate::Error> {
        let content = std::fs::read_to_string(path)?;
        toml::from_str(&content).map_err(|e| crate::Error::Config(e.to_string()))
    }

    /// Create a default configuration for the Stellar testnet.
    ///
    /// This provides sensible defaults for connecting to the public testnet,
    /// including known SDF testnet peers and history archives.
    ///
    /// # Example
    ///
    /// ```rust
    /// use henyey_common::Config;
    ///
    /// let config = Config::testnet();
    /// assert_eq!(config.network.passphrase, "Test SDF Network ; September 2015");
    /// ```
    pub fn testnet() -> Self {
        Self {
            network: NetworkConfig {
                passphrase: "Test SDF Network ; September 2015".to_string(),
                peer_port: default_peer_port(),
                http_port: default_http_port(),
                known_peers: vec![
                    "core-testnet1.stellar.org:11625".to_string(),
                    "core-testnet2.stellar.org:11625".to_string(),
                    "core-testnet3.stellar.org:11625".to_string(),
                ],
                preferred_peers: vec![],
                max_peer_connections: default_max_peers(),
                target_peer_connections: default_target_peers(),
            },
            database: DatabaseConfig {
                path: PathBuf::from("stellar.db"),
            },
            node: NodeConfig {
                is_validator: false,
                node_seed: None,
                quorum_set: QuorumSetConfig::default(),
            },
            history: HistoryConfig {
                get_commands: vec![HistoryArchiveConfig {
                    name: "sdf1".to_string(),
                    get: "curl -sf https://history.stellar.org/prd/core-testnet/core_testnet_001/{0} -o {1}".to_string(),
                    put: None,
                    mkdir: None,
                }],
                put_commands: vec![],
            },
            logging: LoggingConfig::default(),
            bucket_list_db: BucketListDbConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_common_config_unknown_field_rejected() {
        let toml_str = r#"
[network]
passphrase = "Test SDF Network ; September 2015"
peer_port = 11625

[database]
path = "stellar.db"

[node]
is_validator = false

typo_field = "oops"
"#;
        let err = toml::from_str::<Config>(toml_str).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("typo_field"),
            "error should mention the unknown key: {msg}"
        );
    }
}
