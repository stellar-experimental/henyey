//! Integration test: watcher startup without persisted state.
//!
//! Verifies that a watcher node with an empty database starts up cleanly
//! without entering the `CatchingUp` state (i.e., `FallbackCatchup::Skip`
//! is honored), and shuts down gracefully.
//!
//! Regression coverage for #2106.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use henyey_app::config::ConfigBuilder;
use henyey_app::run_cmd::NodeRunner;
use henyey_app::{AppState, RunOptions};

#[tokio::test]
async fn test_watcher_startup_without_state() {
    let tmp = tempfile::tempdir().expect("failed to create tempdir");
    let db_path = tmp.path().join("test.db");

    let mut config = ConfigBuilder::new().database_path(&db_path).build();
    // Prevent start_overlay() from injecting testnet/mainnet seed peers.
    config.is_compat_config = true;
    // No peers — fully hermetic, no DNS resolution or TCP connections.
    config.overlay.known_peers = vec![];
    config.overlay.target_outbound_peers = 0;
    config.overlay.max_outbound_peers = 0;

    let runner = Arc::new(
        NodeRunner::new(config, RunOptions::watcher())
            .await
            .expect("failed to create NodeRunner"),
    );

    let runner_for_task = runner.clone();
    let handle = tokio::spawn(async move { runner_for_task.run().await });

    // Poll state until Synced (5s timeout). Fail immediately if CatchingUp
    // is ever sampled — that would mean FallbackCatchup::Skip was not honored.
    let deadline = Instant::now() + Duration::from_secs(5);

    loop {
        // Detect early task exit (panic or error before reaching Synced).
        if handle.is_finished() {
            panic!(
                "run task exited before reaching Synced state; \
                 likely a startup failure"
            );
        }

        let state = runner.app().state().await;
        assert_ne!(
            state,
            AppState::CatchingUp,
            "watcher must not enter CatchingUp state with empty DB"
        );

        if state == AppState::Synced {
            break;
        }

        if Instant::now() > deadline {
            panic!(
                "timed out waiting for Synced state (last observed: {:?})",
                state
            );
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Supporting evidence: query_is_ready stays false because is_initialized()
    // was false (empty DB, no catchup ran).
    assert!(
        !runner.app().query_is_ready().load(Ordering::Acquire),
        "query_is_ready must remain false for empty-DB watcher startup"
    );

    // Trigger clean shutdown.
    runner.shutdown();

    // Await task completion with timeout to prevent CI hangs.
    let result = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("shutdown timed out after 5s")
        .expect("run task panicked");

    result.expect("run task returned an error");
}
