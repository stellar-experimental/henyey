//! Configuration loading and validation for rs-stellar-core.
//!
//! This module provides a comprehensive configuration system that supports:
//!
//! - Loading configuration from TOML files
//! - Environment variable overrides (prefixed with `RS_STELLAR_CORE_`)
//! - Pre-configured defaults for testnet and mainnet
//! - Validation of configuration values
//! - A builder API for programmatic configuration
//!
//! # Configuration Sections
//!
//! The configuration is organized into logical sections:
//!
//! | Section | Description |
//! |---------|-------------|
//! | `node` | Node identity, validator mode, and quorum set |
//! | `network` | Network passphrase and protocol parameters |
//! | `database` | SQLite database path and connection pool |
//! | `buckets` | Bucket storage directory and cache settings |
//! | `history` | History archive URLs for catchup |
//! | `overlay` | P2P network settings and known peers |
//! | `logging` | Log level, format, and output options |
//! | `http` | HTTP status server configuration |
//! | `surge_pricing` | Transaction lane byte allowances |
//! | `events` | Classic event emission settings |
//! | `maintenance` | Automatic database maintenance settings |
//!
//! # Example Configuration
//!
//! ```toml
//! [node]
//! name = "my-validator"
//! node_seed = "S..."  # Required for validators
//! is_validator = true
//!
//! [network]
//! passphrase = "Test SDF Network ; September 2015"
//!
//! [database]
//! path = "/var/lib/stellar/stellar.db"
//!
//! [[history.archives]]
//! name = "sdf1"
//! url = "https://history.stellar.org/prd/core-testnet/core_testnet_001"
//! ```
//!
//! # Environment Overrides
//!
//! Configuration values can be overridden using environment variables:
//!
//! - `RS_STELLAR_CORE_NODE_NAME` - Node name
//! - `RS_STELLAR_CORE_NODE_SEED` - Node secret seed
//! - `RS_STELLAR_CORE_NETWORK_PASSPHRASE` - Network passphrase
//! - `RS_STELLAR_CORE_DATABASE_PATH` - Database file path
//! - `RS_STELLAR_CORE_LOG_LEVEL` - Log level (trace, debug, info, warn, error)

use henyey_common::BucketListDbConfig;
use henyey_history::CatchupMode;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Main application configuration.
///
/// This struct represents the complete configuration for a Stellar Core node.
/// It can be loaded from a TOML file using [`AppConfig::from_file`], or constructed
/// programmatically using [`ConfigBuilder`].
///
/// # Defaults
///
/// Use [`AppConfig::testnet()`] or [`AppConfig::mainnet()`] for pre-configured
/// defaults that include known validators and history archives.
///
/// # Validation
///
/// Call [`AppConfig::validate()`] to check configuration consistency before use.
/// This validates that:
/// - Validators have a node seed configured
/// - At least one history archive is configured
/// - Quorum set configuration is valid for validators
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Node identity and behavior.
    #[serde(default)]
    pub node: NodeConfig,

    /// Network configuration.
    #[serde(default)]
    pub network: NetworkConfig,

    /// Proposed protocol upgrades.
    #[serde(default)]
    pub upgrades: UpgradeConfig,

    /// Database configuration.
    #[serde(default)]
    pub database: DatabaseConfig,

    /// Bucket storage configuration.
    #[serde(default)]
    pub buckets: BucketConfig,

    /// History archive configuration.
    #[serde(default)]
    pub history: HistoryConfig,

    /// Peer network configuration.
    #[serde(default)]
    pub overlay: OverlayConfig,

    /// Logging configuration.
    #[serde(default)]
    pub logging: LoggingConfig,

    /// HTTP server configuration.
    #[serde(default)]
    pub http: HttpConfig,

    /// Surge pricing configuration.
    #[serde(default)]
    pub surge_pricing: SurgePricingConfig,

    /// Classic event emission configuration.
    #[serde(default)]
    pub events: EventsConfig,

    /// Metadata output stream configuration.
    #[serde(default)]
    pub metadata: MetadataConfig,

    /// Catchup behavior configuration.
    #[serde(default)]
    pub catchup: CatchupConfig,

    /// stellar-core compatibility HTTP server configuration.
    /// When enabled, runs a second HTTP server that matches stellar-core's exact
    /// wire format for drop-in compatibility with stellar-rpc.
    #[serde(default)]
    pub compat_http: CompatHttpConfig,

    /// HTTP query server configuration (for `/getledgerentryraw`, `/getledgerentry`).
    #[serde(default)]
    pub query: QueryConfig,

    /// Diagnostic events configuration.
    #[serde(default)]
    pub diagnostics: DiagnosticsConfig,

    /// Testing overrides (e.g., `ARTIFICIALLY_ACCELERATE_TIME_FOR_TESTING`).
    #[serde(default)]
    pub testing: TestingConfig,

    /// Database maintenance configuration.
    #[serde(default)]
    pub maintenance: MaintenanceAppConfig,

    /// JSON-RPC server configuration.
    #[serde(default)]
    pub rpc: RpcConfig,

    /// Build metadata (programmatically set, not from TOML).
    #[serde(skip)]
    pub build: BuildMetadata,

    /// Whether this config was translated from stellar-core format.
    /// When true, the overlay layer will not inject default seed peers
    /// if no KNOWN_PEERS were specified in the original config.
    #[serde(skip)]
    pub is_compat_config: bool,
}

/// Node identity and behavior configuration.
///
/// Defines the node's identity (keypair), whether it participates in consensus
/// as a validator, and its quorum set configuration for SCP.
///
/// # Validator Requirements
///
/// If `is_validator` is true, the following must also be set:
/// - `node_seed`: The secret seed for signing SCP messages
/// - `quorum_set`: A valid quorum set with sufficient validators
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    /// Node name for identification in logs.
    #[serde(default = "default_node_name")]
    pub name: String,

    /// Secret seed for this node (S... format).
    /// Required for validators, optional for watchers.
    pub node_seed: Option<String>,

    /// Whether this node participates in consensus.
    #[serde(default)]
    pub is_validator: bool,

    /// Home domain for this node.
    pub home_domain: Option<String>,

    /// Quorum set configuration.
    #[serde(default)]
    pub quorum_set: QuorumSetConfig,

    /// Enable manual ledger close mode.
    ///
    /// When true, the node won't automatically close ledgers based on
    /// consensus timing. Instead, ledgers are closed via the /manualclose
    /// HTTP endpoint. This is primarily used for testing.
    #[serde(default)]
    pub manual_close: bool,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            name: default_node_name(),
            node_seed: None,
            is_validator: false,
            home_domain: None,
            quorum_set: QuorumSetConfig::default(),
            manual_close: false,
        }
    }
}

/// Quorum set configuration for SCP consensus.
///
/// Defines the set of validators this node trusts and the threshold required
/// for agreement. Supports hierarchical quorum sets with inner sets for
/// organizational structure.
///
/// # Threshold Calculation
///
/// The `threshold_percent` is applied to the total number of validators and
/// inner sets. For example, with 3 validators and `threshold_percent = 67`,
/// the threshold would be `ceil(3 * 0.67) = 2`.
///
/// # Safety
///
/// A well-configured quorum set should satisfy safety and liveness properties.
/// See the [SCP paper](https://www.stellar.org/papers/stellar-consensus-protocol)
/// for guidance on quorum set design.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QuorumSetConfig {
    /// Threshold percentage (0-100).
    #[serde(default = "default_threshold")]
    pub threshold_percent: u32,

    /// Validator public keys (G... format).
    #[serde(default)]
    pub validators: Vec<String>,

    /// Inner quorum sets for hierarchical structures.
    #[serde(default)]
    pub inner_sets: Vec<QuorumSetConfig>,
}

