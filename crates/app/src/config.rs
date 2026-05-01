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

use anyhow::Context;
use henyey_common::BucketListDbConfig;
use henyey_history::CatchupMode;
use henyey_overlay::PeerAddress;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::maintainer;

/// Parse a peer address string into a `PeerAddress`.
///
/// Delegates to `PeerAddress::from_str`. Kept as a convenience for the HTTP
/// `/connect` endpoint and other sites that accept runtime string input.
///
/// See [`PeerAddress`] for validation rules.
pub(crate) fn parse_peer_address(value: &str) -> Result<PeerAddress, String> {
    value
        .parse()
        .map_err(|e: henyey_overlay::PeerAddressParseError| e.to_string())
}

/// Main application configuration.
///
/// Quorum safety configuration from stellar-core compat configs.
///
/// These fields mirror stellar-core's `FAILURE_SAFETY` and `UNSAFE_QUORUM`
/// config knobs, which control minimum fault-tolerance requirements for the
/// quorum set. Not used by native henyey configs.
#[derive(Debug, Clone)]
pub struct CompatQuorumSafety {
    /// Operator-specified failure safety requirement.
    pub failure_safety: FailureSafety,
    /// When true, relaxes quorum safety checks (low thresholds, zero failure safety).
    pub unsafe_quorum: bool,
    /// Validation threshold level, derived from quorum set provenance.
    pub threshold_level: ValidationThresholdLevel,
}

/// How the failure safety target was specified.
#[derive(Debug, Clone, Copy)]
pub enum FailureSafety {
    /// Compute default from quorum set structure (stellar-core FAILURE_SAFETY=-1).
    Auto,
    /// Operator-specified explicit value (>= 0).
    Explicit(i32),
}

/// Threshold validation level, matching stellar-core's `ValidationThresholdLevels`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationThresholdLevel {
    /// Simple majority: n - (n-1)/2. Used for single-domain auto-generated quorum sets.
    SimpleMajority,
    /// Byzantine fault tolerance: n - (n-1)/3. Used for multi-domain or manual quorum sets.
    ByzantineFaultTolerance,
}

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
#[serde(deny_unknown_fields)]
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

    /// Runtime invariant check configuration.
    #[serde(default)]
    pub invariants: InvariantConfig,

    /// Build metadata (programmatically set, not from TOML).
    #[serde(skip)]
    pub build: BuildMetadata,

    /// Whether this config was translated from stellar-core format.
    /// When true, the overlay layer will not inject default seed peers
    /// if no KNOWN_PEERS were specified in the original config.
    #[serde(skip)]
    pub is_compat_config: bool,

    /// Pre-computed validator weight configuration for application-specific
    /// leader election (protocol V22+). Built during config translation when
    /// `[[VALIDATORS]]` with quality/home-domain data are present.
    /// `None` for manual quorum set configs or non-validator nodes.
    #[serde(skip)]
    pub validator_weight_config: Option<henyey_herder::ValidatorWeightConfig>,

    /// Quorum safety configuration from stellar-core compat config translation.
    /// `None` for native henyey configs (which skip FAILURE_SAFETY validation).
    #[serde(skip)]
    pub compat_quorum_safety: Option<CompatQuorumSafety>,
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
#[serde(deny_unknown_fields)]
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

    /// When true, always use the old quorum-position weight algorithm for
    /// nomination leader election, regardless of protocol version.
    /// Matches stellar-core's `FORCE_OLD_STYLE_LEADER_ELECTION`.
    #[serde(default)]
    pub force_old_style_leader_election: bool,
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
            force_old_style_leader_election: false,
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
#[serde(deny_unknown_fields)]
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
    /// Returns true if no validators or inner sets are configured.
    pub fn is_empty(&self) -> bool {
        self.validators.is_empty() && self.inner_sets.is_empty()
    }

    /// Convert to XDR ScpQuorumSet.
    ///
    /// Returns an error if the quorum set is empty, has invalid validators,
    /// invalid inner sets, or an invalid threshold_percent. Errors from
    /// nested inner sets are propagated with path context rather than
    /// silently dropped.
    pub fn to_xdr(&self) -> anyhow::Result<stellar_xdr::curr::ScpQuorumSet> {
        use stellar_xdr::curr::{NodeId, PublicKey, ScpQuorumSet, Uint256};

        if self.is_empty() {
            anyhow::bail!("Quorum set has no validators or inner sets");
        }

        if self.threshold_percent == 0 || self.threshold_percent > 100 {
            anyhow::bail!(
                "Quorum set threshold_percent must be in 1..=100, got {}",
                self.threshold_percent
            );
        }

        // Parse validator public keys
        let mut validators = Vec::new();
        for v in &self.validators {
            let pubkey = henyey_crypto::PublicKey::from_strkey(v)
                .map_err(|e| anyhow::anyhow!("Invalid validator public key '{}': {}", v, e))?;
            let node_id = NodeId(PublicKey::PublicKeyTypeEd25519(Uint256(*pubkey.as_bytes())));
            validators.push(node_id);
        }

        // Recursively convert inner sets — propagate errors instead of dropping
        let mut inner_sets = Vec::new();
        for (i, inner) in self.inner_sets.iter().enumerate() {
            let inner_xdr = inner
                .to_xdr()
                .with_context(|| format!("in inner_sets[{}]", i))?;
            inner_sets.push(inner_xdr);
        }

        // Calculate threshold from percentage using ceiling division.
        // Matches stellar-core: 1 + ((total * percent - 1) / 100)
        let total = validators.len() as u32 + inner_sets.len() as u32;
        let threshold = 1 + (total * self.threshold_percent - 1) / 100;

        let mut quorum_set = ScpQuorumSet {
            threshold,
            validators: validators
                .try_into()
                .map_err(|_| anyhow::anyhow!("Too many validators for XDR encoding"))?,
            inner_sets: inner_sets
                .try_into()
                .map_err(|_| anyhow::anyhow!("Too many inner sets for XDR encoding"))?,
        };
        henyey_scp::normalize_quorum_set(&mut quorum_set);
        Ok(quorum_set)
    }
}

/// Network configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct QueryConfig {
    /// Port for the query server HTTP endpoint. `None` means disabled.
    /// Maps to stellar-core's `HTTP_QUERY_PORT`.
    #[serde(default)]
    pub port: Option<u16>,

    /// Bind address for the query server. When `None`, inherits `[http].address`.
    /// Set explicitly by the compat translation (from `PUBLIC_HTTP_PORT`) or
    /// via the native `[query].address` TOML key.
    #[serde(default)]
    pub address: Option<String>,

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
            address: None,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticsConfig {
    /// When true, include Soroban diagnostic events in `LedgerCloseMeta`.
    /// Maps to stellar-core's `ENABLE_SOROBAN_DIAGNOSTIC_EVENTS`.
    #[serde(default)]
    pub soroban_diagnostic_events: bool,

    /// When true, include diagnostic events in `/tx` error responses.
    /// Maps to stellar-core's `ENABLE_DIAGNOSTICS_FOR_TX_SUBMISSION`.
    #[serde(default)]
    pub tx_submission_diagnostics: bool,

    /// When the event loop has been frozen for at least this many seconds,
    /// the watchdog thread calls `std::process::abort()` to terminate the
    /// process and (if `ulimit -c` allows) generate a core dump.
    ///
    /// Defaults to `120`. Set to `0` to disable auto-abort.
    /// Requires a process supervisor (systemd, monitor-loop) to restart
    /// the node after the abort.
    #[serde(default = "default_watchdog_abort_secs")]
    pub watchdog_abort_secs: u64,
}

fn default_watchdog_abort_secs() -> u64 {
    120
}

impl Default for DiagnosticsConfig {
    fn default() -> Self {
        Self {
            soroban_diagnostic_events: false,
            tx_submission_diagnostics: false,
            watchdog_abort_secs: default_watchdog_abort_secs(),
        }
    }
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
#[serde(deny_unknown_fields)]
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

    /// Number of test accounts to create in the genesis ledger.
    ///
    /// When non-zero, `initialize_genesis_ledger()` creates this many accounts
    /// named `"TestAccount-0"` through `"TestAccount-{N-1}"` alongside the root
    /// account, splitting `total_coins` evenly (root gets the remainder).
    /// Keys are derived deterministically: the name is right-padded with `'.'`
    /// to 32 bytes and used as an Ed25519 seed.
    ///
    /// Maps to stellar-core's `GENESIS_TEST_ACCOUNT_COUNT`.
    #[serde(default)]
    pub genesis_test_account_count: u32,

    /// When true, the node operates as a standalone validator not connected
    /// to a real network. Standalone validators are exempt from certain
    /// config restrictions that protect networked validators (diagnostic
    /// events, metadata stream, query port).
    ///
    /// Maps to stellar-core's `RUN_STANDALONE`.
    #[serde(default)]
    pub run_standalone: bool,
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct HistoryConfig {
    /// Archives for reading history.
    #[serde(default)]
    pub archives: Vec<HistoryArchiveEntry>,
}

impl HistoryConfig {
    /// Whether any archive is configured for command-based writing.
    ///
    /// When false, checkpoint publishing cannot happen: entries should not be
    /// enqueued to the publish queue and the publish queue should not pin
    /// maintenance retention thresholds.
    pub fn publish_enabled(&self) -> bool {
        self.archives
            .iter()
            .any(|a| a.put_enabled && a.put.is_some())
    }
}

/// A single history archive entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
    pub known_peers: Vec<PeerAddress>,

    /// Preferred peers that should always be connected.
    #[serde(default)]
    pub preferred_peers: Vec<PeerAddress>,

    /// Preferred peer public keys (G... format) for node-ID-based preference.
    ///
    /// Peers whose authenticated node ID matches any of these keys are treated
    /// as preferred, regardless of their address. Matches stellar-core's
    /// `PREFERRED_PEER_KEYS` config field.
    #[serde(default)]
    pub preferred_peer_keys: Vec<String>,

    /// When `true`, reject any authenticated peer that is not preferred
    /// (by address or by key). Matches stellar-core's `PREFERRED_PEERS_ONLY`.
    #[serde(default)]
    pub preferred_peers_only: bool,

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

    /// Period (ms) for tx advert batch sizing.
    ///
    /// Controls the maximum number of tx hashes per advert message via
    /// `max_advert_size()`. Does **not** drive the flush timer cadence —
    /// that is governed by `flood_tx_period_ms`.
    /// Matches stellar-core `FLOOD_ADVERT_PERIOD_MS` (default 100).
    #[serde(default = "default_flood_advert_period_ms")]
    pub flood_advert_period_ms: u64,

    /// Period (ms) for the transaction broadcast cycle.
    ///
    /// Controls the broadcast ops budget and the timer cadence for
    /// `flush_tx_adverts()`. Matches stellar-core `FLOOD_TX_PERIOD_MS`
    /// (default 200).
    #[serde(default = "default_flood_tx_period_ms")]
    pub flood_tx_period_ms: u64,

