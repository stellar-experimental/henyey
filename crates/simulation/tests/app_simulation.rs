use std::time::Duration;

use henyey_app::config::QuorumSetConfig;
use henyey_app::{App, AppState};
use henyey_common::Hash256;
use henyey_crypto::SecretKey;
use henyey_herder::scp_verify::PostVerifyReason;
use henyey_simulation::{
    poll_until, CrashScope, GeneratedLoadConfig, LoadGenerator, LoadStep, PollOutcome, Simulation,
    SimulationMode, Topologies,
};
use serial_test::serial;

/// Timeout for the post-remove_node ledger close over TCP.
/// Conservative guess (2× the original 45s) to absorb CI jitter.
/// See follow-up investigation issue for root cause analysis.
const TCP_POST_REMOVAL_CLOSE_TIMEOUT_SECS: u64 = 90;

// ── Connectivity stabilization timeout policy ────────────────────────
//
// Three tiers based on the complexity of the connectivity scenario:
//
//   STARTUP (60s) — full topology cold-start, all nodes connecting simultaneously
//                   Used by build_app_backed_topology (topology-aware stabilization)
//   RESTART (60s) — single node rejoining over TCP (stabilize_app_tcp_connectivity)
//                   Loopback restart uses wait_for_peer_count + POST_RESTART_PEER_CONNECT_TIMEOUT_SECS
//   REDUCED (30s) — reduced topology with fewer nodes/connections
//                   Used by build_two_running_of_three (both OverTcp and OverLoopback)
//
// Startup and restart share the same value intentionally: although restart
// involves only one node, it must discover peers, TCP connect, and
// authenticate overlay hello — comparable work to a full cold-start.
// These values include slack for CI runner load spikes.

/// Full topology cold-start: all nodes starting and connecting simultaneously.
const STABILIZE_STARTUP_TIMEOUT_SECS: u64 = 60;

/// Single node rejoining an already-running cluster over TCP after removal + restart.
/// Covers peer discovery, TCP connect, and overlay hello authentication.
/// The loopback restart path uses `POST_RESTART_PEER_CONNECT_TIMEOUT_SECS` instead.
const STABILIZE_RESTART_TIMEOUT_SECS: u64 = 60;

/// Reduced topology (e.g., 2-of-3 nodes running): fewer connections needed.
const STABILIZE_REDUCED_TIMEOUT_SECS: u64 = 30;

/// Timeout for remaining nodes to detect peer disconnection after `remove_node()`.
/// The disconnect detection path is the same for TCP and loopback — the overlay
/// layer notices the peer task ended. CI load can delay task scheduling.
const POST_REMOVE_PEER_DETECT_TIMEOUT_SECS: u64 = 30;

/// Timeout for establishing peer connectivity after node restart + reconnect
/// over loopback. Covers: overlay hello authentication, quorum-set propagation.
/// Uses the same 60s value as `STABILIZE_RESTART_TIMEOUT_SECS` (the TCP-mode
/// equivalent) because the handshake work is comparable.
const POST_RESTART_PEER_CONNECT_TIMEOUT_SECS: u64 = 60;

/// Timeout for a restarted node to reach Synced|Validating state.
/// This is a local transition (no peers needed) — initialization + DB load.
/// 15s accommodates disk I/O delays on loaded CI runners.
const POST_RESTART_OPERATIONAL_TIMEOUT_SECS: u64 = 15;

/// Post-connectivity budget for all nodes to reach `AppState::Validating`.
/// The Synced → Validating transition requires the run loop to bootstrap the
/// herder and call `restore_operational_state()` — typically <1s, but 10s
/// accommodates CI load spikes.
const WAIT_FOR_VALIDATING_TIMEOUT_SECS: u64 = 10;

async fn wait_for_app_ledger_close(
    sim: &Simulation,
    target_ledger: u32,
    max_spread: u32,
    timeout: Duration,
) {
    let outcome = poll_until(
        sim,
        timeout,
        Duration::from_millis(100),
        CrashScope::AllNodes,
        || async {
            if sim.have_all_app_nodes_externalized(target_ledger, max_spread) {
                Ok(Some(()))
            } else {
                Ok(None)
            }
        },
    )
    .await
    .expect("fatal error during ledger close wait");
    match outcome {
        PollOutcome::Satisfied(()) => {}
        PollOutcome::NodeExited { node_id, status } => {
            let diag = collect_node_diagnostics(sim).await;
            panic!(
                "node {node_id} task exited while waiting for ledger {target_ledger} \
                 (task_status: {status:?}).{diag}"
            );
        }
        PollOutcome::TimedOut => {
            let mut diag = collect_node_diagnostics(sim).await;
            diag.push_str(&sim.node_task_diagnostics().await);
            assert!(
                sim.have_all_app_nodes_externalized(target_ledger, max_spread),
                "timed out after {timeout:?} waiting for ledger {target_ledger}.{diag}"
            );
        }
    }
}

async fn manual_close_until(
    sim: &Simulation,
    target_ledger: u32,
    max_spread: u32,
    timeout: Duration,
) {
    let last_err = std::cell::RefCell::new(None::<String>);
    let outcome = poll_until(
        sim,
        timeout,
        Duration::from_millis(100),
        CrashScope::AllNodes,
        || async {
            if sim.have_all_app_nodes_externalized(target_ledger, max_spread) {
                return Ok(Some(()));
            }
            if let Err(e) = sim.manual_close_all_app_nodes().await {
                *last_err.borrow_mut() = Some(e.to_string());
            }
            Ok(None)
        },
    )
    .await
    .expect("fatal error during manual_close_until");
    match outcome {
        PollOutcome::Satisfied(()) => {}
        PollOutcome::NodeExited { node_id, status } => {
            let diag = collect_node_diagnostics(sim).await;
            panic!(
                "node {node_id} task exited while waiting for ledger {target_ledger} \
                 in manual_close_until (task_status: {status:?}).{diag}"
            );
        }
        PollOutcome::TimedOut => {
            let mut diag = collect_node_diagnostics(sim).await;
            diag.push_str(&sim.node_task_diagnostics().await);
            if let Some(err) = &*last_err.borrow() {
                diag.push_str(&format!("\n  last manual_close error: {err}"));
            }
            assert!(
                sim.have_all_app_nodes_externalized(target_ledger, max_spread),
                "manual_close_until timed out after {timeout:?} waiting for ledger \
                 {target_ledger}.{diag}"
            );
        }
    }
}

async fn collect_node_diagnostics(sim: &Simulation) -> String {
    let mut diag = String::new();
    for id in sim.app_node_ids() {
        match sim.app_debug_stats(&id).await {
            Some(stats) => {
                let slot = stats.slot.as_ref();
                diag.push_str(&format!(
                    "\n  {id}: ledger={}, peers={}, state={}, herder={}, \
                     pending_envelopes={}, heard_quorum={}, v_blocking={}, \
                     slot_externalized={}, slot_nominating={}, slot_scp_heard_quorum={}, \
                     ballot_phase={}, nomination_round={}, ballot_round={}, fully_validated={}, \
                     nom_timeouts={}, ballot_timeouts={}, \
                     scp_sent={}, scp_recv={}, \
                     trigger_attempts={}, trigger_ok={}, trigger_fail={}",
                    stats.current_ledger,
                    stats.peer_count,
                    stats.app_state,
                    stats.herder_state,
                    stats.pending_envelopes,
                    stats.heard_from_quorum,
                    stats.is_v_blocking,
                    slot.map_or("none".to_string(), |s| s.is_externalized.to_string()),
                    slot.map_or("none".to_string(), |s| s.is_nominating.to_string()),
                    slot.map_or("none".to_string(), |s| s.scp_heard_from_quorum.to_string()),
                    slot.map_or("none", |s| s.ballot_phase.as_str()),
                    slot.map_or("none".to_string(), |s| s.nomination_round.to_string()),
                    slot.and_then(|s| s.ballot_round)
                        .map_or("none".to_string(), |r| r.to_string()),
                    slot.and_then(|s| s.fully_validated)
                        .map_or("none".to_string(), |v| v.to_string()),
                    stats.nomination_timeout_fires,
                    stats.ballot_timeout_fires,
                    stats.scp_messages_sent,
                    stats.scp_messages_received,
                    stats.consensus_trigger_attempts,
                    stats.consensus_trigger_successes,
                    stats.consensus_trigger_failures,
                ));
            }
            None => {
                diag.push_str(&format!("\n  {id}: <not running>"));
            }
        }
    }
    diag
}