impl QuorumSetConfig {
    /// Convert to XDR ScpQuorumSet.
    ///
    /// Returns None if the quorum set is empty or has invalid validators.
    pub fn to_xdr(&self) -> Option<stellar_xdr::curr::ScpQuorumSet> {
        use stellar_xdr::curr::{NodeId, PublicKey, ScpQuorumSet, Uint256};

        // Parse validator public keys
        let mut validators = Vec::new();
        for v in &self.validators {
            // Parse G... public key to bytes
            match henyey_crypto::PublicKey::from_strkey(v) {
                Ok(pubkey) => {
                    let node_id =
                        NodeId(PublicKey::PublicKeyTypeEd25519(Uint256(*pubkey.as_bytes())));
                    validators.push(node_id);
                }
                Err(e) => {
                    tracing::error!(validator = %v, error = %e, "Failed to parse validator public key - check strkey format");
                    return None;
                }
            }
        }

        // Recursively convert inner sets
        let mut inner_sets = Vec::new();
        for inner in &self.inner_sets {
            if let Some(inner_xdr) = inner.to_xdr() {
                inner_sets.push(inner_xdr);
            }
        }

        // If empty, return None
        if validators.is_empty() && inner_sets.is_empty() {
            return None;
        }

        // Calculate threshold from percentage
        let total = validators.len() + inner_sets.len();
        let threshold = ((total as u32 * self.threshold_percent) / 100).max(1);

        let mut quorum_set = ScpQuorumSet {
            threshold,
            validators: validators.try_into().ok()?,
            inner_sets: inner_sets.try_into().ok()?,
        };
        henyey_scp::normalize_quorum_set(&mut quorum_set);
        Some(quorum_set)
    }
}

/// Network configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// Network passphrase (determines which network to connect to).
    pub passphrase: String,

    /// Base fee in stroops.
    #[serde(default = "default_base_fee")]
    pub base_fee: u32,

    /// Base reserve in stroops.
    #[serde(default = "default_base_reserve")]
    pub base_reserve: u32,

    /// Maximum protocol version to support.
    #[serde(default = "default_protocol_version")]
    pub max_protocol_version: u32,
}

/// Surge pricing configuration (lane byte allowances and caps).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SurgePricingConfig {
    /// Classic tx byte allowance for tx set selection.
    #[serde(default = "default_classic_byte_allowance")]
    pub classic_byte_allowance: u32,

    /// Soroban tx byte allowance for tx set selection.
    #[serde(default = "default_soroban_byte_allowance")]
    pub soroban_byte_allowance: u32,

    /// Optional max DEX operations for classic lane selection.
    #[serde(default)]
    pub max_dex_tx_operations: Option<u32>,
}

impl Default for SurgePricingConfig {
    fn default() -> Self {
        Self {
            classic_byte_allowance: default_classic_byte_allowance(),
            soroban_byte_allowance: default_soroban_byte_allowance(),
            max_dex_tx_operations: None,
        }
    }
}

/// Classic event emission configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EventsConfig {
    /// Emit classic asset events in transaction metadata.
    #[serde(default)]
    pub emit_classic_events: bool,

    /// Backfill classic asset events to pre-23 format.
    #[serde(default)]
    pub backfill_stellar_asset_events: bool,
}

/// Metadata output stream configuration.
///
/// Controls how `LedgerCloseMeta` frames are streamed to external consumers
/// (e.g., Horizon ingestion pipelines). This is the Rust equivalent of
/// stellar-core's `--metadata-output-stream` feature.
///
/// # Example
///
/// ```toml
/// [metadata]
/// output_stream = "/tmp/meta.pipe"
/// debug_ledgers = 100
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetadataConfig {
    /// Stream destination: file path, named pipe, or "fd:N" for a pre-opened
    /// file descriptor. When set, every `LedgerCloseMeta` is written as a
    /// size-prefixed XDR frame to this destination.
    #[serde(default)]
    pub output_stream: Option<String>,

    /// Number of ledgers to retain in debug meta files. 0 = disabled.
    /// Debug meta files are written to `<bucket_dir>/meta-debug/` with gzip
    /// compression and segment rotation every 256 ledgers.
    #[serde(default = "default_metadata_debug_ledgers")]
    pub debug_ledgers: u32,

    /// When true, include `SorobanTransactionMetaExtV1` in `TransactionMeta`.
    /// Maps to stellar-core's `EMIT_SOROBAN_TRANSACTION_META_EXT_V1`.
    #[serde(default)]
    pub emit_soroban_tx_meta_ext_v1: bool,

    /// When true, include `LedgerCloseMetaExtV1` in `LedgerCloseMeta`.
    /// Maps to stellar-core's `EMIT_LEDGER_CLOSE_META_EXT_V1`.
    #[serde(default)]
    pub emit_ledger_close_meta_ext_v1: bool,
}

fn default_metadata_debug_ledgers() -> u32 {
    0
}

/// HTTP query server configuration.
///
/// The query server runs on a separate port and serves ledger entry
/// lookups from the bucket list. Required by stellar-rpc for preflight
/// simulation and state queries.
///
/// # Example
///
/// ```toml
/// [query]
/// port = 11627
/// snapshot_ledgers = 5
/// thread_pool_size = 4
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryConfig {
    /// Port for the query server HTTP endpoint. `None` means disabled.
    /// Maps to stellar-core's `HTTP_QUERY_PORT`.
    #[serde(default)]
    pub port: Option<u16>,

    /// Number of historical ledger snapshots to retain for point-in-time queries.
    /// Maps to stellar-core's `QUERY_SNAPSHOT_LEDGERS`.
    #[serde(default = "default_query_snapshot_ledgers")]
    pub snapshot_ledgers: u32,

    /// Number of threads in the query server's blocking thread pool.
    /// Maps to stellar-core's `QUERY_THREAD_POOL_SIZE`.
    #[serde(default = "default_query_thread_pool_size")]
    pub thread_pool_size: usize,
}

impl Default for QueryConfig {
    fn default() -> Self {
        Self {
            port: None,
            snapshot_ledgers: default_query_snapshot_ledgers(),
            thread_pool_size: default_query_thread_pool_size(),
        }
    }
}

fn default_query_snapshot_ledgers() -> u32 {
    5
}

fn default_query_thread_pool_size() -> usize {
    4
}

/// Diagnostic events configuration.
///
/// Controls whether diagnostic events are captured and included in
/// metadata and transaction submission responses.
///
/// # Example
///
/// ```toml
/// [diagnostics]
/// soroban_diagnostic_events = true
/// tx_submission_diagnostics = true
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiagnosticsConfig {
    /// When true, include Soroban diagnostic events in `LedgerCloseMeta`.
    /// Maps to stellar-core's `ENABLE_SOROBAN_DIAGNOSTIC_EVENTS`.
    #[serde(default)]
    pub soroban_diagnostic_events: bool,

    /// When true, include diagnostic events in `/tx` error responses.
    /// Maps to stellar-core's `ENABLE_DIAGNOSTICS_FOR_TX_SUBMISSION`.
    #[serde(default)]
    pub tx_submission_diagnostics: bool,
}

