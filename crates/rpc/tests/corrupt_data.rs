//! Integration tests for RPC handler behavior with corrupt stored data.
//!
//! Verifies that:
//! - `getTransaction` / `getTransactions` return JSON-RPC error -32603
//!   ("XDR data integrity error") when stored body/result/meta XDR is corrupt
//! - `getEvents` skips corrupt event rows but returns surrounding valid events
//!
//! Uses the lightweight [`FakeRpcApp`] harness with corrupt data injected
//! directly into the in-memory SQLite database via `HistoryQueries` /
//! `EventQueries` trait methods.

mod common;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use henyey_db::{
    EventQueries, EventRecord, HistoryQueries, LedgerCloseMetaQueries, LedgerQueries,
    StoreTxParams, TxStatus,
};
use serde_json::json;
use stellar_xdr::curr::{
    ContractEvent, ContractEventBody, ContractEventType, ContractEventV0, ExtensionPoint, Hash,
    LedgerHeader, LedgerHeaderExt, Limits, Memo, MuxedAccount, Preconditions, ScVal,
    SequenceNumber, StellarValue, StellarValueExt, TimePoint, Transaction, TransactionEnvelope,
    TransactionExt, TransactionMeta, TransactionMetaV3, TransactionResult, TransactionResultExt,
    TransactionResultPair, TransactionResultResult, TransactionV1Envelope, Uint256, WriteXdr,
};

use common::fake_app::{FakeRpcApp, FakeRpcTestHarness};
use henyey_rpc::RpcAppHandle;

use crate::common::assert_envelope;

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

/// Seed a ledger header row in the DB so `require_close_times()` succeeds.
fn seed_ledger_header(db: &henyey_db::Database, seq: u32, close_time: u64) {
    let header = test_header(seq, close_time);
    let data = header.to_xdr(Limits::none()).unwrap();
    db.with_connection(|conn| conn.store_ledger_header(&header, &data))
        .unwrap();
}

/// Boot a [`FakeRpcTestHarness`] with `ledger_seq` set and a DB header row
/// seeded at the given sequence. Suitable for getTransaction, getTransactions,
/// and getEvents tests that use `startLedger`.
async fn setup_fake_rpc() -> FakeRpcTestHarness {
    let app = FakeRpcApp::builder().ledger_seq(2).build();
    let h = FakeRpcTestHarness::start(app).await;
    seed_ledger_header(h.app.database(), 2, 1_700_000_000);
    h
}

/// Boot a [`FakeRpcTestHarness`] with a full header snapshot at the given
/// sequence, plus a matching DB header row. Suitable for getLatestLedger
/// and getLedgers tests that depend on `ledger_snapshot().header.ledger_seq`.
async fn setup_fake_rpc_with_header(seq: u32, close_time: u64) -> FakeRpcTestHarness {
    let header = test_header(seq, close_time);
    let app = FakeRpcApp::builder().header_snapshot(header).build();
    let h = FakeRpcTestHarness::start(app).await;
    seed_ledger_header(h.app.database(), seq, close_time);
    h
}

// ---------------------------------------------------------------------------
// getTransaction corrupt tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_transaction_corrupt_body() {
    let h = setup_fake_rpc().await;

    let tx_id = "corrupt_body_test_hash_001";
    h.app
        .database()
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
    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "getTransaction",
            "params": {"hash": tx_id}
        }))
        .await;

    assert_eq!(status, 200);
    assert_xdr_integrity_error(&resp, &id);
}

#[tokio::test]
async fn get_transaction_corrupt_result() {
    let h = setup_fake_rpc().await;

    let tx_id = "corrupt_result_test_hash_002";
    h.app
        .database()
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
    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "getTransaction",
            "params": {"hash": tx_id}
        }))
        .await;

    assert_eq!(status, 200);
    assert_xdr_integrity_error(&resp, &id);
}

#[tokio::test]
async fn get_transaction_corrupt_meta() {
    let h = setup_fake_rpc().await;

    let tx_id = "corrupt_meta_test_hash_003";
    h.app
        .database()
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
    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "getTransaction",
            "params": {"hash": tx_id}
        }))
        .await;

    assert_eq!(status, 200);
    assert_xdr_integrity_error(&resp, &id);
}

// ---------------------------------------------------------------------------
// getTransactions corrupt tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_transactions_corrupt_body() {
    let h = setup_fake_rpc().await;

    h.app
        .database()
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
    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "getTransactions",
            "params": {"startLedger": 2}
        }))
        .await;

    assert_eq!(status, 200);
    assert_xdr_integrity_error(&resp, &id);
}

#[tokio::test]
async fn get_transactions_corrupt_result() {
    let h = setup_fake_rpc().await;

    h.app
        .database()
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
    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "getTransactions",
            "params": {"startLedger": 2}
        }))
        .await;

    assert_eq!(status, 200);
    assert_xdr_integrity_error(&resp, &id);
}

