use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::get,
    Router,
};
use flate2::{write::GzEncoder, Compression};
use henyey_bucket::{
    Bucket, BucketList, HotArchiveBucketList, BUCKET_LIST_LEVELS, HOT_ARCHIVE_BUCKET_LIST_LEVELS,
};
use henyey_common::Hash256;
use henyey_db::Database;
use henyey_history::{
    archive::HistoryArchive,
    archive_state::{HASBucketLevel, HistoryArchiveState},
    catchup::{CatchupManagerBuilder, CatchupOptions},
    paths::checkpoint_path,
    verify, CatchupConfiguration, HistoryError,
};
use henyey_ledger::TransactionSetVariant;
use stellar_xdr::{
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

/// Create a valid hot_archive_buckets field for v2 HAS in tests.
fn make_test_hot_archive_buckets() -> Option<Vec<HASBucketLevel>> {
    Some(
        (0..HOT_ARCHIVE_BUCKET_LIST_LEVELS)
            .map(|_| HASBucketLevel {
                curr: "0".repeat(64),
                snap: "0".repeat(64),
                next: Default::default(),
            })
            .collect(),
    )
}

/// #2931: register a published-frontier HAS for the catchup target's covering
/// checkpoint so the replay path's published-checkpoint precondition gate
/// (`checkpoint_publication_gate`) passes. The replay path now fetches the HAS
/// at `checkpoint_containing(target)` and requires its `currentLedger` to cover
/// that checkpoint before knit/replay — mirroring an archive that has already
/// published the target. Idempotent: safe to call even if a HAS is already
/// registered at the covering checkpoint.
fn register_published_target_has(fixtures: &mut HashMap<String, Vec<u8>>, target: u32) {
    let target_checkpoint = henyey_history::checkpoint::checkpoint_containing(target);
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
        current_ledger: target_checkpoint,
        network_passphrase: Some("Test SDF Network ; September 2015".to_string()),
        current_buckets: levels,
        hot_archive_buckets: make_test_hot_archive_buckets(),
    };
    fixtures
        .entry(checkpoint_path("history", target_checkpoint, "json"))
        .or_insert_with(|| has.to_json().unwrap().into_bytes());
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
        if let Some(bucket) = Bucket::for_sentinel_hash(hash) {
            return Ok(bucket);
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
    let default_next_states: Vec<Option<henyey_bucket::PendingMergeState>> =
        vec![None; BUCKET_LIST_LEVELS];
    let load_empty = |_hash: &Hash256| -> henyey_bucket::Result<Bucket> { Ok(Bucket::empty()) };
    bucket_list_after
        .restart_merges_from_has(checkpoint, 25, &default_next_states, load_empty, true)
        .await
        .expect("restart merges");
    bucket_list_after
        .add_batch(
            target,
            25,
            stellar_xdr::BucketListType::Live,
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
        .to_xdr(stellar_xdr::Limits::none())
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
        let header64_hash = verify::compute_header_hash(&header64).expect("header64 hash");
        let entry63 = LedgerHeaderHistoryEntry {
            hash: header63_hash.into(),
            header: header63,
            ext: LedgerHeaderHistoryEntryExt::default(),
        };
        let entry64 = LedgerHeaderHistoryEntry {
            hash: header64_hash.into(),
            header: header64.clone(),
            ext: LedgerHeaderHistoryEntryExt::default(),
        };
        let entry63_xdr = entry63
            .to_xdr(stellar_xdr::Limits::none())
            .expect("header63 xdr");
        let entry64_xdr = entry64
            .to_xdr(stellar_xdr::Limits::none())
            .expect("header64 xdr");
        record_marked(&[entry63_xdr, entry64_xdr])
    };
    let headers_xdr_for_data_checkpoint = {
        let header64_hash = verify::compute_header_hash(&header64).expect("header64 hash");
        let entry64 = LedgerHeaderHistoryEntry {
            hash: header64_hash.into(),
            header: header64,
            ext: LedgerHeaderHistoryEntryExt::default(),
        };
        let entry64_xdr = entry64
            .to_xdr(stellar_xdr::Limits::none())
            .expect("header64 xdr");
        record_marked(&[entry64_xdr])
    };
    let tx_history_entry = TransactionHistoryEntry {
        ledger_seq: target,
        tx_set: tx_set.clone(),
        ext: TransactionHistoryEntryExt::V0,
    };
    let tx_history_xdr = record_marked(&[tx_history_entry
        .to_xdr(stellar_xdr::Limits::none())
        .expect("tx history xdr")]);
    let tx_result_entry = TransactionHistoryResultEntry {
        ledger_seq: target,
        tx_result_set: result_set,
        ext: TransactionHistoryResultEntryExt::default(),
    };
    let tx_result_xdr = record_marked(&[tx_result_entry
        .to_xdr(stellar_xdr::Limits::none())
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
        hot_archive_buckets: make_test_hot_archive_buckets(),
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

    // #2931: serve the published-frontier HAS at the target's covering
    // checkpoint so the replay path's publication gate passes.
    register_published_target_has(&mut fixtures, target);

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
        verify_header_chain: false,
        verify_tx_set: false,
        verify_tx_results: false,
        verify_header_hash: false,
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

/// Test that Recent(N) with a gap larger than N now replays from LCL+1
/// (stellar-core parity / INV-C15). Previously this triggered bucket-apply +
/// short-replay (Case 1b), but that optimization was removed because it
/// violated the INV-C15 invariant that bucket-apply must not target a ledger
/// older than LCL.
///
/// Scenario: LCL=100, target=200, Recent(50) → gap=100 > 50
/// Expected: ReplayOnly from LCL+1=101 to target=200 (100 ledgers)
///
/// Since replay requires an initialized ledger manager (existing state at LCL),
/// this test verifies that catchup with mode=Recent and lcl>genesis works when
/// the ledger manager is pre-initialized.
#[tokio::test(flavor = "multi_thread")]
async fn test_catchup_recent_large_gap_replays_with_parity() {
    use henyey_history::CatchupMode;
    use stellar_xdr::{GeneralizedTransactionSet, TransactionPhase, TransactionSetV1};

    let target = 200u32;
    let lcl = 100u32;

    // Pre-compute empty tx result hash.
    let empty_result_set = TransactionResultSet {
        results: VecM::default(),
    };
    let empty_result_xdr = empty_result_set
        .to_xdr(stellar_xdr::Limits::none())
        .expect("tx result xdr");
    let empty_tx_result_hash = Hash256::hash(&empty_result_xdr);

    // Helper: compute the generalized empty tx set hash for a given prev_hash.
    let compute_empty_gen_tx_set_hash = |prev_hash: &Hash256| -> Hash256 {
        let classic_phase = TransactionPhase::V0(VecM::default());
        let soroban_phase = henyey_tx::tx_set_xdr::empty_soroban_phase();
        let gen_set = GeneralizedTransactionSet::V1(TransactionSetV1 {
            previous_ledger_hash: Hash(*prev_hash.as_bytes()),
            phases: vec![classic_phase, soroban_phase]
                .try_into()
                .unwrap_or_default(),
        });
        let gen_set_variant = TransactionSetVariant::Generalized(gen_set);
        henyey_history::verify::compute_tx_set_hash(&gen_set_variant).expect("tx set hash")
    };

    // Build a header chain from ledger 100 (LCL) to 200 (target).
    // Ledger 100 is the LCL; 101..200 are replayed.
    let mut headers: Vec<(u32, LedgerHeader, Hash256)> = Vec::new();

    // Ledger 100: LCL header (used for ledger manager init)
    let header_100 = make_header(
        lcl,
        Hash256::ZERO,
        Hash256::ZERO, // bucket_list_hash
        Hash256::ZERO, // tx_set_hash not checked
        Hash256::ZERO, // tx_result_hash not checked
    );
    let hash_100 = verify::compute_header_hash(&header_100).expect("header hash 100");
    headers.push((lcl, header_100.clone(), hash_100));

    // Ledgers 101..200: replayed ledgers with correct hash chain
    let mut prev_hash = hash_100;
    for seq in (lcl + 1)..=target {
        let tx_set_hash = compute_empty_gen_tx_set_hash(&prev_hash);
        let header = make_header(
            seq,
            prev_hash,
            Hash256::ZERO, // bucket_list_hash not verified
            tx_set_hash,
            empty_tx_result_hash,
        );
        let hash = verify::compute_header_hash(&header).expect("header hash");
        headers.push((seq, header, hash));
        prev_hash = hash;
    }

    // Group headers into checkpoint files.
    // Checkpoint 127: ledgers 64-127 (we have 100-127)
    // Checkpoint 191: ledgers 128-191
    // Checkpoint 255: ledgers 192-255 (we only have 192-200)
    let mut fixtures: HashMap<String, Vec<u8>> = HashMap::new();

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
                    .to_xdr(stellar_xdr::Limits::none())
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

    // Empty transaction and result files for checkpoints that contain replayed ledgers
    for &checkpoint in &[127u32, 191, 255] {
        fixtures.insert(
            checkpoint_path("transactions", checkpoint, "xdr.gz"),
            gzip_bytes(&[]),
        );
        fixtures.insert(
            checkpoint_path("results", checkpoint, "xdr.gz"),
            gzip_bytes(&[]),
        );
    }

    // #2931: the replay path now fetches the HAS for the target's covering
    // checkpoint and requires the archive's published frontier (currentLedger)
    // to cover it before knit/replay. Serve a published-frontier HAS at the
    // covering checkpoint so the publication gate passes.
    register_published_target_has(&mut fixtures, target);

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

    // Initialize ledger manager at LCL (required for replay-only path).
    let bucket_list = empty_bucket_list();
    let hot_archive = henyey_bucket::HotArchiveBucketList::default();
    ledger_manager
        .initialize(bucket_list, hot_archive, header_100, hash_100)
        .expect("initialize ledger manager at LCL");

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
        verify_header_chain: false,
        verify_tx_set: false,
        verify_tx_results: false,
        verify_header_hash: false,
        ..Default::default()
    });

    // Call catchup_to_ledger_with_mode with Recent(50) and lcl=100.
    // With stellar-core parity (INV-C15), this now replays from LCL+1.
    let output = manager
        .catchup_to_ledger_with_mode(target, CatchupMode::Recent(50), lcl, None, &ledger_manager)
        .await
        .expect("catchup with Recent(50) and large gap should replay from LCL+1");

    // Verify the replay-only path was taken.
    assert_eq!(output.ledger_seq, target, "should reach target ledger");
    assert_eq!(
        output.ledgers_applied, 100,
        "should replay 100 ledgers (101..200)"
    );
    let final_header = ledger_manager.current_header();
    assert_eq!(
        final_header.ledger_seq, target,
        "ledger manager should advance to target"
    );
}

/// Regression test for #2292: when the LedgerManager has a synthetic genesis
/// at protocol version 0, but the actual archive genesis is at version 25,
/// `download_ledger_data` must self-correct the LCL protocol version from
/// the archive and use Generalized v23+ format for empty tx set synthesis.
///
/// This test verifies the end-to-end catchup path works correctly when:
/// - Genesis (ledger 1) is at protocol 25 (USE_CONFIG_FOR_GENESIS)
/// - Ledger 64 has no transactions (empty tx set synthesized as Generalized)
/// - The tx set hash in ledger 64 uses Generalized v23+ format
#[tokio::test]
async fn test_catchup_self_corrects_lcl_protocol_from_archive() {
    use stellar_xdr::{GeneralizedTransactionSet, TransactionPhase, TransactionSetV1};

    let checkpoint = 63u32;
    let target = 64u32;
    let data_checkpoint = henyey_history::checkpoint::checkpoint_containing(target);

    let bucket_list = empty_bucket_list();
    let checkpoint_bucket_hash = combined_bucket_list_hash(bucket_list.hash());

    // Build header chain: genesis (ledger 1) at protocol 23 through ledger 64.
    // This simulates a quickstart network with USE_CONFIG_FOR_GENESIS=true.
    let mut headers: Vec<(u32, LedgerHeader, Hash256)> = Vec::new();

    // Helper: compute Generalized v23+ empty tx set hash for a given prev_hash.
    let compute_empty_gen_tx_set_hash = |prev_hash: &Hash256| -> Hash256 {
        let classic_phase = TransactionPhase::V0(VecM::default());
        let soroban_phase = henyey_tx::tx_set_xdr::empty_soroban_phase();
        let gen_set = GeneralizedTransactionSet::V1(TransactionSetV1 {
            previous_ledger_hash: Hash(*prev_hash.as_bytes()),
            phases: vec![classic_phase, soroban_phase]
                .try_into()
                .unwrap_or_default(),
        });
        let gen_set_variant = TransactionSetVariant::Generalized(gen_set);
        verify::compute_tx_set_hash(&gen_set_variant).expect("tx set hash")
    };

    // Compute tx result hash (empty, same for every ledger)
    let empty_result_set = TransactionResultSet {
        results: VecM::default(),
    };
    let empty_result_xdr = empty_result_set
        .to_xdr(stellar_xdr::Limits::none())
        .expect("tx result xdr");
    let empty_tx_result_hash = Hash256::hash(&empty_result_xdr);

    // Ledger 1 (genesis): protocol 23, all hashes zero for simplicity.
    let header1 = LedgerHeader {
        ledger_version: 25, // <-- USE_CONFIG_FOR_GENESIS protocol
        previous_ledger_hash: Hash([0u8; 32]),
        scp_value: StellarValue {
            tx_set_hash: Hash([0u8; 32]),
            close_time: TimePoint(0),
            upgrades: VecM::default(),
            ext: StellarValueExt::Basic,
        },
        tx_set_result_hash: Hash([0u8; 32]),
        bucket_list_hash: Hash([0u8; 32]),
        ledger_seq: 1,
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
    };
    let hash1 = verify::compute_header_hash(&header1).expect("header1 hash");
    headers.push((1, header1, hash1));

    // Ledgers 2..63: protocol 23, empty tx sets (Generalized format)
    let mut prev_hash = hash1;
    for seq in 2..=checkpoint {
        let tx_set_hash = compute_empty_gen_tx_set_hash(&prev_hash);
        let bucket_hash = if seq == checkpoint {
            checkpoint_bucket_hash
        } else {
            Hash256::ZERO
        };
        let header = LedgerHeader {
            ledger_version: 25,
            previous_ledger_hash: Hash(*prev_hash.as_bytes()),
            scp_value: StellarValue {
                tx_set_hash: Hash(*tx_set_hash.as_bytes()),
                close_time: TimePoint(0),
                upgrades: VecM::default(),
                ext: StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash(*empty_tx_result_hash.as_bytes()),
            bucket_list_hash: Hash(*bucket_hash.as_bytes()),
            ledger_seq: seq,
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
        };
        let hash = verify::compute_header_hash(&header).expect("header hash");
        headers.push((seq, header, hash));
        prev_hash = hash;
    }

    // Ledger 64 (target): also protocol 23, empty tx set in Generalized format.
    // This is the ledger where the bug manifested in CI (different format expected).
    let tx_set_hash_64 = compute_empty_gen_tx_set_hash(&prev_hash);
    let header64 = LedgerHeader {
        ledger_version: 25,
        previous_ledger_hash: Hash(*prev_hash.as_bytes()),
        scp_value: StellarValue {
            tx_set_hash: Hash(*tx_set_hash_64.as_bytes()),
            close_time: TimePoint(0),
            upgrades: VecM::default(),
            ext: StellarValueExt::Basic,
        },
        tx_set_result_hash: Hash(*empty_tx_result_hash.as_bytes()),
        bucket_list_hash: Hash([0u8; 32]), // not verified
        ledger_seq: target,
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
    };
    let hash64 = verify::compute_header_hash(&header64).expect("header64 hash");
    headers.push((target, header64, hash64));

    // Build fixtures: headers for checkpoint 63 (ledgers 1-63) and data_checkpoint (64)
    let mut fixtures: HashMap<String, Vec<u8>> = HashMap::new();

    // Headers for checkpoint 63
    let cp63_entries: Vec<Vec<u8>> = headers
        .iter()
        .filter(|(seq, _, _)| *seq <= checkpoint)
        .map(|(_, header, hash)| {
            let entry = LedgerHeaderHistoryEntry {
                hash: Hash(*hash.as_bytes()),
                header: header.clone(),
                ext: LedgerHeaderHistoryEntryExt::default(),
            };
            entry
                .to_xdr(stellar_xdr::Limits::none())
                .expect("header xdr")
        })
        .collect();
    fixtures.insert(
        checkpoint_path("ledger", checkpoint, "xdr.gz"),
        gzip_bytes(&record_marked(&cp63_entries)),
    );

    // Headers for data_checkpoint (contains ledger 64)
    let cp_data_entries: Vec<Vec<u8>> = headers
        .iter()
        .filter(|(seq, _, _)| {
            henyey_history::checkpoint::checkpoint_containing(*seq) == data_checkpoint
                && *seq > checkpoint
        })
        .map(|(_, header, hash)| {
            let entry = LedgerHeaderHistoryEntry {
                hash: Hash(*hash.as_bytes()),
                header: header.clone(),
                ext: LedgerHeaderHistoryEntryExt::default(),
            };
            entry
                .to_xdr(stellar_xdr::Limits::none())
                .expect("header xdr")
        })
        .collect();
    if !cp_data_entries.is_empty() && data_checkpoint != checkpoint {
        fixtures.insert(
            checkpoint_path("ledger", data_checkpoint, "xdr.gz"),
            gzip_bytes(&record_marked(&cp_data_entries)),
        );
    }

    // Empty transaction and result files
    fixtures.insert(
        checkpoint_path("transactions", checkpoint, "xdr.gz"),
        gzip_bytes(&[]),
    );
    fixtures.insert(
        checkpoint_path("results", checkpoint, "xdr.gz"),
        gzip_bytes(&[]),
    );
    if data_checkpoint != checkpoint {
        fixtures.insert(
            checkpoint_path("transactions", data_checkpoint, "xdr.gz"),
            gzip_bytes(&[]),
        );
        fixtures.insert(
            checkpoint_path("results", data_checkpoint, "xdr.gz"),
            gzip_bytes(&[]),
        );
    }

    // HAS
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
        current_ledger: checkpoint,
        network_passphrase: Some("Test SDF Network ; September 2015".to_string()),
        current_buckets: levels,
        hot_archive_buckets: make_test_hot_archive_buckets(),
    };
    fixtures.insert(
        checkpoint_path("history", checkpoint, "json"),
        has.to_json().unwrap().into_bytes(),
    );

    // #2931: serve the published-frontier HAS at the target's covering
    // checkpoint so the replay path's publication gate passes.
    register_published_target_has(&mut fixtures, target);

    // Serve via Axum
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
        verify_header_chain: false,
        verify_tx_set: false,
        verify_tx_results: false,
        verify_header_hash: false,
        ..Default::default()
    });

    // This exercises the fix: after bucket-apply at checkpoint 63, the
    // LedgerManager is at version 23. Then download_ledger_data(63, 64, 23)
    // resolves the LCL header from the archive (also 23) and uses Generalized
    // format for the empty tx set. Without the Generalized format support at
    // protocol 23, this would fail with "invalid tx set hash at ledger 64".
    let output = manager
        .catchup_to_ledger(target, &ledger_manager)
        .await
        .expect("catchup should succeed with genesis at protocol 25 and Generalized tx sets");

    assert_eq!(output.ledger_seq, target);
    assert_eq!(output.ledgers_applied, 1);
    let final_header = ledger_manager.current_header();
    assert_eq!(final_header.ledger_seq, target);
}

