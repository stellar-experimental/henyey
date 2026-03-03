//! Run command implementation for rs-stellar-core.
//!
//! The run command starts the node and keeps it synchronized with the network.
//! This is the primary operational mode for a Stellar Core node.
//!
//! # Responsibilities
//!
//! The run command handles:
//! - **Catchup**: Automatically catching up from history if the node is behind
//! - **Peer Management**: Connecting to peers and handling disconnections
//! - **Consensus**: Tracking SCP consensus (or participating for validators)
//! - **Ledger Close**: Applying externalized ledgers to update state
//! - **HTTP API**: Serving status and control endpoints
//!
//! # Command Line Usage
//!
//! ```text
//! rs-stellar-core run                    # Run as a full node
//! rs-stellar-core run --validator        # Run as a validator
//! rs-stellar-core run --watcher          # Run as a watcher (no catchup)
//! ```
//!
//! # Running Modes
//!
//! | Mode | Description | Consensus Role |
//! |------|-------------|----------------|
//! | `Full` | Standard node with catchup | Tracks consensus |
//! | `Validator` | Active consensus participant | Votes in SCP |
//! | `Watcher` | Observe-only, no catchup | Passive observer |
//!
//! # HTTP Status Server
//!
//! When enabled, the [`StatusServer`](crate::http::StatusServer) provides REST
//! endpoints for monitoring and control. See the [`http`](crate::http) module
//! for full endpoint documentation.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::signal;
use tokio::sync::broadcast;

use crate::app::{App, AppState, CatchupTarget};
use crate::config::AppConfig;
use crate::http::{QueryServer, StatusServer};

/// Node running mode determining behavior and consensus participation.
///
/// The mode affects whether the node catches up from history, participates
/// in consensus, and how aggressively it maintains synchronization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunMode {
    /// Full node: catches up from history and tracks consensus.
    ///
    /// This is the default mode suitable for most use cases. The node
    /// maintains full ledger state and can serve queries.
    #[default]
    Full,
    /// Validator: actively participates in SCP consensus.
    ///
    /// Requires a valid `node_seed` and properly configured quorum set.
    /// The node will vote on ledger values and sign SCP messages.
    Validator,
    /// Watcher: observe-only mode without catchup.
    ///
    /// Useful for monitoring the network without maintaining full state.
    /// The node connects to peers but does not sync from history.
    Watcher,
}

impl std::fmt::Display for RunMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunMode::Full => write!(f, "full"),
            RunMode::Validator => write!(f, "validator"),
            RunMode::Watcher => write!(f, "watcher"),
        }
    }
}

/// Configuration options for the run command.
///
/// Controls the running mode, synchronization behavior, and catchup policy.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Running mode (full, validator, or watcher).
    pub mode: RunMode,
    /// Force catchup even if local state exists.
    pub force_catchup: bool,
    /// Block until the node is fully synced before returning from run.
    pub wait_for_sync: bool,
    /// Maximum ledger age (in ledgers) before triggering automatic catchup.
    ///
    /// If the local ledger is more than this many ledgers behind, catchup
    /// will be triggered. Default is 300 (~25 minutes at 5s close time).
    pub max_ledger_age: u32,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            mode: RunMode::Full,
            force_catchup: false,
            wait_for_sync: true,
            max_ledger_age: 300, // ~25 minutes of ledgers
        }
    }
}

impl RunOptions {
    /// Create options for running as a validator.
    pub fn validator() -> Self {
        Self {
            mode: RunMode::Validator,
            ..Default::default()
        }
    }

    /// Create options for running as a watcher.
    pub fn watcher() -> Self {
        Self {
            mode: RunMode::Watcher,
            force_catchup: false,
            wait_for_sync: false,
            ..Default::default()
        }
    }

    /// Set whether to force catchup.
    pub fn with_force_catchup(mut self, force: bool) -> Self {
        self.force_catchup = force;
        self
    }
}

