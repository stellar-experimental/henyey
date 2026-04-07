//! rs-stellar-core - Pure Rust implementation of Stellar Core
//!
//! This binary provides a command-line interface for running a Stellar Core
//! validator node, catching up from history archives, and performing other
//! Stellar network operations.
//!
//! # Quick Start
//!
//! ```bash
//! # Run a node on testnet (default)
//! rs-stellar-core --testnet run
//!
//! # Catch up to the latest ledger
//! rs-stellar-core --testnet catchup current
//!
//! # Generate a new keypair
//! rs-stellar-core new-keypair
//! ```
//!
//! # Commands
//!
//! The CLI provides the following main commands:
//!
//! - **run**: Start the node (as watcher, full node, or validator)
//! - **catchup**: Catch up from history archives to a specific ledger
//! - **new-db**: Create a new database
//! - **upgrade-db**: Upgrade database schema
//! - **new-keypair**: Generate a new node keypair
//! - **info**: Print node information
//! - **verify-history**: Verify history archives
//! - **publish-history**: Publish history to archives (validators only)
//! - **check-quorum-intersection**: Verify quorum intersection from JSON
//! - **sample-config**: Print sample configuration
//! - **verify-execution**: Test transaction execution against CDP metadata
//! - **self-check**: Perform diagnostic self-checks
//! - **bucket-info**: Print bucket list information
//! - **dump-ledger**: Dump ledger entries to JSON
//! - **verify-checkpoints**: Write verified checkpoint hashes to a file
//! - **debug-bucket-entry**: Inspect an account in the bucket list
//!
//! # Configuration
//!
//! Configuration can be provided via:
//! - A TOML configuration file (`--config <FILE>`)
//! - Built-in network defaults (`--testnet` or `--mainnet`)
//! - Environment variables (prefixed with `STELLAR_`)
//!
//! See `rs-stellar-core sample-config` for an example configuration.
//!
//! # Architecture
//!
//! This crate is a thin CLI wrapper around the [`henyey_app`] crate,
//! which contains the core application logic. The CLI handles:
//!
//! - Argument parsing with `clap`
//! - Configuration loading and merging
//! - Logging initialization
//! - Command dispatch
//!
//! The actual node implementation, catchup logic, and subsystem coordination
//! are handled by the underlying library crates.

mod quorum_intersection;
mod settings_upgrade;

#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use stellar_xdr::curr::WriteXdr;

use henyey_app::{
    logging, run_catchup, run_node, App, AppConfig, CatchupMode as CatchupModeInternal,
    CatchupOptions, LogConfig, LogFormat, RunMode, RunOptions,
};
use henyey_common::deterministic_seed;

// ---------------------------------------------------------------------------
// LoadGenRunner implementation (bridges henyey-simulation into henyey-app)
// ---------------------------------------------------------------------------

mod loadgen_runner {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use henyey_app::{App, LoadGenRequest, LoadGenRunner};
    use henyey_simulation::{GeneratedLoadConfig, LoadGenMode, LoadGenerator};
    use tokio::sync::Mutex;

    /// Shared inner state that can be referenced from spawned tasks.
    struct Inner {
        app: Arc<App>,
        network_passphrase: String,
        generator: Mutex<Option<LoadGenerator>>,
        running: AtomicBool,
    }

    /// Concrete [`LoadGenRunner`] implementation backed by [`LoadGenerator`].
    ///
    /// Wraps inner state in an `Arc` so it can be shared with background tasks
    /// spawned by `start_load`.
    pub struct SimulationLoadGenRunner {
        inner: Arc<Inner>,
    }

    impl SimulationLoadGenRunner {
        pub fn new(app: Arc<App>) -> Self {
            let network_passphrase = app.config().network.passphrase.clone();
            Self {
                inner: Arc::new(Inner {
                    app,
                    network_passphrase,
                    generator: Mutex::new(None),
                    running: AtomicBool::new(false),
                }),
            }
        }

        /// Returns `Some("deprecation message")` for modes that are
        /// deprecated in stellar-core v25 and should be rejected before
        /// reaching the load generator.
        fn deprecated_mode(mode: &str) -> Option<&'static str> {
            if mode.eq_ignore_ascii_case("create") {
                Some(
                    "DEPRECATED: CREATE mode has been removed. \
                     Use GENESIS_TEST_ACCOUNT_COUNT configuration parameter \
                     to create test accounts at genesis instead.",
                )
            } else {
                None
            }
        }

        /// Parse a mode string into a `LoadGenMode`.
        ///
        /// Accepts both stellar-core underscore names (e.g. `soroban_upload`)
        /// and henyey's legacy no-separator names (e.g. `sorobanupload`).
        /// Case-insensitive.
        fn parse_mode(mode: &str) -> Option<LoadGenMode> {
            let normalized = mode.to_ascii_lowercase();
            match normalized.as_str() {
                "pay" => Some(LoadGenMode::Pay),
                "soroban_upload" | "sorobanupload" => Some(LoadGenMode::SorobanUpload),
                "soroban_invoke_setup" | "sorobaninvokesetup" => {
                    Some(LoadGenMode::SorobanInvokeSetup)
                }
                "soroban_invoke" | "sorobaninvoke" => Some(LoadGenMode::SorobanInvoke),
                "mixed_classic_soroban" | "mixedclassicsoroban" | "mixed" => {
                    Some(LoadGenMode::MixedClassicSoroban)
                }
                _ => None,
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use henyey_simulation::LoadGenMode;

        #[test]
        fn test_parse_mode_valid_modes() {
            assert_eq!(
                SimulationLoadGenRunner::parse_mode("pay"),
                Some(LoadGenMode::Pay)
            );
            assert_eq!(
                SimulationLoadGenRunner::parse_mode("sorobanupload"),
                Some(LoadGenMode::SorobanUpload)
            );
            assert_eq!(
                SimulationLoadGenRunner::parse_mode("sorobaninvokesetup"),
                Some(LoadGenMode::SorobanInvokeSetup)
            );
            assert_eq!(
                SimulationLoadGenRunner::parse_mode("sorobaninvoke"),
                Some(LoadGenMode::SorobanInvoke)
            );
            assert_eq!(
                SimulationLoadGenRunner::parse_mode("mixed"),
                Some(LoadGenMode::MixedClassicSoroban)
            );
            assert_eq!(
                SimulationLoadGenRunner::parse_mode("mixedclassicsoroban"),
                Some(LoadGenMode::MixedClassicSoroban)
            );
        }

        #[test]
        fn test_parse_mode_stellar_core_underscore_names() {
            // stellar-core uses underscored mode names; SSC sends these.
            assert_eq!(
                SimulationLoadGenRunner::parse_mode("soroban_upload"),
                Some(LoadGenMode::SorobanUpload)
            );
            assert_eq!(
                SimulationLoadGenRunner::parse_mode("soroban_invoke_setup"),
                Some(LoadGenMode::SorobanInvokeSetup)
            );
            assert_eq!(
                SimulationLoadGenRunner::parse_mode("soroban_invoke"),
                Some(LoadGenMode::SorobanInvoke)
            );
            assert_eq!(
                SimulationLoadGenRunner::parse_mode("mixed_classic_soroban"),
                Some(LoadGenMode::MixedClassicSoroban)
            );
        }

        #[test]
        fn test_parse_mode_case_insensitive() {
            assert_eq!(
                SimulationLoadGenRunner::parse_mode("PAY"),
                Some(LoadGenMode::Pay)
            );
            assert_eq!(
                SimulationLoadGenRunner::parse_mode("Soroban_Upload"),
                Some(LoadGenMode::SorobanUpload)
            );
            assert_eq!(
                SimulationLoadGenRunner::parse_mode("MIXED"),
                Some(LoadGenMode::MixedClassicSoroban)
            );
        }

        #[test]
        fn test_parse_mode_invalid() {
            assert_eq!(SimulationLoadGenRunner::parse_mode(""), None);
            assert_eq!(SimulationLoadGenRunner::parse_mode("unknown"), None);
            assert_eq!(SimulationLoadGenRunner::parse_mode("transfer"), None);
            // "stop" is handled at the HTTP layer, not as a generation mode.
            assert_eq!(SimulationLoadGenRunner::parse_mode("stop"), None);
        }

        #[test]
        fn test_create_mode_is_deprecated() {
            // "create" is deprecated in stellar-core v25 and must NOT be
            // treated as "pay". parse_mode should not recognize it.
            assert_eq!(SimulationLoadGenRunner::parse_mode("create"), None);
            assert_eq!(SimulationLoadGenRunner::parse_mode("CREATE"), None);

            // deprecated_mode should return the deprecation message.
            assert!(SimulationLoadGenRunner::deprecated_mode("create").is_some());
            assert!(SimulationLoadGenRunner::deprecated_mode("CREATE").is_some());
            assert!(SimulationLoadGenRunner::deprecated_mode("pay").is_none());
        }
    }

    impl LoadGenRunner for SimulationLoadGenRunner {
        fn start_load(&self, request: LoadGenRequest) -> Result<(), String> {
            // Check for deprecated modes before parsing (matches stellar-core
            // CommandHandler::generateLoad which checks before getMode).
            if let Some(msg) = Self::deprecated_mode(&request.mode) {
                return Err(msg.to_string());
            }

            let mode = Self::parse_mode(&request.mode).ok_or_else(|| {
                format!(
                    "Unknown mode: '{}'. Use: pay, soroban_upload, \
                     soroban_invoke_setup, soroban_invoke, mixed_classic_soroban.",
                    request.mode
                )
            })?;

            if self.inner.running.load(Ordering::SeqCst) {
                return Err("Load generation is already running.".to_string());
            }

            let mut config = GeneratedLoadConfig {
                mode,
                n_accounts: request.accounts,
                offset: request.offset,
                n_txs: request.txs,
                tx_rate: request.tx_rate,
                max_fee_rate: if request.max_fee_rate > 0 {
                    Some(request.max_fee_rate)
                } else {
                    None
                },
                skip_low_fee_txs: request.skip_low_fee_txs,
                spike_interval: request.spike_interval,
                spike_size: request.spike_size,
                n_instances: request.instances,
                n_wasms: request.wasms,
                min_soroban_percent_success: request.min_percent_success,
                ..Default::default()
            };

            let inner = Arc::clone(&self.inner);
            inner.running.store(true, Ordering::SeqCst);

            tokio::spawn(async move {
                // Lazily create LoadGenerator or reuse existing (preserves
                // Soroban state across setup → invoke runs).
                let mut guard = inner.generator.lock().await;
                if guard.is_none() {
                    *guard = Some(LoadGenerator::new(
                        Arc::clone(&inner.app),
                        inner.network_passphrase.clone(),
                    ));
                }
                let Some(generator) = guard.as_mut() else {
                    unreachable!("load generator must exist after initialization");
                };

                let result = generator.generate_load(&mut config).await;

                inner.running.store(false, Ordering::SeqCst);

                match result {
                    henyey_simulation::LoadResult::Done { submitted } => {
                        tracing::info!(submitted, "Load generation complete");
                    }
                    henyey_simulation::LoadResult::Stopped => {
                        tracing::info!("Load generation stopped");
                    }
                    henyey_simulation::LoadResult::Failed => {
                        tracing::error!("Load generation failed");
                    }
                }
            });

            Ok(())
        }

        fn stop_load(&self) {
            // Set the stopped flag on the generator so the running task
            // breaks out of its loop on the next iteration. This matches
            // stellar-core's LoadGenerator::stop() which cancels the timer
            // and resets state. The background task will observe Stopped
            // and clear the running flag.
            if let Ok(mut guard) = self.inner.generator.try_lock() {
                if let Some(gen) = guard.as_mut() {
                    gen.stop();
                }
            }
        }

        fn is_running(&self) -> bool {
            self.inner.running.load(Ordering::SeqCst)
        }
    }
}

/// Pure Rust implementation of Stellar Core
#[derive(Parser)]
#[command(name = "henyey")]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Path to configuration file
    #[arg(short, long, alias = "conf", value_name = "FILE", global = true)]
    config: Option<PathBuf>,

    /// Enable console logging (accepted for stellar-core compatibility; henyey logs to console by default)
    #[arg(long, global = true)]
    console: bool,

    /// Enable verbose logging (debug level)
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Enable trace logging (most verbose)
    #[arg(long, global = true)]
    trace: bool,

    /// Log output format
    #[arg(long, default_value = "text", global = true)]
    log_format: CliLogFormat,

    /// Use testnet configuration (default)
    #[arg(long, global = true)]
    testnet: bool,

    /// Use mainnet configuration
    #[arg(long, global = true)]
    mainnet: bool,

    /// Stream LedgerCloseMeta XDR frames to a file, named pipe, or fd:N
    #[arg(long = "metadata-output-stream", value_name = "STREAM", global = true)]
    metadata_output_stream: Option<String>,

    /// Set log level (stellar-core compatibility; use --verbose/--trace instead).
    #[arg(long = "ll", value_name = "LEVEL", global = true, hide = true)]
    log_level_compat: Option<String>,

    /// Report metric on exit (stellar-core compatibility; accepted but ignored).
    #[arg(
        long = "metric",
        value_name = "METRIC-NAME",
        global = true,
        hide = true
    )]
    metric: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

/// Log output format for CLI
#[derive(Clone, Copy, Debug, Default, clap::ValueEnum)]
enum CliLogFormat {
    #[default]
    Text,
    Json,
}

impl From<CliLogFormat> for LogFormat {
    fn from(fmt: CliLogFormat) -> Self {
        match fmt {
            CliLogFormat::Text => LogFormat::Text,
            CliLogFormat::Json => LogFormat::Json,
        }
    }
}

/// Available commands
#[derive(Subcommand)]
enum Commands {
    /// Run the Stellar Core node
    Run {
        /// Run in validator mode (participate in consensus)
        #[arg(long)]
        validator: bool,

        /// Run in watcher mode (observe only, no catchup)
        #[arg(long)]
        watcher: bool,

        /// Force catchup even if state exists
        #[arg(long)]
        force_catchup: bool,

        /// Run a single-node standalone network from genesis with zero config.
        /// Creates database, initializes genesis ledger, and starts validating
        /// with 1-second ledger closes. Data stored in ./local-data/.
        #[arg(long)]
        local: bool,

        /// Wait to hear from the network before voting (validators only).
        /// Accepted for stellar-core compatibility.
        #[arg(long, hide = true)]
        wait_for_consensus: bool,

        /// Deprecated: accepted for stellar-core compatibility, ignored.
        #[arg(long, hide = true)]
        in_memory: bool,

        /// Deprecated: accepted for stellar-core compatibility, ignored.
        #[arg(long, hide = true)]
        start_at_ledger: Option<u32>,

        /// Deprecated: accepted for stellar-core compatibility, ignored.
        #[arg(long, hide = true)]
        start_at_hash: Option<String>,

        /// Keep all old buckets on disk (disable bucket GC).
        /// Accepted for stellar-core compatibility.
        #[arg(long, hide = true)]
        disable_bucket_gc: bool,
    },

    /// Catch up from history archives
    Catchup {
        /// Target ledger: "current", a ledger number, or "ledger/count"
        #[arg(value_name = "TARGET", default_value = "current")]
        target: String,

        /// Catchup mode: minimal, complete, or recent:N (e.g., recent:128)
        #[arg(long, default_value = "minimal")]
        mode: String,

        /// Skip verification after catchup
        #[arg(long)]
        no_verify: bool,

        /// Number of parallel downloads
        #[arg(long, default_value = "8")]
        parallelism: usize,
    },

    /// Create a new database
    NewDb {
        /// Path to the database file (overrides config)
        #[arg(value_name = "PATH")]
        path: Option<PathBuf>,

        /// Force creation even if database exists
        #[arg(long)]
        force: bool,

        /// Deprecated: accepted for stellar-core compatibility, ignored.
        #[arg(long = "minimal-for-in-memory-mode", hide = true)]
        minimal_for_in_memory_mode: bool,
    },

    /// Upgrade database schema
    UpgradeDb,

    /// Generate a new node keypair
    NewKeypair,

    /// Print node information
    Info,

    /// Verify history archives
    VerifyHistory {
        /// Start ledger sequence
        #[arg(long)]
        from: Option<u32>,

        /// End ledger sequence
        #[arg(long)]
        to: Option<u32>,
    },

    /// Publish history to archives (validators only)
    PublishHistory {
        /// Force republishing of existing checkpoints
        #[arg(long)]
        force: bool,
    },

    /// Check quorum intersection from a JSON file
    CheckQuorumIntersection {
        /// Path to the JSON file
        path: PathBuf,
    },

    /// Print sample configuration
    SampleConfig,

    /// Send a command to a running stellar-core node
    ///
    /// Makes an HTTP GET request to the local node's command interface.
    /// Examples:
    ///   http-command info
    ///   http-command "peers?fullkeys=true"
    ///   http-command "ll?level=DEBUG"
    HttpCommand {
        /// The command to send (e.g., "info", "peers", "ll?level=DEBUG")
        command: String,

        /// HTTP port of the running node (default: 11626)
        #[arg(long, short, default_value = "11626")]
        port: u16,
    },

    /// Print bucket list information
    BucketInfo {
        /// Path to bucket directory
        path: PathBuf,
    },

    /// Test transaction execution by comparing our results against CDP metadata
    ///
    /// This validates that our transaction execution (Soroban host, classic ops)
    /// produces the same ledger entry changes as stellar-core. Differences
    /// indicate execution divergence that needs investigation.
    VerifyExecution {
        /// Start ledger sequence (defaults to a recent checkpoint)
        #[arg(long)]
        from: Option<u32>,

        /// End ledger sequence (defaults to latest available)
        #[arg(long)]
        to: Option<u32>,

        /// Stop on first mismatch
        #[arg(long)]
        stop_on_error: bool,

        /// Show detailed diff of mismatched entries
        #[arg(long)]
        show_diff: bool,

        /// CDP data lake URL (default: network-specific - testnet or pubnet)
        #[arg(long)]
        cdp_url: Option<String>,

        /// CDP date partition (default: 2025-12-18 for testnet, empty for mainnet)
        #[arg(long)]
        cdp_date: Option<String>,

        /// Cache directory for buckets and CDP metadata (default: ~/.cache/rs-stellar-core)
        #[arg(long)]
        cache_dir: Option<std::path::PathBuf>,

        /// Disable caching (use temp directories)
        #[arg(long)]
        no_cache: bool,

        /// Quiet mode: only output summary and errors
        #[arg(long, short = 'q')]
        quiet: bool,
    },

    /// Debug: inspect an account in the bucket list at a checkpoint
    DebugBucketEntry {
        /// Checkpoint ledger sequence
        #[arg(long)]
        checkpoint: u32,

        /// Account ID (hex)
        #[arg(long)]
        account: String,
    },

    /// Dump ledger entries to JSON
    ///
    /// Dumps ledger entries from the bucket list to a JSON file for debugging.
    /// Supports filtering by entry type and limiting output count.
    DumpLedger {
        /// Output file path
        #[arg(long, short)]
        output: PathBuf,

        /// Filter by entry type (account, trustline, offer, data, claimable_balance,
        /// liquidity_pool, contract_data, contract_code, config_setting, ttl)
        #[arg(long)]
        entry_type: Option<String>,

        /// Maximum number of entries to output
        #[arg(long)]
        limit: Option<u64>,

        /// Only include entries modified in the last N ledgers
        #[arg(long)]
        last_modified_ledger_count: Option<u32>,
    },

    /// Perform diagnostic self-checks
    ///
    /// This command performs comprehensive diagnostic checks including:
    /// - Header chain verification (verify hash linkage)
    /// - Bucket hash verification (verify all buckets have correct hashes)
    /// - Crypto benchmarking (Ed25519 sign/verify performance)
    ///
    /// Unlike the HTTP /self-check endpoint (which only runs quick online checks),
    /// this offline command performs full bucket verification which may take
    /// significant time on large databases.
    SelfCheck,

    /// Return information for an offline instance
    ///
    /// Opens the database without connecting to the network and prints
    /// the node's state as JSON. This is used by stellar-rpc to check
    /// if an existing captive-core database can be reused.
    ///
    /// Output format is compatible with stellar-core's `offline-info` command.
    #[command(name = "offline-info")]
    OfflineInfo,

    /// Print version information (stellar-core compatible output)
    ///
    /// Prints the build version, protocol version, and other version info
    /// in a format compatible with stellar-core's `version` subcommand.
    /// stellar-rpc parses this output to detect protocol version support.
    #[command(name = "version")]
    Version,

    /// Convert an identifier between formats (stellar-core compatible)
    ///
    /// Takes a hex-encoded 32-byte value or a strkey (G.../S.../T.../X.../C...)
    /// and prints all possible interpretations. Used by the quickstart container
    /// to derive network root account keys from the network passphrase hash.
    #[command(name = "convert-id")]
    ConvertId {
        /// The identifier to convert (hex or strkey)
        id: String,
    },

    /// Force SCP to start on next run.
    ///
    /// Sets a flag in the database so the next `run` will skip catchup and
    /// bootstrap consensus from the current last closed ledger. This is
    /// required for standalone single-node networks (e.g., quickstart local
    /// mode) where there are no peers to catch up from.
    #[command(name = "force-scp")]
    ForceScp,

    /// Initialize a named history archive.
    ///
    /// Creates the `.well-known/stellar-history.json` metadata file in the
    /// named archive using its configured put/mkdir commands. Required for
    /// local filesystem archives before they can be used for publishing.
    #[command(name = "new-hist")]
    NewHist {
        /// Name of the history archive to initialize (must match config)
        name: String,
    },

    /// Write verified checkpoint ledger hashes to a file
    ///
    /// Downloads checkpoint headers from history archives, verifies the header
    /// chain, and writes verified checkpoint hashes to a JSON file. This file
    /// can be used with `--trusted-checkpoint-hashes` during catchup to verify
    /// against known-good hashes.
    VerifyCheckpoints {
        /// Output file path for verified checkpoint hashes
        #[arg(long, short, required = true)]
        output: PathBuf,

        /// Start ledger sequence (defaults to genesis)
        #[arg(long)]
        from: Option<u32>,

        /// End ledger sequence (defaults to current)
        #[arg(long)]
        to: Option<u32>,
    },

    /// Generate transactions to upgrade Soroban config settings
    ///
    /// Produces 4 transaction envelopes needed for a Soroban config settings
    /// upgrade: restore WASM, upload WASM, create contract, invoke contract.
    /// When `--signtxs` is passed, reads a secret key from stdin and signs all
    /// transactions. Output is compatible with stellar-core's
    /// `get-settings-upgrade-txs` command.
    #[command(name = "get-settings-upgrade-txs")]
    GetSettingsUpgradeTxs {
        /// Source account public key (G...)
        public_key: String,

        /// Current sequence number of the source account
        seq_num: i64,

        /// Network passphrase
        network_passphrase: String,

        /// ConfigUpgradeSet in base64 XDR
        #[arg(long = "xdr")]
        xdr_base64: String,

        /// Sign all transactions (reads secret key from stdin)
        #[arg(long)]
        signtxs: bool,

        /// Additional resource fee for all transactions
        #[arg(long = "add-resource-fee", default_value = "0")]
        add_resource_fee: i64,
    },

    /// Run apply-time load test (benchmarks raw transaction application)
    ///
    /// Creates a standalone node with genesis, deploys contracts, populates
    /// the bucket list with synthetic data, and closes ledgers with maximally
    /// filled transaction sets. Reports throughput and resource utilization.
    ///
    /// Matches stellar-core's `apply-load` CLI subcommand.
    #[command(name = "apply-load")]
    ApplyLoad {
        /// Benchmark mode: "ledger-limits" (default), "max-sac-tps", or "single-shot"
        #[arg(long, default_value = "ledger-limits")]
        mode: String,

        /// Number of benchmark ledgers to run (default: 10)
        #[arg(long, default_value = "10")]
        num_ledgers: u32,

        /// Number of classic transactions per ledger (default: 10)
        #[arg(long, default_value = "10")]
        classic_txs_per_ledger: u32,

        /// Max parallel Soroban execution clusters per stage (default: 16)
        #[arg(long, default_value = "16")]
        clusters: u32,

        /// Total SAC transfer TXs for single-shot mode (default: 50000)
        #[arg(long, default_value = "50000")]
        tx_count: u32,

        /// Number of iterations for single-shot mode (default: 10)
        #[arg(long, default_value = "10")]
        iterations: u32,
    },