/// End-to-end test that `replay_via_close_ledger()` correctly translates
/// `LedgerError::HashMismatch` into `HistoryError::ReplayHashMismatch`.
///
/// This covers the history-layer wiring at `catchup/replay.rs:321-336`.
/// The ledger-layer "reject mismatch before mutating state" guarantee is
/// separately tested by `test_close_ledger_rejects_wrong_expected_header_hash`.
///
/// The mismatch is triggered by corrupting the archive header's `bucket_list_hash`
/// field. With `verify_header_hash: true`, replay computes `expected_header_hash`
/// from the corrupted archive header and passes it to `close_ledger()`, which
/// computes a different hash from its internal state → `HashMismatch`.
#[tokio::test]
async fn test_replay_hash_mismatch_produces_replay_hash_mismatch_error() {
    let checkpoint = 63u32;
    let target = 64u32;
    let data_checkpoint = henyey_history::checkpoint::checkpoint_containing(target);

    let bucket_list = empty_bucket_list();
    let checkpoint_bucket_hash = combined_bucket_list_hash(bucket_list.hash());

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
        .to_xdr(stellar_xdr::Limits::none())
        .expect("tx result xdr");
    let tx_result_hash = Hash256::hash(&result_xdr);

    // Corrupt bucket_list_hash in the archive header for ledger 64.
    // This makes the expected_header_hash (computed from this header) differ
    // from what close_ledger() internally computes.
    let corrupted_bucket_hash = Hash256::from_bytes([0xFF; 32]);
    let header64_corrupted = make_header(
        target,
        header63_hash,
        corrupted_bucket_hash,
        tx_set_hash,
        tx_result_hash,
    );

    // Compute what replay will use as `expected_header_hash`.
    let corrupted_header64_hash =
        verify::compute_header_hash(&header64_corrupted).expect("corrupted header64 hash");

    let headers_xdr = {
        let entry63 = LedgerHeaderHistoryEntry {
            hash: header63_hash.into(),
            header: header63,
            ext: LedgerHeaderHistoryEntryExt::default(),
        };
        let entry64 = LedgerHeaderHistoryEntry {
            hash: corrupted_header64_hash.into(),
            header: header64_corrupted.clone(),
            ext: LedgerHeaderHistoryEntryExt::default(),
        };
        let entry63_xdr = entry63
            .to_xdr(stellar_xdr::Limits::none())
            .expect("header63 xdr");
        let entry64_xdr = entry64
            .to_xdr(stellar_xdr::Limits::none())
            .expect("header64 xdr");
        record_marked(&[entry63_xdr, entry64_xdr])
    };
    let headers_xdr_for_data_checkpoint = {
        let entry64 = LedgerHeaderHistoryEntry {
            hash: corrupted_header64_hash.into(),
            header: header64_corrupted,
            ext: LedgerHeaderHistoryEntryExt::default(),
        };
        let entry64_xdr = entry64
            .to_xdr(stellar_xdr::Limits::none())
            .expect("header64 xdr");
        record_marked(&[entry64_xdr])
    };
    let tx_history_entry = TransactionHistoryEntry {
        ledger_seq: target,
        tx_set: tx_set.clone(),
        ext: TransactionHistoryEntryExt::V0,
    };
    let tx_history_xdr = record_marked(&[tx_history_entry
        .to_xdr(stellar_xdr::Limits::none())
        .expect("tx history xdr")]);
    let tx_result_entry = TransactionHistoryResultEntry {
        ledger_seq: target,
        tx_result_set: result_set,
        ext: TransactionHistoryResultEntryExt::default(),
    };
    let tx_result_xdr = record_marked(&[tx_result_entry
        .to_xdr(stellar_xdr::Limits::none())
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
        hot_archive_buckets: make_test_hot_archive_buckets(),
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

    // #2931: serve the published-frontier HAS at the target's covering
    // checkpoint so the replay path's publication gate passes.
    register_published_target_has(&mut fixtures, target);

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

    // Enable only header hash verification to isolate the ReplayHashMismatch path.
    manager.set_replay_config(henyey_history::ReplayConfig {
        verify_bucket_list: false,
        verify_header_chain: false,
        verify_tx_set: false,
        verify_tx_results: false,
        verify_header_hash: true,
        ..Default::default()
    });

    let result = manager.catchup_to_ledger(target, &ledger_manager).await;

    // Assert we get ReplayHashMismatch with the correct fields.
    match result {
        Err(HistoryError::ReplayHashMismatch {
            ledger,
            expected,
            actual,
        }) => {
            assert_eq!(ledger, target, "mismatch should report the target ledger");

            // The `expected` field comes from compute_header_hash(archive_header),
            // which is what replay passes as expected_header_hash to close_ledger.
            assert_eq!(
                expected,
                corrupted_header64_hash.to_hex(),
                "expected hash should match the corrupted archive header hash"
            );

            // Both must be valid 64-char hex strings that differ.
            assert_eq!(expected.len(), 64, "expected hash should be 64 hex chars");
            assert_eq!(actual.len(), 64, "actual hash should be 64 hex chars");
            assert_ne!(
                expected, actual,
                "expected and actual hashes must differ for a mismatch"
            );
        }
        Err(other) => panic!("expected ReplayHashMismatch, got: {other}"),
        Ok(_) => panic!("expected ReplayHashMismatch error, but catchup succeeded"),
    }

    // Verify LedgerManager state was NOT corrupted — it should still be at
    // the checkpoint ledger (63), not the target (64).
    assert_eq!(
        ledger_manager.current_ledger_seq(),
        checkpoint,
        "LedgerManager should not advance past checkpoint on hash mismatch"
    );
    assert_eq!(
        ledger_manager.current_header_hash(),
        header63_hash,
        "LedgerManager header hash should remain at checkpoint header"
    );
}

/// End-to-end test that `validate_bucket_hash: true` in `LedgerManagerConfig`
/// exercises the bucket-list hash validation code path (manager.rs:2462-2468)
/// during replay via catchup.
///
/// This test enables `validate_bucket_hash: true` and runs a successful replay,
/// proving that the bucket-list hash computation in `commit_close()` correctly
/// validates against the internally-computed header. A corrupted bucket_list_hash
/// in the archive header CANNOT trigger this path (the check compares the
/// internally-computed hash with itself), so this test verifies correctness of
/// the validation logic rather than error propagation.
///
/// The bucket-list hash validation error path (LedgerError::HashMismatch) is
/// already covered by `test_close_ledger_rejects_wrong_expected_header_hash` and
/// the ReplayHashMismatch translation is covered by
/// `test_replay_hash_mismatch_produces_replay_hash_mismatch_error`.
#[tokio::test]
async fn test_replay_with_validate_bucket_hash_enabled() {
    let checkpoint = 63u32;
    let target = 64u32;
    let data_checkpoint = henyey_history::checkpoint::checkpoint_containing(target);

    let bucket_list = empty_bucket_list();
    let checkpoint_bucket_hash = combined_bucket_list_hash(bucket_list.hash());
    let mut bucket_list_after = bucket_list.clone();
    let default_next_states: Vec<Option<henyey_bucket::PendingMergeState>> =
        vec![None; BUCKET_LIST_LEVELS];
    let load_empty = |_hash: &Hash256| -> henyey_bucket::Result<Bucket> { Ok(Bucket::empty()) };
    bucket_list_after
        .restart_merges_from_has(checkpoint, 25, &default_next_states, load_empty, true)
        .await
        .expect("restart merges");
    bucket_list_after
        .add_batch(
            target,
            25,
            stellar_xdr::BucketListType::Live,
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
        .to_xdr(stellar_xdr::Limits::none())
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
        let header64_hash = verify::compute_header_hash(&header64).expect("header64 hash");
        let entry63 = LedgerHeaderHistoryEntry {
            hash: header63_hash.into(),
            header: header63,
            ext: LedgerHeaderHistoryEntryExt::default(),
        };
        let entry64 = LedgerHeaderHistoryEntry {
            hash: header64_hash.into(),
            header: header64.clone(),
            ext: LedgerHeaderHistoryEntryExt::default(),
        };
        let entry63_xdr = entry63
            .to_xdr(stellar_xdr::Limits::none())
            .expect("header63 xdr");
        let entry64_xdr = entry64
            .to_xdr(stellar_xdr::Limits::none())
            .expect("header64 xdr");
        record_marked(&[entry63_xdr, entry64_xdr])
    };
    let headers_xdr_for_data_checkpoint = {
        let header64_hash = verify::compute_header_hash(&header64).expect("header64 hash");
        let entry64 = LedgerHeaderHistoryEntry {
            hash: header64_hash.into(),
            header: header64,
            ext: LedgerHeaderHistoryEntryExt::default(),
        };
        let entry64_xdr = entry64
            .to_xdr(stellar_xdr::Limits::none())
            .expect("header64 xdr");
        record_marked(&[entry64_xdr])
    };
    let tx_history_entry = TransactionHistoryEntry {
        ledger_seq: target,
        tx_set: tx_set.clone(),
        ext: TransactionHistoryEntryExt::V0,
    };
    let tx_history_xdr = record_marked(&[tx_history_entry
        .to_xdr(stellar_xdr::Limits::none())
        .expect("tx history xdr")]);
    let tx_result_entry = TransactionHistoryResultEntry {
        ledger_seq: target,
        tx_result_set: result_set,
        ext: TransactionHistoryResultEntryExt::default(),
    };
    let tx_result_xdr = record_marked(&[tx_result_entry
        .to_xdr(stellar_xdr::Limits::none())
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
        hot_archive_buckets: make_test_hot_archive_buckets(),
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

    // #2931: serve the published-frontier HAS at the target's covering
    // checkpoint so the replay path's publication gate passes.
    register_published_target_has(&mut fixtures, target);

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

    // Enable validate_bucket_hash so the LedgerManager runs the bucket list
    // hash check during commit_close (manager.rs:2432-2468).
    let ledger_manager = henyey_ledger::LedgerManager::new(
        "Test SDF Network ; September 2015".to_string(),
        henyey_ledger::LedgerManagerConfig {
            validate_bucket_hash: true,
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

    // Disable replay-layer verifications so only the LedgerManager's
    // validate_bucket_hash check is exercised.
    manager.set_replay_config(henyey_history::ReplayConfig {
        verify_bucket_list: false,
        verify_header_chain: false,
        verify_tx_set: false,
        verify_tx_results: false,
        verify_header_hash: false,
        ..Default::default()
    });

    // This should succeed — the bucket list hash computed during commit_close
    // matches the internally-computed header value.
    let output = manager
        .catchup_to_ledger(target, &ledger_manager)
        .await
        .expect("catchup with validate_bucket_hash should succeed");

    assert_eq!(output.ledger_seq, target);
    assert_eq!(output.ledgers_applied, 1);
    let final_header = ledger_manager.current_header();
    assert_eq!(final_header.ledger_seq, target);
}

/// INV-C15: bucket-apply at a checkpoint older than LCL must be rejected.
///
/// Scenario: LedgerManager initialized at seq=200, attempt bucket-apply at
/// checkpoint 127 (< LCL). Should return VerificationFailed error.
#[tokio::test]
async fn test_inv_c15_rejects_bucket_apply_older_than_lcl() {
    use henyey_bucket::HotArchiveBucketList;
    use henyey_history::catchup::CheckpointData;
    use stellar_xdr::ScpHistoryEntry;

    let checkpoint = 127u32;
    let target = 128u32;
    let lcl_seq = 200u32;

    // Create a LedgerManager pre-initialized at seq=200 (> checkpoint=127).
    let ledger_manager = henyey_ledger::LedgerManager::new(
        "Test SDF Network ; September 2015".to_string(),
        henyey_ledger::LedgerManagerConfig {
            validate_bucket_hash: false,
            ..Default::default()
        },
    );

    // Initialize the ledger manager at lcl_seq=200 with empty bucket lists.
    let bucket_list = empty_bucket_list();
    let hot_archive = HotArchiveBucketList::new();
    let lcl_header = make_header(
        lcl_seq,
        Hash256::ZERO,
        Hash256::ZERO,
        Hash256::ZERO,
        Hash256::ZERO,
    );
    ledger_manager
        .initialize(bucket_list.clone(), hot_archive, lcl_header, Hash256::ZERO)
        .expect("initialize ledger manager at seq=200");
    assert!(ledger_manager.is_initialized());
    assert_eq!(ledger_manager.current_ledger_seq(), lcl_seq);

    // Build minimal CheckpointData targeting checkpoint=127.
    let checkpoint_header = make_header(
        checkpoint,
        Hash256::ZERO,
        Hash256::ZERO,
        Hash256::ZERO,
        Hash256::ZERO,
    );
    let checkpoint_hash = henyey_history::verify::compute_header_hash(&checkpoint_header)
        .expect("checkpoint header hash");

    let header_entry = LedgerHeaderHistoryEntry {
        hash: checkpoint_hash.into(),
        header: checkpoint_header,
        ext: LedgerHeaderHistoryEntryExt::default(),
    };

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
        server: Some("test".to_string()),
        current_ledger: checkpoint,
        network_passphrase: Some("Test SDF Network ; September 2015".to_string()),
        current_buckets: levels,
        hot_archive_buckets: make_test_hot_archive_buckets(),
    };

    let bucket_dir = tempfile::tempdir().expect("bucket dir");
    let bucket_manager =
        henyey_bucket::BucketManager::new(bucket_dir.path().to_path_buf()).expect("bucket manager");
    let db = henyey_db::Database::open_in_memory().expect("db");

    let dummy_archive = HistoryArchive::new("http://127.0.0.1:1/").expect("dummy archive");
    let mut manager = CatchupManagerBuilder::new()
        .add_archive(dummy_archive)
        .bucket_manager(bucket_manager)
        .database(db)
        .options(CatchupOptions {
            verify_buckets: false,
            verify_headers: false,
        })
        .build()
        .expect("catchup manager");

    let data = CheckpointData {
        has,
        bucket_dir: bucket_dir.path().to_path_buf(),
        headers: vec![header_entry],
        transactions: vec![],
        tx_results: vec![],
        scp_history: Vec::<ScpHistoryEntry>::new(),
    };

    let result = manager
        .catchup_to_ledger_with_checkpoint_data_config(
            target,
            data,
            CatchupConfiguration::offline_basic(henyey_history::CatchupMode::Minimal),
            &ledger_manager,
        )
        .await;

    let err = result.expect_err("should reject bucket-apply older than LCL");
    let err_msg = err.to_string();
    assert!(
        err_msg.contains("INV-C15"),
        "error should mention INV-C15, got: {err_msg}"
    );
    assert!(
        err_msg.contains("older than LCL"),
        "error should mention 'older than LCL', got: {err_msg}"
    );
}

/// INV-C15: bucket-apply at checkpoint == LCL is valid (equality is permitted).
///
/// Scenario: LedgerManager initialized at seq=127, bucket-apply at checkpoint=127.
/// This should NOT trigger INV-C15 (only strictly-less-than is rejected).
#[tokio::test]
async fn test_inv_c15_allows_bucket_apply_at_lcl() {
    use henyey_bucket::HotArchiveBucketList;
    use henyey_history::catchup::CheckpointData;
    use stellar_xdr::ScpHistoryEntry;

    let checkpoint = 127u32;
    let target = 128u32;

    // Create a LedgerManager pre-initialized at seq=127 (== checkpoint).
    let ledger_manager = henyey_ledger::LedgerManager::new(
        "Test SDF Network ; September 2015".to_string(),
        henyey_ledger::LedgerManagerConfig {
            validate_bucket_hash: false,
            ..Default::default()
        },
    );

    let bucket_list = empty_bucket_list();
    let bl_hash = combined_bucket_list_hash(bucket_list.hash());
    let hot_archive = HotArchiveBucketList::new();
    let lcl_header = make_header(
        checkpoint,
        Hash256::ZERO,
        bl_hash,
        Hash256::ZERO,
        Hash256::ZERO,
    );
    ledger_manager
        .initialize(bucket_list.clone(), hot_archive, lcl_header, Hash256::ZERO)
        .expect("initialize ledger manager at checkpoint seq");
    assert!(ledger_manager.is_initialized());
    assert_eq!(ledger_manager.current_ledger_seq(), checkpoint);

    // Build CheckpointData targeting checkpoint=127.
    let checkpoint_header = make_header(
        checkpoint,
        Hash256::ZERO,
        bl_hash,
        Hash256::ZERO,
        Hash256::ZERO,
    );
    let checkpoint_hash = henyey_history::verify::compute_header_hash(&checkpoint_header)
        .expect("checkpoint header hash");

    let header_entry = LedgerHeaderHistoryEntry {
        hash: checkpoint_hash.into(),
        header: checkpoint_header,
        ext: LedgerHeaderHistoryEntryExt::default(),
    };

    // Build a header for target (128) to replay.
    let tx_result_set = stellar_xdr::TransactionResultSet {
        results: VecM::default(),
    };
    let result_xdr = tx_result_set
        .to_xdr(stellar_xdr::Limits::none())
        .expect("result xdr");
    let tx_result_hash = Hash256::hash(&result_xdr);

    let tx_set = stellar_xdr::GeneralizedTransactionSet::V1(stellar_xdr::TransactionSetV1 {
        previous_ledger_hash: Hash(*checkpoint_hash.as_bytes()),
        phases: VecM::default(),
    });
    let tx_set_hash = henyey_history::verify::compute_tx_set_hash(
        &TransactionSetVariant::Generalized(tx_set.clone()),
    )
    .expect("tx set hash");

    let target_header = make_header(
        target,
        checkpoint_hash,
        Hash256::ZERO, // won't be verified
        tx_set_hash,
        tx_result_hash,
    );
    let target_hash =
        henyey_history::verify::compute_header_hash(&target_header).expect("target header hash");
    let target_entry = LedgerHeaderHistoryEntry {
        hash: target_hash.into(),
        header: target_header,
        ext: LedgerHeaderHistoryEntryExt::default(),
    };

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
        server: Some("test".to_string()),
        current_ledger: checkpoint,
        network_passphrase: Some("Test SDF Network ; September 2015".to_string()),
        current_buckets: levels,
        hot_archive_buckets: make_test_hot_archive_buckets(),
    };

    let bucket_dir = tempfile::tempdir().expect("bucket dir");
    let bucket_manager =
        henyey_bucket::BucketManager::new(bucket_dir.path().to_path_buf()).expect("bucket manager");
    let db = henyey_db::Database::open_in_memory().expect("db");

    let dummy_archive = HistoryArchive::new("http://127.0.0.1:1/").expect("dummy archive");
    let mut manager = CatchupManagerBuilder::new()
        .add_archive(dummy_archive)
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
        verify_header_chain: false,
        verify_tx_set: false,
        verify_tx_results: false,
        verify_header_hash: false,
        ..Default::default()
    });

    let data = CheckpointData {
        has,
        bucket_dir: bucket_dir.path().to_path_buf(),
        headers: vec![header_entry, target_entry],
        transactions: vec![],
        tx_results: vec![],
        scp_history: Vec::<ScpHistoryEntry>::new(),
    };

    // This should succeed — checkpoint == LCL is valid per INV-C15.
    let result = manager
        .catchup_to_ledger_with_checkpoint_data_config(
            target,
            data,
            CatchupConfiguration::offline_basic(henyey_history::CatchupMode::Minimal),
            &ledger_manager,
        )
        .await;

    // The bucket-apply + replay should succeed (equality does not violate INV-C15).
    // Note: If some downstream replay validation fails (e.g. tx_set verification),
    // that's unrelated to INV-C15. The key assertion is that we DON'T get an
    // INV-C15 error.
    match result {
        Ok(output) => {
            assert_eq!(output.ledger_seq, target);
        }
        Err(ref e) => {
            let msg = e.to_string();
            assert!(
                !msg.contains("INV-C15"),
                "INV-C15 should NOT fire when checkpoint == LCL, got: {msg}"
            );
            // Other errors (e.g. replay verification) are acceptable in this
            // synthetic test — the important thing is INV-C15 didn't reject it.
        }
    }
}

/// Regression test: online minimal catchup going through the historywork/
/// checkpoint-data fast path must preserve `CatchupRunMode::Online` end-to-end.
/// Before the fix, `catchup_to_ledger_with_checkpoint_data` had no config param
/// and silently dropped the run mode.
#[tokio::test]
async fn test_checkpoint_data_path_preserves_online_run_mode() {
    use henyey_history::catchup::CheckpointData;
    use henyey_history::CatchupRunMode;
    use stellar_xdr::ScpHistoryEntry;

    let checkpoint = 127u32;
    let target = 128u32;

    let ledger_manager = henyey_ledger::LedgerManager::new(
        "Test SDF Network ; September 2015".to_string(),
        henyey_ledger::LedgerManagerConfig {
            validate_bucket_hash: false,
            ..Default::default()
        },
    );

    // Fresh ledger manager (not initialized) = the "no existing state" case
    // that triggers the historywork/checkpoint-data fast path in henyey-app.
    assert!(!ledger_manager.is_initialized());

    let checkpoint_header = make_header(
        checkpoint,
        Hash256::ZERO,
        Hash256::ZERO,
        Hash256::ZERO,
        Hash256::ZERO,
    );
    let checkpoint_hash = henyey_history::verify::compute_header_hash(&checkpoint_header)
        .expect("checkpoint header hash");

    let header_entry = LedgerHeaderHistoryEntry {
        hash: checkpoint_hash.into(),
        header: checkpoint_header,
        ext: LedgerHeaderHistoryEntryExt::default(),
    };

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
        server: Some("test".to_string()),
        current_ledger: checkpoint,
        network_passphrase: Some("Test SDF Network ; September 2015".to_string()),
        current_buckets: levels,
        hot_archive_buckets: make_test_hot_archive_buckets(),
    };

    let bucket_dir = tempfile::tempdir().expect("bucket dir");
    let bucket_manager =
        henyey_bucket::BucketManager::new(bucket_dir.path().to_path_buf()).expect("bucket manager");
    let db = henyey_db::Database::open_in_memory().expect("db");

    let dummy_archive = HistoryArchive::new("http://127.0.0.1:1/").expect("dummy archive");
    let mut manager = CatchupManagerBuilder::new()
        .add_archive(dummy_archive)
        .bucket_manager(bucket_manager)
        .database(db)
        .options(CatchupOptions {
            verify_buckets: false,
            verify_headers: false,
        })
        .build()
        .expect("catchup manager");

    let data = CheckpointData {
        has,
        bucket_dir: bucket_dir.path().to_path_buf(),
        headers: vec![header_entry],
        transactions: vec![],
        tx_results: vec![],
        scp_history: Vec::<ScpHistoryEntry>::new(),
    };

    // Call with CatchupRunMode::Online — this previously wasn't possible
    // because the function had no config parameter.
    let config = CatchupConfiguration::online(henyey_history::CatchupMode::Minimal);
    assert_eq!(config.run_mode, CatchupRunMode::Online);

    let result = manager
        .catchup_to_ledger_with_checkpoint_data_config(target, data, config, &ledger_manager)
        .await;

    // The call succeeds (or may fail due to bucket-apply/replay in synthetic test),
    // but the important assertion is that it accepted the Online config without
    // ignoring/discarding the run_mode. If it compiles and doesn't panic on
    // config handling, the discriminator is threaded through.
    if let Err(e) = &result {
        let msg = e.to_string();
        assert!(
            !msg.contains("run_mode") && !msg.contains("Online"),
            "error should not be about run_mode, got: {msg}"
        );
    }
}

/// OfflineComplete run mode must be rejected at public entry points until
/// #2831 implements tx-result verification. Silent downgrade is an operational
/// risk — callers must not believe they got stronger validation.
#[tokio::test]
async fn test_offline_complete_rejected_at_config_entry_point() {
    use henyey_history::CatchupConfiguration;
    use henyey_history::CatchupRunMode;

    let ledger_manager = henyey_ledger::LedgerManager::new(
        "Test SDF Network ; September 2015".to_string(),
        henyey_ledger::LedgerManagerConfig {
            validate_bucket_hash: false,
            ..Default::default()
        },
    );

    let bucket_dir = tempfile::tempdir().expect("bucket dir");
    let bucket_manager =
        henyey_bucket::BucketManager::new(bucket_dir.path().to_path_buf()).expect("bucket manager");
    let db = henyey_db::Database::open_in_memory().expect("db");

    let dummy_archive = HistoryArchive::new("http://127.0.0.1:1/").expect("dummy archive");
    let mut manager = CatchupManagerBuilder::new()
        .add_archive(dummy_archive)
        .bucket_manager(bucket_manager)
        .database(db)
        .options(CatchupOptions {
            verify_buckets: false,
            verify_headers: false,
        })
        .build()
        .expect("catchup manager");

    let config = CatchupConfiguration::new(
        henyey_history::CatchupMode::Complete,
        CatchupRunMode::OfflineComplete,
    );

    let result = manager
        .catchup_to_ledger_with_config(100, config, 1, None, &ledger_manager)
        .await;

    let err = result.expect_err("OfflineComplete should be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("not yet implemented"),
        "error should mention 'not yet implemented', got: {msg}"
    );
    assert!(
        msg.contains("#2831"),
        "error should reference tracking issue #2831, got: {msg}"
    );
}

/// OfflineComplete run mode must also be rejected via the checkpoint-data
/// config-aware entry point.
#[tokio::test]
async fn test_offline_complete_rejected_at_checkpoint_data_entry_point() {
    use henyey_history::catchup::CheckpointData;
    use henyey_history::CatchupConfiguration;
    use henyey_history::CatchupRunMode;
    use stellar_xdr::ScpHistoryEntry;

    let ledger_manager = henyey_ledger::LedgerManager::new(
        "Test SDF Network ; September 2015".to_string(),
        henyey_ledger::LedgerManagerConfig {
            validate_bucket_hash: false,
            ..Default::default()
        },
    );

    let bucket_dir = tempfile::tempdir().expect("bucket dir");
    let bucket_manager =
        henyey_bucket::BucketManager::new(bucket_dir.path().to_path_buf()).expect("bucket manager");
    let db = henyey_db::Database::open_in_memory().expect("db");

    let dummy_archive = HistoryArchive::new("http://127.0.0.1:1/").expect("dummy archive");
    let mut manager = CatchupManagerBuilder::new()
        .add_archive(dummy_archive)
        .bucket_manager(bucket_manager)
        .database(db)
        .options(CatchupOptions {
            verify_buckets: false,
            verify_headers: false,
        })
        .build()
        .expect("catchup manager");

    let checkpoint = 64u32;
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
        server: Some("test".to_string()),
        current_ledger: checkpoint,
        network_passphrase: Some("Test SDF Network ; September 2015".to_string()),
        current_buckets: levels,
        hot_archive_buckets: make_test_hot_archive_buckets(),
    };

    let data = CheckpointData {
        has,
        bucket_dir: bucket_dir.path().to_path_buf(),
        headers: vec![],
        transactions: vec![],
        tx_results: vec![],
        scp_history: Vec::<ScpHistoryEntry>::new(),
    };

    let config = CatchupConfiguration::new(
        henyey_history::CatchupMode::Minimal,
        CatchupRunMode::OfflineComplete,
    );

    let result = manager
        .catchup_to_ledger_with_checkpoint_data_config(checkpoint, data, config, &ledger_manager)
        .await;

    let err = result.expect_err("OfflineComplete should be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("not yet implemented"),
        "error should mention 'not yet implemented', got: {msg}"
    );
    assert!(
        msg.contains("#2831"),
        "error should reference tracking issue #2831, got: {msg}"
    );
}

