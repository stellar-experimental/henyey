//! Lightweight [`RpcAppHandle`] fake for testing RPC dispatch without booting
//! a full simulation node.
//!
//! See [`FakeRpcApp`] for details and [`FakeRpcAppBuilder`] for customization.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use henyey_app::app::AppInfo;
use henyey_app::config::AppConfig;
use henyey_app::{AppState, LedgerSummary};
use henyey_bucket::BucketSnapshotManager;
use henyey_herder::TxQueueResult;
use henyey_ledger::{
    compute_header_hash, HeaderSnapshot, LedgerManager, LedgerManagerConfig, SorobanNetworkInfo,
};
use henyey_rpc::{RpcAppHandle, RpcServer};
use stellar_xdr::curr::{LedgerHeader, TransactionEnvelope};
use tokio::sync::broadcast;

// ---------------------------------------------------------------------------
// FakeRpcApp
// ---------------------------------------------------------------------------

/// A lightweight [`RpcAppHandle`] implementation backed by in-memory state.
///
/// All RPC-exposed header-derived fields (`sequence`, `protocolVersion`,
/// `closeTime`, `baseFee`, `id`/hash) are stored in a single
/// [`LedgerManager`] and served consistently by both [`ledger_summary()`]
/// and [`ledger_snapshot()`]. Builder scalar setters (`.ledger_seq()`,
/// `.close_time()`, etc.) mutate the underlying header at build time rather
/// than storing independent overrides.
///
/// Construct via [`FakeRpcApp::builder()`] or [`FakeRpcApp::default()`].
pub struct FakeRpcApp {
    config: AppConfig,
    ledger_manager: Arc<LedgerManager>,
    database: henyey_db::Database,
    bucket_snapshot_manager: Arc<BucketSnapshotManager>,
    shutdown_tx: broadcast::Sender<()>,
    state: AppState,
    submit_result: TxQueueResult,
    snapshot_ready: AtomicBool,
}

impl Default for FakeRpcApp {
    fn default() -> Self {
        Self::builder().build()
    }
}

#[async_trait::async_trait]
impl RpcAppHandle for FakeRpcApp {
    fn config(&self) -> &AppConfig {
        &self.config
    }

    fn info(&self) -> AppInfo {
        AppInfo {
            version: "0.0.0-test".to_string(),
            commit_hash: "deadbeef".to_string(),
            build_timestamp: "2024-01-01T00:00:00Z".to_string(),
            node_name: "fake-rpc-test-node".to_string(),
            public_key: String::new(),
            network_passphrase: self.config.network.passphrase.clone(),
            is_validator: false,
            database_path: PathBuf::from(":memory:"),
            meta_stream_bytes_total: 0,
            meta_stream_writes_total: 0,
            scp_verify: Default::default(),
            overlay_fetch_channel: Default::default(),
            post_catchup_hard_reset_total: 0,
        }
    }

    fn ledger_summary(&self) -> LedgerSummary {
        let snap = self.ledger_manager.header_snapshot();
        let flags = match &snap.header.ext {
            stellar_xdr::curr::LedgerHeaderExt::V0 => 0,
            stellar_xdr::curr::LedgerHeaderExt::V1(ext) => ext.flags,
        };
        LedgerSummary {
            num: snap.header.ledger_seq,
            hash: snap.hash,
            close_time: snap.header.scp_value.close_time.0,
            version: snap.header.ledger_version,
            base_fee: snap.header.base_fee,
            base_reserve: snap.header.base_reserve,
            max_tx_set_size: snap.header.max_tx_set_size,
            flags,
            age: 0,
        }
    }

    fn ledger_snapshot(&self) -> HeaderSnapshot {
        self.ledger_manager.header_snapshot()
    }

    fn database(&self) -> &henyey_db::Database {
        &self.database
    }

    fn bucket_snapshot_manager(&self) -> &Arc<BucketSnapshotManager> {
        &self.bucket_snapshot_manager
    }