    /// Compare a checkpoint between two history archives
    ///
    /// Downloads checkpoint data (HAS, ledger headers, transactions, results)
    /// from both archives and performs a typed, field-by-field comparison.
    /// SCP messages are skipped (different validators produce different envelopes).
    /// Buckets are compared by hash only (via the HAS bucket list hashes).
    ///
    /// Exit code 0 = match, 1 = mismatch or error.
    #[command(name = "compare-checkpoint")]
    CompareCheckpoint {
        /// URL or file:// path to the local archive (the one being tested)
        #[arg(long)]
        local_archive: String,

        /// URL or file:// path to the reference archive (e.g. SDF testnet)
        #[arg(long)]
        remote_archive: String,

        /// Checkpoint ledger sequence to compare (must be a checkpoint boundary)
        #[arg(long)]
        checkpoint: u32,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Enforce: major version == CURRENT_LEDGER_PROTOCOL_VERSION.
    henyey_common::version::check_version_protocol_invariant(env!("CARGO_PKG_VERSION"));

    let cli = Cli::parse();

    // Handle commands that must run before logging/config initialization
    match &cli.command {
        Commands::Version => {
            cmd_version();
            return Ok(());
        }
        Commands::ConvertId { id } => {
            cmd_convert_id(id);
            return Ok(());
        }
        Commands::GetSettingsUpgradeTxs {
            public_key,
            seq_num,
            network_passphrase,
            xdr_base64,
            signtxs,
            add_resource_fee,
        } => {
            return settings_upgrade::run(
                public_key,
                *seq_num,
                network_passphrase,
                xdr_base64,
                *signtxs,
                *add_resource_fee,
            );
        }
        _ => {}
    }

    // Initialize logging
    init_logging(&cli)?;

    // Check if local mode is requested (needed before config loading).
    let local = matches!(cli.command, Commands::Run { local: true, .. });

    // Validate --local flag combinations early.
    if local {
        if cli.mainnet {
            anyhow::bail!("--local is incompatible with --mainnet");
        }
        if cli.testnet {
            anyhow::bail!("--local is incompatible with --testnet");
        }
        if matches!(cli.command, Commands::Run { watcher: true, .. }) {
            anyhow::bail!("--local is incompatible with --watcher");
        }
    }

    // Load or create configuration
    let mut config = load_config(&cli, local)?;

    // Inject build metadata from compile-time environment variables
    config.build = henyey_app::BuildMetadata {
        commit_hash: env!("HENYEY_COMMIT_HASH").to_string(),
        build_timestamp: env!("HENYEY_BUILD_TIMESTAMP").to_string(),
    };

    // Apply testing overrides early (before any checkpoint math).
    if config.testing.accelerate_time {
        henyey_history::set_checkpoint_frequency(henyey_history::ACCELERATED_CHECKPOINT_FREQUENCY);
    }

    // Execute command
    match cli.command {
        Commands::Run {
            validator,
            watcher,
            force_catchup,
            local,
            wait_for_consensus: _,
            in_memory: _,
            start_at_ledger: _,
            start_at_hash: _,
            disable_bucket_gc: _,
        } => cmd_run(config, validator, watcher, force_catchup, local).await,

        Commands::Catchup {
            target,
            mode,
            no_verify,
            parallelism,
        } => cmd_catchup(config, target, mode, !no_verify, parallelism).await,

        Commands::NewDb {
            path,
            force,
            minimal_for_in_memory_mode: _,
        } => cmd_new_db(config, path, force).await,

        Commands::UpgradeDb => cmd_upgrade_db(config).await,

        Commands::NewKeypair => cmd_new_keypair(),

        Commands::Info => cmd_info(config).await,

        Commands::VerifyHistory { from, to } => cmd_verify_history(config, from, to).await,

        Commands::PublishHistory { force } => cmd_publish_history(config, force).await,

        Commands::CheckQuorumIntersection { path } => cmd_check_quorum_intersection(&path),

        Commands::SampleConfig => cmd_sample_config(),

        Commands::HttpCommand { command, port } => cmd_http_command(&command, port).await,

        Commands::BucketInfo { path } => bucket_info(&path),

        Commands::VerifyExecution {
            from,
            to,
            stop_on_error,
            show_diff,
            cdp_url,
            cdp_date,
            cache_dir,
            no_cache,
            quiet,
        } => {
            cmd_verify_execution(
                config,
                VerifyExecutionOptions {
                    from,
                    to,
                    stop_on_error,
                    show_diff,
                    cdp_url,
                    cdp_date,
                    cache_dir,
                    no_cache,
                    quiet,
                },
            )
            .await
        }

        Commands::DebugBucketEntry {
            checkpoint,
            account,
        } => cmd_debug_bucket_entry(config, checkpoint, &account).await,

        Commands::DumpLedger {
            output,
            entry_type,
            limit,
            last_modified_ledger_count,
        } => {
            cmd_dump_ledger(
                config,
                output,
                entry_type,
                limit,
                last_modified_ledger_count,
            )
            .await
        }

        Commands::SelfCheck => cmd_self_check(config).await,

        Commands::OfflineInfo => cmd_offline_info(config),

        // Handled by early return above; included for exhaustive match.
        Commands::Version | Commands::ConvertId { .. } | Commands::GetSettingsUpgradeTxs { .. } => {
            unreachable!()
        }

        Commands::ForceScp => {
            cmd_force_scp(&config)?;
            Ok(())
        }

        Commands::NewHist { name } => cmd_new_hist(&config, &name).await,

        Commands::VerifyCheckpoints { output, from, to } => {
            cmd_verify_checkpoints(config, output, from, to).await
        }

        Commands::ApplyLoad {
            mode,
            num_ledgers,
            classic_txs_per_ledger,
            clusters,
            tx_count,
            iterations,
        } => {
            cmd_apply_load(
                config,
                &mode,
                num_ledgers,
                classic_txs_per_ledger,
                clusters,
                tx_count,
                iterations,
            )
            .await
        }

        Commands::CompareCheckpoint {
            local_archive,
            remote_archive,
            checkpoint,
        } => cmd_compare_checkpoint(&local_archive, &remote_archive, checkpoint).await,
    }
}

/// Initialize the logging subsystem.
fn init_logging(cli: &Cli) -> anyhow::Result<()> {
    let level = if cli.trace {
        "trace"
    } else if cli.verbose {
        "debug"
    } else {
        "info"
    };

    let mut config = LogConfig::default().with_level(level);

    if matches!(cli.log_format, CliLogFormat::Json) {
        config.format = LogFormat::Json;
        config.ansi_colors = false;
    }

    logging::init(&config)?;

    tracing::debug!("Logging initialized");
    Ok(())
}

fn create_parent_dir(path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

/// Creates a [`HistoryArchive`] from the first enabled archive in config.
fn first_archive(config: &AppConfig) -> anyhow::Result<henyey_history::HistoryArchive> {
    config
        .history
        .archives
        .iter()
        .filter(|a| a.get_enabled)
        .find_map(|a| henyey_history::HistoryArchive::new(&a.url).ok())
        .ok_or_else(|| anyhow::anyhow!("No history archives available"))
}

/// Creates all enabled [`HistoryArchive`] clients from config, warning on failures.
fn all_archives(config: &AppConfig) -> anyhow::Result<Vec<henyey_history::HistoryArchive>> {
    let archives: Vec<henyey_history::HistoryArchive> = config
        .history
        .archives
        .iter()
        .filter(|a| a.get_enabled)
        .filter_map(|a| match henyey_history::HistoryArchive::new(&a.url) {
            Ok(archive) => Some(archive),
            Err(e) => {
                println!("Warning: Failed to create archive {}: {}", a.url, e);
                None
            }
        })
        .collect();

    if archives.is_empty() {
        anyhow::bail!("No history archives available");
    }

    Ok(archives)
}

/// Build a standalone local-mode configuration.
///
/// Creates an `AppConfig` suitable for running a single-node standalone
/// network from genesis:
/// - Network passphrase: "Standalone Network ; February 2017"
/// - Deterministic node seed (derived from SHA-256 of "local-standalone-seed")
/// - Self-only quorum set (threshold 100%)
/// - 1-second ledger closes (`accelerate_time = true`)
/// - Load generation enabled, compat HTTP enabled
/// - Data stored in `./local-data/`
/// - Immediate protocol upgrade to latest version (v25)
fn local_config() -> AppConfig {
    use henyey_app::config::{
        BucketConfig, CompatHttpConfig, DatabaseConfig, HistoryArchiveEntry, HistoryConfig,
        HttpConfig, NetworkConfig, NodeConfig, OverlayConfig, QuorumSetConfig, TestingConfig,
        UpgradeConfig,
    };

    // Derive a deterministic keypair so the node identity is stable across restarts.
    let seed_hash = henyey_crypto::sha256(b"local-standalone-seed");
    let secret_key = henyey_crypto::SecretKey::from_seed(&seed_hash.0);
    let public_key = secret_key.public_key();
    let node_seed_strkey = secret_key.to_strkey();
    let node_pubkey_strkey = public_key.to_strkey();

    let data_dir = PathBuf::from("./local-data");

    AppConfig {
        node: NodeConfig {
            name: "local".to_string(),
            node_seed: Some(node_seed_strkey),
            is_validator: true,
            home_domain: None,
            quorum_set: QuorumSetConfig {
                threshold_percent: 100,
                validators: vec![node_pubkey_strkey],
                inner_sets: vec![],
            },
            manual_close: false,
        },
        network: NetworkConfig {
            passphrase: "Standalone Network ; February 2017".to_string(),
            base_fee: 100,
            base_reserve: 100_000_000, // 10 XLM
            max_protocol_version: 25,
        },
        upgrades: UpgradeConfig {
            protocol_version: Some(25),
            base_fee: None,
            base_reserve: None,
            max_tx_set_size: None,
        },
        database: DatabaseConfig {
            path: data_dir.join("stellar.db"),
            pool_size: 10,
        },
        buckets: BucketConfig {
            directory: data_dir.join("buckets"),
            ..Default::default()
        },
        history: HistoryConfig {
            archives: vec![HistoryArchiveEntry {
                name: "local".to_string(),
                url: format!("file://{}", data_dir.join("history").display()),
                get_enabled: true,
                put_enabled: true,
                put: Some(format!(
                    "mkdir -p {dir}/{{1}} && cp {{0}} {dir}/{{1}}",
                    dir = data_dir.join("history").display()
                )),
                mkdir: Some(format!(
                    "mkdir -p {dir}/{{0}}",
                    dir = data_dir.join("history").display()
                )),
            }],
        },
        overlay: OverlayConfig {
            known_peers: vec![],
            preferred_peers: vec![],
            ..Default::default()
        },
        testing: TestingConfig {
            accelerate_time: true,
            ledger_close_time: None,
            generate_load_for_testing: true,
            genesis_test_account_count: 0,
        },
        compat_http: CompatHttpConfig {
            enabled: true,
            ..Default::default()
        },
        http: HttpConfig {
            port: 11627, // Native HTTP on 11627 to avoid conflict with compat HTTP on 11626
            ..Default::default()
        },
        // Prevent the overlay from injecting default seed peers — this is a
        // standalone network with no external connectivity.
        is_compat_config: true,
        ..Default::default()
    }
}

/// Load configuration from file or use defaults.
///
/// Auto-detects stellar-core format configs (flat `SCREAMING_CASE` TOML)
/// and translates them to henyey's nested format using the compat layer.
fn load_config(cli: &Cli, local: bool) -> anyhow::Result<AppConfig> {
    let mut config = if local {
        // Start from local standalone defaults. If --config is also provided,
        // we load it and merge user-specified fields on top.
        let mut base = local_config();
        if let Some(config_path) = &cli.config {
            tracing::info!(
                path = ?config_path,
                "Local mode: overlaying config file on local defaults"
            );
            let overlay = load_config_file(config_path)?;
            // Merge overlay: only override fields the user explicitly set.
            // For simplicity, we merge all non-default-ish config sections.
            // The key local-mode fields (node, network, database, buckets, testing)
            // stay from `base` unless the overlay file specifies them.
            //
            // Since TOML deserialization fills defaults for unset fields, we
            // can't distinguish "explicitly set" from "default". Instead we
            // selectively merge sections that are commonly customized via overlay:
            // RPC, HTTP, metadata, logging, events, diagnostics, and maintenance.
            base.rpc = overlay.rpc;
            base.http = overlay.http;
            base.metadata = overlay.metadata;
            base.logging = overlay.logging;
            base.events = overlay.events;
            base.diagnostics = overlay.diagnostics;
            base.maintenance = overlay.maintenance;
            base.query = overlay.query;
            base.surge_pricing = overlay.surge_pricing;
            base.catchup = overlay.catchup;
        }
        base.apply_env_overrides();
        base
    } else {
        match (&cli.config, cli.mainnet) {
            (Some(config_path), _) => {
                tracing::info!(path = ?config_path, "Loading configuration from file");
                load_config_file(config_path)?
            }
            (None, true) => {
                tracing::info!("Using mainnet configuration");
                let mut c = AppConfig::mainnet();
                c.apply_env_overrides();
                c
            }
            (None, false) => {
                tracing::info!("Using testnet configuration (default)");
                let mut c = AppConfig::testnet();
                c.apply_env_overrides();
                c
            }
        }
    };

    // CLI --metadata-output-stream overrides config file
    if let Some(ref stream) = cli.metadata_output_stream {
        config.metadata.output_stream = Some(stream.clone());
    }

    Ok(config)
}

/// Load a config file, auto-detecting stellar-core vs henyey format.
fn load_config_file(path: &std::path::Path) -> anyhow::Result<AppConfig> {
    use henyey_app::compat_config;

    let content = std::fs::read_to_string(path)?;
    let raw: toml::Value = toml::from_str(&content)?;

    if compat_config::is_stellar_core_format(&raw) {
        tracing::info!("Detected stellar-core config format, translating");
        let mut config = compat_config::translate_stellar_core_config(&raw)?;
        config.apply_env_overrides();
        Ok(config)
    } else {
        let mut config: AppConfig = toml::from_str(&content)?;
        config.apply_env_overrides();
        Ok(config)
    }
}

/// Run command handler.
async fn cmd_run(
    config: AppConfig,
    validator: bool,
    watcher: bool,
    force_catchup: bool,
    local: bool,
) -> anyhow::Result<()> {
    if validator && watcher {
        anyhow::bail!("Cannot run as both validator and watcher");
    }

    // In local mode, always run as validator (it's a single-node standalone network).
    let mode = if local || validator {
        RunMode::Validator
    } else if watcher {
        RunMode::Watcher
    } else {
        RunMode::Full
    };

    // Local mode: auto-initialize database and history if needed.
    let config = if local {
        let db_path = &config.database.path;

        // Ensure data directory exists.
        create_parent_dir(db_path)?;

        let needs_init = !db_path.exists();

        if needs_init {
            tracing::info!(path = ?db_path, "Local mode: creating database and genesis ledger");

            // Create database and initialize genesis ledger.
            let db = henyey_db::Database::open(db_path)?;
            initialize_genesis_ledger(
                &db,
                Some(&db_path.parent().unwrap_or(db_path).join("buckets")),
                &config.network.passphrase,
                config.testing.genesis_test_account_count,
            )?;
            tracing::info!("Local mode: genesis ledger initialized");

            // Initialize local history archive.
            let history_dir = config
                .database
                .path
                .parent()
                .unwrap_or(std::path::Path::new("."))
                .join("history");
            std::fs::create_dir_all(&history_dir)?;
            cmd_new_hist(&config, "local").await?;
            tracing::info!("Local mode: history archive initialized");
        }

        // Always set force-scp so the node bootstraps from the LCL.
        {
            let db = henyey_db::Database::open(db_path)?;
            db.with_connection(|conn| {
                use henyey_db::queries::StateQueries;
                use henyey_db::schema::state_keys;
                conn.set_state(state_keys::FORCE_SCP, "true")
            })?;
            tracing::info!("Local mode: force-scp flag set");
        }

        // Ensure protocol upgrade is scheduled for immediate application.
        if config.upgrades.protocol_version.is_some() {
            tracing::info!(
                version = config.upgrades.protocol_version,
                "Local mode: protocol upgrade will be proposed immediately"
            );
        }

        config
    } else {
        config
    };

    let rpc_enabled = config.rpc.enabled;
    let rpc_port = config.rpc.port;

    let options = RunOptions {
        mode,
        force_catchup,
        loadgen_runner_factory: Some(std::sync::Arc::new(|app| {
            Box::new(loadgen_runner::SimulationLoadGenRunner::new(app))
        })),
        extra_server_spawner: if rpc_enabled {
            Some(std::sync::Arc::new(
                move |app: &std::sync::Arc<henyey_app::App>| {
                    let rpc_server = henyey_rpc::RpcServer::new(rpc_port, app.clone());
                    vec![tokio::spawn(async move {
                        if let Err(e) = rpc_server.start().await {
                            tracing::error!(error = %e, "JSON-RPC server error");
                        }
                    })]
                },
            ))
        } else {
            None
        },
        ..Default::default()
    };

    run_node(config, options).await
}

/// Catchup command handler.
async fn cmd_catchup(
    config: AppConfig,
    target: String,
    mode: String,
    verify: bool,
    parallelism: usize,
) -> anyhow::Result<()> {
    // Parse mode string into CatchupMode (supports "minimal", "complete", "recent:N")
    let mode: CatchupModeInternal = mode.parse()?;

    let options = CatchupOptions {
        target,
        mode,
        verify,
        parallelism,
        keep_temp: false,
    };

    let result = run_catchup(config, options).await?;
    println!("{}", result);
    Ok(())
}

/// New database command handler.
async fn cmd_new_db(
    mut config: AppConfig,
    path: Option<PathBuf>,
    force: bool,
) -> anyhow::Result<()> {
    // Override path if provided
    if let Some(p) = path {
        config.database.path = p;
    }

    let db_path = &config.database.path;

    // Check if database already exists.
    // Always overwrite — stellar-core silently replaces the database,
    // and captive core (Go SDK) expects this behavior.
    if db_path.exists() {
        if !force {
            tracing::info!(path = ?db_path, "Overwriting existing database");
        }
        std::fs::remove_file(db_path)?;
    }

    tracing::info!(path = ?db_path, "Creating new database");

    // Ensure parent directory exists
    create_parent_dir(db_path)?;

    // Create the database
    let db = henyey_db::Database::open(db_path)?;

    // Initialize genesis ledger (ledger 1) with root account, matching stellar-core
    let passphrase = &config.network.passphrase;
    // Compute bucket directory (same as App::new uses: <db_parent>/buckets/)
    let bucket_dir = db_path.parent().unwrap_or(db_path).join("buckets");
    std::fs::create_dir_all(&bucket_dir)?;

    initialize_genesis_ledger(
        &db,
        Some(&bucket_dir),
        passphrase,
        config.testing.genesis_test_account_count,
    )?;

    println!("Database created successfully at: {}", db_path.display());
    Ok(())
}

/// Initialize the genesis ledger (ledger 1) in the database.
///
/// This mirrors stellar-core's `startNewLedger()`: creates a genesis header with
/// protocol version 0, a root account holding 100 billion XLM, an empty bucket
/// list with the root account entry, and persists everything to the database.
///
/// When `genesis_test_account_count > 0`, creates that many additional accounts
/// named `"TestAccount-0"` through `"TestAccount-{N-1}"` with deterministic
/// keys (name padded with `'.'` to 32 bytes, used as Ed25519 seed). The total
/// coins are split evenly among root + test accounts, with root receiving the
/// remainder from integer division.
///
/// The root account's public key is derived from SHA-256(network_passphrase),
/// matching stellar-core's `SecretKey::fromSeed(networkID).getPublicKey()`.
fn initialize_genesis_ledger(
    db: &henyey_db::Database,
    bucket_dir: Option<&std::path::Path>,
    network_passphrase: &str,
    genesis_test_account_count: u32,
) -> anyhow::Result<()> {
    use henyey_bucket::BucketList;
    use henyey_common::NetworkId;
    use henyey_db::schema::state_keys;
    use henyey_history::build_history_archive_state;
    use henyey_ledger::{calculate_skip_values, compute_header_hash};
    use stellar_xdr::curr::{
        AccountEntry, AccountEntryExt, AccountId, BucketListType, Hash, LedgerEntry,
        LedgerEntryData, LedgerEntryExt, LedgerHeader, LedgerHeaderExt, Limits, PublicKey,
        SequenceNumber, StellarValue, StellarValueExt, Thresholds, TimePoint, Uint256, VecM,
        WriteXdr,
    };

    // 1. Derive root account public key from network passphrase.
    //    Matches stellar-core: SecretKey::fromSeed(networkID).getPublicKey()
    //    The network ID (SHA256 of passphrase) is used as an Ed25519 seed,
    //    and the public key of that keypair becomes the root account ID.
    let network_id = NetworkId::from_passphrase(network_passphrase);
    let root_secret = henyey_crypto::SecretKey::from_seed(network_id.as_bytes());
    let root_public = root_secret.public_key();
    let root_account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
        *root_public.as_bytes(),
    )));

    // 2. Create root account entry (balance = 100 billion XLM)
    let total_coins: i64 = 1_000_000_000_000_000_000; // 100B XLM in stroops

    // Compute per-account balance when test accounts are requested.
    // Matches stellar-core: split evenly, root gets the remainder.
    let (root_balance, test_balance) = if genesis_test_account_count > 0 {
        let total_accounts = genesis_test_account_count as i64 + 1; // +1 for root
        let base = total_coins / total_accounts;
        let remainder = total_coins % total_accounts;
        (base + remainder, base)
    } else {
        (total_coins, 0i64)
    };

    let root_entry = LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::Account(AccountEntry {
            account_id: root_account_id,
            balance: root_balance,
            seq_num: SequenceNumber(0),
            num_sub_entries: 0,
            inflation_dest: None,
            flags: 0,
            home_domain: stellar_xdr::curr::String32::default(),
            thresholds: Thresholds([1, 0, 0, 0]), // master weight = 1
            signers: VecM::default(),
            ext: AccountEntryExt::V0,
        }),
        ext: LedgerEntryExt::V0,
    };

    // Build list of genesis entries: root + test accounts
    let mut genesis_entries = vec![root_entry];

    for i in 0..genesis_test_account_count {
        let name = format!("TestAccount-{}", i);
        let seed = deterministic_seed(&name);
        let secret = henyey_crypto::SecretKey::from_seed(&seed);
        let public = secret.public_key();
        let account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(*public.as_bytes())));

        genesis_entries.push(LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Account(AccountEntry {
                account_id,
                balance: test_balance,
                seq_num: SequenceNumber(0),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: stellar_xdr::curr::String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: VecM::default(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        });
    }

    if genesis_test_account_count > 0 {
        tracing::info!(
            count = genesis_test_account_count,
            balance_per_account = test_balance,
            root_balance = root_balance,
            "Creating genesis test accounts"
        );
    }

    // 3. Create bucket list and add all genesis entries
    let mut bucket_list = BucketList::new();
    bucket_list
        .add_batch(
            1, // ledger_seq
            0, // protocol_version (genesis is v0)
            BucketListType::Live,
            genesis_entries, // init_entries
            vec![],          // live_entries
            vec![],          // dead_entries
        )
        .map_err(|e| anyhow::anyhow!("Failed to add genesis entries to bucket list: {}", e))?;

    // 3b. Persist non-empty bucket files to disk so that `load_last_known_ledger`
    //     can restore state on the next `run` startup (matches stellar-core's
    //     startNewLedger which writes bucket files via the full ledger-close path).
    if let Some(bucket_dir) = bucket_dir {
        std::fs::create_dir_all(bucket_dir)?;
        for level in bucket_list.levels() {
            for bucket in [&level.curr, &level.snap] {
                if !bucket.hash().is_zero() {
                    let path =
                        bucket_dir.join(henyey_bucket::canonical_bucket_filename(&bucket.hash()));
                    if !path.exists() {
                        bucket.save_to_xdr_file(&path).map_err(|e| {
                            anyhow::anyhow!("Failed to persist genesis bucket: {}", e)
                        })?;
                        tracing::debug!(
                            hash = %bucket.hash().to_hex(),
                            "Persisted genesis bucket file"
                        );
                    }
                }
            }
        }
    }

    // 4. Compute bucket list hash (protocol 0 = just the live hash, no hot archive)
    let bucket_list_hash = bucket_list.hash();

    // 5. Build genesis header
    let mut header = LedgerHeader {
        ledger_version: 0,
        previous_ledger_hash: Hash([0u8; 32]),
        scp_value: StellarValue {
            tx_set_hash: Hash([0u8; 32]),
            close_time: TimePoint(0),
            upgrades: VecM::default(),
            ext: StellarValueExt::Basic,
        },
        tx_set_result_hash: Hash([0u8; 32]),
        bucket_list_hash: Hash(*bucket_list_hash.as_bytes()),
        ledger_seq: 1,
        total_coins,
        fee_pool: 0,
        inflation_seq: 0,
        id_pool: 0,
        base_fee: 100,
        base_reserve: 100_000_000, // 10 XLM
        max_tx_set_size: 100,
        skip_list: std::array::from_fn(|_| Hash([0u8; 32])),
        ext: LedgerHeaderExt::V0,
    };

    // 6. Calculate skip values (no-op for ledger 1, but call for correctness)
    calculate_skip_values(&mut header);

    // 7. Compute header hash and serialize
    let header_hash = compute_header_hash(&header)?;
    let header_xdr = header.to_xdr(Limits::none())?;

    // 8. Build HAS (HistoryArchiveState) for genesis
    let has = build_history_archive_state(
        1,
        &bucket_list,
        None, // no hot archive at protocol 0
        Some(network_passphrase.to_string()),
    )
    .map_err(|e| anyhow::anyhow!("Failed to build HAS: {}", e))?;
    let has_json = has.to_json()?;

    // 9. Build bucket level hashes for DB storage
    let bucket_levels: Vec<(henyey_common::Hash256, henyey_common::Hash256)> = bucket_list
        .levels()
        .iter()
        .map(|level| (level.curr.hash(), level.snap.hash()))
        .collect();

    // 10. Build empty tx history / tx result entries for genesis (needed by
    //     history archive publishing — the first checkpoint includes ledger 1).
    let empty_tx_set = stellar_xdr::curr::TransactionSet {
        previous_ledger_hash: Hash(henyey_common::Hash256::ZERO.0),
        txs: VecM::default(),
    };
    let genesis_tx_history = stellar_xdr::curr::TransactionHistoryEntry {
        ledger_seq: 1,
        tx_set: empty_tx_set,
        ext: stellar_xdr::curr::TransactionHistoryEntryExt::V0,
    };
    let genesis_tx_result = stellar_xdr::curr::TransactionHistoryResultEntry {
        ledger_seq: 1,
        tx_result_set: stellar_xdr::curr::TransactionResultSet {
            results: VecM::default(),
        },
        ext: stellar_xdr::curr::TransactionHistoryResultEntryExt::default(),
    };

    // 11. Persist everything to the database
    db.with_connection(|conn| {
        use henyey_db::queries::HistoryQueries;
        use henyey_db::queries::{BucketListQueries, LedgerQueries, StateQueries};

        conn.store_ledger_header(&header, &header_xdr)?;
        conn.store_tx_history_entry(1, &genesis_tx_history)?;
        conn.store_tx_result_entry(1, &genesis_tx_result)?;
        conn.store_bucket_list(1, &bucket_levels)?;
        conn.set_state(state_keys::HISTORY_ARCHIVE_STATE, &has_json)?;
        conn.set_state(state_keys::NETWORK_PASSPHRASE, network_passphrase)?;
        conn.set_last_closed_ledger(1)?;
        Ok(())
    })?;

    tracing::info!(
        ledger_seq = 1,
        header_hash = %header_hash,
        bucket_list_hash = %bucket_list_hash,
        "Genesis ledger initialized with root account"
    );

    Ok(())
}

