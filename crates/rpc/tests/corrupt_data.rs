//! Integration tests for RPC handler behavior with corrupt stored data.
//!
//! Verifies that:
//! - `getTransaction` / `getTransactions` return JSON-RPC error -32603
//!   ("XDR data integrity error") when stored body/result/meta XDR is corrupt
//! - `getEvents` skips corrupt event rows but returns surrounding valid events
//!
//! Uses the same simulation harness as `http_dispatch.rs`, with corrupt data
//! injected directly into the SQLite database via `HistoryQueries` /
//! `EventQueries` trait methods.

mod common;

use std::time::Duration;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use henyey_db::{
    EventQueries, EventRecord, HistoryQueries, LedgerCloseMetaQueries, StoreTxParams, TxStatus,
};
use henyey_rpc::RpcServer;
use serde_json::json;
use stellar_xdr::curr::{
    ContractEvent, ContractEventBody, ContractEventType, ContractEventV0, ExtensionPoint, Hash,
    Limits, Memo, MuxedAccount, Preconditions, ScVal, SequenceNumber, Transaction,
    TransactionEnvelope, TransactionExt, TransactionMeta, TransactionMetaV3, TransactionResult,
    TransactionResultExt, TransactionResultPair, TransactionResultResult, TransactionV1Envelope,
    Uint256, WriteXdr,
};

use common::{assert_envelope, boot_single_node_sim, post_rpc};

// ---------------------------------------------------------------------------
// XDR fixture builders
// ---------------------------------------------------------------------------

fn valid_envelope_bytes() -> Vec<u8> {
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
    env.to_xdr(Limits::none()).unwrap()
}

fn valid_result_pair_bytes() -> Vec<u8> {
    let pair = TransactionResultPair {
        transaction_hash: Hash([0u8; 32]),
        result: TransactionResult {
            fee_charged: 100,
            result: TransactionResultResult::TxSuccess(Default::default()),
            ext: TransactionResultExt::V0,
        },
    };
    pair.to_xdr(Limits::none()).unwrap()
}

fn valid_meta_bytes() -> Vec<u8> {
    let meta = TransactionMeta::V3(TransactionMetaV3 {
        ext: ExtensionPoint::V0,
        tx_changes_before: Default::default(),
        operations: Default::default(),
        tx_changes_after: Default::default(),
        soroban_meta: None,
    });
    meta.to_xdr(Limits::none()).unwrap()
}

/// Build a valid ContractEvent and return it as base64.
fn valid_event_xdr_b64() -> String {
    let event = ContractEvent {
        ext: ExtensionPoint::V0,
        contract_id: None,
        type_: ContractEventType::Contract,
        body: ContractEventBody::V0(ContractEventV0 {
            topics: vec![].try_into().unwrap(),
            data: ScVal::U32(42),
        }),
    };
    let bytes = event.to_xdr(Limits::none()).unwrap();
    BASE64.encode(&bytes)
}

const CORRUPT_BYTES: &[u8] = &[0xFF, 0xFE];

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Assert that a JSON-RPC response is an "XDR data integrity error".
fn assert_xdr_integrity_error(resp: &serde_json::Value, id: &serde_json::Value) {
    assert_envelope(resp, id);
    let err = &resp["error"];
    assert_eq!(
        err["code"],
        json!(-32603),
        "expected internal error (-32603), got: {err}"
    );
    assert_eq!(
        err["message"],
        json!("XDR data integrity error"),
        "expected 'XDR data integrity error', got: {err}"
    );
}

/// Boot a sim, bind RPC, return (sim, url, client).
async fn setup_rpc() -> (
    henyey_simulation::Simulation,
    String,
    reqwest::Client,
    std::sync::Arc<henyey_app::App>,
    tokio::task::JoinHandle<()>,
) {
    let (sim, node_id) = boot_single_node_sim().await;
    let app = sim.app(&node_id).expect("app");
    let (running, addr) = RpcServer::new(0, app.clone())
        .bind()
        .await
        .expect("rpc bind");
    let url = format!("http://{addr}/");
    let serve_handle = tokio::spawn(async move {
        let _ = running.serve().await;
    });
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("reqwest client");
    (sim, url, client, app, serve_handle)
}