#[tokio::test]
async fn get_transactions_corrupt_meta() {
    let h = setup_fake_rpc().await;

    h.app
        .database()
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
    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "getTransactions",
            "params": {"startLedger": 2}
        }))
        .await;

    assert_eq!(status, 200);
    assert_xdr_integrity_error(&resp, &id);
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
    let h = setup_fake_rpc().await;

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

    h.app
        .database()
        .with_connection(|conn| conn.store_events(&[event_a, event_b, event_c]))
        .unwrap();

    let id = json!("events-corrupt-xdr");
    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "getEvents",
            "params": {"startLedger": 2}
        }))
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
}

#[tokio::test]
async fn get_events_all_corrupt_still_advances_cursor() {
    let h = setup_fake_rpc().await;

    let event_a = make_event_record("0000000008589934592-0000000010", 2, 0, "corrupt-base64-aaa");
    let event_b = make_event_record("0000000008589934592-0000000011", 2, 1, "corrupt-base64-bbb");

    h.app
        .database()
        .with_connection(|conn| conn.store_events(&[event_a, event_b]))
        .unwrap();

    let id = json!("events-all-corrupt");
    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "getEvents",
            "params": {"startLedger": 2}
        }))
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
}

#[tokio::test]
async fn get_events_skips_corrupt_topic_json_mode() {
    let h = setup_fake_rpc().await;

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

    h.app
        .database()
        .with_connection(|conn| conn.store_events(&[event_a, event_b]))
        .unwrap();

    let id = json!("events-corrupt-topic");
    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "getEvents",
            "params": {"startLedger": 2, "xdrFormat": "json"}
        }))
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
}

// ---------------------------------------------------------------------------
// getLatestLedger corrupt / missing metadata tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_latest_ledger_corrupt_metadata() {
    let h = setup_fake_rpc_with_header(2, 1_700_000_000).await;

    // Overwrite the current ledger's LedgerCloseMeta with corrupt bytes.
    h.app
        .database()
        .with_connection(|conn| conn.store_ledger_close_meta(2, CORRUPT_BYTES))
        .unwrap();

    let id = json!("latest-corrupt-meta");
    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "getLatestLedger"
        }))
        .await;

    assert_eq!(status, 200);
    assert_xdr_integrity_error(&resp, &id);
}

#[tokio::test]
async fn get_latest_ledger_missing_metadata() {
    let h = setup_fake_rpc_with_header(2, 1_700_000_000).await;

    // Seed valid metadata then delete it, so it appears "missing" for a
    // ledger that did exist — matching the original sim-backed behavior.
    let valid_lcm = valid_lcm_bytes(2);
    h.app
        .database()
        .with_connection(|conn| {
            conn.store_ledger_close_meta(2, &valid_lcm)?;
            conn.delete_old_ledger_close_meta(2, 1000)
        })
        .unwrap();

    let id = json!("latest-missing-meta");
    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "getLatestLedger"
        }))
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
}

// ---------------------------------------------------------------------------
// getLedgers corrupt metadata test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_ledgers_corrupt_metadata() {
    let h = setup_fake_rpc_with_header(2, 1_700_000_000).await;

    // Overwrite ledger 2's LedgerCloseMeta with corrupt bytes.
    h.app
        .database()
        .with_connection(|conn| conn.store_ledger_close_meta(2, CORRUPT_BYTES))
        .unwrap();

    let id = json!("ledgers-corrupt-meta");
    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "getLedgers",
            "params": {"startLedger": 2}
        }))
        .await;

    assert_eq!(status, 200);
    assert_xdr_integrity_error(&resp, &id);
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
    let h = setup_fake_rpc_with_header(2, 1_700_000_000).await;

    // Store a valid LCM whose embedded sequence (9999) differs from the DB key.
    let wrong_seq_lcm = valid_lcm_bytes(9999);
    h.app
        .database()
        .with_connection(|conn| conn.store_ledger_close_meta(2, &wrong_seq_lcm))
        .unwrap();

    let id = json!("ledgers-seq-mismatch");
    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "getLedgers",
            "params": {"startLedger": 2}
        }))
        .await;

    assert_eq!(status, 200);
    assert_xdr_integrity_error(&resp, &id);
}

#[tokio::test]
async fn get_latest_ledger_sequence_mismatch() {
    let h = setup_fake_rpc_with_header(2, 1_700_000_000).await;

    // Store a valid LCM whose embedded sequence (9999) differs from the
    // current ledger number used by getLatestLedger.
    let wrong_seq_lcm = valid_lcm_bytes(9999);
    h.app
        .database()
        .with_connection(|conn| conn.store_ledger_close_meta(2, &wrong_seq_lcm))
        .unwrap();

    let id = json!("latest-seq-mismatch");
    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "getLatestLedger"
        }))
        .await;

    assert_eq!(status, 200);
    assert_xdr_integrity_error(&resp, &id);
}