async fn wait_for_app_operational(sim: &Simulation, node_id: &str, timeout: Duration) {
    let outcome = poll_until(
        sim,
        timeout,
        Duration::from_millis(100),
        CrashScope::SingleNode(node_id),
        || async {
            if let Some(app) = sim.app(node_id) {
                if matches!(app.state().await, AppState::Synced | AppState::Validating) {
                    return Ok(Some(()));
                }
            }
            Ok(None)
        },
    )
    .await
    .expect("fatal error during wait_for_app_operational");
    match outcome {
        PollOutcome::Satisfied(()) => {}
        PollOutcome::NodeExited { node_id, status } => {
            let diag = collect_node_diagnostics(sim).await;
            panic!(
                "node {node_id} task exited while waiting for operational state \
                 (task_status: {status:?}).{diag}"
            );
        }
        PollOutcome::TimedOut => {
            let finished = sim.app_task_finished(node_id);
            let status = sim.app_task_status(node_id).await;
            let diag = collect_node_diagnostics(sim).await;
            match sim.app(node_id) {
                None => panic!(
                    "timed out after {timeout:?}: node {node_id} not in running_apps \
                     (task_finished={finished:?}, task_status={status:?}).{diag}"
                ),
                Some(app) => {
                    let state = app.state().await;
                    panic!(
                        "timed out after {timeout:?} waiting for {node_id} to become \
                         operational (state: {state:?}, task_finished={finished:?}, \
                         task_status={status:?}).{diag}"
                    );
                }
            }
        }
    }
}

/// Wait for a single node to reach the exact specified `AppState`.
/// Unlike `wait_for_app_operational` (which accepts `Synced | Validating`),
/// this waits for one specific state — no risk of racing with the post-wait
/// assert.
async fn wait_for_app_state(
    sim: &Simulation,
    node_id: &str,
    target_state: AppState,
    timeout: Duration,
) {
    let outcome = poll_until(
        sim,
        timeout,
        Duration::from_millis(100),
        CrashScope::SingleNode(node_id),
        || async {
            if let Some(app) = sim.app(node_id) {
                if app.state().await == target_state {
                    return Ok(Some(()));
                }
            }
            Ok(None)
        },
    )
    .await
    .expect("fatal error during wait_for_app_state");
    match outcome {
        PollOutcome::Satisfied(()) => {}
        PollOutcome::NodeExited { node_id, status } => {
            let diag = collect_node_diagnostics(sim).await;
            panic!(
                "node {node_id} task exited while waiting for {target_state:?} \
                 (task_status: {status:?}).{diag}"
            );
        }
        PollOutcome::TimedOut => {
            let diag = collect_node_diagnostics(sim).await;
            match sim.app(node_id) {
                None => {
                    panic!("timed out after {timeout:?}: node {node_id} not in running_apps.{diag}")
                }
                Some(app) => {
                    let state = app.state().await;
                    panic!(
                        "timed out after {timeout:?} waiting for {node_id} to reach \
                         {target_state:?} (current state: {state:?}).{diag}"
                    );
                }
            }
        }
    }
}

/// Wait for ALL app nodes to reach the exact specified `AppState`.
/// Panics immediately if there are no app nodes (vacuous truth guard).
async fn wait_for_all_app_state(
    sim: &Simulation,
    target_state: AppState,
    timeout: Duration,
    caller: &str,
) {
    let apps = sim.apps();
    assert!(
        !apps.is_empty(),
        "{caller}: wait_for_all_app_state called with zero app nodes"
    );
    let outcome = poll_until(
        sim,
        timeout,
        Duration::from_millis(100),
        CrashScope::AllNodes,
        || async {
            for app in &sim.apps() {
                if app.state().await != target_state {
                    return Ok(None);
                }
            }
            Ok(Some(()))
        },
    )
    .await
    .expect("fatal error during wait_for_all_app_state");
    match outcome {
        PollOutcome::Satisfied(()) => {}
        PollOutcome::NodeExited { node_id, status } => {
            let diag = collect_node_diagnostics(sim).await;
            panic!(
                "{caller}: node {node_id} task exited while waiting for all nodes to reach \
                 {target_state:?} (task_status: {status:?}).{diag}"
            );
        }
        PollOutcome::TimedOut => {
            let diag = collect_node_diagnostics(sim).await;
            panic!(
                "{caller}: timed out after {timeout:?} waiting for all nodes to reach \
                 {target_state:?}.{diag}"
            );
        }
    }
}

async fn wait_for_peer_count(sim: &Simulation, node_id: &str, expected: usize, timeout: Duration) {
    let outcome = poll_until(
        sim,
        timeout,
        Duration::from_millis(100),
        CrashScope::SingleNode(node_id),
        || async {
            match sim.app_peer_count(node_id).await {
                Some(count) if count == expected => Ok(Some(())),
                _ => Ok(None),
            }
        },
    )
    .await
    .expect("fatal error during wait_for_peer_count");
    match outcome {
        PollOutcome::Satisfied(()) => {}
        PollOutcome::NodeExited { node_id, status } => {
            let diag = collect_node_diagnostics(sim).await;
            panic!(
                "node {node_id} task exited while waiting for peer count {expected} \
                 (task_status: {status:?}).{diag}"
            );
        }
        PollOutcome::TimedOut => {
            let actual = sim.app_peer_count(node_id).await;
            let finished = sim.app_task_finished(node_id);
            let status = sim.app_task_status(node_id).await;
            let diag = collect_node_diagnostics(sim).await;
            match actual {
                None => panic!(
                    "timed out after {timeout:?}: node {node_id} not in running_apps \
                     (task_finished={finished:?}, task_status={status:?}, \
                     expected peer count {expected}).{diag}"
                ),
                Some(count) => panic!(
                    "timed out after {timeout:?} waiting for {node_id} peer count \
                     to reach {expected} (actual: {count}, task_finished={finished:?}, \
                     task_status={status:?}).{diag}"
                ),
            }
        }
    }
}

/// Asserts that NOT all nodes reach `ledger_seq` within `observation_window`.
/// Checks ledger-sequence spread (not hash agreement). In a partition scenario,
/// nodes without quorum cannot advance their ledger sequence.
///
/// Also panics immediately if any node's task exits unexpectedly during the
/// observation window (crash detection), preventing a false success where the
/// partition holds because a node died rather than because quorum was broken.
///
/// Observation window: should exceed the time a non-partitioned network would
/// take to externalize (typically <2s with manual_close). 5s provides 2.5× margin.
async fn assert_not_all_nodes_externalized(
    sim: &Simulation,
    ledger_seq: u32,
    max_spread: u32,
    observation_window: Duration,
) {
    // Inverted poll: condition returns Some(()) when all nodes externalized
    // (which is the *failure* case — we expect this NOT to happen).
    let outcome = poll_until(
        sim,
        observation_window,
        Duration::from_millis(250),
        CrashScope::AllNodes,
        || async {
            if sim.have_all_app_nodes_externalized(ledger_seq, max_spread) {
                Ok(Some(()))
            } else {
                Ok(None)
            }
        },
    )
    .await
    .expect("fatal error during partition observation");
    match outcome {
        PollOutcome::Satisfied(()) => {
            // All nodes externalized — partition failed
            let diag = collect_node_diagnostics(sim).await;
            let topo = sim.peer_topology();
            let topo_str: Vec<String> = topo
                .iter()
                .map(|(id, peers)| format!("{id} -> [{peers}]", peers = peers.join(", ")))
                .collect();
            panic!(
                "nodes unexpectedly all reached ledger {ledger_seq} \
                 (max_spread={max_spread}) during {observation_window:?} \
                 observation — partition may be ineffective.\
                 \n  configured topology: {topo_diag}{diag}",
                topo_diag = topo_str.join("; "),
            );
        }
        PollOutcome::NodeExited { node_id, status } => {
            let diag = collect_node_diagnostics(sim).await;
            panic!(
                "node {node_id} task exited during partition observation \
                 (task_status: {status:?}).{diag}"
            );
        }
        PollOutcome::TimedOut => {
            // Success: partition held for the observation window.
            // Final crash check to catch exit in the last sleep interval.
            if let Some((node_id, status)) = sim.find_exited_node(&CrashScope::AllNodes).await {
                let diag = collect_node_diagnostics(sim).await;
                panic!(
                    "node {node_id} task exited after partition observation window \
                     (task_status: {status:?}).{diag}"
                );
            }
        }
    }
}

async fn ensure_app_accounts_funded(sim: &mut Simulation, expected: usize) {
    let mut ledger_target = sim
        .app("node0")
        .map(|app| app.ledger_info().ledger_seq)
        .unwrap_or(1);
    let mut funded_total = 0usize;
    let mut rounds = 0usize;
    while funded_total < expected && rounds < 8 {
        let funded = sim
            .fund_app_accounts(10_000_000)
            .await
            .expect("fund app accounts");
        funded_total += funded;
        ledger_target += 1;
        manual_close_until(sim, ledger_target, 1, Duration::from_secs(20)).await;
        rounds += 1;
    }
    assert_eq!(funded_total, expected);
}

/// Await connectivity stabilization with a named timeout tier.
/// Panics with a diagnostic message including the caller name, timeout, and error.
async fn stabilize_or_panic(sim: &Simulation, min_peers: usize, timeout_secs: u64, caller: &str) {
    sim.stabilize_app_tcp_connectivity(min_peers, Duration::from_secs(timeout_secs))
        .await
        .unwrap_or_else(|err| {
            panic!(
                "{caller}: connectivity did not stabilize within {timeout_secs}s \
                 (min_peers={min_peers}): {err}"
            )
        });
}