/// Testing overrides for accelerated or customized behavior.
///
/// Maps to stellar-core's `ARTIFICIALLY_ACCELERATE_TIME_FOR_TESTING` and related
/// testing knobs. These are primarily used for local standalone networks.
///
/// # Examples
///
/// ```toml
/// [testing]
/// accelerate_time = true  # 1s ledger close, checkpoint frequency 8
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TestingConfig {
    /// When true, accelerate time for testing: 1-second ledger close and
    /// checkpoint frequency of 8 (instead of 64).
    /// Maps to `ARTIFICIALLY_ACCELERATE_TIME_FOR_TESTING`.
    #[serde(default)]
    pub accelerate_time: bool,

    /// Override the ledger close time (in seconds). Computed from `accelerate_time`
    /// if not explicitly set. `None` means use default (5s, or 1s if accelerated).
    #[serde(default)]
    pub ledger_close_time: Option<u32>,

    /// When true, enable the `/generateload` HTTP endpoint for synthetic load
    /// generation. Maps to `ARTIFICIALLY_GENERATE_LOAD_FOR_TESTING`.
    #[serde(default)]
    pub generate_load_for_testing: bool,
}

/// Catchup behavior configuration.
///
/// Controls how the node catches up to the network when joining or recovering.
/// This matches the stellar-core CATCHUP_COMPLETE and CATCHUP_RECENT settings.
///
/// # Examples
///
/// Minimal catchup (default, fastest startup):
/// ```toml
/// [catchup]
/// # No configuration needed, defaults to minimal
/// ```
///
/// Complete history from genesis:
/// ```toml
/// [catchup]
/// complete = true
/// ```
///
/// Recent history (last N ledgers):
/// ```toml
/// [catchup]
/// recent = 10000
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CatchupConfig {
    /// If true, download complete history from genesis.
    /// Equivalent to stellar-core CATCHUP_COMPLETE.
    /// Takes precedence over `recent` if both are set.
    #[serde(default)]
    pub complete: bool,

    /// Number of recent ledgers to download during catchup.
    /// Equivalent to stellar-core CATCHUP_RECENT.
    /// Set to 0 for minimal catchup (default).
    #[serde(default)]
    pub recent: u32,
}

impl CatchupConfig {
    /// Convert config to CatchupMode.
    ///
    /// Priority:
    /// 1. If `complete` is true -> Complete mode
    /// 2. If `recent` > 0 -> Recent(n) mode
    /// 3. Otherwise -> Minimal mode
    pub fn to_mode(&self) -> CatchupMode {
        if self.complete {
            CatchupMode::Complete
        } else if self.recent > 0 {
            CatchupMode::Recent(self.recent)
        } else {
            CatchupMode::Minimal
        }
    }
}

/// Proposed protocol upgrades configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpgradeConfig {
    /// Proposed protocol version upgrade.
    pub protocol_version: Option<u32>,
    /// Proposed base fee upgrade.
    pub base_fee: Option<u32>,
    /// Proposed base reserve upgrade.
    pub base_reserve: Option<u32>,
    /// Proposed max tx set size upgrade.
    pub max_tx_set_size: Option<u32>,
}

impl UpgradeConfig {
    pub fn to_ledger_upgrades(&self) -> Vec<stellar_xdr::curr::LedgerUpgrade> {
        let mut upgrades = Vec::new();
        if let Some(version) = self.protocol_version {
            upgrades.push(stellar_xdr::curr::LedgerUpgrade::Version(version));
        }
        if let Some(fee) = self.base_fee {
            upgrades.push(stellar_xdr::curr::LedgerUpgrade::BaseFee(fee));
        }
        if let Some(reserve) = self.base_reserve {
            upgrades.push(stellar_xdr::curr::LedgerUpgrade::BaseReserve(reserve));
        }
        if let Some(max_tx_set_size) = self.max_tx_set_size {
            upgrades.push(stellar_xdr::curr::LedgerUpgrade::MaxTxSetSize(
                max_tx_set_size,
            ));
        }
        upgrades
    }
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self::testnet()
    }
}

impl NetworkConfig {
    /// Create a testnet configuration.
    pub fn testnet() -> Self {
        Self {
            passphrase: "Test SDF Network ; September 2015".to_string(),
            base_fee: default_base_fee(),
            base_reserve: default_base_reserve(),
            max_protocol_version: default_protocol_version(),
        }
    }

    /// Create a mainnet configuration.
    pub fn mainnet() -> Self {
        Self {
            passphrase: "Public Global Stellar Network ; September 2015".to_string(),
            base_fee: default_base_fee(),
            base_reserve: default_base_reserve(),
            max_protocol_version: default_protocol_version(),
        }
    }
}

/// Database configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    /// Path to the SQLite database file.
    #[serde(default = "default_db_path")]
    pub path: PathBuf,

    /// Connection pool size.
    #[serde(default = "default_pool_size")]
    pub pool_size: u32,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            path: default_db_path(),
            pool_size: default_pool_size(),
        }
    }
}

/// Bucket storage configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BucketConfig {
    /// Directory for bucket files.
    #[serde(default = "default_bucket_dir")]
    pub directory: PathBuf,

    /// Maximum number of buckets to keep in memory cache.
    #[serde(default = "default_bucket_cache_size")]
    pub cache_size: usize,

    /// BucketListDB indexing and caching configuration.
    ///
    /// Controls per-bucket entry caching and index page sizes.
    /// Set `memory_for_caching_mb` to a non-zero value to enable caching.
    #[serde(default)]
    pub bucket_list_db: BucketListDbConfig,

    /// Number of parallel threads used to scan the bucket list on startup
    /// (for populating the offer store, Soroban state, and TTL caches).
    ///
    /// Default is 2. Set to 1 on memory-constrained machines where two
    /// concurrent scans would exceed available RAM.
    #[serde(default = "default_scan_thread_count")]
    pub scan_thread_count: usize,
}

fn default_scan_thread_count() -> usize {
    4
}

impl Default for BucketConfig {
    fn default() -> Self {
        Self {
            directory: default_bucket_dir(),
            cache_size: default_bucket_cache_size(),
            bucket_list_db: BucketListDbConfig::default(),
            scan_thread_count: default_scan_thread_count(),
        }
    }
}

/// History archive configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HistoryConfig {
    /// Archives for reading history.
    #[serde(default)]
    pub archives: Vec<HistoryArchiveEntry>,
}

/// A single history archive entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryArchiveEntry {
    /// Name of this archive.
    pub name: String,

    /// Base URL for the archive.
    pub url: String,

    /// Whether this archive can be used for reading.
    #[serde(default = "default_true")]
    pub get_enabled: bool,

    /// Whether this archive can be used for writing (validators only).
    #[serde(default)]
    pub put_enabled: bool,

    /// Optional command template for publishing files to a remote archive.
    /// Uses {0} = local path, {1} = remote path.
    #[serde(default)]
    pub put: Option<String>,

    /// Optional command template to create remote directories.
    /// Uses {0} = remote directory path.
    #[serde(default)]
    pub mkdir: Option<String>,
}