// ---------------------------------------------------------------------------
// getLatestLedger consistency regression test
// ---------------------------------------------------------------------------

/// Verify that all in-memory fields in the `getLatestLedger` response are
/// derived from the same atomic header snapshot. Decodes `headerXdr` and
/// asserts it matches `sequence`, `protocolVersion`, `closeTime`, and `id`.
///
/// This is a regression guard: if someone re-introduces multi-read (separate
/// `current_header()` + `current_header_hash()` calls), a ledger close
/// between reads can cause these fields to disagree.
#[tokio::test]
async fn get_latest_ledger_fields_consistent_with_header_xdr() {
    use sha2::{Digest, Sha256};
    use stellar_xdr::curr::{LedgerHeader as XdrLedgerHeader, ReadXdr};

    let h = setup_fake_rpc_with_header(2, 1_700_000_000).await;

    // Store a valid LCM at seq 2 so metadataXdr is populated.
    let valid_lcm = valid_lcm_bytes(2);
    h.app
        .database()
        .with_connection(|conn| conn.store_ledger_close_meta(2, &valid_lcm))
        .unwrap();

    let id = json!("consistency-check");
    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "getLatestLedger"
        }))
        .await;

    assert_eq!(status, 200);
    assert_envelope(&resp, &id);
    let result = &resp["result"];

    // Decode headerXdr
    let header_b64 = result["headerXdr"]
        .as_str()
        .expect("headerXdr must be a string");
    let header_bytes = BASE64.decode(header_b64).expect("valid base64");
    let header =
        XdrLedgerHeader::from_xdr(&header_bytes, Limits::none()).expect("valid LedgerHeader XDR");

    // Assert all in-memory fields match the decoded header
    let resp_seq = result["sequence"].as_u64().expect("sequence");
    assert_eq!(
        resp_seq, header.ledger_seq as u64,
        "response sequence must match headerXdr.ledger_seq"
    );

    let resp_version = result["protocolVersion"].as_u64().expect("protocolVersion");
    assert_eq!(
        resp_version, header.ledger_version as u64,
        "response protocolVersion must match headerXdr.ledger_version"
    );

    let resp_close_time: u64 = result["closeTime"]
        .as_str()
        .expect("closeTime string")
        .parse()
        .expect("closeTime parses as u64");
    assert_eq!(
        resp_close_time, header.scp_value.close_time.0,
        "response closeTime must match headerXdr.scp_value.close_time"
    );

    // Verify id == hex(SHA-256(headerXdr bytes))
    let resp_id = result["id"].as_str().expect("id");
    let computed_hash = Sha256::digest(&header_bytes);
    let computed_hex = hex::encode(computed_hash);
    assert_eq!(
        resp_id, computed_hex,
        "response id must equal SHA-256 of headerXdr bytes"
    );
}

// ---------------------------------------------------------------------------
// getLedgers budget truncation and pagination tests
// ---------------------------------------------------------------------------

/// Boot a [`FakeRpcTestHarness`] with a custom `max_ledger_meta_load_bytes`
/// budget and a header snapshot at the given sequence.
async fn setup_fake_rpc_with_budget(
    seq: u32,
    close_time: u64,
    budget: usize,
) -> FakeRpcTestHarness {
    let header = test_header(seq, close_time);
    let app = FakeRpcApp::builder()
        .header_snapshot(header)
        .max_ledger_meta_load_bytes(budget)
        .build();
    let h = FakeRpcTestHarness::start(app).await;
    seed_ledger_header(h.app.database(), seq, close_time);
    h
}

/// Verify getLedgers returns valid results and correct cursor for pagination.
#[tokio::test]
async fn get_ledgers_pagination_with_cursor() {
    // Set up with latest ledger = 10, oldest = 2
    let h = setup_fake_rpc_with_header(10, 1_700_000_010).await;

    // Store valid (small) LCMs for sequences 2..=10
    for seq in 2..=10 {
        let lcm = valid_lcm_bytes(seq);
        h.app
            .database()
            .with_connection(|conn| conn.store_ledger_close_meta(seq, &lcm))
            .unwrap();
        seed_ledger_header(h.app.database(), seq, 1_700_000_000 + seq as u64);
    }

    // First page: startLedger=2, limit=3
    let id = json!("page1");
    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "getLedgers",
            "params": {
                "startLedger": 2,
                "pagination": { "limit": 3 }
            }
        }))
        .await;
    assert_eq!(status, 200);
    let result = &resp["result"];
    let ledgers = result["ledgers"].as_array().expect("ledgers array");
    assert_eq!(ledgers.len(), 3, "first page should have 3 ledgers");
    assert_eq!(ledgers[0]["sequence"], json!(2));
    assert_eq!(ledgers[2]["sequence"], json!(4));
    let cursor = result["cursor"].as_str().expect("cursor");
    assert_eq!(cursor, "4");

    // Second page: cursor=4, limit=3
    let id2 = json!("page2");
    let (status2, resp2) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": id2,
            "method": "getLedgers",
            "params": {
                "pagination": { "cursor": "4", "limit": 3 }
            }
        }))
        .await;
    assert_eq!(status2, 200);
    let result2 = &resp2["result"];
    let ledgers2 = result2["ledgers"].as_array().expect("ledgers array");
    assert_eq!(ledgers2.len(), 3, "second page should have 3 ledgers");
    assert_eq!(ledgers2[0]["sequence"], json!(5));
    assert_eq!(ledgers2[2]["sequence"], json!(7));
}