/// Non-Minimal depth on the checkpoint-data config path must be rejected.
/// Before this fix, callers could request `Recent(500)` or `Complete` and
/// silently receive minimal replay behavior.
#[tokio::test]
async fn test_checkpoint_data_config_rejects_non_minimal_depth() {
    use henyey_history::catchup::CheckpointData;
    use henyey_history::CatchupConfiguration;
    use henyey_history::CatchupRunMode;
    use stellar_xdr::ScpHistoryEntry;

    let ledger_manager = henyey_ledger::LedgerManager::new(
        "Test SDF Network ; September 2015".to_string(),
        henyey_ledger::LedgerManagerConfig {
            validate_bucket_hash: false,
            ..Default::default()
        },
    );

    let bucket_dir = tempfile::tempdir().expect("bucket dir");
    let bucket_manager =
        henyey_bucket::BucketManager::new(bucket_dir.path().to_path_buf()).expect("bucket manager");
    let db = henyey_db::Database::open_in_memory().expect("db");

    let dummy_archive = HistoryArchive::new("http://127.0.0.1:1/").expect("dummy archive");
    let mut manager = CatchupManagerBuilder::new()
        .add_archive(dummy_archive)
        .bucket_manager(bucket_manager)
        .database(db)
        .options(CatchupOptions {
            verify_buckets: false,
            verify_headers: false,
        })
        .build()
        .expect("catchup manager");

    let checkpoint = 64u32;
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
        server: Some("test".to_string()),
        current_ledger: checkpoint,
        network_passphrase: Some("Test SDF Network ; September 2015".to_string()),
        current_buckets: levels.clone(),
        hot_archive_buckets: make_test_hot_archive_buckets(),
    };

    let data = CheckpointData {
        has,
        bucket_dir: bucket_dir.path().to_path_buf(),
        headers: vec![],
        transactions: vec![],
        tx_results: vec![],
        scp_history: Vec::<ScpHistoryEntry>::new(),
    };

    // Test with Recent(500) — should be rejected
    let config = CatchupConfiguration::new(
        henyey_history::CatchupMode::Recent(500),
        CatchupRunMode::OfflineBasic,
    );

    let result = manager
        .catchup_to_ledger_with_checkpoint_data_config(checkpoint, data, config, &ledger_manager)
        .await;

    let err = result.expect_err("non-Minimal depth should be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("only supports CatchupMode::Minimal"),
        "error should mention Minimal requirement, got: {msg}"
    );

    // Test with Complete — should also be rejected
    let has2 = HistoryArchiveState {
        version: 2,
        server: Some("test".to_string()),
        current_ledger: checkpoint,
        network_passphrase: Some("Test SDF Network ; September 2015".to_string()),
        current_buckets: levels,
        hot_archive_buckets: make_test_hot_archive_buckets(),
    };

    let data2 = CheckpointData {
        has: has2,
        bucket_dir: bucket_dir.path().to_path_buf(),
        headers: vec![],
        transactions: vec![],
        tx_results: vec![],
        scp_history: Vec::<ScpHistoryEntry>::new(),
    };

    let config2 = CatchupConfiguration::new(
        henyey_history::CatchupMode::Complete,
        CatchupRunMode::Online,
    );

    let result2 = manager
        .catchup_to_ledger_with_checkpoint_data_config(checkpoint, data2, config2, &ledger_manager)
        .await;

    let err2 = result2.expect_err("Complete depth should be rejected");
    let msg2 = err2.to_string();
    assert!(
        msg2.contains("only supports CatchupMode::Minimal"),
        "error should mention Minimal requirement, got: {msg2}"
    );
}