/// Await topology-aware connectivity stabilization.
/// Panics if any node's peer set does not match the expected topology.
async fn stabilize_topology_or_panic(sim: &Simulation, timeout_secs: u64, caller: &str) {
    sim.stabilize_app_topology_connectivity(Duration::from_secs(timeout_secs))
        .await
        .unwrap_or_else(|err| {
            panic!(
                "{caller}: topology connectivity did not stabilize within {timeout_secs}s: {err}"
            )
        });
}

/// Build a fully-connected app-backed topology and wait for startup readiness.
///
/// Post-condition: all peers connected per topology graph AND all nodes in
/// `AppState::Validating` (herder in `Tracking` state). Callers can
/// immediately issue `manual_close`, submit transactions, or assert
/// consensus state without additional waits.
async fn build_app_backed_topology(mut sim: Simulation, threshold_percent: u32) -> Simulation {
    sim.populate_app_nodes_from_existing(threshold_percent);
    sim.start_all_nodes().await;
    stabilize_topology_or_panic(
        &sim,
        STABILIZE_STARTUP_TIMEOUT_SECS,
        "build_app_backed_topology",
    )
    .await;
    // Wait for all nodes to reach Validating (herder Tracking).
    // Synced is NOT sufficient — bootstrap_from_db() sets Synced before the
    // run loop bootstraps the herder, so a node in Synced may not yet accept
    // manual_close / trigger_next_ledger.
    wait_for_all_app_state(
        &sim,
        AppState::Validating,
        Duration::from_secs(WAIT_FOR_VALIDATING_TIMEOUT_SECS),
        "build_app_backed_topology",
    )
    .await;
    sim
}

/// Build a 3-node topology with only 2 nodes running (threshold 66%).
///
/// Post-condition: both running nodes connected AND in `AppState::Validating`.
async fn build_two_running_of_three(mode: SimulationMode) -> Simulation {
    let mut sim = Topologies::core3(mode);
    let node_ids = sim.node_ids();
    let validators: Vec<String> = node_ids
        .iter()
        .map(|id| sim.app_spec_public_key(id).expect("public key for node"))
        .collect();
    let quorum_set = QuorumSetConfig {
        threshold_percent: 66,
        validators,
        inner_sets: Vec::new(),
    };

    for id in node_ids.iter().take(2) {
        let secret = sim.secret_for_node(id).expect("secret for node");
        sim.add_app_node(id.clone(), secret, quorum_set.clone());
    }
    sim.start_all_nodes().await;
    stabilize_or_panic(
        &sim,
        1,
        STABILIZE_REDUCED_TIMEOUT_SECS,
        "build_two_running_of_three",
    )
    .await;
    wait_for_all_app_state(
        &sim,
        AppState::Validating,
        Duration::from_secs(WAIT_FOR_VALIDATING_TIMEOUT_SECS),
        "build_two_running_of_three",
    )
    .await;
    sim
}

/// Capture the three `pv_counters` variants that prove `pump_scp_intake` traversal.
///
/// Returns `(Accepted, PendingAddBuffered, PendingAddProcessedDirectly)`.
fn scp_intake_counters(app: &App) -> (u64, u64, u64) {
    let c = &app.info().scp_verify.pv_counters;
    (
        c[PostVerifyReason::Accepted],
        c[PostVerifyReason::PendingAddBuffered],
        c[PostVerifyReason::PendingAddProcessedDirectly],
    )
}

/// Assert that `Accepted + PendingAddBuffered + PendingAddProcessedDirectly` is
/// positive, proving SCP envelopes traversed the `pump_scp_intake` pipeline.
///
/// Use after a fresh `restart_node()` + recovery: the new `App` starts with
/// zero counters, so any positive total proves the intake pipeline worked.
fn assert_scp_intake_reached(app: &App, node: &str) {
    let (accepted, buffered, direct) = scp_intake_counters(app);
    let total = accepted + buffered + direct;
    assert!(
        total > 0,
        "{node} must have processed SCP envelopes through pump_scp_intake \
         (Accepted={accepted}, Buffered={buffered}, Direct={direct}, total={total})"
    );
}

#[tokio::test]
async fn test_single_node_app_simulation_can_manual_close_over_tcp() {
    let mut sim =
        Simulation::with_network(SimulationMode::OverTcp, "Test SDF Network ; September 2015");

    let seed = Hash256::hash(b"APP_SIM_NODE_0");
    let secret = SecretKey::from_seed(&seed.0);
    let quorum_set = QuorumSetConfig {
        threshold_percent: 100,
        validators: vec![secret.public_key().to_strkey()],
        inner_sets: Vec::new(),
    };

    sim.add_app_node("node0", secret, quorum_set);
    sim.start_all_nodes().await;

    let app = sim.app("node0").expect("running app node");
    wait_for_app_state(&sim, "node0", AppState::Validating, Duration::from_secs(10)).await;
    assert_eq!(app.state().await, AppState::Validating);

    let closed = sim
        .manual_close_all_app_nodes()
        .await
        .expect("manual close");
    assert_eq!(closed, vec![2]);

    wait_for_app_ledger_close(&sim, 2, 0, Duration::from_secs(10)).await;
    sim.stop_all_nodes().await.expect("stop app-backed nodes");
}

/// Multi-slot standalone validator regression test (follow-up from #2317).
///
/// Extends `test_single_node_app_simulation_can_manual_close_over_tcp` to close
/// 5 consecutive ledgers (2 through 6) in a zero-peer, single-validator topology.
/// Catches regressions in the solo-quorum SCP state machine, tracking-state
/// advancement, and nomination value building across multiple slots.
#[tokio::test]
async fn test_standalone_validator_closes_multiple_ledgers() {
    let mut sim =
        Simulation::with_network(SimulationMode::OverTcp, "Test SDF Network ; September 2015");

    let seed = Hash256::hash(b"STANDALONE_MULTI_SLOT");
    let secret = SecretKey::from_seed(&seed.0);
    let quorum_set = QuorumSetConfig {
        threshold_percent: 100,
        validators: vec![secret.public_key().to_strkey()],
        inner_sets: Vec::new(),
    };

    sim.add_app_node("node0", secret, quorum_set);
    sim.start_all_nodes().await;

    wait_for_app_state(&sim, "node0", AppState::Validating, Duration::from_secs(10)).await;

    // Close 5 ledgers: from genesis (ledger 1) through ledger 6.
    manual_close_until(&sim, 6, 0, Duration::from_secs(60)).await;

    // Confirm the node operated with zero connected peers throughout.
    assert_eq!(
        sim.app_peer_count("node0").await,
        Some(0),
        "standalone validator should have zero peers"
    );

    sim.stop_all_nodes().await.expect("stop standalone node");
}

/// Regression test for #2357: App::load_account_sequence must return the
/// root account's sequence number, not Ok(None).
#[tokio::test]
async fn test_load_account_sequence_finds_root_account() {
    let mut sim = Simulation::with_network(
        SimulationMode::OverLoopback,
        "Test SDF Network ; September 2015",
    );

    let seed = Hash256::hash(b"LOAD_ACCT_SEQ_NODE");
    let secret = SecretKey::from_seed(&seed.0);
    let quorum_set = QuorumSetConfig {
        threshold_percent: 100,
        validators: vec![secret.public_key().to_strkey()],
        inner_sets: Vec::new(),
    };

    sim.add_app_node("node0", secret, quorum_set);
    sim.start_all_nodes().await;

    let app = sim.app("node0").expect("running app node");

    // Wait for the app to be ready.
    wait_for_app_state(&sim, "node0", AppState::Validating, Duration::from_secs(10)).await;
    assert_eq!(app.state().await, AppState::Validating);

    // Derive root account ID from the network passphrase (same derivation
    // as build_genesis_entries / TxGenerator::find_account).
    let network_id = henyey_common::NetworkId::from_passphrase("Test SDF Network ; September 2015");
    let root_sk = henyey_crypto::SecretKey::from_seed(network_id.as_bytes());
    let root_pk = root_sk.public_key();
    let root_account_id = stellar_xdr::AccountId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
        stellar_xdr::Uint256(*root_pk.as_bytes()),
    ));

    // This is the bug: load_account_sequence returns Ok(None) instead of
    // Ok(Some(0)) for the genesis root account.
    let result = app.load_account_sequence(&root_account_id);
    assert!(
        result.is_ok(),
        "load_account_sequence should not error: {:?}",
        result.err()
    );
    let seq = result.unwrap();
    assert!(
        seq.is_some(),
        "load_account_sequence must find the root account (bug #2357), got None"
    );
    // Genesis root starts at seq 0.
    assert_eq!(seq.unwrap(), 0, "root account initial sequence should be 0");

    // Now close a ledger and check again. The issue description says the bug
    // appears "after closing several ledgers".
    let closed = sim
        .manual_close_all_app_nodes()
        .await
        .expect("manual close");
    assert_eq!(closed, vec![2]);

    wait_for_app_ledger_close(&sim, 2, 0, Duration::from_secs(10)).await;

    // Check after ledger close
    let result_after = app.load_account_sequence(&root_account_id);
    assert!(
        result_after.is_ok(),
        "load_account_sequence should not error after close: {:?}",
        result_after.err()
    );
    let seq_after = result_after.unwrap();
    assert!(
        seq_after.is_some(),
        "load_account_sequence must find root account after ledger close (bug #2357)"
    );

    // Fund app accounts (step 2 in the issue's repro steps).
    let funded = sim
        .fund_app_accounts(1_000_000_000)
        .await
        .expect("fund app accounts");
    eprintln!("funded {} accounts", funded);

    // Close several more ledgers (step 3).
    for target in 3..=6 {
        sim.manual_close_all_app_nodes()
            .await
            .expect("manual close");
        wait_for_app_ledger_close(&sim, target, 0, Duration::from_secs(10)).await;
    }

    // Check after multiple ledger closes (step 4).
    let result_final = app.load_account_sequence(&root_account_id);
    assert!(
        result_final.is_ok(),
        "load_account_sequence should not error after several closes: {:?}",
        result_final.err()
    );
    let seq_final = result_final.unwrap();
    assert!(
        seq_final.is_some(),
        "load_account_sequence must find root account after several ledger closes (bug #2357)"
    );

    sim.stop_all_nodes().await.expect("stop nodes");
}

