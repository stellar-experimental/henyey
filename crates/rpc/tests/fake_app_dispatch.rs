//! Integration tests for RPC dispatch using a lightweight [`FakeRpcApp`].
//!
//! These tests exercise the JSON-RPC dispatch layer, envelope validation,
//! error codes, and response shapes without booting a full simulation node.
//! They supplement (not replace) the simulation-backed tests in
//! `http_dispatch.rs` and `corrupt_data.rs`.

mod common;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use henyey_herder::TxQueueResult;
use serde_json::json;
use stellar_xdr::curr::{
    Limits, Memo, MuxedAccount, Preconditions, SequenceNumber, Transaction, TransactionEnvelope,
    TransactionExt, TransactionV1Envelope, Uint256, WriteXdr,
};

use common::fake_app::{FakeRpcApp, FakeRpcTestHarness};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a minimal valid base64-encoded TransactionEnvelope for sendTransaction.
fn valid_tx_b64() -> String {
    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256([0u8; 32])),
        fee: 100,
        seq_num: SequenceNumber(1),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![].try_into().unwrap(),
        ext: TransactionExt::V0,
    };
    let env = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: vec![].try_into().unwrap(),
    });
    BASE64.encode(env.to_xdr(Limits::none()).unwrap())
}

fn assert_envelope(resp: &serde_json::Value, expected_id: &serde_json::Value) {
    assert_eq!(resp["jsonrpc"], json!("2.0"), "jsonrpc must be \"2.0\"");
    assert_eq!(resp["id"], *expected_id, "id must be echoed");
    assert!(
        resp.get("result").is_some() ^ resp.get("error").is_some(),
        "exactly one of result|error must be present: {resp}"
    );
}

// ---------------------------------------------------------------------------
// Core method tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fake_get_health() {
    let h = FakeRpcTestHarness::start_default().await;
    let id = json!(1);
    let (status, resp) = h
        .post_rpc(json!({"jsonrpc": "2.0", "id": id, "method": "getHealth"}))
        .await;

    assert_eq!(status, 200);
    assert_envelope(&resp, &id);
    let result = &resp["result"];
    assert!(result["status"].is_string(), "getHealth.result.status");
    assert!(
        result["latestLedger"].is_number(),
        "getHealth.result.latestLedger"
    );
    assert!(
        result["oldestLedger"].is_number(),
        "getHealth.result.oldestLedger"
    );
    assert!(
        result["ledgerRetentionWindow"].is_number(),
        "getHealth.result.ledgerRetentionWindow"
    );
}

#[tokio::test]
async fn fake_get_health_with_close_time() {
    // Set close_time far in the past so health becomes "unhealthy"
    let app = FakeRpcApp::builder()
        .ledger_seq(50)
        .close_time(1_000_000) // Very old
        .protocol_version(25)
        .build();
    let h = FakeRpcTestHarness::start(app).await;
    let id = json!("health-close-time");
    let (status, resp) = h
        .post_rpc(json!({"jsonrpc": "2.0", "id": id, "method": "getHealth"}))
        .await;

    assert_eq!(status, 200);
    assert_envelope(&resp, &id);
    assert_eq!(
        resp["result"]["status"].as_str().unwrap(),
        "unhealthy",
        "ledger with ancient close_time should be unhealthy"
    );
}

#[tokio::test]
async fn fake_get_network() {
    let h = FakeRpcTestHarness::start_default().await;
    let id = json!("net");
    let (status, resp) = h
        .post_rpc(json!({"jsonrpc": "2.0", "id": id, "method": "getNetwork"}))
        .await;

    assert_eq!(status, 200);
    assert_envelope(&resp, &id);
    assert_eq!(
        resp["result"]["passphrase"].as_str().unwrap(),
        "Test SDF Network ; September 2015"
    );
}

#[tokio::test]
async fn fake_get_version_info() {
    let h = FakeRpcTestHarness::start_default().await;
    let id = json!("ver");
    let (status, resp) = h
        .post_rpc(json!({"jsonrpc": "2.0", "id": id, "method": "getVersionInfo"}))
        .await;

    assert_eq!(status, 200);
    assert_envelope(&resp, &id);
    let result = &resp["result"];
    assert_eq!(result["version"].as_str().unwrap(), "0.0.0-test");
    assert_eq!(result["commitHash"].as_str().unwrap(), "deadbeef");
    assert_eq!(
        result["buildTimestamp"].as_str().unwrap(),
        "2024-01-01T00:00:00Z"
    );
}