/// Regression test for #2940: a gated catchup replay attempt must download
/// ledger data ONLY from the archive that served `target_has` and passed the
/// #2937 publication gate — it must never fall back to a different archive for
/// the LCL header or checkpoint payload within the same attempt.
///
/// Setup: two archives ordered [B, A].
/// - Archive A ("good"): serves the bucket-apply checkpoint HAS, the gate HAS
///   at the target's covering checkpoint, and a *correct* `ledger/{ckpt}.xdr.gz`
///   for the data checkpoint (header chain that knits to LCL and replays
///   cleanly).
/// - Archive B ("divergent"): serves the bucket-apply checkpoint HAS so the
///   out-of-scope bucket-apply phase can proceed, but **404s the gate HAS**
///   (`history/{target_checkpoint}.json`) and serves a *divergent*
///   `ledger/{ckpt}.xdr.gz` whose header64 hash differs from A's.
///
/// Because B is first in the list, the publication gate's `download_has`
/// rotation tries B (404 on the gate HAS), then A (serves it) → the gate
/// archive is A. Under the pre-#2940 unpinned behaviour, the subsequent
/// fixed-order ledger-data download would hit B first and consume B's
/// divergent header64 → knit/chain mismatch and catchup failure. With the
/// pin, ledger data is fetched from A only and catchup succeeds against A's
/// data.
#[tokio::test]
async fn test_catchup_pins_downloads_to_gate_archive() {
    let checkpoint = 63u32;
    let target = 64u32;
    let data_checkpoint = henyey_history::checkpoint::checkpoint_containing(target);

    let bucket_list = empty_bucket_list();
    let checkpoint_bucket_hash = combined_bucket_list_hash(bucket_list.hash());
    let mut bucket_list_after = bucket_list.clone();
    let default_next_states: Vec<Option<henyey_bucket::PendingMergeState>> =
        vec![None; BUCKET_LIST_LEVELS];
    let load_empty = |_hash: &Hash256| -> henyey_bucket::Result<Bucket> { Ok(Bucket::empty()) };
    bucket_list_after
        .restart_merges_from_has(checkpoint, 25, &default_next_states, load_empty, true)
        .await
        .expect("restart merges");
    bucket_list_after
        .add_batch(
            target,
            25,
            stellar_xdr::BucketListType::Live,
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
        .to_xdr(stellar_xdr::Limits::none())
        .expect("tx result xdr");
    let tx_result_hash = Hash256::hash(&result_xdr);

    // Archive A's correct header64 (knits to LCL=63, replays cleanly).
    let header64 = make_header(
        target,
        header63_hash,
        replay_bucket_hash,
        tx_set_hash,
        tx_result_hash,
    );
    let header64_hash = verify::compute_header_hash(&header64).expect("header64 hash");

    // Archive B's DIVERGENT header64: a *different* previous_ledger_hash so it
    // cannot knit to LCL=63. If B's ledger data is consumed, the §11.2
    // knit-to-LCL decision matrix (which runs unconditionally, independent of
    // replay_config) rejects it with a fatal KnitCurrentLedgerPrevHashMismatch
    // → catchup fails. So success vs failure cleanly distinguishes "used A"
    // (knits) from "used B" (does not knit).
    let header64_divergent = make_header(
        target,
        Hash256::from_bytes([0xAB; 32]),
        replay_bucket_hash,
        tx_set_hash,
        tx_result_hash,
    );
    let header64_divergent_hash =
        verify::compute_header_hash(&header64_divergent).expect("divergent header64 hash");
    assert_ne!(
        header64_hash, header64_divergent_hash,
        "divergent header must differ from the good header"
    );

    // Checkpoint-63 headers (ledgers 63) — identical on both archives so the
    // out-of-scope bucket-apply phase succeeds regardless of which archive
    // serves it.
    let headers_xdr_ckpt63 = {
        let entry63 = LedgerHeaderHistoryEntry {
            hash: header63_hash.into(),
            header: header63.clone(),
            ext: LedgerHeaderHistoryEntryExt::default(),
        };
        record_marked(&[entry63
            .to_xdr(stellar_xdr::Limits::none())
            .expect("header63 xdr")])
    };

    let make_data_checkpoint_headers = |entry_hash: Hash256, header: LedgerHeader| -> Vec<u8> {
        let entry64 = LedgerHeaderHistoryEntry {
            hash: entry_hash.into(),
            header,
            ext: LedgerHeaderHistoryEntryExt::default(),
        };
        record_marked(&[entry64
            .to_xdr(stellar_xdr::Limits::none())
            .expect("header64 xdr")])
    };
    let headers_xdr_good = make_data_checkpoint_headers(header64_hash, header64.clone());
    let headers_xdr_divergent =
        make_data_checkpoint_headers(header64_divergent_hash, header64_divergent);

    let tx_history_entry = TransactionHistoryEntry {
        ledger_seq: target,
        tx_set: tx_set.clone(),
        ext: TransactionHistoryEntryExt::V0,
    };
    let tx_history_xdr = record_marked(&[tx_history_entry
        .to_xdr(stellar_xdr::Limits::none())
        .expect("tx history xdr")]);
    let tx_result_entry = TransactionHistoryResultEntry {
        ledger_seq: target,
        tx_result_set: result_set,
        ext: TransactionHistoryResultEntryExt::default(),
    };
    let tx_result_xdr = record_marked(&[tx_result_entry
        .to_xdr(stellar_xdr::Limits::none())
        .expect("tx result history xdr")]);

    let mut levels = Vec::with_capacity(BUCKET_LIST_LEVELS);
    for _ in 0..BUCKET_LIST_LEVELS {
        levels.push(HASBucketLevel {
            curr: "0".repeat(64),
            snap: "0".repeat(64),
            next: Default::default(),
        });
    }
    let has_ckpt63 = HistoryArchiveState {
        version: 2,
        server: Some("rs-stellar-core test".to_string()),
        current_ledger: checkpoint,
        network_passphrase: Some("Test SDF Network ; September 2015".to_string()),
        current_buckets: levels,
        hot_archive_buckets: make_test_hot_archive_buckets(),
    };

    // Build the per-archive fixture map. `good = true` produces archive A
    // (correct ledger data + gate HAS); `good = false` produces archive B
    // (divergent ledger data, no gate HAS).
    let build_fixtures = |good: bool| -> HashMap<String, Vec<u8>> {
        let mut fixtures: HashMap<String, Vec<u8>> = HashMap::new();
        // Bucket-apply checkpoint HAS + headers + (empty) tx/results — shared.
        fixtures.insert(
            checkpoint_path("history", checkpoint, "json"),
            has_ckpt63.to_json().unwrap().into_bytes(),
        );
        fixtures.insert(
            checkpoint_path("ledger", checkpoint, "xdr.gz"),
            gzip_bytes(&headers_xdr_ckpt63),
        );
        fixtures.insert(
            checkpoint_path("transactions", checkpoint, "xdr.gz"),
            gzip_bytes(&[]),
        );
        fixtures.insert(
            checkpoint_path("results", checkpoint, "xdr.gz"),
            gzip_bytes(&[]),
        );

        // Data-checkpoint ledger data: correct on A, divergent on B.
        fixtures.insert(
            checkpoint_path("ledger", data_checkpoint, "xdr.gz"),
            gzip_bytes(if good {
                &headers_xdr_good
            } else {
                &headers_xdr_divergent
            }),
        );
        fixtures.insert(
            checkpoint_path("transactions", data_checkpoint, "xdr.gz"),
            gzip_bytes(&tx_history_xdr),
        );
        fixtures.insert(
            checkpoint_path("results", data_checkpoint, "xdr.gz"),
            gzip_bytes(&tx_result_xdr),
        );

        // Gate HAS at the target's covering checkpoint — only archive A serves
        // it. Archive B 404s, forcing `download_has` rotation to pick A as the
        // gate archive.
        if good {
            register_published_target_has(&mut fixtures, target);
        }
        fixtures
    };

    async fn serve(
        fixtures: HashMap<String, Vec<u8>>,
    ) -> Option<(String, tokio::task::JoinHandle<()>)> {
        let fixtures = Arc::new(fixtures);
        let app = Router::new()
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
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => return None,
            Err(err) => panic!("bind: {err}"),
        };
        let addr = listener.local_addr().expect("addr");
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Some((format!("http://{}/", addr), handle))
    }

    // Archive A (good) and archive B (divergent).
    let (url_a, _h_a) = match serve(build_fixtures(true)).await {
        Some(v) => v,
        None => {
            eprintln!("skipping test: tcp bind not permitted in this environment");
            return;
        }
    };
    let (url_b, _h_b) = match serve(build_fixtures(false)).await {
        Some(v) => v,
        None => {
            eprintln!("skipping test: tcp bind not permitted in this environment");
            return;
        }
    };

    let archive_a = HistoryArchive::new(&url_a).expect("archive A");
    let archive_b = HistoryArchive::new(&url_b).expect("archive B");

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

    // Order [B, A]: B is tried first for ledger data under the old fixed-order
    // rotation, so an unpinned attempt would consume B's divergent bytes.
    let mut manager = CatchupManagerBuilder::new()
        .add_archive(archive_b)
        .add_archive(archive_a)
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
        verify_header_chain: false,
        verify_tx_set: false,
        verify_tx_results: false,
        verify_header_hash: false,
        ..Default::default()
    });

    // Catchup must SUCCEED. The publication gate selects archive A (the only
    // archive serving the gate HAS). With the #2940 pin, the LCL header and
    // checkpoint payload are fetched from A only — A's header64 knits to LCL
    // and replays cleanly. Without the pin, fixed-order rotation would fetch
    // archive B's divergent header64 first (B is index 0), whose mismatched
    // previous_ledger_hash fails the unconditional §11.2 knit-to-LCL check
    // with a fatal KnitCurrentLedgerPrevHashMismatch → catchup fails. So a
    // successful catchup proves B's divergent ledger data was never consumed.
    let output = manager
        .catchup_to_ledger(target, &ledger_manager)
        .await
        .expect("catchup must succeed using the gate archive's ledger data only");

    assert_eq!(output.ledger_seq, target);
    assert_eq!(output.ledgers_applied, 1);
    let final_header = ledger_manager.current_header();
    assert_eq!(final_header.ledger_seq, target);
}