    fn soroban_network_info(&self) -> Option<SorobanNetworkInfo> {
        None
    }

    fn subscribe_shutdown(&self) -> broadcast::Receiver<()> {
        self.shutdown_tx.subscribe()
    }

    async fn submit_transaction(&self, _tx: TransactionEnvelope) -> TxQueueResult {
        self.submit_result
    }

    async fn state(&self) -> AppState {
        self.state
    }

    fn is_snapshot_ready(&self) -> bool {
        self.snapshot_ready
            .load(std::sync::atomic::Ordering::Acquire)
    }
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Builder for [`FakeRpcApp`] with sensible defaults.
///
/// At build time, all scalar overrides (`.ledger_seq()`, `.close_time()`,
/// etc.) are applied to a base [`LedgerHeader`] and stored in the
/// [`LedgerManager`] via `set_header_for_test()`. This ensures
/// [`FakeRpcApp::ledger_summary()`] and [`FakeRpcApp::ledger_snapshot()`]
/// always return consistent RPC-exposed header-derived fields.
pub struct FakeRpcAppBuilder {
    state: AppState,
    submit_result: TxQueueResult,
    network_passphrase: String,
    ledger_seq: Option<u32>,
    close_time: Option<u64>,
    protocol_version: Option<u32>,
    base_fee: Option<u32>,
    header_snapshot: Option<LedgerHeader>,
    snapshot_ready: bool,
    max_ledger_meta_load_bytes: Option<usize>,
    max_tx_load_bytes: Option<usize>,
}

impl Default for FakeRpcAppBuilder {
    fn default() -> Self {
        Self {
            state: AppState::Synced,
            submit_result: TxQueueResult::Added,
            network_passphrase: "Test SDF Network ; September 2015".to_string(),
            ledger_seq: None,
            close_time: None,
            protocol_version: None,
            base_fee: None,
            header_snapshot: None,
            snapshot_ready: true,
            max_ledger_meta_load_bytes: None,
            max_tx_load_bytes: None,
        }
    }
}

impl FakeRpcAppBuilder {
    #[allow(dead_code)]
    pub fn state(mut self, state: AppState) -> Self {
        self.state = state;
        self
    }

    pub fn submit_result(mut self, result: TxQueueResult) -> Self {
        self.submit_result = result;
        self
    }

    #[allow(dead_code)]
    pub fn network_passphrase(mut self, passphrase: &str) -> Self {
        self.network_passphrase = passphrase.to_string();
        self
    }

    pub fn ledger_seq(mut self, seq: u32) -> Self {
        self.ledger_seq = Some(seq);
        self
    }

    pub fn close_time(mut self, time: u64) -> Self {
        self.close_time = Some(time);
        self
    }

    pub fn protocol_version(mut self, version: u32) -> Self {
        self.protocol_version = Some(version);
        self
    }

    #[allow(dead_code)]
    pub fn base_fee(mut self, fee: u32) -> Self {
        self.base_fee = Some(fee);
        self
    }

    /// Set a base [`LedgerHeader`] for the underlying [`LedgerManager`].
    ///
    /// Scalar builder setters (`.ledger_seq()`, `.close_time()`, etc.) are
    /// applied on top of this header at build time. If no scalar overrides
    /// are set, the header is used as-is.
    #[allow(dead_code)]
    pub fn header_snapshot(mut self, header: LedgerHeader) -> Self {
        self.header_snapshot = Some(header);
        self
    }

    pub fn snapshot_ready(mut self, ready: bool) -> Self {
        self.snapshot_ready = ready;
        self
    }

    /// Override the `getLedgers` DB load budget (bytes).
    #[allow(dead_code)]
    pub fn max_ledger_meta_load_bytes(mut self, bytes: usize) -> Self {
        self.max_ledger_meta_load_bytes = Some(bytes);
        self
    }