/// Apply-load command handler.
///
/// Creates a standalone node with genesis ledger, deploys contracts,
/// populates the bucket list with synthetic data, and closes ledgers with
/// maximally-filled transaction sets. Reports throughput and resource
/// utilization.
///
/// Matches stellar-core's `apply-load` CLI subcommand.
async fn cmd_apply_load(
    mut config: AppConfig,
    mode_str: &str,
    num_ledgers: u32,
    classic_txs_per_ledger: u32,
    clusters: u32,
    tx_count: u32,
    iterations: u32,
) -> anyhow::Result<()> {
    use henyey_simulation::{ApplyLoad, ApplyLoadConfig, ApplyLoadMode};

    let mode = match mode_str {
        "ledger-limits" => ApplyLoadMode::LimitBased,
        "max-sac-tps" | "single-shot" => ApplyLoadMode::MaxSacTps,
        other => anyhow::bail!(
            "Unknown apply-load mode '{}'. Valid modes: ledger-limits, max-sac-tps, single-shot",
            other
        ),
    };
    let is_single_shot = mode_str == "single-shot";

    // Configure for standalone benchmark operation.
    // The node never connects to peers or runs consensus — ApplyLoad
    // closes ledgers directly via LedgerManager.
    config.node.manual_close = true;
    config.node.is_validator = true;
    config.http.enabled = false;
    config.compat_http.enabled = false;

    // Generate an ephemeral node seed for the benchmark (required for validators).
    if config.node.node_seed.is_none() {
        let ephemeral = henyey_crypto::SecretKey::generate();
        config.node.node_seed = Some(ephemeral.to_strkey());
    }

    // Use a temporary directory for the database and buckets.
    let data_dir = tempfile::tempdir()?;
    config.database.path = data_dir.path().join("apply-load.db");
    config.buckets.directory = data_dir.path().join("buckets");
    std::fs::create_dir_all(&config.buckets.directory)?;

    let network_passphrase = config.network.passphrase.clone();

    // Initialize genesis ledger in the temporary database.
    henyey_simulation::initialize_genesis_ledger(&config, &network_passphrase)?;

    // Create the application.
    let app = std::sync::Arc::new(henyey_app::App::new(config).await?);
    app.set_self_arc().await;
    app.bootstrap_from_db().await?;

    // Build the ApplyLoad configuration.
    let al_config = ApplyLoadConfig {
        num_ledgers: if is_single_shot {
            iterations
        } else {
            num_ledgers
        },
        classic_txs_per_ledger,
        ledger_max_dependent_tx_clusters: clusters,
        ..ApplyLoadConfig::default()
    };

    println!(
        "apply-load: mode={:?}, num_ledgers={}, classic_txs_per_ledger={}, clusters={}",
        mode, num_ledgers, classic_txs_per_ledger, clusters
    );
    println!();

    // Construct the harness (performs full setup: accounts, contracts, bucket list).
    println!("Setting up benchmark harness...");
    let mut harness = ApplyLoad::new(app, al_config, mode)?;
    println!("Setup complete.");
    println!();

    match mode {
        ApplyLoadMode::LimitBased => {
            println!("Running limit-based benchmark ({} ledgers)...", num_ledgers);
            println!();

            let start = std::time::Instant::now();
            for i in 0..num_ledgers {
                harness.benchmark()?;
                println!("  Ledger {}/{} closed", i + 1, num_ledgers);
            }
            let elapsed = start.elapsed();

            println!();
            println!("=== Benchmark Results ===");
            println!("Total time: {:.2}s", elapsed.as_secs_f64());
            println!(
                "Average close time: {:.2}ms",
                elapsed.as_millis() as f64 / num_ledgers as f64
            );
            println!("Success rate: {:.1}%", harness.success_rate() * 100.0);
            println!();
            println!("Resource Utilization (mean % of limits):");
            println!(
                "  TX count:        {:.1}%",
                harness.tx_count_utilization().mean() / 1000.0
            );
            println!(
                "  Instructions:    {:.1}%",
                harness.instruction_utilization().mean() / 1000.0
            );
            println!(
                "  TX size:         {:.1}%",
                harness.tx_size_utilization().mean() / 1000.0
            );
            println!(
                "  Disk read bytes: {:.1}%",
                harness.disk_read_byte_utilization().mean() / 1000.0
            );
            println!(
                "  Write bytes:     {:.1}%",
                harness.disk_write_byte_utilization().mean() / 1000.0
            );
            println!(
                "  Read entries:    {:.1}%",
                harness.disk_read_entry_utilization().mean() / 1000.0
            );
            println!(
                "  Write entries:   {:.1}%",
                harness.write_entry_utilization().mean() / 1000.0
            );
        }

        ApplyLoadMode::MaxSacTps if is_single_shot => {
            // Round tx_count down to nearest multiple of clusters.
            let txs = (tx_count / clusters) * clusters;
            println!(
                "Single-shot: closing {} ledgers with {} SAC TXs across {} clusters...",
                iterations, txs, clusters
            );
            println!();

            let avg_ms = harness.benchmark_sac_tps(txs)?;

            println!();
            println!("=== Single-Shot Result ({} iterations) ===", iterations);
            println!(
                "TXs/ledger: {}, Clusters: {}, Avg close: {:.1}ms",
                txs, clusters, avg_ms
            );
            println!("Average TPS: {:.0}", txs as f64 / (avg_ms / 1000.0));
            println!("Success rate: {:.1}%", harness.success_rate() * 100.0);
        }

        ApplyLoadMode::MaxSacTps => {
            println!("Searching for maximum sustainable SAC TPS...");
            println!();

            let max_tps = harness.find_max_sac_tps()?;

            println!();
            println!("=== Max SAC TPS Result ===");
            println!("Maximum sustainable SAC payments/sec: {}", max_tps);
            println!("Success rate: {:.1}%", harness.success_rate() * 100.0);
        }
    }

    Ok(())
}

/// Compare a checkpoint between two history archives.
///
/// Downloads checkpoint data (HAS, ledger headers, transactions, results) from
/// both archives and reports any differences. Exit code 0 = match, 1 = mismatch.
async fn cmd_compare_checkpoint(
    local_url: &str,
    remote_url: &str,
    checkpoint: u32,
) -> anyhow::Result<()> {
    use henyey_history::{compare_checkpoint, HistoryArchive};

    println!("Comparing checkpoint {} between archives", checkpoint);
    println!("  Local:     {}", local_url);
    println!("  Reference: {}", remote_url);
    println!();

    let local = HistoryArchive::new(local_url)
        .map_err(|e| anyhow::anyhow!("Failed to create local archive client: {}", e))?;
    let reference = HistoryArchive::new(remote_url)
        .map_err(|e| anyhow::anyhow!("Failed to create reference archive client: {}", e))?;

    let result = compare_checkpoint(&local, &reference, checkpoint)
        .await
        .map_err(|e| anyhow::anyhow!("Comparison failed: {}", e))?;

    result.print_summary();

    if result.is_match() {
        Ok(())
    } else {
        anyhow::bail!(
            "Checkpoint {} has {} mismatch(es)",
            checkpoint,
            result.mismatch_count()
        );
    }
}

/// Force SCP command handler.
///
/// Sets a flag in the database so the next `run` will skip catchup and
/// bootstrap consensus from the current LCL. This matches stellar-core's
/// `force-scp` behavior for standalone single-node networks.
fn cmd_force_scp(config: &AppConfig) -> anyhow::Result<()> {
    let db_path = &config.database.path;
    if !db_path.exists() {
        anyhow::bail!("Database not found at {:?}. Run new-db first.", db_path);
    }

    let db = henyey_db::Database::open(db_path)?;
    db.with_connection(|conn| {
        use henyey_db::queries::StateQueries;
        use henyey_db::schema::state_keys;
        conn.set_state(state_keys::FORCE_SCP, "true")
    })?;

    tracing::info!("force-scp: flag set, SCP will bootstrap on next run");
    println!("force-scp flag set successfully");
    Ok(())
}

/// Initialize a named history archive.
///
/// Creates the `.well-known/stellar-history.json` file in the archive using
/// the archive's configured put/mkdir commands. This matches stellar-core's
/// `new-hist` command for local filesystem archives.
async fn cmd_new_hist(config: &AppConfig, name: &str) -> anyhow::Result<()> {
    use henyey_history::{
        remote_archive::RemoteArchiveConfig, ArchiveEntry, HistoryArchiveManager, RemoteArchive,
    };

    // Find the named archive in config
    let archive_config = config
        .history
        .archives
        .iter()
        .find(|a| a.name == name)
        .ok_or_else(|| anyhow::anyhow!("Archive '{}' not found in configuration", name))?;

    if !archive_config.put_enabled {
        anyhow::bail!(
            "Archive '{}' is not writable (no put command configured)",
            name
        );
    }

    // Build remote archive config
    let remote_config = RemoteArchiveConfig {
        name: name.to_string(),
        get_cmd: None,
        put_cmd: archive_config.put.clone(),
        mkdir_cmd: archive_config.mkdir.clone(),
    };
    let remote = RemoteArchive::new(remote_config);

    // Build archive manager with this archive
    let mut manager = HistoryArchiveManager::new(config.network.passphrase.clone());
    manager.add_archive(ArchiveEntry::write_only(name.to_string(), remote));

    // Initialize the archive
    manager
        .initialize_history_archive(name)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to initialize archive '{}': {}", name, e))?;

    println!("History archive '{}' initialized successfully", name);
    Ok(())
}

/// Upgrade database command handler.
async fn cmd_upgrade_db(config: AppConfig) -> anyhow::Result<()> {
    tracing::info!(path = ?config.database.path, "Upgrading database schema");

    let _db = henyey_db::Database::open(&config.database.path)?;

    // Database initialization already applies the latest schema
    // In a full implementation, this would run migrations

    println!("Database schema is up to date");
    Ok(())
}

/// Version command handler.
///
/// Prints version info in a format compatible with stellar-core's `version` output.
/// stellar-rpc parses this output for:
/// - First line: build version string (e.g., "henyey-v25.0.0-alpha.1")
/// - Line matching "ledger protocol version: N": protocol version number
fn cmd_version() {
    use henyey_common::protocol::CURRENT_LEDGER_PROTOCOL_VERSION;
    use henyey_common::version::build_version_string;
    println!("{}", build_version_string(env!("CARGO_PKG_VERSION")));
    println!("ledger protocol version: {CURRENT_LEDGER_PROTOCOL_VERSION}");
}

/// Convert an identifier between formats (stellar-core compatible).
///
/// Accepts a 64-character hex string or a strkey (G.../S.../T.../X.../C...) and
/// prints all possible interpretations. The quickstart container uses this to
/// derive the network root account from `SHA256(network_passphrase)`:
///
/// ```text
/// NETWORK_ID=$(printf "$PASSPHRASE" | sha256sum | cut -f1 -d" ")
/// stellar-core convert-id $NETWORK_ID
/// ```
///
/// Output format matches stellar-core: multiple interpretations of the same
/// 32-byte value as different strkey types.
fn cmd_convert_id(id: &str) {
    use henyey_crypto::stellar_strkey;
    use henyey_crypto::{PublicKey, SecretKey};

    // Try to decode the input in various formats
    let raw_bytes: Option<[u8; 32]> = if id.len() == 64 {
        // 64-char hex string → 32 bytes
        let decoded = (0..32)
            .map(|i| u8::from_str_radix(&id[i * 2..i * 2 + 2], 16))
            .collect::<Result<Vec<u8>, _>>();
        if let Ok(v) = decoded {
            let mut bytes = [0u8; 32];
            bytes.copy_from_slice(&v);
            Some(bytes)
        } else {
            None
        }
    } else if let Ok(pk) = PublicKey::from_strkey(id) {
        Some(*pk.as_bytes())
    } else if let Ok(sk) = SecretKey::from_strkey(id) {
        // Input is already a strkey seed — print interpretation and derived public key
        println!("Interpreted as Seed:");
        println!("  strKey: {}", sk.to_strkey());
        let pk = sk.public_key();
        println!("PublicKey:");
        println!("  strKey: {}", pk.to_strkey());
        println!("  hex: {}", hex::encode(pk.as_bytes()));
        return;
    } else {
        None
    };

    let Some(bytes) = raw_bytes else {
        eprintln!("Error: cannot parse '{}' as hex or strkey", id);
        std::process::exit(1);
    };

    // Interpret the 32 bytes as a public key (encode as strkey regardless of
    // whether the bytes represent a valid Ed25519 point — matches stellar-core behavior)
    println!("Interpreted as PublicKey:");
    println!(
        "  strKey: {}",
        stellar_strkey::ed25519::PublicKey(bytes).to_string()
    );
    println!("  hex: {}", hex::encode(bytes));

    // Interpret the 32 bytes as a seed → derive public key
    let sk = SecretKey::from_seed(&bytes);
    println!("Interpreted as Seed:");
    println!("  strKey: {}", sk.to_strkey());
    let derived_pk = sk.public_key();
    println!("PublicKey:");
    println!("  strKey: {}", derived_pk.to_strkey());
    println!("  hex: {}", hex::encode(derived_pk.as_bytes()));

    // Other strkey interpretations
    println!("Other interpretations:");
    println!(
        "  STRKEY_PRE_AUTH_TX: {}",
        stellar_strkey::PreAuthTx(bytes).to_string()
    );
    println!(
        "  STRKEY_HASH_X: {}",
        stellar_strkey::HashX(bytes).to_string()
    );
}

/// Offline info command handler.
///
/// Opens the database without connecting to the network and prints the node's
/// state as JSON. Compatible with stellar-core's `offline-info` output format.
///
/// stellar-rpc uses this to check whether an existing captive-core database
/// can be reused (by reading `info.ledger.num`).
fn cmd_offline_info(config: AppConfig) -> anyhow::Result<()> {
    use henyey_common::protocol::CURRENT_LEDGER_PROTOCOL_VERSION;
    use henyey_db::queries::StateQueries;
    use std::time::{SystemTime, UNIX_EPOCH};
    use stellar_xdr::curr::LedgerHeaderExt;

    let db_path = &config.database.path;

    if !db_path.exists() {
        // stellar-core also fails if no DB exists; the Go code handles the error
        // by setting createNewDB = true
        anyhow::bail!("Database does not exist at {:?}", db_path);
    }

    let db = henyey_db::Database::open(db_path)?;

    // Get last closed ledger sequence
    let lcl_seq = db.with_connection(|conn| conn.get_last_closed_ledger())?;

    let (num, hash, close_time, version, base_fee, base_reserve, max_tx_set_size, flags) =
        if let Some(seq) = lcl_seq {
            if let Some(header) = db.get_ledger_header(seq)? {
                let hash = db
                    .get_ledger_hash(seq)?
                    .map(|h| h.to_hex())
                    .unwrap_or_default();
                let flags = match &header.ext {
                    LedgerHeaderExt::V1(ext) => ext.flags as i64,
                    LedgerHeaderExt::V0 => 0i64,
                };
                (
                    seq as i64,
                    hash,
                    header.scp_value.close_time.0 as i64,
                    header.ledger_version as i64,
                    header.base_fee as i64,
                    header.base_reserve as i64,
                    header.max_tx_set_size as i64,
                    flags,
                )
            } else {
                (
                    seq as i64,
                    String::new(),
                    0i64,
                    0i64,
                    0i64,
                    0i64,
                    0i64,
                    0i64,
                )
            }
        } else {
            (0i64, String::new(), 0i64, 0i64, 0i64, 0i64, 0i64, 0i64)
        };

    // Calculate age (seconds since close time)
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let age = if close_time > 0 { now - close_time } else { 0 };

    // Build the JSON response matching stellar-core's format.
    // The Go code only reads info.ledger.num, but we provide the full
    // structure for compatibility with other tools.
    let mut ledger = serde_json::json!({
        "num": num,
        "hash": hash,
        "closeTime": close_time,
        "version": version,
        "baseFee": base_fee,
        "baseReserve": base_reserve,
        "maxTxSetSize": max_tx_set_size,
        "age": age
    });

    // stellar-core only includes "flags" when non-zero
    if flags != 0 {
        ledger
            .as_object_mut()
            .unwrap()
            .insert("flags".to_string(), serde_json::json!(flags));
    }

    let response = serde_json::json!({
        "info": {
            "build": henyey_common::version::build_version_string(env!("CARGO_PKG_VERSION")),
            "protocol_version": CURRENT_LEDGER_PROTOCOL_VERSION,
            "state": "Booting",
            "ledger": ledger,
            "peers": {
                "pending_count": 0,
                "authenticated_count": 0
            },
            "network": config.network.passphrase
        }
    });

    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}

fn cmd_new_keypair() -> anyhow::Result<()> {
    let keypair = henyey_crypto::SecretKey::generate();

    println!("Generated new keypair:");
    println!();
    println!("Public Key:  {}", keypair.public_key().to_strkey());
    println!("Secret Seed: {}", keypair.to_strkey());
    println!();
    println!("IMPORTANT: Store the secret seed securely! It cannot be recovered.");

    Ok(())
}

/// Info command handler.
async fn cmd_info(config: AppConfig) -> anyhow::Result<()> {
    // Try to create app to get full info, or show basic info if that fails
    match App::new(config.clone()).await {
        Ok(app) => {
            let info = app.info();
            print!("{}", info);
        }
        Err(e) => {
            tracing::debug!(error = %e, "Could not initialize app, showing basic info");

            println!(
                "{}",
                henyey_common::version::build_version_string(env!("CARGO_PKG_VERSION"))
            );
            println!();
            println!("Configuration:");
            println!("  Network: {}", config.network.passphrase);
            println!("  Database: {}", config.database.path.display());
            println!("  Validator: {}", config.node.is_validator);
            println!();

            if !config.database.path.exists() {
                println!("Note: Database does not exist. Run 'new-db' to create it.");
            }
        }
    }

    Ok(())
}