#[tokio::test]
async fn fake_get_version_info_empty_metadata() {
    let app = FakeRpcApp::builder()
        .commit_hash(None)
        .build_timestamp(None)
        .build();
    let h = FakeRpcTestHarness::start(app).await;
    let id = json!("ver-empty");
    let (status, resp) = h
        .post_rpc(json!({"jsonrpc": "2.0", "id": id, "method": "getVersionInfo"}))
        .await;

    assert_eq!(status, 200);
    assert_envelope(&resp, &id);
    let result = &resp["result"];
    assert!(
        result["commitHash"].is_null(),
        "commitHash should be null when metadata unavailable"
    );
    assert!(
        result["buildTimestamp"].is_null(),
        "buildTimestamp should be null when metadata unavailable"
    );
    // Other fields remain present
    assert!(result["version"].is_string());
    assert!(result["protocolVersion"].is_number());
    assert!(result["captiveCoreVersion"].is_string());
}

#[tokio::test]
async fn fake_get_fee_stats() {
    let h = FakeRpcTestHarness::start_default().await;
    let id = json!("fees");
    let (status, resp) = h
        .post_rpc(json!({"jsonrpc": "2.0", "id": id, "method": "getFeeStats"}))
        .await;

    assert_eq!(status, 200);
    assert_envelope(&resp, &id);
    let result = &resp["result"];
    assert!(
        result["sorobanInclusionFee"].is_object(),
        "sorobanInclusionFee"
    );
    assert!(result["inclusionFee"].is_object(), "inclusionFee");
    assert!(result["latestLedger"].is_number(), "latestLedger");
}

// ---------------------------------------------------------------------------
// Error handling tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fake_unknown_method() {
    let h = FakeRpcTestHarness::start_default().await;
    let id = json!("unk");
    let (status, resp) = h
        .post_rpc(json!({"jsonrpc": "2.0", "id": id, "method": "doesNotExist"}))
        .await;

    assert_eq!(status, 200);
    assert_envelope(&resp, &id);
    assert_eq!(
        resp["error"]["code"],
        json!(-32601),
        "unknown method must return Method not found (-32601)"
    );
}

#[tokio::test]
async fn fake_invalid_jsonrpc_version() {
    let h = FakeRpcTestHarness::start_default().await;
    let id = json!("bad-ver");
    let (status, resp) = h
        .post_rpc(json!({"jsonrpc": "1.0", "id": id, "method": "getHealth"}))
        .await;

    assert_eq!(status, 200);
    assert_envelope(&resp, &id);
    assert_eq!(
        resp["error"]["code"],
        json!(-32600),
        "wrong jsonrpc version must return invalid request (-32600)"
    );
}

// ---------------------------------------------------------------------------
// sendTransaction — all TxQueueResult branches
// ---------------------------------------------------------------------------

async fn send_tx_with_result(result: TxQueueResult) -> serde_json::Value {
    let app = FakeRpcApp::builder().submit_result(result).build();
    let h = FakeRpcTestHarness::start(app).await;
    let tx_b64 = valid_tx_b64();
    let id = json!("send");
    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "sendTransaction",
            "params": {"transaction": tx_b64}
        }))
        .await;
    assert_eq!(status, 200);
    assert_envelope(&resp, &id);
    resp["result"].clone()
}

#[tokio::test]
async fn fake_send_transaction_added() {
    let result = send_tx_with_result(TxQueueResult::Added).await;
    assert_eq!(result["status"].as_str().unwrap(), "PENDING");
    assert!(result["hash"].is_string());
    assert!(result["latestLedger"].is_number());
}

#[tokio::test]
async fn fake_send_transaction_duplicate() {
    let result = send_tx_with_result(TxQueueResult::Duplicate).await;
    assert_eq!(result["status"].as_str().unwrap(), "DUPLICATE");
}

#[tokio::test]
async fn fake_send_transaction_queue_full() {
    let result = send_tx_with_result(TxQueueResult::QueueFull).await;
    assert_eq!(result["status"].as_str().unwrap(), "TRY_AGAIN_LATER");
}

#[tokio::test]
async fn fake_send_transaction_try_again_later() {
    let result = send_tx_with_result(TxQueueResult::TryAgainLater).await;
    assert_eq!(result["status"].as_str().unwrap(), "TRY_AGAIN_LATER");
}