/// Verify getLedgers works with both xdr (base64) and json output formats.
#[tokio::test]
async fn get_ledgers_both_formats() {
    let h = setup_fake_rpc_with_header(3, 1_700_000_003).await;

    for seq in 2..=3 {
        let lcm = valid_lcm_bytes(seq);
        h.app
            .database()
            .with_connection(|conn| conn.store_ledger_close_meta(seq, &lcm))
            .unwrap();
        seed_ledger_header(h.app.database(), seq, 1_700_000_000 + seq as u64);
    }

    // Test base64 format (default)
    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": "fmt-xdr",
            "method": "getLedgers",
            "params": { "startLedger": 2 }
        }))
        .await;
    assert_eq!(status, 200);
    let result = &resp["result"];
    let ledgers = result["ledgers"].as_array().expect("ledgers");
    assert!(!ledgers.is_empty());
    // Base64 format: should have metadataXdr and headerXdr
    assert!(
        ledgers[0].get("metadataXdr").is_some(),
        "base64 format should have metadataXdr"
    );
    assert!(
        ledgers[0].get("headerXdr").is_some(),
        "base64 format should have headerXdr"
    );

    // Test JSON format
    let (status2, resp2) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": "fmt-json",
            "method": "getLedgers",
            "params": {
                "startLedger": 2,
                "xdrFormat": "json"
            }
        }))
        .await;
    assert_eq!(status2, 200);
    let result2 = &resp2["result"];
    let ledgers2 = result2["ledgers"].as_array().expect("ledgers");
    assert!(!ledgers2.is_empty());
    // JSON format: should have metadataJson and headerJson
    assert!(
        ledgers2[0].get("metadataJson").is_some(),
        "json format should have metadataJson"
    );
    assert!(
        ledgers2[0].get("headerJson").is_some(),
        "json format should have headerJson"
    );
}

/// Verify getLedgers truncates results when the DB load budget is exceeded,
/// and that the cursor correctly points to the last included ledger for
/// pagination to resume.
#[tokio::test]
async fn get_ledgers_budget_truncation() {
    // Each valid_lcm_bytes blob is ~200 bytes. Set budget to 500 so that
    // 2 ledgers fit but the 3rd is excluded.
    let budget = 500;
    let h = setup_fake_rpc_with_budget(10, 1_700_000_010, budget).await;

    // Store 5 valid LCMs at sequences 2..=6
    for seq in 2..=6 {
        let lcm = valid_lcm_bytes(seq);
        h.app
            .database()
            .with_connection(|conn| conn.store_ledger_close_meta(seq, &lcm))
            .unwrap();
        seed_ledger_header(h.app.database(), seq, 1_700_000_000 + seq as u64);
    }

    // Request limit=5 but budget should truncate before all 5 are returned.
    let id = json!("budget-trunc");
    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "getLedgers",
            "params": {
                "startLedger": 2,
                "pagination": { "limit": 5 }
            }
        }))
        .await;

    assert_eq!(status, 200);
    let result = &resp["result"];
    let ledgers = result["ledgers"].as_array().expect("ledgers array");

    // Should get fewer than 5 due to budget truncation.
    assert!(
        ledgers.len() < 5,
        "expected budget truncation to return fewer than 5 ledgers, got {}",
        ledgers.len()
    );
    // Must return at least 1 (first-row guarantee).
    assert!(
        !ledgers.is_empty(),
        "budget truncation must return at least 1 ledger"
    );
    assert_eq!(ledgers[0]["sequence"], json!(2));

    // Cursor should point to the last included ledger.
    let cursor = result["cursor"].as_str().expect("cursor");
    let last_seq = ledgers.last().unwrap()["sequence"].as_u64().unwrap();
    assert_eq!(
        cursor,
        last_seq.to_string(),
        "cursor must equal last included ledger sequence"
    );

    // Resume from cursor: next page should start after the last returned ledger.
    let id2 = json!("budget-trunc-resume");
    let (status2, resp2) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": id2,
            "method": "getLedgers",
            "params": {
                "pagination": { "cursor": cursor, "limit": 5 }
            }
        }))
        .await;

    assert_eq!(status2, 200);
    let result2 = &resp2["result"];
    let ledgers2 = result2["ledgers"].as_array().expect("ledgers array");
    // Should resume from next ledger after cursor.
    if !ledgers2.is_empty() {
        assert_eq!(
            ledgers2[0]["sequence"].as_u64().unwrap(),
            last_seq + 1,
            "resumed page must start at cursor + 1"
        );
    }
}