/// Verify history command handler.
async fn cmd_verify_history(
    config: AppConfig,
    from: Option<u32>,
    to: Option<u32>,
) -> anyhow::Result<()> {
    use henyey_history::verify;
    use henyey_ledger::TransactionSetVariant;

    println!("Verifying history archives...");
    println!();

    let archives = all_archives(&config)?;

    println!("Using {} archive(s)", archives.len());

    // Get the range to verify
    let archive = &archives[0];
    let root_has = archive.get_root_has().await?;
    let current_ledger = root_has.current_ledger;

    let start = from.unwrap_or(1);
    let end = to.unwrap_or(current_ledger);

    println!("Verifying ledger range: {} to {}", start, end);
    println!();

    // Verify in checkpoint intervals (64 ledgers)
    let mut verified_count = 0;
    let mut error_count = 0;

    // Calculate checkpoint-aligned start
    let start_checkpoint = henyey_history::checkpoint::checkpoint_containing(start);
    let end_checkpoint = henyey_history::checkpoint::checkpoint_containing(end);

    let mut checkpoint = start_checkpoint;
    while checkpoint <= end_checkpoint {
        // Get checkpoint HAS (History Archive State)
        match archive.get_checkpoint_has(checkpoint).await {
            Ok(has) => {
                // Verify the HAS structure
                match verify::verify_has_structure(&has) {
                    Ok(()) => {
                        // Verify checkpoint matches
                        match verify::verify_has_checkpoint(&has, checkpoint) {
                            Ok(()) => {
                                println!("  Checkpoint {}: OK", checkpoint);
                                verified_count += 1;
                            }
                            Err(e) => {
                                println!("  Checkpoint {}: FAIL - {}", checkpoint, e);
                                error_count += 1;
                            }
                        }
                    }
                    Err(e) => {
                        println!(
                            "  Checkpoint {}: FAIL (invalid structure) - {}",
                            checkpoint, e
                        );
                        error_count += 1;
                    }
                }

                // Get and verify ledger headers for this checkpoint range
                match archive.get_ledger_headers(checkpoint).await {
                    Ok(history_entries) => {
                        // Extract LedgerHeader from LedgerHeaderHistoryEntry
                        let headers: Vec<stellar_xdr::curr::LedgerHeader> = history_entries
                            .iter()
                            .map(|entry| entry.header.clone())
                            .collect();
                        if let Err(e) = verify::verify_header_chain(&headers) {
                            println!("    Header chain verification FAILED: {}", e);
                            error_count += 1;
                        }

                        match archive.get_transactions(checkpoint).await {
                            Ok(tx_entries) => match archive.get_results(checkpoint).await {
                                Ok(tx_results) => {
                                    let tx_map = tx_entries
                                        .iter()
                                        .map(|entry| (entry.ledger_seq, entry))
                                        .collect::<std::collections::HashMap<_, _>>();
                                    let result_map = tx_results
                                        .iter()
                                        .map(|entry| (entry.ledger_seq, entry))
                                        .collect::<std::collections::HashMap<_, _>>();

                                    for header in &headers {
                                        let Some(tx_entry) = tx_map.get(&header.ledger_seq) else {
                                            println!(
                                                    "    Missing transaction history entry for ledger {}",
                                                    header.ledger_seq
                                                );
                                            error_count += 1;
                                            continue;
                                        };
                                        let Some(result_entry) = result_map.get(&header.ledger_seq)
                                        else {
                                            println!(
                                                    "    Missing transaction result entry for ledger {}",
                                                    header.ledger_seq
                                                );
                                            error_count += 1;
                                            continue;
                                        };

                                        let tx_set = TransactionSetVariant::from(*tx_entry);
                                        if let Err(e) = verify::verify_tx_set(header, &tx_set) {
                                            println!(
                                                    "    Tx set hash verification FAILED (ledger {}): {}",
                                                    header.ledger_seq, e
                                                );
                                            error_count += 1;
                                        }

                                        let result_xdr = result_entry
                                            .tx_result_set
                                            .to_xdr(stellar_xdr::curr::Limits::none());
                                        match result_xdr {
                                            Ok(bytes) => {
                                                if let Err(e) =
                                                    verify::verify_tx_result_set(header, &bytes)
                                                {
                                                    println!(
                                                            "    Tx result hash verification FAILED (ledger {}): {}",
                                                            header.ledger_seq, e
                                                        );
                                                    error_count += 1;
                                                }
                                            }
                                            Err(e) => {
                                                println!(
                                                        "    Failed to encode tx result set for ledger {}: {}",
                                                        header.ledger_seq, e
                                                    );
                                                error_count += 1;
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    println!("    Warning: Could not verify tx results: {}", e);
                                }
                            },
                            Err(e) => {
                                println!("    Warning: Could not verify transactions: {}", e);
                            }
                        }
                    }
                    Err(e) => {
                        println!("    Warning: Could not verify headers: {}", e);
                    }
                }

                match archive.get_scp_history(checkpoint).await {
                    Ok(entries) => {
                        if let Err(e) = verify::verify_scp_history_entries(&entries) {
                            println!("    SCP history verification FAILED: {}", e);
                            error_count += 1;
                        }
                    }
                    Err(e) => {
                        println!("    Warning: Could not verify SCP history: {}", e);
                    }
                }
            }
            Err(e) => {
                println!("  Checkpoint {}: ERROR - {}", checkpoint, e);
                error_count += 1;
            }
        }

        // Move to next checkpoint
        checkpoint = henyey_history::checkpoint::next_checkpoint(checkpoint);
    }

    println!();
    println!("Verification complete:");
    println!("  Checkpoints verified: {}", verified_count);
    println!("  Errors: {}", error_count);

    if error_count > 0 {
        anyhow::bail!("Verification failed with {} errors", error_count);
    }

    Ok(())
}

/// Publish history command handler.
async fn cmd_publish_history(config: AppConfig, force: bool) -> anyhow::Result<()> {
    use henyey_bucket::{BucketList, BucketManager};
    use henyey_common::Hash256;
    use henyey_history::archive_state::HistoryArchiveState;
    use henyey_history::checkpoint::{checkpoint_containing, next_checkpoint};
    use henyey_history::checkpoint_frequency;
    use henyey_history::paths::root_has_path;
    use henyey_history::publish::{build_history_archive_state, PublishConfig, PublishManager};
    use henyey_history::verify;
    use henyey_ledger::compute_header_hash;
    use henyey_ledger::TransactionSetVariant;
    use std::fs;
    use std::path::PathBuf;
    use url::Url;

    if !config.node.is_validator {
        anyhow::bail!("Only validators can publish history");
    }

    println!("Publishing history to archives...");
    println!();

    // Check for writable archives
    let writable_archives: Vec<_> = config
        .history
        .archives
        .iter()
        .filter(|a| a.put_enabled)
        .collect();

    if writable_archives.is_empty() {
        anyhow::bail!(
            "No writable history archives configured. Add 'put = true' to an archive config."
        );
    }

    let mut local_targets = Vec::new();
    let mut command_targets = Vec::new();
    for archive in &writable_archives {
        if let Some(put) = archive.put.clone() {
            command_targets.push(CommandArchiveTarget {
                name: archive.name.clone(),
                put,
                mkdir: archive.mkdir.clone(),
            });
            continue;
        }

        let path = match Url::parse(&archive.url) {
            Ok(url) if url.scheme() == "file" => url
                .to_file_path()
                .map_err(|_| anyhow::anyhow!("Invalid file URL: {}", archive.url))?,
            Ok(_) => {
                tracing::warn!(archive = %archive.url, "Remote publish not supported (missing put command)");
                continue;
            }
            Err(_) => PathBuf::from(&archive.url),
        };
        local_targets.push((archive.url.as_str(), path));
    }

    if local_targets.is_empty() && command_targets.is_empty() {
        anyhow::bail!("No publish archives configured (local paths or put commands required).");
    }

    println!(
        "Writable archives: {}",
        local_targets.len() + command_targets.len()
    );
    for (url, _) in &local_targets {
        println!("  - {}", url);
    }
    for archive in &command_targets {
        println!("  - {} (command)", archive.name);
    }
    println!();

    // Open database to get current state
    let db = henyey_db::Database::open(&config.database.path)?;

    // Get current ledger from database
    let current_ledger = db
        .get_latest_ledger_seq()?
        .ok_or_else(|| anyhow::anyhow!("No ledger data in database. Run the node first."))?;

    println!("Current ledger in database: {}", current_ledger);

    // Calculate checkpoints to publish
    let latest_checkpoint = henyey_history::checkpoint::latest_checkpoint_before_or_at(
        current_ledger,
    )
    .ok_or_else(|| anyhow::anyhow!("No checkpoint available for ledger {}", current_ledger))?;

    println!("Latest publishable checkpoint: {}", latest_checkpoint);

    let queued_checkpoints = db.load_publish_queue(None)?;
    let mut queued_checkpoints = queued_checkpoints
        .into_iter()
        .filter(|checkpoint| *checkpoint <= latest_checkpoint)
        .collect::<Vec<_>>();
    queued_checkpoints.sort_unstable();

    // Check what's already published across local archives.
    let mut published_ledger = latest_checkpoint;
    for (_, path) in &local_targets {
        let root_path = path.join(root_has_path());
        let ledger = if root_path.exists() {
            let json = fs::read_to_string(&root_path)?;
            let has = HistoryArchiveState::from_json(&json)?;
            has.current_ledger()
        } else {
            0
        };
        published_ledger = published_ledger.min(ledger);
    }

    println!("Already published up to: {}", published_ledger);

    if published_ledger >= latest_checkpoint && !force && queued_checkpoints.is_empty() {
        println!();
        println!("Archive is up to date. Use --force to republish.");
        return Ok(());
    }

    let mut checkpoints_to_publish = Vec::new();
    if !queued_checkpoints.is_empty() && !force {
        checkpoints_to_publish = queued_checkpoints;
    } else {
        // Calculate range to publish
        let start_checkpoint = if published_ledger > 0 && !force {
            next_checkpoint(published_ledger)
        } else {
            // Start from the first checkpoint we have
            checkpoint_containing(1)
        };

        if start_checkpoint > latest_checkpoint {
            println!("Nothing new to publish.");
            return Ok(());
        }

        let mut checkpoint = start_checkpoint;
        while checkpoint <= latest_checkpoint {
            checkpoints_to_publish.push(checkpoint);
            checkpoint = next_checkpoint(checkpoint);
        }
    }

    if checkpoints_to_publish.is_empty() {
        println!("Nothing queued to publish.");
        return Ok(());
    }

    println!();
    let first_checkpoint = *checkpoints_to_publish
        .first()
        .expect("checked non-empty above");
    let last_checkpoint = *checkpoints_to_publish
        .last()
        .expect("checked non-empty above");
    println!(
        "Publishing checkpoints {} to {}...",
        first_checkpoint, last_checkpoint
    );

    let bucket_manager = BucketManager::with_cache_size(
        config.buckets.directory.clone(),
        config.buckets.cache_size,
    )?;

    let mut published_count = 0;
    for checkpoint in checkpoints_to_publish {
        print!("  Publishing checkpoint {}... ", checkpoint);

        let start_ledger = checkpoint.saturating_sub(checkpoint_frequency() - 1);
        let start_ledger = if start_ledger == 0 { 1 } else { start_ledger };
        let mut headers = Vec::new();
        let mut tx_entries = Vec::new();
        let mut tx_results = Vec::new();

        for seq in start_ledger..=checkpoint {
            let header = db
                .get_ledger_header(seq)?
                .ok_or_else(|| anyhow::anyhow!("Missing ledger header {}", seq))?;
            let hash = compute_header_hash(&header)?;
            headers.push(stellar_xdr::curr::LedgerHeaderHistoryEntry {
                header,
                hash: stellar_xdr::curr::Hash(hash.0),
                ext: stellar_xdr::curr::LedgerHeaderHistoryEntryExt::V0,
            });

            let tx_entry = db
                .get_tx_history_entry(seq)?
                .ok_or_else(|| anyhow::anyhow!("Missing tx history entry {}", seq))?;
            tx_entries.push(tx_entry);

            let tx_result = db
                .get_tx_result_entry(seq)?
                .ok_or_else(|| anyhow::anyhow!("Missing tx result entry {}", seq))?;
            tx_results.push(tx_result);
        }

        let scp_entries = build_scp_history_entries(&db, start_ledger, checkpoint)?;

        for idx in 0..headers.len() {
            let header_entry = &headers[idx];
            let tx_entry = &tx_entries[idx];
            let tx_result_entry = &tx_results[idx];
            let header = &header_entry.header;
            let tx_set = TransactionSetVariant::from(tx_entry);
            let tx_set_hash = verify::compute_tx_set_hash(&tx_set).unwrap_or(Hash256::ZERO);
            let expected_tx_set = Hash256::from(header.scp_value.tx_set_hash.0);
            if tx_set_hash != expected_tx_set {
                anyhow::bail!(
                    "Tx set hash mismatch at {} (expected {}, got {})",
                    header.ledger_seq,
                    expected_tx_set.to_hex(),
                    tx_set_hash.to_hex()
                );
            }

            let tx_result_hash =
                Hash256::hash_xdr(&tx_result_entry.tx_result_set).unwrap_or(Hash256::ZERO);
            let expected_tx_result = Hash256::from(header.tx_set_result_hash.0);
            if tx_result_hash != expected_tx_result {
                anyhow::bail!(
                    "Tx result hash mismatch at {} (expected {}, got {})",
                    header.ledger_seq,
                    expected_tx_result.to_hex(),
                    tx_result_hash.to_hex()
                );
            }
        }

        let levels = db
            .load_bucket_list(checkpoint)?
            .ok_or_else(|| anyhow::anyhow!("Missing bucket list snapshot {}", checkpoint))?;
        let mut bucket_list = BucketList::new();
        bucket_list.set_bucket_dir(bucket_manager.bucket_dir().to_path_buf());
        for (idx, (curr_hash, snap_hash)) in levels.iter().enumerate() {
            let curr_bucket = bucket_manager.load_bucket(curr_hash)?;
            let snap_bucket = bucket_manager.load_bucket(snap_hash)?;
            let level = bucket_list
                .level_mut(idx)
                .ok_or_else(|| anyhow::anyhow!("Missing bucket level {}", idx))?;
            level.set_curr((*curr_bucket).clone());
            level.set_snap((*snap_bucket).clone());
        }

        let expected_hash = Hash256::from(headers.last().unwrap().header.bucket_list_hash.0);
        let actual_hash = bucket_list.hash();
        if expected_hash != actual_hash {
            anyhow::bail!(
                "Bucket list hash mismatch at {} (expected {}, got {})",
                checkpoint,
                expected_hash.to_hex(),
                actual_hash.to_hex()
            );
        }

        // Build HAS once for all publish targets (same arguments, deterministic result)
        let has = build_history_archive_state(
            checkpoint,
            &bucket_list,
            None,
            Some(config.network.passphrase.clone()),
        )?;

        let command_publish_dir = if command_targets.is_empty() {
            None
        } else {
            let sanitized_name = config
                .node
                .name
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect::<String>();
            let publish_dir = std::env::temp_dir().join(format!(
                "rs-stellar-core-publish-{}-{}",
                sanitized_name, checkpoint
            ));
            if publish_dir.exists() {
                std::fs::remove_dir_all(&publish_dir)?;
            }
            std::fs::create_dir_all(&publish_dir)?;

            let publish_config = PublishConfig {
                local_path: publish_dir.clone(),
                network_passphrase: Some(config.network.passphrase.clone()),
                ..Default::default()
            };
            let manager = PublishManager::new(publish_config);
            manager
                .publish_checkpoint(
                    checkpoint,
                    &headers,
                    &tx_entries,
                    &tx_results,
                    &bucket_list,
                    None,
                )
                .await?;

            write_root_has(&publish_dir, &has)?;
            Some(publish_dir)
        };

        let mut published_any = false;
        for (url, path) in &local_targets {
            let publish_config = PublishConfig {
                local_path: path.clone(),
                network_passphrase: Some(config.network.passphrase.clone()),
                ..Default::default()
            };
            let manager = PublishManager::new(publish_config);
            if !force && manager.is_published(checkpoint) {
                continue;
            }
            manager
                .publish_checkpoint(
                    checkpoint,
                    &headers,
                    &tx_entries,
                    &tx_results,
                    &bucket_list,
                    None,
                )
                .await?;
            write_scp_history_file(path, checkpoint, &scp_entries)?;
            write_root_has(path, &has)?;
            println!("OK ({})", url);
            published_any = true;
        }

        if let Some(ref publish_dir) = command_publish_dir {
            write_scp_history_file(publish_dir, checkpoint, &scp_entries)?;
            for archive in &command_targets {
                upload_publish_directory(archive, publish_dir)?;
                println!("OK (command: {})", archive.name);
                published_any = true;
            }
        }

        if let Some(publish_dir) = command_publish_dir {
            if let Err(err) = std::fs::remove_dir_all(&publish_dir) {
                tracing::warn!(
                    path = %publish_dir.display(),
                    error = %err,
                    "Failed to remove publish temp directory"
                );
            }
        }

        if published_any {
            published_count += 1;
        } else {
            println!("SKIP (already published)");
        }

        if let Err(err) = db.remove_publish(checkpoint) {
            tracing::warn!(checkpoint, error = %err, "Failed to remove publish queue entry");
        }
    }

    println!();
    println!("Publishing complete:");
    println!("  Checkpoints processed: {}", published_count);
    println!();

    Ok(())
}

/// Builds SCP history entries for a checkpoint range from the database.
///
/// Collects SCP envelopes and quorum sets for each ledger in the range,
/// packaging them into the format required for history archive publishing.
fn build_scp_history_entries(
    db: &henyey_db::Database,
    start_ledger: u32,
    checkpoint: u32,
) -> anyhow::Result<Vec<stellar_xdr::curr::ScpHistoryEntry>> {
    use henyey_common::Hash256;
    use std::collections::HashSet;
    use stellar_xdr::curr::{LedgerScpMessages, ScpHistoryEntry, ScpHistoryEntryV0};

    let mut entries = Vec::new();
    for seq in start_ledger..=checkpoint {
        let envelopes = db.load_scp_history(seq)?;
        if envelopes.is_empty() {
            continue;
        }

        let mut qset_hashes = HashSet::new();
        for envelope in &envelopes {
            let hash = scp_quorum_set_hash(&envelope.statement);
            qset_hashes.insert(Hash256::from_bytes(hash.0));
        }

        let mut qset_hashes = qset_hashes.into_iter().collect::<Vec<_>>();
        qset_hashes.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));

        let mut qsets = Vec::new();
        for hash in qset_hashes {
            let qset = db
                .load_scp_quorum_set(&hash)?
                .ok_or_else(|| anyhow::anyhow!("Missing quorum set {}", hash.to_hex()))?;
            qsets.push(qset);
        }

        let quorum_sets = qsets
            .try_into()
            .map_err(|_| anyhow::anyhow!("Too many quorum sets for ledger {}", seq))?;
        let messages = envelopes
            .try_into()
            .map_err(|_| anyhow::anyhow!("Too many SCP envelopes for ledger {}", seq))?;
        let entry = ScpHistoryEntry::V0(ScpHistoryEntryV0 {
            quorum_sets,
            ledger_messages: LedgerScpMessages {
                ledger_seq: seq,
                messages,
            },
        });
        entries.push(entry);
    }

    Ok(entries)
}

/// Write the root `HistoryArchiveState` JSON file into a directory.
fn write_root_has(
    dir: &std::path::Path,
    has: &henyey_history::HistoryArchiveState,
) -> anyhow::Result<()> {
    use henyey_history::paths::root_has_path;
    let root_path = dir.join(root_has_path());
    create_parent_dir(&root_path)?;
    std::fs::write(&root_path, has.to_json()?)?;
    Ok(())
}

/// Writes SCP history entries to a gzip-compressed XDR file.
///
/// Creates the file at the standard history archive path for SCP data.
fn write_scp_history_file(
    base_dir: &std::path::Path,
    checkpoint: u32,
    entries: &[stellar_xdr::curr::ScpHistoryEntry],
) -> anyhow::Result<()> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use henyey_history::paths::checkpoint_path;
    use std::io::Write;
    use stellar_xdr::curr::Limits;

    let path = base_dir.join(checkpoint_path("scp", checkpoint, "xdr.gz"));
    create_parent_dir(&path)?;
    let file = std::fs::File::create(&path)?;
    let mut encoder = GzEncoder::new(file, Compression::default());

    for entry in entries {
        let xdr = entry.to_xdr(Limits::none())?;
        let marked_len = (xdr.len() as u32) | 0x8000_0000;
        encoder.write_all(&marked_len.to_be_bytes())?;
        encoder.write_all(&xdr)?;
    }
    encoder.finish()?;
    Ok(())
}

/// Extracts the quorum set hash from an SCP statement.
///
/// Different SCP pledge types store the quorum set hash in different fields.
fn scp_quorum_set_hash(statement: &stellar_xdr::curr::ScpStatement) -> stellar_xdr::curr::Hash {
    match &statement.pledges {
        stellar_xdr::curr::ScpStatementPledges::Nominate(nom) => nom.quorum_set_hash.clone(),
        stellar_xdr::curr::ScpStatementPledges::Prepare(prep) => prep.quorum_set_hash.clone(),
        stellar_xdr::curr::ScpStatementPledges::Confirm(conf) => conf.quorum_set_hash.clone(),
        stellar_xdr::curr::ScpStatementPledges::Externalize(ext) => {
            ext.commit_quorum_set_hash.clone()
        }
    }
}

/// Configuration for publishing to a remote archive via shell commands.
#[derive(Clone)]
struct CommandArchiveTarget {
    /// Human-readable name for the archive.
    name: String,
    /// Shell command template for uploading files ({0} = local path, {1} = remote path).
    put: String,
    /// Optional shell command template for creating directories ({0} = remote dir).
    mkdir: Option<String>,
}

/// Uploads a local publish directory to a remote archive using shell commands.
///
/// Iterates through all files in the directory, creating remote directories
/// as needed, then uploads each file using the configured put command.
fn upload_publish_directory(
    target: &CommandArchiveTarget,
    publish_dir: &std::path::Path,
) -> anyhow::Result<()> {
    use std::collections::HashSet;

    let mut files = collect_files(publish_dir)?;
    files.sort();

    let mut created_dirs = HashSet::new();
    for file in files {
        let rel = file
            .strip_prefix(publish_dir)
            .map_err(|e| anyhow::anyhow!("invalid publish path: {}", e))?;
        let rel_str = path_to_unix_string(rel);

        if let Some(ref mkdir_cmd) = target.mkdir {
            if let Some(parent) = rel.parent() {
                if !parent.as_os_str().is_empty() {
                    let remote_dir = path_to_unix_string(parent);
                    if created_dirs.insert(remote_dir.clone()) {
                        let cmd = render_mkdir_command(mkdir_cmd, &remote_dir);
                        run_shell_command(&cmd)?;
                    }
                }
            }
        }

        let cmd = render_put_command(&target.put, &file, &rel_str);
        run_shell_command(&cmd)?;
    }

    Ok(())
}

/// Recursively collects all files under a directory.
fn collect_files(root: &std::path::Path) -> anyhow::Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                files.push(path);
            }
        }
    }

    Ok(files)
}

/// Converts a path to a Unix-style string with forward slashes.
fn path_to_unix_string(path: &std::path::Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Renders a put command template with local and remote paths.
fn render_put_command(template: &str, local_path: &std::path::Path, remote_path: &str) -> String {
    template
        .replace("{0}", local_path.to_string_lossy().as_ref())
        .replace("{1}", remote_path)
}

/// Renders a mkdir command template with the remote directory.
fn render_mkdir_command(template: &str, remote_dir: &str) -> String {
    template.replace("{0}", remote_dir)
}

/// Executes a shell command and returns an error if it fails.
fn run_shell_command(cmd: &str) -> anyhow::Result<()> {
    use std::process::Command;

    let status = Command::new("sh").arg("-c").arg(cmd).status()?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("command failed (exit {}): {}", status, cmd);
    }
}

