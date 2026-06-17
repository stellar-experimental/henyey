//! Deterministic multi-node simulation harness for validating consensus,
//! overlay, and ledger-close behavior across configurable topologies.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use henyey_app::config::{ConfigBuilder, QuorumSetConfig};
use henyey_app::{App, AppConfig, SimulationDebugStats};
use henyey_clock::RealClock;
use henyey_common::{Hash256, NetworkId};
use henyey_crypto::SecretKey;
use henyey_overlay::{
    AddPeerOutcome, ConnectionFactory, LoopbackConnectionFactory, OverlayError, PeerAddress,
    PeerId, TcpConnectionFactory,
};
use stellar_xdr::curr::{
    AccountId, Asset, CreateAccountOp, Memo, MuxedAccount, Operation, OperationBody, Preconditions,
    PublicKey, SequenceNumber, Transaction, TransactionEnvelope, TransactionExt,
    TransactionV1Envelope, Uint256, VecM,
};
use tempfile::TempDir;
use tokio::task::JoinHandle;

mod loadgen;
mod loopback;
use loopback::LoopbackNetwork;

const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const ROLLBACK_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// Wait for a task handle to complete within `timeout`, aborting it if the
/// deadline expires.
///
/// - Clean exit → returns `Ok(())`
/// - Task error or `JoinError` → returns `Err(...)`
/// - Timeout → calls `handle.abort()` and returns `Ok(())` (abort is
///   best-effort cancellation, not an error for the caller)
async fn join_or_abort(
    mut handle: JoinHandle<anyhow::Result<()>>,
    timeout: Duration,
) -> anyhow::Result<()> {
    match tokio::time::timeout(timeout, &mut handle).await {
        Ok(join_result) => join_result?,
        Err(_elapsed) => {
            handle.abort();
            Ok(())
        }
    }
}
mod applyload;
mod loadgen_pregenerated;
mod loadgen_soroban;
pub use applyload::{ApplyLoad, ApplyLoadConfig, ApplyLoadMode, Histogram};
pub use loadgen::{
    GeneratedLoadConfig, GeneratedTransaction, LoadGenMode, LoadGenerator, LoadReport, LoadResult,
    LoadStep, TestAccount, TxGenerator,
};
pub use loadgen_pregenerated::PregeneratedTxReader;
pub use loadgen_soroban::{BatchTransfer, ContractInvocation, SacTransfer, SorobanTxBuilder};

mod poll;
pub use poll::{poll_until, CrashScope, PollOutcome};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimulationMode {
    OverLoopback,
    OverTcp,
}

/// How a simulation node should be initialized.
///
/// Controls both database initialization (genesis vs. existing state) and
/// the post-construction bootstrap path. Used by [`NodeStartupPlan`] to bind
/// the mode at plan-construction time so that `insert_ready_node` cannot
/// diverge from the build-time decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BootstrapMode {
    /// Fresh genesis — writes genesis state to an empty DB, then boots via
    /// `bootstrap_from_db()`. Asserts `ledger_seq == 1` as a postcondition.
    Genesis,
    /// Restore from persisted state — skips genesis init, loads last known
    /// ledger. Errors if no persisted state exists. This is stricter than
    /// `App::load_last_known_ledger()` (which treats NoState as non-fatal)
    /// because the simulation's Restore path is only used by `restart_node`,
    /// which always has a persisted data dir.
    Restore,
}

/// A prepared node ready for bootstrap. Carries the [`BootstrapMode`] chosen
/// at build time so that [`Simulation::insert_ready_node`] cannot use a
/// different mode than the one used to initialize the database.
struct NodeStartupPlan {
    id: String,
    app: App,
    data_dir: Arc<TempDir>,
    peer_port: u16,
    mode: BootstrapMode,
    pre_bound_listener: Option<henyey_overlay::Listener>,
}

/// Aggregate stats from one round of `repair_app_connectivity`.
///
/// All conditions are retryable — the stabilize loop always retries until its
/// overall timeout expires.  `RepairReport` is purely diagnostic: when the
/// timeout fires, these stats are included in the error message.
#[derive(Debug, Default)]
struct RepairReport {
    initiated: usize,
    already_connected: usize,
    pool_full: usize,
    overlay_not_ready: usize,
    errors: usize,
}

impl std::fmt::Display for RepairReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "initiated={}, already_connected={}, pool_full={}, overlay_not_ready={}, errors={}",
            self.initiated,
            self.already_connected,
            self.pool_full,
            self.overlay_not_ready,
            self.errors,
        )
    }
}

/// Check whether all sequence numbers in `seqs` are at least `min_ledger`
/// and within `max_spread` of each other.
fn seqs_within_spread(seqs: &[u32], min_ledger: u32, max_spread: u32) -> bool {
    if seqs.is_empty() {
        return false;
    }
    let min_seq = *seqs.iter().min().unwrap_or(&0);
    let max_seq = *seqs.iter().max().unwrap_or(&0);
    min_seq >= min_ledger && max_seq.saturating_sub(min_seq) <= max_spread
}

#[derive(Debug, Clone)]
pub struct SimNode {
    pub node_id: String,
    pub secret_key: SecretKey,
    pub ledger_seq: u32,
    pub ledger_hash: Hash256,
}

#[derive(Debug, Clone)]
struct AppNodeSpec {
    node_id: String,
    secret_key: SecretKey,
    quorum_set: QuorumSetConfig,
    is_validator: bool,
    manual_close: bool,
    data_dir: Option<Arc<TempDir>>,
    peer_port: Option<u16>,
}

/// Task handle and exit status for a running node.
///
/// Grouped so that crash-detection (`find_exited_node`, `app_task_finished`,
/// `app_task_status`) can operate on task state without touching the `App`.
pub(crate) struct NodeTaskHandle {
    handle: JoinHandle<anyhow::Result<()>>,
    status: Arc<tokio::sync::RwLock<Option<Result<(), String>>>>,
}

struct RunningAppNode {
    app: Arc<App>,
    task: NodeTaskHandle,
    _data_dir: Arc<TempDir>,
    peer_port: u16,
}

pub struct Simulation {
    mode: SimulationMode,
    network_passphrase: String,
    nodes: HashMap<String, SimNode>,
    loopback: LoopbackNetwork,
    app_specs: HashMap<String, AppNodeSpec>,
    running_apps: HashMap<String, RunningAppNode>,
    app_account_sequences: HashMap<String, i64>,
    root_sequence: i64,
    overlay_connection_factory: Option<Arc<dyn ConnectionFactory>>,
    /// Whether Soroban upgrade setup has completed (set by `SorobanUpgradeSetup`
    /// loadgen mode).
    ///
    /// Matches stellar-core `Simulation::mSetupForSorobanUpgrade`.
    setup_for_soroban_upgrade: bool,
    /// Test-only task handles for mock nodes. `find_exited_node`,
    /// `app_task_finished`, and `app_task_status` check this map
    /// before `running_apps`, enabling crash-detection tests without
    /// constructing a real `App`.
    #[cfg(test)]
    test_nodes: HashMap<String, NodeTaskHandle>,
    /// Test-only counter for `crank_all_nodes()` invocations.
    #[cfg(test)]
    crank_count: usize,
}