/// Verify that a single oversized first ledger is still returned (pagination
/// forward-progress guarantee) even when the budget is very small.
#[tokio::test]
async fn get_ledgers_oversized_first_row_still_returned() {
    // Budget of 1 byte — far smaller than any valid LCM (~200 bytes).
    let h = setup_fake_rpc_with_budget(3, 1_700_000_003, 1).await;

    let lcm = valid_lcm_bytes(2);
    h.app
        .database()
        .with_connection(|conn| conn.store_ledger_close_meta(2, &lcm))
        .unwrap();
    seed_ledger_header(h.app.database(), 2, 1_700_000_002);

    let lcm3 = valid_lcm_bytes(3);
    h.app
        .database()
        .with_connection(|conn| conn.store_ledger_close_meta(3, &lcm3))
        .unwrap();
    seed_ledger_header(h.app.database(), 3, 1_700_000_003);

    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": "oversized-first",
            "method": "getLedgers",
            "params": { "startLedger": 2, "pagination": { "limit": 10 } }
        }))
        .await;

    assert_eq!(status, 200);
    let result = &resp["result"];
    let ledgers = result["ledgers"].as_array().expect("ledgers");
    // Must return exactly 1 ledger: the first one is always included,
    // but the second exceeds the budget.
    assert_eq!(
        ledgers.len(),
        1,
        "with 1-byte budget, only the first ledger should be returned"
    );
    assert_eq!(ledgers[0]["sequence"], json!(2));
}

// ---------------------------------------------------------------------------
// getTransactions budget truncation and pagination tests
// ---------------------------------------------------------------------------

/// Boot a [`FakeRpcTestHarness`] with a custom `max_tx_load_bytes` budget
/// and a header snapshot at the given sequence.
async fn setup_fake_rpc_with_tx_budget(
    seq: u32,
    close_time: u64,
    budget: usize,
) -> FakeRpcTestHarness {
    let header = test_header(seq, close_time);
    let app = FakeRpcApp::builder()
        .header_snapshot(header)
        .max_tx_load_bytes(budget)
        .build();
    let h = FakeRpcTestHarness::start(app).await;
    seed_ledger_header(h.app.database(), seq, close_time);
    h
}

/// Seed several valid transactions at a given ledger sequence, each with
/// the same payload sizes. Returns the per-row byte count.
fn seed_transactions(h: &FakeRpcTestHarness, ledger_seq: u32, count: u32) -> usize {
    let body = valid_envelope_bytes();
    let result = valid_result_pair_bytes();
    let meta = valid_meta_bytes();
    let row_bytes = body.len() + result.len() + meta.len();
    for i in 0..count {
        let tx_id = format!("budget_tx_{ledger_seq}_{i:04}");
        h.app
            .database()
            .with_connection(|conn| {
                conn.store_transaction(&StoreTxParams {
                    ledger_seq,
                    tx_index: i,
                    tx_id: &tx_id,
                    body: &body,
                    result: &result,
                    meta: Some(&meta),
                    status: TxStatus::Success,
                })
            })
            .unwrap();
    }
    row_bytes
}

/// Verify getTransactions respects byte budget and truncates results.
#[tokio::test]
async fn get_transactions_budget_truncation() {
    // Each valid tx blob is relatively small. Set budget so only ~2 fit.
    let h = setup_fake_rpc_with_tx_budget(10, 1_700_000_010, 1).await;

    // We need to know the actual per-row size to set a proper budget.
    let row_bytes = seed_transactions(&h, 2, 5);
    // Seed the header for ledger 2 (where txs live)
    seed_ledger_header(h.app.database(), 2, 1_700_000_002);

    // Recreate with a budget that fits 2 rows but not 3
    let budget = row_bytes * 2 + row_bytes / 2; // 2.5x one row
    let h = setup_fake_rpc_with_tx_budget(10, 1_700_000_010, budget).await;
    let row_bytes2 = seed_transactions(&h, 2, 5);
    assert_eq!(row_bytes, row_bytes2);
    seed_ledger_header(h.app.database(), 2, 1_700_000_002);

    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": "budget-tx-trunc",
            "method": "getTransactions",
            "params": {
                "startLedger": 2,
                "pagination": { "limit": 5 }
            }
        }))
        .await;

    assert_eq!(status, 200);
    let result = &resp["result"];
    let txs = result["transactions"]
        .as_array()
        .expect("transactions array");

    // Should get fewer than 5 due to budget truncation.
    assert!(
        txs.len() < 5,
        "expected budget truncation to return fewer than 5 txs, got {}",
        txs.len()
    );
    // Must return at least 1 (first-row guarantee).
    assert!(
        !txs.is_empty(),
        "budget truncation must return at least 1 tx"
    );
    // Cursor must be present and non-empty for pagination.
    let cursor = result["cursor"].as_str().expect("cursor");
    assert!(!cursor.is_empty());
}