/// Regression test for #2357 with a pair topology: App::load_account_sequence
/// must return the root account's sequence number, not Ok(None).
#[tokio::test]
async fn test_load_account_sequence_pair_topology() {
    let mut sim =
        build_app_backed_topology(Topologies::pair(SimulationMode::OverLoopback), 100).await;

    // Close a few ledgers.
    manual_close_until(&sim, 3, 1, Duration::from_secs(20)).await;

    // Derive root account ID.
    let network_id = henyey_common::NetworkId::from_passphrase("Test SDF Network ; September 2015");
    let root_sk = henyey_crypto::SecretKey::from_seed(network_id.as_bytes());
    let root_pk = root_sk.public_key();
    let root_account_id = stellar_xdr::AccountId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
        stellar_xdr::Uint256(*root_pk.as_bytes()),
    ));

    // Check on both nodes.
    for node_id in ["node0", "node1"] {
        let app = sim.app(node_id).expect("app");
        let result = app.load_account_sequence(&root_account_id);
        assert!(
            result.is_ok(),
            "{node_id}: load_account_sequence error: {:?}",
            result.err()
        );
        let seq = result.unwrap();
        assert!(
            seq.is_some(),
            "{node_id}: load_account_sequence returned None for root account (bug #2357)"
        );
    }

    sim.stop_all_nodes().await.expect("stop nodes");
}

#[tokio::test]
async fn test_core3_app_simulation_starts_over_tcp() {
    let mut sim = build_app_backed_topology(Topologies::core3(SimulationMode::OverTcp), 67).await;

    let mut total_peers = 0usize;

    for id in ["node0", "node1", "node2"] {
        let app = sim.app(id).expect("running core3 app node");
        let status = sim.app_task_status(id).await;
        assert_eq!(
            sim.app_task_finished(id),
            Some(false),
            "{id} status: {status:?}"
        );
        assert!(matches!(
            app.state().await,
            AppState::Synced | AppState::Validating
        ));
        total_peers += sim.app_peer_count(id).await.unwrap_or(0);
    }

    assert!(
        total_peers > 0,
        "expected at least one active TCP peer connection"
    );

    sim.stop_all_nodes().await.expect("stop core3 app nodes");
}

#[tokio::test]
async fn test_three_nodes_two_running_threshold_two_over_tcp() {
    let mut sim = build_two_running_of_three(SimulationMode::OverTcp).await;

    manual_close_until(&sim, 2, 1, Duration::from_secs(20)).await;

    sim.stop_all_nodes().await.expect("stop two-of-three tcp");
}

#[tokio::test]
async fn test_three_nodes_two_running_threshold_two_over_loopback() {
    let mut sim = build_two_running_of_three(SimulationMode::OverLoopback).await;

    manual_close_until(&sim, 2, 1, Duration::from_secs(20)).await;

    sim.stop_all_nodes()
        .await
        .expect("stop two-of-three loopback");
}

#[tokio::test]
async fn test_core3_app_simulation_can_attempt_multi_node_close() {
    let mut sim = build_app_backed_topology(Topologies::core3(SimulationMode::OverTcp), 67).await;

    manual_close_until(&sim, 2, 1, Duration::from_secs(20)).await;
    sim.stop_all_nodes().await.expect("stop core3 app nodes");
}

#[tokio::test]
async fn test_pair_app_simulation_can_close_ledgers_over_tcp() {
    let mut sim = build_app_backed_topology(Topologies::pair(SimulationMode::OverTcp), 100).await;

    manual_close_until(&sim, 2, 1, Duration::from_secs(20)).await;

    sim.stop_all_nodes().await.expect("stop pair app nodes");
}

#[tokio::test]
async fn test_pair_app_simulation_can_close_ledgers_over_loopback() {
    let mut sim =
        build_app_backed_topology(Topologies::pair(SimulationMode::OverLoopback), 100).await;

    manual_close_until(&sim, 2, 1, Duration::from_secs(20)).await;

    sim.stop_all_nodes()
        .await
        .expect("stop pair loopback app nodes");
}

#[tokio::test]
async fn test_pair_app_simulation_executes_generated_load_over_tcp() {
    let mut sim = build_app_backed_topology(Topologies::pair(SimulationMode::OverTcp), 100).await;

    ensure_app_accounts_funded(&mut sim, 2).await;

    let steps = sim.generate_load_plan_for_app_nodes(1, 1, 100, 1_000);
    let submitted = sim
        .submit_generated_load_step(&steps[0])
        .await
        .expect("submit generated load step");
    assert_eq!(submitted, 1);

    let ledger_target = sim
        .app("node0")
        .expect("node0 app exists")
        .ledger_info()
        .ledger_seq
        + 1;
    manual_close_until(&sim, ledger_target, 1, Duration::from_secs(40)).await;

    sim.stop_all_nodes().await.expect("stop pair tcp load test");
}

#[tokio::test]
async fn test_pair_app_simulation_executes_generated_load_over_loopback() {
    let mut sim =
        build_app_backed_topology(Topologies::pair(SimulationMode::OverLoopback), 100).await;

    ensure_app_accounts_funded(&mut sim, 2).await;

    let steps = sim.generate_load_plan_for_app_nodes(1, 1, 100, 1_000);
    let submitted = sim
        .submit_generated_load_step(&steps[0])
        .await
        .expect("submit generated load step loopback");
    assert_eq!(submitted, 1);

    let ledger_target = sim
        .app("node0")
        .expect("node0 app exists")
        .ledger_info()
        .ledger_seq
        + 1;
    manual_close_until(&sim, ledger_target, 1, Duration::from_secs(40)).await;

    sim.stop_all_nodes()
        .await
        .expect("stop pair loopback load test");
}

#[tokio::test]
async fn test_core4_app_simulation_can_close_ledgers_over_tcp() {
    let mut sim = build_app_backed_topology(Topologies::core(4, SimulationMode::OverTcp), 75).await;

    manual_close_until(&sim, 2, 1, Duration::from_secs(20)).await;

    sim.stop_all_nodes().await.expect("stop core4 app nodes");
}

#[tokio::test]
async fn test_cycle4_app_simulation_can_close_ledgers_over_tcp() {
    let mut sim = build_app_backed_topology(Topologies::cycle4(SimulationMode::OverTcp), 75).await;

    manual_close_until(&sim, 2, 1, Duration::from_secs(30)).await;

    sim.stop_all_nodes().await.expect("stop cycle4 app nodes");
}

#[tokio::test]
async fn test_core3_app_simulation_can_close_ledgers_over_loopback() {
    let mut sim =
        build_app_backed_topology(Topologies::core3(SimulationMode::OverLoopback), 67).await;

    manual_close_until(&sim, 2, 1, Duration::from_secs(20)).await;

    sim.stop_all_nodes()
        .await
        .expect("stop core3 loopback app nodes");
}

#[tokio::test]
async fn test_separate_app_simulation_stays_partitioned_over_tcp() {
    let mut sim =
        build_app_backed_topology(Topologies::separate(SimulationMode::OverTcp), 75).await;

    let _ = sim
        .manual_close_all_app_nodes()
        .await
        .expect("manual close separate");

    assert_not_all_nodes_externalized(&sim, 2, 1, Duration::from_secs(5)).await;

    sim.stop_all_nodes().await.expect("stop separate app nodes");
}

#[tokio::test]
async fn test_separate_app_simulation_stays_partitioned_over_loopback() {
    let mut sim =
        build_app_backed_topology(Topologies::separate(SimulationMode::OverLoopback), 75).await;

    let _ = sim
        .manual_close_all_app_nodes()
        .await
        .expect("manual close separate loopback");

    assert_not_all_nodes_externalized(&sim, 2, 1, Duration::from_secs(5)).await;

    sim.stop_all_nodes()
        .await
        .expect("stop separate loopback app nodes");
}