// ---------------------------------------------------------------------------
// getTransaction corrupt tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_transaction_corrupt_body() {
    let (sim, url, client, app, handle) = setup_rpc().await;

    let tx_id = "corrupt_body_test_hash_001";
    app.database()
        .with_connection(|conn| {
            conn.store_transaction(&StoreTxParams {
                ledger_seq: 2,
                tx_index: 100,
                tx_id,
                body: CORRUPT_BYTES,
                result: &valid_result_pair_bytes(),
                meta: Some(&valid_meta_bytes()),
                status: TxStatus::Success,
            })
        })
        .unwrap();

    let id = json!("corrupt-body");
    let (status, resp) = post_rpc(
        &client,
        &url,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "getTransaction",
            "params": {"hash": tx_id}
        }),
    )
    .await;

    assert_eq!(status, 200);
    assert_xdr_integrity_error(&resp, &id);

    handle.abort();
    drop(sim);
}

#[tokio::test]
async fn get_transaction_corrupt_result() {
    let (sim, url, client, app, handle) = setup_rpc().await;

    let tx_id = "corrupt_result_test_hash_002";
    app.database()
        .with_connection(|conn| {
            conn.store_transaction(&StoreTxParams {
                ledger_seq: 2,
                tx_index: 101,
                tx_id,
                body: &valid_envelope_bytes(),
                result: CORRUPT_BYTES,
                meta: Some(&valid_meta_bytes()),
                status: TxStatus::Success,
            })
        })
        .unwrap();

    let id = json!("corrupt-result");
    let (status, resp) = post_rpc(
        &client,
        &url,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "getTransaction",
            "params": {"hash": tx_id}
        }),
    )
    .await;

    assert_eq!(status, 200);
    assert_xdr_integrity_error(&resp, &id);

    handle.abort();
    drop(sim);
}

#[tokio::test]
async fn get_transaction_corrupt_meta() {
    let (sim, url, client, app, handle) = setup_rpc().await;

    let tx_id = "corrupt_meta_test_hash_003";
    app.database()
        .with_connection(|conn| {
            conn.store_transaction(&StoreTxParams {
                ledger_seq: 2,
                tx_index: 102,
                tx_id,
                body: &valid_envelope_bytes(),
                result: &valid_result_pair_bytes(),
                meta: Some(CORRUPT_BYTES),
                status: TxStatus::Success,
            })
        })
        .unwrap();

    let id = json!("corrupt-meta");
    let (status, resp) = post_rpc(
        &client,
        &url,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "getTransaction",
            "params": {"hash": tx_id}
        }),
    )
    .await;

    assert_eq!(status, 200);
    assert_xdr_integrity_error(&resp, &id);

    handle.abort();
    drop(sim);
}

// ---------------------------------------------------------------------------
// getTransactions corrupt tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_transactions_corrupt_body() {
    let (sim, url, client, app, handle) = setup_rpc().await;

    app.database()
        .with_connection(|conn| {
            conn.store_transaction(&StoreTxParams {
                ledger_seq: 2,
                tx_index: 200,
                tx_id: "corrupt_txs_body_001",
                body: CORRUPT_BYTES,
                result: &valid_result_pair_bytes(),
                meta: Some(&valid_meta_bytes()),
                status: TxStatus::Success,
            })
        })
        .unwrap();

    let id = json!("txs-corrupt-body");
    let (status, resp) = post_rpc(
        &client,
        &url,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "getTransactions",
            "params": {"startLedger": 2}
        }),
    )
    .await;

    assert_eq!(status, 200);
    assert_xdr_integrity_error(&resp, &id);

    handle.abort();
    drop(sim);
}

#[tokio::test]
async fn get_transactions_corrupt_result() {
    let (sim, url, client, app, handle) = setup_rpc().await;

    app.database()
        .with_connection(|conn| {
            conn.store_transaction(&StoreTxParams {
                ledger_seq: 2,
                tx_index: 201,
                tx_id: "corrupt_txs_result_002",
                body: &valid_envelope_bytes(),
                result: CORRUPT_BYTES,
                meta: Some(&valid_meta_bytes()),
                status: TxStatus::Success,
            })
        })
        .unwrap();

    let id = json!("txs-corrupt-result");
    let (status, resp) = post_rpc(
        &client,
        &url,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "getTransactions",
            "params": {"startLedger": 2}
        }),
    )
    .await;

    assert_eq!(status, 200);
    assert_xdr_integrity_error(&resp, &id);

    handle.abort();
    drop(sim);
}

