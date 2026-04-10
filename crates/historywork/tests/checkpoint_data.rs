use std::sync::Arc;

use henyey_history::archive_state::HistoryArchiveState;
use henyey_historywork::{build_checkpoint_data, HistoryWorkState};
use tokio::sync::Mutex;

#[tokio::test]
async fn test_build_checkpoint_data_requires_has() {
    let state = Arc::new(Mutex::new(HistoryWorkState::default()));
    let err = build_checkpoint_data(&state).await.unwrap_err();
    assert!(err.to_string().contains("missing History Archive State"));
}

#[tokio::test]
async fn test_build_checkpoint_data_takes_state() {
    let tmp_dir = tempfile::tempdir().unwrap();
    let mut work = HistoryWorkState::default();
    work.has = Some(HistoryArchiveState {
        version: 2,
        server: None,
        current_ledger: 64.into(),
        network_passphrase: None,
        current_buckets: Vec::new(),
        hot_archive_buckets: None,
    });
    work.bucket_dir = Some(tmp_dir.path().to_path_buf());

    let state = Arc::new(Mutex::new(work));
    let checkpoint = build_checkpoint_data(&state).await.unwrap();

    assert_eq!(checkpoint.has.current_ledger, 64);
    assert_eq!(checkpoint.bucket_dir, tmp_dir.path());
    assert!(checkpoint.headers.is_empty());
    assert!(checkpoint.transactions.is_empty());
    assert!(checkpoint.tx_results.is_empty());
    assert!(checkpoint.scp_history.is_empty());

    // Second call fails because the data has been moved out.
    let err = build_checkpoint_data(&state).await.unwrap_err();
    assert!(err.to_string().contains("missing History Archive State"));
}
