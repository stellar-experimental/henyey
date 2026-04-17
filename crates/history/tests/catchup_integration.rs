use henyey_db::Database;
use henyey_history::{
    archive::HistoryArchive,
    catchup::{CatchupManagerBuilder, CatchupOptions},
    test_utils::{build_single_checkpoint_archive, DEFAULT_FIXTURE_PASSPHRASE},
};

#[tokio::test]
async fn test_catchup_against_local_archive_checkpoint() {
    let checkpoint = 63u32;
    let fixture = build_single_checkpoint_archive(checkpoint).await;

    let archive = HistoryArchive::new(&fixture.base_url).expect("archive");

    let bucket_dir = tempfile::tempdir().expect("bucket dir");
    let bucket_manager =
        henyey_bucket::BucketManager::new(bucket_dir.path().to_path_buf()).expect("bucket manager");
    let db = Database::open_in_memory().expect("db");

    let ledger_manager = henyey_ledger::LedgerManager::new(
        DEFAULT_FIXTURE_PASSPHRASE.to_string(),
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