/// Verify getTransactions pagination resumes correctly after budget truncation.
#[tokio::test]
async fn get_transactions_budget_truncation_resume() {
    let body = valid_envelope_bytes();
    let result_bytes = valid_result_pair_bytes();
    let meta = valid_meta_bytes();
    let row_bytes = body.len() + result_bytes.len() + meta.len();

    // Budget fits exactly 1 row (but first-row guarantee means at least 1)
    let budget = row_bytes;
    let h = setup_fake_rpc_with_tx_budget(10, 1_700_000_010, budget).await;
    seed_ledger_header(h.app.database(), 2, 1_700_000_002);

    // Seed 3 transactions at ledger 2
    for i in 0..3u32 {
        let tx_id = format!("resume_tx_{i:04}");
        h.app
            .database()
            .with_connection(|conn| {
                conn.store_transaction(&StoreTxParams {
                    ledger_seq: 2,
                    tx_index: i,
                    tx_id: &tx_id,
                    body: &body,
                    result: &result_bytes,
                    meta: Some(&meta),
                    status: TxStatus::Success,
                })
            })
            .unwrap();
    }

    // First page
    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": "resume-p1",
            "method": "getTransactions",
            "params": {
                "startLedger": 2,
                "pagination": { "limit": 10 }
            }
        }))
        .await;
    assert_eq!(status, 200);
    let result = &resp["result"];
    let txs = result["transactions"].as_array().expect("transactions");
    assert_eq!(txs.len(), 1, "budget should limit to 1 tx per page");
    let cursor = result["cursor"].as_str().expect("cursor");

    // Second page using cursor
    let (status2, resp2) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": "resume-p2",
            "method": "getTransactions",
            "params": {
                "pagination": { "cursor": cursor, "limit": 10 }
            }
        }))
        .await;
    assert_eq!(status2, 200);
    let result2 = &resp2["result"];
    let txs2 = result2["transactions"].as_array().expect("transactions");
    assert_eq!(txs2.len(), 1, "second page should also have 1 tx");

    // Third page
    let cursor2 = result2["cursor"].as_str().expect("cursor");
    let (status3, resp3) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": "resume-p3",
            "method": "getTransactions",
            "params": {
                "pagination": { "cursor": cursor2, "limit": 10 }
            }
        }))
        .await;
    assert_eq!(status3, 200);
    let txs3 = resp3["result"]["transactions"]
        .as_array()
        .expect("transactions");
    assert_eq!(txs3.len(), 1, "third page should have the last tx");
}

/// Verify the first-row guarantee: even with a tiny budget, at least one
/// transaction is returned and the cursor advances.
#[tokio::test]
async fn get_transactions_budget_oversized_first_row() {
    // Budget of 1 byte — far smaller than any real tx
    let h = setup_fake_rpc_with_tx_budget(10, 1_700_000_010, 1).await;
    seed_ledger_header(h.app.database(), 2, 1_700_000_002);

    let body = valid_envelope_bytes();
    let result_bytes = valid_result_pair_bytes();
    let meta = valid_meta_bytes();
    h.app
        .database()
        .with_connection(|conn| {
            conn.store_transaction(&StoreTxParams {
                ledger_seq: 2,
                tx_index: 0,
                tx_id: "oversized_first_row",
                body: &body,
                result: &result_bytes,
                meta: Some(&meta),
                status: TxStatus::Success,
            })
        })
        .unwrap();

    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": "oversized-first",
            "method": "getTransactions",
            "params": {
                "startLedger": 2,
                "pagination": { "limit": 10 }
            }
        }))
        .await;

    assert_eq!(status, 200);
    let result = &resp["result"];
    let txs = result["transactions"].as_array().expect("transactions");
    assert_eq!(
        txs.len(),
        1,
        "with 1-byte budget, first tx must still be returned"
    );
    let cursor = result["cursor"].as_str().expect("cursor");
    assert!(
        !cursor.is_empty(),
        "cursor must be present for forward progress"
    );
}

// ---------------------------------------------------------------------------
// getTransactions query-budget (max_tx_query_ops) tests
// ---------------------------------------------------------------------------

