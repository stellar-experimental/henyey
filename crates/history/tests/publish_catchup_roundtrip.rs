//! AC#3 of the SSC history mission (#3295): an offline publish -> catchup
//! round-trip that exercises the *real* publish + command-template upload +
//! catchup code paths (not a stub), asserting ledger-hash agreement.
//!
//! Both tests build a deterministic single-checkpoint archive from the public
//! `test_utils` `make_*` helpers (NOT the HTTP-server fixture
//! `build_single_checkpoint_archive`), publish it with
//! `PublishManager::publish_checkpoint`, then make it readable to catchup:
//!
//!   * `test_publish_command_template_then_catchup` uploads the staged
//!     checkpoint into a destination dir via `UploadPlan::execute` against a
//!     `RemoteArchive { put: "cp {0} {1}", mkdir: "mkdir -p {0}" }` — the same
//!     SSC-style command-template upload path the publishing-validator config
//!     in `ssc_mission_history.cfg` uses — then catches up over `file://<dest>`.
//!
//!   * `test_publish_file_local_then_catchup` skips the command-template hop:
//!     catchup reads directly from the publish staging dir over `file://`.
//!
//! Both assert the caught-up ledger sequence and final ledger hash match the
//! published checkpoint header. No loopback socket is bound, so (unlike the
//! HTTP-server fixture) there is no `FixtureBindDenied` skip guard.

use henyey_bucket::{Bucket, HotArchiveBucketList};
use henyey_common::Hash256;
use henyey_db::Database;
use henyey_history::{
    archive::HistoryArchive,
    catchup::{CatchupManagerBuilder, CatchupOptions},
    publish::{PublishConfig, PublishManager},
    test_utils::{
        combined_bucket_list_hash, make_bucket_list_with_hash, make_test_header,
        DEFAULT_FIXTURE_PASSPHRASE,
    },
    upload::UploadPlan,
    RemoteArchive, RemoteArchiveConfig,
};
use henyey_ledger::{LedgerManager, LedgerManagerConfig};
use stellar_xdr::curr::{LedgerHeaderHistoryEntry, LedgerHeaderHistoryEntryExt};

/// Build the deterministic single empty-bucket checkpoint and publish it into
/// `staging_dir`. Returns `(checkpoint_seq, expected_header_hash)`.
///
/// This mirrors `test_utils::build_single_checkpoint_archive`'s data — an empty
/// level-0 bucket, a header whose `bucket_list_hash` is the combined live ||
/// hot-archive hash — but routes it through the real publish pipeline
/// (`PublishManager::publish_checkpoint`) instead of an HTTP fixture.
fn publish_single_checkpoint(checkpoint: u32, staging_dir: &std::path::Path) -> Hash256 {
    let bucket = Bucket::from_entries(vec![]).expect("empty bucket");
    let bucket_hash = bucket.hash();
    let bucket_list = make_bucket_list_with_hash(bucket_hash, bucket);
    let bucket_list_hash = bucket_list.hash();

    let hot_archive = HotArchiveBucketList::new();
    let combined = combined_bucket_list_hash(bucket_list_hash, hot_archive.hash());

    let header = make_test_header(checkpoint, combined);
    let header_hash = henyey_history::verify::compute_header_hash(&header).expect("header hash");
    let header_entry = LedgerHeaderHistoryEntry {
        hash: header_hash.into(),
        header,
        ext: LedgerHeaderHistoryEntryExt::default(),
    };

    let manager = PublishManager::new(PublishConfig {
        local_path: staging_dir.to_path_buf(),
        network_passphrase: Some(DEFAULT_FIXTURE_PASSPHRASE.to_string()),
        ..Default::default()
    });

    // Build the HAS from the bucket list + hot archive so it carries the
    // version-2 hot-archive levels catchup expects, then publish.
    let has = henyey_history::build_history_archive_state(
        checkpoint,
        &bucket_list,
        Some(&hot_archive),
        Some(DEFAULT_FIXTURE_PASSPHRASE.to_string()),
    )
    .expect("build HAS");

    let state = manager
        .publish_checkpoint(
            checkpoint,
            std::slice::from_ref(&header_entry),
            &[],
            &[],
            &bucket_list,
            Some(&has),
        )
        .expect("publish checkpoint");
    assert!(
        state.files_written > 0,
        "publish must write at least the headers + HAS + bucket"
    );

    header_hash
}

/// Catch up from `file://<archive_dir>/` to `checkpoint` and assert the
/// resulting ledger seq + hash match the published checkpoint header.
async fn catchup_and_assert(
    archive_dir: &std::path::Path,
    checkpoint: u32,
    expected_hash: Hash256,
) {
    // file:// URL with a trailing slash so relative archive paths resolve.
    let url = format!("file://{}/", archive_dir.display());
    let archive = HistoryArchive::new(&url).expect("file:// archive");

    let bucket_dir = tempfile::tempdir().expect("bucket dir");
    let bucket_manager =
        henyey_bucket::BucketManager::new(bucket_dir.path().to_path_buf()).expect("bucket manager");
    let db = Database::open_in_memory().expect("db");

    let ledger_manager = LedgerManager::new(
        DEFAULT_FIXTURE_PASSPHRASE.to_string(),
        LedgerManagerConfig {
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
        })
        .build()
        .expect("catchup manager");

    let output = manager
        .catchup_to_ledger(checkpoint, &ledger_manager)
        .await
        .expect("catchup");

    assert_eq!(output.ledger_seq, checkpoint, "caught up to wrong ledger");
    assert_eq!(
        output.ledger_hash, expected_hash,
        "caught-up ledger hash must match the published checkpoint header hash"
    );
}

/// Publish -> SSC-style `cp`/`mkdir` command-template upload -> catchup over file://.
#[tokio::test]
async fn test_publish_command_template_then_catchup() {
    let checkpoint = 63u32;

    let staging = tempfile::tempdir().expect("staging dir");
    let dest = tempfile::tempdir().expect("dest dir");

    let expected_hash = publish_single_checkpoint(checkpoint, staging.path());

    // Upload every staged file into the destination archive via the SSC-style
    // command templates, using stellar-core's substitution convention:
    // `{0}` = LOCAL staged file (absolute), `{1}` = REMOTE (dest-relative) path.
    // `cp {0} <dest>/{1}` copies the staged file into place; `mkdir -p
    // <dest>/{0}` (mkdir uses `{0}` for the directory) creates the parent first.
    let put_cmd = format!("cp {{0}} {}/{{1}}", dest.path().display());
    let mkdir_cmd = format!("mkdir -p {}/{{0}}", dest.path().display());
    let remote = RemoteArchive::new(RemoteArchiveConfig {
        name: "ssc_local".to_string(),
        get_cmd: None,
        put_cmd: Some(put_cmd),
        mkdir_cmd: Some(mkdir_cmd),
    });

    let plan = UploadPlan::from_staging_dir(staging.path()).expect("upload plan");
    plan.execute(&remote)
        .await
        .expect("command-template upload");

    catchup_and_assert(dest.path(), checkpoint, expected_hash).await;
}

/// Publish -> catchup directly from the staging dir over file:// (no upload hop).
#[tokio::test]
async fn test_publish_file_local_then_catchup() {
    let checkpoint = 127u32;

    let staging = tempfile::tempdir().expect("staging dir");
    let expected_hash = publish_single_checkpoint(checkpoint, staging.path());

    catchup_and_assert(staging.path(), checkpoint, expected_hash).await;
}