#[tokio::test]
async fn test_core3_restart_rejoin_over_tcp() {
    let mut sim = build_app_backed_topology(Topologies::core3(SimulationMode::OverTcp), 66).await;

    manual_close_until(&sim, 2, 1, Duration::from_secs(45)).await;

    sim.remove_node("node0").await.expect("remove node0 tcp");
    wait_for_peer_count(
        &sim,
        "node1",
        1,
        Duration::from_secs(POST_REMOVE_PEER_DETECT_TIMEOUT_SECS),
    )
    .await;
    wait_for_peer_count(
        &sim,
        "node2",
        1,
        Duration::from_secs(POST_REMOVE_PEER_DETECT_TIMEOUT_SECS),
    )
    .await;

    manual_close_until(
        &sim,
        3,
        0,
        Duration::from_secs(TCP_POST_REMOVAL_CLOSE_TIMEOUT_SECS),
    )
    .await;

    sim.restart_node("node0").await.expect("restart node0 tcp");
    wait_for_app_operational(
        &sim,
        "node0",
        Duration::from_secs(POST_RESTART_OPERATIONAL_TIMEOUT_SECS),
    )
    .await;

    // Re-establish peer connections with retry (TCP connections can fail transiently).
    stabilize_or_panic(
        &sim,
        1,
        STABILIZE_RESTART_TIMEOUT_SECS,
        "test_core3_restart_rejoin_over_tcp",
    )
    .await;

    // Request SCP state so node0 learns about the externalized slots it missed.
    sim.app("node0")
        .expect("restarted node0 app")
        .request_scp_state_from_peers()
        .await;

    // Wait for node0 to catch up to ledger 3 (where node1/node2 are).
    // 60s timeout: post-restart catchup can be slow on CI runners.
    wait_for_app_ledger_close(&sim, 3, 1, Duration::from_secs(60)).await;

    // Verify SCP envelopes traversed the pump_scp_intake pipeline during recovery.
    // restart_node() creates a fresh App with zero counters, so any positive
    // total proves the intake pipeline worked.
    assert_scp_intake_reached(&sim.app("node0").expect("node0 for post-check"), "node0");

    // Now advance all nodes to ledger 4.
    manual_close_until(&sim, 4, 1, Duration::from_secs(60)).await;

    sim.stop_all_nodes()
        .await
        .expect("stop core3 tcp restart test");
}

#[tokio::test]
#[serial]
async fn test_core3_restart_rejoin_over_loopback() {
    let mut sim =
        build_app_backed_topology(Topologies::core3(SimulationMode::OverLoopback), 66).await;

    manual_close_until(&sim, 2, 1, Duration::from_secs(45)).await;

    sim.remove_node("node0")
        .await
        .expect("remove node0 loopback");
    wait_for_peer_count(
        &sim,
        "node1",
        1,
        Duration::from_secs(POST_REMOVE_PEER_DETECT_TIMEOUT_SECS),
    )
    .await;
    wait_for_peer_count(
        &sim,
        "node2",
        1,
        Duration::from_secs(POST_REMOVE_PEER_DETECT_TIMEOUT_SECS),
    )
    .await;

    manual_close_until(&sim, 3, 0, Duration::from_secs(45)).await;

    sim.restart_node("node0")
        .await
        .expect("restart node0 loopback");
    wait_for_app_operational(
        &sim,
        "node0",
        Duration::from_secs(POST_RESTART_OPERATIONAL_TIMEOUT_SECS),
    )
    .await;

    // Re-establish peer connections.
    let _ = sim.add_connection("node0", "node1").await;
    let _ = sim.add_connection("node0", "node2").await;

    // Wait for peer connections to be fully established before requesting
    // SCP state. add_connection() spawns the handshake asynchronously, so
    // without this wait request_scp_state_from_peers() can find zero peers
    // and silently return without requesting any state.
    wait_for_peer_count(
        &sim,
        "node0",
        2,
        Duration::from_secs(POST_RESTART_PEER_CONNECT_TIMEOUT_SECS),
    )
    .await;

    // Request SCP state so node0 learns about externalized slots it missed.
    sim.app("node0")
        .expect("restarted node0 app")
        .request_scp_state_from_peers()
        .await;

    // Wait for node0 to catch up to ledger 3 before triggering ledger 4.
    // 60s timeout: post-restart catchup can be slow on CI runners.
    wait_for_app_ledger_close(&sim, 3, 1, Duration::from_secs(60)).await;

    // Verify SCP envelopes traversed the pump_scp_intake pipeline during recovery.
    // restart_node() creates a fresh App with zero counters, so any positive
    // total proves the intake pipeline worked.
    assert_scp_intake_reached(&sim.app("node0").expect("node0 for post-check"), "node0");

    // Now advance all nodes to ledger 4.
    manual_close_until(&sim, 4, 1, Duration::from_secs(60)).await;

    sim.stop_all_nodes()
        .await
        .expect("stop core3 loopback restart test");
}

#[tokio::test]
async fn test_wait_for_app_connectivity_returns_error_on_timeout() {
    let mut sim = Topologies::core3(SimulationMode::OverTcp);
    sim.populate_app_nodes_from_existing(67);
    sim.start_all_nodes().await;
    // Request more peers than possible (3 nodes, asking for 10)
    let result = sim
        .wait_for_app_connectivity(10, Duration::from_millis(500))
        .await;
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("not all apps reached"));
    sim.stop_all_nodes().await.ok();
}

#[tokio::test]
async fn test_wait_for_app_connectivity_zero_apps_succeeds() {
    let sim = Topologies::core3(SimulationMode::OverTcp);
    // No app nodes started — running_apps is empty → vacuous success
    let result = sim
        .wait_for_app_connectivity(5, Duration::from_millis(100))
        .await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_stabilize_app_tcp_connectivity_returns_error_on_timeout() {
    let mut sim = Topologies::core3(SimulationMode::OverTcp);
    sim.populate_app_nodes_from_existing(67);
    sim.start_all_nodes().await;
    // Request impossible peer count — should timeout without panic
    let result = sim
        .stabilize_app_tcp_connectivity(100, Duration::from_millis(500))
        .await;
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("did not stabilize"),
        "expected 'did not stabilize' in error: {err_msg}"
    );
    // Verify per-node diagnostics are propagated
    assert!(
        err_msg.contains("not all apps reached"),
        "expected per-node detail in error: {err_msg}"
    );
    sim.stop_all_nodes().await.ok();
}

#[tokio::test]
async fn test_task_exit_detection_during_wait_for_connectivity() {
    let mut sim = Topologies::core3(SimulationMode::OverTcp);
    sim.populate_app_nodes_from_existing(67);
    sim.start_all_nodes().await;

    // Abort one node's task so it exits (but remains in running_apps).
    let ids = sim.app_node_ids();
    let victim = &ids[0];
    sim.abort_node_task(victim);

    // Wait for the task to register as finished.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if sim.app_task_finished(victim) == Some(true) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        sim.app_task_finished(victim),
        Some(true),
        "node {victim} task did not exit after abort"
    );

    // Call wait_for_app_connectivity with a long timeout — should fail fast.
    let start = tokio::time::Instant::now();
    let result = sim
        .wait_for_app_connectivity(1, Duration::from_secs(10))
        .await;
    let elapsed = start.elapsed();

    assert!(result.is_err(), "expected error from exited node");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("task exited"),
        "expected 'task exited' in error: {err_msg}"
    );
    assert!(
        err_msg.contains(victim),
        "expected node ID '{victim}' in error: {err_msg}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "fail-fast took too long: {elapsed:?}"
    );

    sim.stop_all_nodes().await.ok();
}

#[tokio::test]
async fn test_task_exit_detection_during_stabilize_connectivity() {
    let mut sim = Topologies::core3(SimulationMode::OverTcp);
    sim.populate_app_nodes_from_existing(67);
    sim.start_all_nodes().await;

    // Abort one node's task so it exits (but remains in running_apps).
    let ids = sim.app_node_ids();
    let victim = &ids[0];
    sim.abort_node_task(victim);

    // Wait for the task to register as finished.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if sim.app_task_finished(victim) == Some(true) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        sim.app_task_finished(victim),
        Some(true),
        "node {victim} task did not exit after abort"
    );

    // Call stabilize_app_tcp_connectivity with a long timeout — should fail fast.
    let start = tokio::time::Instant::now();
    let result = sim
        .stabilize_app_tcp_connectivity(1, Duration::from_secs(10))
        .await;
    let elapsed = start.elapsed();

    assert!(result.is_err(), "expected error from exited node");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("task exited"),
        "expected 'task exited' in error: {err_msg}"
    );
    assert!(
        err_msg.contains(victim),
        "expected node ID '{victim}' in error: {err_msg}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "fail-fast took too long: {elapsed:?}"
    );

    sim.stop_all_nodes().await.ok();
}