impl std::fmt::Debug for RunningAppNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunningAppNode")
            .field("peer_port", &self.peer_port)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for Simulation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Simulation")
            .field("mode", &self.mode)
            .field("network_passphrase", &self.network_passphrase)
            .field("nodes", &self.nodes.keys().collect::<Vec<_>>())
            .field("app_specs", &self.app_specs.keys().collect::<Vec<_>>())
            .field(
                "running_apps",
                &self.running_apps.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl Simulation {
    pub fn new(mode: SimulationMode) -> Self {
        Self::with_network(mode, AppConfig::testnet().network.passphrase)
    }

    pub fn with_network(mode: SimulationMode, network_passphrase: impl Into<String>) -> Self {
        Self {
            mode,
            network_passphrase: network_passphrase.into(),
            nodes: HashMap::new(),
            loopback: LoopbackNetwork::new(),
            app_specs: HashMap::new(),
            running_apps: HashMap::new(),
            app_account_sequences: HashMap::new(),
            root_sequence: 1,
            overlay_connection_factory: None,
            setup_for_soroban_upgrade: false,
            #[cfg(test)]
            test_nodes: HashMap::new(),
            #[cfg(test)]
            crank_count: 0,
        }
    }

    pub fn add_node(&mut self, node_id: impl Into<String>, secret_key: SecretKey) {
        let node_id = node_id.into();
        let node = SimNode {
            node_id: node_id.clone(),
            secret_key,
            ledger_seq: 1,
            ledger_hash: Hash256::hash(node_id.as_bytes()),
        };
        self.nodes.insert(node_id, node);
    }

    pub fn add_app_node(
        &mut self,
        node_id: impl Into<String>,
        secret_key: SecretKey,
        quorum_set: QuorumSetConfig,
    ) {
        let node_id = node_id.into();
        self.app_specs.insert(
            node_id.clone(),
            AppNodeSpec {
                node_id,
                secret_key,
                quorum_set,
                is_validator: true,
                // Henyey simulation uses MANUAL_CLOSE=true with real SCP
                // consensus, unlike stellar-core simulation which sets
                // MANUAL_CLOSE=false (Simulation.cpp:99). This requires
                // run_standalone=false (the default) so that SCP envelope
                // exchange is not suppressed by HerderConfig::suppress_scp().
                manual_close: true,
                data_dir: None,
                peer_port: None,
            },
        );
    }

    pub fn populate_app_nodes_from_existing(&mut self, threshold_percent: u32) {
        self.populate_app_nodes_from_existing_with_quorum_adjuster(
            threshold_percent,
            |_, quorum_set| quorum_set,
        );
    }

    pub fn populate_app_nodes_from_existing_with_quorum_adjuster<F>(
        &mut self,
        threshold_percent: u32,
        mut adjuster: F,
    ) where
        F: FnMut(&str, QuorumSetConfig) -> QuorumSetConfig,
    {
        let mut ids: Vec<String> = self.nodes.keys().cloned().collect();
        ids.sort();
        let validators: Vec<String> = ids
            .iter()
            .filter_map(|id| {
                self.nodes
                    .get(id)
                    .map(|n| n.secret_key.public_key().to_strkey())
            })
            .collect();

        let quorum_set = QuorumSetConfig {
            threshold_percent,
            validators,
            inner_sets: Vec::new(),
        };

        for id in ids {
            if let Some(node) = self.nodes.get(&id) {
                self.add_app_node(
                    id.clone(),
                    node.secret_key.clone(),
                    adjuster(&id, quorum_set.clone()),
                );
            }
        }
    }

    pub fn add_pending_connection(&mut self, a: impl Into<String>, b: impl Into<String>) {
        let a = a.into();
        let b = b.into();
        if a == b {
            return;
        }
        self.loopback.add_link(a, b);
    }

    pub async fn start_all_nodes(&mut self) {
        if self.app_specs.is_empty() {
            return;
        }

        self.try_start_all_nodes()
            .await
            .expect("failed to start app-backed simulation nodes");
    }

    pub async fn try_start_all_nodes(&mut self) -> anyhow::Result<()> {
        if self.app_specs.is_empty() {
            return Ok(());
        }

        if !self.running_apps.is_empty() {
            return Ok(());
        }

        let mut ids: Vec<String> = self.app_specs.keys().cloned().collect();
        ids.sort();

        let overlay_connection_factory: Arc<dyn ConnectionFactory> = match self.mode {
            SimulationMode::OverLoopback => Arc::new(LoopbackConnectionFactory::default()),
            SimulationMode::OverTcp => Arc::new(TcpConnectionFactory),
        };
        self.overlay_connection_factory = Some(Arc::clone(&overlay_connection_factory));

        // Build port_map and optionally pre-bind TCP listeners.
        //
        // TCP mode: bind each node's listener to port 0 (OS-assigned ephemeral
        // port) up front, then derive the port_map from the actual ports.
        // This eliminates the process-local AtomicU16 counter that caused
        // cross-binary port collisions (see #2480, #2491).
        //
        // Loopback mode: the loopback registry is keyed by port number, so
        // port 0 has no meaning.  Continue using allocate_port_block().
        let (port_map, mut pre_bound_listeners) = match self.mode {
            SimulationMode::OverTcp => {
                let mut port_map = HashMap::new();
                let mut listeners: HashMap<String, henyey_overlay::Listener> = HashMap::new();
                for id in &ids {
                    let listener = TcpConnectionFactory
                        .bind(0)
                        .await
                        .with_context(|| format!("pre-bind TCP listener for {id}"))?;
                    let port = listener.local_addr().port();
                    port_map.insert(id.clone(), port);
                    listeners.insert(id.clone(), listener);
                }
                (port_map, listeners)
            }
            SimulationMode::OverLoopback => {
                let base_port = allocate_port_block(ids.len() as u16 + 8);
                let port_map: HashMap<String, u16> = ids
                    .iter()
                    .enumerate()
                    .map(|(i, id)| (id.clone(), base_port + i as u16))
                    .collect();
                (port_map, HashMap::new())
            }
        };

        // Phase 1: Collect all data needed for spawning. Write data_dir and
        // peer_port back into self.app_specs (needed by restart_node later).
        let mut plans: Vec<NodeStartupPlan> = Vec::new();
        for id in ids {
            let spec = self
                .app_specs
                .get_mut(&id)
                .with_context(|| format!("missing app spec for {}", id))?;

            let data_dir = spec.data_dir.clone().unwrap_or_else(|| {
                Arc::new(tempfile::tempdir().expect("create simulation data dir"))
            });
            spec.data_dir = Some(Arc::clone(&data_dir));
            spec.peer_port = port_map.get(&id).copied();

            let listener = pre_bound_listeners.remove(&id);

            // Re-borrow as shared ref to satisfy build_node_startup_plan(&self, ...).
            let spec = &self.app_specs[&id];
            let plan = self
                .build_node_startup_plan(
                    spec,
                    &port_map,
                    data_dir,
                    Arc::clone(&overlay_connection_factory),
                    BootstrapMode::Genesis,
                    listener,
                )
                .await
                .with_context(|| format!("build app node {}", spec.node_id))?;

            plans.push(plan);
        }

        // Phase 2: Bootstrap and insert each node. Each call waits for
        // overlay readiness before returning, enforcing the invariant by
        // construction.
        for plan in plans {
            let node_id = plan.id.clone();
            self.insert_ready_node(plan, Duration::from_secs(30))
                .await
                .with_context(|| format!("bootstrap app node {}", node_id))?;
        }

        Ok(())
    }

    pub async fn stop_all_nodes(&mut self) -> anyhow::Result<()> {
        let running = std::mem::take(&mut self.running_apps);
        // Phase 1: Signal shutdown to ALL nodes before waiting on any.
        for node in running.values() {
            node.app.shutdown();
        }
        // Phase 2: Wait for each task to exit, aborting on timeout.
        // Attempt cleanup of every node before returning the first error.
        let mut first_error: Option<anyhow::Error> = None;
        for (id, node) in running {
            if let Err(e) = join_or_abort(node.task.handle, DEFAULT_SHUTDOWN_TIMEOUT)
                .await
                .with_context(|| format!("join app task for {}", id))
            {
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
        }
        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    pub async fn remove_node(&mut self, node_id: &str) -> anyhow::Result<()> {
        self.disconnect_node_from_peers(node_id).await?;
        if let Some(node) = self.running_apps.remove(node_id) {
            node.app.shutdown();
            let _ = join_or_abort(node.task.handle, DEFAULT_SHUTDOWN_TIMEOUT).await;
        }
        Ok(())
    }

    pub async fn restart_node(&mut self, node_id: &str) -> anyhow::Result<()> {
        self.remove_node(node_id).await?;

        let spec = self
            .app_specs
            .get(node_id)
            .cloned()
            .with_context(|| format!("missing app spec for {}", node_id))?;
        let data_dir = spec
            .data_dir
            .clone()
            .with_context(|| format!("missing persisted data dir for {}", node_id))?;
        let peer_port = spec
            .peer_port
            .with_context(|| format!("missing peer port for {}", node_id))?;
        let mut port_map: HashMap<String, u16> = self
            .app_specs
            .iter()
            .filter_map(|(id, spec)| spec.peer_port.map(|port| (id.clone(), port)))
            .collect();
        port_map.insert(node_id.to_string(), peer_port);

        let overlay_connection_factory = self
            .overlay_connection_factory
            .clone()
            .with_context(|| "simulation overlay connection factory not initialized".to_string())?;

        let plan = self
            .build_node_startup_plan(
                &spec,
                &port_map,
                data_dir,
                Arc::clone(&overlay_connection_factory),
                BootstrapMode::Restore,
                None,
            )
            .await?;

        self.insert_ready_node(plan, Duration::from_secs(30))
            .await?;

        Ok(())
    }

    pub async fn add_connection(&self, a: &str, b: &str) -> anyhow::Result<()> {
        let b_port = self
            .running_apps
            .get(b)
            .with_context(|| format!("missing running app node {}", b))?
            .peer_port;
        let a_app = self
            .running_apps
            .get(a)
            .with_context(|| format!("missing running app node {}", a))?;
        a_app
            .app
            .add_peer(henyey_overlay::PeerAddress::new("127.0.0.1", b_port))
            .await?;
        Ok(())
    }

    async fn disconnect_node_from_peers(&self, node_id: &str) -> anyhow::Result<()> {
        let secret = self.secret_for_node(node_id)?;
        let peer_id = henyey_overlay::PeerId::from_bytes(*secret.public_key().as_bytes());
        for (id, node) in &self.running_apps {
            if id == node_id {
                continue;
            }
            let _ = node.app.disconnect_peer(&peer_id).await;
        }
        Ok(())
    }

    pub async fn manual_close_all_app_nodes(&self) -> anyhow::Result<Vec<u32>> {
        let mut ids: Vec<_> = self.running_apps.keys().cloned().collect();
        ids.sort();
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            out.push(self.manual_close_app_node(&id).await?);
        }
        Ok(out)
    }

    pub async fn manual_close_app_node(&self, node_id: &str) -> anyhow::Result<u32> {
        let node = self
            .running_apps
            .get(node_id)
            .with_context(|| format!("missing running app node {}", node_id))?;
        node.app.manual_close_ledger().await
    }

    pub fn app_ledger_seq(&self, node_id: &str) -> Option<u32> {
        self.running_apps
            .get(node_id)
            .map(|n| n.app.ledger_info().ledger_seq)
    }

    pub fn app_latest_externalized_slot(&self, node_id: &str) -> Option<u64> {
        self.running_apps
            .get(node_id)
            .and_then(|n| n.app.latest_externalized_slot())
    }

    pub fn app(&self, node_id: &str) -> Option<Arc<App>> {
        self.running_apps.get(node_id).map(|n| Arc::clone(&n.app))
    }

    /// Return all running `App` instances.
    ///
    /// Matches stellar-core `Simulation::getNodes()`.
    pub fn apps(&self) -> Vec<Arc<App>> {
        let mut ids: Vec<&String> = self.running_apps.keys().collect();
        ids.sort();
        ids.into_iter()
            .filter_map(|id| self.running_apps.get(id).map(|n| Arc::clone(&n.app)))
            .collect()
    }

    /// Directed disconnect: tell `initiator` to drop its connection to `acceptor`.
    ///
    /// Matches stellar-core `Simulation::dropConnection(initiator, acceptor)`.
    pub async fn drop_connection(&self, initiator: &str, acceptor: &str) -> anyhow::Result<()> {
        let acceptor_secret = self.secret_for_node(acceptor)?;
        let peer_id = henyey_overlay::PeerId::from_bytes(*acceptor_secret.public_key().as_bytes());
        let initiator_app = self
            .running_apps
            .get(initiator)
            .with_context(|| format!("missing running app node {}", initiator))?;
        let _ = initiator_app.app.disconnect_peer(&peer_id).await;
        Ok(())
    }

    /// Get the expected Unix timestamp (seconds) of the next ledger close for a node.
    ///
    /// Returns `tracking_consensus_close_time + ledger_close_duration.as_secs()`.
    /// Matches stellar-core's expected close time calculation used in
    /// various `crankUntil` calls.
    pub fn expected_next_ledger_close_unix_secs(&self, node_id: &str) -> Option<u64> {
        let app = self.running_apps.get(node_id)?;
        Some(app.app.expected_next_ledger_close_unix_secs())
    }

    /// Check whether Soroban upgrade setup has completed.
    ///
    /// Matches stellar-core `Simulation::isSetUpForSorobanUpgrade()`.
    pub fn is_setup_for_soroban_upgrade(&self) -> bool {
        self.setup_for_soroban_upgrade
    }

    /// Mark Soroban upgrade setup as complete.
    ///
    /// Matches stellar-core `Simulation::markReadyForSorobanUpgrade()`.
    pub fn mark_ready_for_soroban_upgrade(&mut self) {
        self.setup_for_soroban_upgrade = true;
    }

    pub async fn app_peer_count(&self, node_id: &str) -> Option<usize> {
        let app = self.running_apps.get(node_id)?;
        Some(app.app.peer_count().await)
    }

    pub fn app_task_finished(&self, node_id: &str) -> Option<bool> {
        #[cfg(test)]
        if let Some(th) = self.test_nodes.get(node_id) {
            return Some(th.handle.is_finished());
        }
        self.running_apps
            .get(node_id)
            .map(|n| n.task.handle.is_finished())
    }

    /// Abort a node's task without removing it from `running_apps`.
    ///
    /// The handle remains in place so `app_task_finished` returns
    /// `Some(true)` — useful for testing fail-fast crash detection.
    pub fn abort_node_task(&self, node_id: &str) {
        #[cfg(test)]
        if let Some(th) = self.test_nodes.get(node_id) {
            th.handle.abort();
            return;
        }
        if let Some(node) = self.running_apps.get(node_id) {
            node.task.handle.abort();
        }
    }

    pub async fn app_task_status(&self, node_id: &str) -> Option<Result<(), String>> {
        #[cfg(test)]
        if let Some(th) = self.test_nodes.get(node_id) {
            return th.status.read().await.clone();
        }
        let node = self.running_apps.get(node_id)?;
        node.task.status.read().await.clone()
    }

    /// Bail immediately if any running app node's task has exited.
    ///
    /// Thin wrapper around `find_exited_node` with `CrashScope::AllNodes`.
    async fn bail_if_any_node_exited(&self) -> anyhow::Result<()> {
        if let Some((id, status)) = self.find_exited_node(&CrashScope::AllNodes).await {
            anyhow::bail!(
                "node {id} task exited during connectivity wait \
                 (task_status: {status:?})"
            );
        }
        Ok(())
    }

    /// Return sorted per-node task diagnostics for all known nodes.
    ///
    /// Covers all nodes from `app_specs` (via `app_node_ids`), including
    /// nodes removed from `running_apps`. Removed/not-started nodes show
    /// `<not running>`.
    pub async fn node_task_diagnostics(&self) -> String {
        let mut diag = String::new();
        for id in self.app_node_ids() {
            let finished = self.app_task_finished(&id);
            let status = self.app_task_status(&id).await;
            match finished {
                Some(_) => {
                    diag.push_str(&format!(
                        "\n  {id}: task_finished={finished:?}, task_status={status:?}"
                    ));
                }
                None => {
                    diag.push_str(&format!("\n  {id}: <not running>"));
                }
            }
        }
        diag
    }

    pub async fn app_debug_stats(&self, node_id: &str) -> Option<SimulationDebugStats> {
        let node = self.running_apps.get(node_id)?;
        Some(node.app.simulation_debug_stats().await)
    }

    pub async fn wait_for_app_connectivity(
        &self,
        min_peers: usize,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        let outcome = poll_until(
            self,
            timeout,
            Duration::from_millis(100),
            CrashScope::AllNodes,
            || async {
                let mut ids: Vec<&String> = self.running_apps.keys().collect();
                ids.sort();
                for id in &ids {
                    if self.app_peer_count(id).await.unwrap_or(0) < min_peers {
                        return Ok(None);
                    }
                }
                Ok(Some(()))
            },
        )
        .await?;
        match outcome {
            PollOutcome::Satisfied(()) => Ok(()),
            PollOutcome::NodeExited { node_id, status } => {
                anyhow::bail!(
                    "node {node_id} task exited during connectivity wait \
                     (task_status: {status:?})"
                )
            }
            PollOutcome::TimedOut => {
                let mut counts = Vec::new();
                let mut ids: Vec<&String> = self.running_apps.keys().collect();
                ids.sort();
                for id in &ids {
                    let count = self.app_peer_count(id).await.unwrap_or(0);
                    counts.push(format!("{id}={count}"));
                }
                let task_diag = self.node_task_diagnostics().await;
                anyhow::bail!(
                    "wait_for_app_connectivity: not all apps reached {min_peers} peers \
                     within {timeout:?} (current counts: {}){task_diag}",
                    counts.join(", ")
                )
            }
        }
    }

    /// Wait until every running app node's overlay manager has been started.
    ///
    /// Polls `is_overlay_started()` for each node with 50ms intervals using
    /// the shared `poll_until` + `CrashScope::AllNodes` machinery.
    ///
    /// Called at the end of `try_start_all_nodes()` and `restart_node()` to
    /// guarantee that `add_peer()` will not return `OverlayError::NotStarted`
    /// when callers begin establishing connections.
    async fn wait_for_all_overlays_started(&self, timeout: Duration) -> anyhow::Result<()> {
        let outcome = poll_until(
            self,
            timeout,
            Duration::from_millis(50),
            CrashScope::AllNodes,
            || async {
                for node in self.running_apps.values() {
                    if !node.app.is_overlay_started().await {
                        return Ok(None);
                    }
                }
                Ok(Some(()))
            },
        )
        .await?;
        match outcome {
            PollOutcome::Satisfied(()) => Ok(()),
            PollOutcome::NodeExited { node_id, status } => {
                anyhow::bail!(
                    "wait_for_all_overlays_started: node {node_id} task exited \
                     (status: {status:?}) before all overlays were ready"
                )
            }
            PollOutcome::TimedOut => {
                let mut not_ready = Vec::new();
                for (id, node) in &self.running_apps {
                    if !node.app.is_overlay_started().await {
                        not_ready.push(id.clone());
                    }
                }
                not_ready.sort();
                let task_diag = self.node_task_diagnostics().await;
                anyhow::bail!(
                    "wait_for_all_overlays_started: timed out after {timeout:?}, \
                     nodes not ready: {not_ready:?}{task_diag}"
                )
            }
        }
    }

    /// Kick-start connections between topology neighbors.
    ///
    /// Only dials peers that are linked in the configured topology
    /// (`LoopbackNetwork`), using the same `known_peers_for()` helper that
    /// `build_app_from_spec` uses for `config.overlay.known_peers`. This
    /// prevents cross-partition connections in sparse topologies like
    /// `separate` (two isolated pairs).
    ///
    /// Returns a [`RepairReport`] with aggregate stats.  The function never
    /// short-circuits — every node/address pair is attempted regardless of
    /// per-node errors.
    async fn repair_app_connectivity(&self) -> RepairReport {
        let port_map: HashMap<String, u16> = self
            .running_apps
            .iter()
            .map(|(id, node)| (id.clone(), node.peer_port))
            .collect();

        let mut ids: Vec<&String> = self.running_apps.keys().collect();
        ids.sort();

        let mut report = RepairReport::default();

        for id in ids {
            let node = &self.running_apps[id];
            for addr in self.known_peers_for(id, &port_map) {
                match node.app.add_peer(addr).await {
                    Ok(AddPeerOutcome::Initiated) => report.initiated += 1,
                    Ok(AddPeerOutcome::AlreadyConnected) => report.already_connected += 1,
                    Ok(AddPeerOutcome::PoolFull) => report.pool_full += 1,
                    Err(OverlayError::NotStarted) => report.overlay_not_ready += 1,
                    Err(e) => {
                        report.errors += 1;
                        tracing::warn!("add_peer error for node {id}: {e}");
                    }
                }
            }
        }

        report
    }

    pub async fn stabilize_app_tcp_connectivity(
        &self,
        min_peers: usize,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut cumulative_report = RepairReport::default();
        loop {
            self.bail_if_any_node_exited().await?;

            let report = self.repair_app_connectivity().await;
            cumulative_report.initiated += report.initiated;
            cumulative_report.already_connected += report.already_connected;
            cumulative_report.pool_full += report.pool_full;
            cumulative_report.overlay_not_ready += report.overlay_not_ready;
            cumulative_report.errors += report.errors;

            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .unwrap_or(Duration::ZERO);
            let probe_timeout = remaining.min(Duration::from_secs(1));
            match self
                .wait_for_app_connectivity(min_peers, probe_timeout)
                .await
            {
                Ok(()) => return Ok(()),
                Err(_) => {
                    // Distinguish fatal task exit from retryable probe timeout.
                    self.bail_if_any_node_exited().await?;
                }
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .unwrap_or(Duration::ZERO);
            tokio::time::sleep(Duration::from_millis(100).min(remaining)).await;
        }
        // Final probe with zero timeout to get per-node peer counts for diagnostics.
        match self
            .wait_for_app_connectivity(min_peers, Duration::from_millis(0))
            .await
        {
            Err(detail) => anyhow::bail!(
                "stabilize_app_tcp_connectivity: connectivity did not stabilize \
                 within {timeout:?}: {detail} (repair: {cumulative_report})"
            ),
            Ok(()) => Ok(()),
        }
    }

    /// Returns the expected set of authenticated peer IDs for a node based on
    /// the configured topology (`LoopbackNetwork` links) and currently running
    /// apps.
    ///
    /// Only neighbors that are present in `running_apps` are included — a
    /// topology neighbor that has not been started (or has been removed) is
    /// excluded.
    fn expected_peer_ids(&self, node_id: &str) -> HashSet<PeerId> {
        self.loopback
            .neighbors(node_id)
            .into_iter()
            .filter(|neighbor| self.running_apps.contains_key(neighbor))
            .filter_map(|neighbor| {
                self.secret_for_node(&neighbor)
                    .ok()
                    .map(|secret| PeerId::from_bytes(*secret.public_key().as_bytes()))
            })
            .collect()
    }

    /// Wait until every running node is connected to **exactly** the set of
    /// peers defined by the configured topology.
    ///
    /// Unlike `wait_for_app_connectivity` (which uses a global minimum-peer
    /// floor), this checks per-node peer *identity* against the topology graph,
    /// catching under-connection, over-connection, and wrong-peer topologies.
    ///
    /// **Precondition:** validates the *static configured link graph* among
    /// currently running apps. Does **not** account for runtime fault injection
    /// (`partition()`, `set_drop_prob()`). Invalid to call during active fault
    /// injection.
    pub async fn wait_for_topology_connectivity(&self, timeout: Duration) -> anyhow::Result<()> {
        let outcome = poll_until(
            self,
            timeout,
            Duration::from_millis(100),
            CrashScope::AllNodes,
            || async {
                let mut ids: Vec<&String> = self.running_apps.keys().collect();
                ids.sort();
                for id in &ids {
                    let expected = self.expected_peer_ids(id);
                    let actual: HashSet<PeerId> = self.running_apps[*id]
                        .app
                        .peer_snapshots()
                        .await
                        .into_iter()
                        .map(|s| s.info.peer_id)
                        .collect();
                    if expected != actual {
                        return Ok(None);
                    }
                }
                Ok(Some(()))
            },
        )
        .await?;
        match outcome {
            PollOutcome::Satisfied(()) => Ok(()),
            PollOutcome::NodeExited { node_id, status } => {
                anyhow::bail!(
                    "node {node_id} task exited during connectivity wait \
                     (task_status: {status:?})"
                )
            }
            PollOutcome::TimedOut => {
                // Build per-node diagnostics with sorted strkeys for determinism.
                let mut diag_lines = Vec::new();
                let mut ids: Vec<&String> = self.running_apps.keys().collect();
                ids.sort();
                for id in &ids {
                    let expected = self.expected_peer_ids(id);
                    let actual: HashSet<PeerId> = self.running_apps[*id]
                        .app
                        .peer_snapshots()
                        .await
                        .into_iter()
                        .map(|s| s.info.peer_id)
                        .collect();
                    let mut missing: Vec<String> = expected
                        .difference(&actual)
                        .map(|p| p.to_strkey())
                        .collect();
                    missing.sort();
                    let mut unexpected: Vec<String> = actual
                        .difference(&expected)
                        .map(|p| p.to_strkey())
                        .collect();
                    unexpected.sort();

                    if !missing.is_empty() || !unexpected.is_empty() {
                        let mut parts = vec![format!(
                            "{id}: expected={} actual={}",
                            expected.len(),
                            actual.len()
                        )];
                        if !missing.is_empty() {
                            parts.push(format!("missing=[{}]", missing.join(", ")));
                        }
                        if !unexpected.is_empty() {
                            parts.push(format!("unexpected=[{}]", unexpected.join(", ")));
                        }
                        diag_lines.push(parts.join(" "));
                    }
                }

                let task_diag = self.node_task_diagnostics().await;
                anyhow::bail!(
                    "wait_for_topology_connectivity: topology mismatch after {timeout:?} \
                     ({}){}",
                    diag_lines.join("; "),
                    task_diag
                )
            }
        }
    }

    /// Kick-start topology-correct connections and wait until every node is
    /// connected to exactly its expected peers.
    ///
    /// Combines `repair_app_connectivity()` with
    /// `wait_for_topology_connectivity()` in a retry loop, analogous to
    /// `stabilize_app_tcp_connectivity()` but topology-aware and without a
    /// caller-supplied `min_peers`.
    ///
    /// **Precondition:** same as `wait_for_topology_connectivity` — validates
    /// the static configured link graph. Invalid during active fault injection.
    pub async fn stabilize_app_topology_connectivity(
        &self,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut cumulative_report = RepairReport::default();
        loop {
            self.bail_if_any_node_exited().await?;

            let report = self.repair_app_connectivity().await;
            cumulative_report.initiated += report.initiated;
            cumulative_report.already_connected += report.already_connected;
            cumulative_report.pool_full += report.pool_full;
            cumulative_report.overlay_not_ready += report.overlay_not_ready;
            cumulative_report.errors += report.errors;

            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .unwrap_or(Duration::ZERO);
            let probe_timeout = remaining.min(Duration::from_secs(1));
            match self.wait_for_topology_connectivity(probe_timeout).await {
                Ok(()) => return Ok(()),
                Err(_) => {
                    self.bail_if_any_node_exited().await?;
                }
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .unwrap_or(Duration::ZERO);
            tokio::time::sleep(Duration::from_millis(100).min(remaining)).await;
        }
        match self
            .wait_for_topology_connectivity(Duration::from_millis(0))
            .await
        {
            Err(detail) => anyhow::bail!(
                "stabilize_app_topology_connectivity: topology did not stabilize \
                 within {timeout:?}: {detail} (repair: {cumulative_report})"
            ),
            Ok(()) => Ok(()),
        }
    }

    pub fn have_all_app_nodes_externalized(&self, ledger_seq: u32, max_spread: u32) -> bool {
        if self.running_apps.is_empty() {
            return false;
        }
        let seqs: Vec<u32> = self
            .running_apps
            .values()
            .map(|n| n.app.ledger_info().ledger_seq)
            .collect();
        seqs_within_spread(&seqs, ledger_seq, max_spread)
    }

    /// Returns the configured peer topology as a sorted map from each node ID
    /// to its list of neighbor node IDs. Useful for diagnostic logging in tests
    /// to verify partition setup.
    pub fn peer_topology(&self) -> BTreeMap<String, Vec<String>> {
        let mut topo = BTreeMap::new();
        for id in self.nodes.keys().chain(self.app_specs.keys()) {
            topo.entry(id.clone())
                .or_insert_with(|| self.loopback.neighbors(id));
        }
        topo
    }

    /// Advance a SimNode's ledger sequence and recompute its hash.
    fn advance_node(&mut self, node_id: &str, next_seq: u32) {
        let hash_input = format!("{}:{}", node_id, next_seq);
        if let Some(node) = self.nodes.get_mut(node_id) {
            node.ledger_seq = next_seq;
            node.ledger_hash = Hash256::hash(hash_input.as_bytes());
        }
    }

    /// Check whether `node_id` has an active link to any non-partitioned peer.
    fn has_connected_peer(&self, node_id: &str, candidates: &[String]) -> bool {
        candidates.iter().any(|other| {
            other != node_id
                && !self.loopback.is_partitioned(other)
                && self.loopback.link_active(node_id, other)
        })
    }

    /// Advance a single lightweight SimNode by one ledger (if possible).
    ///
    /// Returns `true` if the node advanced.
    /// Matches stellar-core `Simulation::crankNode(id, timeout)` — the
    /// timeout parameter is omitted because our lightweight SimNodes don't
    /// use a real event loop.
    pub fn crank_node(&mut self, node_id: &str) -> bool {
        if self.loopback.is_partitioned(node_id) {
            return false;
        }

        let current = match self.nodes.get(node_id) {
            Some(n) => n.ledger_seq,
            None => return false,
        };

        let max_seq = self
            .nodes
            .values()
            .filter(|n| !self.loopback.is_partitioned(&n.node_id))
            .map(|n| n.ledger_seq)
            .max()
            .unwrap_or(current);

        if current >= max_seq {
            return false;
        }

        let ids: Vec<String> = self.nodes.keys().cloned().collect();
        if self.has_connected_peer(node_id, &ids) {
            self.advance_node(node_id, current + 1);
            true
        } else {
            false
        }
    }

    /// Crank all lightweight SimNodes repeatedly for at most `duration`
    /// (wall-clock time).
    ///
    /// Matches stellar-core `Simulation::crankForAtMost(duration, finalCrank)`.
    pub async fn crank_for_at_most(&mut self, duration: Duration, final_crank: bool) {
        self.crank_loop(duration, true, final_crank).await;
    }

    /// Crank all lightweight SimNodes repeatedly for at least `duration`
    /// (wall-clock time).
    ///
    /// Matches stellar-core `Simulation::crankForAtLeast(duration, finalCrank)`.
    pub async fn crank_for_at_least(&mut self, duration: Duration, final_crank: bool) {
        self.crank_loop(duration, false, final_crank).await;
    }

    /// Shared crank loop.
    ///
    /// When `stop_when_idle` is true, the loop breaks early if no node did work
    /// (at-most semantics). Otherwise it runs until the deadline (at-least semantics).
    async fn crank_loop(&mut self, duration: Duration, stop_when_idle: bool, final_crank: bool) {
        let deadline = tokio::time::Instant::now() + duration;
        loop {
            let did_work = self.crank_all_nodes().await;
            if stop_when_idle && !did_work {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
        }
        if final_crank {
            let _ = self.crank_all_nodes().await;
        }
    }

    pub async fn crank_all_nodes(&mut self) -> bool {
        #[cfg(test)]
        {
            self.crank_count += 1;
        }
        if self.nodes.is_empty() {
            return false;
        }

        let mut did_work = false;
        let ids: Vec<String> = self.nodes.keys().cloned().collect();
        let max_seq = ids
            .iter()
            .filter_map(|id| self.nodes.get(id).map(|n| n.ledger_seq))
            .max()
            .unwrap_or(1);

        for id in &ids {
            if self.loopback.is_partitioned(id) {
                continue;
            }

            let current = self.nodes.get(id).map(|n| n.ledger_seq).unwrap_or(1);
            if current < max_seq && self.has_connected_peer(id, &ids) {
                self.advance_node(id, current + 1);
                did_work = true;
            }
        }

        if self.try_advance_non_partitioned(&ids, max_seq) {
            did_work = true;
        }

        did_work
    }

    /// Attempt to advance all non-partitioned, fully-caught-up nodes by one
    /// ledger if they are mutually connected.
    fn try_advance_non_partitioned(&mut self, ids: &[String], max_seq: u32) -> bool {
        let non_partitioned: Vec<String> = ids
            .iter()
            .filter(|id| !self.loopback.is_partitioned(id))
            .cloned()
            .collect();
        if non_partitioned.len() < 2 {
            return false;
        }
        let all_equal = non_partitioned
            .iter()
            .filter_map(|id| self.nodes.get(id).map(|n| n.ledger_seq))
            .all(|seq| seq == max_seq);
        if !all_equal {
            return false;
        }
        let connected = non_partitioned
            .iter()
            .all(|id| self.has_connected_peer(id, &non_partitioned));
        if !connected {
            return false;
        }
        let next = max_seq + 1;
        for id in &non_partitioned {
            self.advance_node(id, next);
        }
        true
    }

    pub async fn crank_until<P>(&mut self, predicate: P, timeout: Duration) -> anyhow::Result<()>
    where
        P: Fn(&Simulation) -> bool,
    {
        let mut elapsed = Duration::ZERO;
        while elapsed <= timeout {
            if predicate(self) {
                return Ok(());
            }
            let _ = self.crank_all_nodes().await;
            elapsed = elapsed.saturating_add(Duration::from_millis(100));
        }
        if predicate(self) {
            return Ok(());
        }
        anyhow::bail!("crank_until: predicate not satisfied within {timeout:?} (synthetic time)")
    }

    pub fn have_all_externalized(&self, ledger_seq: u32, max_spread: u32) -> bool {
        if self.nodes.is_empty() {
            return false;
        }
        let seqs: Vec<u32> = self
            .nodes
            .iter()
            .filter(|(id, _)| !self.loopback.is_partitioned(id))
            .map(|(_, n)| n.ledger_seq)
            .collect();
        seqs_within_spread(&seqs, ledger_seq, max_spread)
    }

    pub fn ledger_seq(&self, node_id: &str) -> u32 {
        self.nodes.get(node_id).map(|n| n.ledger_seq).unwrap_or(0)
    }

    pub fn node_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.nodes.keys().cloned().collect();
        ids.sort();
        ids
    }

    pub fn app_node_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.app_specs.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Check whether a specific node's overlay is started.
    ///
    /// Returns false if the node is not in `running_apps`.
    pub async fn is_app_overlay_started(&self, node_id: &str) -> bool {
        match self.running_apps.get(node_id) {
            Some(node) => node.app.is_overlay_started().await,
            None => false,
        }
    }

    pub fn app_spec_public_key(&self, node_id: &str) -> Option<String> {
        self.app_specs
            .get(node_id)
            .map(|spec| spec.secret_key.public_key().to_strkey())
            .or_else(|| {
                self.nodes
                    .get(node_id)
                    .map(|n| n.secret_key.public_key().to_strkey())
            })
    }

    pub fn all_links(&self) -> Vec<(String, String)> {
        self.loopback.links()
    }

    pub fn partition(&mut self, node_id: &str) {
        self.loopback.partition(node_id);
    }

    pub fn heal_partition(&mut self, node_id: &str) {
        self.loopback.heal_partition(node_id);
    }

    pub fn set_drop_prob(&mut self, a: &str, b: &str, prob: f64) {
        self.loopback.set_drop_prob(a, b, prob);
    }

    pub fn ledger_hashes(&self) -> Vec<Hash256> {
        let mut items: Vec<(&String, Hash256)> = self
            .nodes
            .iter()
            .map(|(id, n)| (id, n.ledger_hash))
            .collect();
        items.sort_by(|a, b| a.0.cmp(b.0));
        items.into_iter().map(|(_, h)| h).collect()
    }

    pub fn is_fully_connected(&self) -> bool {
        let ids = self.node_ids();
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                if !self.loopback.link_active(&ids[i], &ids[j]) {
                    return false;
                }
            }
        }
        true
    }

    pub fn generate_load_plan_for_app_nodes(
        &self,
        txs_per_step: usize,
        steps: usize,
        fee_bid: u32,
        amount: i64,
    ) -> Vec<LoadStep> {
        let config = GeneratedLoadConfig {
            accounts: self.app_node_ids(),
            txs_per_step,
            steps,
            fee_bid,
            amount,
            ..Default::default()
        };
        LoadGenerator::step_plan(&config)
    }

    pub async fn fund_app_accounts(&mut self, starting_balance: i64) -> anyhow::Result<usize> {
        let node_ids = self.app_node_ids();
        let mut submitted = 0usize;
        for node_id in node_ids {
            if self.app_account_sequences.contains_key(&node_id) {
                continue;
            }
            let tx =
                self.build_create_account_tx(&node_id, starting_balance, self.root_sequence)?;
            let result = self.submit_transaction_to_network(tx).await;
            if matches!(result, henyey_herder::TxQueueResult::Added) {
                submitted += 1;
                self.root_sequence += 1;
                self.app_account_sequences.insert(node_id, 1);
            }
        }
        Ok(submitted)
    }

    pub async fn submit_generated_load_step(&mut self, step: &LoadStep) -> anyhow::Result<usize> {
        let mut submitted = 0usize;
        for generated in &step.transactions {
            let Some(sequence) = self.app_account_sequences.get(&generated.source).copied() else {
                continue;
            };
            let tx = self.build_payment_tx(generated, sequence)?;
            let result = self.submit_transaction_to_network(tx).await;
            if matches!(result, henyey_herder::TxQueueResult::Added) {
                submitted += 1;
                if let Some(seq) = self.app_account_sequences.get_mut(&generated.source) {
                    *seq += 1;
                }
            }
        }
        Ok(submitted)
    }

    /// Spawn a node's run loop, insert into `running_apps`, and wait until
    /// ALL nodes (including the new one) have their overlay ready.
    ///
    /// Uses `wait_for_all_overlays_started` with `CrashScope::AllNodes`, so
    /// if any already-running node crashes during the wait, it's detected.
    ///
    /// **Failure semantics**: If the wait fails (timeout or crash detected),
    /// the newly inserted node is removed and shut down before returning Err.
    async fn spawn_insert_ready_node(
        &mut self,
        id: String,
        app: Arc<App>,
        data_dir: Arc<TempDir>,
        peer_port: u16,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        let running = Self::spawn_app_run_loop(app, data_dir, peer_port)?;
        self.running_apps.insert(id.clone(), running);
        if let Err(e) = self.wait_for_all_overlays_started(timeout).await {
            // Rollback: remove and shut down the node we just inserted.
            if let Some(node) = self.running_apps.remove(&id) {
                node.app.shutdown();
                let _ = join_or_abort(node.task.handle, ROLLBACK_SHUTDOWN_TIMEOUT).await;
            }
            return Err(e);
        }
        Ok(())
    }

    /// Bootstrap or restore a node, spawn its run loop, insert into
    /// `running_apps`, and wait for overlay readiness.
    ///
    /// Consumes a [`NodeStartupPlan`] so the bootstrap mode is locked to the
    /// same value used during database initialization.
    async fn insert_ready_node(
        &mut self,
        plan: NodeStartupPlan,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        let NodeStartupPlan {
            id,
            app,
            data_dir,
            peer_port,
            mode,
            pre_bound_listener,
        } = plan;

        // Install pre-bound TCP listener before wrapping in Arc.
        if let Some(listener) = pre_bound_listener {
            app.set_pre_bound_listener(listener);
        }

        let app = Self::wrap_app(app).await;

        match mode {
            BootstrapMode::Genesis => {
                app.bootstrap_from_db().await?;
                // Defense-in-depth: a fresh simulation node must start at ledger 1.
                let info = app.ledger_info();
                anyhow::ensure!(
                    info.ledger_seq == 1,
                    "SIMULATION STATE LEAK: node {id} bootstrapped at ledger_seq={}, \
                     expected 1 (genesis). Database: {:?}",
                    info.ledger_seq,
                    data_dir.path(),
                );
            }
            BootstrapMode::Restore => match app.load_last_known_ledger().await {
                Ok(henyey_app::RestoreResult::Restored) => {
                    let info = app.ledger_info();
                    tracing::info!(
                        lcl_seq = info.ledger_seq,
                        "Restored restarted node from disk"
                    );
                }
                Ok(henyey_app::RestoreResult::NoState) => {
                    anyhow::bail!(
                        "Restore mode: no persisted state in {:?}. \
                             Use BootstrapMode::Genesis for fresh nodes.",
                        data_dir.path(),
                    );
                }
                Err(e) => {
                    return Err(
                        e.context("Failed to restore restarted node — persisted state is corrupt")
                    );
                }
            },
        }

        self.spawn_insert_ready_node(id, app, data_dir, peer_port, timeout)
            .await
    }

    /// Wrap a raw `App` in an `Arc`, set its self-reference, and configure
    /// it for simulation (skip fee balance checks).
    async fn wrap_app(app: App) -> Arc<App> {
        let app = Arc::new(app);
        app.set_self_arc().await;
        // All simulation transactions are loadgen txs — skip fee balance checks
        // to match stellar-core's `isLoadgenTx` bypass in TransactionQueue::canAdd().
        app.set_skip_fee_balance_check(true);
        app
    }

    /// Low-level: spawn the app run loop and return a RunningAppNode.
    ///
    /// **Does NOT wait for overlay readiness.** Use `spawn_insert_ready_node`
    /// (or its bootstrap/restore variants) for normal node startup. This
    /// exists only as the building block for those higher-level methods.
    fn spawn_app_run_loop(
        app: Arc<App>,
        data_dir: Arc<TempDir>,
        peer_port: u16,
    ) -> anyhow::Result<RunningAppNode> {
        let app_clone = Arc::clone(&app);
        let status = Arc::new(tokio::sync::RwLock::new(None));
        let status_clone = Arc::clone(&status);
        let handle = tokio::spawn(async move {
            let result = app_clone
                .run(henyey_app::FallbackCatchup::Allow)
                .await
                .map_err(|e| e.to_string());
            *status_clone.write().await = Some(result.clone());
            result.map_err(anyhow::Error::msg)
        });

        Ok(RunningAppNode {
            app,
            task: NodeTaskHandle { handle, status },
            _data_dir: data_dir,
            peer_port,
        })
    }

    /// Build a [`NodeStartupPlan`] from an [`AppNodeSpec`].
    ///
    /// The `mode` parameter controls both genesis DB initialization and the
    /// post-construction bootstrap path, binding both decisions into the
    /// returned plan so they cannot diverge.
    async fn build_node_startup_plan(
        &self,
        spec: &AppNodeSpec,
        port_map: &HashMap<String, u16>,
        data_dir: Arc<TempDir>,
        overlay_connection_factory: Arc<dyn ConnectionFactory>,
        mode: BootstrapMode,
        pre_bound_listener: Option<henyey_overlay::Listener>,
    ) -> anyhow::Result<NodeStartupPlan> {
        let peer_port = *port_map
            .get(&spec.node_id)
            .with_context(|| format!("missing peer port for {}", spec.node_id))?;

        let mut config = ConfigBuilder::simulation()
            .node_name(spec.node_id.clone())
            .node_seed(spec.secret_key.to_strkey())
            .validator(spec.is_validator)
            .database_path(data_dir.path().join("node.db"))
            .bucket_directory(data_dir.path().join("buckets"))
            .peer_port(peer_port)
            .build();

        config.network.passphrase = self.network_passphrase.clone();
        config.node.quorum_set = spec.quorum_set.clone();
        config.node.manual_close = spec.manual_close;
        config.overlay.known_peers = self.known_peers_for(&spec.node_id, port_map);
        config.overlay.preferred_peers.clear();
        config.overlay.target_outbound_peers = config.overlay.known_peers.len();
        config.overlay.max_outbound_peers = config.overlay.known_peers.len().max(1);
        config.overlay.max_inbound_peers = self.app_specs.len().max(1);
        config.http.enabled = false;
        config.compat_http.enabled = false;

        // For TCP simulation, use a short connect timeout.  Localhost TCP
        // connects in <1ms; the 10s production default wastes retry budget
        // when a peer hasn't started its listener yet.
        if matches!(self.mode, SimulationMode::OverTcp) {
            config.overlay.connect_timeout_secs = Some(2);
        }

        if let Some(parent) = config.database.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::create_dir_all(&config.buckets.directory)?;

        if matches!(mode, BootstrapMode::Genesis) {
            initialize_genesis_ledger(&config, &self.network_passphrase)?;
        }

        let app = App::new_with_clock_and_connection_factory(
            config,
            Arc::new(RealClock),
            overlay_connection_factory,
        )
        .await?;

        Ok(NodeStartupPlan {
            id: spec.node_id.clone(),
            app,
            data_dir,
            peer_port,
            mode,
            pre_bound_listener,
        })
    }

    fn known_peers_for(&self, node_id: &str, port_map: &HashMap<String, u16>) -> Vec<PeerAddress> {
        self.loopback
            .neighbors(node_id)
            .into_iter()
            .filter_map(|neighbor| {
                port_map
                    .get(&neighbor)
                    .map(|port| PeerAddress::new("127.0.0.1", *port))
            })
            .collect()
    }

    async fn submit_transaction_to_network(
        &self,
        tx: TransactionEnvelope,
    ) -> henyey_herder::TxQueueResult {
        let mut ids: Vec<String> = self.running_apps.keys().cloned().collect();
        ids.sort();
        let app = self
            .running_apps
            .get(ids.first().expect("running app node exists"))
            .expect("running app node present");
        app.app.submit_transaction(tx).await
    }

    fn build_create_account_tx(
        &self,
        destination_node_id: &str,
        starting_balance: i64,
        sequence: i64,
    ) -> anyhow::Result<TransactionEnvelope> {
        let destination = self.account_id_for_node(destination_node_id)?;
        let operation = Operation {
            source_account: None,
            body: OperationBody::CreateAccount(CreateAccountOp {
                destination,
                starting_balance,
            }),
        };
        let tx = Transaction {
            source_account: self.root_muxed_account(),
            fee: 100,
            seq_num: SequenceNumber(sequence),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![operation].try_into().unwrap_or_default(),
            ext: TransactionExt::V0,
        };
        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: VecM::default(),
        });
        self.sign_transaction_envelope(envelope, &root_secret(&self.network_passphrase))
    }

    fn build_payment_tx(
        &self,
        generated: &GeneratedTransaction,
        sequence: i64,
    ) -> anyhow::Result<TransactionEnvelope> {
        let source_secret = self.secret_for_node(&generated.source)?;
        let destination = self.destination_for_node(&generated.destination)?;
        let operation = Operation {
            source_account: None,
            body: OperationBody::Payment(stellar_xdr::curr::PaymentOp {
                destination,
                asset: Asset::Native,
                amount: generated.amount,
            }),
        };
        let tx = Transaction {
            source_account: MuxedAccount::Ed25519(Uint256(*source_secret.public_key().as_bytes())),
            fee: generated.fee_bid,
            seq_num: SequenceNumber(sequence),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![operation].try_into().unwrap_or_default(),
            ext: TransactionExt::V0,
        };
        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: VecM::default(),
        });
        self.sign_transaction_envelope(envelope, &source_secret)
    }

    fn sign_transaction_envelope(
        &self,
        mut envelope: TransactionEnvelope,
        secret: &SecretKey,
    ) -> anyhow::Result<TransactionEnvelope> {
        loadgen_soroban::sign_envelope(&mut envelope, secret, &self.network_passphrase)?;
        Ok(envelope)
    }

    fn destination_for_node(&self, node_id: &str) -> anyhow::Result<MuxedAccount> {
        let secret = self.secret_for_node(node_id)?;
        Ok(MuxedAccount::Ed25519(Uint256(
            *secret.public_key().as_bytes(),
        )))
    }

    fn account_id_for_node(&self, node_id: &str) -> anyhow::Result<AccountId> {
        let secret = self.secret_for_node(node_id)?;
        Ok(AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
            *secret.public_key().as_bytes(),
        ))))
    }

    pub fn secret_for_node(&self, node_id: &str) -> anyhow::Result<SecretKey> {
        self.app_specs
            .get(node_id)
            .map(|spec| spec.secret_key.clone())
            .or_else(|| self.nodes.get(node_id).map(|node| node.secret_key.clone()))
            .with_context(|| format!("missing secret key for node {}", node_id))
    }

    fn root_muxed_account(&self) -> MuxedAccount {
        let secret = root_secret(&self.network_passphrase);
        MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes()))
    }
}