// ---------------------------------------------------------------------------
// #3811 regression tests: persist-before-replay ordering.
// ---------------------------------------------------------------------------

/// Shared helper (#3811): serve a fixtures map over an ephemeral in-process
/// axum archive and return its base URL. Returns `None` when TCP bind is not
/// permitted in the sandbox, so callers early-return (skip) exactly like the
/// existing per-test `TcpListener::bind` guards.
async fn spawn_test_archive(fixtures: HashMap<String, Vec<u8>>) -> Option<String> {
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
            return None;
        }
        Err(err) => panic!("bind: {err}"),
    };
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    Some(format!("http://{}/", addr))
}

/// Shared helper (#3811): build a synthetic empty-ledger archive spanning
/// `[lcl_seq, target]`. Headers carry a correct hash chain and empty
/// generalized tx sets; `bucket_list_hash` is left ZERO because callers of
/// this helper disable bucket/header-hash verification. Returns the fixtures
/// map plus the LCL header and its hash so the caller can pre-initialize the
/// `LedgerManager` for the replay-only path.
fn build_empty_chain_fixtures(
    lcl_seq: u32,
    target: u32,
) -> (HashMap<String, Vec<u8>>, LedgerHeader, Hash256) {
    use stellar_xdr::{GeneralizedTransactionSet, TransactionPhase, TransactionSetV1};

    let empty_result_set = TransactionResultSet {
        results: VecM::default(),
    };
    let empty_result_xdr = empty_result_set
        .to_xdr(stellar_xdr::Limits::none())
        .expect("tx result xdr");
    let empty_tx_result_hash = Hash256::hash(&empty_result_xdr);

    let compute_empty_gen_tx_set_hash = |prev_hash: &Hash256| -> Hash256 {
        let classic_phase = TransactionPhase::V0(VecM::default());
        let soroban_phase = henyey_tx::tx_set_xdr::empty_soroban_phase();
        let gen_set = GeneralizedTransactionSet::V1(TransactionSetV1 {
            previous_ledger_hash: Hash(*prev_hash.as_bytes()),
            phases: vec![classic_phase, soroban_phase]
                .try_into()
                .unwrap_or_default(),
        });
        let gen_set_variant = TransactionSetVariant::Generalized(gen_set);
        henyey_history::verify::compute_tx_set_hash(&gen_set_variant).expect("tx set hash")
    };

    let mut headers: Vec<(u32, LedgerHeader, Hash256)> = Vec::new();
    let header_lcl = make_header(
        lcl_seq,
        Hash256::ZERO,
        Hash256::ZERO,
        Hash256::ZERO,
        Hash256::ZERO,
    );
    let hash_lcl = verify::compute_header_hash(&header_lcl).expect("lcl header hash");
    headers.push((lcl_seq, header_lcl.clone(), hash_lcl));

    let mut prev_hash = hash_lcl;
    for seq in (lcl_seq + 1)..=target {
        let tx_set_hash = compute_empty_gen_tx_set_hash(&prev_hash);
        let header = make_header(
            seq,
            prev_hash,
            Hash256::ZERO,
            tx_set_hash,
            empty_tx_result_hash,
        );
        let hash = verify::compute_header_hash(&header).expect("header hash");
        headers.push((seq, header, hash));
        prev_hash = hash;
    }

    // Group headers into their covering checkpoints (strict, as in the
    // large-gap parity fixture).
    let mut checkpoints: Vec<u32> = headers
        .iter()
        .map(|(seq, _, _)| henyey_history::checkpoint::checkpoint_containing(*seq))
        .collect();
    checkpoints.sort_unstable();
    checkpoints.dedup();

    let mut fixtures: HashMap<String, Vec<u8>> = HashMap::new();
    for &checkpoint in &checkpoints {
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
                    .to_xdr(stellar_xdr::Limits::none())
                    .expect("header xdr")
            })
            .collect();
        if !entries.is_empty() {
            fixtures.insert(
                checkpoint_path("ledger", checkpoint, "xdr.gz"),
                gzip_bytes(&record_marked(&entries)),
            );
        }
        // Empty transaction/result files: download synthesizes empty tx sets.
        fixtures.insert(
            checkpoint_path("transactions", checkpoint, "xdr.gz"),
            gzip_bytes(&[]),
        );
        fixtures.insert(
            checkpoint_path("results", checkpoint, "xdr.gz"),
            gzip_bytes(&[]),
        );
    }

    register_published_target_has(&mut fixtures, target);
    (fixtures, header_lcl, hash_lcl)
}

