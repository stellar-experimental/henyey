//! Reusable in-process history-archive fixtures for integration tests.
//!
//! Only available when the `test-utils` feature is enabled.
//!
//! Exposes helpers for constructing a well-formed history archive tree in a
//! tempdir, serving it over an in-process axum HTTP server bound to an
//! ephemeral port, and computing the correctly-chained ledger header /
//! bucket-list hashes that catchup will verify against.
//!
//! Primary entry point: [`build_single_checkpoint_archive`] which produces a
//! [`HistoryArchiveFixture`] pointing at a running local server.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::Router;
use flate2::{write::GzEncoder, Compression};
use henyey_bucket::{Bucket, BucketList, HotArchiveBucketList};
use henyey_common::Hash256;
use sha2::{Digest, Sha256};
use stellar_xdr::curr::{
    Hash, LedgerHeader, LedgerHeaderExt, LedgerHeaderHistoryEntry, LedgerHeaderHistoryEntryExt,
    StellarValue, StellarValueExt, TimePoint, VecM, WriteXdr,
};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::archive_state::{HASBucketLevel, HistoryArchiveState};
use crate::paths::{bucket_path, checkpoint_path};

/// Default network passphrase used by fixtures. Matches the testnet
/// passphrase so subprocesses that default to `--testnet` can catch up
/// against the fixture without additional plumbing.
pub const DEFAULT_FIXTURE_PASSPHRASE: &str = "Test SDF Network ; September 2015";

/// Gzip-compress a byte slice.
pub fn gzip_bytes(data: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    use std::io::Write;
    encoder.write_all(data).expect("gzip write");
    encoder.finish().expect("gzip finish")
}

/// Wrap XDR data in RFC-5531 record marking.
///
/// Each record is prefixed with a 4-byte mark: high bit set (last fragment)
/// plus a 31-bit big-endian length. Matches stellar-core's
/// `XDROutputFileStream::writeOne` framing — the same framing emitted by
/// `henyey_common::xdr_stream::XdrOutputStream`.
pub fn wrap_in_record_marks(data: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(4 + data.len());
    let record_mark = (data.len() as u32) | 0x80000000;
    result.extend_from_slice(&record_mark.to_be_bytes());
    result.extend_from_slice(data);
    result
}

/// Build a minimal [`LedgerHeader`] at `ledger_seq` with its `bucket_list_hash`
/// set to `combined_hash` (i.e. SHA-256(live_hash || hot_archive_hash) — see
/// [`combined_bucket_list_hash`]).
pub fn make_test_header(ledger_seq: u32, combined_hash: Hash256) -> LedgerHeader {
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
        bucket_list_hash: Hash(*combined_hash.as_bytes()),
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

/// Build an 11-level [`BucketList`] whose level-0 `curr` is `bucket_hash`
/// and whose remaining levels are all zero hashes (empty buckets).
///
/// `bucket` must be the actual bucket whose hash equals `bucket_hash`.
pub fn make_bucket_list_with_hash(bucket_hash: Hash256, bucket: Bucket) -> BucketList {
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

    let load_bucket = move |hash: &Hash256| -> henyey_bucket::Result<Bucket> {
        if hash.is_zero() {
            return Ok(Bucket::empty());
        }
        if *hash == bucket_hash {
            return Ok(bucket.clone());
        }
        Err(henyey_bucket::BucketError::Serialization(format!(
            "bucket not found: {}",
            hash.to_hex(),
        )))
    };

    BucketList::restore_from_hashes(&hashes, load_bucket).expect("restore bucket list")
}

/// Compute SHA-256(live_hash || hot_archive_hash) as stored in a
/// [`LedgerHeader`]'s `bucket_list_hash` field.
pub fn combined_bucket_list_hash(live_hash: Hash256, hot_archive_hash: Hash256) -> Hash256 {
    let mut hasher = Sha256::new();
    hasher.update(live_hash.as_bytes());
    hasher.update(hot_archive_hash.as_bytes());
    let result = hasher.finalize();
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&result);
    Hash256::from_bytes(bytes)
}

/// A running history-archive fixture: a tempdir seeded with the correct
/// archive file tree plus an in-process axum server serving it over HTTP.
///
/// Dropping the fixture shuts down the server (via the abort handle) and
/// removes the tempdir. The fixture's `base_url` is what `[[history.archives]]`
/// entries should point at.
pub struct HistoryArchiveFixture {
    /// `http://127.0.0.1:<port>/` — suitable for the `url` field of a
    /// `[[history.archives]]` config entry.
    pub base_url: String,
    /// Sequence of the checkpoint this fixture exposes.
    pub checkpoint: u32,
    /// Network passphrase embedded in the HAS.
    pub network_passphrase: String,
    _server: AbortOnDrop,
}