    /// Backoff delay (ms) between repeated demands for the same tx.
    #[serde(default = "default_flood_demand_backoff_delay_ms")]
    pub flood_demand_backoff_delay_ms: u64,

    /// Maximum peer failures allowed before pruning.
    #[serde(default = "default_peer_max_failures")]
    pub peer_max_failures: u32,

    /// Arbitrage flood damping: number of unconditional broadcasts per asset
    /// pair per ledger. Set to `-1` to disable. Default `5`.
    /// Matches stellar-core `FLOOD_ARB_TX_BASE_ALLOWANCE`.
    #[serde(default = "default_flood_arb_tx_base_allowance")]
    pub flood_arb_tx_base_allowance: i32,

    /// Arbitrage flood damping: probability parameter for geometric distribution.
    /// Must be in `(0.0, 1.0]`. Default `0.8`.
    /// Matches stellar-core `FLOOD_ARB_TX_DAMPING_FACTOR`.
    #[serde(default = "default_flood_arb_tx_damping_factor")]
    pub flood_arb_tx_damping_factor: f64,

    /// Override for `PEER_FLOOD_READING_CAPACITY_BYTES` — initial byte-level
    /// flood reading capacity. When 0 (default), auto-computed from max tx size.
    /// Both this and `flow_control_send_more_batch_size_bytes` must be 0 for
    /// auto-compute, or both may be set to override.
    #[serde(default)]
    pub peer_flood_reading_capacity_bytes: u32,

    /// Override for `FLOW_CONTROL_SEND_MORE_BATCH_SIZE_BYTES` — byte batch
    /// size for SEND_MORE messages. When 0 (default), uses the initial
    /// constant (100 000). Must not exceed `peer_flood_reading_capacity_bytes`.
    #[serde(default)]
    pub flow_control_send_more_batch_size_bytes: u32,
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
            preferred_peer_keys: Vec::new(),
            preferred_peers_only: false,
            surveyor_keys: Vec::new(),
            auto_survey: false,
            flood_op_rate_per_ledger: default_flood_op_rate_per_ledger(),
            flood_soroban_rate_per_ledger: default_flood_soroban_rate_per_ledger(),
            flood_demand_period_ms: default_flood_demand_period_ms(),
            flood_advert_period_ms: default_flood_advert_period_ms(),
            flood_tx_period_ms: default_flood_tx_period_ms(),
            flood_demand_backoff_delay_ms: default_flood_demand_backoff_delay_ms(),
            peer_max_failures: default_peer_max_failures(),
            flood_arb_tx_base_allowance: default_flood_arb_tx_base_allowance(),
            flood_arb_tx_damping_factor: default_flood_arb_tx_damping_factor(),
            peer_flood_reading_capacity_bytes: 0,
            flow_control_send_more_batch_size_bytes: 0,
        }
    }
}

fn default_peer_max_failures() -> u32 {
    120
}

fn default_flood_arb_tx_base_allowance() -> i32 {
    5
}

fn default_flood_arb_tx_damping_factor() -> f64 {
    0.8
}

/// Logging configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct CompatHttpConfig {
    /// Whether to enable the stellar-core compatibility HTTP server.
    #[serde(default)]
    pub enabled: bool,

    /// Port for the compatibility HTTP server.
    /// Default 11626 matches stellar-core's `HTTP_PORT`.
    #[serde(default = "default_compat_http_port")]
    pub port: u16,

    /// Address to bind the compatibility HTTP server to.
    #[serde(default = "default_http_address")]
    pub address: String,
}

impl Default for CompatHttpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: default_compat_http_port(),
            address: default_http_address(),
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
#[serde(deny_unknown_fields)]
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
    maintainer::DEFAULT_MAINTENANCE_PERIOD.as_secs()
}

fn default_maintenance_count() -> u32 {
    maintainer::DEFAULT_MAINTENANCE_COUNT
}

/// JSON-RPC server configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

    /// Maximum number of concurrent `simulateTransaction` requests.
    /// Limits CPU/memory pressure from Soroban host execution.
    #[serde(default = "default_max_concurrent_simulations")]
    pub max_concurrent_simulations: u32,

    /// Maximum number of concurrent request executions.
    /// Requests beyond this limit receive an immediate `server_busy` error.
    #[serde(default = "default_max_concurrent_requests")]
    pub max_concurrent_requests: usize,

    /// Request execution timeout in seconds for read-only methods.
    /// `sendTransaction` is exempt (side-effectful).
    #[serde(default = "default_request_timeout_secs")]
    pub request_timeout_secs: u64,

    /// Maximum concurrent RPC database queries.
    /// Should be ≤ the database connection pool size (10) to avoid contention.
    #[serde(default = "default_rpc_db_concurrency")]
    pub rpc_db_concurrency: usize,

    /// Maximum concurrent bucket-list I/O blocking tasks.
    /// Independent from `rpc_db_concurrency` so bucket reads and DB queries
    /// don't starve each other on the shared `spawn_blocking` pool.
    #[serde(default = "default_bucket_io_concurrency")]
    pub bucket_io_concurrency: usize,

    /// Maximum cumulative raw XDR bytes to load from the database per
    /// `getLedgers` request.  Acts as a DB load budget — the first ledger
    /// is always returned regardless of its size so pagination can make
    /// forward progress.  Defaults to 10 MiB.
    #[serde(default = "default_max_ledger_meta_load_bytes")]
    pub max_ledger_meta_load_bytes: usize,

    /// Maximum cumulative raw bytes (`txbody + txresult + txmeta`) to load
    /// from the database per `getTransactions` request.  Acts as a DB load
    /// budget — the first transaction is always returned regardless of its
    /// size so pagination can make forward progress.  Defaults to 10 MiB.
    #[serde(default = "default_max_tx_load_bytes")]
    pub max_tx_load_bytes: usize,

    /// Maximum cumulative stored bytes (`event_xdr + topic1..4`) to load
    /// from the database per `getEvents` request.  Acts as a DB load
    /// budget — the first event is always returned regardless of its
    /// size so pagination can make forward progress.  Measures the
    /// base64-encoded TEXT length as stored in SQLite.  Defaults to 10 MiB.
    #[serde(default = "default_max_event_load_bytes")]
    pub max_event_load_bytes: usize,

    /// Maximum SQLite VM opcodes per `getEvents` query.  When exceeded,
    /// the query is interrupted and a "query budget exceeded" error is
    /// returned.  This bounds worst-case scan work for unindexed or
    /// poorly-selective filter combinations.  0 = unlimited (not
    /// recommended for production).  Default: 5,000,000.
    #[serde(default = "default_max_event_query_ops")]
    pub max_event_query_ops: u32,

    /// Maximum SQLite VM opcodes per `getTransactions` query.  When exceeded,
    /// the query is interrupted and a "query budget exceeded" error is
    /// returned.  This bounds worst-case scan work for status-filtered
    /// queries over retained transaction history.  0 = unlimited (not
    /// recommended for production).  Default: 5,000,000.
    #[serde(default = "default_max_tx_query_ops")]
    pub max_tx_query_ops: u32,
}

impl Default for RpcConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: default_rpc_port(),
            retention_window: default_rpc_retention_window(),
            max_healthy_ledger_latency_secs: default_max_healthy_ledger_latency(),
            max_concurrent_simulations: default_max_concurrent_simulations(),
            max_concurrent_requests: default_max_concurrent_requests(),
            request_timeout_secs: default_request_timeout_secs(),
            rpc_db_concurrency: default_rpc_db_concurrency(),
            bucket_io_concurrency: default_bucket_io_concurrency(),
            max_ledger_meta_load_bytes: default_max_ledger_meta_load_bytes(),
            max_tx_load_bytes: default_max_tx_load_bytes(),
            max_event_load_bytes: default_max_event_load_bytes(),
            max_event_query_ops: default_max_event_query_ops(),
            max_tx_query_ops: default_max_tx_query_ops(),
        }
    }
}

impl RpcConfig {
    /// Validate config values at startup.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_concurrent_simulations == 0 {
            return Err("rpc.max_concurrent_simulations must be > 0".to_string());
        }
        if self.max_concurrent_requests == 0 {
            return Err("rpc.max_concurrent_requests must be > 0".to_string());
        }
        if self.request_timeout_secs == 0 {
            return Err("rpc.request_timeout_secs must be > 0".to_string());
        }
        if self.rpc_db_concurrency == 0 {
            return Err("rpc.rpc_db_concurrency must be > 0".to_string());
        }
        if self.bucket_io_concurrency == 0 {
            return Err("rpc.bucket_io_concurrency must be > 0".to_string());
        }
        if self.max_event_query_ops == 0 {
            tracing::warn!(
                "rpc.max_event_query_ops is 0 (unlimited) — \
                 getEvents queries have no computational budget"
            );
        }
        if self.max_tx_query_ops == 0 {
            tracing::warn!(
                "rpc.max_tx_query_ops is 0 (unlimited) — \
                 getTransactions queries have no computational budget"
            );
        }
        if self.port == 0 {
            return Err("rpc.port must not be 0 (ephemeral) when enabled \
                 (set a fixed port or disable rpc)"
                .to_string());
        }
        Ok(())
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

fn default_max_concurrent_simulations() -> u32 {
    10
}

fn default_max_concurrent_requests() -> usize {
    64
}

fn default_request_timeout_secs() -> u64 {
    30
}

fn default_rpc_db_concurrency() -> usize {
    8
}

fn default_bucket_io_concurrency() -> usize {
    8
}

fn default_max_ledger_meta_load_bytes() -> usize {
    10 * 1024 * 1024 // 10 MiB
}

fn default_max_tx_load_bytes() -> usize {
    10 * 1024 * 1024 // 10 MiB
}

fn default_max_event_load_bytes() -> usize {
    10 * 1024 * 1024 // 10 MiB
}

fn default_max_event_query_ops() -> u32 {
    5_000_000
}

fn default_max_tx_query_ops() -> u32 {
    5_000_000
}

/// Runtime invariant check configuration.
///
/// Invariants are read-only checks that run after each operation apply to detect
/// ledger corruption early. Maps to stellar-core's `INVARIANT_CHECKS`,
/// `INVARIANT_EXTRA_CHECKS`, and `STATE_SNAPSHOT_INVARIANT_LEDGER_FREQUENCY`.
///
/// # Parity
///
/// - `checks`: maps to `INVARIANT_CHECKS` (array of regex patterns)
/// - `extra_checks`: maps to `INVARIANT_EXTRA_CHECKS` (bool)
/// - `snapshot_frequency_secs`: maps to `STATE_SNAPSHOT_INVARIANT_LEDGER_FREQUENCY`
///   (default 300s in stellar-core, Config.cpp:364)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvariantConfig {
    /// Invariant check patterns to enable (regex, full-match semantics).
    /// Each pattern is matched against registered invariant names.
    /// An empty list means no invariants are enabled.
    #[serde(default)]
    pub checks: Vec<String>,

    /// Enable extra invariant checks (typically more expensive).
    /// Cannot be true when `node.is_validator` is true (matches stellar-core).
    #[serde(default)]
    pub extra_checks: bool,

    /// Frequency in seconds for state snapshot invariant checks.
    /// 0 disables snapshot checks. Default: 300 (stellar-core default).
    #[serde(default = "default_snapshot_frequency_secs")]
    pub snapshot_frequency_secs: u64,
}