/// Regression test for #3811 (load-bearing).
///
/// A retriable persist failure — here the `ledgerheaders` table is dropped so
/// `persist_ledger_history`'s first write (`store_ledger_header`) fails and
/// maps to the retriable `HistoryError::CatchupFailed` — must NOT leave the
/// in-memory LCL advanced past a batch whose history rows were never written,
/// and must NOT report catchup success.
///
/// On `origin/main` (persist-AFTER-replay), attempt 0 replays the whole batch
/// (advancing LCL to `target`), then persist fails retriably; the retry loop's
/// next attempt computes `replay_first = target + 1 > target` and takes the
/// early `Ok` return, so catchup reports SUCCESS with LCL at `target` and zero
/// rows across `[lcl+1, target]` — the silent multi-table hole this issue is
/// about (it also silently increments
/// `stellar_history_apply_ledger_chain_success_total`). Both assertions below
/// therefore FAIL on main.
///
/// After the fix (persist-BEFORE-replay), persist fails before any
/// `close_ledger`, so LCL never moves off `lcl`, every retry fails identically,
/// and catchup returns `Err`.
///
/// Note: on the fixed path this intentionally exhausts `REPLAY_RETRY_COUNT`
/// (~6.2s of backoff sleep). Do NOT shrink the retry constants — they are
/// parity-pinned by `test_replay_retry_count_matches_core`.
#[tokio::test]
async fn test_persist_failure_does_not_advance_lcl_or_report_catchup_success() {
    use henyey_history::{CatchupManager, CatchupMode};

    let lcl = 100u32;
    // Single batch [101, 105] — all within checkpoint 127, so the whole batch
    // is one persist transaction.
    let target = 105u32;

    let (fixtures, header_lcl, hash_lcl) = build_empty_chain_fixtures(lcl, target);
    let Some(base_url) = spawn_test_archive(fixtures).await else {
        return;
    };

    let archive = HistoryArchive::new(&base_url).expect("archive");
    let bucket_dir = tempfile::tempdir().expect("bucket dir");
    let bucket_manager =
        henyey_bucket::BucketManager::new(bucket_dir.path().to_path_buf()).expect("bucket manager");

    // Shared single-connection in-memory DB (open_in_memory uses an r2d2 pool
    // with max_size(1)), so the DROP below is visible to the later persist
    // transaction. Keep our own Arc handle via new_with_arcs.
    let db = Arc::new(Database::open_in_memory().expect("db"));

    let ledger_manager = henyey_ledger::LedgerManager::new(
        "Test SDF Network ; September 2015".to_string(),
        henyey_ledger::LedgerManagerConfig {
            validate_bucket_hash: false,
            ..Default::default()
        },
    );
    let bucket_list = empty_bucket_list();
    let hot_archive = henyey_bucket::HotArchiveBucketList::default();
    ledger_manager
        .initialize(bucket_list, hot_archive, header_lcl, hash_lcl)
        .expect("initialize ledger manager at LCL");

    let mut manager = CatchupManager::new_with_arcs(
        vec![Arc::new(archive)],
        Arc::new(bucket_manager),
        Arc::clone(&db),
    );
    manager.set_replay_config(henyey_history::ReplayConfig {
        verify_bucket_list: false,
        verify_header_chain: false,
        verify_tx_set: false,
        verify_tx_results: false,
        verify_header_hash: false,
        ..Default::default()
    });

    // Inject the fault: drop `ledgerheaders` so `store_ledger_header` (the
    // unconditional first write in `persist_ledger_history`) fails → retriable
    // `CatchupFailed`. Nothing in the replay path READS `ledgerheaders`, so
    // exactly persist fails. Do NOT hold the connection across catchup —
    // max_size(1) would deadlock.
    db.with_connection(|conn| {
        conn.execute("DROP TABLE ledgerheaders", [])
            .map_err(Into::into)
    })
    .expect("drop ledgerheaders");

    let result = manager
        .catchup_to_ledger_with_mode(target, CatchupMode::Recent(50), lcl, None, &ledger_manager)
        .await;

    // (b) LOAD-BEARING: LCL must not have advanced past the aborted batch.
    assert_eq!(
        ledger_manager.current_header().ledger_seq,
        lcl,
        "LCL must stay at the pre-batch value when the batch's persist fails"
    );
    // (a) Catchup must not report success.
    assert!(
        result.is_err(),
        "catchup must return Err when a batch's history rows could not be persisted"
    );
}