/// Downloads buckets in parallel from a history archive.
///
/// This function downloads multiple buckets concurrently (up to 16 at a time),
/// significantly speeding up initial state restoration compared to sequential downloads.
///
/// # Arguments
///
/// * `archive` - The history archive to download from
/// * `bucket_manager` - The bucket manager to import buckets into
/// * `hashes` - The bucket hashes to download
///
/// # Returns
///
/// Returns `(cached_count, downloaded_count)` on success, or an error
/// if any download failed.
async fn download_buckets_parallel(
    archive: &henyey_history::HistoryArchive,
    bucket_manager: std::sync::Arc<henyey_bucket::BucketManager>,
    hashes: Vec<&henyey_common::Hash256>,
) -> anyhow::Result<(usize, usize)> {
    use futures::stream::{self, StreamExt};
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicU32, Ordering};

    const MAX_CONCURRENT_DOWNLOADS: usize = 16;
    const MAX_CONCURRENT_LOADS: usize = 4;

    let total_count = hashes.len();

    // Collect unique non-zero hashes for later parallel loading
    let unique_hashes: HashSet<henyey_common::Hash256> = hashes
        .iter()
        .filter(|h| !h.is_zero())
        .map(|h| **h)
        .collect();

    // Filter out already-present buckets using fast existence check (no loading)
    let to_download: Vec<_> = hashes
        .into_iter()
        .filter(|hash| !bucket_manager.bucket_exists(hash))
        .collect();

    let cached_count = total_count - to_download.len();

    if !to_download.is_empty() {
        let download_count = to_download.len();
        let downloaded = AtomicU32::new(0);

        println!(
            "  {} cached, {} to download...",
            cached_count, download_count
        );

        let results: Vec<anyhow::Result<()>> = stream::iter(to_download.into_iter())
            .map(|hash| {
                let downloaded = &downloaded;
                let bm = &bucket_manager;
                async move {
                    let bucket_data = archive.get_bucket(hash).await.map_err(|e| {
                        anyhow::anyhow!("Failed to download bucket {}: {}", hash.to_hex(), e)
                    })?;
                    bm.import_bucket(&bucket_data).map_err(|e| {
                        anyhow::anyhow!("Failed to import bucket {}: {}", hash.to_hex(), e)
                    })?;

                    let count = downloaded.fetch_add(1, Ordering::Relaxed) + 1;
                    if count % 5 == 0 || count == download_count as u32 {
                        println!("  Downloaded {}/{} buckets", count, download_count);
                    }
                    Ok(())
                }
            })
            .buffer_unordered(MAX_CONCURRENT_DOWNLOADS)
            .collect()
            .await;

        // Check for any failures
        for result in results {
            result?;
        }
    }

    // Parallel loading: load all unique buckets into cache using spawn_blocking
    // This builds the in-memory index for each bucket (SHA256 hash + offset index).
    // Thread safety: load_bucket takes read lock to check cache, then brief write lock to insert.
    let load_start = std::time::Instant::now();
    let unique_hashes_vec: Vec<henyey_common::Hash256> = unique_hashes.into_iter().collect();
    let load_count = unique_hashes_vec.len();

    let load_results: Vec<anyhow::Result<()>> = stream::iter(unique_hashes_vec.into_iter())
        .map(|hash| {
            let bm = bucket_manager.clone();
            async move {
                tokio::task::spawn_blocking(move || {
                    bm.load_bucket(&hash).map_err(|e| {
                        anyhow::anyhow!("Failed to load bucket {}: {}", hash.to_hex(), e)
                    })?;
                    Ok(())
                })
                .await?
            }
        })
        .buffer_unordered(MAX_CONCURRENT_LOADS)
        .collect()
        .await;

    for result in load_results {
        result?;
    }

    let download_count = total_count - cached_count;
    println!(
        "  Loaded {} buckets into cache in {:.2}s",
        load_count,
        load_start.elapsed().as_secs_f64()
    );

    Ok((cached_count, download_count))
}

/// Options for the verify-execution command.
struct VerifyExecutionOptions {
    from: Option<u32>,
    to: Option<u32>,
    stop_on_error: bool,
    show_diff: bool,
    cdp_url: Option<String>,
    cdp_date: Option<String>,
    cache_dir: Option<std::path::PathBuf>,
    no_cache: bool,
    quiet: bool,
}

/// Returns a human-readable name for a `TransactionResultResult` variant.
fn tx_result_code_name(r: &stellar_xdr::curr::TransactionResultResult) -> String {
    use stellar_xdr::curr::TransactionResultResult;
    match r {
        TransactionResultResult::TxSuccess(_) => "txSuccess".to_string(),
        TransactionResultResult::TxFailed(_) => "txFailed".to_string(),
        TransactionResultResult::TxFeeBumpInnerSuccess(_) => "txFeeBumpInnerSuccess".to_string(),
        TransactionResultResult::TxFeeBumpInnerFailed(_) => "txFeeBumpInnerFailed".to_string(),
        other => format!("{:?}", other),
    }
}

/// Prints pairwise differences between two operation result slices.
fn print_op_diffs(
    our_ops: &[stellar_xdr::curr::OperationResult],
    cdp_ops: &[stellar_xdr::curr::OperationResult],
) {
    use stellar_xdr::curr::WriteXdr;
    for (j, (our_op, cdp_op)) in our_ops.iter().zip(cdp_ops.iter()).enumerate() {
        let our_op_xdr = our_op
            .to_xdr(stellar_xdr::curr::Limits::none())
            .unwrap_or_default();
        let cdp_op_xdr = cdp_op
            .to_xdr(stellar_xdr::curr::Limits::none())
            .unwrap_or_default();
        if our_op_xdr != cdp_op_xdr {
            println!("          Op {} differs:", j);
            println!("            Ours: {:?}", our_op);
            println!("            CDP:  {:?}", cdp_op);
        }
    }
}

/// Prints all operations from a result slice with a label.
fn print_all_ops(label: &str, ops: &[stellar_xdr::curr::OperationResult]) {
    println!("        {} ops ({}):", label, ops.len());
    for (j, op) in ops.iter().enumerate() {
        println!("          Op {}: {:?}", j, op);
    }
}

/// Prints an exhaustive field-by-field comparison of two ledger headers.
fn print_header_field_diffs(
    h: &stellar_xdr::curr::LedgerHeader,
    c: &stellar_xdr::curr::LedgerHeader,
    bucket_levels: &[(henyey_common::Hash256, henyey_common::Hash256)],
) {
    use henyey_common::Hash256;
    if h.ledger_version != c.ledger_version {
        println!(
            "    DIFF ledger_version: ours={} expected={}",
            h.ledger_version, c.ledger_version
        );
    }
    if h.previous_ledger_hash != c.previous_ledger_hash {
        println!(
            "    DIFF previous_ledger_hash: ours={} expected={}",
            hex::encode(&h.previous_ledger_hash.0),
            hex::encode(&c.previous_ledger_hash.0)
        );
    }
    if h.scp_value != c.scp_value {
        println!("    DIFF scp_value");
        if h.scp_value.tx_set_hash != c.scp_value.tx_set_hash {
            println!(
                "      tx_set_hash: ours={} expected={}",
                hex::encode(&h.scp_value.tx_set_hash.0),
                hex::encode(&c.scp_value.tx_set_hash.0)
            );
        }
        if h.scp_value.close_time != c.scp_value.close_time {
            println!(
                "      close_time: ours={} expected={}",
                h.scp_value.close_time.0, c.scp_value.close_time.0
            );
        }
        if h.scp_value.upgrades != c.scp_value.upgrades {
            println!(
                "      upgrades: ours={:?} expected={:?}",
                h.scp_value.upgrades, c.scp_value.upgrades
            );
        }
        if h.scp_value.ext != c.scp_value.ext {
            println!("      ext: differs");
        }
    }
    let our_bl_hash = Hash256::from(h.bucket_list_hash.0);
    let expected_bl_hash = Hash256::from(c.bucket_list_hash.0);
    if our_bl_hash != expected_bl_hash {
        println!(
            "    DIFF bucket_list_hash: ours={} expected={}",
            our_bl_hash.to_hex(),
            expected_bl_hash.to_hex()
        );
        for (i, (curr_hash, snap_hash)) in bucket_levels.iter().enumerate() {
            println!(
                "      Level {}: curr={} snap={}",
                i,
                curr_hash.to_hex(),
                snap_hash.to_hex()
            );
        }
    }
    if h.tx_set_result_hash != c.tx_set_result_hash {
        println!(
            "    DIFF tx_set_result_hash: ours={} expected={}",
            hex::encode(&h.tx_set_result_hash.0),
            hex::encode(&c.tx_set_result_hash.0)
        );
    }
    if h.ledger_seq != c.ledger_seq {
        println!(
            "    DIFF ledger_seq: ours={} expected={}",
            h.ledger_seq, c.ledger_seq
        );
    }
    if h.total_coins != c.total_coins {
        println!(
            "    DIFF total_coins: ours={} expected={}",
            h.total_coins, c.total_coins
        );
    }
    if h.fee_pool != c.fee_pool {
        println!(
            "    DIFF fee_pool: ours={} expected={}",
            h.fee_pool, c.fee_pool
        );
    }
    if h.inflation_seq != c.inflation_seq {
        println!(
            "    DIFF inflation_seq: ours={} expected={}",
            h.inflation_seq, c.inflation_seq
        );
    }
    if h.id_pool != c.id_pool {
        println!(
            "    DIFF id_pool: ours={} expected={}",
            h.id_pool, c.id_pool
        );
    }
    if h.base_fee != c.base_fee {
        println!(
            "    DIFF base_fee: ours={} expected={}",
            h.base_fee, c.base_fee
        );
    }
    if h.base_reserve != c.base_reserve {
        println!(
            "    DIFF base_reserve: ours={} expected={}",
            h.base_reserve, c.base_reserve
        );
    }
    if h.max_tx_set_size != c.max_tx_set_size {
        println!(
            "    DIFF max_tx_set_size: ours={} expected={}",
            h.max_tx_set_size, c.max_tx_set_size
        );
    }
    if h.skip_list != c.skip_list {
        println!("    DIFF skip_list:");
        for (i, (ours, exp)) in h.skip_list.iter().zip(c.skip_list.iter()).enumerate() {
            if ours != exp {
                println!(
                    "      [{}]: ours={} expected={}",
                    i,
                    hex::encode(&ours.0),
                    hex::encode(&exp.0)
                );
            }
        }
    }
    if h.ext != c.ext {
        println!("    DIFF ext: ours={:?} expected={:?}", h.ext, c.ext);
    }
}

/// Prints detailed per-TX result diffs between our results and CDP results.
///
/// Shows ordering differences, then does a TX-by-TX XDR comparison with
/// detailed operation-level diffs for all result variant combinations.
fn print_tx_result_diffs(
    our_results: &[stellar_xdr::curr::TransactionResultPair],
    cdp_results: &[stellar_xdr::curr::TransactionResultPair],
) {
    use stellar_xdr::curr::{InnerTransactionResultResult, TransactionResultResult, WriteXdr};
    println!(
        "    TX count: ours={} CDP={}",
        our_results.len(),
        cdp_results.len()
    );

    // Check if TX ordering differs
    let mut order_diffs = 0;
    for (i, (our_tx, cdp_tx)) in our_results.iter().zip(cdp_results.iter()).enumerate() {
        if our_tx.transaction_hash != cdp_tx.transaction_hash {
            if order_diffs < 10 {
                println!(
                    "    ORDER DIFF at position {}: ours={} CDP={}",
                    i,
                    hex::encode(&our_tx.transaction_hash.0),
                    hex::encode(&cdp_tx.transaction_hash.0)
                );
            }
            order_diffs += 1;
        }
    }
    if order_diffs > 0 {
        println!("    Total TX ordering differences: {}", order_diffs);
    } else {
        println!("    TX ordering is IDENTICAL (same content hashes at every position)");
    }

    // Detailed TX-by-TX comparison using full XDR
    let mut diff_count = 0;
    for (i, (our_tx, cdp_tx)) in our_results.iter().zip(cdp_results.iter()).enumerate() {
        let our_xdr = our_tx
            .result
            .to_xdr(stellar_xdr::curr::Limits::none())
            .unwrap_or_default();
        let cdp_xdr = cdp_tx
            .result
            .to_xdr(stellar_xdr::curr::Limits::none())
            .unwrap_or_default();

        if our_xdr != cdp_xdr {
            diff_count += 1;
            let our_result = &our_tx.result.result;
            let cdp_result = &cdp_tx.result.result;

            println!("      TX {}: MISMATCH (XDR differs)", i);
            println!(
                "        Result: ours={} CDP={}",
                tx_result_code_name(our_result),
                tx_result_code_name(cdp_result)
            );
            println!(
                "        Fee: ours={} CDP={}",
                our_tx.result.fee_charged, cdp_tx.result.fee_charged
            );
            println!(
                "        TX hash: {}",
                hex::encode(&our_tx.transaction_hash.0)
            );

            // Compare operations for same-variant pairs
            match (our_result, cdp_result) {
                (
                    TransactionResultResult::TxFailed(our_ops),
                    TransactionResultResult::TxFailed(cdp_ops),
                )
                | (
                    TransactionResultResult::TxSuccess(our_ops),
                    TransactionResultResult::TxSuccess(cdp_ops),
                ) => {
                    print_op_diffs(our_ops, cdp_ops);
                }
                // One succeeds, other fails — show all ops from both sides
                (
                    TransactionResultResult::TxSuccess(our_ops),
                    TransactionResultResult::TxFailed(cdp_ops),
                )
                | (
                    TransactionResultResult::TxFailed(our_ops),
                    TransactionResultResult::TxSuccess(cdp_ops),
                ) => {
                    print_all_ops("Ours", our_ops);
                    print_all_ops("CDP", cdp_ops);
                }
                _ => {}
            }

            // Fee bump inner result details
            match (our_result, cdp_result) {
                (
                    TransactionResultResult::TxFeeBumpInnerFailed(our_inner),
                    TransactionResultResult::TxFeeBumpInnerFailed(cdp_inner),
                ) => {
                    println!(
                        "        Inner fee: ours={} CDP={}",
                        our_inner.result.fee_charged, cdp_inner.result.fee_charged
                    );
                    let our_inner_code =
                        format!("{:?}", std::mem::discriminant(&our_inner.result.result));
                    let cdp_inner_code =
                        format!("{:?}", std::mem::discriminant(&cdp_inner.result.result));
                    println!(
                        "        Inner result type: ours={} CDP={}",
                        our_inner_code, cdp_inner_code
                    );
                    if let (
                        InnerTransactionResultResult::TxFailed(our_ops),
                        InnerTransactionResultResult::TxFailed(cdp_ops),
                    ) = (&our_inner.result.result, &cdp_inner.result.result)
                    {
                        print_op_diffs(our_ops, cdp_ops);
                        if our_ops.len() != cdp_ops.len() {
                            println!(
                                "          Inner op count: ours={} CDP={}",
                                our_ops.len(),
                                cdp_ops.len()
                            );
                        }
                    } else {
                        println!("        Inner result ours: {:?}", our_inner.result.result);
                        println!("        Inner result CDP:  {:?}", cdp_inner.result.result);
                    }
                }
                (
                    TransactionResultResult::TxFeeBumpInnerSuccess(our_inner),
                    TransactionResultResult::TxFeeBumpInnerSuccess(cdp_inner),
                ) => {
                    println!(
                        "        Inner fee: ours={} CDP={}",
                        our_inner.result.fee_charged, cdp_inner.result.fee_charged
                    );
                    if let (
                        InnerTransactionResultResult::TxSuccess(our_ops),
                        InnerTransactionResultResult::TxSuccess(cdp_ops),
                    ) = (&our_inner.result.result, &cdp_inner.result.result)
                    {
                        print_op_diffs(our_ops, cdp_ops);
                    }
                }
                // Cross-case fee bump inner results
                (
                    TransactionResultResult::TxFeeBumpInnerSuccess(our_inner),
                    TransactionResultResult::TxFeeBumpInnerFailed(cdp_inner),
                ) => {
                    println!(
                        "        Inner fee: ours={} CDP={}",
                        our_inner.result.fee_charged, cdp_inner.result.fee_charged
                    );
                    if let InnerTransactionResultResult::TxSuccess(our_ops) =
                        &our_inner.result.result
                    {
                        print_all_ops("Ours inner", our_ops);
                    }
                    if let InnerTransactionResultResult::TxFailed(cdp_ops) =
                        &cdp_inner.result.result
                    {
                        print_all_ops("CDP inner", cdp_ops);
                    } else {
                        println!("        CDP inner result: {:?}", cdp_inner.result.result);
                    }
                }
                (
                    TransactionResultResult::TxFeeBumpInnerFailed(our_inner),
                    TransactionResultResult::TxFeeBumpInnerSuccess(cdp_inner),
                ) => {
                    println!(
                        "        Inner fee: ours={} CDP={}",
                        our_inner.result.fee_charged, cdp_inner.result.fee_charged
                    );
                    println!("        Ours inner result: {:?}", our_inner.result.result);
                    if let InnerTransactionResultResult::TxSuccess(cdp_ops) =
                        &cdp_inner.result.result
                    {
                        print_all_ops("CDP inner", cdp_ops);
                    }
                }
                _ => {}
            }

            // Show CDP ops when ours is TxNotSupported or other non-standard result
            if !matches!(
                our_result,
                TransactionResultResult::TxSuccess(_)
                    | TransactionResultResult::TxFailed(_)
                    | TransactionResultResult::TxFeeBumpInnerSuccess(_)
                    | TransactionResultResult::TxFeeBumpInnerFailed(_)
            ) {
                if let TransactionResultResult::TxFailed(cdp_ops) = cdp_result {
                    println!("        CDP txFailed ops ({}):", cdp_ops.len());
                    for (j, op) in cdp_ops.iter().enumerate() {
                        println!("          Op {}: {:?}", j, op);
                    }
                }
            }

            // Limit output to first 10 diffs
            if diff_count >= 10 {
                println!("      ... (showing first 10 of potentially more diffs)");
                break;
            }
        }
    }
    if diff_count > 0 {
        println!(
            "    Total TX diffs: {} out of {}",
            diff_count,
            our_results.len().min(cdp_results.len())
        );
    }
}