/// Run the node with the given configuration and options.
pub async fn run_node(config: AppConfig, options: RunOptions) -> anyhow::Result<()> {
    tracing::info!(
        mode = %options.mode,
        node_name = %config.node.name,
        network = %config.network.passphrase,
        "Starting henyey node"
    );

    // Validate mode-specific requirements
    validate_run_options(&config, &options)?;

    // Store HTTP config before moving config
    let http_enabled = config.http.enabled;
    let http_port = config.http.port;
    let query_port = config.query.port;

    // Create the application
    let app = Arc::new(App::new(config).await?);

    // Set the weak self reference for spawning tasks from &self methods
    app.set_self_arc().await;

    // Set up shutdown handling
    let shutdown_app = app.clone();
    let shutdown_handle = tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        tracing::info!("Shutdown signal received");
        shutdown_app.shutdown();
    });

    // Start the HTTP status server if enabled
    let http_handle = if http_enabled {
        let status_server = StatusServer::new(http_port, app.clone());
        Some(tokio::spawn(async move {
            if let Err(e) = status_server.start().await {
                tracing::error!(error = %e, "HTTP status server error");
            }
        }))
    } else {
        None
    };

    // Start the HTTP query server if configured
    let query_handle = if let Some(port) = query_port {
        let query_server = QueryServer::new(port, app.clone());
        Some(tokio::spawn(async move {
            if let Err(e) = query_server.start().await {
                tracing::error!(error = %e, "HTTP query server error");
            }
        }))
    } else {
        None
    };

    // Print startup info
    print_startup_info(&app, &options);

    // Run the main loop
    let result = run_main_loop(app.clone(), options).await;

    // Clean up
    shutdown_handle.abort();
    if let Some(handle) = http_handle {
        handle.abort();
    }
    if let Some(handle) = query_handle {
        handle.abort();
    }

    match result {
        Ok(()) => {
            tracing::info!("Node stopped gracefully");
            Ok(())
        }
        Err(e) => {
            tracing::error!(error = %e, "Node stopped with error");
            Err(e)
        }
    }
}

/// Validate that the options are compatible with the configuration.
fn validate_run_options(config: &AppConfig, options: &RunOptions) -> anyhow::Result<()> {
    if options.mode == RunMode::Validator {
        if !config.node.is_validator {
            anyhow::bail!("Cannot run in validator mode: node is not configured as a validator");
        }
        if config.node.node_seed.is_none() {
            anyhow::bail!("Validators must have a node_seed configured");
        }
    }

    Ok(())
}

/// Print information about the node at startup.
fn print_startup_info(app: &App, options: &RunOptions) {
    let info = app.info();
    println!("henyey {}", info.version);
    println!();
    println!("Node: {}", info.node_name);
    println!("Mode: {}", options.mode);
    println!("Public Key: {}", info.public_key);
    println!("Network: {}", info.network_passphrase);
    println!();
}

/// Run the main application loop.
async fn run_main_loop(app: Arc<App>, options: RunOptions) -> anyhow::Result<()> {
    // Attempt to restore node state from persisted DB + on-disk bucket files.
    // This avoids a full catchup when the node restarts with intact state.
    if !options.force_catchup {
        match app.load_last_known_ledger().await {
            Ok(true) => {
                let (seq, _hash, _close_time, _protocol) = app.ledger_info();
                tracing::info!(lcl_seq = seq, "Restored state from disk, skipping full catchup");
                app.set_state(AppState::Synced).await;
                app.set_current_ledger(seq).await;
            }
            Ok(false) => {
                tracing::debug!("No persisted state available, will check catchup");
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to restore from disk, will check catchup");
            }
        }
    }

    // Check if we need to catch up
    let needs_catchup = check_needs_catchup(&app, &options).await?;

    if needs_catchup {
        if options.mode == RunMode::Watcher {
            tracing::info!("Watcher mode: skipping catchup");
        } else {
            tracing::info!("Node is behind, starting catchup");

            // Start overlay network BEFORE catchup to receive tx_sets during catchup.
            // This helps bridge the gap between catchup checkpoint and live consensus.
            app.start_overlay().await?;

            // Start a background task to cache messages during catchup.
            let catchup_message_handle = app.start_catchup_message_caching().await;

            // Start a background task to request SCP state after peers connect.
            // This triggers peers to send us EXTERNALIZE messages which contain tx_set_hashes.
            let scp_request_handle = {
                let app_clone = Arc::clone(&app);
                tokio::spawn(async move {
                    // Wait for peers to complete handshake
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    tracing::info!("Requesting SCP state during catchup");
                    app_clone.request_scp_state_from_peers().await;
                })
            };

            // Run catchup using mode from config
            let catchup_mode = app.config().catchup.to_mode();
            tracing::info!(?catchup_mode, "Starting catchup with configured mode");
            app.catchup_with_mode(CatchupTarget::Current, catchup_mode).await?;

            // Wait for SCP state request to complete
            let _ = scp_request_handle.await;

            // Stop the catchup message caching task
            if let Some(handle) = catchup_message_handle {
                handle.abort();
                tracing::info!("Stopped catchup message caching task");
            }
        }
    }

    // Start the sync recovery manager for consensus stuck detection
    app.start_sync_recovery();

    // Start the main run loop in the background so we can optionally wait for sync.
    tracing::info!("Starting main run loop");
    let run_app = Arc::clone(&app);
    let run_handle = tokio::spawn(async move { run_app.run().await });
    if options.wait_for_sync {
        wait_for_sync(&app).await;
    }
    match run_handle.await {
        Ok(result) => result?,
        Err(err) => anyhow::bail!("run loop task failed: {}", err),
    }

    Ok(())
}