#[tokio::test]
async fn test_wait_for_topology_connectivity_zero_apps_succeeds() {
    let sim = Topologies::core3(SimulationMode::OverTcp);
    // No app nodes started — running_apps is empty → vacuous success
    let result = sim
        .wait_for_topology_connectivity(Duration::from_millis(100))
        .await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_wait_for_topology_connectivity_returns_error_on_timeout() {
    let mut sim = Topologies::core3(SimulationMode::OverTcp);
    sim.populate_app_nodes_from_existing(67);
    sim.start_all_nodes().await;
    // Immediately check before connections establish — should timeout
    let result = sim
        .wait_for_topology_connectivity(Duration::from_millis(0))
        .await;
    // Whether it succeeds or times out depends on how fast connections are,
    // but the function should not panic either way.
    if let Err(err) = result {
        let err_msg = err.to_string();
        assert!(
            err_msg.contains("topology mismatch"),
            "expected 'topology mismatch' in error: {err_msg}"
        );
    }
    sim.stop_all_nodes().await.ok();
}

#[tokio::test]
async fn test_wait_for_topology_connectivity_detects_crash() {
    let mut sim = Topologies::core3(SimulationMode::OverTcp);
    sim.populate_app_nodes_from_existing(67);
    sim.start_all_nodes().await;

    let ids = sim.app_node_ids();
    let victim = &ids[0];
    sim.abort_node_task(victim);

    // Wait for the task to register as finished.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if sim.app_task_finished(victim) == Some(true) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        sim.app_task_finished(victim),
        Some(true),
        "node {victim} task did not exit after abort"
    );

    // Should fail fast with task-exited error, not wait for timeout.
    let start = tokio::time::Instant::now();
    let result = sim
        .wait_for_topology_connectivity(Duration::from_secs(10))
        .await;
    let elapsed = start.elapsed();

    assert!(result.is_err(), "expected error from exited node");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("task exited"),
        "expected 'task exited' in error: {err_msg}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "fail-fast took too long: {elapsed:?}"
    );

    sim.stop_all_nodes().await.ok();
}

#[tokio::test]
async fn test_stabilize_app_topology_connectivity_returns_error_on_timeout() {
    let mut sim = Topologies::core3(SimulationMode::OverTcp);
    sim.populate_app_nodes_from_existing(67);
    sim.start_all_nodes().await;

    // Abort a node to make topology unreachable.
    let ids = sim.app_node_ids();
    let victim = &ids[0];
    sim.abort_node_task(victim);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if sim.app_task_finished(victim) == Some(true) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let result = sim
        .stabilize_app_topology_connectivity(Duration::from_secs(10))
        .await;
    assert!(result.is_err(), "expected error from exited node");
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("task exited"),
        "expected 'task exited' in error: {err_msg}"
    );

    sim.stop_all_nodes().await.ok();
}

/// After `start_all_nodes()` returns, all overlays must be started and
/// `repair_app_connectivity()` must not observe `overlay_not_ready`.
#[tokio::test]
async fn test_start_all_nodes_overlays_ready() {
    for mode in [SimulationMode::OverLoopback, SimulationMode::OverTcp] {
        let mut sim = Topologies::pair(mode);
        sim.populate_app_nodes_from_existing(100);
        sim.start_all_nodes().await;

        // All overlays must be started after start_all_nodes returns.
        for id in sim.app_node_ids() {
            assert!(
                sim.is_app_overlay_started(&id).await,
                "overlay not started for {id} after start_all_nodes ({mode:?})"
            );
        }

        sim.stop_all_nodes().await.ok();
    }
}

/// After `restart_node()` returns, the restarted node's overlay must be
/// started and an immediate connection must not return `NotStarted`.
#[tokio::test]
async fn test_restart_node_overlay_ready() {
    for mode in [SimulationMode::OverLoopback, SimulationMode::OverTcp] {
        let mut sim = build_app_backed_topology(Topologies::pair(mode), 100).await;
        let ids = sim.app_node_ids();
        let target = &ids[0];
        let other = &ids[1];

        sim.restart_node(target).await.expect("restart failed");

        assert!(
            sim.is_app_overlay_started(target).await,
            "overlay not started for restarted node {target} ({mode:?})"
        );

        // Verify an immediate connection attempt succeeds (no NotStarted).
        sim.add_connection(target, other)
            .await
            .expect("add_connection after restart should not return NotStarted");

        sim.stop_all_nodes().await.ok();
    }
}

#[tokio::test]
async fn test_topology_connectivity_detects_cross_partition_connection() {
    // Create a separate topology: two isolated pairs (node0-node1, node2-node3).
    let mut sim =
        build_app_backed_topology(Topologies::separate(SimulationMode::OverTcp), 75).await;

    // Verify the topology-aware check passes with correct connections.
    sim.wait_for_topology_connectivity(Duration::from_millis(500))
        .await
        .expect("topology should match before modification");

    // Modify the topology definition to add a link that doesn't exist in
    // reality. Now expected_peer_ids("node0") returns {node1, node2} but
    // node0 is only connected to node1 → "missing" node2.
    sim.add_pending_connection("node0", "node2");

    // The topology-aware check should now fail because node0 is missing node2.
    let result = sim
        .wait_for_topology_connectivity(Duration::from_millis(500))
        .await;
    assert!(
        result.is_err(),
        "expected topology mismatch after adding non-existent link"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("missing"),
        "expected 'missing' peer in error diagnostics: {err_msg}"
    );

    sim.stop_all_nodes().await.expect("stop separate nodes");
}

#[tokio::test]
#[serial]
async fn test_task_exit_detection_during_partition_assertion() {
    use futures::FutureExt;
    use std::panic::AssertUnwindSafe;

    let mut sim = Topologies::core3(SimulationMode::OverTcp);
    sim.populate_app_nodes_from_existing(67);
    sim.start_all_nodes().await;

    // Abort one node's task so it exits (but remains in running_apps).
    let ids = sim.app_node_ids();
    let victim = ids[0].clone();
    sim.abort_node_task(&victim);

    // Wait for the task to register as finished.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if sim.app_task_finished(&victim) == Some(true) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        sim.app_task_finished(&victim),
        Some(true),
        "node {victim} task did not exit after abort"
    );

    // Call assert_not_all_nodes_externalized with an unattainable target
    // ledger so it can only panic from crash detection, not from the
    // "all nodes externalized" path.
    let result = AssertUnwindSafe(assert_not_all_nodes_externalized(
        &sim,
        9999,
        1,
        Duration::from_secs(5),
    ))
    .catch_unwind()
    .await;

    assert!(result.is_err(), "expected panic from crashed node");
    let panic_payload = result.unwrap_err();
    let msg = panic_payload
        .downcast_ref::<String>()
        .map(|s| s.as_str())
        .or_else(|| panic_payload.downcast_ref::<&str>().copied())
        .unwrap_or("<non-string panic>");
    assert!(
        msg.contains("task exited"),
        "expected 'task exited' in panic: {msg}"
    );
    assert!(
        msg.contains(&victim),
        "expected node ID '{victim}' in panic: {msg}"
    );

    sim.stop_all_nodes().await.ok();
}

// ---------------------------------------------------------------------------
// Shared helpers for Supercluster-inspired tests
// ---------------------------------------------------------------------------

/// Submit a load step, close one ledger, and assert all nodes externalize.
/// Returns the number of transactions accepted by the queue.
async fn submit_and_close(
    sim: &mut Simulation,
    step: &LoadStep,
    spread: u32,
    timeout: Duration,
) -> usize {
    let submitted = sim
        .submit_generated_load_step(step)
        .await
        .expect("submit load step");
    let target = sim
        .app("node0")
        .expect("node0 for target ledger")
        .ledger_info()
        .ledger_seq
        + 1;
    manual_close_until(sim, target, spread, timeout).await;
    submitted
}

/// Assert all app nodes are healthy: task running, overlay connected,
/// and in an operational state (Synced or Validating).
async fn assert_all_nodes_healthy(sim: &Simulation) {
    for id in sim.app_node_ids() {
        assert_eq!(
            sim.app_task_finished(&id),
            Some(false),
            "{id} should still be running"
        );
        assert!(
            sim.app_peer_count(&id).await.unwrap_or(0) > 0,
            "{id} should have peers"
        );
        let state = sim.app(&id).expect("{id} app").state().await;
        assert!(
            state == AppState::Synced || state == AppState::Validating,
            "{id} should be operational, got {state}"
        );
    }
}

// ---------------------------------------------------------------------------
// Supercluster-inspired simulation tests
// ---------------------------------------------------------------------------

/// Core3 payment: submit 3 txs, close a ledger, verify all accepted.
#[tokio::test]
async fn test_simple_payment_app_backed_core3() {
    let mut sim =
        build_app_backed_topology(Topologies::core3(SimulationMode::OverLoopback), 67).await;

    ensure_app_accounts_funded(&mut sim, 3).await;

    let steps = sim.generate_load_plan_for_app_nodes(3, 1, 100, 1_000);
    let accepted = submit_and_close(&mut sim, &steps[0], 1, Duration::from_secs(30)).await;
    assert_eq!(accepted, 3, "all 3 txs should be accepted");

    assert_all_nodes_healthy(&sim).await;

    sim.stop_all_nodes().await.expect("stop core3 payment test");
}