/// Overlay P2P network configuration.
///
/// Controls peer connections, transaction flooding, and survey authorization.
/// The overlay network is responsible for propagating transactions and SCP
/// messages between nodes.
///
/// # Connection Limits
///
/// - `max_inbound_peers`: Maximum connections accepted from other nodes
/// - `max_outbound_peers`: Maximum connections initiated to other nodes
/// - `target_outbound_peers`: Target number of outbound connections to maintain
///
/// # Transaction Flooding
///
/// The `flood_*` parameters control how transactions are advertised and
/// requested between peers to optimize bandwidth usage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlayConfig {
    /// Port to listen on for peer connections.
    #[serde(default = "default_peer_port")]
    pub peer_port: u16,

    /// Maximum number of inbound peer connections.
    #[serde(default = "default_max_inbound")]
    pub max_inbound_peers: usize,

    /// Maximum number of outbound peer connections.
    #[serde(default = "default_max_outbound")]
    pub max_outbound_peers: usize,

    /// Target number of outbound connections to maintain.
    #[serde(default = "default_target_outbound")]
    pub target_outbound_peers: usize,

    /// Known peers to connect to on startup.
    #[serde(default)]
    pub known_peers: Vec<String>,

    /// Preferred peers that should always be connected.
    #[serde(default)]
    pub preferred_peers: Vec<String>,

    /// Allowed surveyor node public keys (G...); empty means follow quorum/defaults.
    #[serde(default)]
    pub surveyor_keys: Vec<String>,

    /// Enable automatic survey scheduling (non-stellar-core behavior).
    #[serde(default)]
    pub auto_survey: bool,

    /// Target fraction of max ops to flood per ledger for classic transactions.
    #[serde(default = "default_flood_op_rate_per_ledger")]
    pub flood_op_rate_per_ledger: f64,

    /// Target fraction of max ops to flood per ledger for Soroban transactions.
    #[serde(default = "default_flood_soroban_rate_per_ledger")]
    pub flood_soroban_rate_per_ledger: f64,

    /// Period (ms) between tx demand cycles.
    #[serde(default = "default_flood_demand_period_ms")]
    pub flood_demand_period_ms: u64,

    /// Period (ms) between tx advert flushes.
    #[serde(default = "default_flood_advert_period_ms")]
    pub flood_advert_period_ms: u64,

    /// Backoff delay (ms) between repeated demands for the same tx.
    #[serde(default = "default_flood_demand_backoff_delay_ms")]
    pub flood_demand_backoff_delay_ms: u64,

    /// Maximum peer failures allowed before pruning.
    #[serde(default = "default_peer_max_failures")]
    pub peer_max_failures: u32,
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            peer_port: default_peer_port(),
            max_inbound_peers: default_max_inbound(),
            max_outbound_peers: default_max_outbound(),
            target_outbound_peers: default_target_outbound(),
            known_peers: Vec::new(),
            preferred_peers: Vec::new(),
            surveyor_keys: Vec::new(),
            auto_survey: false,
            flood_op_rate_per_ledger: default_flood_op_rate_per_ledger(),
            flood_soroban_rate_per_ledger: default_flood_soroban_rate_per_ledger(),
            flood_demand_period_ms: default_flood_demand_period_ms(),
            flood_advert_period_ms: default_flood_advert_period_ms(),
            flood_demand_backoff_delay_ms: default_flood_demand_backoff_delay_ms(),
            peer_max_failures: default_peer_max_failures(),
        }
    }
}

fn default_peer_max_failures() -> u32 {
    120
}

/// Logging configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// Log level (trace, debug, info, warn, error).
    #[serde(default = "default_log_level")]
    pub level: String,

    /// Log format (text or json).
    #[serde(default = "default_log_format")]
    pub format: String,

    /// Whether to use ANSI colors.
    #[serde(default = "default_true")]
    pub colors: bool,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            format: default_log_format(),
            colors: true,
        }
    }
}

/// HTTP server configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpConfig {
    /// Whether to enable the HTTP server.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Port for the HTTP server.
    #[serde(default = "default_http_port")]
    pub port: u16,

    /// Address to bind the HTTP server to.
    #[serde(default = "default_http_address")]
    pub address: String,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: default_http_port(),
            address: default_http_address(),
        }
    }
}

/// Configuration for the stellar-core compatibility HTTP server.
///
/// This server mirrors stellar-core's exact wire format (camelCase JSON fields,
/// GET-based endpoints, plain-text responses for admin commands, etc.) so that
/// stellar-rpc can connect to henyey without any changes.
///
/// When `enabled = false` (default), only the native henyey HTTP server runs.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CompatHttpConfig {
    /// Whether to enable the stellar-core compatibility HTTP server.
    #[serde(default)]
    pub enabled: bool,

    /// Port for the compatibility HTTP server.
    /// Default 11626 matches stellar-core's `HTTP_PORT`.
    #[serde(default = "default_compat_http_port")]
    pub port: u16,
}

impl Default for CompatHttpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: default_compat_http_port(),
        }
    }
}

/// Database maintenance configuration.
///
/// Controls automatic background cleanup of old ledger headers, SCP history,
/// and other accumulated data. Maps to stellar-core's
/// `AUTOMATIC_MAINTENANCE_PERIOD` and `AUTOMATIC_MAINTENANCE_COUNT`.
///
/// # Example
///
/// ```toml
/// [maintenance]
/// enabled = true
/// period_secs = 14400   # 4 hours
/// count = 50000
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaintenanceAppConfig {
    /// Whether automatic maintenance is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// How often to run maintenance, in seconds.
    /// Default: 14400 (4 hours). Maps to `AUTOMATIC_MAINTENANCE_PERIOD`.
    #[serde(default = "default_maintenance_period_secs")]
    pub period_secs: u64,

    /// Maximum entries to delete per maintenance cycle.
    /// Default: 50000. Maps to `AUTOMATIC_MAINTENANCE_COUNT`.
    #[serde(default = "default_maintenance_count")]
    pub count: u32,
}

impl Default for MaintenanceAppConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            period_secs: default_maintenance_period_secs(),
            count: default_maintenance_count(),
        }
    }
}

fn default_maintenance_period_secs() -> u64 {
    4 * 60 * 60 // 4 hours
}

fn default_maintenance_count() -> u32 {
    50_000
}

/// JSON-RPC server configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcConfig {
    /// Whether to enable the JSON-RPC server.
    #[serde(default)]
    pub enabled: bool,

    /// Port for the JSON-RPC server.
    #[serde(default = "default_rpc_port")]
    pub port: u16,

    /// Number of ledgers to retain for range queries (~1 day at 5s close).
    #[serde(default = "default_rpc_retention_window")]
    pub retention_window: u32,

    /// Maximum ledger age (in seconds) before getHealth reports "unhealthy".
    /// Set to 0 to disable the latency check.
    #[serde(default = "default_max_healthy_ledger_latency")]
    pub max_healthy_ledger_latency_secs: u64,
}

impl Default for RpcConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: default_rpc_port(),
            retention_window: default_rpc_retention_window(),
            max_healthy_ledger_latency_secs: default_max_healthy_ledger_latency(),
        }
    }
}

fn default_rpc_port() -> u16 {
    8000
}

fn default_rpc_retention_window() -> u32 {
    2880
}

fn default_max_healthy_ledger_latency() -> u64 {
    30
}

