//! Lightweight [`RpcAppHandle`] fake for testing RPC dispatch without booting
//! a full simulation node.
//!
//! See [`FakeRpcApp`] for details and [`FakeRpcAppBuilder`] for customization.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use henyey_app::app::AppInfo;
use henyey_app::config::AppConfig;
use henyey_app::{AppState, LedgerSummary};
use henyey_bucket::BucketSnapshotManager;
use henyey_common::Hash256;
use henyey_herder::TxQueueResult;
use henyey_ledger::{HeaderSnapshot, LedgerManager, LedgerManagerConfig, SorobanNetworkInfo};
use henyey_rpc::{RpcAppHandle, RpcServer};
use stellar_xdr::curr::{LedgerHeader, Limits, TransactionEnvelope, WriteXdr};
use tokio::sync::broadcast;

// ---------------------------------------------------------------------------
// FakeRpcApp
// ---------------------------------------------------------------------------

/// A lightweight [`RpcAppHandle`] implementation backed by in-memory state.
///
/// Suitable for testing RPC dispatch logic, envelope validation, error codes,
/// and response shapes without booting a full `App` / simulation node.
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
    // Optional overrides for ledger_summary(); when None, derived from
    // ledger_manager.current_header().
    ledger_seq_override: Option<u32>,
    close_time_override: Option<u64>,
    protocol_version_override: Option<u32>,
    base_fee_override: Option<u32>,
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
        }
    }

    fn ledger_summary(&self) -> LedgerSummary {
        let snap = self.ledger_manager.header_snapshot();
        let flags = match &snap.header.ext {
            stellar_xdr::curr::LedgerHeaderExt::V0 => 0,
            stellar_xdr::curr::LedgerHeaderExt::V1(ext) => ext.flags,
        };
        LedgerSummary {
            num: self.ledger_seq_override.unwrap_or(snap.header.ledger_seq),
            hash: snap.hash,
            close_time: self
                .close_time_override
                .unwrap_or(snap.header.scp_value.close_time.0),
            version: self
                .protocol_version_override
                .unwrap_or(snap.header.ledger_version),
            base_fee: self.base_fee_override.unwrap_or(snap.header.base_fee),
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
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Builder for [`FakeRpcApp`] with sensible defaults.
pub struct FakeRpcAppBuilder {
    state: AppState,
    submit_result: TxQueueResult,
    network_passphrase: String,
    ledger_seq: Option<u32>,
    close_time: Option<u64>,
    protocol_version: Option<u32>,
    base_fee: Option<u32>,
    header_snapshot: Option<LedgerHeader>,
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

    /// Set a custom [`LedgerHeader`] on the underlying [`LedgerManager`].
    ///
    /// The header hash is computed automatically from the XDR encoding.
    /// When set, `ledger_summary()` derives `num`, `close_time`, and
    /// `version` from this header (unless explicitly overridden), keeping
    /// `ledger_summary()` and `ledger_snapshot()` consistent.
    #[allow(dead_code)]
    pub fn header_snapshot(mut self, header: LedgerHeader) -> Self {
        self.header_snapshot = Some(header);
        self
    }

    pub fn build(self) -> FakeRpcApp {
        let mut config = AppConfig::testnet();
        config.rpc.enabled = true;
        config.network.passphrase = self.network_passphrase.clone();

        let ledger_manager = Arc::new(LedgerManager::new(
            self.network_passphrase,
            LedgerManagerConfig::default(),
        ));

        // Apply the header snapshot if provided, and derive overrides from it.
        let (ledger_seq_override, close_time_override, protocol_version_override) =
            if let Some(ref header) = self.header_snapshot {
                let xdr_bytes = header.to_xdr(Limits::none()).expect("header XDR encoding");
                let hash = Hash256::hash(&xdr_bytes);
                ledger_manager.set_header_for_test(header.clone(), hash);
                // Derive overrides from the header; explicit builder values take
                // precedence so callers can still tweak individual fields.
                (
                    Some(self.ledger_seq.unwrap_or(header.ledger_seq)),
                    Some(self.close_time.unwrap_or(header.scp_value.close_time.0)),
                    Some(self.protocol_version.unwrap_or(header.ledger_version)),
                )
            } else {
                (self.ledger_seq, self.close_time, self.protocol_version)
            };

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
            ledger_seq_override,
            close_time_override,
            protocol_version_override,
            base_fee_override: self.base_fee,
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