#[tokio::test]
async fn fake_send_transaction_invalid_with_code() {
    use stellar_xdr::curr::TransactionResultCode;
    let result = send_tx_with_result(TxQueueResult::Invalid(Some(
        TransactionResultCode::TxFailed,
    )))
    .await;
    assert_eq!(result["status"].as_str().unwrap(), "ERROR");
    assert!(
        result.get("errorResultXdr").is_some() || result.get("errorResult").is_some(),
        "ERROR response must include errorResult"
    );
}

#[tokio::test]
async fn fake_send_transaction_invalid_none() {
    let result = send_tx_with_result(TxQueueResult::Invalid(None)).await;
    assert_eq!(result["status"].as_str().unwrap(), "ERROR");
}

#[tokio::test]
async fn fake_send_transaction_banned() {
    let result = send_tx_with_result(TxQueueResult::Banned).await;
    assert_eq!(result["status"].as_str().unwrap(), "ERROR");
}

#[tokio::test]
async fn fake_send_transaction_fee_too_low() {
    let result = send_tx_with_result(TxQueueResult::FeeTooLow).await;
    assert_eq!(result["status"].as_str().unwrap(), "ERROR");
}

#[tokio::test]
async fn fake_send_transaction_filtered() {
    let result = send_tx_with_result(TxQueueResult::Filtered).await;
    assert_eq!(result["status"].as_str().unwrap(), "ERROR");
}

// ---------------------------------------------------------------------------
// Ledger consistency regression tests
// ---------------------------------------------------------------------------

use henyey_rpc::RpcAppHandle;
use stellar_xdr::curr::{
    Hash, LedgerHeader, LedgerHeaderExt, ReadXdr, StellarValue, StellarValueExt, TimePoint,
};

/// Build a minimal [`LedgerHeader`] at the given sequence and close time.
fn test_header(seq: u32, close_time: u64) -> LedgerHeader {
    LedgerHeader {
        ledger_version: 25,
        previous_ledger_hash: Hash([0; 32]),
        scp_value: StellarValue {
            tx_set_hash: Hash([0; 32]),
            close_time: TimePoint(close_time),
            upgrades: vec![].try_into().unwrap(),
            ext: StellarValueExt::Basic,
        },
        tx_set_result_hash: Hash([0; 32]),
        bucket_list_hash: Hash([0; 32]),
        ledger_seq: seq,
        total_coins: 0,
        fee_pool: 0,
        inflation_seq: 0,
        id_pool: 0,
        base_fee: 100,
        base_reserve: 5_000_000,
        max_tx_set_size: 100,
        skip_list: [Hash([0; 32]), Hash([0; 32]), Hash([0; 32]), Hash([0; 32])],
        ext: LedgerHeaderExt::V0,
    }
}

/// Helper: assert `ledger_summary()` and `ledger_snapshot()` agree on all
/// RPC-exposed header-derived fields.
fn assert_summary_snapshot_consistent(app: &FakeRpcApp) {
    let summary = app.ledger_summary();
    let snap = app.ledger_snapshot();

    assert_eq!(
        summary.num, snap.header.ledger_seq,
        "summary.num must equal snapshot.header.ledger_seq"
    );
    assert_eq!(
        summary.close_time, snap.header.scp_value.close_time.0,
        "summary.close_time must equal snapshot close_time"
    );
    assert_eq!(
        summary.version, snap.header.ledger_version,
        "summary.version must equal snapshot.header.ledger_version"
    );
    assert_eq!(
        summary.base_fee, snap.header.base_fee,
        "summary.base_fee must equal snapshot.header.base_fee"
    );
    assert_eq!(
        summary.base_reserve, snap.header.base_reserve,
        "summary.base_reserve must equal snapshot.header.base_reserve"
    );
    assert_eq!(
        summary.hash, snap.hash,
        "summary.hash must equal snapshot.hash"
    );
}

/// (a) Scalar-only builder: summary and snapshot agree on all fields.
#[test]
fn consistency_scalar_only_builder() {
    let app = FakeRpcApp::builder()
        .ledger_seq(50)
        .close_time(1_700_000_000)
        .protocol_version(25)
        .base_fee(200)
        .build();

    assert_summary_snapshot_consistent(&app);

    let snap = app.ledger_snapshot();
    assert_eq!(snap.header.ledger_seq, 50);
    assert_eq!(snap.header.scp_value.close_time.0, 1_700_000_000);
    assert_eq!(snap.header.ledger_version, 25);
    assert_eq!(snap.header.base_fee, 200);
}

