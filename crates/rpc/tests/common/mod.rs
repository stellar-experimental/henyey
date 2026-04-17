//! Shared test helpers for RPC integration tests.

use std::time::Duration;

use henyey_app::config::QuorumSetConfig;
use henyey_app::AppState;
use henyey_common::Hash256;
use henyey_crypto::SecretKey;
use henyey_simulation::{Simulation, SimulationMode};
use serde_json::{json, Value};

/// Build a single-node simulation running standalone, manually close one
/// ledger, and return the `Simulation` plus its one app node id. The
/// returned simulation owns the app; dropping it stops the node.
pub async fn boot_single_node_sim() -> (Simulation, String) {
    let mut sim =
        Simulation::with_network(SimulationMode::OverTcp, "Test SDF Network ; September 2015");

    let seed = Hash256::hash(b"RPC_HTTP_DISPATCH_NODE_0");
    let secret = SecretKey::from_seed(&seed.0);
    let quorum_set = QuorumSetConfig {
        threshold_percent: 100,
        validators: vec![secret.public_key().to_strkey()],
        inner_sets: Vec::new(),
    };

    sim.add_app_node("node0", secret, quorum_set);
    sim.start_all_nodes().await;

    // Wait for the node to reach Validating.
    let app = sim.app("node0").expect("app node");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        if app.state().await == AppState::Validating {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(app.state().await, AppState::Validating);

    // Close ledger 2 so the DB has at least one persisted close.
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
    assert!(
        sim.have_all_app_nodes_externalized(2, 0),
        "node failed to externalize ledger 2 within 10s"
    );

    (sim, "node0".to_string())
}

/// Send a JSON-RPC request over HTTP, parse the response, and return
/// `(status, body_json)`.
pub async fn post_rpc(client: &reqwest::Client, url: &str, body: Value) -> (u16, Value) {
    let resp = client
        .post(url)
        .json(&body)
        .send()
        .await
        .expect("rpc request send");
    let status = resp.status().as_u16();
    let json: Value = resp.json().await.expect("rpc response json");
    (status, json)
}

/// Invariants every JSON-RPC 2.0 response must satisfy regardless of method.
pub fn assert_envelope(resp: &Value, expected_id: &Value) {
    assert_eq!(resp["jsonrpc"], json!("2.0"), "jsonrpc must be \"2.0\"");
    assert_eq!(resp["id"], *expected_id, "id must be echoed");
    assert!(
        resp.get("result").is_some() ^ resp.get("error").is_some(),
        "exactly one of result|error must be present: {resp}"
    );
}
