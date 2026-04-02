use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::get,
    Router,
};
use flate2::{write::GzEncoder, Compression};
use henyey_bucket::{Bucket, BucketList, HotArchiveBucketList};
use henyey_common::Hash256;
use henyey_db::Database;
use henyey_history::{
    archive::HistoryArchive,
    archive_state::{HASBucketLevel, HistoryArchiveState},
    catchup::{CatchupManagerBuilder, CatchupOptions},
    paths::{bucket_path, checkpoint_path},
};
use sha2::{Digest, Sha256};
use stellar_xdr::curr::{
    Hash, LedgerHeader, LedgerHeaderExt, LedgerHeaderHistoryEntry, LedgerHeaderHistoryEntryExt,
    StellarValue, StellarValueExt, TimePoint, VecM, WriteXdr,
};
use tokio::net::TcpListener;

fn gzip_bytes(data: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    use std::io::Write;
    encoder.write_all(data).expect("gzip write");
    encoder.finish().expect("gzip finish")
}

/// Wrap XDR data in record marking format (RFC 5531).
/// Each record is prefixed with a 4-byte mark: high bit set + 31-bit size (big-endian).
fn wrap_in_record_marks(data: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(4 + data.len());
    let size = data.len() as u32;
    let record_mark = size | 0x80000000; // Set high bit (last fragment)
    result.extend_from_slice(&record_mark.to_be_bytes());
    result.extend_from_slice(data);
    result
}

fn make_test_header(ledger_seq: u32, bucket_list_hash: Hash256) -> LedgerHeader {
    LedgerHeader {
        ledger_version: 25,
        previous_ledger_hash: Hash([0u8; 32]),
        scp_value: StellarValue {
            tx_set_hash: Hash([0u8; 32]),
            close_time: TimePoint(0),
            upgrades: VecM::default(),
            ext: StellarValueExt::Basic,
        },
        tx_set_result_hash: Hash([0u8; 32]),
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

fn make_bucket_list_with_hash(bucket_hash: Hash256) -> BucketList {
    let mut hashes = Vec::with_capacity(22);
    for level in 0..11 {
        if level == 0 {
            hashes.push(bucket_hash);
            hashes.push(Hash256::ZERO);
        } else {
            hashes.push(Hash256::ZERO);
            hashes.push(Hash256::ZERO);
        }
    }

    let empty_bucket_hash = Hash256::hash(&[]);
    let bucket_data: Vec<u8> = Vec::new();
    let load_bucket = move |hash: &Hash256| -> henyey_bucket::Result<Bucket> {
        if hash.is_zero() || *hash == empty_bucket_hash {
            return Ok(Bucket::empty());
        }
        Bucket::from_xdr_bytes(&bucket_data)
    };

    BucketList::restore_from_hashes(&hashes, load_bucket).expect("restore bucket list")
}

#[tokio::test]
async fn test_catchup_against_local_archive_checkpoint() {
    let checkpoint = 63u32;
    let bucket_data: Vec<u8> = Vec::new();
    let bucket_hash = Hash256::hash(&bucket_data);
    let bucket_list = make_bucket_list_with_hash(bucket_hash);
    let bucket_list_hash = bucket_list.hash();

    // The header's bucket_list_hash is SHA256(live_hash || hot_archive_hash),
    // not just the live bucket list hash.
    let hot_archive = HotArchiveBucketList::new();
    let hot_archive_hash = hot_archive.hash();
    let mut hasher = Sha256::new();
    hasher.update(bucket_list_hash.as_bytes());
    hasher.update(hot_archive_hash.as_bytes());
    let result = hasher.finalize();
    let mut combined_bytes = [0u8; 32];
    combined_bytes.copy_from_slice(&result);
    let combined_hash = Hash256::from_bytes(combined_bytes);

    let header = make_test_header(checkpoint, combined_hash);
    let header_entry = LedgerHeaderHistoryEntry {
        hash: Hash([0u8; 32]),
        header,
        ext: LedgerHeaderHistoryEntryExt::default(),
    };
    let header_xdr = header_entry
        .to_xdr(stellar_xdr::curr::Limits::none())
        .expect("header xdr");

    // Create all 11 bucket levels (required by BucketList)
    let zero_hash = "0".repeat(64);
    let mut current_buckets = Vec::with_capacity(11);
    for level in 0..11 {
        if level == 0 {
            current_buckets.push(HASBucketLevel {
                curr: bucket_hash.to_hex(),
                snap: zero_hash.clone(),
                next: Default::default(),
            });
        } else {
            current_buckets.push(HASBucketLevel {
                curr: zero_hash.clone(),
                snap: zero_hash.clone(),
                next: Default::default(),
            });
        }
    }

    let has = HistoryArchiveState {
        version: 2,
        server: Some("rs-stellar-core test".to_string()),
        current_ledger: checkpoint,
        network_passphrase: Some("Test SDF Network ; September 2015".to_string()),
        current_buckets,
        hot_archive_buckets: None,
    };
    let has_json = has.to_json().expect("has json");

    let mut fixtures: HashMap<String, Vec<u8>> = HashMap::new();
    let has_path = checkpoint_path("history", checkpoint, "json");
    let ledger_path = checkpoint_path("ledger", checkpoint, "xdr.gz");
    let bucket_path_str = bucket_path(&bucket_hash);

    fixtures.insert(has_path, has_json.into_bytes());
    // Ledger headers need record marking format
    fixtures.insert(ledger_path, gzip_bytes(&wrap_in_record_marks(&header_xdr)));
    fixtures.insert(bucket_path_str, gzip_bytes(&bucket_data));

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
            verify_buckets: true,
            verify_headers: true,
            ..CatchupOptions::default()
        })
        .build()
        .expect("catchup manager");

    let output = manager
        .catchup_to_ledger(checkpoint, &ledger_manager)
        .await
        .expect("catchup");

    assert_eq!(output.ledger_seq, checkpoint);
    assert_eq!(output.buckets_downloaded, 1);
    assert_eq!(output.ledgers_applied, 0);
}