/// Build-time metadata populated by the binary crate's `build.rs`.
///
/// These values are not read from TOML; they are set programmatically
/// before creating the `App`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BuildMetadata {
    /// Git commit hash (from `git rev-parse HEAD`).
    pub commit_hash: String,
    /// Build timestamp in ISO 8601 format.
    pub build_timestamp: String,
}

fn default_compat_http_port() -> u16 {
    11626
}

fn default_http_port() -> u16 {
    11626
}

fn default_http_address() -> String {
    "0.0.0.0".to_string()
}

// Default value functions

fn default_node_name() -> String {
    "henyey".to_string()
}

fn default_threshold() -> u32 {
    67
}

fn default_base_fee() -> u32 {
    100
}

fn default_base_reserve() -> u32 {
    5_000_000 // 0.5 XLM in stroops
}

fn default_protocol_version() -> u32 {
    25
}

fn default_db_path() -> PathBuf {
    PathBuf::from("stellar.db")
}

fn default_pool_size() -> u32 {
    10
}

fn default_bucket_dir() -> PathBuf {
    PathBuf::from("buckets")
}

fn default_bucket_cache_size() -> usize {
    256
}

fn default_peer_port() -> u16 {
    11625
}

fn default_max_inbound() -> usize {
    64
}

fn default_max_outbound() -> usize {
    8
}

fn default_target_outbound() -> usize {
    8
}

fn default_flood_op_rate_per_ledger() -> f64 {
    1.0
}

fn default_flood_soroban_rate_per_ledger() -> f64 {
    1.0
}

fn default_flood_demand_period_ms() -> u64 {
    200
}

fn default_flood_advert_period_ms() -> u64 {
    100
}