fn allocate_port_block(width: u16) -> u16 {
    static NEXT_PORT: AtomicU16 = AtomicU16::new(23000);
    NEXT_PORT.fetch_add(width.max(1), Ordering::Relaxed)
}

fn add_seeded_node(
    sim: &mut Simulation,
    node_id: impl Into<String>,
    seed_material: impl AsRef<[u8]>,
) {
    let node_id = node_id.into();
    let seed = Hash256::hash(seed_material.as_ref());
    let secret_key = SecretKey::from_seed(&seed.0);
    sim.add_node(node_id, secret_key);
}

pub struct Topologies;

impl Topologies {
    pub fn core3(mode: SimulationMode) -> Simulation {
        Self::core(3, mode)
    }

    pub fn core(n: usize, mode: SimulationMode) -> Simulation {
        let mut sim = Simulation::new(mode);
        let mut ids = Vec::with_capacity(n);
        for i in 0..n {
            let id = format!("node{}", i);
            add_seeded_node(&mut sim, id.clone(), format!("SIM_NODE_SEED_{}", i));
            ids.push(id);
        }

        for (i, id_i) in ids.iter().enumerate() {
            for id_j in &ids[i + 1..] {
                sim.add_pending_connection(id_i.clone(), id_j.clone());
            }
        }

        sim
    }