#[tokio::test]
async fn get_transactions_corrupt_meta() {
    let (sim, url, client, app, handle) = setup_rpc().await;

    app.database()
        .with_connection(|conn| {
            conn.store_transaction(&StoreTxParams {
                ledger_seq: 2,
                tx_index: 202,
                tx_id: "corrupt_txs_meta_003",
                body: &valid_envelope_bytes(),
                result: &valid_result_pair_bytes(),
                meta: Some(CORRUPT_BYTES),
                status: TxStatus::Success,
            })
        })
        .unwrap();

    let id = json!("txs-corrupt-meta");
    let (status, resp) = post_rpc(
        &client,
        &url,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "getTransactions",
            "params": {"startLedger": 2}
        }),
    )
    .await;

    assert_eq!(status, 200);
    assert_xdr_integrity_error(&resp, &id);

    handle.abort();
    drop(sim);
}

// ---------------------------------------------------------------------------
// getEvents corrupt tests
// ---------------------------------------------------------------------------

fn make_event_record(id: &str, ledger_seq: u32, tx_index: u32, event_xdr: &str) -> EventRecord {
    EventRecord {
        id: id.to_string(),
        ledger_seq,
        tx_index,
        op_index: 0,
        tx_hash: format!("tx_hash_{id}"),
        contract_id: None,
        event_type: ContractEventType::Contract,
        topics: vec![],
        event_xdr: event_xdr.to_string(),
        in_successful_contract_call: true,
    }
}

#[tokio::test]
async fn get_events_skips_corrupt_xdr() {
    let (sim, url, client, app, handle) = setup_rpc().await;

    let valid_xdr = valid_event_xdr_b64();
    let event_a = make_event_record("0000000008589934592-0000000001", 2, 0, &valid_xdr);
    let event_b = make_event_record("0000000008589934592-0000000002", 2, 1, &valid_xdr);
    // Corrupt event is last — tests cursor advancement past skipped row
    let event_c = make_event_record(
        "0000000008589934592-0000000003",
        2,
        2,
        "not-valid-base64!!!",
    );

    app.database()
        .with_connection(|conn| conn.store_events(&[event_a, event_b, event_c]))
        .unwrap();

    let id = json!("events-corrupt-xdr");
    let (status, resp) = post_rpc(
        &client,
        &url,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "getEvents",
            "params": {"startLedger": 2}
        }),
    )
    .await;

    assert_eq!(status, 200);
    assert_envelope(&resp, &id);
    let result = &resp["result"];
    let events = result["events"].as_array().expect("events array");
    assert_eq!(
        events.len(),
        2,
        "expected 2 valid events, got {}",
        events.len()
    );
    assert_eq!(events[0]["id"], "0000000008589934592-0000000001");
    assert_eq!(events[1]["id"], "0000000008589934592-0000000002");
    // Cursor advances past all DB rows including the corrupt last one
    assert_eq!(
        result["cursor"], "0000000008589934592-0000000003",
        "cursor should advance past corrupt row"
    );

    handle.abort();
    drop(sim);
}

#[tokio::test]
async fn get_events_all_corrupt_still_advances_cursor() {
    let (sim, url, client, app, handle) = setup_rpc().await;

    let event_a = make_event_record("0000000008589934592-0000000010", 2, 0, "corrupt-base64-aaa");
    let event_b = make_event_record("0000000008589934592-0000000011", 2, 1, "corrupt-base64-bbb");

    app.database()
        .with_connection(|conn| conn.store_events(&[event_a, event_b]))
        .unwrap();

    let id = json!("events-all-corrupt");
    let (status, resp) = post_rpc(
        &client,
        &url,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "getEvents",
            "params": {"startLedger": 2}
        }),
    )
    .await;

    assert_eq!(status, 200);
    assert_envelope(&resp, &id);
    let result = &resp["result"];
    let events = result["events"].as_array().expect("events array");
    assert_eq!(events.len(), 0, "all corrupt events should be skipped");
    // Cursor still advances to the last DB row
    assert_eq!(
        result["cursor"], "0000000008589934592-0000000011",
        "cursor should still advance past all corrupt rows"
    );

    handle.abort();
    drop(sim);
}

