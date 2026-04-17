//! Integration test for the JSON-RPC HTTP dispatch layer.
//!
//! Boots a minimal single-node `App` via the `henyey_simulation` harness,
//! binds `RpcServer` on an ephemeral port, and sends real HTTP POSTs via
//! `reqwest`. Asserts JSON-RPC 2.0 envelope invariants and shape invariants
//! on each method's `result` object. Covers guarantee #3 from #1755
//! (the RPC HTTP surface) without needing stellar-rpc or horizon.

mod common;

use std::time::Duration;

use henyey_rpc::RpcServer;
use serde_json::json;

use common::{assert_envelope, boot_single_node_sim, post_rpc};

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

    // `bind` already completed `TcpListener::bind`, so the kernel accept
    // queue accepts connects before `serve()` starts polling. No sleep
    // needed.
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