    /// Override the `getTransactions` DB load budget (bytes).
    #[allow(dead_code)]
    pub fn max_tx_load_bytes(mut self, bytes: usize) -> Self {
        self.max_tx_load_bytes = Some(bytes);
        self
    }

    pub fn build(self) -> FakeRpcApp {
        let mut config = AppConfig::testnet();
        config.rpc.enabled = true;
        config.network.passphrase = self.network_passphrase.clone();
        if let Some(budget) = self.max_ledger_meta_load_bytes {
            config.rpc.max_ledger_meta_load_bytes = budget;
        }
        if let Some(budget) = self.max_tx_load_bytes {
            config.rpc.max_tx_load_bytes = budget;
        }

        let ledger_manager = Arc::new(LedgerManager::new(
            self.network_passphrase,
            LedgerManagerConfig::default(),
        ));

        // Build the header: start from a custom base or the LedgerManager default,
        // then apply any scalar overrides. This ensures ledger_summary() and
        // ledger_snapshot() always agree on RPC-exposed fields.
        let mut header = self
            .header_snapshot
            .unwrap_or_else(|| ledger_manager.header_snapshot().header);

        if let Some(seq) = self.ledger_seq {
            header.ledger_seq = seq;
        }
        if let Some(time) = self.close_time {
            header.scp_value.close_time.0 = time;
        }
        if let Some(version) = self.protocol_version {
            header.ledger_version = version;
        }
        if let Some(fee) = self.base_fee {
            header.base_fee = fee;
        }

        let hash = compute_header_hash(&header).expect("header XDR encoding");
        ledger_manager.set_header_for_test(header, hash);

        let database =
            henyey_db::Database::open_in_memory().expect("in-memory database must succeed");

        let bucket_snapshot_manager = Arc::new(BucketSnapshotManager::empty(0));

        let (shutdown_tx, _) = broadcast::channel(1);

        FakeRpcApp {
            config,
            ledger_manager,
            database,
            bucket_snapshot_manager,
            shutdown_tx,
            state: self.state,
            submit_result: self.submit_result,
            snapshot_ready: AtomicBool::new(self.snapshot_ready),
        }
    }
}

impl FakeRpcApp {
    pub fn builder() -> FakeRpcAppBuilder {
        FakeRpcAppBuilder::default()
    }
}

// ---------------------------------------------------------------------------
// Test harness
// ---------------------------------------------------------------------------

/// A running RPC server backed by a [`FakeRpcApp`], with cleanup on drop.
pub struct FakeRpcTestHarness {
    pub url: String,
    pub app: Arc<FakeRpcApp>,
    pub shutdown_tx: broadcast::Sender<()>,
    pub serve_handle: tokio::task::JoinHandle<()>,
    pub client: reqwest::Client,
}

impl FakeRpcTestHarness {
    /// Start an RPC server backed by the given [`FakeRpcApp`].
    pub async fn start(app: FakeRpcApp) -> Self {
        let shutdown_tx = app.shutdown_tx.clone();
        let app = Arc::new(app);

        let (running, addr) = RpcServer::new(0, app.clone())
            .bind()
            .await
            .expect("fake RPC server bind");

        let url = format!("http://{addr}/");

        let serve_handle = tokio::spawn(async move {
            let _ = running.serve().await;
        });

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("reqwest client");

        Self {
            url,
            app,
            shutdown_tx,
            serve_handle,
            client,
        }
    }

    /// Start an RPC server with default [`FakeRpcApp`] settings.
    pub async fn start_default() -> Self {
        Self::start(FakeRpcApp::default()).await
    }

    /// Send a JSON-RPC request and return `(status_code, response_json)`.
    pub async fn post_rpc(&self, body: serde_json::Value) -> (u16, serde_json::Value) {
        let resp = self
            .client
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .expect("rpc request send");
        let status = resp.status().as_u16();
        let json: serde_json::Value = resp.json().await.expect("rpc response json");
        (status, json)
    }
}

impl Drop for FakeRpcTestHarness {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(());
        self.serve_handle.abort();
    }
}