    pub fn pair(mode: SimulationMode) -> Simulation {
        Self::core(2, mode)
    }

    pub fn cycle4(mode: SimulationMode) -> Simulation {
        Self::cycle(4, mode)
    }

    pub fn cycle(n: usize, mode: SimulationMode) -> Simulation {
        let mut sim = Simulation::new(mode);
        let mut ids = Vec::with_capacity(n);
        for i in 0..n {
            let id = format!("node{}", i);
            add_seeded_node(&mut sim, id.clone(), format!("SIM_NODE_SEED_{}", i));
            ids.push(id);
        }

        if n >= 2 {
            for (id_i, id_j) in ids.iter().zip(ids.iter().cycle().skip(1)) {
                sim.add_pending_connection(id_i.clone(), id_j.clone());
            }
        }

        sim
    }

    pub fn branchedcycle(n: usize, mode: SimulationMode) -> Simulation {
        let mut sim = Self::cycle(n, mode);
        let ids = sim.node_ids();
        if n >= 4 {
            for (i, id_i) in ids.iter().enumerate() {
                let other = (i + (n / 2)) % n;
                if i != other {
                    sim.add_pending_connection(id_i.clone(), ids[other].clone());
                }
            }
        }
        sim
    }

    pub fn hierarchical_quorum(n_branches: usize, mode: SimulationMode) -> Simulation {
        let mut sim = Self::core(4, mode);
        for branch in 0..n_branches {
            let id = format!("branch{}", branch);
            add_seeded_node(&mut sim, id.clone(), format!("SIM_BRANCH_SEED_{}", branch));
            sim.add_pending_connection(id, format!("node{}", branch % 4));
            sim.add_pending_connection(
                format!("branch{}", branch),
                format!("node{}", (branch + 1) % 4),
            );
        }
        sim
    }

