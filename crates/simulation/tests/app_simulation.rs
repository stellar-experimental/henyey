use std::time::Duration;

use henyey_app::config::QuorumSetConfig;
use henyey_app::AppState;
use henyey_common::Hash256;
use henyey_crypto::SecretKey;
use henyey_simulation::{Simulation, SimulationMode, Topologies};

/// Timeout for the post-remove_node ledger close over TCP.
/// Conservative guess (2× the original 45s) to absorb CI jitter.
/// See follow-up investigation issue for root cause analysis.
const TCP_POST_REMOVAL_CLOSE_TIMEOUT_SECS: u64 = 90;

async fn wait_for_app_ledger_close(sim: &Simulation, target_ledger: u32, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if sim.have_all_app_nodes_externalized(target_ledger, 1) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let diag = collect_node_diagnostics(sim).await;
    assert!(
        sim.have_all_app_nodes_externalized(target_ledger, 1),
        "timed out after {timeout:?} waiting for ledger {target_ledger}.{diag}"
    );
}

async fn manual_close_until(
    sim: &Simulation,
    target_ledger: u32,
    max_spread: u32,
    timeout: Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last_err: Option<String> = None;
    while tokio::time::Instant::now() < deadline {
        if sim.have_all_app_nodes_externalized(target_ledger, max_spread) {
            return;
        }
        if let Err(e) = sim.manual_close_all_app_nodes().await {
            last_err = Some(e.to_string());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut diag = collect_node_diagnostics(sim).await;
    if let Some(err) = &last_err {
        diag.push_str(&format!("\n  last manual_close error: {err}"));
    }
    assert!(
        sim.have_all_app_nodes_externalized(target_ledger, max_spread),
        "manual_close_until timed out after {timeout:?} waiting for ledger {target_ledger}.{diag}"
    );
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
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if let Some(app) = sim.app(node_id) {
            if matches!(app.state().await, AppState::Synced | AppState::Validating) {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let app = sim.app(node_id).expect("app exists for operational wait");
    assert!(matches!(
        app.state().await,
        AppState::Synced | AppState::Validating
    ));
}

async fn wait_for_peer_count(sim: &Simulation, node_id: &str, expected: usize, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if sim.app_peer_count(node_id).await.unwrap_or(usize::MAX) == expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        sim.app_peer_count(node_id).await.unwrap_or(usize::MAX),
        expected
    );
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

async fn build_app_backed_topology(mut sim: Simulation, threshold_percent: u32) -> Simulation {
    sim.populate_app_nodes_from_existing(threshold_percent);
    sim.start_all_nodes().await;
    sim.stabilize_app_tcp_connectivity(1, Duration::from_secs(20))
        .await
        .expect(
            "build_app_backed_topology: TCP connectivity did not stabilize \
             within 20s (min_peers=1). Nodes may not have completed handshakes.",
        );
    sim
}

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
    sim.stabilize_app_tcp_connectivity(1, Duration::from_secs(20))
        .await
        .expect(
            "build_two_running_of_three: TCP connectivity did not stabilize \
             within 20s (min_peers=1).",
        );
    sim
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
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        if app.state().await == AppState::Validating {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(app.state().await, AppState::Validating);

    let closed = sim
        .manual_close_all_app_nodes()
        .await
        .expect("manual close");
    assert_eq!(closed, vec![2]);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        if sim.have_all_app_nodes_externalized(2, 0) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert!(sim.have_all_app_nodes_externalized(2, 0));
    sim.stop_all_nodes().await.expect("stop app-backed nodes");
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

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        let mut all_validating = true;
        for id in ["node0", "node1", "node2"] {
            let app = sim.app(id).expect("running core3 app node");
            if app.state().await != AppState::Validating {
                all_validating = false;
                break;
            }
        }
        if all_validating {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    for id in ["node0", "node1", "node2"] {
        let app = sim.app(id).expect("running core3 app node");
        assert_eq!(app.state().await, AppState::Validating);
    }

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

    // Cycle4 topology: each node should have 2 peers (ring neighbors).
    // Wait for full topology connectivity to ensure SCP envelopes can
    // propagate without multi-hop relay delays that cause nomination timeouts
    // on slow CI runners.
    sim.stabilize_app_tcp_connectivity(2, Duration::from_secs(30))
        .await
        .expect("cycle4: topology connectivity (2 peers/node) did not stabilize within 30s");

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

    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!sim.have_all_app_nodes_externalized(2, 1));

    sim.stop_all_nodes().await.expect("stop separate app nodes");
}

#[tokio::test]
async fn test_core3_restart_rejoin_over_tcp() {
    let mut sim = build_app_backed_topology(Topologies::core3(SimulationMode::OverTcp), 66).await;

    manual_close_until(&sim, 2, 1, Duration::from_secs(45)).await;

    sim.remove_node("node0").await.expect("remove node0 tcp");
    wait_for_peer_count(&sim, "node1", 1, Duration::from_secs(10)).await;
    wait_for_peer_count(&sim, "node2", 1, Duration::from_secs(10)).await;

    manual_close_until(
        &sim,
        3,
        0,
        Duration::from_secs(TCP_POST_REMOVAL_CLOSE_TIMEOUT_SECS),
    )
    .await;

    sim.restart_node("node0").await.expect("restart node0 tcp");
    wait_for_app_operational(&sim, "node0", Duration::from_secs(5)).await;

    // Re-establish peer connections with retry (TCP connections can fail transiently).
    sim.stabilize_app_tcp_connectivity(1, Duration::from_secs(10))
        .await
        .expect("node0 failed to establish peer connectivity after restart");

    // Request SCP state so node0 learns about the externalized slots it missed.
    sim.app("node0")
        .expect("restarted node0 app")
        .request_scp_state_from_peers()
        .await;

    // Wait for node0 to catch up to ledger 3 (where node1/node2 are).
    // 60s timeout: post-restart catchup can be slow on CI runners.
    wait_for_app_ledger_close(&sim, 3, Duration::from_secs(60)).await;

    // Now advance all nodes to ledger 4.
    manual_close_until(&sim, 4, 1, Duration::from_secs(60)).await;

    sim.stop_all_nodes()
        .await
        .expect("stop core3 tcp restart test");
}

#[tokio::test]
async fn test_core3_restart_rejoin_over_loopback() {
    let mut sim =
        build_app_backed_topology(Topologies::core3(SimulationMode::OverLoopback), 66).await;

    manual_close_until(&sim, 2, 1, Duration::from_secs(45)).await;

    sim.remove_node("node0")
        .await
        .expect("remove node0 loopback");
    wait_for_peer_count(&sim, "node1", 1, Duration::from_secs(10)).await;
    wait_for_peer_count(&sim, "node2", 1, Duration::from_secs(10)).await;

    manual_close_until(&sim, 3, 0, Duration::from_secs(45)).await;

    sim.restart_node("node0")
        .await
        .expect("restart node0 loopback");
    wait_for_app_operational(&sim, "node0", Duration::from_secs(5)).await;

    // Re-establish peer connections.
    let _ = sim.add_connection("node0", "node1").await;
    let _ = sim.add_connection("node0", "node2").await;

    // Wait for peer connections to be fully established before requesting
    // SCP state. add_connection() spawns the handshake asynchronously, so
    // without this wait request_scp_state_from_peers() can find zero peers
    // and silently return without requesting any state.
    wait_for_peer_count(&sim, "node0", 2, Duration::from_secs(10)).await;

    // Request SCP state so node0 learns about externalized slots it missed.
    sim.app("node0")
        .expect("restarted node0 app")
        .request_scp_state_from_peers()
        .await;

    // Wait for node0 to catch up to ledger 3 before triggering ledger 4.
    // 60s timeout: post-restart catchup can be slow on CI runners.
    wait_for_app_ledger_close(&sim, 3, Duration::from_secs(60)).await;

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
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("did not stabilize"));
    sim.stop_all_nodes().await.ok();
}