fn default_flood_demand_backoff_delay_ms() -> u64 {
    500
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_log_format() -> String {
    "text".to_string()
}

fn default_true() -> bool {
    true
}

fn default_classic_byte_allowance() -> u32 {
    5 * 1024 * 1024
}

fn default_soroban_byte_allowance() -> u32 {
    5 * 1024 * 1024
}

impl Default for AppConfig {
    fn default() -> Self {
        Self::testnet()
    }
}

impl AppConfig {
    /// Create a default testnet configuration.
    pub fn testnet() -> Self {
        use henyey_scp::quorum_config::known_validators;

        Self {
            node: NodeConfig {
                quorum_set: QuorumSetConfig {
                    threshold_percent: known_validators::RECOMMENDED_THRESHOLD_PERCENT,
                    validators: known_validators::TESTNET_VALIDATORS
                        .iter()
                        .map(|s| s.to_string())
                        .collect(),
                    inner_sets: Vec::new(),
                },
                ..Default::default()
            },
            network: NetworkConfig::testnet(),
            upgrades: UpgradeConfig::default(),
            database: DatabaseConfig::default(),
            buckets: BucketConfig::default(),
            history: HistoryConfig {
                archives: vec![
                    HistoryArchiveEntry {
                        name: "sdf1".to_string(),
                        url: "https://history.stellar.org/prd/core-testnet/core_testnet_001"
                            .to_string(),
                        get_enabled: true,
                        put_enabled: false,
                        put: None,
                        mkdir: None,
                    },
                    HistoryArchiveEntry {
                        name: "sdf2".to_string(),
                        url: "https://history.stellar.org/prd/core-testnet/core_testnet_002"
                            .to_string(),
                        get_enabled: true,
                        put_enabled: false,
                        put: None,
                        mkdir: None,
                    },
                    HistoryArchiveEntry {
                        name: "sdf3".to_string(),
                        url: "https://history.stellar.org/prd/core-testnet/core_testnet_003"
                            .to_string(),
                        get_enabled: true,
                        put_enabled: false,
                        put: None,
                        mkdir: None,
                    },
                ],
            },
            overlay: OverlayConfig {
                known_peers: vec![
                    "core-testnet1.stellar.org:11625".to_string(),
                    "core-testnet2.stellar.org:11625".to_string(),
                    "core-testnet3.stellar.org:11625".to_string(),
                ],
                ..Default::default()
            },
            logging: LoggingConfig::default(),
            http: HttpConfig::default(),
            compat_http: CompatHttpConfig::default(),
            surge_pricing: SurgePricingConfig::default(),
            events: EventsConfig::default(),
            metadata: MetadataConfig::default(),
            catchup: CatchupConfig::default(),
            query: QueryConfig::default(),
            diagnostics: DiagnosticsConfig::default(),
            testing: TestingConfig::default(),
            maintenance: MaintenanceAppConfig::default(),
            rpc: RpcConfig::default(),
            build: BuildMetadata::default(),
            is_compat_config: false,
        }
    }

    /// Create a default mainnet configuration.
    pub fn mainnet() -> Self {
        use henyey_scp::quorum_config::known_validators;

        Self {
            node: NodeConfig {
                quorum_set: QuorumSetConfig {
                    threshold_percent: known_validators::RECOMMENDED_THRESHOLD_PERCENT,
                    validators: known_validators::MAINNET_SDF_VALIDATORS
                        .iter()
                        .map(|s| s.to_string())
                        .collect(),
                    inner_sets: Vec::new(),
                },
                ..Default::default()
            },
            network: NetworkConfig::mainnet(),
            upgrades: UpgradeConfig::default(),
            database: DatabaseConfig::default(),
            buckets: BucketConfig::default(),
            history: HistoryConfig {
                archives: vec![
                    HistoryArchiveEntry {
                        name: "sdf1".to_string(),
                        url: "https://history.stellar.org/prd/core-live/core_live_001".to_string(),
                        get_enabled: true,
                        put_enabled: false,
                        put: None,
                        mkdir: None,
                    },
                    HistoryArchiveEntry {
                        name: "sdf2".to_string(),
                        url: "https://history.stellar.org/prd/core-live/core_live_002".to_string(),
                        get_enabled: true,
                        put_enabled: false,
                        put: None,
                        mkdir: None,
                    },
                    HistoryArchiveEntry {
                        name: "sdf3".to_string(),
                        url: "https://history.stellar.org/prd/core-live/core_live_003".to_string(),
                        get_enabled: true,
                        put_enabled: false,
                        put: None,
                        mkdir: None,
                    },
                ],
            },
            overlay: OverlayConfig {
                known_peers: vec![
                    "core-live-a.stellar.org:11625".to_string(),
                    "core-live-b.stellar.org:11625".to_string(),
                    "core-live-c.stellar.org:11625".to_string(),
                ],
                ..Default::default()
            },
            logging: LoggingConfig::default(),
            http: HttpConfig::default(),
            compat_http: CompatHttpConfig::default(),
            surge_pricing: SurgePricingConfig::default(),
            events: EventsConfig::default(),
            metadata: MetadataConfig::default(),
            catchup: CatchupConfig::default(),
            query: QueryConfig::default(),
            diagnostics: DiagnosticsConfig::default(),
            testing: TestingConfig::default(),
            maintenance: MaintenanceAppConfig::default(),
            rpc: RpcConfig::default(),
            build: BuildMetadata::default(),
            is_compat_config: false,
        }
    }

    /// Load configuration from a TOML file.
    pub fn from_file(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path.as_ref())?;
        let config: Self = toml::from_str(&content)?;
        Ok(config)
    }

    /// Load configuration with environment variable overrides.
    ///
    /// Environment variables take precedence over file configuration.
    /// Variables use the pattern: `RS_STELLAR_CORE_<SECTION>_<KEY>`
    ///
    /// Examples:
    /// - RS_STELLAR_CORE_NODE_NAME
    /// - RS_STELLAR_CORE_NODE_SEED
    /// - RS_STELLAR_CORE_NETWORK_PASSPHRASE
    /// - RS_STELLAR_CORE_DATABASE_PATH
    /// - RS_STELLAR_CORE_OVERLAY_PEER_PORT
    pub fn from_file_with_env(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let mut config = Self::from_file(path)?;
        config.apply_env_overrides();
        Ok(config)
    }

    /// Apply environment variable overrides.
    pub fn apply_env_overrides(&mut self) {
        // Node overrides
        if let Ok(val) = std::env::var("RS_STELLAR_CORE_NODE_NAME") {
            self.node.name = val;
        }
        if let Ok(val) = std::env::var("RS_STELLAR_CORE_NODE_SEED") {
            self.node.node_seed = Some(val);
        }
        if let Ok(val) = std::env::var("RS_STELLAR_CORE_NODE_VALIDATOR") {
            self.node.is_validator = val.parse().unwrap_or(false);
        }

        // Network overrides
        if let Ok(val) = std::env::var("RS_STELLAR_CORE_NETWORK_PASSPHRASE") {
            self.network.passphrase = val;
        }

        // Database overrides
        if let Ok(val) = std::env::var("RS_STELLAR_CORE_DATABASE_PATH") {
            self.database.path = PathBuf::from(val);
        }

        // Bucket overrides
        if let Ok(val) = std::env::var("RS_STELLAR_CORE_BUCKETS_DIRECTORY") {
            self.buckets.directory = PathBuf::from(val);
        }

        // Overlay overrides
        if let Ok(val) = std::env::var("RS_STELLAR_CORE_OVERLAY_PEER_PORT") {
            if let Ok(port) = val.parse() {
                self.overlay.peer_port = port;
            }
        }

        // Logging overrides
        if let Ok(val) = std::env::var("RS_STELLAR_CORE_LOG_LEVEL") {
            self.logging.level = val;
        }
        if let Ok(val) = std::env::var("RS_STELLAR_CORE_LOG_FORMAT") {
            self.logging.format = val;
        }
    }

    /// Validate the configuration.
    pub fn validate(&self) -> anyhow::Result<()> {
        // Validators must have a node seed
        if self.node.is_validator && self.node.node_seed.is_none() {
            anyhow::bail!("Validators must have a node_seed configured");
        }

        // Validate node seed format if provided
        if let Some(ref seed) = self.node.node_seed {
            if !seed.starts_with('S') || seed.len() != 56 {
                anyhow::bail!("Invalid node_seed format (must be S... format)");
            }
        }

        // Must have at least one history archive for catchup
        if self.history.archives.is_empty() {
            anyhow::bail!("At least one history archive must be configured");
        }

        if self.overlay.flood_op_rate_per_ledger <= 0.0 {
            anyhow::bail!("flood_op_rate_per_ledger must be > 0");
        }
        if self.overlay.flood_soroban_rate_per_ledger <= 0.0 {
            anyhow::bail!("flood_soroban_rate_per_ledger must be > 0");
        }
        if self.overlay.flood_demand_period_ms == 0 {
            anyhow::bail!("flood_demand_period_ms must be > 0");
        }
        if self.overlay.flood_advert_period_ms == 0 {
            anyhow::bail!("flood_advert_period_ms must be > 0");
        }
        if self.overlay.flood_demand_backoff_delay_ms == 0 {
            anyhow::bail!("flood_demand_backoff_delay_ms must be > 0");
        }
        if self.overlay.auto_survey {
            anyhow::bail!("auto_survey is not supported; surveys are manual only");
        }
        for key in &self.overlay.surveyor_keys {
            if henyey_crypto::PublicKey::from_strkey(key).is_err() {
                anyhow::bail!("Invalid surveyor key: {}", key);
            }
        }

        let total_bytes = self
            .surge_pricing
            .classic_byte_allowance
            .saturating_add(self.surge_pricing.soroban_byte_allowance);
        if total_bytes > 10 * 1024 * 1024 {
            anyhow::bail!("surge_pricing byte allowances exceed 10MB total");
        }

        if self.events.backfill_stellar_asset_events && !self.events.emit_classic_events {
            anyhow::bail!(
                "events.backfill_stellar_asset_events requires events.emit_classic_events"
            );
        }

        // Warn if metadata stream is configured on a validator node
        if self.metadata.output_stream.is_some() && self.node.is_validator {
            tracing::warn!(
                "metadata.output_stream is set on a validator node; \
                 this may impact consensus performance"
            );
        }

        // HTTP port and peer port must not conflict
        if self.http.enabled && self.http.port == self.overlay.peer_port {
            anyhow::bail!(
                "HTTP port ({}) and peer port ({}) must be different",
                self.http.port,
                self.overlay.peer_port
            );
        }

        // Native HTTP and compat HTTP must not bind the same port
        if self.http.enabled && self.compat_http.enabled && self.http.port == self.compat_http.port
        {
            anyhow::bail!(
                "Native HTTP port ({}) and compat HTTP port ({}) must be different \
                 (disable one or change the port)",
                self.http.port,
                self.compat_http.port
            );
        }

        // Query server port must not conflict with other HTTP ports
        if let Some(query_port) = self.query.port {
            if self.http.enabled && query_port == self.http.port {
                anyhow::bail!(
                    "Query port ({}) and native HTTP port ({}) must be different",
                    query_port,
                    self.http.port
                );
            }
            if self.compat_http.enabled && query_port == self.compat_http.port {
                anyhow::bail!(
                    "Query port ({}) and compat HTTP port ({}) must be different",
                    query_port,
                    self.compat_http.port
                );
            }
        }

        // Bucket page size exponent must be in valid range (4-24)
        let exp = self.buckets.bucket_list_db.index_page_size_exponent;
        if !(4..=24).contains(&exp) {
            anyhow::bail!(
                "bucket_list_db.index_page_size_exponent must be between 4 and 24, got {}",
                exp
            );
        }

        // Warn about writable-without-readable archive configuration
        for archive in &self.history.archives {
            if archive.put_enabled && !archive.get_enabled {
                tracing::warn!(
                    archive = %archive.name,
                    "Archive has put_enabled but not get_enabled; cannot verify uploads"
                );
            }
        }

        // Validate quorum set if validator
        if self.node.is_validator {
            if self.node.quorum_set.validators.is_empty()
                && self.node.quorum_set.inner_sets.is_empty()
            {
                anyhow::bail!("Validators must have a quorum set configured");
            }
            if self.node.quorum_set.threshold_percent == 0 {
                anyhow::bail!("Quorum set threshold_percent must be > 0 for validators");
            }
            let quorum_set = self
                .node
                .quorum_set
                .to_xdr()
                .ok_or_else(|| anyhow::anyhow!("Invalid quorum set configuration"))?;
            if let Err(err) = henyey_scp::is_quorum_set_sane(&quorum_set, true) {
                anyhow::bail!("Invalid quorum set: {}", err);
            }
        }

        Ok(())
    }

    /// Get the network ID hash.
    pub fn network_id(&self) -> henyey_common::Hash256 {
        henyey_common::Hash256::hash(self.network.passphrase.as_bytes())
    }

    /// Generate a sample configuration file.
    pub fn sample_config() -> String {
        let config = Self::testnet();
        toml::to_string_pretty(&config).unwrap_or_default()
    }
}