    pub fn hierarchical_quorum_simplified(
        core_size: usize,
        outer_nodes: usize,
        mode: SimulationMode,
    ) -> Simulation {
        let mut sim = Self::core(core_size, mode);
        for outer in 0..outer_nodes {
            let id = format!("outer{}", outer);
            add_seeded_node(&mut sim, id.clone(), format!("SIM_OUTER_SEED_{}", outer));
            sim.add_pending_connection(id.clone(), format!("node{}", outer % core_size.max(1)));
            if core_size > 1 {
                sim.add_pending_connection(id, format!("node{}", (outer + 1) % core_size));
            }
        }
        sim
    }

    pub fn custom_a(mode: SimulationMode) -> Simulation {
        let mut sim = Simulation::new(mode);
        for id in ["A", "B", "C", "T", "I", "E", "S"] {
            add_seeded_node(&mut sim, id, format!("SIM_CUSTOM_A_{}", id));
        }
        for (a, b) in [
            ("A", "B"),
            ("A", "C"),
            ("B", "C"),
            ("A", "T"),
            ("B", "E"),
            ("C", "S"),
            ("T", "E"),
            ("E", "S"),
        ] {
            sim.add_pending_connection(a, b);
        }
        sim.partition("I");
        sim
    }

    pub fn asymmetric(mode: SimulationMode) -> Simulation {
        let mut sim = Self::core(4, mode);
        for extra in 0..3 {
            let id = format!("tier1_{}", extra);
            add_seeded_node(&mut sim, id.clone(), format!("SIM_ASYM_SEED_{}", extra));
            sim.add_pending_connection(id, "node0");
        }
        sim
    }