/// (b) header_snapshot-only builder: summary and snapshot agree.
#[test]
fn consistency_header_snapshot_only() {
    let header = test_header(42, 1_600_000_000);
    let app = FakeRpcApp::builder().header_snapshot(header).build();

    assert_summary_snapshot_consistent(&app);

    let snap = app.ledger_snapshot();
    assert_eq!(snap.header.ledger_seq, 42);
    assert_eq!(snap.header.scp_value.close_time.0, 1_600_000_000);
}

/// (c) header_snapshot + all four scalar overrides: overrides win, hash
/// reflects the fully mutated header.
#[test]
fn consistency_header_snapshot_with_scalar_overrides() {
    let base_header = test_header(10, 1_500_000_000);
    let app = FakeRpcApp::builder()
        .header_snapshot(base_header)
        .ledger_seq(99)
        .close_time(1_800_000_000)
        .protocol_version(26)
        .base_fee(500)
        .build();

    assert_summary_snapshot_consistent(&app);

    let snap = app.ledger_snapshot();
    assert_eq!(snap.header.ledger_seq, 99);
    assert_eq!(snap.header.scp_value.close_time.0, 1_800_000_000);
    assert_eq!(snap.header.ledger_version, 26);
    assert_eq!(snap.header.base_fee, 500);

    // Verify hash matches what we'd compute from the mutated header.
    let expected_hash = henyey_ledger::compute_header_hash(&snap.header).expect("header hash");
    assert_eq!(
        snap.hash, expected_hash,
        "hash must reflect the mutated header"
    );
}

/// (d) Default builder: hash is non-zero and summary/snapshot agree.
#[test]
fn consistency_default_builder() {
    let app = FakeRpcApp::default();
    assert_summary_snapshot_consistent(&app);

    let snap = app.ledger_snapshot();
    assert_ne!(
        snap.hash,
        henyey_common::Hash256::default(),
        "default builder should produce a non-zero hash"
    );
}

/// getLatestLedger via the simple scalar builder path: verify in-memory fields
/// match `headerXdr` and the builder inputs.
#[tokio::test]
async fn fake_get_latest_ledger_scalar_builder() {
    let app = FakeRpcApp::builder()
        .ledger_seq(77)
        .close_time(1_700_000_000)
        .protocol_version(25)
        .base_fee(150)
        .build();
    let h = FakeRpcTestHarness::start(app).await;
    let id = json!("latest-scalar");
    let (status, resp) = h
        .post_rpc(json!({"jsonrpc": "2.0", "id": id, "method": "getLatestLedger"}))
        .await;

    assert_eq!(status, 200);
    assert_envelope(&resp, &id);
    let result = &resp["result"];

    assert_eq!(result["sequence"].as_u64().unwrap(), 77);
    assert_eq!(result["protocolVersion"].as_u64().unwrap(), 25);
    let close_time: u64 = result["closeTime"]
        .as_str()
        .expect("closeTime string")
        .parse()
        .expect("closeTime parses");
    assert_eq!(close_time, 1_700_000_000);

    // Decode headerXdr and verify it matches the in-memory fields.
    let header_b64 = result["headerXdr"].as_str().expect("headerXdr");
    let header_bytes = BASE64.decode(header_b64).expect("valid base64");
    let header = LedgerHeader::from_xdr(&header_bytes, Limits::none()).expect("valid header XDR");

    assert_eq!(header.ledger_seq, 77);
    assert_eq!(header.ledger_version, 25);
    assert_eq!(header.scp_value.close_time.0, 1_700_000_000);
    assert_eq!(header.base_fee, 150);

    // Verify id == hex(SHA-256(headerXdr bytes))
    use sha2::{Digest, Sha256};
    let computed_hash = Sha256::digest(&header_bytes);
    let computed_hex = hex::encode(computed_hash);
    assert_eq!(
        result["id"].as_str().unwrap(),
        computed_hex,
        "response id must equal SHA-256 of headerXdr bytes"
    );
}