/// 4-node flat-quorum payment load across 3 consecutive steps.
///
/// Exercises a larger quorum (4 nodes, 75% threshold) under multi-step load,
/// complementing the existing single-step 4-node test and the 3-node
/// sustained-load test.
#[tokio::test]
async fn test_core4_multi_step_payment_load() {
    let mut sim = build_app_backed_topology(
        Topologies::core(4, SimulationMode::OverLoopback),
        75, // ceil(3/4) = 75%
    )
    .await;

    ensure_app_accounts_funded(&mut sim, 4).await;

    let steps = sim.generate_load_plan_for_app_nodes(4, 3, 100, 1_000);

    for (i, step) in steps.iter().enumerate() {
        let accepted = submit_and_close(&mut sim, step, 1, Duration::from_secs(30)).await;
        assert_eq!(accepted, 4, "step {i}: all 4 txs should be accepted");
    }

    assert_all_nodes_healthy(&sim).await;

    sim.stop_all_nodes()
        .await
        .expect("stop core4 multi-step test");
}

/// 5-node OverLoopback: validates that the larger outbound channel capacity
/// (2048 for loopback vs 256 for TCP) prevents SCP relay drops that cause
/// nodes to fall into CatchingUp. See issue #2356.
#[tokio::test]
async fn test_core5_over_loopback_can_close_ledgers() {
    let mut sim =
        build_app_backed_topology(Topologies::core(5, SimulationMode::OverLoopback), 80).await;

    // Close ledgers 3-10 (8 closes, exercises multiple SCP rounds).
    manual_close_until(&sim, 10, 2, Duration::from_secs(120)).await;

    // Verify no node is stuck in CatchingUp.
    for id in sim.app_node_ids() {
        if let Some(stats) = sim.app_debug_stats(&id).await {
            assert_ne!(
                stats.app_state, "Catching Up",
                "node {id} should not be in CatchingUp after 5-node consensus"
            );
        }
    }

    sim.stop_all_nodes()
        .await
        .expect("stop core5 loopback test");
}

/// 5-step sustained load across consecutive ledger closes.
#[tokio::test]
async fn test_sustained_payment_load_app_backed() {
    let mut sim =
        build_app_backed_topology(Topologies::core3(SimulationMode::OverLoopback), 67).await;

    ensure_app_accounts_funded(&mut sim, 3).await;

    let steps = sim.generate_load_plan_for_app_nodes(3, 5, 100, 1_000);

    for (i, step) in steps.iter().enumerate() {
        let accepted = submit_and_close(&mut sim, step, 1, Duration::from_secs(30)).await;
        assert_eq!(accepted, 3, "step {i}: all 3 txs should be accepted");
    }

    assert_all_nodes_healthy(&sim).await;

    sim.stop_all_nodes()
        .await
        .expect("stop sustained load test");
}

/// Burst-then-normal payment pattern (spike load approximation).
///
/// Uses a 4-node topology so the spike step (4 txs) can use one tx per
/// account, avoiding queue contention. Normal steps use only the first 2
/// accounts (2 txs).
#[tokio::test]
async fn test_spike_payment_load_app_backed() {
    let mut sim =
        build_app_backed_topology(Topologies::core(4, SimulationMode::OverLoopback), 75).await;

    ensure_app_accounts_funded(&mut sim, 4).await;

    let all_accounts = sim.app_node_ids();
    let normal_accounts: Vec<String> = all_accounts.iter().take(2).cloned().collect();

    // Normal step: 2 txs on first 2 accounts.
    let normal_config = GeneratedLoadConfig {
        accounts: normal_accounts.clone(),
        txs_per_step: 2,
        steps: 1,
        fee_bid: 100,
        amount: 1_000,
        ..Default::default()
    };
    let normal_steps = LoadGenerator::step_plan(&normal_config);

    // Spike step: 4 txs on all 4 accounts.
    let spike_config = GeneratedLoadConfig {
        accounts: all_accounts.clone(),
        txs_per_step: 4,
        steps: 1,
        fee_bid: 100,
        amount: 1_000,
        ..Default::default()
    };
    let spike_steps = LoadGenerator::step_plan(&spike_config);

    // Round 1: normal (2 txs).
    let accepted = submit_and_close(&mut sim, &normal_steps[0], 1, Duration::from_secs(30)).await;
    assert_eq!(accepted, 2, "normal round 1");

    // Round 2: spike (4 txs).
    let accepted = submit_and_close(&mut sim, &spike_steps[0], 1, Duration::from_secs(30)).await;
    assert_eq!(accepted, 4, "spike round");

    // Round 3: normal (2 txs).
    let normal_steps_2 = LoadGenerator::step_plan(&normal_config);
    let accepted = submit_and_close(&mut sim, &normal_steps_2[0], 1, Duration::from_secs(30)).await;
    assert_eq!(accepted, 2, "normal round 2");

    assert_all_nodes_healthy(&sim).await;

    sim.stop_all_nodes().await.expect("stop spike load test");
}

/// Tx queue contention: back-to-back submissions without closing.
///
/// Validates the one-pending-tx-per-account `TryAgainLater` behavior.
#[tokio::test]
async fn test_tx_queue_contention_app_backed() {
    let mut sim =
        build_app_backed_topology(Topologies::core3(SimulationMode::OverLoopback), 67).await;

    ensure_app_accounts_funded(&mut sim, 3).await;

    // Generate 3 independent load plans (3 txs each).
    let plan1 = sim.generate_load_plan_for_app_nodes(3, 1, 100, 1_000);
    let plan2 = sim.generate_load_plan_for_app_nodes(3, 1, 100, 1_000);
    let plan3 = sim.generate_load_plan_for_app_nodes(3, 1, 100, 1_000);

    // Submit all 3 back-to-back without closing.
    let accepted1 = sim
        .submit_generated_load_step(&plan1[0])
        .await
        .expect("submit step 1");
    let accepted2 = sim
        .submit_generated_load_step(&plan2[0])
        .await
        .expect("submit step 2");
    let accepted3 = sim
        .submit_generated_load_step(&plan3[0])
        .await
        .expect("submit step 3");

    // First batch: all 3 accepted (one per account).
    assert_eq!(accepted1, 3, "first batch should accept all 3");
    // Subsequent batches: rejected (one-pending-tx-per-account).
    assert_eq!(accepted2, 0, "second batch rejected (TryAgainLater)");
    assert_eq!(accepted3, 0, "third batch rejected (TryAgainLater)");

    // Close a ledger to apply the pending transactions.
    let target = sim.app("node0").expect("node0").ledger_info().ledger_seq + 1;
    manual_close_until(&sim, target, 1, Duration::from_secs(30)).await;

    assert_all_nodes_healthy(&sim).await;

    sim.stop_all_nodes()
        .await
        .expect("stop queue contention test");
}

/// Lagging node recovery: remove a node, submit payment transactions to
/// the majority, then restart the lagging node and verify it catches up
/// with non-trivial ledger content.
///
/// Builds on the existing `test_core3_restart_rejoin_over_loopback` pattern
/// by advancing with real payment load (not empty closes) while the node
/// is down, exercising catch-up with actual transaction data.
#[tokio::test]
#[serial]
async fn test_slow_node_lagging_node_recovers() {
    let mut sim =
        build_app_backed_topology(Topologies::core3(SimulationMode::OverLoopback), 66).await;

    // Fund accounts so we can submit payments.
    ensure_app_accounts_funded(&mut sim, 3).await;

    // Close to ledger 2 + funding rounds so all nodes are in sync.
    let base_ledger = sim.app("node0").expect("node0").ledger_info().ledger_seq;

    // Remove node0 (simulates a lagging/crashed node).
    sim.remove_node("node0")
        .await
        .expect("remove node0 to simulate lag");
    wait_for_peer_count(
        &sim,
        "node1",
        1,
        Duration::from_secs(POST_REMOVE_PEER_DETECT_TIMEOUT_SECS),
    )
    .await;
    wait_for_peer_count(
        &sim,
        "node2",
        1,
        Duration::from_secs(POST_REMOVE_PEER_DETECT_TIMEOUT_SECS),
    )
    .await;

    // Submit payment load to the majority and close 2 ledgers.
    // This ensures the lagging node must catch up with real tx data.
    let steps = sim.generate_load_plan_for_app_nodes(3, 2, 100, 1_000);
    for step in &steps {
        // Submit to node1 (node0 is down, but submit_generated_load_step
        // distributes across running nodes).
        let _ = sim
            .submit_generated_load_step(step)
            .await
            .expect("submit load while node0 down");
        let target = sim.app("node1").expect("node1").ledger_info().ledger_seq + 1;
        manual_close_until(&sim, target, 0, Duration::from_secs(45)).await;
    }

    let majority_ledger = sim.app_ledger_seq("node1").unwrap_or(0);
    assert!(
        majority_ledger > base_ledger,
        "majority should have advanced: {majority_ledger} > {base_ledger}"
    );

    // Restart node0 (simulates the lagging node recovering).
    sim.restart_node("node0")
        .await
        .expect("restart node0 to recover");
    wait_for_app_operational(
        &sim,
        "node0",
        Duration::from_secs(POST_RESTART_OPERATIONAL_TIMEOUT_SECS),
    )
    .await;

    // Re-establish peer connections.
    let _ = sim.add_connection("node0", "node1").await;
    let _ = sim.add_connection("node0", "node2").await;
    wait_for_peer_count(
        &sim,
        "node0",
        2,
        Duration::from_secs(POST_RESTART_PEER_CONNECT_TIMEOUT_SECS),
    )
    .await;

    // Request SCP state so node0 learns about missed slots.
    sim.app("node0")
        .expect("node0 app")
        .request_scp_state_from_peers()
        .await;

    // Wait for node0 to catch up to the majority's ledger.
    wait_for_app_ledger_close(&sim, majority_ledger, 1, Duration::from_secs(60)).await;

    // Verify SCP envelopes traversed the pump_scp_intake pipeline during recovery.
    // restart_node() creates a fresh App with zero counters, so any positive
    // total proves the intake pipeline worked.
    assert_scp_intake_reached(&sim.app("node0").expect("node0 for post-check"), "node0");

    // Close one more ledger to confirm full sync with all 3 nodes.
    manual_close_until(&sim, majority_ledger + 1, 1, Duration::from_secs(60)).await;

    sim.stop_all_nodes().await.expect("stop lagging node test");
}