/// Check if the node needs to catch up.
async fn check_needs_catchup(app: &App, options: &RunOptions) -> anyhow::Result<bool> {
    if options.force_catchup {
        return Ok(true);
    }

    let current_state = app.state().await;
    if current_state == AppState::Initializing {
        return Ok(true);
    }

    let (_seq, _hash, close_time, _protocol_version) = app.ledger_info();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let target_close_time = app.target_ledger_close_time() as u64;
    let max_age_seconds = target_close_time.saturating_mul(options.max_ledger_age as u64);
    Ok(is_ledger_too_old(close_time, now, max_age_seconds))
}

fn is_ledger_too_old(close_time: u64, now: u64, max_age_seconds: u64) -> bool {
    if close_time == 0 {
        return true;
    }
    if max_age_seconds == 0 {
        return false;
    }
    now.saturating_sub(close_time) > max_age_seconds
}

async fn wait_for_sync(app: &App) {
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    loop {
        interval.tick().await;
        let state = app.state().await;
        if matches!(state, AppState::Synced | AppState::Validating) {
            tracing::info!(state = %state, "Node is synced");
            break;
        }
        if state == AppState::ShuttingDown {
            tracing::warn!("Shutdown requested before sync completed");
            break;
        }
    }
}

/// Wait for a shutdown signal (Ctrl+C or SIGTERM).
async fn wait_for_shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            tracing::info!("Received Ctrl+C");
        }
        _ = terminate => {
            tracing::info!("Received SIGTERM");
        }
    }
}

/// Current node status and metrics.
///
/// Provides a snapshot of the node's operational state, useful for
/// monitoring and health checks. Available via the `/status` HTTP endpoint.
#[derive(Debug, Clone, Default, Serialize)]
pub struct NodeStatus {
    /// Current ledger sequence number.
    pub ledger_seq: u32,
    /// Hash of the current ledger header (hex-encoded).
    pub ledger_hash: Option<String>,
    /// Number of currently connected peers.
    pub peer_count: usize,
    /// Current SCP consensus state (e.g., "tracking", "synced").
    pub consensus_state: String,
    /// Number of transactions in the pending queue.
    pub pending_tx_count: usize,
    /// Node uptime in seconds since startup.
    pub uptime_secs: u64,
    /// Current application state (see [`AppState`]).
    pub state: String,
}

impl std::fmt::Display for NodeStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Node Status:")?;
        writeln!(f, "  State: {}", self.state)?;
        writeln!(f, "  Ledger: {}", self.ledger_seq)?;
        if let Some(hash) = &self.ledger_hash {
            writeln!(f, "  Ledger Hash: {}", hash)?;
        }
        writeln!(f, "  Peers: {}", self.peer_count)?;
        writeln!(f, "  Consensus: {}", self.consensus_state)?;
        writeln!(f, "  Pending TXs: {}", self.pending_tx_count)?;
        writeln!(f, "  Uptime: {}s", self.uptime_secs)?;
        Ok(())
    }
}