    pub fn separate(mode: SimulationMode) -> Simulation {
        let mut sim = Simulation::new(mode);
        for i in 0..4 {
            let id = format!("node{}", i);
            add_seeded_node(&mut sim, id, format!("SIM_NODE_SEED_{}", i));
        }

        sim.add_pending_connection("node0", "node1");
        sim.add_pending_connection("node2", "node3");
        sim
    }

    /// Create a split topology with `n` validators and `watchers` watcher
    /// nodes, where watchers observe but do not participate in consensus.
    ///
    /// Matches stellar-core `Topologies::separate(n, watchers, mode)` overload
    /// which adds extra (non-validator) nodes that connect to the first validator
    /// partition.
    pub fn separate_with_watchers(n: usize, watchers: usize, mode: SimulationMode) -> Simulation {
        let mut sim = Simulation::new(mode);

        // Create validator nodes in two partitions.
        let half = n / 2;
        for i in 0..n {
            let id = format!("node{}", i);
            add_seeded_node(&mut sim, id, format!("SIM_NODE_SEED_{}", i));
        }

        // Partition A: nodes 0..half, Partition B: nodes half..n
        for i in 0..half {
            for j in (i + 1)..half {
                sim.add_pending_connection(format!("node{}", i), format!("node{}", j));
            }
        }
        for i in half..n {
            for j in (i + 1)..n {
                sim.add_pending_connection(format!("node{}", i), format!("node{}", j));
            }
        }

        // Add watcher nodes connected to partition A.
        for w in 0..watchers {
            let id = format!("watcher{}", w);
            add_seeded_node(&mut sim, id.clone(), format!("SIM_WATCHER_SEED_{}", w));
            // Connect watcher to all nodes in partition A.
            for i in 0..half {
                sim.add_pending_connection(id.clone(), format!("node{}", i));
            }
        }

        sim
    }
}

pub fn initialize_genesis_ledger(
    config: &AppConfig,
    network_passphrase: &str,
) -> anyhow::Result<()> {
    let db = henyey_db::Database::open(&config.database.path)?;
    henyey_app::initialize_genesis(
        &db,
        None,
        network_passphrase,
        config.testing.genesis_test_account_count,
    )
}

fn root_secret(network_passphrase: &str) -> SecretKey {
    let network_id = NetworkId::from_passphrase(network_passphrase);
    SecretKey::from_seed(network_id.as_bytes())
}

impl Drop for Simulation {
    fn drop(&mut self) {
        if !self.running_apps.is_empty() {
            tracing::warn!(
                count = self.running_apps.len(),
                "Simulation::drop: running apps not stopped, forcing shutdown"
            );
            for (id, node) in &self.running_apps {
                node.app.shutdown();
                tracing::debug!(node = %id, "Simulation::drop: shutdown requested");
            }
            for (_id, node) in self.running_apps.drain() {
                node.task.handle.abort();
            }
        }

        #[cfg(test)]
        for (_id, node) in self.test_nodes.drain() {
            node.handle.abort();
        }
    }
}

#[cfg(test)]
impl Simulation {
    /// Insert a mock running node whose task is the given handle.
    ///
    /// Only `handle` and `status` are functional — no real `App` is created.
    /// Used by `poll.rs` unit tests for lightweight crash-detection testing.
    pub(crate) fn insert_test_node(
        &mut self,
        node_id: impl Into<String>,
        handle: JoinHandle<anyhow::Result<()>>,
        status: Option<Result<(), String>>,
    ) {
        self.test_nodes.insert(
            node_id.into(),
            NodeTaskHandle {
                handle,
                status: Arc::new(tokio::sync::RwLock::new(status)),
            },
        );
    }

    /// Insert a mock running node into `running_apps` (not `test_nodes`).
    ///
    /// Used by defense-layer tests to exercise `Simulation::Drop` cleanup
    /// without starting a real overlay or run loop.
    pub(crate) fn insert_running_test_node(
        &mut self,
        node_id: impl Into<String>,
        app: Arc<App>,
        handle: JoinHandle<anyhow::Result<()>>,
        data_dir: Arc<TempDir>,
        peer_port: u16,
    ) {
        self.running_apps.insert(
            node_id.into(),
            RunningAppNode {
                app,
                task: NodeTaskHandle {
                    handle,
                    status: Arc::new(tokio::sync::RwLock::new(None)),
                },
                _data_dir: data_dir,
                peer_port,
            },
        );
    }
}

