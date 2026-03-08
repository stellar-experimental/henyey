//! Deterministic multi-node simulation harness for validating consensus,
//! overlay, and ledger-close behavior across configurable topologies.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use henyey_app::config::{ConfigBuilder, QuorumSetConfig};
use henyey_app::{App, AppConfig, SimulationDebugStats};
use henyey_clock::RealClock;
use henyey_clock::VirtualClock;
use henyey_common::{Hash256, NetworkId};
use henyey_crypto::{sign_hash, SecretKey};
use henyey_overlay::{ConnectionFactory, LoopbackConnectionFactory, TcpConnectionFactory};
use stellar_xdr::curr::{
    AccountId, Asset, CreateAccountOp, DecoratedSignature, Memo, MuxedAccount, Operation,
    OperationBody, Preconditions, PublicKey, SequenceNumber, Signature, SignatureHint,
    Transaction, TransactionEnvelope, TransactionExt, TransactionV1Envelope, Uint256, VecM,
};
use tempfile::TempDir;
use tokio::task::JoinHandle;

mod loopback;
mod loadgen;
use loopback::LoopbackNetwork;
pub use loadgen::{
    GeneratedLoadConfig, GeneratedTransaction, LoadGenMode, LoadGenerator, LoadReport, LoadResult,
    LoadStep, TestAccount, TxGenerator,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimulationMode {
    OverLoopback,
    OverTcp,
}

#[derive(Debug, Clone)]
pub struct SimNode {
    pub node_id: String,
    pub secret_key: SecretKey,
    pub clock: VirtualClock,
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

struct RunningAppNode {
    app: Arc<App>,
    handle: JoinHandle<anyhow::Result<()>>,
    status: Arc<tokio::sync::RwLock<Option<Result<(), String>>>>,
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
            .field("running_apps", &self.running_apps.keys().collect::<Vec<_>>())
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
        }
    }

    pub fn add_node(&mut self, node_id: impl Into<String>, secret_key: SecretKey) {
        let node_id = node_id.into();
        let node = SimNode {
            node_id: node_id.clone(),
            secret_key,
            clock: VirtualClock::new(),
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
                manual_close: true,
                data_dir: None,
                peer_port: None,
            },
        );
    }

    pub fn remove_app_node_spec(&mut self, node_id: &str) -> Option<()> {
        self.app_specs.remove(node_id).map(|_| ())
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
    )
    where
        F: FnMut(&str, QuorumSetConfig) -> QuorumSetConfig,
    {
        let mut ids: Vec<String> = self.nodes.keys().cloned().collect();
        ids.sort();
        let validators: Vec<String> = ids
            .iter()
            .filter_map(|id| self.nodes.get(id).map(|n| n.secret_key.public_key().to_strkey()))
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

        let base_port = allocate_port_block(ids.len() as u16 + 8);
        let port_map: HashMap<String, u16> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| (id.clone(), base_port + i as u16))
            .collect();

        let overlay_connection_factory: Arc<dyn ConnectionFactory> = match self.mode {
            SimulationMode::OverLoopback => Arc::new(LoopbackConnectionFactory::new()),
            SimulationMode::OverTcp => Arc::new(TcpConnectionFactory),
        };
        self.overlay_connection_factory = Some(Arc::clone(&overlay_connection_factory));

        for id in ids {
            let mut spec = self
                .app_specs
                .get(&id)
                .cloned()
                .with_context(|| format!("missing app spec for {}", id))?;

            let data_dir = spec.data_dir.clone().unwrap_or_else(|| {
                Arc::new(tempfile::tempdir().expect("create simulation data dir"))
            });
            spec.data_dir = Some(Arc::clone(&data_dir));
            spec.peer_port = port_map.get(&id).copied();
            self.app_specs.insert(id.clone(), spec.clone());
            let app = self
                .build_app_from_spec(
                    &spec,
                    &port_map,
                    data_dir.path().to_path_buf(),
                    Arc::clone(&overlay_connection_factory),
                    true,
                )
                .await
                .with_context(|| format!("build app node {}", spec.node_id))?;

            let peer_port = *port_map
                .get(&spec.node_id)
                .expect("port assigned for app spec");
            let running = Self::bootstrap_and_spawn(app, data_dir, peer_port).await?;
            self.running_apps.insert(id, running);
        }

        Ok(())
    }

    pub async fn stop_all_nodes(&mut self) -> anyhow::Result<()> {
        let mut running = std::mem::take(&mut self.running_apps);
        for node in running.values() {
            node.app.shutdown();
        }
        for (id, node) in running.drain() {
            let mut handle = node.handle;
            let join = tokio::time::timeout(Duration::from_secs(5), &mut handle).await;
            match join {
                Ok(result) => {
                    result.with_context(|| format!("join app task for {}", id))??;
                }
                Err(_) => {
                    handle.abort();
                }
            }
        }
        Ok(())
    }

    pub async fn remove_node(&mut self, node_id: &str) -> anyhow::Result<()> {
        self.disconnect_node_from_peers(node_id).await?;
        if let Some(node) = self.running_apps.remove(node_id) {
            node.app.shutdown();
            let mut handle = node.handle;
            let join = tokio::time::timeout(Duration::from_secs(5), &mut handle).await;
            if join.is_err() {
                handle.abort();
            }
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

        let app = self
            .build_app_from_spec(
                &spec,
                &port_map,
                data_dir.path().to_path_buf(),
                Arc::clone(&overlay_connection_factory),
                false,
            )
            .await?;

        let running = Self::restore_and_spawn(app, data_dir, peer_port).await?;
        self.running_apps.insert(node_id.to_string(), running);
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
        let _ = a_app
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
            .map(|n| n.app.ledger_info().0)
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
        let peer_id =
            henyey_overlay::PeerId::from_bytes(*acceptor_secret.public_key().as_bytes());
        let initiator_app = self
            .running_apps
            .get(initiator)
            .with_context(|| format!("missing running app node {}", initiator))?;
        let _ = initiator_app.app.disconnect_peer(&peer_id).await;
        Ok(())
    }

    /// Look up a running app by its peer port.
    ///
    /// Matches stellar-core `Simulation::getAppFromPeerMap(peerPort)`.
    pub fn app_by_port(&self, port: u16) -> Option<Arc<App>> {
        self.running_apps
            .values()
            .find(|n| n.peer_port == port)
            .map(|n| Arc::clone(&n.app))
    }

    /// Check whether a loopback link is active between two nodes.
    ///
    /// Exposes `LoopbackNetwork::link_active` for test assertions.
    pub fn has_loopback_link(&self, a: &str, b: &str) -> bool {
        self.loopback.link_active(a, b)
    }

    /// Get the expected next ledger close time for an app node.
    ///
    /// Returns `tracking_consensus_close_time + ledger_close_time`.
    /// Matches stellar-core's expected close time calculation used in
    /// various `crankUntil` calls.
    pub fn expected_ledger_close_time(&self, node_id: &str) -> Option<u64> {
        let app = self.running_apps.get(node_id)?;
        Some(app.app.expected_ledger_close_time())
    }

    pub async fn app_peer_count(&self, node_id: &str) -> Option<usize> {
        let app = self.running_apps.get(node_id)?;
        Some(app.app.peer_count().await)
    }

    pub fn app_task_finished(&self, node_id: &str) -> Option<bool> {
        self.running_apps.get(node_id).map(|n| n.handle.is_finished())
    }

    pub async fn app_task_status(&self, node_id: &str) -> Option<Result<(), String>> {
        let node = self.running_apps.get(node_id)?;
        node.status.read().await.clone()
    }

    pub async fn app_debug_stats(&self, node_id: &str) -> Option<SimulationDebugStats> {
        let node = self.running_apps.get(node_id)?;
        Some(node.app.simulation_debug_stats().await)
    }

    pub async fn wait_for_app_connectivity(
        &self,
        min_peers: usize,
        timeout: Duration,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            let mut connected = true;
            for id in self.running_apps.keys() {
                if self.app_peer_count(id).await.unwrap_or(0) < min_peers {
                    connected = false;
                    break;
                }
            }
            if connected {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    }

    pub async fn repair_app_tcp_connectivity(&self) -> anyhow::Result<()> {
        let peers: Vec<(String, u16, Arc<App>)> = self
            .running_apps
            .iter()
            .map(|(id, node)| (id.clone(), node.peer_port, Arc::clone(&node.app)))
            .collect();

        for (id, _port, app) in &peers {
            for (other_id, other_port, _) in &peers {
                if id == other_id {
                    continue;
                }
                let addr = henyey_overlay::PeerAddress::new("127.0.0.1", *other_port);
                let _ = app.add_peer(addr).await?;
            }
        }

        Ok(())
    }

    pub async fn stabilize_app_tcp_connectivity(
        &self,
        min_peers: usize,
        timeout: Duration,
    ) -> anyhow::Result<bool> {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if let Err(err) = self.repair_app_tcp_connectivity().await {
                if !err.to_string().contains("overlay not started") {
                    return Err(err);
                }
            }
            if self
                .wait_for_app_connectivity(min_peers, Duration::from_secs(1))
                .await
            {
                return Ok(true);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(false)
    }

    pub fn have_all_app_nodes_externalized(&self, ledger_seq: u32, max_spread: u32) -> bool {
        if self.running_apps.is_empty() {
            return false;
        }

        let seqs: Vec<u32> = self
            .running_apps
            .values()
            .map(|n| n.app.ledger_info().0)
            .collect();
        let min_seq = *seqs.iter().min().unwrap_or(&0);
        let max_seq = *seqs.iter().max().unwrap_or(&0);
        min_seq >= ledger_seq && max_seq.saturating_sub(min_seq) <= max_spread
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

        let has_path = self.nodes.keys().any(|other| {
            if other == node_id || self.loopback.is_partitioned(other) {
                return false;
            }
            self.loopback.link_active(node_id, other)
        });

        if has_path {
            let next = current + 1;
            let hash_input = format!("{}:{}", node_id, next);
            if let Some(node) = self.nodes.get_mut(node_id) {
                node.ledger_seq = next;
                node.ledger_hash = Hash256::hash(hash_input.as_bytes());
            }
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
        let deadline = tokio::time::Instant::now() + duration;
        while tokio::time::Instant::now() < deadline {
            if !self.crank_all_nodes().await {
                break;
            }
        }
        if final_crank {
            let _ = self.crank_all_nodes().await;
        }
    }

    /// Crank all lightweight SimNodes repeatedly for at least `duration`
    /// (wall-clock time).
    ///
    /// Matches stellar-core `Simulation::crankForAtLeast(duration, finalCrank)`.
    pub async fn crank_for_at_least(&mut self, duration: Duration, final_crank: bool) {
        let deadline = tokio::time::Instant::now() + duration;
        loop {
            let _ = self.crank_all_nodes().await;
            if tokio::time::Instant::now() >= deadline {
                break;
            }
        }
        if final_crank {
            let _ = self.crank_all_nodes().await;
        }
    }

    pub async fn crank_all_nodes(&mut self) -> bool {
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
            if current < max_seq {
                let has_path = ids.iter().any(|other| {
                    if other == id || self.loopback.is_partitioned(other) {
                        return false;
                    }
                    self.loopback.link_active(id, other)
                });
                if has_path {
                    let next = current + 1;
                    let hash_input = format!("{}:{}", id, next);
                    if let Some(node) = self.nodes.get_mut(id) {
                        node.ledger_seq = next;
                        node.ledger_hash = Hash256::hash(hash_input.as_bytes());
                    }
                    did_work = true;
                }
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
        let connected = non_partitioned.iter().all(|id| {
            non_partitioned
                .iter()
                .filter(|other| *other != id)
                .any(|other| self.loopback.link_active(id, other))
        });
        if !connected {
            return false;
        }
        let next = max_seq + 1;
        for id in &non_partitioned {
            let hash_input = format!("{}:{}", id, next);
            if let Some(node) = self.nodes.get_mut(id) {
                node.ledger_seq = next;
                node.ledger_hash = Hash256::hash(hash_input.as_bytes());
            }
        }
        true
    }

    pub async fn crank_until<P>(&mut self, predicate: P, timeout: Duration) -> bool
    where
        P: Fn(&Simulation) -> bool,
    {
        let mut elapsed = Duration::ZERO;
        while elapsed <= timeout {
            if predicate(self) {
                return true;
            }
            let _ = self.crank_all_nodes().await;
            elapsed = elapsed.saturating_add(Duration::from_millis(100));
        }
        predicate(self)
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

        if seqs.is_empty() {
            return false;
        }

        let min_seq = *seqs.iter().min().unwrap_or(&0);
        let max_seq = *seqs.iter().max().unwrap_or(&0);
        min_seq >= ledger_seq && max_seq.saturating_sub(min_seq) <= max_spread
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

    pub fn app_spec_public_key(&self, node_id: &str) -> Option<String> {
        self.app_specs
            .get(node_id)
            .map(|spec| spec.secret_key.public_key().to_strkey())
            .or_else(|| self.nodes.get(node_id).map(|n| n.secret_key.public_key().to_strkey()))
    }

    pub fn secret_for_test(&self, node_id: &str) -> Option<SecretKey> {
        self.app_specs
            .get(node_id)
            .map(|spec| spec.secret_key.clone())
            .or_else(|| self.nodes.get(node_id).map(|n| n.secret_key.clone()))
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
            let tx = self.build_create_account_tx(&node_id, starting_balance, self.root_sequence)?;
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

    /// Bootstrap a fresh node from genesis and spawn its run loop.
    async fn bootstrap_and_spawn(
        app: App,
        data_dir: Arc<TempDir>,
        peer_port: u16,
    ) -> anyhow::Result<RunningAppNode> {
        let app = Arc::new(app);
        app.set_self_arc().await;
        app.bootstrap_from_db().await?;
        Self::spawn_app_run_loop(app, data_dir, peer_port)
    }

    /// Restore a previously-running node from its persisted DB + bucket
    /// state and spawn its run loop.  Used by `restart_node` so that the
    /// node resumes at its last closed ledger instead of re-initializing
    /// genesis.
    async fn restore_and_spawn(
        app: App,
        data_dir: Arc<TempDir>,
        peer_port: u16,
    ) -> anyhow::Result<RunningAppNode> {
        let app = Arc::new(app);
        app.set_self_arc().await;

        // Restore the LedgerManager from persisted DB + on-disk buckets.
        // App::run() will read the restored ledger via get_current_ledger()
        // and set state via restore_operational_state().
        match app.load_last_known_ledger().await {
            Ok(true) => {
                let (seq, _hash, _close_time, _protocol) = app.ledger_info();
                tracing::info!(lcl_seq = seq, "Restored restarted node from disk");
            }
            Ok(false) => {
                tracing::warn!("No persisted state for restarted node, falling back to genesis bootstrap");
                app.bootstrap_from_db().await?;
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to restore restarted node from disk, falling back to genesis bootstrap");
                app.bootstrap_from_db().await?;
            }
        }

        Self::spawn_app_run_loop(app, data_dir, peer_port)
    }

    fn spawn_app_run_loop(
        app: Arc<App>,
        data_dir: Arc<TempDir>,
        peer_port: u16,
    ) -> anyhow::Result<RunningAppNode> {
        let app_clone = Arc::clone(&app);
        let status = Arc::new(tokio::sync::RwLock::new(None));
        let status_clone = Arc::clone(&status);
        let handle = tokio::spawn(async move {
            let result = app_clone.run().await.map_err(|e| e.to_string());
            *status_clone.write().await = Some(result.clone());
            result.map_err(anyhow::Error::msg)
        });

        Ok(RunningAppNode {
            app,
            handle,
            status,
            _data_dir: data_dir,
            peer_port,
        })
    }

    async fn build_app_from_spec(
        &self,
        spec: &AppNodeSpec,
        port_map: &HashMap<String, u16>,
        data_dir: PathBuf,
        overlay_connection_factory: Arc<dyn ConnectionFactory>,
        init_genesis: bool,
    ) -> anyhow::Result<App> {
        let peer_port = *port_map
            .get(&spec.node_id)
            .with_context(|| format!("missing peer port for {}", spec.node_id))?;

        let mut config = ConfigBuilder::new()
            .node_name(spec.node_id.clone())
            .node_seed(spec.secret_key.to_strkey())
            .validator(spec.is_validator)
            .database_path(data_dir.join("node.db"))
            .bucket_directory(data_dir.join("buckets"))
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

        if let Some(parent) = config.database.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::create_dir_all(&config.buckets.directory)?;

        if init_genesis {
            initialize_genesis_ledger(&config, &self.network_passphrase)?;
        }
        App::new_with_clock_and_connection_factory(
            config,
            Arc::new(RealClock),
            overlay_connection_factory,
        )
        .await
    }

    fn known_peers_for(
        &self,
        node_id: &str,
        port_map: &HashMap<String, u16>,
    ) -> Vec<String> {
        self.loopback
            .links()
            .into_iter()
            .filter_map(|(a, b)| {
                if a == node_id && a < b {
                    port_map.get(&b).map(|port| format!("127.0.0.1:{}", port))
                } else {
                    None
                }
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
        let network_id = NetworkId::from_passphrase(&self.network_passphrase);
        let frame = henyey_tx::TransactionFrame::with_network(envelope.clone(), network_id);
        let hash = frame.hash(&network_id)?;
        let signature = sign_hash(secret, &hash);
        let public_key = secret.public_key();
        let pk_bytes = public_key.as_bytes();
        let hint = SignatureHint([pk_bytes[28], pk_bytes[29], pk_bytes[30], pk_bytes[31]]);
        let decorated = DecoratedSignature {
            hint,
            signature: Signature(signature.0.to_vec().try_into().unwrap_or_default()),
        };

        if let TransactionEnvelope::Tx(ref mut env) = envelope {
            env.signatures = vec![decorated].try_into().unwrap_or_default();
        }
        Ok(envelope)
    }

    fn destination_for_node(&self, node_id: &str) -> anyhow::Result<MuxedAccount> {
        let secret = self.secret_for_node(node_id)?;
        Ok(MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())))
    }

    fn account_id_for_node(&self, node_id: &str) -> anyhow::Result<AccountId> {
        let secret = self.secret_for_node(node_id)?;
        Ok(AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
            *secret.public_key().as_bytes(),
        ))))
    }

    fn secret_for_node(&self, node_id: &str) -> anyhow::Result<SecretKey> {
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
            let seed = Hash256::hash(format!("SIM_NODE_SEED_{}", i).as_bytes());
            let sk = SecretKey::from_seed(&seed.0);
            sim.add_node(id.clone(), sk);
            ids.push(id);
        }

        for i in 0..n {
            for j in (i + 1)..n {
                sim.add_pending_connection(ids[i].clone(), ids[j].clone());
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
            let seed = Hash256::hash(format!("SIM_NODE_SEED_{}", i).as_bytes());
            let sk = SecretKey::from_seed(&seed.0);
            sim.add_node(id.clone(), sk);
            ids.push(id);
        }

        if n >= 2 {
            for i in 0..n {
                let j = (i + 1) % n;
                sim.add_pending_connection(ids[i].clone(), ids[j].clone());
            }
        }

        sim
    }

    pub fn branchedcycle(n: usize, mode: SimulationMode) -> Simulation {
        let mut sim = Self::cycle(n, mode);
        let ids = sim.node_ids();
        if n >= 4 {
            for i in 0..n {
                let other = (i + (n / 2)) % n;
                if i != other {
                    sim.add_pending_connection(ids[i].clone(), ids[other].clone());
                }
            }
        }
        sim
    }

    pub fn hierarchical_quorum(n_branches: usize, mode: SimulationMode) -> Simulation {
        let mut sim = Self::core(4, mode);
        for branch in 0..n_branches {
            let id = format!("branch{}", branch);
            let seed = Hash256::hash(format!("SIM_BRANCH_SEED_{}", branch).as_bytes());
            let sk = SecretKey::from_seed(&seed.0);
            sim.add_node(id.clone(), sk);
            sim.add_pending_connection(id, format!("node{}", branch % 4));
            sim.add_pending_connection(format!("branch{}", branch), format!("node{}", (branch + 1) % 4));
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
            let seed = Hash256::hash(format!("SIM_OUTER_SEED_{}", outer).as_bytes());
            let sk = SecretKey::from_seed(&seed.0);
            sim.add_node(id.clone(), sk);
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
            let seed = Hash256::hash(format!("SIM_CUSTOM_A_{}", id).as_bytes());
            let sk = SecretKey::from_seed(&seed.0);
            sim.add_node(id.to_string(), sk);
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
            let seed = Hash256::hash(format!("SIM_ASYM_SEED_{}", extra).as_bytes());
            let sk = SecretKey::from_seed(&seed.0);
            sim.add_node(id.clone(), sk);
            sim.add_pending_connection(id, "node0");
        }
        sim
    }

    pub fn separate(mode: SimulationMode) -> Simulation {
        let mut sim = Simulation::new(mode);
        for i in 0..4 {
            let id = format!("node{}", i);
            let seed = Hash256::hash(format!("SIM_NODE_SEED_{}", i).as_bytes());
            let sk = SecretKey::from_seed(&seed.0);
            sim.add_node(id, sk);
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
    pub fn separate_with_watchers(
        n: usize,
        watchers: usize,
        mode: SimulationMode,
    ) -> Simulation {
        let mut sim = Simulation::new(mode);

        // Create validator nodes in two partitions.
        let half = n / 2;
        for i in 0..n {
            let id = format!("node{}", i);
            let seed = Hash256::hash(format!("SIM_NODE_SEED_{}", i).as_bytes());
            let sk = SecretKey::from_seed(&seed.0);
            sim.add_node(id, sk);
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
            let seed = Hash256::hash(format!("SIM_WATCHER_SEED_{}", w).as_bytes());
            let sk = SecretKey::from_seed(&seed.0);
            sim.add_node(id.clone(), sk);
            // Connect watcher to all nodes in partition A.
            for i in 0..half {
                sim.add_pending_connection(id.clone(), format!("node{}", i));
            }
        }

        sim
    }
}

fn initialize_genesis_ledger(config: &AppConfig, network_passphrase: &str) -> anyhow::Result<()> {
    use henyey_bucket::BucketList;
    use henyey_db::queries::{BucketListQueries, HistoryQueries, LedgerQueries, StateQueries};
    use henyey_db::schema::state_keys;
    use henyey_history::build_history_archive_state;
    use henyey_ledger::{calculate_skip_values, compute_header_hash};
    use stellar_xdr::curr::{
        AccountEntry, AccountEntryExt, AccountId, BucketListType, Hash, LedgerEntry,
        LedgerEntryData, LedgerEntryExt, LedgerHeader, LedgerHeaderExt, Limits, PublicKey,
        SequenceNumber, StellarValue, StellarValueExt, Thresholds, TimePoint,
        TransactionHistoryEntry, TransactionHistoryEntryExt, TransactionHistoryResultEntry,
        TransactionHistoryResultEntryExt, TransactionResultSet, TransactionSet, Uint256, VecM,
        WriteXdr,
    };

    let db = henyey_db::Database::open(&config.database.path)?;
    let network_id = NetworkId::from_passphrase(network_passphrase);
    let root_secret = SecretKey::from_seed(network_id.as_bytes());
    let root_public = root_secret.public_key();
    let root_account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(*root_public.as_bytes())));

    let total_coins: i64 = 1_000_000_000_000_000_000;
    let root_entry = LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::Account(AccountEntry {
            account_id: root_account_id,
            balance: total_coins,
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
    };

    let mut bucket_list = BucketList::new();
    bucket_list
        .add_batch(1, 0, BucketListType::Live, vec![root_entry], vec![], vec![])
        .map_err(|e| anyhow::anyhow!("Failed to add genesis entry to bucket list: {}", e))?;

    let bucket_list_hash = bucket_list.hash();
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
        base_reserve: 100_000_000,
        max_tx_set_size: 100,
        skip_list: std::array::from_fn(|_| Hash([0u8; 32])),
        ext: LedgerHeaderExt::V0,
    };
    calculate_skip_values(&mut header);
    let header_xdr = header.to_xdr(Limits::none())?;
    let has = build_history_archive_state(
        1,
        &bucket_list,
        None,
        Some(network_passphrase.to_string()),
    )
    .map_err(|e| anyhow::anyhow!("Failed to build HAS: {}", e))?;
    let has_json = has.to_json()?;
    let bucket_levels: Vec<(Hash256, Hash256)> = bucket_list
        .levels()
        .iter()
        .map(|level| (level.curr.hash(), level.snap.hash()))
        .collect();
    let genesis_tx_history = TransactionHistoryEntry {
        ledger_seq: 1,
        tx_set: TransactionSet {
            previous_ledger_hash: Hash(Hash256::ZERO.0),
            txs: VecM::default(),
        },
        ext: TransactionHistoryEntryExt::V0,
    };
    let genesis_tx_result = TransactionHistoryResultEntry {
        ledger_seq: 1,
        tx_result_set: TransactionResultSet {
            results: VecM::default(),
        },
        ext: TransactionHistoryResultEntryExt::default(),
    };

    db.with_connection(|conn| {
        conn.store_ledger_header(&header, &header_xdr)?;
        conn.store_tx_history_entry(1, &genesis_tx_history)?;
        conn.store_tx_result_entry(1, &genesis_tx_result)?;
        conn.store_bucket_list(1, &bucket_levels)?;
        conn.set_state(state_keys::HISTORY_ARCHIVE_STATE, &has_json)?;
        conn.set_state(state_keys::NETWORK_PASSPHRASE, network_passphrase)?;
        conn.set_last_closed_ledger(1)?;
        Ok::<_, henyey_db::DbError>(())
    })?;

    let _ = compute_header_hash(&header)?;
    Ok(())
}

fn root_secret(network_passphrase: &str) -> SecretKey {
    let network_id = NetworkId::from_passphrase(network_passphrase);
    SecretKey::from_seed(network_id.as_bytes())
}