// ---------------------------------------------------------------------------
// Snapshot readiness gate tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fake_get_ledger_entries_not_ready() {
    let app = FakeRpcApp::builder().snapshot_ready(false).build();
    let h = FakeRpcTestHarness::start(app).await;
    let id = json!("gle-not-ready");
    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "getLedgerEntries",
            "params": {"keys": ["AAAAAAAAAAA="]}
        }))
        .await;

    assert_eq!(status, 200);
    assert_envelope(&resp, &id);
    assert_eq!(resp["error"]["code"], json!(-32001));
    assert_eq!(
        resp["error"]["message"].as_str().unwrap(),
        "server is not ready, try again later"
    );
}

#[tokio::test]
async fn fake_simulate_transaction_not_ready() {
    let app = FakeRpcApp::builder().snapshot_ready(false).build();
    let h = FakeRpcTestHarness::start(app).await;
    let id = json!("sim-not-ready");
    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "simulateTransaction",
            "params": {}
        }))
        .await;

    assert_eq!(status, 200);
    assert_envelope(&resp, &id);
    assert_eq!(resp["error"]["code"], json!(-32001));
    assert_eq!(
        resp["error"]["message"].as_str().unwrap(),
        "server is not ready, try again later"
    );
}

#[tokio::test]
async fn fake_simulate_not_ready_beats_invalid_params() {
    let app = FakeRpcApp::builder().snapshot_ready(false).build();
    let h = FakeRpcTestHarness::start(app).await;
    let id = json!("sim-nr-vs-ip");
    // Send with missing 'transaction' param — would normally be -32602
    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "simulateTransaction",
            "params": {"wrong_field": 42}
        }))
        .await;

    assert_eq!(status, 200);
    assert_envelope(&resp, &id);
    assert_eq!(
        resp["error"]["code"],
        json!(-32001),
        "not-ready must take priority over invalid params"
    );
}

#[tokio::test]
async fn fake_get_ledger_entries_not_ready_beats_invalid_params() {
    let app = FakeRpcApp::builder().snapshot_ready(false).build();
    let h = FakeRpcTestHarness::start(app).await;
    let id = json!("gle-nr-vs-ip");
    // Send with missing 'keys' param — would normally be -32602
    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "getLedgerEntries",
            "params": {"wrong_field": 42}
        }))
        .await;

    assert_eq!(status, 200);
    assert_envelope(&resp, &id);
    assert_eq!(
        resp["error"]["code"],
        json!(-32001),
        "not-ready must take priority over invalid params"
    );
}

#[tokio::test]
async fn fake_non_snapshot_methods_work_while_not_ready() {
    let app = FakeRpcApp::builder().snapshot_ready(false).build();
    let h = FakeRpcTestHarness::start(app).await;

    // getHealth should succeed
    let (status, resp) = h
        .post_rpc(json!({"jsonrpc": "2.0", "id": 1, "method": "getHealth"}))
        .await;
    assert_eq!(status, 200);
    assert!(
        resp.get("result").is_some(),
        "getHealth must succeed while not ready"
    );

    // getNetwork should succeed
    let (status, resp) = h
        .post_rpc(json!({"jsonrpc": "2.0", "id": 2, "method": "getNetwork"}))
        .await;
    assert_eq!(status, 200);
    assert!(
        resp.get("result").is_some(),
        "getNetwork must succeed while not ready"
    );

    // getVersionInfo should succeed
    let (status, resp) = h
        .post_rpc(json!({"jsonrpc": "2.0", "id": 3, "method": "getVersionInfo"}))
        .await;
    assert_eq!(status, 200);
    assert!(
        resp.get("result").is_some(),
        "getVersionInfo must succeed while not ready"
    );
}

/// Defensive fallback: snapshot_ready=true but empty snapshot manager returns
/// None. The handler must return internal error (-32603), not server-not-ready.
#[tokio::test]
async fn fake_get_ledger_entries_defensive_fallback() {
    use stellar_xdr::curr::{LedgerKey, LedgerKeyAccount, WriteXdr};

    let h = FakeRpcTestHarness::start_default().await;
    let id = json!("gle-defensive");

    // Build a valid LedgerKey (Account key with zero pubkey)
    let key = LedgerKey::Account(LedgerKeyAccount {
        account_id: stellar_xdr::curr::AccountId(
            stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(stellar_xdr::curr::Uint256(
                [0u8; 32],
            )),
        ),
    });
    let key_b64 = BASE64.encode(key.to_xdr(Limits::none()).unwrap());

    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "getLedgerEntries",
            "params": {"keys": [key_b64]}
        }))
        .await;

    assert_eq!(status, 200);
    assert_envelope(&resp, &id);
    assert_eq!(
        resp["error"]["code"],
        json!(-32603),
        "ready=true but no snapshot must return internal error, not server-not-ready"
    );
}

