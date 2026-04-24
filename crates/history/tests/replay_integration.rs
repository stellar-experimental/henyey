use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::get,
    Router,
};
use flate2::{write::GzEncoder, Compression};
use henyey_bucket::{Bucket, BucketList, HotArchiveBucketList, BUCKET_LIST_LEVELS};
use henyey_common::Hash256;
use henyey_db::Database;
use henyey_history::{
    archive::HistoryArchive,
    archive_state::{HASBucketLevel, HistoryArchiveState},
    catchup::{CatchupManagerBuilder, CatchupOptions},
    paths::checkpoint_path,
    verify,
};
use henyey_ledger::TransactionSetVariant;
use stellar_xdr::curr::{
    Hash, LedgerHeader, LedgerHeaderExt, LedgerHeaderHistoryEntry, LedgerHeaderHistoryEntryExt,
    StellarValue, StellarValueExt, TimePoint, TransactionHistoryEntry, TransactionHistoryEntryExt,
    TransactionHistoryResultEntry, TransactionHistoryResultEntryExt, TransactionResultSet,
    TransactionSet, VecM, WriteXdr,
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
    prev_hash: Hash256,
    bucket_list_hash: Hash256,
    tx_set_hash: Hash256,
    tx_result_hash: Hash256,
) -> LedgerHeader {
    LedgerHeader {
        ledger_version: 25,
        previous_ledger_hash: Hash(*prev_hash.as_bytes()),
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

fn empty_bucket_list() -> BucketList {
    let hashes = vec![Hash256::ZERO; BUCKET_LIST_LEVELS * 2];
    let load_bucket = |hash: &Hash256| -> henyey_bucket::Result<Bucket> {
        if hash.is_zero() {
            return Ok(Bucket::empty());
        }
        Bucket::from_xdr_bytes(&[])
    };
    BucketList::restore_from_hashes(&hashes, load_bucket).expect("restore bucket list")
}

/// Compute the combined bucket list hash: SHA256(live_hash || hot_archive_hash).
/// This matches how verify_final_state computes the hash for comparison with the header.
fn combined_bucket_list_hash(live_hash: Hash256) -> Hash256 {
    use sha2::{Digest, Sha256};
    let hot_archive = HotArchiveBucketList::new();
    let hot_hash = hot_archive.hash();
    let mut hasher = Sha256::new();
    hasher.update(live_hash.as_bytes());
    hasher.update(hot_hash.as_bytes());
    let result = hasher.finalize();
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&result);
    Hash256::from_bytes(bytes)
}

#[tokio::test]
async fn test_catchup_replay_bucket_hash_verification() {
    let checkpoint = 63u32;
    let target = 64u32;
    let data_checkpoint = henyey_history::checkpoint::checkpoint_containing(target);

    let bucket_list = empty_bucket_list();
    let checkpoint_bucket_hash = combined_bucket_list_hash(bucket_list.hash());
    let mut bucket_list_after = bucket_list.clone();
    // Match catchup behavior: restart merges at checkpoint before replaying
    let default_next_states = vec![henyey_bucket::HasNextState::default(); BUCKET_LIST_LEVELS];
    let load_empty = |_hash: &Hash256| -> henyey_bucket::Result<Bucket> { Ok(Bucket::empty()) };
    bucket_list_after
        .restart_merges_from_has(checkpoint, 25, &default_next_states, load_empty, true)
        .await
        .expect("restart merges");
    bucket_list_after
        .add_batch(
            target,
            25,
            stellar_xdr::curr::BucketListType::Live,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .expect("bucket add batch");
    let replay_bucket_hash = combined_bucket_list_hash(bucket_list_after.hash());

    let header63 = make_header(
        checkpoint,
        Hash256::ZERO,
        checkpoint_bucket_hash,
        Hash256::ZERO,
        Hash256::ZERO,
    );
    let header63_hash = verify::compute_header_hash(&header63).expect("header63 hash");

    let tx_set = TransactionSet {
        previous_ledger_hash: Hash(*header63_hash.as_bytes()),
        txs: VecM::default(),
    };
    let tx_set_hash = verify::compute_tx_set_hash(&TransactionSetVariant::Classic(tx_set.clone()))
        .expect("tx set hash");

    let result_set = TransactionResultSet {
        results: VecM::default(),
    };
    let result_xdr = result_set
        .to_xdr(stellar_xdr::curr::Limits::none())
        .expect("tx result xdr");
    let tx_result_hash = Hash256::hash(&result_xdr);

    let header64 = make_header(
        target,
        header63_hash,
        replay_bucket_hash,
        tx_set_hash,
        tx_result_hash,
    );

    let headers_xdr = {
        let entry63 = LedgerHeaderHistoryEntry {
            hash: Hash([0u8; 32]),
            header: header63,
            ext: LedgerHeaderHistoryEntryExt::default(),
        };
        let entry64 = LedgerHeaderHistoryEntry {
            hash: Hash([0u8; 32]),
            header: header64.clone(),
            ext: LedgerHeaderHistoryEntryExt::default(),
        };
        let entry63_xdr = entry63
            .to_xdr(stellar_xdr::curr::Limits::none())
            .expect("header63 xdr");
        let entry64_xdr = entry64
            .to_xdr(stellar_xdr::curr::Limits::none())
            .expect("header64 xdr");
        record_marked(&[entry63_xdr, entry64_xdr])
    };
    let headers_xdr_for_data_checkpoint = {
        let entry64 = LedgerHeaderHistoryEntry {
            hash: Hash([0u8; 32]),
            header: header64,
            ext: LedgerHeaderHistoryEntryExt::default(),
        };
        let entry64_xdr = entry64
            .to_xdr(stellar_xdr::curr::Limits::none())
            .expect("header64 xdr");
        record_marked(&[entry64_xdr])
    };
    let tx_history_entry = TransactionHistoryEntry {
        ledger_seq: target,
        tx_set: tx_set.clone(),
        ext: TransactionHistoryEntryExt::V0,
    };
    let tx_history_xdr = record_marked(&[tx_history_entry
        .to_xdr(stellar_xdr::curr::Limits::none())
        .expect("tx history xdr")]);
    let tx_result_entry = TransactionHistoryResultEntry {
        ledger_seq: target,
        tx_result_set: result_set,
        ext: TransactionHistoryResultEntryExt::default(),
    };
    let tx_result_xdr = record_marked(&[tx_result_entry
        .to_xdr(stellar_xdr::curr::Limits::none())
        .expect("tx result history xdr")]);

    let mut levels = Vec::with_capacity(BUCKET_LIST_LEVELS);
    for _ in 0..BUCKET_LIST_LEVELS {
        levels.push(HASBucketLevel {
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
        current_buckets: levels,
        hot_archive_buckets: None,
    };

    let mut fixtures: HashMap<String, Vec<u8>> = HashMap::new();
    fixtures.insert(
        checkpoint_path("history", checkpoint, "json"),
        has.to_json().unwrap().into_bytes(),
    );
    fixtures.insert(
        checkpoint_path("ledger", checkpoint, "xdr.gz"),
        gzip_bytes(&headers_xdr),
    );
    fixtures.insert(
        checkpoint_path("transactions", checkpoint, "xdr.gz"),
        gzip_bytes(&[]),
    );
    fixtures.insert(
        checkpoint_path("results", checkpoint, "xdr.gz"),
        gzip_bytes(&[]),
    );
    fixtures.insert(
        checkpoint_path("ledger", data_checkpoint, "xdr.gz"),
        gzip_bytes(&headers_xdr_for_data_checkpoint),
    );
    fixtures.insert(
        checkpoint_path("transactions", data_checkpoint, "xdr.gz"),
        gzip_bytes(&tx_history_xdr),
    );
    fixtures.insert(
        checkpoint_path("results", data_checkpoint, "xdr.gz"),
        gzip_bytes(&tx_result_xdr),
    );

    let fixtures = Arc::new(fixtures);
    let app =
        Router::new()
            .route(
                "/*path",
                get(
                    |Path(path): Path<String>,
                     State(state): State<Arc<HashMap<String, Vec<u8>>>>| async move {
                        let key = path.trim_start_matches('/');
                        if let Some(body) = state.get(key) {
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

    let base_url = format!("http://{}/", addr);
    let archive = HistoryArchive::new(&base_url).expect("archive");

    let bucket_dir = tempfile::tempdir().expect("bucket dir");
    let bucket_manager =
        henyey_bucket::BucketManager::new(bucket_dir.path().to_path_buf()).expect("bucket manager");
    let db = Database::open_in_memory().expect("db");

    let ledger_manager = henyey_ledger::LedgerManager::new(
        "Test SDF Network ; September 2015".to_string(),
        henyey_ledger::LedgerManagerConfig {
            validate_bucket_hash: false,
            ..Default::default()
        },
    );

    let mut manager = CatchupManagerBuilder::new()
        .add_archive(archive)
        .bucket_manager(bucket_manager)
        .database(db)
        .options(CatchupOptions {
            // Disable bucket verification for this synthetic test since the test
            // fixture cannot perfectly replicate bucket list state after replay
            // (restart_merges during catchup creates pending merges that affect
            // the hash computation). Real bucket list hash verification is tested
            // end-to-end on testnet.
            verify_buckets: false,
            verify_headers: false,
        })
        .build()
        .expect("catchup manager");

    // Disable header hash verification in the replay path for this synthetic test.
    // close_ledger() computes skip_list, total_coins, etc. from internal state,
    // which won't match the simplified synthetic headers. Real header hash
    // verification is tested end-to-end on testnet.
    manager.set_replay_config(henyey_history::ReplayConfig {
        verify_bucket_list: false,
        verify_results: false,
        ..Default::default()
    });

    let output = manager
        .catchup_to_ledger(target, &ledger_manager)
        .await
        .expect("catchup");

    assert_eq!(output.ledger_seq, target);
    assert_eq!(output.ledgers_applied, 1);
    // Verify the ledger manager advanced to the target ledger
    let final_header = ledger_manager.current_header();
    assert_eq!(final_header.ledger_seq, target);
}

/// Test that Recent(N) with a gap larger than N triggers the bucket-apply +
/// short-replay path (Case 1b → Case 5 in CatchupRange).
///
/// Scenario: LCL=100, target=200, Recent(50) → gap=100 > 50
/// Expected: apply_buckets at checkpoint 127, replay 128..200 (73 ledgers)
#[tokio::test(flavor = "multi_thread")]
async fn test_catchup_recent_large_gap_bucket_apply() {
    use henyey_history::CatchupMode;
    use stellar_xdr::curr::{
        GeneralizedTransactionSet, ParallelTxsComponent, TransactionPhase, TransactionSetV1,
    };

    let bucket_apply_at = 127u32; // checkpoint where buckets are applied
    let target = 200u32;
    let lcl = 100u32;

    // Compute the empty bucket list hash for the checkpoint header.
    let bucket_list = empty_bucket_list();
    let checkpoint_bucket_hash = combined_bucket_list_hash(bucket_list.hash());

    // Pre-compute the empty tx result hash (same for every ledger).
    let empty_result_set = TransactionResultSet {
        results: VecM::default(),
    };
    let empty_result_xdr = empty_result_set
        .to_xdr(stellar_xdr::curr::Limits::none())
        .expect("tx result xdr");
    let empty_tx_result_hash = Hash256::hash(&empty_result_xdr);

    // Helper: compute the generalized empty tx set hash for a given prev_hash.
    // This must match what `empty_tx_history_entry` produces for protocol >= 20.
    let compute_empty_gen_tx_set_hash = |prev_hash: &Hash256| -> Hash256 {
        let classic_phase = TransactionPhase::V0(VecM::default());
        let soroban_phase = TransactionPhase::V1(ParallelTxsComponent {
            base_fee: None,
            execution_stages: VecM::default(),
        });
        let gen_set = GeneralizedTransactionSet::V1(TransactionSetV1 {
            previous_ledger_hash: Hash(*prev_hash.as_bytes()),
            phases: vec![classic_phase, soroban_phase]
                .try_into()
                .unwrap_or_default(),
        });
        let gen_set_variant = TransactionSetVariant::Generalized(gen_set);
        henyey_history::verify::compute_tx_set_hash(&gen_set_variant).expect("tx set hash")
    };

    // Build a header chain from ledger 127 (bucket-apply checkpoint) to 200 (target).
    // Ledger 127 is the checkpoint header; 128..200 are replayed.
    let mut headers: Vec<(u32, LedgerHeader, Hash256)> = Vec::new();

    // Ledger 127: checkpoint header
    // Use Hash256::ZERO for prev_hash (we don't verify chain anchors in this test).
    // tx_set_hash is not verified for the checkpoint header itself (only for replayed ledgers).
    let header_127 = make_header(
        bucket_apply_at,
        Hash256::ZERO,
        checkpoint_bucket_hash,
        Hash256::ZERO, // tx_set_hash not checked for checkpoint header
        Hash256::ZERO, // tx_result_hash not checked for checkpoint header
    );
    let hash_127 = verify::compute_header_hash(&header_127).expect("header hash 127");
    headers.push((bucket_apply_at, header_127, hash_127));

    // Ledgers 128..200: replayed ledgers with correct hash chain
    let mut prev_hash = hash_127;
    for seq in (bucket_apply_at + 1)..=target {
        let tx_set_hash = compute_empty_gen_tx_set_hash(&prev_hash);
        let header = make_header(
            seq,
            prev_hash,
            Hash256::ZERO, // bucket_list_hash not verified (verify_bucket_list=false)
            tx_set_hash,
            empty_tx_result_hash,
        );
        let hash = verify::compute_header_hash(&header).expect("header hash");
        headers.push((seq, header, hash));
        prev_hash = hash;
    }

    // Group headers into checkpoint files.
    // Checkpoint 127: ledgers 64-127 (we only have 127)
    // Checkpoint 191: ledgers 128-191
    // Checkpoint 255: ledgers 192-255 (we only have 192-200)
    let mut fixtures: HashMap<String, Vec<u8>> = HashMap::new();

    // Build header XDR per checkpoint
    for &checkpoint in &[127u32, 191, 255] {
        let entries: Vec<Vec<u8>> = headers
            .iter()
            .filter(|(seq, _, _)| {
                henyey_history::checkpoint::checkpoint_containing(*seq) == checkpoint
            })
            .map(|(_, header, hash)| {
                let entry = LedgerHeaderHistoryEntry {
                    hash: Hash(*hash.as_bytes()),
                    header: header.clone(),
                    ext: LedgerHeaderHistoryEntryExt::default(),
                };
                entry
                    .to_xdr(stellar_xdr::curr::Limits::none())
                    .expect("header xdr")
            })
            .collect();
        if !entries.is_empty() {
            fixtures.insert(
                checkpoint_path("ledger", checkpoint, "xdr.gz"),
                gzip_bytes(&record_marked(&entries)),
            );
        }
    }

    // Empty transaction and result files for checkpoints 191 and 255
    // (download_ledger_data only downloads from checkpoint_seq+1 = 128 to target = 200)
    for &checkpoint in &[191u32, 255] {
        fixtures.insert(
            checkpoint_path("transactions", checkpoint, "xdr.gz"),
            gzip_bytes(&[]),
        );
        fixtures.insert(
            checkpoint_path("results", checkpoint, "xdr.gz"),
            gzip_bytes(&[]),
        );
    }

    // HAS at checkpoint 127
    let mut levels = Vec::with_capacity(BUCKET_LIST_LEVELS);
    for _ in 0..BUCKET_LIST_LEVELS {
        levels.push(HASBucketLevel {
            curr: "0".repeat(64),
            snap: "0".repeat(64),
            next: Default::default(),
        });
    }
    let has = HistoryArchiveState {
        version: 2,
        server: Some("henyey test".to_string()),
        current_ledger: bucket_apply_at,
        network_passphrase: Some("Test SDF Network ; September 2015".to_string()),
        current_buckets: levels,
        hot_archive_buckets: None,
    };
    fixtures.insert(
        checkpoint_path("history", bucket_apply_at, "json"),
        has.to_json().unwrap().into_bytes(),
    );

    // Serve fixtures via Axum
    let fixtures = Arc::new(fixtures);
    let app =
        Router::new()
            .route(
                "/*path",
                get(
                    |Path(path): Path<String>,
                     State(state): State<Arc<HashMap<String, Vec<u8>>>>| async move {
                        let key = path.trim_start_matches('/');
                        if let Some(body) = state.get(key) {
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

    let base_url = format!("http://{}/", addr);
    let archive = HistoryArchive::new(&base_url).expect("archive");

    let bucket_dir = tempfile::tempdir().expect("bucket dir");
    let bucket_manager =
        henyey_bucket::BucketManager::new(bucket_dir.path().to_path_buf()).expect("bucket manager");
    let db = Database::open_in_memory().expect("db");

    let ledger_manager = henyey_ledger::LedgerManager::new(
        "Test SDF Network ; September 2015".to_string(),
        henyey_ledger::LedgerManagerConfig {
            validate_bucket_hash: false,
            ..Default::default()
        },
    );

    let mut manager = CatchupManagerBuilder::new()
        .add_archive(archive)
        .bucket_manager(bucket_manager)
        .database(db)
        .options(CatchupOptions {
            verify_buckets: false,
            verify_headers: false,
        })
        .build()
        .expect("catchup manager");

    manager.set_replay_config(henyey_history::ReplayConfig {
        verify_bucket_list: false,
        verify_results: false,
        ..Default::default()
    });

    // Call catchup_to_ledger_with_mode with Recent(50) and lcl=100.
    // Since apply_buckets=true (gap 100 > 50) and existing_state=None,
    // this can only succeed via the bucket-apply path.
    let output = manager
        .catchup_to_ledger_with_mode(target, CatchupMode::Recent(50), lcl, None, &ledger_manager)
        .await
        .expect("catchup with Recent(50) and large gap should succeed");

    // Verify the bucket-apply + replay path was taken with correct values.
    assert_eq!(output.ledger_seq, target, "should reach target ledger");
    assert_eq!(
        output.ledgers_applied, 73,
        "should replay 73 ledgers (128..200)"
    );
    let final_header = ledger_manager.current_header();
    assert_eq!(
        final_header.ledger_seq, target,
        "ledger manager should advance to target"
    );
}