#[cfg(test)]
mod crank_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// Create a simulation with two connected lightweight nodes "A" and "B",
    /// both starting at ledger 1. One `crank_all_nodes()` call advances both
    /// from ledger N to N+1 via `try_advance_non_partitioned`.
    fn sim_with_two_nodes() -> Simulation {
        let mut sim = Simulation::new(SimulationMode::OverLoopback);
        sim.add_node("A", SecretKey::from_seed(&[1u8; 32]));
        sim.add_node("B", SecretKey::from_seed(&[2u8; 32]));
        sim.add_pending_connection("A", "B");
        sim
    }

    // ==================================================================
    // crank_until tests
    // ==================================================================

    #[tokio::test(start_paused = true)]
    async fn test_crank_until_immediate_satisfaction() {
        let mut sim = Simulation::new(SimulationMode::OverLoopback);
        let counter = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&counter);

        let result = sim
            .crank_until(
                |_| {
                    c.fetch_add(1, Ordering::SeqCst);
                    true
                },
                Duration::from_secs(5),
            )
            .await;

        assert!(result.is_ok());
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn test_crank_until_satisfaction_after_cranks() {
        let mut sim = sim_with_two_nodes();
        let counter = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&counter);

        let result = sim
            .crank_until(
                |sim| {
                    c.fetch_add(1, Ordering::SeqCst);
                    sim.nodes.get("A").map_or(false, |n| n.ledger_seq >= 3)
                },
                Duration::from_secs(10),
            )
            .await;

        assert!(result.is_ok());
        // Check 1: ledger=1 (false), crank→2
        // Check 2: ledger=2 (false), crank→3
        // Check 3: ledger=3 (true) → Ok
        assert_eq!(counter.load(Ordering::SeqCst), 3);
        assert_eq!(sim.nodes["A"].ledger_seq, 3);
        assert_eq!(sim.nodes["B"].ledger_seq, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn test_crank_until_zero_timeout_satisfied() {
        let mut sim = Simulation::new(SimulationMode::OverLoopback);
        let counter = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&counter);

        let result = sim
            .crank_until(
                |_| {
                    c.fetch_add(1, Ordering::SeqCst);
                    true
                },
                Duration::ZERO,
            )
            .await;

        assert!(result.is_ok());
        // while 0 <= 0 → enters loop, predicate true → returns Ok
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn test_crank_until_zero_timeout_unsatisfied() {
        let mut sim = Simulation::new(SimulationMode::OverLoopback);
        let counter = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&counter);

        let result = sim
            .crank_until(
                |_| {
                    c.fetch_add(1, Ordering::SeqCst);
                    false
                },
                Duration::ZERO,
            )
            .await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("crank_until: predicate not satisfied"));
        // Loop: check false (elapsed=0), crank, elapsed=100ms > 0 → exit.
        // Post-loop: check false → bail.
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn test_crank_until_success_at_elapsed_eq_timeout() {
        let mut sim = Simulation::new(SimulationMode::OverLoopback);
        let counter = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&counter);

        // timeout=200ms. Predicate returns true on 3rd call.
        // Iteration 1: elapsed=0 (<=200ms), check false, crank, elapsed=100ms
        // Iteration 2: elapsed=100ms (<=200ms), check false, crank, elapsed=200ms
        // Iteration 3: elapsed=200ms (<=200ms), check TRUE → Ok
        // This proves the `<=` boundary (not `<`).
        let result = sim
            .crank_until(
                |_| {
                    let n = c.fetch_add(1, Ordering::SeqCst) + 1;
                    n >= 3
                },
                Duration::from_millis(200),
            )
            .await;

        assert!(result.is_ok());
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn test_crank_until_boundary_post_check_saves() {
        let mut sim = Simulation::new(SimulationMode::OverLoopback);
        let counter = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&counter);

        // timeout=100ms. Predicate returns true on 3rd call.
        // Iteration 1: elapsed=0 (<=100ms), check false (call 1), crank, elapsed=100ms
        // Iteration 2: elapsed=100ms (<=100ms), check false (call 2), crank, elapsed=200ms
        // 200ms > 100ms → loop exits.
        // Post-loop re-check: check true (call 3) → Ok
        let result = sim
            .crank_until(
                |_| {
                    let n = c.fetch_add(1, Ordering::SeqCst) + 1;
                    n >= 3
                },
                Duration::from_millis(100),
            )
            .await;

        assert!(result.is_ok());
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn test_crank_until_timeout_error() {
        let mut sim = Simulation::new(SimulationMode::OverLoopback);
        let counter = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&counter);

        let result = sim
            .crank_until(
                |_| {
                    c.fetch_add(1, Ordering::SeqCst);
                    false
                },
                Duration::from_millis(500),
            )
            .await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("crank_until: predicate not satisfied within 500ms"));
        // In-loop: elapsed 0, 100, 200, 300, 400, 500ms → 6 checks.
        // Post-loop: 1 check. Total = 7.
        assert_eq!(counter.load(Ordering::SeqCst), 7);
    }

    // ==================================================================
    // crank_for_at_most tests
    // ==================================================================

    #[tokio::test(start_paused = true)]
    async fn test_crank_for_at_most_idle_exits_early() {
        let mut sim = Simulation::new(SimulationMode::OverLoopback);
        sim.add_node("A", SecretKey::from_seed(&[1u8; 32]));

        // Single node, no peers → crank_all_nodes returns false → stop_when_idle breaks.
        sim.crank_for_at_most(Duration::from_secs(999), false).await;

        assert_eq!(sim.nodes["A"].ledger_seq, 1);
        // Loop: 1 crank (idle → break), no final_crank.
        assert_eq!(sim.crank_count, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn test_crank_for_at_most_final_crank_after_idle() {
        let mut sim = Simulation::new(SimulationMode::OverLoopback);
        sim.add_node("A", SecretKey::from_seed(&[1u8; 32]));

        // Idle exit, then final_crank triggers one extra crank_all_nodes.
        sim.crank_for_at_most(Duration::from_secs(999), true).await;

        assert_eq!(sim.nodes["A"].ledger_seq, 1);
        // Loop: 1 crank (idle → break) + 1 final_crank = 2.
        assert_eq!(sim.crank_count, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn test_crank_for_at_most_zero_duration() {
        let mut sim = sim_with_two_nodes();

        // Loop: crank advances both to 2 (did_work=true),
        // deadline=start+0=start, now >= deadline → breaks.
        sim.crank_for_at_most(Duration::ZERO, false).await;

        assert_eq!(sim.nodes["A"].ledger_seq, 2);
        assert_eq!(sim.nodes["B"].ledger_seq, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn test_crank_for_at_most_zero_duration_with_final_crank() {
        let mut sim = sim_with_two_nodes();

        // Loop: crank → ledger 2, deadline hit → breaks.
        // Final crank → ledger 3.
        sim.crank_for_at_most(Duration::ZERO, true).await;

        assert_eq!(sim.nodes["A"].ledger_seq, 3);
        assert_eq!(sim.nodes["B"].ledger_seq, 3);
    }

    // ==================================================================
    // crank_for_at_least tests
    // ==================================================================

    #[tokio::test(start_paused = true)]
    async fn test_crank_for_at_least_zero_duration_idle() {
        let mut sim = Simulation::new(SimulationMode::OverLoopback);
        sim.add_node("A", SecretKey::from_seed(&[1u8; 32]));

        // No idle exit (stop_when_idle=false), deadline=start → breaks after one crank.
        sim.crank_for_at_least(Duration::ZERO, false).await;

        assert_eq!(sim.nodes["A"].ledger_seq, 1);
        // Loop: 1 crank (no work, no idle exit, deadline hit → break), no final_crank.
        assert_eq!(sim.crank_count, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn test_crank_for_at_least_zero_duration_idle_final_crank() {
        let mut sim = Simulation::new(SimulationMode::OverLoopback);
        sim.add_node("A", SecretKey::from_seed(&[1u8; 32]));

        // Idle + final crank.
        sim.crank_for_at_least(Duration::ZERO, true).await;

        assert_eq!(sim.nodes["A"].ledger_seq, 1);
        // Loop: 1 crank + 1 final_crank = 2.
        assert_eq!(sim.crank_count, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn test_crank_for_at_least_zero_duration_workful() {
        let mut sim = sim_with_two_nodes();

        // Loop: crank → ledger 2 (did_work=true), no idle exit, deadline hit → breaks.
        sim.crank_for_at_least(Duration::ZERO, false).await;

        assert_eq!(sim.nodes["A"].ledger_seq, 2);
        assert_eq!(sim.nodes["B"].ledger_seq, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn test_crank_for_at_least_zero_duration_workful_final_crank() {
        let mut sim = sim_with_two_nodes();

        // Loop: crank → ledger 2, deadline hit → breaks.
        // Final crank → ledger 3.
        sim.crank_for_at_least(Duration::ZERO, true).await;

        assert_eq!(sim.nodes["A"].ledger_seq, 3);
        assert_eq!(sim.nodes["B"].ledger_seq, 3);
    }
}

#[cfg(test)]
mod genesis_freshness_tests {
    use henyey_app::config::ConfigBuilder;
    use henyey_db::queries::StateQueries;

    use super::initialize_genesis_ledger;

    #[test]
    fn test_genesis_init_rejects_prepopulated_database() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("node.db");
        let db = henyey_db::Database::open(&db_path).unwrap();
        db.with_connection(|conn| {
            conn.set_last_closed_ledger(999)?;
            Ok::<_, henyey_db::DbError>(())
        })
        .unwrap();
        drop(db);

        let config = ConfigBuilder::new()
            .database_path(&db_path)
            .bucket_directory(dir.path().join("buckets"))
            .build();

        let result = initialize_genesis_ledger(&config, "Test SDF Network ; September 2015");
        assert!(result.is_err(), "should reject pre-populated database");
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Cannot initialize genesis"), "error: {err}");
    }

    #[test]
    fn test_genesis_init_rejects_lcl_zero() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("node.db");
        let db = henyey_db::Database::open(&db_path).unwrap();
        db.with_connection(|conn| {
            conn.set_last_closed_ledger(0)?;
            Ok::<_, henyey_db::DbError>(())
        })
        .unwrap();
        drop(db);

        let config = ConfigBuilder::new()
            .database_path(&db_path)
            .bucket_directory(dir.path().join("buckets"))
            .build();

        let result = initialize_genesis_ledger(&config, "Test SDF Network ; September 2015");
        assert!(result.is_err(), "should reject database with LCL=0");
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Cannot initialize genesis"), "error: {err}");
    }

    #[test]
    fn test_genesis_init_rejects_stale_headers_without_lcl() {
        use henyey_db::queries::LedgerQueries;
        use stellar_xdr::curr::{
            Hash, LedgerHeader, LedgerHeaderExt, StellarValue, StellarValueExt, TimePoint, VecM,
        };

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("node.db");
        let db = henyey_db::Database::open(&db_path).unwrap();

        // Write a stale ledger header without setting LCL.
        let header = LedgerHeader {
            ledger_version: 0,
            previous_ledger_hash: Hash([0u8; 32]),
            scp_value: StellarValue {
                tx_set_hash: Hash([0u8; 32]),
                close_time: TimePoint(0),
                upgrades: VecM::default(),
                ext: StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash([0u8; 32]),
            bucket_list_hash: Hash([0u8; 32]),
            ledger_seq: 42,
            total_coins: 0,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 100_000_000,
            max_tx_set_size: 100,
            skip_list: std::array::from_fn(|_| Hash([0u8; 32])),
            ext: LedgerHeaderExt::V0,
        };
        let header_xdr =
            stellar_xdr::curr::WriteXdr::to_xdr(&header, stellar_xdr::curr::Limits::none())
                .unwrap();
        db.with_connection(|conn| {
            conn.store_ledger_header(&header, &header_xdr)?;
            Ok::<_, henyey_db::DbError>(())
        })
        .unwrap();
        drop(db);

        let config = ConfigBuilder::new()
            .database_path(&db_path)
            .bucket_directory(dir.path().join("buckets"))
            .build();

        let result = initialize_genesis_ledger(&config, "Test SDF Network ; September 2015");
        assert!(
            result.is_err(),
            "should reject database with stale headers but no LCL"
        );
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Cannot initialize genesis"), "error: {err}");
    }

    #[test]
    fn test_genesis_init_succeeds_on_empty_database() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("node.db");
        // Create an empty database (schema only, no data).
        let db = henyey_db::Database::open(&db_path).unwrap();
        drop(db);

        let config = ConfigBuilder::new()
            .database_path(&db_path)
            .bucket_directory(dir.path().join("buckets"))
            .build();

        let result = initialize_genesis_ledger(&config, "Test SDF Network ; September 2015");
        assert!(
            result.is_ok(),
            "should succeed on empty database: {result:?}"
        );
    }
}

#[cfg(test)]
mod defense_layer_tests {
    use super::*;
    use henyey_app::config::ConfigBuilder;
    use henyey_clock::RealClock;
    use henyey_db::queries::{LedgerQueries, StateQueries};
    use henyey_overlay::LoopbackConnectionFactory;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    /// Layer 2: `insert_ready_node` with `BootstrapMode::Genesis` rejects a
    /// database seeded at ledger_seq != 1.
    ///
    /// Fixture: run `initialize_genesis_ledger` to produce a valid genesis DB,
    /// then mutate the stored LCL and header to seq 5. This bypasses Layer 1
    /// (pre-genesis emptiness check) but triggers Layer 2 (post-bootstrap
    /// `ledger_seq == 1` assertion).
    #[tokio::test]
    async fn test_bootstrap_rejects_seeded_db() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("node.db");
        let bucket_dir = dir.path().join("buckets");
        std::fs::create_dir_all(&bucket_dir).unwrap();

        let passphrase = "Test SDF Network ; September 2015";

        // Step 1: Initialize a valid genesis DB at LCL=1.
        let config = ConfigBuilder::simulation()
            .database_path(&db_path)
            .bucket_directory(&bucket_dir)
            .build();
        initialize_genesis_ledger(&config, passphrase).unwrap();

        // Step 2: Mutate the DB — move LCL to 5 and write a header at seq 5.
        {
            let db = henyey_db::Database::open(&db_path).unwrap();
            db.with_connection(|conn| {
                let header = conn
                    .load_ledger_header(1)
                    .expect("load genesis header")
                    .expect("genesis header exists");

                // Clone header with ledger_seq = 5
                let mut seeded_header = header;
                seeded_header.ledger_seq = 5;
                let seeded_xdr = stellar_xdr::curr::WriteXdr::to_xdr(
                    &seeded_header,
                    stellar_xdr::curr::Limits::none(),
                )
                .unwrap();

                conn.store_ledger_header(&seeded_header, &seeded_xdr)
                    .expect("store seeded header");
                conn.set_last_closed_ledger(5).expect("set LCL to 5");
                Ok::<_, henyey_db::DbError>(())
            })
            .unwrap();
        }

        // Step 3: Build a Simulation with one app node.
        let mut sim = Simulation::new(SimulationMode::OverLoopback);
        let secret = SecretKey::from_seed(&[42u8; 32]);
        let node_id = "seeded-node";
        sim.add_node(node_id, secret.clone());
        sim.populate_app_nodes_from_existing(100);

        // Give the spec the pre-seeded data_dir.
        let data_dir = Arc::new(dir);
        sim.app_specs.get_mut(node_id).unwrap().data_dir = Some(Arc::clone(&data_dir));

        // Step 4: Build a plan with BootstrapMode::Restore (bypasses Layer 1
        // genesis init) but still use Genesis mode for insert_ready_node to
        // trigger the ledger_seq == 1 check.
        let port_map: HashMap<String, u16> = [(node_id.to_string(), 19000)].into();
        let conn_factory: Arc<dyn ConnectionFactory> =
            Arc::new(LoopbackConnectionFactory::default());

        let plan = sim
            .build_node_startup_plan(
                &sim.app_specs[node_id].clone(),
                &port_map,
                data_dir,
                conn_factory,
                BootstrapMode::Restore, // skip Layer 1 genesis init
                None,
            )
            .await
            .expect("build node startup plan");

        // Override the mode to Genesis to trigger the ledger_seq == 1 check.
        let plan = NodeStartupPlan {
            mode: BootstrapMode::Genesis,
            ..plan
        };

        // Step 5: Call insert_ready_node — Layer 2 should reject it.
        let result = sim.insert_ready_node(plan, Duration::from_secs(5)).await;

        assert!(result.is_err(), "Layer 2 should reject seeded DB");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("SIMULATION STATE LEAK"),
            "error should mention STATE LEAK: {err}"
        );
    }

    /// `insert_ready_node` with `BootstrapMode::Restore` on an empty database
    /// produces a clear error instead of silently failing.
    #[tokio::test]
    async fn test_restore_on_empty_db_errors() {
        let dir = tempfile::tempdir().unwrap();
        let _db_path = dir.path().join("node.db");
        let bucket_dir = dir.path().join("buckets");
        std::fs::create_dir_all(&bucket_dir).unwrap();

        // Build a simulation with one app node.
        let mut sim = Simulation::new(SimulationMode::OverLoopback);
        let secret = SecretKey::from_seed(&[99u8; 32]);
        let node_id = "empty-node";
        sim.add_node(node_id, secret.clone());
        sim.populate_app_nodes_from_existing(100);

        let data_dir = Arc::new(dir);
        sim.app_specs.get_mut(node_id).unwrap().data_dir = Some(Arc::clone(&data_dir));

        let port_map: HashMap<String, u16> = [(node_id.to_string(), 19001)].into();
        let conn_factory: Arc<dyn ConnectionFactory> =
            Arc::new(LoopbackConnectionFactory::default());

        // Build a plan with BootstrapMode::Restore on an empty DB.
        let plan = sim
            .build_node_startup_plan(
                &sim.app_specs[node_id].clone(),
                &port_map,
                data_dir,
                conn_factory,
                BootstrapMode::Restore,
                None,
            )
            .await
            .expect("build node startup plan");

        // insert_ready_node should fail with a clear error.
        let result = sim.insert_ready_node(plan, Duration::from_secs(5)).await;

        assert!(result.is_err(), "Restore on empty DB should error");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Restore mode: no persisted state"),
            "error should mention restore mode: {err}"
        );
    }

    /// Layer 3: Dropping a `Simulation` with running apps forces shutdown and
    /// aborts their tasks.
    ///
    /// Inserts two mock `RunningAppNode` entries (via `insert_running_test_node`)
    /// and verifies that both apps receive shutdown signals and both task handles
    /// are aborted when the `Simulation` is dropped.
    #[tokio::test]
    async fn test_drop_shuts_down_leaked_nodes() {
        let mut abort_handles = Vec::new();
        let mut shutdown_rxs = Vec::new();

        {
            let mut sim = Simulation::new(SimulationMode::OverLoopback);

            for i in 0..2 {
                let dir = tempfile::tempdir().unwrap();
                let db_path = dir.path().join("node.db");
                let bucket_dir = dir.path().join("buckets");
                std::fs::create_dir_all(&bucket_dir).unwrap();

                let config = ConfigBuilder::simulation()
                    .database_path(&db_path)
                    .bucket_directory(&bucket_dir)
                    .node_name(format!("drop-test-{i}"))
                    .build();

                let conn_factory: Arc<dyn ConnectionFactory> =
                    Arc::new(LoopbackConnectionFactory::default());
                let app = App::new_with_clock_and_connection_factory(
                    config,
                    Arc::new(RealClock),
                    conn_factory,
                )
                .await
                .expect("create test app");
                let app = Arc::new(app);

                // Subscribe to shutdown before handing the app to the sim.
                shutdown_rxs.push(app.subscribe_shutdown());

                // Spawn a long-sleeping task as the mock run loop.
                let handle = tokio::spawn(async {
                    tokio::time::sleep(Duration::from_secs(999)).await;
                    Ok(())
                });
                let abort = handle.abort_handle();
                abort_handles.push(abort);

                let data_dir = Arc::new(dir);
                sim.insert_running_test_node(
                    format!("node-{i}"),
                    app,
                    handle,
                    data_dir,
                    19100 + i as u16,
                );
            }

            // Simulation is dropped here — Drop impl should shutdown + abort.
        }

        // Give the runtime a chance to process the abort signals.
        tokio::task::yield_now().await;

        // Verify all tasks were aborted.
        for (i, abort) in abort_handles.iter().enumerate() {
            assert!(
                abort.is_finished(),
                "node-{i} task should be finished (aborted) after Simulation drop"
            );
        }

        // Verify all apps received shutdown signals.
        for (i, rx) in shutdown_rxs.iter_mut().enumerate() {
            assert!(
                rx.try_recv().is_ok(),
                "node-{i} should have received shutdown signal"
            );
        }
    }

    /// Verify that `Simulation::drop` aborts task handles in `test_nodes`,
    /// not just `running_apps`. Regression test for #2560.
    #[tokio::test]
    async fn test_drop_aborts_test_nodes() {
        let mut abort_handles = Vec::new();

        {
            let mut sim = Simulation::new(SimulationMode::OverLoopback);

            for i in 0..2 {
                let handle = tokio::spawn(async {
                    // Long-running pending task that should be aborted on drop.
                    tokio::time::sleep(Duration::from_secs(999)).await;
                    Ok(())
                });
                let abort = handle.abort_handle();
                abort_handles.push(abort);

                sim.insert_test_node(format!("test-node-{i}"), handle, None);
            }

            // Simulation is dropped here — Drop impl should abort test_nodes.
        }

        // Give the runtime a chance to process the abort signals.
        tokio::task::yield_now().await;

        // Verify all test_nodes tasks were aborted.
        for (i, abort) in abort_handles.iter().enumerate() {
            assert!(
                abort.is_finished(),
                "test-node-{i} task should be finished (aborted) after Simulation drop"
            );
        }
    }

    /// Verify that `join_or_abort` aborts a task that exceeds the timeout.
    #[tokio::test]
    async fn test_join_or_abort_aborts_on_timeout() {
        let handle = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(999)).await;
            Ok(())
        });
        let abort = handle.abort_handle();

        let result = join_or_abort(handle, Duration::from_millis(10)).await;
        assert!(result.is_ok(), "timeout-abort should return Ok");

        tokio::task::yield_now().await;
        assert!(abort.is_finished(), "task should be aborted after timeout");
    }

    /// Verify that `join_or_abort` returns the task error on failure.
    #[tokio::test]
    async fn test_join_or_abort_propagates_error() {
        let handle = tokio::spawn(async { anyhow::bail!("task failed") });

        // Wait for the task to complete.
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(10)).await;

        let result = join_or_abort(handle, Duration::from_secs(1)).await;
        assert!(result.is_err(), "should propagate task error");
        assert!(
            result.unwrap_err().to_string().contains("task failed"),
            "error message should be preserved"
        );
    }

    /// Verify that the sequential join_or_abort pattern (as used by
    /// `stop_all_nodes` phase 2) cleans up all nodes even when one errors.
    #[tokio::test]
    async fn test_join_or_abort_sequence_cleans_all_on_error() {
        let failing_handle = tokio::spawn(async { anyhow::bail!("intentional failure") });
        let hanging_handle = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(999)).await;
            Ok(())
        });
        let hanging_abort = hanging_handle.abort_handle();

        // Let the failing task complete.
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Mimic stop_all_nodes phase 2: collect errors, don't stop early.
        let mut first_error: Option<anyhow::Error> = None;
        for handle in [failing_handle, hanging_handle] {
            if let Err(e) = join_or_abort(handle, Duration::from_millis(10)).await {
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
        }

        assert!(first_error.is_some(), "should report first error");
        tokio::task::yield_now().await;
        assert!(
            hanging_abort.is_finished(),
            "hanging task should be aborted even though first node errored"
        );
    }
}