// ---------------------------------------------------------------------------
// Strict parameter type validation tests
// ---------------------------------------------------------------------------
//
// These tests exercise the full dispatch path to verify that wrong-type
// parameters are rejected with -32602 (invalid_params) at the endpoint level.

/// Shared assertion: the request must produce a -32602 error whose message
/// contains `expected_msg`.
async fn assert_invalid_params(
    h: &FakeRpcTestHarness,
    method: &str,
    params: serde_json::Value,
    expected_msg: &str,
) {
    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": "type-val",
            "method": method,
            "params": params
        }))
        .await;
    assert_eq!(status, 200, "HTTP status for {method}");
    assert!(
        resp.get("error").is_some(),
        "expected error for {method} with params {params}, got: {resp}"
    );
    assert_eq!(
        resp["error"]["code"],
        json!(-32602),
        "expected -32602 for {method}, got: {}",
        resp["error"]
    );
    let msg = resp["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains(expected_msg),
        "for {method}: expected message containing '{expected_msg}', got '{msg}'"
    );
}

/// Build a minimal valid Soroban InvokeHostFunction envelope (Wasm upload)
/// that passes `extract_soroban_op()`, memo validation, and structural checks.
fn valid_soroban_tx_b64() -> String {
    use stellar_xdr::curr::{
        HostFunction, InvokeHostFunctionOp, LedgerFootprint, Operation, OperationBody,
        SorobanResources, SorobanTransactionData, SorobanTransactionDataExt,
    };

    let invoke_op = Operation {
        source_account: None,
        body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
            host_function: HostFunction::UploadContractWasm(vec![0u8; 10].try_into().unwrap()),
            auth: vec![].try_into().unwrap(),
        }),
    };

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256([0u8; 32])),
        fee: 100,
        seq_num: SequenceNumber(1),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![invoke_op].try_into().unwrap(),
        ext: TransactionExt::V1(SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: vec![].try_into().unwrap(),
                    read_write: vec![].try_into().unwrap(),
                },
                instructions: 1_000_000,
                disk_read_bytes: 1000,
                write_bytes: 1000,
            },
            resource_fee: 10000,
        }),
    };
    let env = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: vec![].try_into().unwrap(),
    });
    BASE64.encode(env.to_xdr(Limits::none()).unwrap())
}

// --- Group 1: Root params must be an object ---

#[tokio::test]
async fn fake_type_validation_root_params_object() {
    let h = FakeRpcTestHarness::start_default().await;

    let methods = [
        "getLedgerEntries",
        "getTransaction",
        "getTransactions",
        "getLedgers",
        "getEvents",
        "sendTransaction",
        "simulateTransaction",
    ];

    let bad_params = [json!([1, 2, 3]), json!("a string"), json!(42), json!(true)];

    for method in &methods {
        for params in &bad_params {
            assert_invalid_params(&h, method, params.clone(), "params must be an object").await;
        }
    }

    // Positive: params: null passes the object gate (produces a different error)
    for method in &methods {
        let (status, resp) = h
            .post_rpc(json!({
                "jsonrpc": "2.0",
                "id": "null-params",
                "method": method,
                "params": null
            }))
            .await;
        assert_eq!(status, 200);
        // If there's an error, it must NOT be "params must be an object"
        if let Some(err) = resp.get("error") {
            let msg = err["message"].as_str().unwrap_or("");
            assert!(
                !msg.contains("params must be an object"),
                "{method} with null params should pass object gate, got: {msg}"
            );
        }
    }
}

// --- Group 2: Null field semantics ---

#[tokio::test]
async fn fake_type_validation_null_semantics() {
    let h = FakeRpcTestHarness::start_default().await;

    // require_str with null → "missing" error
    assert_invalid_params(
        &h,
        "sendTransaction",
        json!({"transaction": null}),
        "missing 'transaction'",
    )
    .await;

    assert_invalid_params(
        &h,
        "getTransaction",
        json!({"hash": null}),
        "missing 'hash'",
    )
    .await;

    // Optional field with null passes type gate (pagination: null is fine)
    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": "null-opt",
            "method": "getTransactions",
            "params": {"pagination": null}
        }))
        .await;
    assert_eq!(status, 200);
    // Should NOT fail with "must be an object" for pagination
    if let Some(err) = resp.get("error") {
        let msg = err["message"].as_str().unwrap_or("");
        assert!(
            !msg.contains("'pagination' must be an object"),
            "null pagination should pass type gate, got: {msg}"
        );
    }
}

