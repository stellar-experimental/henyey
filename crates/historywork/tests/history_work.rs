use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::get,
    Router,
};
use flate2::{write::GzEncoder, Compression};
use henyey_common::Hash256;
use henyey_history::verify;
use henyey_history::{
    archive::HistoryArchive,
    archive_state::{HASBucketLevel, HistoryArchiveState},
    paths::{bucket_path, checkpoint_path},
};
use henyey_historywork::{HistoryWorkBuilder, HistoryWorkState};
use henyey_ledger::TransactionSetVariant;
use henyey_work::{WorkScheduler, WorkSchedulerConfig};
use stellar_xdr::curr::{
    Hash, LedgerHeader, LedgerHeaderExt, LedgerHeaderHistoryEntry, LedgerHeaderHistoryEntryExt,
    LedgerScpMessages, ScpHistoryEntry, ScpHistoryEntryV0, StellarValue, StellarValueExt,
    TimePoint, TransactionHistoryEntry, TransactionHistoryEntryExt, TransactionHistoryResultEntry,
    TransactionHistoryResultEntryExt, TransactionResultSet, TransactionSet, VecM, WriteXdr,
};
use tokio::net::TcpListener;

fn gzip_bytes(data: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    use std::io::Write;
    encoder.write_all(data).expect("gzip write");
    encoder.finish().expect("gzip finish")
}

fn record_marked(entries: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for entry in entries {
        let len = u32::try_from(entry.len()).expect("entry too large");
        let record_mark = len | 0x8000_0000;
        out.extend_from_slice(&record_mark.to_be_bytes());
        out.extend_from_slice(entry);
    }
    out
}

fn make_header(
    ledger_seq: u32,
    bucket_list_hash: Hash256,
    tx_set_hash: Hash256,
    tx_result_hash: Hash256,
) -> LedgerHeader {
    LedgerHeader {
        ledger_version: 25,
        previous_ledger_hash: Hash([0u8; 32]),
        scp_value: StellarValue {
            tx_set_hash: Hash(*tx_set_hash.as_bytes()),
            close_time: TimePoint(0),
            upgrades: VecM::default(),
            ext: StellarValueExt::Basic,
        },
        tx_set_result_hash: Hash(*tx_result_hash.as_bytes()),
        bucket_list_hash: Hash(*bucket_list_hash.as_bytes()),
        ledger_seq,
        total_coins: 1_000_000,
        fee_pool: 0,
        inflation_seq: 0,
        id_pool: 0,
        base_fee: 100,
        base_reserve: 100,
        max_tx_set_size: 100,
        skip_list: [
            Hash([0u8; 32]),
            Hash([0u8; 32]),
            Hash([0u8; 32]),
            Hash([0u8; 32]),
        ],
        ext: LedgerHeaderExt::V0,
    }
}