#[tokio::test]
async fn get_events_skips_corrupt_topic_json_mode() {
    let (sim, url, client, app, handle) = setup_rpc().await;

    let valid_xdr = valid_event_xdr_b64();
    // Valid topic: a base64-encoded ScVal
    let valid_topic = {
        let bytes = ScVal::U32(1).to_xdr(Limits::none()).unwrap();
        BASE64.encode(&bytes)
    };

    let event_a = EventRecord {
        id: "0000000008589934592-0000000020".to_string(),
        ledger_seq: 2,
        tx_index: 0,
        op_index: 0,
        tx_hash: "tx_hash_topic_a".to_string(),
        contract_id: None,
        event_type: ContractEventType::Contract,
        topics: vec![valid_topic],
        event_xdr: valid_xdr.clone(),
        in_successful_contract_call: true,
    };
    let event_b = EventRecord {
        id: "0000000008589934592-0000000021".to_string(),
        ledger_seq: 2,
        tx_index: 1,
        op_index: 0,
        tx_hash: "tx_hash_topic_b".to_string(),
        contract_id: None,
        event_type: ContractEventType::Contract,
        topics: vec!["not-valid-base64!!!".to_string()],
        event_xdr: valid_xdr,
        in_successful_contract_call: true,
    };

    app.database()
        .with_connection(|conn| conn.store_events(&[event_a, event_b]))
        .unwrap();

    let id = json!("events-corrupt-topic");
    let (status, resp) = post_rpc(
        &client,
        &url,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "getEvents",
            "params": {"startLedger": 2, "xdrFormat": "json"}
        }),
    )
    .await;

    assert_eq!(status, 200);
    assert_envelope(&resp, &id);
    let result = &resp["result"];
    let events = result["events"].as_array().expect("events array");
    assert_eq!(
        events.len(),
        1,
        "corrupt-topic event should be skipped in JSON mode"
    );
    assert_eq!(events[0]["id"], "0000000008589934592-0000000020");

    handle.abort();
    drop(sim);
}

// ---------------------------------------------------------------------------
// getLatestLedger corrupt / missing metadata tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_latest_ledger_corrupt_metadata() {
    let (sim, url, client, app, handle) = setup_rpc().await;

    // Overwrite the current ledger's LedgerCloseMeta with corrupt bytes.
    let ledger_num = app.ledger_summary().num;
    app.database()
        .with_connection(|conn| conn.store_ledger_close_meta(ledger_num, CORRUPT_BYTES))
        .unwrap();

    let id = json!("latest-corrupt-meta");
    let (status, resp) = post_rpc(
        &client,
        &url,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "getLatestLedger"
        }),
    )
    .await;

    assert_eq!(status, 200);
    assert_xdr_integrity_error(&resp, &id);

    handle.abort();
    drop(sim);
}

#[tokio::test]
async fn get_latest_ledger_missing_metadata() {
    let (sim, url, client, app, handle) = setup_rpc().await;

    // Delete the current ledger's LedgerCloseMeta row so it appears missing.
    let ledger_num = app.ledger_summary().num;
    app.database()
        .with_connection(|conn| conn.delete_old_ledger_close_meta(ledger_num, 1000))
        .unwrap();

    let id = json!("latest-missing-meta");
    let (status, resp) = post_rpc(
        &client,
        &url,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "getLatestLedger"
        }),
    )
    .await;

    assert_eq!(status, 200);
    assert_envelope(&resp, &id);
    let result = &resp["result"];
    assert_eq!(
        result["metadataXdr"], "",
        "missing metadata should produce empty string"
    );
    // Other fields should still be present.
    assert!(result["sequence"].as_u64().unwrap() > 0);
    assert!(!result["headerXdr"].as_str().unwrap().is_empty());

    handle.abort();
    drop(sim);
}