// --- Group 3: getTransactions ---

#[tokio::test]
async fn fake_type_validation_get_transactions() {
    let h = FakeRpcTestHarness::start_default().await;

    let cases: &[(&str, serde_json::Value, &str)] = &[
        ("status as int", json!({"status": 123}), "must be a string"),
        (
            "startLedger as string",
            json!({"startLedger": "abc"}),
            "must be a non-negative integer",
        ),
        (
            "startLedger overflow",
            json!({"startLedger": 5_000_000_000_u64}),
            "exceeds maximum value",
        ),
        (
            "pagination as array",
            json!({"pagination": [1, 2, 3]}),
            "must be an object",
        ),
        (
            "pagination.cursor as int",
            json!({"pagination": {"cursor": 999}}),
            "must be a string",
        ),
        (
            "pagination.limit as string",
            json!({"pagination": {"limit": "ten"}}),
            "must be a non-negative integer",
        ),
        (
            "pagination.limit overflow",
            json!({"pagination": {"limit": 5_000_000_000_u64}}),
            "exceeds maximum value",
        ),
        (
            "xdrFormat as int",
            json!({"xdrFormat": 42}),
            "must be a string",
        ),
    ];

    for (label, params, expected) in cases {
        assert_invalid_params(&h, "getTransactions", params.clone(), expected).await;
        let _ = label; // suppress unused warning in non-debug
    }
}

// --- Group 4: getLedgers ---

#[tokio::test]
async fn fake_type_validation_get_ledgers() {
    let h = FakeRpcTestHarness::start_default().await;

    let cases: &[(&str, serde_json::Value, &str)] = &[
        (
            "startLedger as string",
            json!({"startLedger": "abc"}),
            "must be a non-negative integer",
        ),
        (
            "startLedger overflow",
            json!({"startLedger": 5_000_000_000_u64}),
            "exceeds maximum value",
        ),
        (
            "pagination as bool",
            json!({"pagination": true}),
            "must be an object",
        ),
        (
            "pagination.cursor as int",
            json!({"pagination": {"cursor": 123}}),
            "must be a string",
        ),
        (
            "pagination.limit as string",
            json!({"pagination": {"limit": "ten"}}),
            "must be a non-negative integer",
        ),
        (
            "pagination.limit overflow",
            json!({"pagination": {"limit": 5_000_000_000_u64}}),
            "exceeds maximum value",
        ),
        (
            "xdrFormat as array",
            json!({"xdrFormat": [1]}),
            "must be a string",
        ),
    ];

    for (label, params, expected) in cases {
        assert_invalid_params(&h, "getLedgers", params.clone(), expected).await;
        let _ = label;
    }
}

// --- Group 5: getEvents ---

#[tokio::test]
async fn fake_type_validation_get_events() {
    let h = FakeRpcTestHarness::start_default().await;

    let cases: &[(&str, serde_json::Value, &str)] = &[
        (
            "startLedger as string",
            json!({"startLedger": "abc"}),
            "must be a non-negative integer",
        ),
        (
            "startLedger overflow",
            json!({"startLedger": 5_000_000_000_u64}),
            "exceeds maximum value",
        ),
        // Cases needing valid startLedger scaffolding
        (
            "endLedger as string",
            json!({"startLedger": 1, "endLedger": "foo"}),
            "must be a non-negative integer",
        ),
        (
            "endLedger overflow",
            json!({"startLedger": 1, "endLedger": 5_000_000_000_u64}),
            "exceeds maximum value",
        ),
        (
            "pagination as int",
            json!({"startLedger": 1, "pagination": 42}),
            "must be an object",
        ),
        (
            "pagination.cursor as int",
            json!({"startLedger": 1, "pagination": {"cursor": 123}}),
            "must be a string",
        ),
        (
            "pagination.limit as string",
            json!({"startLedger": 1, "pagination": {"limit": "ten"}}),
            "must be a non-negative integer",
        ),
        (
            "pagination.limit overflow",
            json!({"startLedger": 1, "pagination": {"limit": 5_000_000_000_u64}}),
            "exceeds maximum value",
        ),
        (
            "xdrFormat as object",
            json!({"startLedger": 1, "xdrFormat": {}}),
            "must be a string",
        ),
    ];

    for (label, params, expected) in cases {
        assert_invalid_params(&h, "getEvents", params.clone(), expected).await;
        let _ = label;
    }
}

