//! Integration test for the JSON-RPC HTTP dispatch layer.
//!
//! Boots a minimal single-node `App` via the `henyey_simulation` harness,
//! binds `RpcServer` on an ephemeral port, and sends real HTTP POSTs via
//! `reqwest`. Asserts JSON-RPC 2.0 envelope invariants and shape invariants
//! on each method's `result` object. Covers guarantee #3 from #1755
//! (the RPC HTTP surface) without needing stellar-rpc or horizon.

use std::time::Duration;

use henyey_app::config::QuorumSetConfig;
use henyey_app::AppState;
use henyey_common::Hash256;
use henyey_crypto::SecretKey;
use henyey_rpc::RpcServer;
use henyey_simulation::{Simulation, SimulationMode};
use serde_json::{json, Value};

/// Build a single-node simulation running standalone, manually close one
/// ledger, and return the `Simulation` plus its one app node id. The
/// returned simulation owns the app; dropping it stops the node.
async fn boot_single_node_sim() -> (Simulation, String) {
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

    (sim, "node0".to_string())
}

/// Send a JSON-RPC request over HTTP, parse the response, and return
/// `(status, body_json)`.
async fn post_rpc(client: &reqwest::Client, url: &str, body: Value) -> (u16, Value) {
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
fn assert_envelope(resp: &Value, expected_id: &Value) {
    assert_eq!(resp["jsonrpc"], json!("2.0"), "jsonrpc must be \"2.0\"");
    assert_eq!(resp["id"], *expected_id, "id must be echoed");
    assert!(
        resp.get("result").is_some() ^ resp.get("error").is_some(),
        "exactly one of result|error must be present: {resp}"
    );
}

#[tokio::test]
async fn rpc_http_dispatch_covers_core_methods() {
    let (sim, node_id) = boot_single_node_sim().await;
    let app = sim.app(&node_id).expect("app");

    // Bind the RPC server on an ephemeral port.
    let (running, addr) = RpcServer::new(0, app.clone())
        .bind()
        .await
        .expect("rpc bind");
    let url = format!("http://{addr}/");

    let serve_handle = tokio::spawn(async move {
        let _ = running.serve().await;
    });

    // Give the serve loop a tick to be ready; `bind` has already completed
    // the TcpListener::bind, so this is just letting axum's accept start.
    tokio::time::sleep(Duration::from_millis(20)).await;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("reqwest client");

    // --- getHealth ---
    let id = json!(1);
    let (status, resp) = post_rpc(
        &client,
        &url,
        json!({"jsonrpc": "2.0", "id": id, "method": "getHealth"}),
    )
    .await;
    assert_eq!(status, 200);
    assert_envelope(&resp, &id);
    let result = &resp["result"];
    assert!(result["status"].is_string(), "getHealth.result.status");
    assert!(
        result["latestLedger"].is_number(),
        "getHealth.result.latestLedger"
    );

    // --- getLatestLedger ---
    let id = json!("latest-1");
    let (status, resp) = post_rpc(
        &client,
        &url,
        json!({"jsonrpc": "2.0", "id": id, "method": "getLatestLedger"}),
    )
    .await;
    assert_eq!(status, 200);
    assert_envelope(&resp, &id);
    let result = &resp["result"];
    assert!(
        result["sequence"].is_number(),
        "getLatestLedger.result.sequence must be a number"
    );
    let seq = result["sequence"].as_u64().unwrap();
    assert!(
        seq >= 2,
        "expected sequence >= 2 after manual close, got {seq}"
    );

    // --- getNetwork ---
    let id = json!(2);
    let (status, resp) = post_rpc(
        &client,
        &url,
        json!({"jsonrpc": "2.0", "id": id, "method": "getNetwork"}),
    )
    .await;
    assert_eq!(status, 200);
    assert_envelope(&resp, &id);
    assert!(
        resp["result"]["passphrase"].is_string(),
        "getNetwork.result.passphrase"
    );

    // --- unknown method → -32601 ---
    let id = json!(3);
    let (status, resp) = post_rpc(
        &client,
        &url,
        json!({"jsonrpc": "2.0", "id": id, "method": "doesNotExist"}),
    )
    .await;
    assert_eq!(status, 200);
    assert_envelope(&resp, &id);
    let err = &resp["error"];
    assert_eq!(
        err["code"],
        json!(-32601),
        "unknown method must return Method not found (-32601)"
    );

    // --- invalid jsonrpc version → -32600 (invalid request) ---
    let id = json!(4);
    let (status, resp) = post_rpc(
        &client,
        &url,
        json!({"jsonrpc": "1.0", "id": id, "method": "getHealth"}),
    )
    .await;
    assert_eq!(status, 200);
    assert_envelope(&resp, &id);
    assert_eq!(
        resp["error"]["code"],
        json!(-32600),
        "wrong jsonrpc version must return invalid request (-32600)"
    );

    serve_handle.abort();
    drop(sim);
}