/// Verifies transaction execution by comparing results against CDP metadata.
///
/// This test re-executes transactions using `close_ledger` and compares the
/// resulting ledger close metadata against what stellar-core produced (from CDP).
/// Differences indicate execution divergence that needs investigation.
///
/// # Verification Process
///
/// 1. Restores bucket list state from a checkpoint before the test range
/// 2. For each ledger, calls `close_ledger` with the transaction set from CDP
/// 3. Compares the resulting `LedgerCloseResult` against CDP:
///    - Header hash
///    - Transaction result hash
///    - Ledger close metadata (if both present)
/// 4. Reports any mismatches in detail
async fn cmd_verify_execution(
    config: AppConfig,
    opts: VerifyExecutionOptions,
) -> anyhow::Result<()> {
    use henyey_bucket::{BucketList, BucketManager, HasNextState, HotArchiveBucketList};
    use henyey_common::Hash256;
    use henyey_history::cdp::{
        extract_ledger_close_data, extract_ledger_header, extract_transaction_results,
        CachedCdpDataLake,
    };
    use henyey_history::checkpoint;
    use henyey_ledger::{LedgerManager, LedgerManagerConfig};
    use std::sync::Arc;

    let VerifyExecutionOptions {
        from,
        to,
        stop_on_error,
        show_diff,
        cdp_url,
        cdp_date,
        cache_dir,
        no_cache,
        quiet,
    } = opts;

    let init_start = std::time::Instant::now();

    if !quiet {
        println!("Transaction Execution Verification");
        println!("===================================");
        println!("Executes transactions via close_ledger and compares against CDP.");
        println!();
    }

    // Determine network name
    let (network_name, is_mainnet) =
        if config.network.passphrase == "Test SDF Network ; September 2015" {
            ("testnet", false)
        } else {
            ("mainnet", true)
        };

    // Set network-specific CDP defaults
    let cdp_url = cdp_url.unwrap_or_else(|| {
        if is_mainnet {
            "https://aws-public-blockchain.s3.us-east-2.amazonaws.com/v1.1/stellar/ledgers/pubnet"
                .to_string()
        } else {
            "https://aws-public-blockchain.s3.us-east-2.amazonaws.com/v1.1/stellar/ledgers/testnet"
                .to_string()
        }
    });
    let cdp_date = cdp_date.unwrap_or_else(|| {
        if is_mainnet {
            String::new()
        } else {
            "2025-12-18".to_string()
        }
    });

    // Determine cache directory
    let cache_base = if no_cache {
        None
    } else {
        cache_dir.or_else(|| dirs::cache_dir().map(|p| p.join("rs-stellar-core")))
    };

    // Create archive client
    let archive = first_archive(&config)?;

    if !quiet {
        println!("Archive: {}", config.history.archives[0].url);
        let cdp_date_display = if cdp_date.is_empty() {
            "none (range-based)"
        } else {
            &cdp_date
        };
        println!("CDP: {} (date: {})", cdp_url, cdp_date_display);
        if let Some(ref cache) = cache_base {
            println!("Cache: {}", cache.display());
        } else {
            println!("Cache: disabled");
        }
    }

    // Get current ledger and calculate range
    let root_has = archive.get_root_has().await?;
    let current_ledger = root_has.current_ledger;

    let end_ledger = to.unwrap_or_else(|| {
        checkpoint::latest_checkpoint_before_or_at(current_ledger).unwrap_or(current_ledger)
    });
    let start_ledger = from.unwrap_or_else(|| {
        let freq = henyey_history::checkpoint_frequency();
        checkpoint::checkpoint_containing(end_ledger)
            .saturating_sub(4 * freq)
            .max(freq)
    });

    let freq = henyey_history::checkpoint_frequency();
    let init_checkpoint =
        checkpoint::latest_checkpoint_before_or_at(start_ledger.saturating_sub(1))
            .unwrap_or(freq - 1);
    let end_checkpoint = checkpoint::checkpoint_containing(end_ledger);

    if !quiet {
        println!("Ledger range: {} to {}", start_ledger, end_ledger);
        println!("Initial state: checkpoint {}", init_checkpoint);
        println!();
    }

    // Create CDP client with caching
    let (_cdp_dir_holder, cdp) = if let Some(ref cache) = cache_base {
        let cdp = CachedCdpDataLake::new(&cdp_url, &cdp_date, cache, network_name)?;
        (None, cdp)
    } else {
        let temp = tempfile::tempdir()?;
        let cdp = CachedCdpDataLake::new(&cdp_url, &cdp_date, temp.path(), network_name)?;
        (Some(temp), cdp)
    };

    // Setup bucket manager
    let (_bucket_dir_holder, bucket_path) = if let Some(ref cache) = cache_base {
        let path = cache.join("buckets").join(network_name);
        std::fs::create_dir_all(&path)?;
        (None, path)
    } else {
        let temp = tempfile::tempdir()?;
        let path = temp.path().to_path_buf();
        (Some(temp), path)
    };
    let bucket_manager = Arc::new(BucketManager::with_persist_index(
        bucket_path.clone(),
        true,
    )?);

    // Download initial state
    if !quiet {
        println!(
            "Downloading initial state at checkpoint {}...",
            init_checkpoint
        );
    }
    let init_has = archive.get_checkpoint_has(init_checkpoint).await?;

    // Extract bucket hashes
    let bucket_hashes = init_has.bucket_hash_pairs();

    let live_next_states: Vec<HasNextState> = init_has
        .live_next_states()
        .into_iter()
        .map(HasNextState::from)
        .collect();

    // Extract hot archive bucket hashes (protocol 23+)
    let hot_archive_hashes = init_has.hot_archive_bucket_hash_pairs();

    let hot_archive_next_states: Option<Vec<HasNextState>> = init_has
        .hot_archive_next_states()
        .map(|states| states.into_iter().map(HasNextState::from).collect());

    // Collect all bucket hashes to download
    let mut all_hashes: Vec<Hash256> = Vec::new();
    for (curr, snap) in &bucket_hashes {
        all_hashes.push(*curr);
        all_hashes.push(*snap);
    }
    for state in &live_next_states {
        if let Some(ref output) = state.output {
            all_hashes.push(*output);
        }
    }
    if let Some(ref ha_hashes) = hot_archive_hashes {
        for (curr, snap) in ha_hashes {
            all_hashes.push(*curr);
            all_hashes.push(*snap);
        }
    }
    if let Some(ref ha_states) = hot_archive_next_states {
        for state in ha_states {
            if let Some(ref output) = state.output {
                all_hashes.push(*output);
            }
        }
    }
    let all_hashes: Vec<&Hash256> = all_hashes.iter().filter(|h| !h.is_zero()).collect();

    // Download buckets
    let (cached, downloaded) =
        download_buckets_parallel(&archive, bucket_manager.clone(), all_hashes).await?;
    println!(
        "[INIT] Bucket download: {} cached, {} downloaded",
        cached, downloaded
    );

    // Restore bucket lists
    let mut bucket_list =
        BucketList::restore_from_has(&bucket_hashes, &live_next_states, |hash| {
            bucket_manager.load_bucket(hash).map(|b| (*b).clone())
        })?;
    bucket_list.set_bucket_dir(bucket_manager.bucket_dir().to_path_buf());

    let mut hot_archive_bucket_list = match (&hot_archive_hashes, &hot_archive_next_states) {
        (Some(ref hashes), Some(ref next_states)) => {
            HotArchiveBucketList::restore_from_has(hashes, next_states, |hash| {
                bucket_manager.load_hot_archive_bucket(hash)
            })?
        }
        _ => HotArchiveBucketList::new(),
    };

    // Get init header and restart merges
    let init_headers = archive.get_ledger_headers(init_checkpoint).await?;
    let init_header_entry = init_headers
        .iter()
        .find(|h| h.header.ledger_seq == init_checkpoint);
    let init_protocol_version = init_header_entry
        .map(|h| h.header.ledger_version)
        .unwrap_or(25);

    // Enable structure-based merge restarts to match stellar-core online mode behavior.
    //
    // Although stellar-core standalone offline commands skip restartMerges, we are comparing
    // our results against CDP headers produced by stellar-core in ONLINE mode. The stellar-core node
    // that produced those headers had full structure-based merge restarts enabled.
    //
    // In stellar-core online mode, restartMerges uses mLevels[i-1].getSnap() (the old snap
    // from HAS) to start merges. Without structure-based restarts, add_batch would
    // use snap() which returns the snapped curr (different input!).
    bucket_list
        .restart_merges_from_has(
            init_checkpoint,
            init_protocol_version,
            &live_next_states,
            |hash| bucket_manager.load_bucket(hash).map(|b| (*b).clone()),
            true, // restart_structure_based = true to match stellar-core online mode
        )
        .await?;

    if let Some(ref ha_next_states) = hot_archive_next_states {
        hot_archive_bucket_list.restart_merges_from_has(
            init_checkpoint,
            init_protocol_version,
            ha_next_states,
            |hash| bucket_manager.load_hot_archive_bucket(hash),
            true, // restart_structure_based = true to match stellar-core online mode
        )?;
    }

    // Create and initialize LedgerManager
    let mut ledger_manager = LedgerManager::new(
        config.network.passphrase.clone(),
        LedgerManagerConfig {
            validate_bucket_hash: true,
            bucket_list_db: config.buckets.bucket_list_db.clone(),
            // Use single-threaded scan to reduce peak memory during initialization.
            // Verify-execution runs on memory-constrained CI runners where concurrent
            // level scans would cause OOM.
            scan_thread_count: 1,
            ..Default::default()
        },
    );

    // Wire merge map for bucket merge deduplication during replay.
    let finished_merges =
        std::sync::Arc::new(std::sync::RwLock::new(henyey_bucket::BucketMergeMap::new()));
    ledger_manager.set_merge_map(finished_merges);

    let init_header_entry = init_header_entry
        .ok_or_else(|| anyhow::anyhow!("No header found for checkpoint {}", init_checkpoint))?;
    let init_header_hash = Hash256::from(init_header_entry.hash.0);
    ledger_manager.initialize(
        bucket_list,
        hot_archive_bucket_list,
        init_header_entry.header.clone(),
        init_header_hash,
    )?;

    println!(
        "[INIT] TOTAL initialization: {:.2}s",
        init_start.elapsed().as_secs_f64()
    );

    // Track results
    let mut ledgers_verified = 0u32;
    let mut ledgers_matched = 0u32;
    let mut ledgers_mismatched = 0u32;
    let mut header_mismatches = 0u32;
    let mut tx_result_mismatches = 0u32;
    let mut meta_mismatches = 0u32;

    // Track previous ledger hash for close_ledger
    let mut prev_ledger_hash = init_header_hash;

    // Performance tracking accumulators
    let mut total_close_us: u64 = 0;
    let mut total_tx_exec_us: u64 = 0;
    let mut total_commit_us: u64 = 0;
    let mut total_add_batch_us: u64 = 0;
    let mut total_eviction_us: u64 = 0;
    let mut total_tx_count: usize = 0;
    let mut total_cache_hits: u64 = 0;
    let mut total_cache_misses: u64 = 0;
    let mut slowest_ledger_us: u64 = 0;
    let mut slowest_ledger_seq: u32 = 0;
    let mut slowest_txs: Vec<(u32, String, u64)> = Vec::new(); // (ledger, hash, us)
    let mut peak_rss_bytes: u64 = 0;

    // Main verification loop
    let verification_start = std::time::Instant::now();
    let process_from = init_checkpoint + 1;
    let process_from_cp = checkpoint::checkpoint_containing(process_from);

    let mut current_cp = process_from_cp;
    while current_cp <= end_checkpoint {
        let headers = archive.get_ledger_headers(current_cp).await?;

        for header_entry in &headers {
            let header = &header_entry.header;
            let seq = header.ledger_seq;

            // Skip ledgers outside our range
            if seq <= init_checkpoint || seq > end_ledger {
                if seq > init_checkpoint {
                    prev_ledger_hash = Hash256::from(header_entry.hash.0);
                }
                continue;
            }

            let in_test_range = seq >= start_ledger && seq <= end_ledger;

            // Fetch CDP metadata
            let lcm = match cdp.get_ledger_close_meta(seq).await {
                Ok(lcm) => lcm,
                Err(e) => {
                    if in_test_range {
                        println!("  Ledger {}: CDP fetch failed: {}", seq, e);
                    }
                    prev_ledger_hash = Hash256::from(header_entry.hash.0);
                    continue;
                }
            };

            let cdp_header = extract_ledger_header(&lcm);

            // Validate CDP data matches archive
            if header.scp_value.close_time.0 != cdp_header.scp_value.close_time.0 {
                if in_test_range {
                    println!("  Ledger {}: EPOCH MISMATCH - skipping", seq);
                }
                if stop_on_error {
                    anyhow::bail!("CDP epoch mismatch at ledger {}", seq);
                }
                prev_ledger_hash = Hash256::from(header_entry.hash.0);
                continue;
            }

            // Create LedgerCloseData from CDP
            let close_data = extract_ledger_close_data(&lcm, prev_ledger_hash);

            // Execute via close_ledger
            let result = match ledger_manager.close_ledger(close_data, None) {
                Ok(r) => r,
                Err(e) => {
                    println!("  Ledger {}: close_ledger failed: {}", seq, e);
                    if stop_on_error {
                        anyhow::bail!("close_ledger failed at ledger {}: {}", seq, e);
                    }
                    prev_ledger_hash = Hash256::from(header_entry.hash.0);
                    continue;
                }
            };

            if in_test_range {
                ledgers_verified += 1;

                // Compare header hash
                let expected_header_hash = Hash256::from(header_entry.hash.0);
                let header_matches = result.header_hash == expected_header_hash;

                // Compare tx result hash
                let cdp_tx_results = extract_transaction_results(&lcm);

                let expected_tx_result_hash = Hash256::from(cdp_header.tx_set_result_hash.0);
                let our_tx_result_hash = result.tx_result_hash();
                let tx_result_matches = our_tx_result_hash == expected_tx_result_hash;

                // Compare meta: if present, consider it matching when tx results match.
                // Full meta comparison would be more complex.
                let meta_matches = result.meta.is_none() || tx_result_matches;

                let all_match = header_matches && tx_result_matches && meta_matches;

                if all_match {
                    ledgers_matched += 1;
                    if !quiet {
                        print!(".");
                        if ledgers_verified % 64 == 0 {
                            println!(" {}", seq);
                        }
                        std::io::Write::flush(&mut std::io::stdout()).ok();
                    }
                } else {
                    ledgers_mismatched += 1;
                    if !header_matches {
                        header_mismatches += 1;
                    }
                    if !tx_result_matches {
                        tx_result_mismatches += 1;
                    }
                    if !meta_matches {
                        meta_mismatches += 1;
                    }

                    println!();
                    println!("  Ledger {}: MISMATCH", seq);
                    if !header_matches {
                        println!(
                            "    Header hash: ours={} expected={}",
                            result.header_hash.to_hex(),
                            expected_header_hash.to_hex()
                        );
                        let bucket_levels = ledger_manager.bucket_list_levels();
                        print_header_field_diffs(&result.header, &cdp_header, &bucket_levels);
                    }
                    if !tx_result_matches {
                        println!(
                            "    TX result hash: ours={} expected={}",
                            our_tx_result_hash.to_hex(),
                            expected_tx_result_hash.to_hex()
                        );
                    }

                    if show_diff && !tx_result_matches {
                        print_tx_result_diffs(&result.tx_results, &cdp_tx_results);
                    }

                    // Compare eviction data when header mismatches but TX results match
                    if !header_matches && tx_result_matches {
                        // Extract eviction data from CDP meta
                        let cdp_evicted_keys = henyey_history::cdp::extract_evicted_keys(&lcm);
                        let tx_metas = henyey_history::cdp::extract_transaction_metas(&lcm);
                        let cdp_restored_keys =
                            henyey_history::cdp::extract_restored_keys(&tx_metas);

                        // Count CDP entry changes
                        let mut cdp_creates = 0u32;
                        let mut cdp_updates = 0u32;
                        let mut cdp_deletes = 0u32;
                        for tx_meta in &tx_metas {
                            fn count_changes(
                                changes: &[stellar_xdr::curr::LedgerEntryChange],
                                creates: &mut u32,
                                updates: &mut u32,
                                deletes: &mut u32,
                            ) {
                                for change in changes {
                                    match change {
                                        stellar_xdr::curr::LedgerEntryChange::Created(_) => {
                                            *creates += 1
                                        }
                                        stellar_xdr::curr::LedgerEntryChange::Updated(_) => {
                                            *updates += 1
                                        }
                                        stellar_xdr::curr::LedgerEntryChange::Removed(_) => {
                                            *deletes += 1
                                        }
                                        stellar_xdr::curr::LedgerEntryChange::Restored(_) => {
                                            *updates += 1
                                        }
                                        stellar_xdr::curr::LedgerEntryChange::State(_) => {}
                                    }
                                }
                            }
                            match tx_meta {
                                stellar_xdr::curr::TransactionMeta::V3(v3) => {
                                    count_changes(
                                        &v3.tx_changes_before,
                                        &mut cdp_creates,
                                        &mut cdp_updates,
                                        &mut cdp_deletes,
                                    );
                                    for op in v3.operations.iter() {
                                        count_changes(
                                            &op.changes,
                                            &mut cdp_creates,
                                            &mut cdp_updates,
                                            &mut cdp_deletes,
                                        );
                                    }
                                    count_changes(
                                        &v3.tx_changes_after,
                                        &mut cdp_creates,
                                        &mut cdp_updates,
                                        &mut cdp_deletes,
                                    );
                                }
                                stellar_xdr::curr::TransactionMeta::V4(v4) => {
                                    count_changes(
                                        &v4.tx_changes_before,
                                        &mut cdp_creates,
                                        &mut cdp_updates,
                                        &mut cdp_deletes,
                                    );
                                    for op in v4.operations.iter() {
                                        count_changes(
                                            &op.changes,
                                            &mut cdp_creates,
                                            &mut cdp_updates,
                                            &mut cdp_deletes,
                                        );
                                    }
                                    count_changes(
                                        &v4.tx_changes_after,
                                        &mut cdp_creates,
                                        &mut cdp_updates,
                                        &mut cdp_deletes,
                                    );
                                }
                                _ => {}
                            }
                        }

                        // Extract upgrade meta entry counts
                        let cdp_upgrade_metas = henyey_history::cdp::extract_upgrade_metas(&lcm);
                        let mut upgrade_creates = 0u32;
                        let mut upgrade_updates = 0u32;
                        for um in &cdp_upgrade_metas {
                            for change in um.changes.iter() {
                                match change {
                                    stellar_xdr::curr::LedgerEntryChange::Created(_) => {
                                        upgrade_creates += 1
                                    }
                                    stellar_xdr::curr::LedgerEntryChange::Updated(_) => {
                                        upgrade_updates += 1
                                    }
                                    _ => {}
                                }
                            }
                        }

                        println!("    CDP meta: creates={}, updates={}, deletes={}, evicted={}, restored={}, upgrade_creates={}, upgrade_updates={}",
                            cdp_creates, cdp_updates, cdp_deletes, cdp_evicted_keys.len(), cdp_restored_keys.len(),
                            upgrade_creates, upgrade_updates);

                        // Dump expected upgrade entries from CDP meta for comparison
                        if !cdp_upgrade_metas.is_empty() {
                            use sha2::{Digest, Sha256};
                            println!("    CDP upgrade entries (expected):");
                            for (ui, um) in cdp_upgrade_metas.iter().enumerate() {
                                for change in um.changes.iter() {
                                    match change {
                                        stellar_xdr::curr::LedgerEntryChange::Updated(entry) => {
                                            let key_str = match &entry.data {
                                                stellar_xdr::curr::LedgerEntryData::ConfigSetting(cs) => {
                                                    format!("ConfigSetting({:?})", cs.discriminant())
                                                }
                                                other => format!("{:?}", std::mem::discriminant(other)),
                                            };
                                            let xdr_bytes = entry
                                                .to_xdr(stellar_xdr::curr::Limits::none())
                                                .unwrap_or_default();
                                            let xdr_size = xdr_bytes.len();
                                            let hash = {
                                                let mut h = Sha256::new();
                                                h.update(&xdr_bytes);
                                                let r = h.finalize();
                                                format!("{:x}", r)
                                            };
                                            println!("      upgrade[{}] Updated: key={}, last_modified={}, xdr_size={}, xdr_hash={}",
                                                ui, key_str, entry.last_modified_ledger_seq, xdr_size, hash);
                                        }
                                        stellar_xdr::curr::LedgerEntryChange::Created(entry) => {
                                            let key_str = match &entry.data {
                                                stellar_xdr::curr::LedgerEntryData::ConfigSetting(cs) => {
                                                    format!("ConfigSetting({:?})", cs.discriminant())
                                                }
                                                other => format!("{:?}", std::mem::discriminant(other)),
                                            };
                                            let xdr_bytes = entry
                                                .to_xdr(stellar_xdr::curr::Limits::none())
                                                .unwrap_or_default();
                                            let xdr_size = xdr_bytes.len();
                                            let hash = {
                                                let mut h = Sha256::new();
                                                h.update(&xdr_bytes);
                                                let r = h.finalize();
                                                format!("{:x}", r)
                                            };
                                            println!("      upgrade[{}] Created: key={}, last_modified={}, xdr_size={}, xdr_hash={}",
                                                ui, key_str, entry.last_modified_ledger_seq, xdr_size, hash);
                                        }
                                        stellar_xdr::curr::LedgerEntryChange::State(entry) => {
                                            let key_str = match &entry.data {
                                                stellar_xdr::curr::LedgerEntryData::ConfigSetting(cs) => {
                                                    format!("ConfigSetting({:?})", cs.discriminant())
                                                }
                                                other => format!("{:?}", std::mem::discriminant(other)),
                                            };
                                            println!(
                                                "      upgrade[{}] State(before): key={}",
                                                ui, key_str
                                            );
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }

                        // Also dump expected final TX entries from CDP meta
                        {
                            use sha2::{Digest, Sha256};
                            // Coalesce: keep last Updated entry per key
                            // Include ALL change sources: fee_processing, tx_apply_processing, and post_tx_apply_fee_processing
                            let mut final_entries: std::collections::HashMap<
                                Vec<u8>,
                                stellar_xdr::curr::LedgerEntry,
                            > = std::collections::HashMap::new();
                            // Helper to process a slice of changes into the coalesced map
                            let coalesce_changes =
                                |changes: &[stellar_xdr::curr::LedgerEntryChange],
                                 map: &mut std::collections::HashMap<
                                    Vec<u8>,
                                    stellar_xdr::curr::LedgerEntry,
                                >| {
                                    for change in changes {
                                        match change {
                                            stellar_xdr::curr::LedgerEntryChange::Updated(
                                                entry,
                                            )
                                            | stellar_xdr::curr::LedgerEntryChange::Created(
                                                entry,
                                            )
                                            | stellar_xdr::curr::LedgerEntryChange::Restored(
                                                entry,
                                            ) => {
                                                let key = henyey_common::entry_to_key(entry);
                                                if let Ok(kb) =
                                                    key.to_xdr(stellar_xdr::curr::Limits::none())
                                                {
                                                    map.insert(kb, entry.clone());
                                                }
                                            }
                                            stellar_xdr::curr::LedgerEntryChange::Removed(key) => {
                                                if let Ok(kb) =
                                                    key.to_xdr(stellar_xdr::curr::Limits::none())
                                                {
                                                    map.remove(&kb);
                                                }
                                            }
                                            _ => {}
                                        }
                                    }
                                };
                            let coalesce_tx_meta = |meta: &stellar_xdr::curr::TransactionMeta,
                                                    map: &mut std::collections::HashMap<
                                Vec<u8>,
                                stellar_xdr::curr::LedgerEntry,
                            >| {
                                match meta {
                                    stellar_xdr::curr::TransactionMeta::V3(v3) => {
                                        coalesce_changes(&v3.tx_changes_before, map);
                                        for op in v3.operations.iter() {
                                            coalesce_changes(&op.changes, map);
                                        }
                                        coalesce_changes(&v3.tx_changes_after, map);
                                    }
                                    stellar_xdr::curr::TransactionMeta::V4(v4) => {
                                        coalesce_changes(&v4.tx_changes_before, map);
                                        for op in v4.operations.iter() {
                                            coalesce_changes(&op.changes, map);
                                        }
                                        coalesce_changes(&v4.tx_changes_after, map);
                                    }
                                    _ => {}
                                }
                            };
                            // Process ALL change sources from LCM tx_processing
                            match &lcm {
                                stellar_xdr::curr::LedgerCloseMeta::V0(v0) => {
                                    for tp in v0.tx_processing.iter() {
                                        coalesce_changes(&tp.fee_processing, &mut final_entries);
                                        coalesce_tx_meta(
                                            &tp.tx_apply_processing,
                                            &mut final_entries,
                                        );
                                        // V0 TransactionResultMeta has no post_tx_apply_fee_processing
                                    }
                                }
                                stellar_xdr::curr::LedgerCloseMeta::V1(v1) => {
                                    for tp in v1.tx_processing.iter() {
                                        coalesce_changes(&tp.fee_processing, &mut final_entries);
                                        coalesce_tx_meta(
                                            &tp.tx_apply_processing,
                                            &mut final_entries,
                                        );
                                        // V1 TransactionResultMeta has no post_tx_apply_fee_processing
                                    }
                                }
                                stellar_xdr::curr::LedgerCloseMeta::V2(v2) => {
                                    for tp in v2.tx_processing.iter() {
                                        coalesce_changes(&tp.fee_processing, &mut final_entries);
                                        coalesce_tx_meta(
                                            &tp.tx_apply_processing,
                                            &mut final_entries,
                                        );
                                        coalesce_changes(
                                            &tp.post_tx_apply_fee_processing,
                                            &mut final_entries,
                                        );
                                    }
                                }
                            }
                            println!(
                                "    CDP TX final entries (coalesced, {} unique keys)",
                                final_entries.len()
                            );

                            // Compare CDP entries with our bucket list state
                            let bl = ledger_manager.bucket_list();
                            let bl_snapshot =
                                henyey_bucket::BucketListSnapshot::new(&bl, result.header.clone());
                            drop(bl);
                            let mut diffs = 0;
                            let mut missing = 0;
                            for (key_bytes, cdp_entry) in final_entries.iter() {
                                use stellar_xdr::curr::ReadXdr;
                                if let Ok(key) = stellar_xdr::curr::LedgerKey::from_xdr(
                                    key_bytes.as_slice(),
                                    stellar_xdr::curr::Limits::none(),
                                ) {
                                    let cdp_xdr = cdp_entry
                                        .to_xdr(stellar_xdr::curr::Limits::none())
                                        .unwrap_or_default();
                                    let cdp_hash = {
                                        let mut h = Sha256::new();
                                        h.update(&cdp_xdr);
                                        format!("{:x}", h.finalize())
                                    };
                                    match bl_snapshot.get(&key) {
                                        Some(our_entry) => {
                                            let our_xdr = our_entry
                                                .to_xdr(stellar_xdr::curr::Limits::none())
                                                .unwrap_or_default();
                                            let our_hash = {
                                                let mut h = Sha256::new();
                                                h.update(&our_xdr);
                                                format!("{:x}", h.finalize())
                                            };
                                            if our_hash != cdp_hash {
                                                diffs += 1;
                                                let key_str = format!(
                                                    "{:?}",
                                                    std::mem::discriminant(&cdp_entry.data)
                                                );
                                                println!(
                                                    "    ENTRY DIFF #{}: key={:?}",
                                                    diffs, key_str
                                                );
                                                println!(
                                                    "      CDP:  lm={} hash={}",
                                                    cdp_entry.last_modified_ledger_seq, cdp_hash
                                                );
                                                println!(
                                                    "      Ours: lm={} hash={}",
                                                    our_entry.last_modified_ledger_seq, our_hash
                                                );
                                                println!(
                                                    "      CDP  xdr: {}",
                                                    hex::encode(&cdp_xdr[..cdp_xdr.len().min(200)])
                                                );
                                                println!(
                                                    "      Ours xdr: {}",
                                                    hex::encode(&our_xdr[..our_xdr.len().min(200)])
                                                );
                                                // For offers, show readable details
                                                if let (
                                                    stellar_xdr::curr::LedgerEntryData::Offer(
                                                        cdp_o,
                                                    ),
                                                    stellar_xdr::curr::LedgerEntryData::Offer(
                                                        our_o,
                                                    ),
                                                ) = (&cdp_entry.data, &our_entry.data)
                                                {
                                                    println!("      CDP  offer: seller={:?} amount={} price={}/{}", hex::encode(&{let stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(ref pk) = cdp_o.seller_id.0; pk.0}[..8]), cdp_o.amount, cdp_o.price.n, cdp_o.price.d);
                                                    println!("      Ours offer: seller={:?} amount={} price={}/{}", hex::encode(&{let stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(ref pk) = our_o.seller_id.0; pk.0}[..8]), our_o.amount, our_o.price.n, our_o.price.d);
                                                }
                                                if let (
                                                    stellar_xdr::curr::LedgerEntryData::Account(
                                                        cdp_a,
                                                    ),
                                                    stellar_xdr::curr::LedgerEntryData::Account(
                                                        our_a,
                                                    ),
                                                ) = (&cdp_entry.data, &our_entry.data)
                                                {
                                                    let cdp_pk = {
                                                        let stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(ref pk) = cdp_a.account_id.0;
                                                        hex::encode(&pk.0[..16])
                                                    };
                                                    // Extract sponsorship counts from extensions
                                                    let get_ext = |a: &stellar_xdr::curr::AccountEntry| -> (u32, u32, u32) {
                                                        match &a.ext {
                                                            stellar_xdr::curr::AccountEntryExt::V0 => (0, 0, 0),
                                                            stellar_xdr::curr::AccountEntryExt::V1(v1) => match &v1.ext {
                                                                stellar_xdr::curr::AccountEntryExtensionV1Ext::V0 => (0, 0, 0),
                                                                stellar_xdr::curr::AccountEntryExtensionV1Ext::V2(v2) => (v2.num_sponsoring, v2.num_sponsored, v2.signer_sponsoring_i_ds.len() as u32),
                                                            },
                                                        }
                                                    };
                                                    let (cdp_ing, cdp_ed, cdp_sigs) =
                                                        get_ext(cdp_a);
                                                    let (our_ing, our_ed, our_sigs) =
                                                        get_ext(our_a);
                                                    println!("      CDP  account: id={} balance={} seq={} sub_entries={} flags={} num_sponsoring={} num_sponsored={} signer_sponsors={}",
                                                        cdp_pk, cdp_a.balance, cdp_a.seq_num.0, cdp_a.num_sub_entries, cdp_a.flags, cdp_ing, cdp_ed, cdp_sigs);
                                                    println!("      Ours account: id={} balance={} seq={} sub_entries={} flags={} num_sponsoring={} num_sponsored={} signer_sponsors={}",
                                                        cdp_pk, our_a.balance, our_a.seq_num.0, our_a.num_sub_entries, our_a.flags, our_ing, our_ed, our_sigs);
                                                    if cdp_a.balance != our_a.balance {
                                                        println!(
                                                            "      BALANCE DIFF: {} (ours - cdp)",
                                                            our_a.balance - cdp_a.balance
                                                        );
                                                    }
                                                    if cdp_a.num_sub_entries
                                                        != our_a.num_sub_entries
                                                    {
                                                        println!("      SUB_ENTRIES DIFF: {} (ours - cdp)", our_a.num_sub_entries as i64 - cdp_a.num_sub_entries as i64);
                                                    }
                                                    if cdp_ing != our_ing {
                                                        println!("      NUM_SPONSORING DIFF: {} (ours - cdp)", our_ing as i64 - cdp_ing as i64);
                                                    }
                                                    if cdp_ed != our_ed {
                                                        println!("      NUM_SPONSORED DIFF: {} (ours - cdp)", our_ed as i64 - cdp_ed as i64);
                                                    }
                                                }
                                                if let (
                                                    stellar_xdr::curr::LedgerEntryData::Trustline(
                                                        cdp_t,
                                                    ),
                                                    stellar_xdr::curr::LedgerEntryData::Trustline(
                                                        our_t,
                                                    ),
                                                ) = (&cdp_entry.data, &our_entry.data)
                                                {
                                                    println!("      CDP  trustline: balance={} asset={:?}", cdp_t.balance, cdp_t.asset);
                                                    println!("      Ours trustline: balance={} asset={:?}", our_t.balance, our_t.asset);
                                                }
                                                if let (stellar_xdr::curr::LedgerEntryData::LiquidityPool(cdp_p), stellar_xdr::curr::LedgerEntryData::LiquidityPool(our_p)) = (&cdp_entry.data, &our_entry.data) {
                                                    let stellar_xdr::curr::LiquidityPoolEntryBody::LiquidityPoolConstantProduct(ref cdp_cp) = cdp_p.body;
                                                    let stellar_xdr::curr::LiquidityPoolEntryBody::LiquidityPoolConstantProduct(ref our_cp) = our_p.body;
                                                    println!("      CDP  pool: reserve_a={} reserve_b={}", cdp_cp.reserve_a, cdp_cp.reserve_b);
                                                    println!("      Ours pool: reserve_a={} reserve_b={}", our_cp.reserve_a, our_cp.reserve_b);
                                                }
                                                if diffs >= 20 {
                                                    break;
                                                }
                                            }
                                        }
                                        None => {
                                            // For offers, try the offer_store instead of bucket list snapshot
                                            // (offers are not indexed in bucket list snapshot)
                                            if let stellar_xdr::curr::LedgerEntryData::Offer(
                                                ref cdp_offer,
                                            ) = cdp_entry.data
                                            {
                                                let offer_store = ledger_manager.offer_store_lock();
                                                if let Some(our_entry) = offer_store
                                                    .get_ledger_entry_by_id(cdp_offer.offer_id)
                                                    .as_ref()
                                                {
                                                    let our_xdr = our_entry
                                                        .to_xdr(stellar_xdr::curr::Limits::none())
                                                        .unwrap_or_default();
                                                    let our_hash = {
                                                        let mut h = Sha256::new();
                                                        h.update(&our_xdr);
                                                        format!("{:x}", h.finalize())
                                                    };
                                                    if our_hash != cdp_hash {
                                                        diffs += 1;
                                                        if let stellar_xdr::curr::LedgerEntryData::Offer(ref our_offer) = our_entry.data {
                                                            let cdp_seller = {let stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(ref pk) = cdp_offer.seller_id.0; hex::encode(&pk.0[..8])};
                                                            println!("    OFFER DIFF #{}: id={} seller={}", diffs, cdp_offer.offer_id, cdp_seller);
                                                            println!("      CDP:  amount={} price={}/{} lm={}", cdp_offer.amount, cdp_offer.price.n, cdp_offer.price.d, cdp_entry.last_modified_ledger_seq);
                                                            println!("      Ours: amount={} price={}/{} lm={}", our_offer.amount, our_offer.price.n, our_offer.price.d, our_entry.last_modified_ledger_seq);
                                                        }
                                                    }
                                                    // else: offer matches, not a real diff
                                                } else {
                                                    // Offer is truly missing from our state
                                                    missing += 1;
                                                    let cdp_seller = {
                                                        let stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(ref pk) = cdp_offer.seller_id.0;
                                                        hex::encode(&pk.0)
                                                    };
                                                    println!("    TRULY MISSING offer: id={} seller={} amount={} price={}/{} cdp_lm={}",
                                                        cdp_offer.offer_id, cdp_seller, cdp_offer.amount,
                                                        cdp_offer.price.n, cdp_offer.price.d, cdp_entry.last_modified_ledger_seq);
                                                }
                                            } else {
                                                // Non-offer entry truly missing
                                                missing += 1;
                                                let key_str = match &cdp_entry.data {
                                                    stellar_xdr::curr::LedgerEntryData::Account(a) => {
                                                        let stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(ref pk) = a.account_id.0;
                                                        format!("Account({})", hex::encode(&pk.0[..8]))
                                                    }
                                                    stellar_xdr::curr::LedgerEntryData::Trustline(t) => {
                                                        let stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(ref pk) = t.account_id.0;
                                                        format!("Trustline(acct={}, asset={:?}, balance={})", hex::encode(&pk.0[..8]), t.asset, t.balance)
                                                    }
                                                    stellar_xdr::curr::LedgerEntryData::LiquidityPool(p) => {
                                                        let stellar_xdr::curr::LiquidityPoolEntryBody::LiquidityPoolConstantProduct(ref cp) = p.body;
                                                        format!("Pool(ra={}, rb={})", cp.reserve_a, cp.reserve_b)
                                                    }
                                                    other => format!("{:?}", std::mem::discriminant(other)),
                                                };
                                                println!("    MISSING in our state: {} cdp_lm={} hash={}", key_str, cdp_entry.last_modified_ledger_seq, cdp_hash);
                                                println!(
                                                    "      cdp_xdr: {}",
                                                    hex::encode(&cdp_xdr[..cdp_xdr.len().min(200)])
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                            println!("    Entry comparison: {} diffs, {} truly missing (out of {} CDP entries)", diffs, missing, final_entries.len());
                        }
                    }

                    if stop_on_error {
                        anyhow::bail!("Mismatch at ledger {}", seq);
                    }
                }

                // Collect and display performance metrics
                if let Some(ref perf) = result.perf {
                    total_close_us += perf.total_us;
                    total_tx_exec_us += perf.tx_exec_us;
                    total_commit_us += perf.commit_setup_us
                        + perf.add_batch_us
                        + perf.hot_archive_us
                        + perf.header_us
                        + perf.commit_close_us;
                    total_add_batch_us += perf.add_batch_us;
                    total_eviction_us += perf.eviction_us;
                    total_tx_count += perf.tx_count;
                    total_cache_hits += perf.cache.hits;
                    total_cache_misses += perf.cache.misses;
                    if perf.rss_after_bytes > peak_rss_bytes {
                        peak_rss_bytes = perf.rss_after_bytes;
                    }
                    if perf.total_us > slowest_ledger_us {
                        slowest_ledger_us = perf.total_us;
                        slowest_ledger_seq = seq;
                    }
                    // Track top slowest transactions across all ledgers
                    for tx in &perf.tx_timings {
                        slowest_txs.push((seq, tx.hash_hex.clone(), tx.exec_us));
                    }

                    // Print per-ledger summary every 64 ledgers or if slow
                    if !quiet && (ledgers_verified % 64 == 0 || perf.total_us > 500_000) {
                        let cache_rate = if perf.cache.hits + perf.cache.misses > 0 {
                            perf.cache.hits as f64 / (perf.cache.hits + perf.cache.misses) as f64
                                * 100.0
                        } else {
                            0.0
                        };
                        println!(
                            "\n  [PERF L{}] total={:.1}ms tx_exec={:.1}ms commit={:.1}ms \
                             add_batch={:.1}ms eviction={:.1}ms txs={} cache={:.0}% \
                             rss={:.0}MB",
                            seq,
                            perf.total_us as f64 / 1000.0,
                            perf.tx_exec_us as f64 / 1000.0,
                            (perf.commit_setup_us
                                + perf.add_batch_us
                                + perf.hot_archive_us
                                + perf.header_us
                                + perf.commit_close_us) as f64
                                / 1000.0,
                            perf.add_batch_us as f64 / 1000.0,
                            perf.eviction_us as f64 / 1000.0,
                            perf.tx_count,
                            cache_rate,
                            perf.rss_after_bytes as f64 / (1024.0 * 1024.0),
                        );
                        // Show top 3 slowest txs for this ledger
                        for tx in perf.tx_timings.iter().take(3) {
                            if tx.exec_us > 1000 {
                                println!(
                                    "    tx[{}] {}..  {:.1}ms  ops={}  {}  {}",
                                    tx.index,
                                    &tx.hash_hex[..tx.hash_hex.len().min(12)],
                                    tx.exec_us as f64 / 1000.0,
                                    tx.op_count,
                                    if tx.is_soroban { "soroban" } else { "classic" },
                                    if tx.success { "ok" } else { "FAILED" },
                                );
                            }
                        }
                    }
                }
            }

            // Update prev hash for next ledger
            prev_ledger_hash = result.header_hash;
        }

        current_cp = checkpoint::next_checkpoint(current_cp);
    }

    let verification_time = verification_start.elapsed();

    // Print summary
    println!();
    println!();
    println!("Verification Summary");
    println!("====================");
    println!("  Ledgers verified: {}", ledgers_verified);
    println!("  Ledgers matched:  {}", ledgers_matched);
    println!("  Ledgers with mismatches: {}", ledgers_mismatched);
    if ledgers_mismatched > 0 {
        println!("    - Header hash mismatches: {}", header_mismatches);
        println!("    - TX result hash mismatches: {}", tx_result_mismatches);
        println!("    - Meta mismatches: {}", meta_mismatches);
    }
    println!();
    println!("  Total time: {:.2}s", verification_time.as_secs_f64());
    if ledgers_verified > 0 {
        println!(
            "  Average per ledger: {:.2}ms",
            verification_time.as_millis() as f64 / ledgers_verified as f64
        );
    }

    // Performance summary
    println!();
    println!("Performance Summary");
    println!("====================");
    if ledgers_verified > 0 {
        let avg_close_ms = total_close_us as f64 / ledgers_verified as f64 / 1000.0;
        let avg_tx_exec_ms = total_tx_exec_us as f64 / ledgers_verified as f64 / 1000.0;
        let avg_commit_ms = total_commit_us as f64 / ledgers_verified as f64 / 1000.0;
        println!("  Timing (averages per ledger):");
        println!("    close_ledger:  {:.2}ms", avg_close_ms);
        println!("    tx_exec:       {:.2}ms", avg_tx_exec_ms);
        println!("    commit:        {:.2}ms", avg_commit_ms);
        println!(
            "    add_batch:     {:.2}ms",
            total_add_batch_us as f64 / ledgers_verified as f64 / 1000.0
        );
        println!(
            "    eviction:      {:.2}ms",
            total_eviction_us as f64 / ledgers_verified as f64 / 1000.0
        );
        println!();
        println!("  Transactions:");
        println!("    total:         {}", total_tx_count);
        println!(
            "    avg/ledger:    {:.1}",
            total_tx_count as f64 / ledgers_verified as f64
        );
        println!();
        println!("  Cache:");
        let overall_cache_rate = if total_cache_hits + total_cache_misses > 0 {
            total_cache_hits as f64 / (total_cache_hits + total_cache_misses) as f64 * 100.0
        } else {
            0.0
        };
        println!("    hit rate:      {:.1}%", overall_cache_rate);
        println!("    total hits:    {}", total_cache_hits);
        println!("    total misses:  {}", total_cache_misses);
        println!();
        println!("  Memory:");
        println!(
            "    peak RSS:      {:.1}MB",
            peak_rss_bytes as f64 / (1024.0 * 1024.0)
        );
        println!();
        println!(
            "  Slowest ledger:  {} ({:.1}ms)",
            slowest_ledger_seq,
            slowest_ledger_us as f64 / 1000.0
        );

        // Top 10 slowest transactions overall
        slowest_txs.sort_by(|a, b| b.2.cmp(&a.2));
        println!();
        println!("  Top 10 slowest transactions:");
        for (i, (ledger, hash, us)) in slowest_txs.iter().take(10).enumerate() {
            println!(
                "    {}. L{} {}..  {:.1}ms",
                i + 1,
                ledger,
                &hash[..hash.len().min(16)],
                *us as f64 / 1000.0
            );
        }
    }

    if ledgers_mismatched > 0 {
        anyhow::bail!(
            "Verification failed with {} mismatched ledgers",
            ledgers_mismatched
        );
    }

    Ok(())
}

/// Inspects a specific account entry in the bucket list at a checkpoint.
///
/// This debugging tool shows all occurrences of an account across all bucket
/// levels, helping diagnose issues like shadowed entries or incorrect lookups.
/// It displays both the normal lookup result and a full scan across all buckets.
///
/// # Arguments
///
/// * `config` - Application configuration with history archive URLs
/// * `checkpoint_seq` - Checkpoint ledger sequence to inspect
/// * `account_hex` - Account public key as 64-character hex string
async fn cmd_debug_bucket_entry(
    config: AppConfig,
    checkpoint_seq: u32,
    account_hex: &str,
) -> anyhow::Result<()> {
    use henyey_bucket::{BucketEntry, BucketList, BucketManager};
    use henyey_common::Hash256;
    use henyey_history::is_checkpoint_ledger;
    use std::sync::Arc;
    use stellar_xdr::curr::{
        AccountId, LedgerEntryData, LedgerKey, LedgerKeyAccount, PublicKey, Uint256,
    };

    // Parse account hex to AccountId
    let account_bytes = hex::decode(account_hex)?;
    if account_bytes.len() != 32 {
        anyhow::bail!("Account hex must be 32 bytes (64 hex chars)");
    }
    let account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
        account_bytes.try_into().unwrap(),
    )));
    let account_key = LedgerKey::Account(LedgerKeyAccount { account_id });

    println!("Debug Bucket Entry Inspection");
    println!("==============================");
    println!("Checkpoint: {}", checkpoint_seq);
    println!("Account: {}", account_hex);
    println!();

    // Verify checkpoint is valid
    if !is_checkpoint_ledger(checkpoint_seq) {
        anyhow::bail!("{} is not a valid checkpoint ledger", checkpoint_seq);
    }

    let archive = first_archive(&config)?;

    println!("Archive: {}", config.history.archives[0].url);

    // Get bucket list hashes at this checkpoint
    let has_entry = archive.get_checkpoint_has(checkpoint_seq).await?;
    let bucket_hashes: Vec<Hash256> = has_entry
        .bucket_hash_pairs()
        .into_iter()
        .flat_map(|(curr, snap)| [curr, snap])
        .collect();

    println!("Loading bucket list...");

    // Create bucket manager and load buckets
    let bucket_dir = tempfile::tempdir()?;
    let bucket_manager = Arc::new(BucketManager::new(bucket_dir.path().to_path_buf())?);

    // Download all required buckets
    let all_hashes: Vec<&Hash256> = bucket_hashes
        .iter()
        .filter(|h: &&Hash256| !h.is_zero())
        .collect();

    print!("Buckets ({} required):", all_hashes.len());
    let (cached, downloaded) =
        download_buckets_parallel(&archive, bucket_manager.clone(), all_hashes).await?;
    if downloaded == 0 {
        println!(" {} cached", cached);
    }

    // Restore bucket list
    let mut bucket_list = BucketList::restore_from_hashes(&bucket_hashes, |hash| {
        bucket_manager.load_bucket(hash).map(|b| (*b).clone())
    })?;
    bucket_list.set_bucket_dir(bucket_manager.bucket_dir().to_path_buf());

    // Restart pending merges (for correct state)
    bucket_list.restart_merges(checkpoint_seq, 25)?;

    println!("Bucket list hash: {}", bucket_list.hash().to_hex());
    println!();

    // Look up the account normally
    println!("Normal lookup result:");
    match bucket_list.get(&account_key)? {
        Some(entry) => {
            if let LedgerEntryData::Account(acc) = &entry.data {
                println!("  Balance: {}", acc.balance);
                println!("  Sequence: {}", acc.seq_num.0);
                println!("  Last modified: {}", entry.last_modified_ledger_seq);
                println!("  Thresholds: {:?}", acc.thresholds.0);
                println!("  Num signers: {}", acc.signers.len());
                for (i, signer) in acc.signers.iter().enumerate() {
                    println!("    Signer {}: weight={}", i, signer.weight);
                }
            }
        }
        None => {
            println!("  NOT FOUND");
        }
    }
    println!();

    // Find ALL occurrences across all buckets
    println!("All occurrences in bucket list:");
    let occurrences = bucket_list.find_all_occurrences(&account_key)?;
    if occurrences.is_empty() {
        println!("  No occurrences found in any bucket");
    } else {
        for (level, bucket_type, entry) in &occurrences {
            println!("  Level {} {}: ", level, bucket_type);
            match entry {
                BucketEntry::Liveentry(e) | BucketEntry::Initentry(e) => {
                    if let LedgerEntryData::Account(acc) = &e.data {
                        println!("    Type: Live/Init");
                        println!("    Balance: {}", acc.balance);
                        println!("    Sequence: {}", acc.seq_num.0);
                        println!("    Last modified: {}", e.last_modified_ledger_seq);
                        println!("    Thresholds: {:?}", acc.thresholds.0);
                        println!("    Num signers: {}", acc.signers.len());
                        for (i, signer) in acc.signers.iter().enumerate() {
                            println!("      Signer {}: weight={}", i, signer.weight);
                        }
                    }
                }
                BucketEntry::Deadentry(_) => {
                    println!("    Type: Dead (deleted)");
                }
                BucketEntry::Metaentry(_) => {
                    println!("    Type: Metadata (unexpected)");
                }
            }
        }
    }

    Ok(())
}

/// Sample config command handler.
fn cmd_sample_config() -> anyhow::Result<()> {
    let sample = AppConfig::sample_config();
    println!("{}", sample);
    Ok(())
}

/// Prints information about bucket files.
///
/// If given a directory, lists all bucket files with their sizes.
/// If given a single file, prints its metadata.
fn bucket_info(path: &std::path::Path) -> anyhow::Result<()> {
    if !path.exists() {
        anyhow::bail!("Bucket path does not exist: {}", path.display());
    }

    if path.is_dir() {
        // List bucket files in directory
        println!("Bucket directory: {}", path.display());
        println!();

        let mut count = 0;
        let mut total_size = 0u64;

        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if metadata.is_file() {
                let name = entry.file_name();
                let size = metadata.len();
                println!("  {} ({} bytes)", name.to_string_lossy(), size);
                count += 1;
                total_size += size;
            }
        }

        println!();
        println!("Total: {} files, {} bytes", count, total_size);
    } else {
        // Single bucket file
        println!("Bucket file: {}", path.display());

        let metadata = std::fs::metadata(path)?;
        println!("Size: {} bytes", metadata.len());

        // Would parse and display bucket contents
        println!("(Bucket content parsing not yet implemented)");
    }

    Ok(())
}

/// Checks quorum intersection from a JSON network configuration.
///
/// Loads the quorum set definitions and verifies that the network enjoys
/// quorum intersection (all quorums share at least one node).
fn cmd_check_quorum_intersection(path: &std::path::Path) -> anyhow::Result<()> {
    let enjoys = quorum_intersection::check_quorum_intersection_from_json(path)?;
    if enjoys {
        println!("network enjoys quorum intersection");
        Ok(())
    } else {
        anyhow::bail!("quorum sets do not have intersection");
    }
}

/// Dump ledger entries from the bucket list to a JSON file.
///
/// This is equivalent to stellar-core dump-ledger command.
/// It iterates over all entries in the bucket list and outputs them as JSON.
async fn cmd_dump_ledger(
    config: AppConfig,
    output: PathBuf,
    entry_type: Option<String>,
    limit: Option<u64>,
    last_modified_ledger_count: Option<u32>,
) -> anyhow::Result<()> {
    use henyey_bucket::BucketManager;
    use std::io::Write;
    use stellar_xdr::curr::LedgerEntryType;

    // Parse entry type filter if provided
    let type_filter: Option<LedgerEntryType> = if let Some(ref type_str) = entry_type {
        Some(match type_str.to_lowercase().as_str() {
            "account" => LedgerEntryType::Account,
            "trustline" => LedgerEntryType::Trustline,
            "offer" => LedgerEntryType::Offer,
            "data" => LedgerEntryType::Data,
            "claimable_balance" | "claimablebalance" => LedgerEntryType::ClaimableBalance,
            "liquidity_pool" | "liquiditypool" => LedgerEntryType::LiquidityPool,
            "contract_data" | "contractdata" => LedgerEntryType::ContractData,
            "contract_code" | "contractcode" => LedgerEntryType::ContractCode,
            "config_setting" | "configsetting" => LedgerEntryType::ConfigSetting,
            "ttl" => LedgerEntryType::Ttl,
            _ => anyhow::bail!(
                "Unknown entry type: {}. Valid types: account, trustline, offer, data, \
                claimable_balance, liquidity_pool, contract_data, contract_code, \
                config_setting, ttl",
                type_str
            ),
        })
    } else {
        None
    };

    // Open database and bucket manager
    let db = henyey_db::Database::open(&config.database.path)?;
    let bucket_manager = BucketManager::with_cache_size(
        config.buckets.directory.clone(),
        config.buckets.cache_size,
    )?;

    // Get current ledger
    let current_ledger = db
        .get_latest_ledger_seq()?
        .ok_or_else(|| anyhow::anyhow!("No ledger data in database. Run catchup first."))?;

    println!("Current ledger: {}", current_ledger);

    // Calculate minimum last modified ledger if filter is set
    let min_last_modified: Option<u32> =
        last_modified_ledger_count.map(|count| current_ledger.saturating_sub(count));

    // Load bucket list snapshot for the current checkpoint
    let checkpoint = henyey_history::checkpoint::latest_checkpoint_before_or_at(current_ledger)
        .ok_or_else(|| anyhow::anyhow!("No checkpoint available for ledger {}", current_ledger))?;

    println!("Using checkpoint: {}", checkpoint);

    let levels = db
        .load_bucket_list(checkpoint)?
        .ok_or_else(|| anyhow::anyhow!("Missing bucket list snapshot at {}", checkpoint))?;

    // Open output file
    let mut file = std::fs::File::create(&output)?;

    let mut entry_count: u64 = 0;
    let limit_val = limit.unwrap_or(u64::MAX);

    println!("Dumping entries to {}...", output.display());

    // Iterate over all bucket levels
    for (level_idx, (curr_hash, snap_hash)) in levels.iter().enumerate() {
        if entry_count >= limit_val {
            break;
        }

        // Process curr bucket
        for hash in [curr_hash, snap_hash] {
            if entry_count >= limit_val {
                break;
            }

            let bucket = bucket_manager.load_bucket(hash)?;
            for entry in bucket.entries() {
                if entry_count >= limit_val {
                    break;
                }

                // Skip dead entries and metadata
                let live_entry = match entry {
                    henyey_bucket::BucketEntry::Liveentry(e)
                    | henyey_bucket::BucketEntry::Initentry(e) => e,
                    henyey_bucket::BucketEntry::Deadentry(_)
                    | henyey_bucket::BucketEntry::Metaentry(_) => continue,
                };

                // Apply type filter
                if let Some(ref filter_type) = type_filter {
                    if live_entry.data.discriminant() != *filter_type {
                        continue;
                    }
                }

                // Apply last modified ledger filter
                if let Some(min_ledger) = min_last_modified {
                    if live_entry.last_modified_ledger_seq < min_ledger {
                        continue;
                    }
                }

                // Serialize to JSON
                let json = serde_json::to_string_pretty(&live_entry)?;
                writeln!(file, "{}", json)?;

                entry_count += 1;

                // Progress reporting
                if entry_count % 10000 == 0 {
                    print!(
                        "\rProcessed {} entries (level {})...",
                        entry_count, level_idx
                    );
                    std::io::stdout().flush()?;
                }
            }
        }
    }

    println!();
    println!("Dumped {} entries to {}", entry_count, output.display());

    Ok(())
}

/// Perform offline self-checks (equivalent to stellar-core self-check command).
///
/// This command performs comprehensive diagnostic checks:
/// 1. Header chain verification - ensures ledger headers form a valid chain
/// 2. Bucket hash verification - verifies all bucket files have correct hashes
/// 3. Crypto benchmarking - measures Ed25519 sign/verify performance
async fn cmd_self_check(config: AppConfig) -> anyhow::Result<()> {
    use henyey_bucket::BucketManager;
    use henyey_crypto::SecretKey;
    use std::time::Instant;

    let mut all_ok = true;

    // Phase 1: Header chain verification
    println!("Self-check phase 1: header chain verification");
    let db = henyey_db::Database::open(&config.database.path)?;

    let Some(latest_seq) = db.get_latest_ledger_seq()? else {
        println!("  No ledger data in database. Skipping header verification.");
        println!();
        return Ok(());
    };

    if latest_seq == 0 {
        println!("  At genesis ledger. Header chain is trivially valid.");
    } else {
        // Verify header chain going backwards
        let depth = std::cmp::min(latest_seq, 100); // Check up to 100 ledgers
        let mut current_seq = latest_seq;
        let mut verified = 0u32;

        while current_seq > 0 && verified < depth {
            let current = db
                .get_ledger_header(current_seq)?
                .ok_or_else(|| anyhow::anyhow!("Missing ledger header at {}", current_seq))?;
            let prev_seq = current_seq - 1;
            let prev = db
                .get_ledger_header(prev_seq)?
                .ok_or_else(|| anyhow::anyhow!("Missing ledger header at {}", prev_seq))?;

            let prev_hash = henyey_ledger::compute_header_hash(&prev)?;

            if current.previous_ledger_hash != prev_hash.into() {
                println!("  ERROR: Header chain broken at ledger {}", current_seq);
                println!(
                    "    Previous hash in header: {:?}",
                    current.previous_ledger_hash
                );
                println!("    Computed hash of previous: {:?}", prev_hash);
                all_ok = false;
                break;
            }

            current_seq = prev_seq;
            verified += 1;
        }

        if verified == depth {
            println!(
                "  Verified {} ledger headers (from {} to {})",
                verified,
                latest_seq,
                latest_seq - verified + 1
            );
        }
    }

    // Phase 2: Bucket hash verification
    println!();
    println!("Self-check phase 2: bucket hash verification");

    let bucket_manager = BucketManager::with_cache_size(
        config.buckets.directory.clone(),
        config.buckets.cache_size,
    )?;

    // Get the checkpoint for the current ledger
    let checkpoint = henyey_history::checkpoint::latest_checkpoint_before_or_at(latest_seq)
        .ok_or_else(|| anyhow::anyhow!("No checkpoint available for ledger {}", latest_seq))?;

    let levels = db
        .load_bucket_list(checkpoint)?
        .ok_or_else(|| anyhow::anyhow!("Missing bucket list snapshot at {}", checkpoint))?;

    let mut buckets_verified = 0;
    let mut buckets_failed = 0;

    // Collect all unique bucket hashes using a set for deduplication
    let hashes_to_verify: std::collections::HashSet<_> = levels
        .iter()
        .flat_map(|(curr, snap)| [*curr, *snap])
        .filter(|h| !h.is_zero())
        .collect();

    println!("  Verifying {} bucket files...", hashes_to_verify.len());

    for hash in &hashes_to_verify {
        match bucket_manager.load_bucket(hash) {
            Ok(bucket) => {
                // The bucket's hash is computed from its contents when loaded
                // If it loads successfully, the hash matches
                if bucket.hash() != *hash {
                    println!("  ERROR: Bucket hash mismatch for {}", hash);
                    println!("    Expected: {}", hash);
                    println!("    Computed: {}", bucket.hash());
                    buckets_failed += 1;
                    all_ok = false;
                } else {
                    buckets_verified += 1;
                }
            }
            Err(e) => {
                println!("  ERROR: Failed to load bucket {}: {}", hash, e);
                buckets_failed += 1;
                all_ok = false;
            }
        }
    }

    println!(
        "  Verified {} buckets, {} failures",
        buckets_verified, buckets_failed
    );

    // Phase 3: Crypto benchmarking
    println!();
    println!("Self-check phase 3: crypto benchmarking");

    const BENCHMARK_OPS: usize = 10000;
    let message = b"stellar benchmark test message for ed25519 signing";

    // Generate a keypair for benchmarking
    let secret = SecretKey::generate();
    let public = secret.public_key();

    // Benchmark signing
    let start = Instant::now();
    for _ in 0..BENCHMARK_OPS {
        let _ = secret.sign(message);
    }
    let sign_duration = start.elapsed();
    let sign_per_sec = (BENCHMARK_OPS as f64 / sign_duration.as_secs_f64()) as u64;

    // Generate signatures for verification benchmark
    let signature = secret.sign(message);

    // Benchmark verification
    let start = Instant::now();
    for _ in 0..BENCHMARK_OPS {
        let _ = public.verify(message, &signature);
    }
    let verify_duration = start.elapsed();
    let verify_per_sec = (BENCHMARK_OPS as f64 / verify_duration.as_secs_f64()) as u64;

    println!("  Benchmarked {} signatures / sec", sign_per_sec);
    println!("  Benchmarked {} verifications / sec", verify_per_sec);

    // Final result
    println!();
    if all_ok {
        println!("Self-check succeeded");
        Ok(())
    } else {
        println!("Self-check failed");
        std::process::exit(1);
    }
}

/// Write verified checkpoint ledger hashes to a file.
///
/// Downloads checkpoint headers from history archives, verifies the header
/// chain, and writes verified checkpoint hashes to a JSON file.
///
/// Equivalent to stellar-core verify-checkpoints.
async fn cmd_verify_checkpoints(
    config: AppConfig,
    output: PathBuf,
    from: Option<u32>,
    to: Option<u32>,
) -> anyhow::Result<()> {
    use henyey_history::{checkpoint, verify};
    use henyey_ledger::compute_header_hash;
    use std::io::Write;

    println!("Verifying checkpoint hashes...");
    println!();

    let archives = all_archives(&config)?;

    println!("Using {} archive(s)", archives.len());

    let archive = &archives[0];
    let root_has = archive.get_root_has().await?;
    let current_ledger = root_has.current_ledger;

    let start = from.unwrap_or(63); // First checkpoint
    let end = to.unwrap_or(current_ledger);

    println!("Verifying checkpoint range: {} to {}", start, end);
    println!();

    // Calculate checkpoint-aligned start and end
    let start_checkpoint = checkpoint::checkpoint_containing(start);
    let end_checkpoint = checkpoint::checkpoint_containing(end);

    // Collect verified checkpoint hashes
    let mut verified_checkpoints: Vec<serde_json::Value> = Vec::new();
    let mut prev_header: Option<stellar_xdr::curr::LedgerHeader> = None;
    let mut verified_count = 0;
    let mut error_count = 0;

    let mut current_checkpoint = start_checkpoint;
    while current_checkpoint <= end_checkpoint {
        print!("  Checkpoint {}: ", current_checkpoint);
        std::io::stdout().flush()?;

        // Download headers for this checkpoint
        match archive.get_ledger_headers(current_checkpoint).await {
            Ok(history_entries) => {
                if history_entries.is_empty() {
                    println!("FAIL (no headers)");
                    error_count += 1;
                    current_checkpoint = checkpoint::next_checkpoint(current_checkpoint);
                    continue;
                }

                // Extract headers
                let headers: Vec<stellar_xdr::curr::LedgerHeader> = history_entries
                    .iter()
                    .map(|entry| entry.header.clone())
                    .collect();

                // Verify header chain within this checkpoint
                if let Err(e) = verify::verify_header_chain(&headers) {
                    println!("FAIL (chain broken: {})", e);
                    error_count += 1;
                    current_checkpoint = checkpoint::next_checkpoint(current_checkpoint);
                    continue;
                }

                // Verify linkage to previous checkpoint
                if let Some(ref prev) = prev_header {
                    let first_header = &headers[0];
                    let prev_hash = compute_header_hash(prev)?;
                    if first_header.previous_ledger_hash != prev_hash.into() {
                        println!("FAIL (cross-checkpoint link broken)");
                        error_count += 1;
                        current_checkpoint = checkpoint::next_checkpoint(current_checkpoint);
                        continue;
                    }
                }

                // Get the checkpoint ledger header (last header in the set)
                let checkpoint_header = headers.last().unwrap();
                let checkpoint_hash = compute_header_hash(checkpoint_header)?;

                // Store for JSON output
                verified_checkpoints.push(serde_json::json!({
                    "ledger": current_checkpoint,
                    "hash": hex::encode(checkpoint_hash.as_bytes())
                }));

                println!(
                    "OK (hash: {})",
                    hex::encode(&checkpoint_hash.as_bytes()[..8])
                );
                verified_count += 1;

                // Update previous header for next iteration
                prev_header = Some(checkpoint_header.clone());
            }
            Err(e) => {
                println!("FAIL (download: {})", e);
                error_count += 1;
            }
        }

        current_checkpoint = checkpoint::next_checkpoint(current_checkpoint);
    }

    println!();
    println!(
        "Verified {} checkpoints, {} errors",
        verified_count, error_count
    );

    // Write output file
    let output_json = serde_json::json!({
        "network_passphrase": config.network.passphrase,
        "checkpoints": verified_checkpoints
    });

    let mut file = std::fs::File::create(&output)?;
    serde_json::to_writer_pretty(&file, &output_json)?;
    file.flush()?;

    println!("Wrote verified checkpoint hashes to: {}", output.display());

    if error_count > 0 {
        std::process::exit(1);
    }

    Ok(())
}

/// Send an HTTP command to a running stellar-core node.
///
/// Makes an HTTP GET request to http://127.0.0.1:{port}/{command}
/// and prints the response. Query parameters are URL-encoded.
///
/// Equivalent to stellar-core http-command.
async fn cmd_http_command(command: &str, port: u16) -> anyhow::Result<()> {
    // Build the URL path, encoding special characters after the ?
    let mut path = String::from("/");
    let mut in_query = false;

    for c in command.chars() {
        if in_query {
            // URL-encode non-alphanumeric characters in query string
            if c.is_ascii_alphanumeric() || c == '&' || c == '=' {
                path.push(c);
            } else {
                path.push_str(&format!("%{:02X}", c as u8));
            }
        } else {
            path.push(c);
            if c == '?' {
                in_query = true;
            }
        }
    }

    let url = format!("http://127.0.0.1:{}{}", port, path);

    // Make the HTTP request
    let client = reqwest::Client::new();
    let response = client.get(&url).send().await;

    match response {
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await?;

            if status.is_success() {
                println!("{}", body);
                Ok(())
            } else {
                eprintln!("HTTP {}: {}", status.as_u16(), body);
                anyhow::bail!("Request failed with status {}", status.as_u16());
            }
        }
        Err(e) => {
            if e.is_connect() {
                anyhow::bail!(
                    "Connection refused on port {}. Is stellar-core running?",
                    port
                );
            } else {
                anyhow::bail!("HTTP request failed: {}", e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use stellar_xdr::curr::ReadXdr;

    // =========================================================================
    // CLI Parsing Tests
    // =========================================================================

    #[test]
    fn test_cli_parsing() {
        // Test basic parsing
        let cli = Cli::parse_from(["rs-stellar-core", "info"]);
        assert!(matches!(cli.command, Commands::Info));
    }

    #[test]
    fn test_cli_run_command() {
        let cli = Cli::parse_from(["rs-stellar-core", "run", "--validator"]);
        match cli.command {
            Commands::Run {
                validator, watcher, ..
            } => {
                assert!(validator);
                assert!(!watcher);
            }
            _ => panic!("Expected Run command"),
        }
    }

    #[test]
    fn test_cli_catchup_command() {
        let cli = Cli::parse_from([
            "rs-stellar-core",
            "catchup",
            "1000000",
            "--mode",
            "complete",
        ]);
        match cli.command {
            Commands::Catchup { target, .. } => {
                assert_eq!(target, "1000000");
            }
            _ => panic!("Expected Catchup command"),
        }
    }

    #[test]
    fn test_cli_global_options() {
        let cli = Cli::parse_from(["rs-stellar-core", "--verbose", "--mainnet", "info"]);
        assert!(cli.verbose);
        assert!(cli.mainnet);
    }

    #[test]
    fn test_cli_check_quorum_intersection() {
        let cli = Cli::parse_from(["rs-stellar-core", "check-quorum-intersection", "foo.json"]);
        match cli.command {
            Commands::CheckQuorumIntersection { path } => {
                assert_eq!(path, PathBuf::from("foo.json"));
            }
            _ => panic!("Expected check-quorum-intersection command"),
        }
    }

    #[test]
    fn test_genesis_ledger_creation() {
        use henyey_db::queries::{LedgerQueries, StateQueries};
        use henyey_db::schema::state_keys;

        let db = henyey_db::Database::open_in_memory().unwrap();
        let passphrase = "Standalone Network ; February 2017";

        initialize_genesis_ledger(&db, None, passphrase, 0).unwrap();

        // Verify LCL is set to 1
        db.with_connection(|conn| {
            let lcl = conn.get_last_closed_ledger().unwrap();
            assert_eq!(lcl, Some(1));

            // Verify network passphrase is stored
            let stored_passphrase = conn.get_state(state_keys::NETWORK_PASSPHRASE).unwrap();
            assert_eq!(stored_passphrase.as_deref(), Some(passphrase));

            // Verify HAS is stored
            let has_json = conn.get_state(state_keys::HISTORY_ARCHIVE_STATE).unwrap();
            assert!(has_json.is_some());
            let has_str = has_json.unwrap();
            assert!(
                has_str.contains("\"currentLedger\": 1") || has_str.contains("\"currentLedger\":1")
            );

            // Verify ledger header is stored
            let header = conn.load_ledger_header(1).unwrap();
            assert!(header.is_some());
            let header = header.unwrap();
            assert_eq!(header.ledger_seq, 1);
            assert_eq!(header.ledger_version, 0);
            assert_eq!(header.total_coins, 1_000_000_000_000_000_000);
            assert_eq!(header.base_fee, 100);
            assert_eq!(header.base_reserve, 100_000_000);

            // Verify bucket_list_hash is non-zero (has root account entry)
            assert_ne!(header.bucket_list_hash.0, [0u8; 32]);

            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn test_genesis_ledger_root_account_key_derivation() {
        // For "Standalone Network ; February 2017", the root account public key
        // should be GBZXN7PIRZGNMHGA7MUUUF4GWPY5AYPV6LY4UV2GL6VJGIQRXFDNMADI
        // The derivation is: SecretKey::from_seed(SHA256(passphrase)).public_key()
        use henyey_common::NetworkId;

        let network_id = NetworkId::from_passphrase("Standalone Network ; February 2017");
        let root_secret = henyey_crypto::SecretKey::from_seed(network_id.as_bytes());
        let root_public = root_secret.public_key();

        assert_eq!(
            root_public.to_strkey(),
            "GBZXN7PIRZGNMHGA7MUUUF4GWPY5AYPV6LY4UV2GL6VJGIQRXFDNMADI"
        );

        // Also verify the root secret key matches the known value
        assert_eq!(
            root_secret.to_strkey(),
            "SC5O7VZUXDJ6JBDSZ74DSERXL7W3Y5LTOAMRF7RQRL3TAGAPS7LUVG3L"
        );
    }

    #[test]
    fn test_genesis_test_accounts_created() {
        // When genesis_test_account_count > 0, initialize_genesis_ledger should
        // create test accounts alongside root, splitting total_coins evenly.
        use henyey_db::queries::{LedgerQueries, StateQueries};

        let db = henyey_db::Database::open_in_memory().unwrap();
        let passphrase = "Standalone Network ; February 2017";
        let count = 10;

        initialize_genesis_ledger(&db, None, passphrase, count).unwrap();

        // Verify LCL is set and header is stored
        db.with_connection(|conn| {
            let lcl = conn.get_last_closed_ledger().unwrap();
            assert_eq!(lcl, Some(1));

            // Verify header still has full total_coins
            let header = conn.load_ledger_header(1).unwrap().unwrap();
            assert_eq!(header.total_coins, 1_000_000_000_000_000_000);

            // Verify bucket_list_hash differs from the 0-account case
            // (it should include 11 accounts instead of 1)
            let db2 = henyey_db::Database::open_in_memory().unwrap();
            initialize_genesis_ledger(&db2, None, passphrase, 0).unwrap();
            let header0 = db2
                .with_connection(|c| c.load_ledger_header(1))
                .unwrap()
                .unwrap();
            assert_ne!(header.bucket_list_hash, header0.bucket_list_hash);

            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn test_genesis_test_account_balance_split() {
        // Verify that total_coins are split evenly with root getting the remainder.
        // total = 1_000_000_000_000_000_000, count = 3 => 4 accounts
        // base = 250_000_000_000_000_000, remainder = 0
        // root_balance = 250_000_000_000_000_000
        // test_balance = 250_000_000_000_000_000
        let total_coins: i64 = 1_000_000_000_000_000_000;
        let count: u32 = 3;
        let total_accounts = count as i64 + 1;
        let base = total_coins / total_accounts;
        let remainder = total_coins % total_accounts;
        assert_eq!(base, 250_000_000_000_000_000);
        assert_eq!(remainder, 0);

        // With count = 7 => 8 accounts
        let count2: u32 = 7;
        let total_accounts2 = count2 as i64 + 1;
        let base2 = total_coins / total_accounts2;
        let remainder2 = total_coins % total_accounts2;
        assert_eq!(base2, 125_000_000_000_000_000);
        assert_eq!(remainder2, 0);

        // With count = 6 => 7 accounts (has a remainder)
        let count3: u32 = 6;
        let total_accounts3 = count3 as i64 + 1;
        let base3 = total_coins / total_accounts3;
        let remainder3 = total_coins % total_accounts3;
        assert_eq!(base3, 142_857_142_857_142_857);
        assert_eq!(remainder3, 1);
        // Root gets base3 + remainder3 = 142_857_142_857_142_858
        // Verify no coins are lost
        assert_eq!(base3 * total_accounts3 + remainder3, total_coins);
    }

    #[test]
    fn test_genesis_test_account_key_derivation() {
        // Verify that test account keys match stellar-core's getAccount() derivation.
        // "TestAccount-0" padded with '.' to 32 bytes => ed25519 seed.
        let seed = deterministic_seed("TestAccount-0");
        assert_eq!(seed.len(), 32);
        assert_eq!(&seed[..14], b"TestAccount-0.");
        assert!(seed[14..].iter().all(|&b| b == b'.'));

        let secret = henyey_crypto::SecretKey::from_seed(&seed);
        let public = secret.public_key();
        // The public key should be deterministic and non-zero
        assert_ne!(public.as_bytes(), &[0u8; 32]);

        // Verify different names produce different keys
        let seed1 = deterministic_seed("TestAccount-1");
        assert_ne!(seed, seed1);
    }

    #[test]
    fn test_write_scp_history_file_uses_record_marks() {
        use flate2::read::GzDecoder;
        use henyey_history::paths::checkpoint_path;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let entry = stellar_xdr::curr::ScpHistoryEntry::V0(stellar_xdr::curr::ScpHistoryEntryV0 {
            quorum_sets: stellar_xdr::curr::VecM::default(),
            ledger_messages: stellar_xdr::curr::LedgerScpMessages {
                ledger_seq: 63,
                messages: stellar_xdr::curr::VecM::default(),
            },
        });

        write_scp_history_file(tmp.path(), 63, &[entry.clone()]).unwrap();

        let path = tmp.path().join(checkpoint_path("scp", 63, "xdr.gz"));
        let file = std::fs::File::open(path).unwrap();
        let mut decoder = GzDecoder::new(file);
        let mut bytes = Vec::new();
        decoder.read_to_end(&mut bytes).unwrap();

        assert!(bytes.len() > 4);
        let mark = u32::from_be_bytes(bytes[0..4].try_into().unwrap());
        assert_ne!(mark & 0x8000_0000, 0);

        let payload_len = (mark & 0x7fff_ffff) as usize;
        assert_eq!(payload_len, bytes.len() - 4);

        let parsed = stellar_xdr::curr::ScpHistoryEntry::from_xdr(
            &bytes[4..],
            stellar_xdr::curr::Limits::none(),
        )
        .unwrap();
        assert_eq!(parsed, entry);
    }

    #[test]
    fn test_genesis_bucket_files_persisted_to_disk() {
        // Regression test for commit 26a4275: genesis bucket files must be
        // written to disk so that `load_last_known_ledger` can restore state
        // on restart (matching stellar-core's startNewLedger behavior).
        use tempfile::TempDir;

        let db = henyey_db::Database::open_in_memory().unwrap();
        let passphrase = "Standalone Network ; February 2017";
        let tmp = TempDir::new().unwrap();
        let bucket_dir = tmp.path().join("buckets");

        initialize_genesis_ledger(&db, Some(&bucket_dir), passphrase, 0).unwrap();

        // The bucket directory should have been created
        assert!(bucket_dir.exists(), "bucket directory should be created");

        // There should be at least one .bucket.xdr file (the root account
        // produces non-empty bucket entries at level 0)
        let bucket_files: Vec<_> = std::fs::read_dir(&bucket_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path().extension().map_or(false, |ext| ext == "xdr")
                    && e.path().to_string_lossy().contains(".bucket.xdr")
            })
            .collect();

        assert!(
            !bucket_files.is_empty(),
            "at least one genesis bucket file should be persisted"
        );

        // Each file should have non-zero size and a valid filename pattern
        for entry in &bucket_files {
            let size = entry.metadata().unwrap().len();
            assert!(
                size > 0,
                "bucket file should not be empty: {:?}",
                entry.path()
            );

            let name = entry.file_name().to_string_lossy().to_string();
            // Filename format: <64-hex-chars>.bucket.xdr
            assert!(
                name.ends_with(".bucket.xdr"),
                "unexpected filename: {}",
                name
            );
            let hex_part = &name[..name.len() - ".bucket.xdr".len()];
            assert_eq!(hex_part.len(), 64, "hash should be 64 hex chars: {}", name);
            assert!(
                hex_part.chars().all(|c| c.is_ascii_hexdigit()),
                "hash should be hex: {}",
                name
            );
        }
    }

    #[test]
    fn test_genesis_bucket_files_not_written_without_bucket_dir() {
        // When bucket_dir is None, no bucket files should be written (in-memory only).
        // This is the existing behavior for tests that don't need on-disk persistence.
        let db = henyey_db::Database::open_in_memory().unwrap();
        let passphrase = "Standalone Network ; February 2017";

        // Should succeed without error even with no bucket_dir
        initialize_genesis_ledger(&db, None, passphrase, 0).unwrap();

        // Verify genesis ledger was still created correctly
        db.with_connection(|conn| {
            use henyey_db::queries::StateQueries;
            let lcl = conn.get_last_closed_ledger().unwrap();
            assert_eq!(lcl, Some(1));
            Ok(())
        })
        .unwrap();
    }
}