// --- Group 6: getTransaction ---

#[tokio::test]
async fn fake_type_validation_get_transaction() {
    let h = FakeRpcTestHarness::start_default().await;

    assert_invalid_params(
        &h,
        "getTransaction",
        json!({"hash": 12345}),
        "must be a string",
    )
    .await;
    assert_invalid_params(
        &h,
        "getTransaction",
        json!({"hash": "abc123", "xdrFormat": 0}),
        "must be a string",
    )
    .await;
}

// --- Group 7: sendTransaction ---

#[tokio::test]
async fn fake_type_validation_send_transaction() {
    let h = FakeRpcTestHarness::start_default().await;

    assert_invalid_params(
        &h,
        "sendTransaction",
        json!({"transaction": 42}),
        "must be a string",
    )
    .await;
    assert_invalid_params(
        &h,
        "sendTransaction",
        json!({"transaction": "validbase64", "xdrFormat": 99}),
        "must be a string",
    )
    .await;
}

// --- Group 8: simulateTransaction ---

#[tokio::test]
async fn fake_type_validation_simulate_transaction() {
    let h = FakeRpcTestHarness::start_default().await;

    // Wrong type for transaction field
    assert_invalid_params(
        &h,
        "simulateTransaction",
        json!({"transaction": false}),
        "must be a string",
    )
    .await;

    // Cases that need a valid Soroban envelope to reach later param parsing
    let valid_tx = valid_soroban_tx_b64();

    assert_invalid_params(
        &h,
        "simulateTransaction",
        json!({"transaction": valid_tx, "authMode": 999}),
        "must be a string",
    )
    .await;

    assert_invalid_params(
        &h,
        "simulateTransaction",
        json!({"transaction": valid_tx, "resourceConfig": "nope"}),
        "must be an object",
    )
    .await;

    assert_invalid_params(
        &h,
        "simulateTransaction",
        json!({"transaction": valid_tx, "resourceConfig": {"instructionLeeway": "big"}}),
        "must be a non-negative integer",
    )
    .await;

    assert_invalid_params(
        &h,
        "simulateTransaction",
        json!({"transaction": valid_tx, "xdrFormat": 42}),
        "must be a string",
    )
    .await;
}

// --- Group 9: getLedgerEntries ---

#[tokio::test]
async fn fake_type_validation_get_ledger_entries() {
    let h = FakeRpcTestHarness::start_default().await;

    // xdrFormat wrong type (parsed before keys)
    assert_invalid_params(
        &h,
        "getLedgerEntries",
        json!({"xdrFormat": 99}),
        "must be a string",
    )
    .await;

    // keys missing entirely
    assert_invalid_params(
        &h,
        "getLedgerEntries",
        json!({}),
        "missing or invalid 'keys'",
    )
    .await;

    // keys as non-array (string)
    assert_invalid_params(
        &h,
        "getLedgerEntries",
        json!({"keys": "not-an-array"}),
        "missing or invalid 'keys'",
    )
    .await;

    // keys as non-array (integer)
    assert_invalid_params(
        &h,
        "getLedgerEntries",
        json!({"keys": 42}),
        "missing or invalid 'keys'",
    )
    .await;

    // keys[0] as non-string (integer)
    assert_invalid_params(
        &h,
        "getLedgerEntries",
        json!({"keys": [123]}),
        "keys[0] must be a string",
    )
    .await;

    // keys[1] as non-string (object) — use valid base64 XDR key for keys[0]
    use stellar_xdr::curr::{LedgerKey, LedgerKeyAccount, WriteXdr};
    let key = LedgerKey::Account(LedgerKeyAccount {
        account_id: stellar_xdr::curr::AccountId(
            stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32])),
        ),
    });
    let key_b64 = BASE64.encode(key.to_xdr(Limits::none()).unwrap());
    assert_invalid_params(
        &h,
        "getLedgerEntries",
        json!({"keys": [key_b64, {"not": "string"}]}),
        "keys[1] must be a string",
    )
    .await;

    // keys empty array
    assert_invalid_params(
        &h,
        "getLedgerEntries",
        json!({"keys": []}),
        "must not be empty",
    )
    .await;
}