// ---------------------------------------------------------------------------
// getLedgers corrupt metadata test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_ledgers_corrupt_metadata() {
    let (sim, url, client, app, handle) = setup_rpc().await;

    // Overwrite ledger 2's LedgerCloseMeta with corrupt bytes.
    let ledger_num = app.ledger_summary().num;
    app.database()
        .with_connection(|conn| conn.store_ledger_close_meta(ledger_num, CORRUPT_BYTES))
        .unwrap();

    let id = json!("ledgers-corrupt-meta");
    let (status, resp) = post_rpc(
        &client,
        &url,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "getLedgers",
            "params": {"startLedger": ledger_num}
        }),
    )
    .await;

    assert_eq!(status, 200);
    assert_xdr_integrity_error(&resp, &id);

    handle.abort();
    drop(sim);
}

// ---------------------------------------------------------------------------
// Sequence mismatch tests (valid XDR, wrong embedded sequence)
// ---------------------------------------------------------------------------

/// Build a minimal, valid `LedgerCloseMeta` with the given ledger sequence.
fn valid_lcm_bytes(seq: u32) -> Vec<u8> {
    use stellar_xdr::curr::{
        Hash, LedgerCloseMeta, LedgerCloseMetaV0, LedgerHeader, LedgerHeaderExt,
        LedgerHeaderHistoryEntry, LedgerHeaderHistoryEntryExt, StellarValue, StellarValueExt,
        TimePoint, TransactionSet, WriteXdr,
    };
    let lcm = LedgerCloseMeta::V0(LedgerCloseMetaV0 {
        ledger_header: LedgerHeaderHistoryEntry {
            hash: Hash([0; 32]),
            header: LedgerHeader {
                ledger_version: 21,
                previous_ledger_hash: Hash([0; 32]),
                scp_value: StellarValue {
                    tx_set_hash: Hash([0; 32]),
                    close_time: TimePoint(0),
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
            },
            ext: LedgerHeaderHistoryEntryExt::V0,
        },
        tx_set: TransactionSet {
            previous_ledger_hash: Hash([0; 32]),
            txs: vec![].try_into().unwrap(),
        },
        tx_processing: vec![].try_into().unwrap(),
        upgrades_processing: vec![].try_into().unwrap(),
        scp_info: vec![].try_into().unwrap(),
    });
    lcm.to_xdr(Limits::none()).unwrap()
}

#[tokio::test]
async fn get_ledgers_sequence_mismatch() {
    let (sim, url, client, app, handle) = setup_rpc().await;

    // Store a valid LCM whose embedded sequence (9999) differs from the DB key.
    let ledger_num = app.ledger_summary().num;
    let wrong_seq_lcm = valid_lcm_bytes(9999);
    app.database()
        .with_connection(|conn| conn.store_ledger_close_meta(ledger_num, &wrong_seq_lcm))
        .unwrap();

    let id = json!("ledgers-seq-mismatch");
    let (status, resp) = post_rpc(
        &client,
        &url,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "getLedgers",
            "params": {"startLedger": ledger_num}
        }),
    )
    .await;

    assert_eq!(status, 200);
    assert_xdr_integrity_error(&resp, &id);

    handle.abort();
    drop(sim);
}

#[tokio::test]
async fn get_latest_ledger_sequence_mismatch() {
    let (sim, url, client, app, handle) = setup_rpc().await;

    // Store a valid LCM whose embedded sequence (9999) differs from the
    // current ledger number used by getLatestLedger.
    let ledger_num = app.ledger_summary().num;
    let wrong_seq_lcm = valid_lcm_bytes(9999);
    app.database()
        .with_connection(|conn| conn.store_ledger_close_meta(ledger_num, &wrong_seq_lcm))
        .unwrap();

    let id = json!("latest-seq-mismatch");
    let (status, resp) = post_rpc(
        &client,
        &url,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "getLatestLedger"
        }),
    )
    .await;

    assert_eq!(status, 200);
    assert_xdr_integrity_error(&resp, &id);

    handle.abort();
    drop(sim);
}