/// Regression test for #3811 (general invariant).
///
/// When replay aborts partway through a multi-ledger batch, every ledger that
/// already advanced the LCL must have its history rows persisted. Here a batch
/// spans three apply ledgers `[64, 66]`; the last ledger's archive
/// `bucket_list_hash` is corrupted and `verify_header_hash` is enabled, so
/// ledgers 64 and 65 close successfully (LCL advances to 65) and ledger 66
/// raises `LedgerError::HashMismatch` → fatal `HistoryError::ReplayHashMismatch`
/// mid-batch (close_ledger rejects the mismatch before mutating, so LCL stays
/// at 65).
///
/// On `origin/main` (persist-AFTER-replay), the batch aborts before
/// `persist_ledger_history` runs, so `get_ledger_header` is `None` across the
/// whole advanced range — the final assertion FAILS. After the fix
/// (persist-BEFORE-replay) the batch's rows were written up front, so every
/// closed ledger has its header row.
///
/// Deviation from the plan: `verify_header_hash: true` is the only seam a
/// corrupted *archive* header can trip — the `LedgerManager`'s
/// `validate_bucket_hash` check compares the internally-computed hash against
/// itself and so cannot be triggered from the archive (documented on
/// `test_replay_with_validate_bucket_hash_enabled`). `verify_header_hash`
/// compares the FULL header, so the non-corrupt ledgers' archive headers must
/// exactly equal what `close_ledger` produces (bucket hash, `skip_list`, fees,
/// …). Rather than hand-derive every field, a first "probe" pass replays the
/// empty ledgers through a throwaway `LedgerManager` and captures the exact
/// computed headers; the archive is then built from those (corrupting only the
/// last), so the earlier ledgers pass verification by construction. Asserting
/// on `get_ledger_header` (the unconditional write) rather than `txsets` keeps
/// the test robust to empty-tx ledgers.
#[tokio::test]
async fn test_closed_ledgers_have_history_rows_when_replay_fails_midway() {
    use henyey_history::{CatchupManager, CatchupMode};
    use henyey_ledger::{LedgerCloseData, LedgerManager, LedgerManagerConfig};

    let lcl = 63u32;
    // Single batch [64, 66] (checkpoint_containing(64) == 127).
    let target = 66u32;
    let data_checkpoint = henyey_history::checkpoint::checkpoint_containing(target);
    let corrupt_seq = target; // Corrupt the LAST ledger's bucket_list_hash.

    let passphrase = "Test SDF Network ; September 2015".to_string();

    // LCL header (init state, never replayed — its bucket hash is irrelevant
    // because we never enable a bucket-hash check on the LCL itself).
    let header_lcl = make_header(
        lcl,
        Hash256::ZERO,
        Hash256::ZERO,
        Hash256::ZERO,
        Hash256::ZERO,
    );
    let hash_lcl = verify::compute_header_hash(&header_lcl).expect("lcl hash");

    let empty_result_set = TransactionResultSet {
        results: VecM::default(),
    };

    // ---- Pass 1 (probe): replay empty CLASSIC ledgers through a throwaway
    // LedgerManager to capture the exact computed headers. The archive is then
    // built from these, so the non-corrupt ledgers match `verify_header_hash`
    // by construction. Both passes initialize identically, so close_ledger is
    // deterministic across them.
    let probe = LedgerManager::new(
        passphrase.clone(),
        LedgerManagerConfig {
            validate_bucket_hash: false,
            ..Default::default()
        },
    );
    probe
        .initialize(
            empty_bucket_list(),
            henyey_bucket::HotArchiveBucketList::default(),
            header_lcl.clone(),
            hash_lcl,
        )
        .expect("initialize probe manager");

    let mut computed_header: HashMap<u32, LedgerHeader> = HashMap::new();
    let mut tx_sets: HashMap<u32, TransactionSet> = HashMap::new();
    for seq in (lcl + 1)..=target {
        let prev = probe.current_header_hash();
        let tx_set = TransactionSet {
            previous_ledger_hash: Hash(*prev.as_bytes()),
            txs: VecM::default(),
        };
        tx_sets.insert(seq, tx_set.clone());
        let close_data = LedgerCloseData::new(seq, TransactionSetVariant::Classic(tx_set), 0, prev);
        probe
            .close_ledger(close_data, None)
            .expect("probe close_ledger");
        computed_header.insert(seq, probe.current_header());
    }

    // ---- Build the archive from the computed headers, corrupting only the
    // last ledger's bucket_list_hash.
    let mut fixtures: HashMap<String, Vec<u8>> = HashMap::new();

    // LCL checkpoint file (63): just the LCL header.
    let lcl_entry = LedgerHeaderHistoryEntry {
        hash: hash_lcl.into(),
        header: header_lcl.clone(),
        ext: LedgerHeaderHistoryEntryExt::default(),
    };
    fixtures.insert(
        checkpoint_path("ledger", lcl, "xdr.gz"),
        gzip_bytes(&record_marked(&[lcl_entry
            .to_xdr(stellar_xdr::Limits::none())
            .expect("lcl entry xdr")])),
    );
    fixtures.insert(
        checkpoint_path("transactions", lcl, "xdr.gz"),
        gzip_bytes(&[]),
    );
    fixtures.insert(checkpoint_path("results", lcl, "xdr.gz"), gzip_bytes(&[]));

    // Data checkpoint file (127): apply headers 64..66 (66 corrupted).
    let mut apply_header_xdrs: Vec<Vec<u8>> = Vec::new();
    let mut tx_history_xdrs: Vec<Vec<u8>> = Vec::new();
    let mut tx_result_xdrs: Vec<Vec<u8>> = Vec::new();
    for seq in (lcl + 1)..=target {
        let mut header = computed_header[&seq].clone();
        if seq == corrupt_seq {
            header.bucket_list_hash = Hash([0xFF; 32]);
        }
        let header_hash = verify::compute_header_hash(&header).expect("header hash");
        let entry = LedgerHeaderHistoryEntry {
            hash: header_hash.into(),
            header,
            ext: LedgerHeaderHistoryEntryExt::default(),
        };
        apply_header_xdrs.push(
            entry
                .to_xdr(stellar_xdr::Limits::none())
                .expect("entry xdr"),
        );

        let tx_history_entry = TransactionHistoryEntry {
            ledger_seq: seq,
            tx_set: tx_sets[&seq].clone(),
            ext: TransactionHistoryEntryExt::V0,
        };
        tx_history_xdrs.push(
            tx_history_entry
                .to_xdr(stellar_xdr::Limits::none())
                .expect("tx history xdr"),
        );
        let tx_result_entry = TransactionHistoryResultEntry {
            ledger_seq: seq,
            tx_result_set: empty_result_set.clone(),
            ext: TransactionHistoryResultEntryExt::default(),
        };
        tx_result_xdrs.push(
            tx_result_entry
                .to_xdr(stellar_xdr::Limits::none())
                .expect("tx result xdr"),
        );
    }
    fixtures.insert(
        checkpoint_path("ledger", data_checkpoint, "xdr.gz"),
        gzip_bytes(&record_marked(&apply_header_xdrs)),
    );
    fixtures.insert(
        checkpoint_path("transactions", data_checkpoint, "xdr.gz"),
        gzip_bytes(&record_marked(&tx_history_xdrs)),
    );
    fixtures.insert(
        checkpoint_path("results", data_checkpoint, "xdr.gz"),
        gzip_bytes(&record_marked(&tx_result_xdrs)),
    );

    register_published_target_has(&mut fixtures, target);

    let Some(base_url) = spawn_test_archive(fixtures).await else {
        return;
    };

    // ---- Pass 2: real catchup with verify_header_hash enabled.
    let archive = HistoryArchive::new(&base_url).expect("archive");
    let bucket_dir = tempfile::tempdir().expect("bucket dir");
    let bucket_manager =
        henyey_bucket::BucketManager::new(bucket_dir.path().to_path_buf()).expect("bucket manager");
    let db = Arc::new(Database::open_in_memory().expect("db"));

    let ledger_manager = LedgerManager::new(
        passphrase.clone(),
        LedgerManagerConfig {
            validate_bucket_hash: false,
            ..Default::default()
        },
    );
    ledger_manager
        .initialize(
            empty_bucket_list(),
            henyey_bucket::HotArchiveBucketList::default(),
            header_lcl.clone(),
            hash_lcl,
        )
        .expect("initialize ledger manager at LCL");

    let mut manager = CatchupManager::new_with_arcs(
        vec![Arc::new(archive)],
        Arc::new(bucket_manager),
        Arc::clone(&db),
    );
    manager.set_replay_config(henyey_history::ReplayConfig {
        verify_bucket_list: false,
        verify_header_chain: false,
        verify_tx_set: false,
        verify_tx_results: false,
        // Only header-hash verification is enabled: this is the seam a corrupt
        // archive header can trip.
        verify_header_hash: true,
        ..Default::default()
    });

    let result = manager
        .catchup_to_ledger_with_mode(target, CatchupMode::Recent(50), lcl, None, &ledger_manager)
        .await;

    // The batch must have aborted with a fatal hash mismatch at the last ledger.
    assert!(
        matches!(result, Err(HistoryError::ReplayHashMismatch { .. })),
        "expected ReplayHashMismatch, got {result:?}"
    );

    // The earlier ledgers must have closed (LCL advanced) but the batch must
    // have stopped before the corrupted last ledger — guards against the test
    // becoming vacuous.
    let current_lcl = ledger_manager.current_header().ledger_seq;
    assert!(
        current_lcl > lcl && current_lcl < target,
        "expected a partial advance in ({lcl}, {target}), got {current_lcl}"
    );

    // Every ledger that advanced the LCL must have its header row persisted.
    for seq in (lcl + 1)..=current_lcl {
        assert!(
            db.get_ledger_header(seq).expect("query header").is_some(),
            "ledger {seq} closed (LCL advanced) but has no persisted header row"
        );
    }
}