/// Verify that exceeding max_tx_query_ops returns JSON-RPC error -32002.
#[tokio::test]
async fn get_transactions_query_budget_exceeded_returns_error_code() {
    // Budget of 1000 opcodes → div_ceil(1000/1000) = 1 callback → first
    // progress-handler check interrupts the query.
    let app = FakeRpcApp::builder()
        .ledger_seq(10)
        .max_tx_query_ops(1000)
        .build();
    let h = FakeRpcTestHarness::start(app).await;
    seed_ledger_header(h.app.database(), 2, 1_700_000_002);
    seed_ledger_header(h.app.database(), 10, 1_700_000_010);

    // Seed 100 transactions — enough rows that even a simple range scan
    // exceeds 1000 VM opcodes (validated by the unit test at
    // crates/db/src/queries/history.rs:test_query_budget_exceeded).
    let body = valid_envelope_bytes();
    let result = valid_result_pair_bytes();
    let meta = valid_meta_bytes();
    for i in 0..100u32 {
        let tx_id = format!("tx_budget_exceeded_{i:04}");
        h.app
            .database()
            .with_connection(|conn| {
                conn.store_transaction(&StoreTxParams {
                    ledger_seq: 2,
                    tx_index: i,
                    tx_id: &tx_id,
                    body: &body,
                    result: &result,
                    meta: Some(&meta),
                    status: TxStatus::Success,
                })
            })
            .unwrap();
    }

    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": "budget-exceeded",
            "method": "getTransactions",
            "params": {
                "startLedger": 2,
                "pagination": { "limit": 100 }
            }
        }))
        .await;

    assert_eq!(status, 200, "HTTP status should be 200 for JSON-RPC errors");
    assert!(resp.get("result").is_none(), "should not have a result");
    let error = &resp["error"];
    assert_eq!(
        error["code"].as_i64().unwrap(),
        -32002,
        "expected QUERY_BUDGET_EXCEEDED error code"
    );
    let msg = error["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("budget"),
        "error message should mention budget, got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// getEvents budget truncation and pagination tests
// ---------------------------------------------------------------------------

/// Boot a [`FakeRpcTestHarness`] with a custom `max_event_load_bytes` budget.
async fn setup_fake_rpc_with_event_budget(budget: usize) -> FakeRpcTestHarness {
    let app = FakeRpcApp::builder()
        .ledger_seq(10)
        .max_event_load_bytes(budget)
        .build();
    let h = FakeRpcTestHarness::start(app).await;
    seed_ledger_header(h.app.database(), 2, 1_700_000_002);
    seed_ledger_header(h.app.database(), 10, 1_700_000_010);
    h
}

/// Seed N events at a given ledger, each with a known-size XDR payload.
/// Returns the per-row stored byte count (event_xdr length + topic lengths).
fn seed_events_with_budget(h: &FakeRpcTestHarness, ledger_seq: u32, count: u32) -> usize {
    let valid_xdr = valid_event_xdr_b64();
    let topic_b64 = {
        let topic_val = ScVal::U32(99);
        let bytes = topic_val.to_xdr(Limits::none()).unwrap();
        BASE64.encode(&bytes)
    };
    // row_bytes = len(event_xdr) + len(topic1)
    let row_bytes = valid_xdr.len() + topic_b64.len();

    for i in 0..count {
        let event = EventRecord {
            id: format!("{:019}-{:010}", (ledger_seq as u64) << 32, i),
            ledger_seq,
            tx_index: i,
            op_index: 0,
            tx_hash: format!("tx_budget_{ledger_seq}_{i:04}"),
            contract_id: None,
            event_type: ContractEventType::Contract,
            topics: vec![topic_b64.clone()],
            event_xdr: valid_xdr.clone(),
            in_successful_contract_call: true,
        };
        h.app
            .database()
            .with_connection(|conn| conn.store_events(&[event]))
            .unwrap();
    }
    row_bytes
}

/// Verify getEvents respects byte budget and truncates results.
#[tokio::test]
async fn get_events_budget_truncation() {
    // First, discover per-row size
    let h = setup_fake_rpc_with_event_budget(1).await;
    let row_bytes = seed_events_with_budget(&h, 2, 5);

    // Recreate with budget that fits 2 rows but not 3
    let budget = row_bytes * 2 + row_bytes / 2;
    let h = setup_fake_rpc_with_event_budget(budget).await;
    let row_bytes2 = seed_events_with_budget(&h, 2, 5);
    assert_eq!(row_bytes, row_bytes2);

    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": "budget-events-trunc",
            "method": "getEvents",
            "params": {
                "startLedger": 2,
                "pagination": { "limit": 10 }
            }
        }))
        .await;

    assert_eq!(status, 200);
    let result = &resp["result"];
    let events = result["events"].as_array().expect("events array");
    assert_eq!(
        events.len(),
        2,
        "expected budget to truncate to 2 events, got {}",
        events.len()
    );
    let cursor = result["cursor"].as_str().expect("cursor");
    assert!(!cursor.is_empty());
}