/// Fluent builder for constructing [`AppConfig`] programmatically.
///
/// Provides a convenient API for building configurations without TOML files,
/// useful for tests and embedded usage.
///
/// # Example
///
/// ```
/// use henyey_app::config::ConfigBuilder;
///
/// let config = ConfigBuilder::new()
///     .node_name("test-node")
///     .database_path("/tmp/stellar.db")
///     .peer_port(11626)
///     .log_level("debug")
///     .build();
/// ```
#[derive(Debug, Default)]
pub struct ConfigBuilder {
    config: AppConfig,
}

impl ConfigBuilder {
    /// Create a new builder with testnet defaults.
    pub fn new() -> Self {
        Self {
            config: AppConfig::testnet(),
        }
    }

    /// Create a builder for mainnet.
    pub fn mainnet() -> Self {
        Self {
            config: AppConfig::mainnet(),
        }
    }

    /// Set the node name.
    pub fn node_name(mut self, name: impl Into<String>) -> Self {
        self.config.node.name = name.into();
        self
    }

    /// Set the node seed.
    pub fn node_seed(mut self, seed: impl Into<String>) -> Self {
        self.config.node.node_seed = Some(seed.into());
        self
    }

    /// Set validator mode.
    pub fn validator(mut self, is_validator: bool) -> Self {
        self.config.node.is_validator = is_validator;
        self
    }