/// End-to-end test that two validators exchanging SCP messages over TCP
/// exercise the full `pump_scp_intake` accept path across multiple slots.
///
/// Covers the app-level SCP pipeline:
///   overlay → scp_message_rx → pump_scp_intake → pre-filter → verify
///   → process_verified → SCP state machine
///
/// Complements the overlay-level self-echo test
/// (`test_scp_self_echo_not_dropped_after_broadcast`) which only covers
/// FloodGate routing. This test proves peer-sourced SCP envelopes reach
/// the SCP state machine and are accepted (`PostVerifyReason::Accepted`).
///
/// Regression context: #2317, #2325, #2364.
#[tokio::test]
async fn test_pair_tcp_scp_messages_exercise_pump_scp_intake() {
    let mut sim = build_app_backed_topology(Topologies::pair(SimulationMode::OverTcp), 100).await;

    // Baseline: capture SCP counters after startup to isolate manual-close traffic.
    let app_0 = sim.app("node0").unwrap();
    let app_1 = sim.app("node1").unwrap();
    let base_accepted_0 = app_0.info().scp_verify.pv_counters[PostVerifyReason::Accepted];
    let base_accepted_1 = app_1.info().scp_verify.pv_counters[PostVerifyReason::Accepted];
    let base_stats_0 = sim.app_debug_stats("node0").await.unwrap();
    let base_stats_1 = sim.app_debug_stats("node1").await.unwrap();

    // Close 5 ledgers: from genesis (ledger 1) through ledger 6.
    // max_spread = 1 follows the convention of existing pair tests.
    manual_close_until(&sim, 6, 1, Duration::from_secs(60)).await;

    // Per-node assertions: each node must have exchanged SCP messages and
    // accepted peer envelopes through the full pump_scp_intake pipeline.
    let post_accepted_0 = app_0.info().scp_verify.pv_counters[PostVerifyReason::Accepted];
    let post_accepted_1 = app_1.info().scp_verify.pv_counters[PostVerifyReason::Accepted];
    let stats_0 = sim.app_debug_stats("node0").await.unwrap();
    let stats_1 = sim.app_debug_stats("node1").await.unwrap();

    let delta_accepted_0 = post_accepted_0 - base_accepted_0;
    let delta_accepted_1 = post_accepted_1 - base_accepted_1;
    let delta_sent_0 = stats_0.scp_messages_sent - base_stats_0.scp_messages_sent;
    let delta_sent_1 = stats_1.scp_messages_sent - base_stats_1.scp_messages_sent;

    assert!(
        delta_accepted_0 > 0,
        "node0 must have accepted peer SCP envelopes through pump_scp_intake \
         (pv_counters[Accepted] delta = {delta_accepted_0})"
    );
    assert!(
        delta_accepted_1 > 0,
        "node1 must have accepted peer SCP envelopes through pump_scp_intake \
         (pv_counters[Accepted] delta = {delta_accepted_1})"
    );
    assert!(
        delta_sent_0 > 0,
        "node0 must have broadcast SCP envelopes (sent delta = {delta_sent_0})"
    );
    assert!(
        delta_sent_1 > 0,
        "node1 must have broadcast SCP envelopes (sent delta = {delta_sent_1})"
    );

    // Both nodes must have reached at least ledger 6.
    assert!(
        stats_0.current_ledger >= 6,
        "node0 current_ledger = {}, expected >= 6",
        stats_0.current_ledger
    );
    assert!(
        stats_1.current_ledger >= 6,
        "node1 current_ledger = {}, expected >= 6",
        stats_1.current_ledger
    );

    sim.stop_all_nodes()
        .await
        .expect("stop pair pump_scp_intake test nodes");
}

/// App-level self-echo test: proves that a node's own SCP envelope, echoed
/// back by a peer, traverses the full `pump_scp_intake → pre-filter → verify
/// → process_verified` pipeline and is correctly classified as
/// `PostVerifyReason::SelfMessage`.
///
/// Coverage supplement to:
/// - overlay-level `test_scp_self_echo_not_dropped_after_broadcast` (FloodGate only)
/// - `test_pair_tcp_scp_messages_exercise_pump_scp_intake` (peer-originated only)
///
/// Regression context: #2317, #2364, #2374.
#[tokio::test]
async fn test_self_echo_scp_reaches_pump_scp_intake() {
    use stellar_xdr::{NodeId, StellarMessage};

    let mut sim = build_app_backed_topology(Topologies::pair(SimulationMode::OverTcp), 100).await;

    // Close ledgers so both nodes have SCP envelopes in memory.
    manual_close_until(&sim, 5, 1, Duration::from_secs(30)).await;

    let app_0 = sim.app("node0").unwrap();
    let app_1 = sim.app("node1").unwrap();

    // Derive node0's NodeId (matches herder's node_id_from_public_key).
    let pk_0 = app_0.public_key();
    let node0_id = NodeId(stellar_xdr::PublicKey::from(&pk_0));

    // Find an envelope authored by node0 from node1's SCP state.
    // Node0's own SCP slot doesn't store self-authored envelopes (they're
    // emitted directly, not via process_envelope). But node1 received them
    // as peer messages, so node1's slot DOES have node0's envelopes.
    let latest_slot = app_0
        .latest_externalized_slot()
        .expect("node0 must have externalized at least one slot");
    let envelopes = app_1.get_scp_envelopes(latest_slot);
    let self_envelope = envelopes
        .into_iter()
        .find(|e| e.statement.node_id == node0_id)
        .expect("node1 must have received at least one SCP envelope from node0 in the latest slot");

    // Derive node0's PeerId for directed send from node1.
    let node0_peer_id = henyey_overlay::PeerId::from_bytes(*pk_0.as_bytes());

    // Baseline SelfMessage counter before injection.
    let baseline = app_0.info().scp_verify.pv_counters[PostVerifyReason::SelfMessage];

    // Inject: node1 echoes node0's own envelope back to node0.
    // Deadline-based retry in case the outbound channel is temporarily full.
    let send_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        match app_1
            .try_send_to_peer(
                &node0_peer_id,
                StellarMessage::ScpMessage(self_envelope.clone()),
            )
            .await
        {
            Ok(()) => break,
            Err(e) => {
                if tokio::time::Instant::now() >= send_deadline {
                    panic!("try_send_to_peer failed after 2s deadline: {e}");
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }

    // Poll until SelfMessage counter increments (crash-aware via poll_until).
    let outcome = poll_until(
        &sim,
        Duration::from_secs(5),
        Duration::from_millis(20),
        CrashScope::SingleNode("node0"),
        || async {
            let current = app_0.info().scp_verify.pv_counters[PostVerifyReason::SelfMessage];
            if current > baseline {
                Ok(Some(current))
            } else {
                Ok(None)
            }
        },
    )
    .await
    .expect("fatal error during SelfMessage polling");

    match outcome {
        PollOutcome::Satisfied(_) => {}
        PollOutcome::NodeExited { node_id, status } => {
            panic!("node {node_id} crashed during SelfMessage polling (status: {status:?})");
        }
        PollOutcome::TimedOut => {
            let final_val = app_0.info().scp_verify.pv_counters[PostVerifyReason::SelfMessage];
            panic!(
                "SelfMessage counter did not increment within 5s \
                 (baseline={baseline}, final={final_val}, slot={latest_slot})"
            );
        }
    }

    sim.stop_all_nodes()
        .await
        .expect("stop self-echo test nodes");
}

/// Regression test for #2491: concurrent TCP simulations must not collide on
/// ports.  Two independent 3-node simulations start in parallel; if ephemeral
/// port assignment works correctly each one binds unique OS-assigned ports and
/// both reach the Validating state without interfering with each other.
#[tokio::test]
async fn test_concurrent_tcp_simulations_no_port_conflict() {
    let start_sim = |label: &'static str| async move {
        let mut sim = Topologies::core3(SimulationMode::OverTcp);
        sim.populate_app_nodes_from_existing(67);
        sim.start_all_nodes().await;

        wait_for_all_app_state(&sim, AppState::Validating, Duration::from_secs(30), label).await;
        sim.stop_all_nodes()
            .await
            .unwrap_or_else(|e| panic!("{label}: stop all nodes: {e}"));
    };

    let ((), ()) = tokio::join!(start_sim("sim-A"), start_sim("sim-B"));
}