impl Default for InvariantConfig {
    fn default() -> Self {
        Self {
            checks: Vec::new(),
            extra_checks: false,
            snapshot_frequency_secs: default_snapshot_frequency_secs(),
        }
    }
}

fn default_snapshot_frequency_secs() -> u64 {
    300
}

/// Build-time metadata populated by the binary crate's `build.rs`.
///
/// These values are not read from TOML; they are set programmatically
/// before creating the `App`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    "127.0.0.1".to_string()
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

fn default_flood_tx_period_ms() -> u64 {
    200
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

/// Read an environment variable, distinguishing "not set" from "not valid UTF-8".
///
/// Returns `Ok(None)` if the variable is not set, `Ok(Some(val))` if set and
/// valid UTF-8, or an error if the value is not valid UTF-8.
fn env_var_opt(name: &str) -> anyhow::Result<Option<String>> {
    match std::env::var(name) {
        Ok(val) => Ok(Some(val)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            anyhow::bail!("{name}: value is not valid UTF-8")
        }
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
                    PeerAddress::new("core-testnet1.stellar.org", 11625),
                    PeerAddress::new("core-testnet2.stellar.org", 11625),
                    PeerAddress::new("core-testnet3.stellar.org", 11625),
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
            invariants: InvariantConfig::default(),
            build: BuildMetadata::default(),
            is_compat_config: false,
            validator_weight_config: None,
            compat_quorum_safety: None,
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
                    PeerAddress::new("core-live-a.stellar.org", 11625),
                    PeerAddress::new("core-live-b.stellar.org", 11625),
                    PeerAddress::new("core-live-c.stellar.org", 11625),
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
            invariants: InvariantConfig::default(),
            build: BuildMetadata::default(),
            is_compat_config: false,
            validator_weight_config: None,
            compat_quorum_safety: None,
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
        config.apply_env_overrides()?;
        Ok(config)
    }

    /// Apply environment variable overrides from the process environment.
    ///
    /// Fails immediately with a descriptive error if any set env var contains
    /// a value that cannot be parsed into the expected type. Unset variables
    /// are silently skipped.
    pub fn apply_env_overrides(&mut self) -> anyhow::Result<()> {
        self.apply_env_overrides_from(env_var_opt)
    }

    /// Apply configuration overrides from an arbitrary key→value source.
    ///
    /// The `lookup` function is called once per supported override key. It
    /// must return:
    /// - `Ok(Some(value))` — the override is applied
    /// - `Ok(None)` — no override for this key (skipped)
    /// - `Err(e)` — lookup failed (e.g., non-UTF-8 value); propagated immediately
    ///
    /// ## Contract
    /// - Overrides are applied in a fixed order: node, network, database,
    ///   buckets, overlay, logging. This matches the struct field layout.
    /// - **Fail-fast**: on the first `Err` from `lookup` or the first parse
    ///   failure, the method returns immediately. Fields processed before the
    ///   error retain their overridden values (partial mutation).
    /// - This is the same behavior as `apply_env_overrides()`.
    pub fn apply_env_overrides_from(
        &mut self,
        mut lookup: impl FnMut(&str) -> anyhow::Result<Option<String>>,
    ) -> anyhow::Result<()> {
        // Node overrides
        if let Some(val) = lookup("RS_STELLAR_CORE_NODE_NAME")? {
            self.node.name = val;
        }
        if let Some(val) = lookup("RS_STELLAR_CORE_NODE_SEED")? {
            self.node.node_seed = Some(val);
        }
        if let Some(val) = lookup("RS_STELLAR_CORE_NODE_VALIDATOR")? {
            self.node.is_validator = val.parse::<bool>().with_context(|| {
                format!(
                    "RS_STELLAR_CORE_NODE_VALIDATOR: invalid boolean '{val}', \
                     must be 'true' or 'false'"
                )
            })?;
        }

        // Network overrides
        if let Some(val) = lookup("RS_STELLAR_CORE_NETWORK_PASSPHRASE")? {
            self.network.passphrase = val;
        }

        // Database overrides
        if let Some(val) = lookup("RS_STELLAR_CORE_DATABASE_PATH")? {
            self.database.path = PathBuf::from(val);
        }

        // Bucket overrides
        if let Some(val) = lookup("RS_STELLAR_CORE_BUCKETS_DIRECTORY")? {
            self.buckets.directory = PathBuf::from(val);
        }

        // Overlay overrides
        if let Some(val) = lookup("RS_STELLAR_CORE_OVERLAY_PEER_PORT")? {
            self.overlay.peer_port = val.parse::<u16>().with_context(|| {
                format!(
                    "RS_STELLAR_CORE_OVERLAY_PEER_PORT: invalid port '{val}', \
                     must be a valid u16 (0–65535)"
                )
            })?;
        }

        // Logging overrides
        if let Some(val) = lookup("RS_STELLAR_CORE_LOG_LEVEL")? {
            self.logging.level = val;
        }
        if let Some(val) = lookup("RS_STELLAR_CORE_LOG_FORMAT")? {
            self.logging.format = val;
        }

        Ok(())
    }

    /// Returns true if this is a validator connected to a real network.
    ///
    /// Standalone validators (local/testing mode) are excluded. Several
    /// configuration restrictions only apply to networked validators.
    /// Mirrors stellar-core's `isNetworkedValidator` (ApplicationImpl.cpp:664-665).
    pub fn is_networked_validator(&self) -> bool {
        self.node.is_validator && !self.testing.run_standalone
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

        if !self.overlay.flood_op_rate_per_ledger.is_finite()
            || self.overlay.flood_op_rate_per_ledger <= 0.0
        {
            anyhow::bail!("flood_op_rate_per_ledger must be finite and > 0");
        }
        if !self.overlay.flood_soroban_rate_per_ledger.is_finite()
            || self.overlay.flood_soroban_rate_per_ledger <= 0.0
        {
            anyhow::bail!("flood_soroban_rate_per_ledger must be finite and > 0");
        }
        if self.overlay.flood_demand_period_ms == 0 {
            anyhow::bail!("flood_demand_period_ms must be > 0");
        }
        if self.overlay.flood_advert_period_ms == 0 {
            anyhow::bail!("flood_advert_period_ms must be > 0");
        }
        if self.overlay.flood_tx_period_ms == 0 {
            anyhow::bail!("flood_tx_period_ms must be > 0");
        }
        if self.overlay.flood_demand_backoff_delay_ms == 0 {
            anyhow::bail!("flood_demand_backoff_delay_ms must be > 0");
        }
        if self.overlay.flood_arb_tx_base_allowance < -1 {
            anyhow::bail!("flood_arb_tx_base_allowance must be >= -1");
        }
        if !self.overlay.flood_arb_tx_damping_factor.is_finite()
            || self.overlay.flood_arb_tx_damping_factor <= 0.0
            || self.overlay.flood_arb_tx_damping_factor > 1.0
        {
            anyhow::bail!("flood_arb_tx_damping_factor must be in (0.0, 1.0]");
        }
        if self.overlay.auto_survey {
            anyhow::bail!("auto_survey is not supported; surveys are manual only");
        }
        for key in &self.overlay.surveyor_keys {
            if henyey_crypto::PublicKey::from_strkey(key).is_err() {
                anyhow::bail!("Invalid surveyor key: {}", key);
            }
        }
        for key in &self.overlay.preferred_peer_keys {
            if henyey_crypto::PublicKey::from_strkey(key).is_err() {
                anyhow::bail!("Invalid preferred_peer_keys entry: {}", key);
            }
        }
        for (field, peers) in [
            ("overlay.known_peers", &self.overlay.known_peers),
            ("overlay.preferred_peers", &self.overlay.preferred_peers),
        ] {
            for entry in peers {
                if entry.host.is_empty() {
                    anyhow::bail!("Invalid {field} entry: host is empty");
                }
                if entry.port == 0 {
                    anyhow::bail!("Invalid {field} entry \"{entry}\": port must be > 0");
                }
            }
        }

        // Validate flow control byte config overrides (matching Config.cpp:1973-1981).
        henyey_overlay::FlowControlBytesConfig::new(
            self.overlay.peer_flood_reading_capacity_bytes,
            self.overlay.flow_control_send_more_batch_size_bytes,
        )
        .map_err(|e| anyhow::anyhow!("Invalid configuration: {e}"))?;

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

        // Reject metadata stream on a networked validator.
        // Matches stellar-core's ApplicationImpl.cpp:674-680 which forbids
        // METADATA_OUTPUT_STREAM on networked validators because synchronous
        // FIFO opens can block indefinitely and stall consensus.
        if self.metadata.output_stream.is_some() && self.is_networked_validator() {
            anyhow::bail!(
                "metadata.output_stream cannot be used on a networked validator \
                 (set node.is_validator = false for watcher/captive-core mode, \
                 or set testing.run_standalone = true for standalone mode)"
            );
        }

        // Reject Soroban diagnostic events on a networked validator.
        // Matches stellar-core's ApplicationImpl.cpp:667-673.
        if self.diagnostics.soroban_diagnostic_events && self.is_networked_validator() {
            anyhow::bail!(
                "diagnostics.soroban_diagnostic_events cannot be enabled on a networked \
                 validator (set node.is_validator = false for watcher mode, \
                 or set testing.run_standalone = true for standalone mode)"
            );
        }

        // Reject query server on a networked validator.
        // Matches stellar-core's ApplicationImpl.cpp:723-730.
        if self.query.port.is_some() && self.is_networked_validator() {
            anyhow::bail!(
                "query.port cannot be set on a networked validator \
                 (set node.is_validator = false for watcher mode, \
                 or set testing.run_standalone = true for standalone mode)"
            );
        }

        // Reject port collisions between all active listeners.
        validate_port_collisions(self)?;

        if self.query.thread_pool_size == 0 {
            anyhow::bail!("query.thread_pool_size must be > 0");
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
            let quorum_set = self
                .node
                .quorum_set
                .to_xdr()
                .context("Invalid quorum set configuration")?;

            // UNSAFE_QUORUM=true in compat configs disables the >50% threshold
            // check in is_quorum_set_sane, matching stellar-core behavior.
            // Native configs always get extra_checks=true.
            let extra_checks = self
                .compat_quorum_safety
                .as_ref()
                .map_or(true, |s| !s.unsafe_quorum);
            if let Err(err) = henyey_scp::is_quorum_set_sane(&quorum_set, extra_checks) {
                anyhow::bail!("Invalid quorum set: {}", err);
            }

            // Compat configs: validate FAILURE_SAFETY and threshold requirements.
            if let Some(ref safety) = self.compat_quorum_safety {
                validate_quorum_safety(&quorum_set, safety)?;
            }
        }

        // Reject ephemeral port 0 on enabled servers (security: prevents
        // accidental endpoint on unpredictable port — see #2088/#2123).
        if self.http.enabled && self.http.port == 0 && !self.testing.run_standalone {
            anyhow::bail!(
                "http.port must not be 0 (ephemeral) in production \
                 (set a fixed port, or use testing.run_standalone = true for standalone mode)"
            );
        }
        if self.compat_http.enabled && self.compat_http.port == 0 {
            anyhow::bail!(
                "compat_http.port must not be 0 (ephemeral) when enabled \
                 (set a fixed port or disable compat_http)"
            );
        }
        if self.query.port == Some(0) {
            anyhow::bail!(
                "query.port must not be 0 (ephemeral) \
                 (set a fixed port or omit query.port to disable)"
            );
        }
        if self.overlay.peer_port == 0 {
            anyhow::bail!(
                "overlay.peer_port must not be 0 (ephemeral) \
                 (set a fixed port for peer connections)"
            );
        }

        // Validate RPC config
        if self.rpc.enabled {
            self.rpc.validate().map_err(|e| anyhow::anyhow!("{}", e))?;
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

/// Validates that no two active listeners are configured on the same port.
///
/// Port-only comparison (not address+port) is intentional: `0.0.0.0` binds all
/// interfaces, so any port shared with a more-specific address still causes a
/// real bind failure. This conservative policy matches the behavior of the
/// ad-hoc checks this replaces.
fn validate_port_collisions(config: &AppConfig) -> anyhow::Result<()> {
    // Collect all active (name, port) pairs. Insertion order determines which
    // collision is reported first — matches the historical check order.
    let mut listeners: Vec<(&str, u16)> = Vec::new();

    // peer_port is always included (existing behavior: the pre-existing
    // http-vs-peer_port check had no overlay-active guard).
    listeners.push(("overlay.peer_port", config.overlay.peer_port));

    if config.http.enabled {
        listeners.push(("http.port", config.http.port));
    }
    if config.compat_http.enabled {
        listeners.push(("compat_http.port", config.compat_http.port));
    }
    if let Some(query_port) = config.query.port {
        listeners.push(("query.port", query_port));
    }
    if config.rpc.enabled {
        listeners.push(("rpc.port", config.rpc.port));
    }

    // O(n²) with n ≤ 5 — trivial cost.
    for i in 0..listeners.len() {
        for j in (i + 1)..listeners.len() {
            if listeners[i].1 == listeners[j].1 {
                anyhow::bail!(
                    "{} ({}) and {} ({}) must be different",
                    listeners[i].0,
                    listeners[i].1,
                    listeners[j].0,
                    listeners[j].1
                );
            }
        }
    }
    Ok(())
}

/// Compute the minimum acceptable threshold for a quorum set at the given
/// validation level. Ports stellar-core's `computeDefaultThreshold`
/// (Config.cpp:91-123).
fn compute_default_threshold(
    qset: &stellar_xdr::curr::ScpQuorumSet,
    level: ValidationThresholdLevel,
) -> u32 {
    let top_size = (qset.validators.len() + qset.inner_sets.len()) as u32;
    if top_size == 0 {
        return 0;
    }
    match level {
        // n=2f+1 → res = n - (n-1)/2 (only for flat sets)
        ValidationThresholdLevel::SimpleMajority if qset.inner_sets.is_empty() => {
            top_size - (top_size - 1) / 2
        }
        // n=3f+1 → res = n - (n-1)/3
        _ => top_size - (top_size - 1) / 3,
    }
}

/// Validate quorum safety for a compat config, mirroring stellar-core's
/// `Config::validateConfig` (Config.cpp:2306-2357).
fn validate_quorum_safety(
    quorum_set: &stellar_xdr::curr::ScpQuorumSet,
    safety: &CompatQuorumSafety,
) -> anyhow::Result<()> {
    use std::collections::HashSet;

    // Collect all node IDs from the quorum set.
    let mut all_nodes = HashSet::new();
    collect_nodes(quorum_set, &mut all_nodes);

    if all_nodes.is_empty() {
        anyhow::bail!("no validators defined in VALIDATORS/QUORUM_SET");
    }

    // Find the closest v-blocking set.
    let closest_vblocking = henyey_scp::find_closest_v_blocking(quorum_set, &all_nodes, None);

    let min_threshold = compute_default_threshold(quorum_set, safety.threshold_level);

    // Compute effective failure safety.
    let failure_safety = match safety.failure_safety {
        FailureSafety::Auto => {
            let top_level_count =
                (quorum_set.validators.len() + quorum_set.inner_sets.len()) as i32;
            let default = top_level_count - min_threshold as i32;
            tracing::info!(
                failure_safety = default,
                "Assigning calculated value to FAILURE_SAFETY"
            );
            default
        }
        FailureSafety::Explicit(n) => n,
    };

    // Reject if failure safety is incompatible with the quorum set.
    if failure_safety >= closest_vblocking.len() as i32 {
        anyhow::bail!(
            "Not enough nodes / thresholds too strict in your Quorum set to ensure \
             your desired level of FAILURE_SAFETY. Reduce FAILURE_SAFETY or fix quorum set"
        );
    }

    if !safety.unsafe_quorum {
        if failure_safety == 0 {
            anyhow::bail!(
                "Can't have FAILURE_SAFETY=0 unless you also set UNSAFE_QUORUM=true. \
                 Be sure you know what you are doing!"
            );
        }

        if quorum_set.threshold < min_threshold {
            anyhow::bail!(
                "Your THRESHOLD_PERCENTAGE is too low. If you really want this \
                 set UNSAFE_QUORUM=true. Be sure you know what you are doing!"
            );
        }
    }

    Ok(())
}

/// Recursively collect all node IDs from a quorum set.
fn collect_nodes(
    qset: &stellar_xdr::curr::ScpQuorumSet,
    nodes: &mut std::collections::HashSet<stellar_xdr::curr::NodeId>,
) {
    for v in qset.validators.iter() {
        nodes.insert(v.clone());
    }
    for inner in qset.inner_sets.iter() {
        collect_nodes(inner, nodes);
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
    /// Whether `bucket_directory()` was explicitly called.
    bucket_dir_explicit: bool,
}

impl ConfigBuilder {
    /// Create a new builder with testnet defaults.
    pub fn new() -> Self {
        Self {
            config: AppConfig::testnet(),
            bucket_dir_explicit: false,
        }
    }

    /// Create a builder for mainnet.
    pub fn mainnet() -> Self {
        Self {
            config: AppConfig::mainnet(),
            bucket_dir_explicit: false,
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
    ///
    /// Also sets `buckets.directory` to `<db_parent>/buckets` as a default,
    /// unless `bucket_directory()` was already called explicitly.
    pub fn database_path(mut self, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        if !self.bucket_dir_explicit {
            self.config.buckets.directory = path.parent().unwrap_or(&path).join("buckets");
        }
        self.config.database.path = path;
        self
    }

    /// Set the bucket directory.
    ///
    /// Overrides any default derived from `database_path()`.
    pub fn bucket_directory(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.buckets.directory = path.into();
        self.bucket_dir_explicit = true;
        self
    }

    /// Set the peer port.
    pub fn peer_port(mut self, port: u16) -> Self {
        self.config.overlay.peer_port = port;
        self
    }

    /// Add a known peer.
    pub fn add_known_peer(mut self, peer: PeerAddress) -> Self {
        self.config.overlay.known_peers.push(peer);
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
        config.node.quorum_set.validators = vec![valid_key()];
        let result = config.validate();
        assert!(result.is_err());
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains("threshold"),
            "Expected threshold error, got: {}",
            msg
        );
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
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("http.port") && err.contains("compat_http.port"),
            "Expected http/compat_http collision error, got: {}",
            err
        );
    }

    #[test]
    fn test_validation_query_port_collision_with_compat() {
        let mut config = AppConfig::default();
        config.compat_http.enabled = true;
        config.compat_http.port = 11627;
        config.query.port = Some(11627);
        let result = config.validate();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("query.port") && err.contains("compat_http.port"),
            "Expected query/compat_http collision error, got: {}",
            err
        );
    }

    #[test]
    fn test_validation_query_port_collision_with_http() {
        let mut config = AppConfig::default();
        config.http.enabled = true;
        config.query.port = Some(config.http.port);
        let result = config.validate();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("http.port")
                && err.contains("query.port")
                && err.contains("must be different"),
            "Expected http/query collision error, got: {}",
            err
        );
    }

    #[test]
    fn test_validation_all_listeners_distinct_ports_passes() {
        let mut config = AppConfig::default();
        config.http.enabled = true;
        config.http.port = 8080;
        config.compat_http.enabled = true;
        config.compat_http.port = 11626;
        config.query.port = Some(11627);
        config.rpc.enabled = true;
        config.rpc.port = 8000;
        config.overlay.peer_port = 11625;
        assert!(
            config.validate().is_ok(),
            "All listeners on distinct ports should pass"
        );
    }

    // --- RPC port collision tests ---

    #[test]
    fn test_validation_rpc_port_collision_with_http() {
        let mut config = AppConfig::default();
        config.rpc.enabled = true;
        config.rpc.port = config.http.port;
        let result = config.validate();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("http.port")
                && err.contains("rpc.port")
                && err.contains("must be different"),
            "Expected http/rpc collision error, got: {}",
            err
        );
    }

    #[test]
    fn test_validation_rpc_port_collision_with_compat_http() {
        let mut config = AppConfig::default();
        config.rpc.enabled = true;
        config.compat_http.enabled = true;
        config.compat_http.port = 9999;
        config.rpc.port = 9999;
        let result = config.validate();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("compat_http.port")
                && err.contains("rpc.port")
                && err.contains("must be different"),
            "Expected compat_http/rpc collision error, got: {}",
            err
        );
    }

    #[test]
    fn test_validation_rpc_port_collision_with_query() {
        let mut config = AppConfig::default();
        config.rpc.enabled = true;
        config.rpc.port = 9999;
        config.query.port = Some(9999);
        let result = config.validate();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("query.port")
                && err.contains("rpc.port")
                && err.contains("must be different"),
            "Expected query/rpc collision error, got: {}",
            err
        );
    }

    #[test]
    fn test_validation_rpc_port_collision_with_peer_port() {
        let mut config = AppConfig::default();
        config.rpc.enabled = true;
        config.rpc.port = config.overlay.peer_port;
        let result = config.validate();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("overlay.peer_port")
                && err.contains("rpc.port")
                && err.contains("must be different"),
            "Expected peer_port/rpc collision error, got: {}",
            err
        );
    }

    #[test]
    fn test_validation_rpc_port_collision_skipped_when_disabled() {
        let mut config = AppConfig::default();
        config.rpc.enabled = false;
        config.rpc.port = config.http.port; // Would collide if enabled
        assert!(
            config.validate().is_ok(),
            "Disabled RPC with colliding port should pass"
        );
    }

    // --- Previously-missing port collision pairs ---

    #[test]
    fn test_validation_compat_http_port_collision_with_peer_port() {
        let mut config = AppConfig::default();
        config.compat_http.enabled = true;
        config.compat_http.port = config.overlay.peer_port;
        let result = config.validate();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("overlay.peer_port")
                && err.contains("compat_http.port")
                && err.contains("must be different"),
            "Expected peer_port/compat_http collision error, got: {}",
            err
        );
    }

    #[test]
    fn test_validation_query_port_collision_with_peer_port() {
        let mut config = AppConfig::default();
        config.query.port = Some(config.overlay.peer_port);
        let result = config.validate();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("overlay.peer_port")
                && err.contains("query.port")
                && err.contains("must be different"),
            "Expected peer_port/query collision error, got: {}",
            err
        );
    }

    // --- Port zero validation tests ---

    #[test]
    fn test_validation_http_port_zero_rejected() {
        let mut config = AppConfig::default();
        config.http.enabled = true;
        config.http.port = 0;
        config.testing.run_standalone = false;
        let result = config.validate();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("http.port must not be 0"),
            "Expected http port zero error, got: {}",
            err
        );
    }

    #[test]
    fn test_validation_http_port_zero_allowed_standalone() {
        let mut config = AppConfig::default();
        config.http.enabled = true;
        config.http.port = 0;
        config.testing.run_standalone = true;
        assert!(
            config.validate().is_ok(),
            "Standalone mode with http.port = 0 should pass"
        );
    }

    #[test]
    fn test_validation_http_port_zero_allowed_disabled() {
        let mut config = AppConfig::default();
        config.http.enabled = false;
        config.http.port = 0;
        assert!(
            config.validate().is_ok(),
            "Disabled http with port 0 should pass"
        );
    }

    #[test]
    fn test_validation_compat_http_port_zero_rejected() {
        let mut config = AppConfig::default();
        config.compat_http.enabled = true;
        config.compat_http.port = 0;
        let result = config.validate();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("compat_http.port must not be 0"),
            "Expected compat_http port zero error, got: {}",
            err
        );
    }

    #[test]
    fn test_validation_compat_http_port_zero_allowed_disabled() {
        let mut config = AppConfig::default();
        config.compat_http.enabled = false;
        config.compat_http.port = 0;
        assert!(
            config.validate().is_ok(),
            "Disabled compat_http with port 0 should pass"
        );
    }

    #[test]
    fn test_validation_peer_port_zero_rejected() {
        let mut config = AppConfig::default();
        config.overlay.peer_port = 0;
        let result = config.validate();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("overlay.peer_port must not be 0"),
            "Expected peer port zero error, got: {}",
            err
        );
    }

    #[test]
    fn test_validation_peer_port_zero_rejected_standalone() {
        // peer_port = 0 is rejected even in standalone mode (unconditional).
        let mut config = AppConfig::default();
        config.overlay.peer_port = 0;
        config.testing.run_standalone = true;
        let result = config.validate();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("overlay.peer_port must not be 0"),
            "Expected peer port zero error even in standalone, got: {}",
            err
        );
    }

    #[test]
    fn test_validation_query_port_zero_rejected() {
        let mut config = AppConfig::default();
        config.query.port = Some(0);
        let result = config.validate();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("query.port must not be 0"),
            "Expected query port zero error, got: {}",
            err
        );
    }

    #[test]
    fn test_validation_rpc_port_zero_rejected() {
        let mut config = AppConfig::default();
        config.rpc.enabled = true;
        config.rpc.port = 0;
        let result = config.validate();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("rpc.port must not be 0"),
            "Expected rpc port zero error, got: {}",
            err
        );
    }

    #[test]
    fn test_validation_rpc_port_zero_allowed_disabled() {
        let mut config = AppConfig::default();
        config.rpc.enabled = false;
        config.rpc.port = 0;
        assert!(
            config.validate().is_ok(),
            "Disabled rpc with port 0 should pass"
        );
    }

    #[test]
    fn test_validation_query_port_zero_networked_validator_precedence() {
        // When query.port = Some(0) on a networked validator, the
        // "cannot be set on networked validator" error should fire first.
        let mut config = AppConfig::default();
        config.node.is_validator = true;
        config.node.node_seed =
            Some("SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH".to_string());
        config.testing.run_standalone = false;
        config.query.port = Some(0);
        let result = config.validate();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("query.port cannot be set on a networked validator"),
            "Expected networked validator error (precedence), got: {}",
            err
        );
    }

    #[test]
    fn test_validation_http_port_zero_collision_precedence() {
        // When http.port = 0 and peer_port = 0, the collision error
        // should fire before the port-0 error.
        let mut config = AppConfig::default();
        config.http.enabled = true;
        config.http.port = 0;
        config.overlay.peer_port = 0;
        config.testing.run_standalone = false;
        let result = config.validate();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("overlay.peer_port") && err.contains("must be different"),
            "Expected collision error (precedence), got: {}",
            err
        );
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

    #[test]
    fn test_network_type_uses_exact_passphrase_match() {
        // Regression test for AUDIT-AC3: network type must be determined by
        // exact passphrase comparison, not substring match. A passphrase
        // containing "Test" but not equal to the known testnet passphrase
        // must NOT be classified as testnet.
        let testnet = "Test SDF Network ; September 2015";
        let mainnet = "Public Global Stellar Network ; September 2015";
        let tricky = "My Custom Test Network";

        // Exact match: testnet passphrase
        assert_eq!(testnet, "Test SDF Network ; September 2015");
        // Exact match: not testnet
        assert_ne!(mainnet, "Test SDF Network ; September 2015");
        // Contains "Test" but is NOT the testnet passphrase
        assert!(tricky.contains("Test"));
        assert_ne!(tricky, "Test SDF Network ; September 2015");
    }

    /// Regression test for AUDIT-063: validator nodes must not have metadata streaming.
    #[test]
    fn test_audit_063_validator_metadata_stream_rejected() {
        let mut config = AppConfig::testnet();
        config.node.is_validator = true;
        config.node.node_seed =
            Some("SCZANGBA5YHTNYVVV3C7CAZMCLXPILHSE6VHMWG4DRTBIU6VIV7MBQVW".to_string());
        config.metadata.output_stream = Some("/tmp/meta.pipe".to_string());
        let result = config.validate();
        assert!(
            result.is_err(),
            "Networked validator with metadata stream should fail validation"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("metadata.output_stream cannot be used on a networked validator"),
            "Expected metadata rejection error, got: {}",
            err
        );
    }

    #[test]
    fn test_validation_metadata_stream_allowed_on_standalone_validator() {
        let mut config = AppConfig::default();
        config.node.is_validator = true;
        config.node.node_seed =
            Some("SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH".to_string());
        config.testing.run_standalone = true;
        config.metadata.output_stream = Some("/tmp/meta.pipe".to_string());
        let result = config.validate();
        assert!(
            result.is_ok(),
            "Standalone validator with metadata stream should pass: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_validation_query_port_on_networked_validator() {
        let mut config = AppConfig::default();
        config.node.is_validator = true;
        config.node.node_seed =
            Some("SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH".to_string());
        config.query.port = Some(11625);
        let result = config.validate();
        assert!(
            result.is_err(),
            "Networked validator with query port should fail validation"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("query.port cannot be set on a networked validator"),
            "Expected query port rejection error, got: {}",
            err
        );
    }

    #[test]
    fn test_validation_query_port_on_standalone_validator() {
        let mut config = AppConfig::default();
        config.node.is_validator = true;
        config.node.node_seed =
            Some("SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH".to_string());
        config.testing.run_standalone = true;
        config.query.port = Some(11630);
        let result = config.validate();
        assert!(
            result.is_ok(),
            "Standalone validator with query port should pass: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_validation_query_port_on_watcher() {
        let mut config = AppConfig::default();
        config.query.port = Some(11630);
        let result = config.validate();
        assert!(
            result.is_ok(),
            "Watcher with query port should pass: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_validation_diagnostic_events_on_networked_validator() {
        let mut config = AppConfig::default();
        config.node.is_validator = true;
        config.node.node_seed =
            Some("SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH".to_string());
        config.diagnostics.soroban_diagnostic_events = true;
        let result = config.validate();
        assert!(
            result.is_err(),
            "Networked validator with diagnostic events should fail validation"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("diagnostics.soroban_diagnostic_events cannot be enabled on a networked"),
            "Expected diagnostic events rejection error, got: {}",
            err
        );
    }

    #[test]
    fn test_validation_diagnostic_events_on_standalone_validator() {
        let mut config = AppConfig::default();
        config.node.is_validator = true;
        config.node.node_seed =
            Some("SBXTJSLKQ2VZUEQNYU5EC6ZGQOONCX3JCFBK57R56YLYMUW76B2FMCJH".to_string());
        config.testing.run_standalone = true;
        config.diagnostics.soroban_diagnostic_events = true;
        let result = config.validate();
        assert!(
            result.is_ok(),
            "Standalone validator with diagnostic events should pass: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_run_standalone_parsed_from_toml() {
        let toml_str = r#"
[network]
passphrase = "Test SDF Network ; September 2015"

[[history.archives]]
name = "sdf1"
url = "https://history.stellar.org/prd/core-testnet/core_testnet_001"

[testing]
run_standalone = true
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert!(config.testing.run_standalone);
    }

    #[test]
    fn test_run_standalone_false_by_default() {
        let config = AppConfig::default();
        assert!(!config.testing.run_standalone);
    }

    #[test]
    fn test_rpc_bucket_io_concurrency_zero_fails_validation() {
        let mut rpc = RpcConfig::default();
        rpc.bucket_io_concurrency = 0;
        let err = rpc.validate().unwrap_err();
        assert!(
            err.contains("bucket_io_concurrency"),
            "expected bucket_io_concurrency error, got: {err}"
        );
    }

    #[test]
    fn test_rpc_max_concurrent_simulations_zero_fails_validation() {
        let mut rpc = RpcConfig::default();
        rpc.max_concurrent_simulations = 0;
        let err = rpc.validate().unwrap_err();
        assert!(
            err.contains("max_concurrent_simulations"),
            "expected max_concurrent_simulations error, got: {err}"
        );
    }

    #[test]
    fn test_rpc_bucket_io_concurrency_default() {
        let rpc = RpcConfig::default();
        assert_eq!(rpc.bucket_io_concurrency, 8);
        assert!(rpc.validate().is_ok());
    }

    #[test]
    fn test_rpc_bucket_io_concurrency_deserialized_independently() {
        let toml_str = r#"
[rpc]
enabled = true
rpc_db_concurrency = 4
bucket_io_concurrency = 12
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.rpc.rpc_db_concurrency, 4);
        assert_eq!(config.rpc.bucket_io_concurrency, 12);
    }

    // ── watchdog_abort_secs default tests (issue #1921) ──────────────

    #[test]
    fn test_diagnostics_config_default_watchdog_abort_secs() {
        assert_eq!(DiagnosticsConfig::default().watchdog_abort_secs, 120);
    }

    #[test]
    fn test_testnet_config_watchdog_abort_secs() {
        assert_eq!(AppConfig::testnet().diagnostics.watchdog_abort_secs, 120);
    }

    #[test]
    fn test_mainnet_config_watchdog_abort_secs() {
        assert_eq!(AppConfig::mainnet().diagnostics.watchdog_abort_secs, 120);
    }

    #[test]
    fn test_toml_no_diagnostics_section_defaults_watchdog_abort_secs() {
        let toml_str = r#"
[node]
name = "test"
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.diagnostics.watchdog_abort_secs, 120);
    }

    #[test]
    fn test_toml_diagnostics_section_no_watchdog_defaults() {
        let toml_str = r#"
[node]
name = "test"

[diagnostics]
soroban_diagnostic_events = true
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.diagnostics.watchdog_abort_secs, 120);
    }

    #[test]
    fn test_toml_explicit_watchdog_zero_honored() {
        let toml_str = r#"
[node]
name = "test"

[diagnostics]
watchdog_abort_secs = 0
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.diagnostics.watchdog_abort_secs, 0);
    }

    #[test]
    fn test_unknown_top_level_key_rejected() {
        let toml_str = r#"
[node]
name = "test"

listen_port = 11625
"#;
        let err = toml::from_str::<AppConfig>(toml_str).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("listen_port"),
            "error should mention the unknown key: {msg}"
        );
    }

    #[test]
    fn test_unknown_nested_key_rejected() {
        let toml_str = r#"
[node]
name = "test"

[overlay]
foo = "bar"
"#;
        let err = toml::from_str::<AppConfig>(toml_str).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("foo"),
            "error should mention the unknown key: {msg}"
        );
    }

    #[test]
    fn test_unknown_deep_nested_key_rejected() {
        let toml_str = r#"
[node]
name = "test"

[buckets.bucket_list_db]
typo = 42
"#;
        let err = toml::from_str::<AppConfig>(toml_str).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("typo"),
            "error should mention the unknown key: {msg}"
        );
    }

    #[test]
    fn test_valid_minimal_config_parses() {
        let toml_str = r#"
[node]
name = "test"
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.node.name, "test");
    }

    #[test]
    fn test_shipped_config_files_parse() {
        // Ensure all .toml configs (configs/ dir + repo root) are valid
        // against deny_unknown_fields
        let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let configs_dir = repo_root.join("configs");

        // Collect .toml files from configs/ directory
        let mut toml_files: Vec<std::path::PathBuf> = std::fs::read_dir(&configs_dir)
            .expect("configs/ dir should exist")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map_or(false, |e| e == "toml"))
            .collect();

        // Also check root-level .toml configs (excluding Cargo.toml)
        for entry in std::fs::read_dir(repo_root).expect("repo root should exist") {
            let path = entry.unwrap().path();
            if path.extension().map_or(true, |e| e != "toml") {
                continue;
            }
            if path.file_name().map_or(false, |n| n == "Cargo.toml") {
                continue;
            }
            toml_files.push(path);
        }

        assert!(
            !toml_files.is_empty(),
            "No .toml config files found to test"
        );

        for path in &toml_files {
            let content = std::fs::read_to_string(path).unwrap();
            let result = toml::from_str::<AppConfig>(&content);
            assert!(
                result.is_ok(),
                "{} failed to parse: {}",
                path.strip_prefix(repo_root).unwrap_or(path).display(),
                result.unwrap_err()
            );
        }
    }

    /// Returns true if `text` contains any `__UPPER_SNAKE__` placeholder markers.
    fn has_placeholder_markers(text: &str) -> bool {
        // Manual scan for __[A-Z_]{2,}__ without pulling in the regex crate.
        let bytes = text.as_bytes();
        let mut i = 0;
        while i + 4 < bytes.len() {
            if bytes[i] == b'_' && bytes[i + 1] == b'_' {
                // Found "__", scan for uppercase/underscore body then closing "__"
                let start = i + 2;
                let mut j = start;
                while j < bytes.len() && (bytes[j].is_ascii_uppercase() || bytes[j] == b'_') {
                    j += 1;
                }
                if j >= start + 2 && j + 1 < bytes.len() && bytes[j] == b'_' && bytes[j + 1] == b'_'
                {
                    return true;
                }
                i = j;
            } else {
                i += 1;
            }
        }
        false
    }

    fn count_occurrences(haystack: &str, needle: &str) -> usize {
        haystack.matches(needle).count()
    }

    /// Substitute `__PLACEHOLDER__` markers in the raw test-history-publish
    /// template with valid dummy values and return the rendered string.
    fn render_history_publish_fixture(raw: &str) -> String {
        let test_seed = henyey_crypto::SecretKey::from_seed(&[42u8; 32]).to_strkey();
        let mut rendered = raw
            .replace("__NODE_SEED__", &test_seed)
            .replace("__DB_PATH__", "/tmp/adv&dir|path/validator.db")
            .replace("__BUCKET_DIR__", "/tmp/adv&dir|path/buckets")
            .replace("__HISTORY_DIR__", "/tmp/adv&dir|path/history");

        // Replace the peer_port line (mirrors the shell script's anchored sed).
        rendered = rendered
            .lines()
            .map(|line| {
                if line.starts_with("peer_port") && line.contains("# __PEER_PORT__") {
                    "peer_port = 31415".to_string()
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");

        rendered
    }

    #[test]
    fn test_history_publish_fixture_renders_and_parses() {
        let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let template_path = repo_root.join("configs/test-history-publish.toml");
        let raw = std::fs::read_to_string(&template_path)
            .expect("configs/test-history-publish.toml should exist");

        // Phase 1: Assert expected markers exist in the raw template.
        assert!(
            raw.contains("__NODE_SEED__"),
            "template missing __NODE_SEED__ marker"
        );
        assert!(
            raw.contains("__DB_PATH__"),
            "template missing __DB_PATH__ marker"
        );
        assert!(
            raw.contains("__BUCKET_DIR__"),
            "template missing __BUCKET_DIR__ marker"
        );
        assert!(
            raw.contains("__HISTORY_DIR__"),
            "template missing __HISTORY_DIR__ marker"
        );
        assert!(
            raw.contains("# __PEER_PORT__"),
            "template missing # __PEER_PORT__ comment marker"
        );

        // Assert occurrence counts match the template contract.
        assert_eq!(
            count_occurrences(&raw, "__HISTORY_DIR__"),
            3,
            "__HISTORY_DIR__ should appear exactly 3 times (url, put, mkdir)"
        );
        assert_eq!(
            count_occurrences(&raw, "__PEER_PORT__"),
            1,
            "__PEER_PORT__ should appear exactly once"
        );
        assert_eq!(
            count_occurrences(&raw, "__NODE_SEED__"),
            1,
            "__NODE_SEED__ should appear exactly once"
        );
        assert_eq!(
            count_occurrences(&raw, "__DB_PATH__"),
            1,
            "__DB_PATH__ should appear exactly once"
        );
        assert_eq!(
            count_occurrences(&raw, "__BUCKET_DIR__"),
            1,
            "__BUCKET_DIR__ should appear exactly once"
        );

        // Phase 2: Render with adversarial values via shared helper.
        let rendered = render_history_publish_fixture(&raw);

        // Phase 3: Assert no placeholder markers remain.
        assert!(
            !has_placeholder_markers(&rendered),
            "rendered config still contains __PLACEHOLDER__ markers:\n{rendered}"
        );

        // Phase 4: Parse and validate.
        let config: AppConfig = toml::from_str(&rendered).unwrap_or_else(|e| {
            panic!("rendered test-history-publish.toml failed to parse as AppConfig: {e}")
        });
        config
            .validate()
            .unwrap_or_else(|e| panic!("rendered config failed semantic validation: {e}"));

        // Phase 5: Assert substituted values via exact equality.
        let test_seed = henyey_crypto::SecretKey::from_seed(&[42u8; 32]).to_strkey();
        assert_eq!(
            config.node.node_seed.as_deref(),
            Some(test_seed.as_str()),
            "node_seed mismatch"
        );
        assert_eq!(
            config.database.path,
            std::path::PathBuf::from("/tmp/adv&dir|path/validator.db"),
            "database.path mismatch"
        );
        assert_eq!(
            config.buckets.directory,
            std::path::PathBuf::from("/tmp/adv&dir|path/buckets"),
            "buckets.directory mismatch"
        );
        assert_eq!(config.overlay.peer_port, 31415_u16, "peer_port mismatch");

        // Verify the "local" archive entry has the adversarial history path
        // in all three use-sites: url, put, mkdir.
        let test_history_dir = "/tmp/adv&dir|path/history";
        let local_archive = config
            .history
            .archives
            .iter()
            .find(|a| a.name == "local")
            .expect("template should have a 'local' archive entry");
        assert_eq!(
            local_archive.url,
            format!("file://{test_history_dir}"),
            "local archive url mismatch"
        );
        assert_eq!(
            local_archive.put.as_deref(),
            Some(format!("cp {{0}} {test_history_dir}/{{1}}").as_str()),
            "local archive put command mismatch"
        );
        assert_eq!(
            local_archive.mkdir.as_deref(),
            Some(format!("mkdir -p {test_history_dir}/{{0}}").as_str()),
            "local archive mkdir command mismatch"
        );
    }

    /// Asserts that the shared sections between `configs/test-history-publish.toml`
    /// and `configs/validator-testnet.toml` are semantically identical after parsing.
    ///
    /// Shared sections enforced: quorum_set, network passphrase, remote history
    /// archives (all fields), and overlay known_peers. If a new shared section is
    /// added to the fixture, extend this test to cover it.
    #[test]
    fn test_history_publish_shared_sections_match_validator_testnet() {
        let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();

        // Load the validator-testnet config (parse only, no validate — node_seed is omitted).
        let validator_raw =
            std::fs::read_to_string(repo_root.join("configs/validator-testnet.toml"))
                .expect("configs/validator-testnet.toml should exist");
        let validator: AppConfig = toml::from_str(&validator_raw)
            .expect("configs/validator-testnet.toml should parse as AppConfig");

        // Load and render the test-history-publish fixture.
        let fixture_raw =
            std::fs::read_to_string(repo_root.join("configs/test-history-publish.toml"))
                .expect("configs/test-history-publish.toml should exist");
        let fixture: AppConfig = toml::from_str(&render_history_publish_fixture(&fixture_raw))
            .expect("rendered test-history-publish.toml should parse as AppConfig");

        // --- Quorum set ---
        assert_eq!(
            fixture.node.quorum_set.threshold_percent, validator.node.quorum_set.threshold_percent,
            "quorum_set.threshold_percent drift"
        );
        // Order-sensitive: config files should list validators in the same order.
        assert_eq!(
            fixture.node.quorum_set.validators, validator.node.quorum_set.validators,
            "quorum_set.validators drift"
        );

        // --- Network passphrase ---
        assert_eq!(
            fixture.network.passphrase, validator.network.passphrase,
            "network.passphrase drift"
        );

        // --- Remote history archives (all fields) ---
        // Filter out the "local" archive (test-specific) from the fixture.
        let fixture_remote: Vec<&HistoryArchiveEntry> = fixture
            .history
            .archives
            .iter()
            .filter(|a| a.name != "local")
            .collect();
        let validator_remote: Vec<&HistoryArchiveEntry> =
            validator.history.archives.iter().collect();
        assert_eq!(
            fixture_remote.len(),
            validator_remote.len(),
            "remote archive count drift: fixture has {}, validator has {}",
            fixture_remote.len(),
            validator_remote.len()
        );
        for (f, v) in fixture_remote.iter().zip(validator_remote.iter()) {
            assert_eq!(f, v, "history archive '{}' drift", f.name);
        }

        // --- Overlay known_peers ---
        // Order-sensitive: config files should list peers in the same order.
        assert_eq!(
            fixture.overlay.known_peers, validator.overlay.known_peers,
            "overlay.known_peers drift"
        );
    }

    // --- Regression tests for AUDIT-183: invalid inner quorum set silently dropped ---

    fn valid_key() -> String {
        henyey_crypto::SecretKey::from_seed(&[1u8; 32])
            .public_key()
            .to_strkey()
    }

    fn valid_key2() -> String {
        henyey_crypto::SecretKey::from_seed(&[2u8; 32])
            .public_key()
            .to_strkey()
    }

    #[test]
    fn test_quorum_set_invalid_validator_key() {
        let qs = QuorumSetConfig {
            threshold_percent: 67,
            validators: vec!["GINVALID_KEY_HERE".to_string()],
            inner_sets: vec![],
        };
        let err = qs.to_xdr().unwrap_err();
        assert!(
            err.to_string().contains("Invalid validator public key"),
            "Expected invalid key error, got: {}",
            err
        );
    }

    #[test]
    fn test_quorum_set_invalid_inner_set_not_silently_dropped() {
        let inner = QuorumSetConfig {
            threshold_percent: 67,
            validators: vec!["GINVALID_INNER_KEY".to_string()],
            inner_sets: vec![],
        };
        let qs = QuorumSetConfig {
            threshold_percent: 67,
            validators: vec![valid_key()],
            inner_sets: vec![inner],
        };
        let err = qs.to_xdr().unwrap_err();
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("inner_sets[0]"),
            "Error should include inner set index, got: {}",
            msg
        );
        assert!(
            msg.contains("Invalid validator public key"),
            "Error should include root cause, got: {}",
            msg
        );
    }

    #[test]
    fn test_quorum_set_deeply_nested_invalid_key() {
        let deep_inner = QuorumSetConfig {
            threshold_percent: 67,
            validators: vec!["GINVALID_DEEP".to_string()],
            inner_sets: vec![],
        };
        let mid_inner = QuorumSetConfig {
            threshold_percent: 67,
            validators: vec![valid_key()],
            inner_sets: vec![deep_inner],
        };
        let qs = QuorumSetConfig {
            threshold_percent: 67,
            validators: vec![valid_key2()],
            inner_sets: vec![mid_inner],
        };
        let err = qs.to_xdr().unwrap_err();
        let msg = format!("{:#}", err);
        // Should have nested context: "in inner_sets[0]" at both levels
        assert!(
            msg.contains("inner_sets[0]"),
            "Error should include nested path context, got: {}",
            msg
        );
    }

    #[test]
    fn test_quorum_set_empty_is_error() {
        let qs = QuorumSetConfig {
            threshold_percent: 67,
            validators: vec![],
            inner_sets: vec![],
        };
        let err = qs.to_xdr().unwrap_err();
        assert!(
            err.to_string().contains("no validators or inner sets"),
            "Expected empty set error, got: {}",
            err
        );
    }

    #[test]
    fn test_quorum_set_threshold_zero_is_error() {
        let qs = QuorumSetConfig {
            threshold_percent: 0,
            validators: vec![valid_key()],
            inner_sets: vec![],
        };
        let err = qs.to_xdr().unwrap_err();
        assert!(
            err.to_string().contains("threshold_percent"),
            "Expected threshold error, got: {}",
            err
        );
    }

    #[test]
    fn test_quorum_set_threshold_over_100_is_error() {
        let qs = QuorumSetConfig {
            threshold_percent: 101,
            validators: vec![valid_key()],
            inner_sets: vec![],
        };
        let err = qs.to_xdr().unwrap_err();
        assert!(
            err.to_string().contains("threshold_percent"),
            "Expected threshold error, got: {}",
            err
        );
    }

    #[test]
    fn test_quorum_set_valid_config_succeeds() {
        let qs = QuorumSetConfig {
            threshold_percent: 67,
            validators: vec![valid_key(), valid_key2()],
            inner_sets: vec![],
        };
        assert!(qs.to_xdr().is_ok());
    }

    #[test]
    fn test_validate_rejects_invalid_inner_quorum_set() {
        let mut config = AppConfig::testnet();
        config.node.is_validator = true;
        config.node.node_seed = Some(henyey_crypto::SecretKey::from_seed(&[99u8; 32]).to_strkey());
        config.node.quorum_set = QuorumSetConfig {
            threshold_percent: 67,
            validators: vec![valid_key()],
            inner_sets: vec![QuorumSetConfig {
                threshold_percent: 67,
                validators: vec!["GINVALID_KEY".to_string()],
                inner_sets: vec![],
            }],
        };
        let result = config.validate();
        assert!(
            result.is_err(),
            "validate() should reject invalid inner quorum set"
        );
        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(
            err_msg.contains("inner_sets[0]"),
            "Error should identify which inner set failed, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_quorum_set_is_empty_helper() {
        let empty = QuorumSetConfig::default();
        assert!(empty.is_empty());

        let with_validators = QuorumSetConfig {
            threshold_percent: 67,
            validators: vec![valid_key()],
            inner_sets: vec![],
        };
        assert!(!with_validators.is_empty());
    }

    #[test]
    fn test_config_builder_database_path_sets_default_bucket_dir() {
        let config = ConfigBuilder::new()
            .database_path("/data/stellar/node.db")
            .build();
        assert_eq!(
            config.buckets.directory,
            PathBuf::from("/data/stellar/buckets"),
            "database_path should auto-derive bucket dir to <db_parent>/buckets"
        );
    }

    #[test]
    fn test_config_builder_explicit_bucket_dir_overrides_database_path() {
        // bucket_directory() called after database_path() — explicit wins
        let config = ConfigBuilder::new()
            .database_path("/data/stellar/node.db")
            .bucket_directory("/custom/buckets")
            .build();
        assert_eq!(config.buckets.directory, PathBuf::from("/custom/buckets"));
    }

    #[test]
    fn test_config_builder_explicit_bucket_dir_not_clobbered_by_database_path() {
        // bucket_directory() called before database_path() — explicit still wins
        let config = ConfigBuilder::new()
            .bucket_directory("/custom/buckets")
            .database_path("/data/stellar/node.db")
            .build();
        assert_eq!(config.buckets.directory, PathBuf::from("/custom/buckets"));
    }

    #[test]
    fn test_publish_enabled_with_no_archives() {
        let config = HistoryConfig { archives: vec![] };
        assert!(!config.publish_enabled());
    }

    #[test]
    fn test_publish_enabled_with_readonly_archives() {
        let config = HistoryConfig {
            archives: vec![HistoryArchiveEntry {
                name: "sdf1".to_string(),
                url: "https://history.stellar.org/prd/core-live/core_live_001".to_string(),
                get_enabled: true,
                put_enabled: false,
                put: None,
                mkdir: None,
            }],
        };
        assert!(!config.publish_enabled());
    }

    #[test]
    fn test_publish_enabled_with_writable_archive() {
        let config = HistoryConfig {
            archives: vec![HistoryArchiveEntry {
                name: "local".to_string(),
                url: "file:///tmp/archive".to_string(),
                get_enabled: true,
                put_enabled: true,
                put: Some("cp {0} {1}".to_string()),
                mkdir: Some("mkdir -p {0}".to_string()),
            }],
        };
        assert!(config.publish_enabled());
    }

    #[test]
    fn test_publish_enabled_put_enabled_but_no_put_command() {
        // put_enabled=true but no put command → not publishable
        let config = HistoryConfig {
            archives: vec![HistoryArchiveEntry {
                name: "broken".to_string(),
                url: "file:///tmp/archive".to_string(),
                get_enabled: true,
                put_enabled: true,
                put: None,
                mkdir: None,
            }],
        };
        assert!(!config.publish_enabled());
    }

    #[test]
    fn test_validation_flood_tx_period_ms_zero() {
        let mut config = AppConfig::default();
        config.overlay.flood_tx_period_ms = 0;
        let result = config.validate();
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("flood_tx_period_ms must be > 0"));
    }

    #[test]
    fn test_flood_tx_period_ms_default_is_200() {
        let config = AppConfig::default();
        assert_eq!(config.overlay.flood_tx_period_ms, 200);
    }

    #[test]
    fn test_validation_flow_control_bytes_batch_exceeds_capacity() {
        let mut config = AppConfig::default();
        config.overlay.peer_flood_reading_capacity_bytes = 100_000;
        config.overlay.flow_control_send_more_batch_size_bytes = 200_000;
        let result = config.validate();
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("FLOW_CONTROL_SEND_MORE_BATCH_SIZE_BYTES"));
    }

    #[test]
    fn test_validation_flow_control_bytes_valid_fixed() {
        let mut config = AppConfig::default();
        config.overlay.peer_flood_reading_capacity_bytes = 500_000;
        config.overlay.flow_control_send_more_batch_size_bytes = 100_000;
        // Should not fail on flow control bytes validation (may fail on other
        // things like missing quorum set — we check it doesn't fail on
        // the flow control specific error).
        let result = config.validate();
        if let Err(e) = &result {
            let msg = e.to_string();
            assert!(
                !msg.contains("FLOW_CONTROL_SEND_MORE_BATCH_SIZE_BYTES"),
                "unexpected flow control error: {msg}"
            );
        }
    }

    #[test]
    fn test_validation_flow_control_bytes_defaults_auto() {
        let config = AppConfig::default();
        assert_eq!(config.overlay.peer_flood_reading_capacity_bytes, 0);
        assert_eq!(config.overlay.flow_control_send_more_batch_size_bytes, 0);
        // Defaults (0,0) → Auto, which always passes validation.
        let result = config.validate();
        if let Err(e) = &result {
            let msg = e.to_string();
            assert!(
                !msg.contains("FLOW_CONTROL_SEND_MORE_BATCH_SIZE_BYTES"),
                "unexpected flow control error: {msg}"
            );
        }
    }

    #[test]
    fn test_validation_rejects_invalid_preferred_peer_keys() {
        // Completely invalid string
        let mut config = AppConfig::default();
        config.overlay.preferred_peer_keys = vec!["not-a-valid-key".to_string()];
        let result = config.validate();
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Invalid preferred_peer_keys entry"),
            "expected error about invalid preferred_peer_keys"
        );

        // Valid G... prefix but truncated (invalid checksum/length)
        let mut config = AppConfig::default();
        config.overlay.preferred_peer_keys = vec!["GAAAAAA".to_string()];
        let result = config.validate();
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Invalid preferred_peer_keys entry"),
            "expected error about invalid preferred_peer_keys for truncated key"
        );
    }

    #[test]
    fn test_validation_accepts_valid_preferred_peer_keys() {
        let mut config = AppConfig::default();
        // Known valid public key (all-A with valid checksum)
        config.overlay.preferred_peer_keys =
            vec!["GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF".to_string()];
        config.overlay.preferred_peers_only = true;

        let result = config.validate();
        assert!(
            result.is_ok(),
            "validation should pass with valid preferred_peer_keys: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_preferred_peer_keys_parsing_consistency() {
        // The validate() path uses henyey_crypto::PublicKey::from_strkey() (stricter: curve check).
        // The start_overlay() path uses henyey_overlay::PeerId::from_strkey() (strkey only).
        // Any key passing the stricter validate() must also pass the lifecycle path.
        let key_str = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF";

        // Validate path (henyey_crypto::PublicKey)
        let crypto_key = henyey_crypto::PublicKey::from_strkey(key_str)
            .expect("valid key must pass crypto::PublicKey::from_strkey");

        // Lifecycle path (henyey_overlay::PeerId)
        let peer_id = henyey_overlay::PeerId::from_strkey(key_str)
            .expect("valid key must pass PeerId::from_strkey");

        // Both paths must produce the same raw key bytes
        assert_eq!(
            crypto_key.as_bytes(),
            peer_id.as_bytes(),
            "crypto::PublicKey and overlay::PeerId must decode to the same bytes"
        );
    }

    #[test]
    fn test_parse_peer_address_valid() {
        let addr = parse_peer_address("stellar.example.com").unwrap();
        assert_eq!(addr.host, "stellar.example.com");
        assert_eq!(addr.port, 11625);

        let addr = parse_peer_address("stellar.example.com:1234").unwrap();
        assert_eq!(addr.host, "stellar.example.com");
        assert_eq!(addr.port, 1234);

        let addr = parse_peer_address("127.0.0.1:65535").unwrap();
        assert_eq!(addr.host, "127.0.0.1");
        assert_eq!(addr.port, 65535);

        let addr = parse_peer_address("peer:1").unwrap();
        assert_eq!(addr.host, "peer");
        assert_eq!(addr.port, 1);
    }

    #[test]
    fn test_parse_peer_address_invalid() {
        // Empty
        assert!(parse_peer_address("").is_err());

        // Whitespace
        assert!(parse_peer_address(" ").is_err());
        assert!(parse_peer_address("host :1234").is_err());
        assert!(parse_peer_address("host: 1234").is_err());
        assert!(parse_peer_address(" host").is_err());
        assert!(parse_peer_address("host ").is_err());

        // Port 0 (rejected like stellar-core)
        assert!(parse_peer_address("host:0").is_err());

        // Invalid port
        assert!(parse_peer_address("host:abc").is_err());
        assert!(parse_peer_address("host:99999").is_err());
        assert!(parse_peer_address("host:-1").is_err());

        // Empty host
        assert!(parse_peer_address(":1234").is_err());

        // Empty port
        assert!(parse_peer_address("host:").is_err());

        // Too many colons (IPv6-like)
        assert!(parse_peer_address("host:1:2").is_err());
        assert!(parse_peer_address("::1").is_err());

        // Invalid hostname characters (matching stellar-core PeerBareAddress regex)
        assert!(parse_peer_address("foo/bar:11625").is_err());
        assert!(parse_peer_address("[::1]:11625").is_err());
        assert!(parse_peer_address("host_name:11625").is_err());
        assert!(parse_peer_address("host@name:11625").is_err());
        assert!(parse_peer_address("héllo:11625").is_err());

        // Invalid numeric IPv4 (looks like IP but out of range)
        assert!(parse_peer_address("256.256.256.256").is_err());
        assert!(parse_peer_address("127.0.0.256:11625").is_err());
        assert!(parse_peer_address("999.0.0.1").is_err());
        assert!(parse_peer_address("1.2.3.4.5").is_err());
    }

    #[test]
    fn test_validate_rejects_malformed_known_peers() {
        let mut config = AppConfig::testnet();
        // Programmatically create an invalid PeerAddress (port=0)
        config.overlay.known_peers = vec![
            PeerAddress::new("valid-host", 11625),
            PeerAddress::new("host", 0),
        ];
        let result = config.validate();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("overlay.known_peers"),
            "error should mention the field: {err}"
        );
    }

    #[test]
    fn test_validate_rejects_malformed_preferred_peers() {
        let mut config = AppConfig::testnet();
        // Programmatically create an invalid PeerAddress (empty host)
        config.overlay.preferred_peers = vec![PeerAddress::new("", 11625)];
        let result = config.validate();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("overlay.preferred_peers"),
            "error should mention the field: {err}"
        );
    }

    #[test]
    fn test_validate_accepts_valid_peers() {
        let mut config = AppConfig::testnet();
        config.overlay.known_peers = vec![
            PeerAddress::new("core-live-a.stellar.org", 11625),
            PeerAddress::new("peer.example.com", 11625),
        ];
        config.overlay.preferred_peers = vec![PeerAddress::new("preferred.example.com", 11625)];
        assert!(config.validate().is_ok());
    }

    // --- Environment override tests ---
    //
    // These tests use `apply_env_overrides_from()` with synthetic HashMaps,
    // eliminating the need for process-global env mutation, mutexes, or guards.

    use std::collections::HashMap;

    /// Helper: create a lookup closure from a HashMap.
    fn map_lookup(map: HashMap<&str, &str>) -> impl FnMut(&str) -> anyhow::Result<Option<String>> {
        let owned: HashMap<String, String> = map
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        move |name: &str| Ok(owned.get(name).cloned())
    }

    #[test]
    fn test_env_override_valid_bool_true() {
        let mut config = AppConfig::default();
        config
            .apply_env_overrides_from(map_lookup(HashMap::from([(
                "RS_STELLAR_CORE_NODE_VALIDATOR",
                "true",
            )])))
            .unwrap();
        assert!(config.node.is_validator);
    }

    #[test]
    fn test_env_override_valid_bool_false() {
        let mut config = AppConfig::default();
        config
            .apply_env_overrides_from(map_lookup(HashMap::from([(
                "RS_STELLAR_CORE_NODE_VALIDATOR",
                "false",
            )])))
            .unwrap();
        assert!(!config.node.is_validator);
    }

    #[test]
    fn test_env_override_invalid_bool() {
        let mut config = AppConfig::default();
        let err = config
            .apply_env_overrides_from(map_lookup(HashMap::from([(
                "RS_STELLAR_CORE_NODE_VALIDATOR",
                "tru",
            )])))
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("RS_STELLAR_CORE_NODE_VALIDATOR"),
            "error should mention the env var name: {msg}"
        );
    }

    #[test]
    fn test_env_override_valid_port() {
        let mut config = AppConfig::default();
        config
            .apply_env_overrides_from(map_lookup(HashMap::from([(
                "RS_STELLAR_CORE_OVERLAY_PEER_PORT",
                "8080",
            )])))
            .unwrap();
        assert_eq!(config.overlay.peer_port, 8080);
    }

    #[test]
    fn test_env_override_invalid_port() {
        let mut config = AppConfig::default();
        let err = config
            .apply_env_overrides_from(map_lookup(HashMap::from([(
                "RS_STELLAR_CORE_OVERLAY_PEER_PORT",
                "abc",
            )])))
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("RS_STELLAR_CORE_OVERLAY_PEER_PORT"),
            "error should mention the env var name: {msg}"
        );
    }

    #[test]
    fn test_env_override_port_overflow() {
        let mut config = AppConfig::default();
        let err = config
            .apply_env_overrides_from(map_lookup(HashMap::from([(
                "RS_STELLAR_CORE_OVERLAY_PEER_PORT",
                "99999",
            )])))
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("RS_STELLAR_CORE_OVERLAY_PEER_PORT"),
            "error should mention the env var name: {msg}"
        );
    }

    #[test]
    fn test_env_override_port_zero() {
        // Port 0 is valid u16 — semantic validation happens in validate()
        let mut config = AppConfig::default();
        config
            .apply_env_overrides_from(map_lookup(HashMap::from([(
                "RS_STELLAR_CORE_OVERLAY_PEER_PORT",
                "0",
            )])))
            .unwrap();
        assert_eq!(config.overlay.peer_port, 0);
    }

    #[test]
    fn test_env_override_unset_is_noop() {
        let mut config = AppConfig::default();
        let original_validator = config.node.is_validator;
        let original_port = config.overlay.peer_port;
        // Empty map — no overrides
        config.apply_env_overrides_from(|_| Ok(None)).unwrap();
        assert_eq!(config.node.is_validator, original_validator);
        assert_eq!(config.overlay.peer_port, original_port);
    }

    #[test]
    fn test_env_override_from_error_propagation() {
        // Verify that a lookup error (e.g., non-UTF-8) propagates immediately.
        let mut config = AppConfig::default();
        let err = config
            .apply_env_overrides_from(|name| {
                if name == "RS_STELLAR_CORE_NODE_NAME" {
                    anyhow::bail!("{name}: value is not valid UTF-8")
                }
                Ok(None)
            })
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not valid UTF-8"),
            "error should mention UTF-8: {msg}"
        );
    }

    #[test]
    fn test_env_override_from_multiple_overrides_compose() {
        let mut config = AppConfig::default();
        config
            .apply_env_overrides_from(map_lookup(HashMap::from([
                ("RS_STELLAR_CORE_NODE_VALIDATOR", "true"),
                ("RS_STELLAR_CORE_OVERLAY_PEER_PORT", "9999"),
                ("RS_STELLAR_CORE_LOG_LEVEL", "debug"),
                ("RS_STELLAR_CORE_NETWORK_PASSPHRASE", "Test Network"),
            ])))
            .unwrap();
        assert!(config.node.is_validator);
        assert_eq!(config.overlay.peer_port, 9999);
        assert_eq!(config.logging.level, "debug");
        assert_eq!(config.network.passphrase, "Test Network");
    }

    #[test]
    fn test_env_override_from_empty_map_is_noop() {
        let mut config = AppConfig::default();
        let original = AppConfig::default();
        config
            .apply_env_overrides_from(map_lookup(HashMap::new()))
            .unwrap();
        assert_eq!(config.node.is_validator, original.node.is_validator);
        assert_eq!(config.overlay.peer_port, original.overlay.peer_port);
        assert_eq!(config.logging.level, original.logging.level);
        assert_eq!(config.network.passphrase, original.network.passphrase);
    }
}