/// RAII guard that aborts a tokio task on drop. Keeps fixture lifetime tied
/// to the caller's scope so the server doesn't leak beyond the test.
struct AbortOnDrop(Option<JoinHandle<()>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            h.abort();
        }
    }
}

/// Error returned when the fixture server cannot bind a loopback socket
/// — typically because the test environment does not permit listening
/// sockets. Callers can treat this as a signal to skip the test.
#[derive(Debug)]
pub struct FixtureBindDenied;

impl std::fmt::Display for FixtureBindDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("tcp bind not permitted in this environment")
    }
}

impl std::error::Error for FixtureBindDenied {}

/// Build a history archive fixture containing a single checkpoint at
/// `checkpoint` (which must be a valid checkpoint boundary — e.g. 63, 127, …
/// under the default 64-ledger cadence).
///
/// Returns `Err(FixtureBindDenied)` when the loopback bind is refused by
/// the OS (sandboxed environments), so callers can gracefully skip the
/// test instead of panicking. All other bind errors panic.
///
/// The archive has:
/// - an empty bucket (level 0 `curr`) plus zero-hash buckets on all other
///   levels,
/// - a record-marked, gzipped `ledger-<checkpoint>.xdr.gz` containing a
///   single [`LedgerHeaderHistoryEntry`] whose `bucket_list_hash` matches
///   the bucket list + empty hot-archive,
/// - a `history-<checkpoint>.json` HAS file,
/// - an in-process axum server serving all of the above.
pub async fn build_single_checkpoint_archive(
    checkpoint: u32,
) -> Result<HistoryArchiveFixture, FixtureBindDenied> {
    let bucket_data: Vec<u8> = Vec::new();
    let bucket_hash = Hash256::hash(&bucket_data);
    // Create a real empty bucket whose hash = SHA256("") = bucket_hash.
    // Bucket::from_entries(vec![]) produces exactly this hash since it
    // hashes zero entries with the same SHA-256 streaming approach.
    let bucket = Bucket::from_entries(vec![]).expect("empty bucket");
    assert_eq!(bucket.hash(), bucket_hash, "empty bucket hash mismatch");
    let bucket_list = make_bucket_list_with_hash(bucket_hash, bucket);
    let bucket_list_hash = bucket_list.hash();

    let hot_archive = HotArchiveBucketList::new();
    let combined = combined_bucket_list_hash(bucket_list_hash, hot_archive.hash());

    let header = make_test_header(checkpoint, combined);
    let header_hash =
        crate::verify::compute_header_hash(&header).expect("test checkpoint header hash");
    let header_entry = LedgerHeaderHistoryEntry {
        hash: header_hash.into(),
        header,
        ext: LedgerHeaderHistoryEntryExt::default(),
    };
    let header_xdr = header_entry
        .to_xdr(stellar_xdr::curr::Limits::none())
        .expect("header xdr");

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
        server: Some("henyey-history test_utils".to_string()),
        current_ledger: checkpoint,
        network_passphrase: Some(DEFAULT_FIXTURE_PASSPHRASE.to_string()),
        current_buckets,
        hot_archive_buckets: None,
    };
    let has_json = has.to_json().expect("has json");

    let mut fixtures: HashMap<String, Vec<u8>> = HashMap::new();
    fixtures.insert(
        checkpoint_path("history", checkpoint, "json"),
        has_json.into_bytes(),
    );
    fixtures.insert(
        checkpoint_path("ledger", checkpoint, "xdr.gz"),
        gzip_bytes(&wrap_in_record_marks(&header_xdr)),
    );
    fixtures.insert(bucket_path(&bucket_hash), gzip_bytes(&bucket_data));

    // Also serve a well-known root HAS that points at the latest checkpoint
    // so clients that do root-HAS discovery succeed against this fixture.
    fixtures.insert(
        ".well-known/stellar-history.json".to_string(),
        has.to_json().expect("has json").into_bytes(),
    );

    let fixtures = Arc::new(fixtures);
    let router =
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
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(FixtureBindDenied);
        }
        Err(e) => panic!("fixture bind failed: {e}"),
    };
    let addr = listener.local_addr().expect("fixture addr");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    Ok(HistoryArchiveFixture {
        base_url: format!("http://{}/", addr),
        checkpoint,
        network_passphrase: DEFAULT_FIXTURE_PASSPHRASE.to_string(),
        _server: AbortOnDrop(Some(handle)),
    })
}