    /// Set the database path.
    pub fn database_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.database.path = path.into();
        self
    }

    /// Set the bucket directory.
    pub fn bucket_directory(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.buckets.directory = path.into();
        self
    }

    /// Set the peer port.
    pub fn peer_port(mut self, port: u16) -> Self {
        self.config.overlay.peer_port = port;
        self
    }

    /// Add a known peer.
    pub fn add_known_peer(mut self, peer: impl Into<String>) -> Self {
        self.config.overlay.known_peers.push(peer.into());
        self
    }

    /// Add a history archive.
    pub fn add_history_archive(mut self, name: impl Into<String>, url: impl Into<String>) -> Self {
        self.config.history.archives.push(HistoryArchiveEntry {
            name: name.into(),
            url: url.into(),
            get_enabled: true,
            put_enabled: false,
            put: None,
            mkdir: None,
        });
        self
    }

    /// Set the log level.
    pub fn log_level(mut self, level: impl Into<String>) -> Self {
        self.config.logging.level = level.into();
        self
    }

    /// Build the configuration.
    pub fn build(self) -> AppConfig {
        self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = AppConfig::default();
        assert_eq!(
            config.network.passphrase,
            "Test SDF Network ; September 2015"
        );
        assert!(!config.node.is_validator);
        assert!(config.upgrades.to_ledger_upgrades().is_empty());
    }

    #[test]
    fn test_mainnet_config() {
        let config = AppConfig::mainnet();
        assert_eq!(
            config.network.passphrase,
            "Public Global Stellar Network ; September 2015"
        );
    }

    #[test]
    fn test_config_builder() {
        let config = ConfigBuilder::new()
            .node_name("my-node")
            .database_path("/tmp/stellar.db")
            .peer_port(11626)
            .log_level("debug")
            .build();

        assert_eq!(config.node.name, "my-node");
        assert_eq!(config.database.path, PathBuf::from("/tmp/stellar.db"));
        assert_eq!(config.overlay.peer_port, 11626);
        assert_eq!(config.logging.level, "debug");
    }

    #[test]
    fn test_validation_validator_without_seed() {
        let mut config = AppConfig::default();
        config.node.is_validator = true;

        let result = config.validate();
        assert!(result.is_err());
    }

    #[test]
    fn test_validation_validator_with_unsafe_quorum() {
        let mut config = AppConfig::default();
        config.node.is_validator = true;
        config.node.node_seed =
            Some("SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH".to_string());
        config.node.quorum_set.validators = vec![
            "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF".to_string(),
            "GBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBQB4".to_string(),
        ];
        config.node.quorum_set.threshold_percent = 50;

        let result = config.validate();
        assert!(result.is_err());
    }

    #[test]
    fn test_network_id() {
        let config = AppConfig::testnet();
        let network_id = config.network_id();
        // Testnet network ID is well-known
        assert!(!network_id.is_zero());
    }

    #[test]
    fn test_sample_config() {
        let sample = AppConfig::sample_config();
        assert!(!sample.is_empty());
        assert!(sample.contains("[node]"));
        assert!(sample.contains("[network]"));
    }

    #[test]
    fn test_quorum_set_normalizes_validators() {
        let mut config = AppConfig::default();
        let first_key = henyey_scp::quorum_config::known_validators::TESTNET_VALIDATORS[0];
        let second_key = henyey_scp::quorum_config::known_validators::TESTNET_VALIDATORS[1];
        config.node.quorum_set.validators = vec![second_key.to_string(), first_key.to_string()];
        config.node.quorum_set.threshold_percent = 67;

        let quorum_set = config.node.quorum_set.to_xdr().expect("quorum set");
        let validators: Vec<_> = quorum_set.validators.iter().cloned().collect();

        assert_eq!(validators.len(), 2);
        let bytes: Vec<[u8; 32]> = validators
            .iter()
            .map(|node_id| match &node_id.0 {
                stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(stellar_xdr::curr::Uint256(
                    bytes,
                )) => *bytes,
            })
            .collect();
        assert!(bytes[0] <= bytes[1]);
    }

    #[test]
    fn test_metadata_config_defaults() {
        let config = AppConfig::default();
        assert!(config.metadata.output_stream.is_none());
        assert_eq!(config.metadata.debug_ledgers, 0);
    }

    #[test]
    fn test_metadata_config_from_toml() {
        let toml_str = r#"
[network]
passphrase = "Test SDF Network ; September 2015"

[[history.archives]]
name = "sdf1"
url = "https://history.stellar.org/prd/core-testnet/core_testnet_001"

[metadata]
output_stream = "/tmp/meta.pipe"
debug_ledgers = 200
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(
            config.metadata.output_stream.as_deref(),
            Some("/tmp/meta.pipe")
        );
        assert_eq!(config.metadata.debug_ledgers, 200);
    }

    #[test]
    fn test_catchup_config_defaults() {
        let config = CatchupConfig::default();
        assert!(!config.complete);
        assert_eq!(config.recent, 0);
        assert!(matches!(config.to_mode(), CatchupMode::Minimal));
    }

    #[test]
    fn test_catchup_config_complete() {
        let config = CatchupConfig {
            complete: true,
            recent: 0,
        };
        assert!(matches!(config.to_mode(), CatchupMode::Complete));
    }

    #[test]
    fn test_catchup_config_recent() {
        let config = CatchupConfig {
            complete: false,
            recent: 10000,
        };
        assert!(matches!(config.to_mode(), CatchupMode::Recent(10000)));
    }

    #[test]
    fn test_catchup_config_complete_takes_precedence() {
        // If both complete and recent are set, complete wins
        let config = CatchupConfig {
            complete: true,
            recent: 10000,
        };
        assert!(matches!(config.to_mode(), CatchupMode::Complete));
    }

    #[test]
    fn test_catchup_config_from_toml() {
        let toml_str = r#"
[network]
passphrase = "Test SDF Network ; September 2015"

[[history.archives]]
name = "sdf1"
url = "https://history.stellar.org/prd/core-testnet/core_testnet_001"

[catchup]
recent = 5000
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert!(!config.catchup.complete);
        assert_eq!(config.catchup.recent, 5000);
        assert!(matches!(
            config.catchup.to_mode(),
            CatchupMode::Recent(5000)
        ));
    }

    #[test]
    fn test_catchup_config_complete_from_toml() {
        let toml_str = r#"
[network]
passphrase = "Test SDF Network ; September 2015"

[[history.archives]]
name = "sdf1"
url = "https://history.stellar.org/prd/core-testnet/core_testnet_001"

[catchup]
complete = true
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert!(config.catchup.complete);
        assert!(matches!(config.catchup.to_mode(), CatchupMode::Complete));
    }

    // Item 9: Config validation tests
    #[test]
    fn test_validation_http_peer_port_conflict() {
        let mut config = AppConfig::default();
        config.http.port = 11625;
        config.overlay.peer_port = 11625;
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("port"));
    }

    #[test]
    fn test_validation_bucket_page_size_exponent_bounds() {
        let mut config = AppConfig::default();
        config.buckets.bucket_list_db.index_page_size_exponent = 3; // Below min of 4
        let result = config.validate();
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("index_page_size_exponent"));

        let mut config = AppConfig::default();
        config.buckets.bucket_list_db.index_page_size_exponent = 25; // Above max of 24
        let result = config.validate();
        assert!(result.is_err());
    }

    #[test]
    fn test_validation_quorum_threshold_zero() {
        let mut config = AppConfig::default();
        config.node.is_validator = true;
        config.node.node_seed =
            Some("SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH".to_string());
        config.node.quorum_set.threshold_percent = 0;
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("threshold"));
    }

    #[test]
    fn test_validation_valid_config_different_ports() {
        // Default config should have different ports (11625 vs 11626) and pass
        let config = AppConfig::default();
        // Not a validator, so quorum checks are skipped
        let result = config.validate();
        assert!(result.is_ok());
    }

    #[test]
    fn test_bucket_list_db_config_default() {
        let config = AppConfig::default();
        assert_eq!(config.buckets.bucket_list_db.memory_for_caching_mb, 1024);
        assert_eq!(config.buckets.bucket_list_db.index_page_size_exponent, 14);
        assert!(config.buckets.bucket_list_db.persist_index);
    }

    #[test]
    fn test_bucket_list_db_config_from_toml() {
        let toml_str = r#"
[network]
passphrase = "Test SDF Network ; September 2015"

[[history.archives]]
name = "sdf1"
url = "https://history.stellar.org/prd/core-testnet/core_testnet_001"

[buckets.bucket_list_db]
memory_for_caching_mb = 512
index_page_size_exponent = 16
index_cutoff_mb = 40
persist_index = false
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.buckets.bucket_list_db.memory_for_caching_mb, 512);
        assert_eq!(config.buckets.bucket_list_db.index_page_size_exponent, 16);
        assert_eq!(config.buckets.bucket_list_db.index_cutoff_mb, 40);
        assert!(!config.buckets.bucket_list_db.persist_index);
    }

    #[test]
    fn test_bucket_list_db_config_partial_toml() {
        // Only setting memory_for_caching_mb, other fields should get defaults
        let toml_str = r#"
[network]
passphrase = "Test SDF Network ; September 2015"

[[history.archives]]
name = "sdf1"
url = "https://history.stellar.org/prd/core-testnet/core_testnet_001"

[buckets.bucket_list_db]
memory_for_caching_mb = 256
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.buckets.bucket_list_db.memory_for_caching_mb, 256);
        // Defaults for unspecified fields
        assert_eq!(config.buckets.bucket_list_db.index_page_size_exponent, 14);
        assert!(config.buckets.bucket_list_db.persist_index);
    }

    #[test]
    fn test_validation_compat_http_port_collision() {
        let mut config = AppConfig::default();
        config.http.enabled = true;
        config.http.port = 11626;
        config.compat_http.enabled = true;
        config.compat_http.port = 11626;
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("compat HTTP"));
    }

    #[test]
    fn test_validation_query_port_collision_with_compat() {
        let mut config = AppConfig::default();
        config.compat_http.enabled = true;
        config.compat_http.port = 11627;
        config.query.port = Some(11627);
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Query port"));
    }

    // --- Maintenance config tests ---

    #[test]
    fn test_maintenance_config_defaults() {
        let config = AppConfig::default();
        assert!(config.maintenance.enabled);
        assert_eq!(config.maintenance.period_secs, 4 * 60 * 60);
        assert_eq!(config.maintenance.count, 50_000);
    }

    #[test]
    fn test_maintenance_config_from_toml() {
        let toml_str = r#"
[network]
passphrase = "Test SDF Network ; September 2015"

[[history.archives]]
name = "sdf1"
url = "https://history.stellar.org/prd/core-testnet/core_testnet_001"

[maintenance]
enabled = false
period_secs = 7200
count = 10000
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert!(!config.maintenance.enabled);
        assert_eq!(config.maintenance.period_secs, 7200);
        assert_eq!(config.maintenance.count, 10_000);
    }

    #[test]
    fn test_maintenance_config_partial_toml() {
        // Only set count; enabled and period_secs should get defaults
        let toml_str = r#"
[network]
passphrase = "Test SDF Network ; September 2015"

[[history.archives]]
name = "sdf1"
url = "https://history.stellar.org/prd/core-testnet/core_testnet_001"

[maintenance]
count = 100000
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert!(config.maintenance.enabled);
        assert_eq!(config.maintenance.period_secs, 4 * 60 * 60);
        assert_eq!(config.maintenance.count, 100_000);
    }

    #[test]
    fn test_maintenance_config_omitted_uses_defaults() {
        // No [maintenance] section at all
        let toml_str = r#"
[network]
passphrase = "Test SDF Network ; September 2015"

[[history.archives]]
name = "sdf1"
url = "https://history.stellar.org/prd/core-testnet/core_testnet_001"
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert!(config.maintenance.enabled);
        assert_eq!(config.maintenance.period_secs, 4 * 60 * 60);
        assert_eq!(config.maintenance.count, 50_000);
    }

    #[test]
    fn test_testing_config_defaults() {
        let config = TestingConfig::default();
        assert!(!config.accelerate_time);
        assert!(config.ledger_close_time.is_none());
        assert!(!config.generate_load_for_testing);
    }

    #[test]
    fn test_testing_config_from_toml() {
        let toml_str = r#"
[network]
passphrase = "Test SDF Network ; September 2015"

[[history.archives]]
name = "sdf1"
url = "https://history.stellar.org/prd/core-testnet/core_testnet_001"

[testing]
accelerate_time = true
generate_load_for_testing = true
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert!(config.testing.accelerate_time);
        assert!(config.testing.generate_load_for_testing);
    }

    #[test]
    fn test_testing_config_generate_load_false_by_default() {
        let toml_str = r#"
[network]
passphrase = "Test SDF Network ; September 2015"

[[history.archives]]
name = "sdf1"
url = "https://history.stellar.org/prd/core-testnet/core_testnet_001"
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert!(!config.testing.generate_load_for_testing);
    }
}