#[tokio::test]
async fn test_history_work_chain() {
    let checkpoint = 63u32;
    let bucket_data: Vec<u8> = vec![0u8; 8]; // non-empty so it doesn't get filtered as empty bucket
    let bucket_hash = Hash256::hash(&bucket_data);

    let tx_set = TransactionSet {
        previous_ledger_hash: Hash([0u8; 32]),
        txs: VecM::default(),
    };
    let tx_set_hash = verify::compute_tx_set_hash(&TransactionSetVariant::Classic(tx_set.clone()))
        .expect("tx set hash");

    let tx_result_set = TransactionResultSet {
        results: VecM::default(),
    };
    let tx_result_xdr = tx_result_set
        .to_xdr(stellar_xdr::curr::Limits::none())
        .expect("tx result xdr");
    let tx_result_hash = Hash256::hash(&tx_result_xdr);

    let header = make_header(checkpoint, Hash256::ZERO, tx_set_hash, tx_result_hash);
    let header_hash = verify::compute_header_hash(&header).expect("header hash");
    let header_entry = LedgerHeaderHistoryEntry {
        hash: header_hash.into(),
        header,
        ext: LedgerHeaderHistoryEntryExt::default(),
    };
    let header_xdr_raw = header_entry
        .to_xdr(stellar_xdr::curr::Limits::none())
        .expect("xdr");
    let header_xdr = record_marked(&[header_xdr_raw]);

    // Build a structurally valid HAS: BUCKET_LIST_LEVELS (11) levels with
    // the test bucket at level 0 and empty (zero-hash) buckets elsewhere.
    // Stage E historywork now runs verify_has_structure +
    // verify_has_checkpoint inline before counting HAS-acquisition success,
    // so the fixture must satisfy both invariants.
    let mut current_buckets = vec![HASBucketLevel {
        curr: bucket_hash.to_hex(),
        snap: "0".repeat(64),
        next: Default::default(),
    }];
    for _ in 1..henyey_bucket::BUCKET_LIST_LEVELS {
        current_buckets.push(HASBucketLevel {
            curr: "0".repeat(64),
            snap: "0".repeat(64),
            next: Default::default(),
        });
    }
    let has = HistoryArchiveState {
        version: 2,
        server: Some("rs-stellar-core test".to_string()),
        current_ledger: checkpoint,
        network_passphrase: Some("Test SDF Network ; September 2015".to_string()),
        current_buckets,
        hot_archive_buckets: None,
    };

    let mut fixtures: HashMap<String, Vec<u8>> = HashMap::new();
    fixtures.insert(
        checkpoint_path("history", checkpoint, "json"),
        has.to_json().unwrap().into_bytes(),
    );
    fixtures.insert(
        checkpoint_path("ledger", checkpoint, "xdr.gz"),
        gzip_bytes(&header_xdr),
    );
    fixtures.insert(bucket_path(&bucket_hash), gzip_bytes(&bucket_data));

    let tx_entry = TransactionHistoryEntry {
        ledger_seq: checkpoint,
        tx_set,
        ext: TransactionHistoryEntryExt::default(),
    };
    let tx_entry_xdr_raw = tx_entry
        .to_xdr(stellar_xdr::curr::Limits::none())
        .expect("tx entry xdr");
    let tx_entry_xdr = record_marked(&[tx_entry_xdr_raw]);
    fixtures.insert(
        checkpoint_path("transactions", checkpoint, "xdr.gz"),
        gzip_bytes(&tx_entry_xdr),
    );

    let tx_result_entry = TransactionHistoryResultEntry {
        ledger_seq: checkpoint,
        tx_result_set,
        ext: TransactionHistoryResultEntryExt::default(),
    };
    let tx_result_entry_xdr_raw = tx_result_entry
        .to_xdr(stellar_xdr::curr::Limits::none())
        .expect("tx result entry xdr");
    let tx_result_entry_xdr = record_marked(&[tx_result_entry_xdr_raw]);
    fixtures.insert(
        checkpoint_path("results", checkpoint, "xdr.gz"),
        gzip_bytes(&tx_result_entry_xdr),
    );

    let scp_entry = ScpHistoryEntry::V0(ScpHistoryEntryV0 {
        quorum_sets: VecM::default(),
        ledger_messages: LedgerScpMessages {
            ledger_seq: checkpoint,
            messages: VecM::default(),
        },
    });
    let scp_entry_xdr_raw = scp_entry
        .to_xdr(stellar_xdr::curr::Limits::none())
        .expect("scp entry xdr");
    let scp_entry_xdr = record_marked(&[scp_entry_xdr_raw]);
    fixtures.insert(
        checkpoint_path("scp", checkpoint, "xdr.gz"),
        gzip_bytes(&scp_entry_xdr),
    );

    let fixtures = Arc::new(fixtures);
    let app =
        Router::new()
            .route(
                "/*path",
                get(
                    |Path(path): Path<String>,
                     State(state): State<Arc<HashMap<String, Vec<u8>>>>| async move {
                        if let Some(body) = state.get(&path) {
                            (StatusCode::OK, body.clone())
                        } else {
                            (StatusCode::NOT_FOUND, Vec::new())
                        }
                    },
                ),
            )
            .with_state(Arc::clone(&fixtures));

    let listener = match TcpListener::bind("127.0.0.1:0").await {
        Ok(listener) => listener,
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!("skipping test: tcp bind not permitted in this environment");
            return;
        }
        Err(err) => panic!("bind: {err}"),
    };
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    let archive = Arc::new(HistoryArchive::new(&format!("http://{}/", addr)).expect("archive"));
    let state = Arc::new(tokio::sync::Mutex::new(HistoryWorkState::default()));
    let bucket_download_dir = tempfile::tempdir().expect("bucket download dir");

    let mut scheduler = WorkScheduler::new(WorkSchedulerConfig {
        max_concurrency: 2,
        retry_delay: std::time::Duration::from_millis(10),
        event_tx: None,
    });
    let builder = HistoryWorkBuilder::new(
        archive,
        checkpoint,
        Arc::clone(&state),
        bucket_download_dir.path().to_path_buf(),
    );
    builder.register(&mut scheduler);

    scheduler.run_until_done().await;

    let guard = state.lock().await;
    assert!(guard.has.is_some());
    assert!(
        guard.bucket_dir.is_some(),
        "bucket_dir should be set after download"
    );
    assert_eq!(guard.headers.len(), 1);
    assert_eq!(guard.transactions.len(), 1);
    assert_eq!(guard.tx_results.len(), 1);
    assert_eq!(guard.scp_history.len(), 1);
    assert!(guard.progress.stage.is_some());
    assert!(!guard.progress.message.is_empty());
}