/// High-level node runner that manages the complete run lifecycle.
///
/// Wraps an [`App`] instance and provides a simpler interface for running
/// the node. Handles startup, shutdown coordination, and status queries.
///
/// # Example
///
/// ```no_run
/// use henyey_app::{AppConfig, RunOptions};
/// use henyey_app::run_cmd::NodeRunner;
///
/// # async fn example() -> anyhow::Result<()> {
/// let config = AppConfig::testnet();
/// let runner = NodeRunner::new(config, RunOptions::default()).await?;
/// runner.run().await?;
/// # Ok(())
/// # }
/// ```
pub struct NodeRunner {
    app: Arc<App>,
    options: RunOptions,
    start_time: std::time::Instant,
    shutdown_tx: broadcast::Sender<()>,
}

impl NodeRunner {
    /// Create a new node runner.
    pub async fn new(config: AppConfig, options: RunOptions) -> anyhow::Result<Self> {
        let app = Arc::new(App::new(config).await?);
        app.set_self_arc().await;
        let (shutdown_tx, _) = broadcast::channel(1);

        Ok(Self {
            app,
            options,
            start_time: std::time::Instant::now(),
            shutdown_tx,
        })
    }

    /// Get the application instance.
    pub fn app(&self) -> &Arc<App> {
        &self.app
    }

    /// Get the current node status.
    pub async fn status(&self) -> NodeStatus {
        let (ledger_seq, ledger_hash, _close_time, _protocol_version) = self.app.ledger_info();
        let stats = self.app.herder_stats();
        let peer_count = self.app.peer_snapshots().await.len();
        NodeStatus {
            ledger_seq,
            ledger_hash: Some(ledger_hash.to_hex()),
            peer_count,
            consensus_state: stats.state.to_string(),
            pending_tx_count: stats.pending_transactions,
            uptime_secs: self.start_time.elapsed().as_secs(),
            state: format!("{}", self.app.state().await),
        }
    }

    /// Run the node.
    pub async fn run(&self) -> anyhow::Result<()> {
        run_main_loop(self.app.clone(), self.options.clone()).await
    }

    /// Request shutdown.
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(());
        self.app.shutdown();
    }

    /// Subscribe to shutdown notifications.
    pub fn subscribe_shutdown(&self) -> broadcast::Receiver<()> {
        self.shutdown_tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run_mode_display() {
        assert_eq!(format!("{}", RunMode::Full), "full");
        assert_eq!(format!("{}", RunMode::Validator), "validator");
        assert_eq!(format!("{}", RunMode::Watcher), "watcher");
    }

    #[test]
    fn test_run_options_default() {
        let options = RunOptions::default();
        assert_eq!(options.mode, RunMode::Full);
        assert!(!options.force_catchup);
        assert!(options.wait_for_sync);
    }

    #[test]
    fn test_run_options_validator() {
        let options = RunOptions::validator();
        assert_eq!(options.mode, RunMode::Validator);
    }

    #[test]
    fn test_run_options_watcher() {
        let options = RunOptions::watcher();
        assert_eq!(options.mode, RunMode::Watcher);
        assert!(!options.wait_for_sync);
    }

    #[test]
    fn test_node_status_display() {
        let status = NodeStatus {
            ledger_seq: 1000,
            ledger_hash: None,
            peer_count: 5,
            consensus_state: "tracking".to_string(),
            pending_tx_count: 10,
            uptime_secs: 3600,
            state: "Synced".to_string(),
        };

        let display = format!("{}", status);
        assert!(display.contains("Ledger: 1000"));
        assert!(display.contains("Peers: 5"));
        assert!(display.contains("Uptime: 3600s"));
    }

    #[test]
    fn test_is_ledger_too_old() {
        assert!(is_ledger_too_old(0, 100, 10));
        assert!(!is_ledger_too_old(100, 105, 10));
        assert!(is_ledger_too_old(100, 111, 10));
        assert!(!is_ledger_too_old(100, 1000, 0));
    }
}