/// Verify first event is always returned even with 1-byte budget.
#[tokio::test]
async fn get_events_budget_first_row_always_returned() {
    let h = setup_fake_rpc_with_event_budget(1).await;
    seed_events_with_budget(&h, 2, 3);

    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": "budget-events-first",
            "method": "getEvents",
            "params": {
                "startLedger": 2,
                "pagination": { "limit": 10 }
            }
        }))
        .await;

    assert_eq!(status, 200);
    let result = &resp["result"];
    let events = result["events"].as_array().expect("events array");
    assert_eq!(
        events.len(),
        1,
        "with 1-byte budget, first event must still be returned"
    );
    let cursor = result["cursor"].as_str().expect("cursor");
    assert!(
        !cursor.is_empty(),
        "cursor must be present for forward progress"
    );
}

/// Verify pagination resumes correctly after budget truncation.
#[tokio::test]
async fn get_events_budget_pagination() {
    let h = setup_fake_rpc_with_event_budget(1).await;
    let row_bytes = seed_events_with_budget(&h, 2, 3);

    // Budget fits exactly 1 row
    let h = setup_fake_rpc_with_event_budget(row_bytes).await;
    seed_events_with_budget(&h, 2, 3);

    // First page — should get 1 event
    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": "budget-events-page1",
            "method": "getEvents",
            "params": {
                "startLedger": 2,
                "pagination": { "limit": 10 }
            }
        }))
        .await;
    assert_eq!(status, 200);
    let result = &resp["result"];
    let events = result["events"].as_array().expect("events");
    assert_eq!(events.len(), 1, "page 1 should have 1 event");
    let cursor = result["cursor"].as_str().expect("cursor").to_string();

    // Second page — use the cursor
    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": "budget-events-page2",
            "method": "getEvents",
            "params": {
                "startLedger": 2,
                "pagination": { "limit": 10, "cursor": cursor }
            }
        }))
        .await;
    assert_eq!(status, 200);
    let result = &resp["result"];
    let events = result["events"].as_array().expect("events");
    assert_eq!(events.len(), 1, "page 2 should have 1 event");
    let cursor2 = result["cursor"].as_str().expect("cursor").to_string();
    assert_ne!(cursor, cursor2, "cursor should advance");

    // Third page
    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": "budget-events-page3",
            "method": "getEvents",
            "params": {
                "startLedger": 2,
                "pagination": { "limit": 10, "cursor": cursor2 }
            }
        }))
        .await;
    assert_eq!(status, 200);
    let result = &resp["result"];
    let events = result["events"].as_array().expect("events");
    assert_eq!(events.len(), 1, "page 3 should have 1 event");
}

// ---------------------------------------------------------------------------
// getEvents query-budget (max_event_query_ops) tests
// ---------------------------------------------------------------------------

/// Verify that exceeding max_event_query_ops returns JSON-RPC error -32002.
#[tokio::test]
async fn get_events_query_budget_exceeded_returns_error_code() {
    // Use an absurdly low budget so the query is interrupted during a scan.
    let app = FakeRpcApp::builder()
        .ledger_seq(10)
        .max_event_query_ops(100)
        .build();
    let h = FakeRpcTestHarness::start(app).await;
    seed_ledger_header(h.app.database(), 2, 1_700_000_002);
    seed_ledger_header(h.app.database(), 10, 1_700_000_010);

    // Seed many events so the query requires significant VM ops.
    let valid_xdr = valid_event_xdr_b64();
    for i in 0..500u32 {
        let event = EventRecord {
            id: format!("{:019}-{:010}", (2u64) << 32, i),
            ledger_seq: 2,
            tx_index: i,
            op_index: 0,
            tx_hash: format!("tx_budget_exceeded_{i:04}"),
            contract_id: None,
            event_type: ContractEventType::Contract,
            topics: vec![format!("topic-{}", i % 50)],
            event_xdr: valid_xdr.clone(),
            in_successful_contract_call: true,
        };
        h.app
            .database()
            .with_connection(|conn| conn.store_events(&[event]))
            .unwrap();
    }

    // Query without a topic filter — forces a full table scan that will exceed
    // the tight budget (the index only helps when topic1 is specified).
    let (status, resp) = h
        .post_rpc(json!({
            "jsonrpc": "2.0",
            "id": "budget-exceeded",
            "method": "getEvents",
            "params": {
                "startLedger": 2,
                "pagination": { "limit": 100 }
            }
        }))
        .await;

    assert_eq!(status, 200, "HTTP status should be 200 for JSON-RPC errors");
    assert!(resp.get("result").is_none(), "should not have a result");
    let error = &resp["error"];
    assert_eq!(
        error["code"].as_i64().unwrap(),
        -32002,
        "expected QUERY_BUDGET_EXCEEDED error code"
    );
    let message = error["message"].as_str().unwrap();
    assert!(
        message.contains("budget"),
        "error message should mention budget: {message}"
    );
}
