//! Integration tests for per-ledger stale-bucket garbage collection (#3028).
//!
//! Issue #3028 removed the every-100-ledger GC throttle so stale-bucket GC runs
//! on every ledger close, matching stellar-core's unconditional
//! `forgetUnreferencedBuckets`. Because GC now fires every ~5s rather than every
//! ~500s, a slow run (blocked resolving an in-flight async merge) could still be
//! running when the next close fires. The `bucket_gc_in_flight` re-entrancy guard
//! coalesces overlapping invocations: at most one background GC at a time.
//!
//! These tests assert the observable behavior of that guard:
//!   - a second invocation while a GC is "in flight" coalesces (no new task), and
//!   - the guard flag resets after a real run so a later close re-arms GC.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use henyey_app::config::ConfigBuilder;
use henyey_app::App;

/// Build a minimal, hermetic `App` backed by a temp SQLite DB. No overlay peers,
/// no network — sufficient to exercise the bucket-GC re-entrancy guard, which
/// only touches the bucket manager / ledger manager / DB.
async fn build_test_app(db_dir: &std::path::Path) -> Arc<App> {
    build_test_app_with(db_dir, false).await
}

/// Like `build_test_app`, but lets the caller pick the `disable_bucket_gc`
/// kill-switch (#3153) and pins the bucket directory under `db_dir/buckets` so
/// the test can drop a stale file there directly.
async fn build_test_app_with(db_dir: &std::path::Path, disable_bucket_gc: bool) -> Arc<App> {
    let db_path = db_dir.join("bucket_gc_test.db");
    let mut config = ConfigBuilder::new()
        .database_path(&db_path)
        .bucket_directory(db_dir.join("buckets"))
        .build();
    config.is_compat_config = true;
    config.overlay.known_peers = vec![];
    config.overlay.target_outbound_peers = 0;
    config.overlay.max_outbound_peers = 0;
    config.buckets.disable_bucket_gc = disable_bucket_gc;
    Arc::new(App::new(config).await.expect("failed to build App"))
}

/// Write a syntactically-valid-but-unreferenced bucket file (64-hex name with
/// the canonical `.bucket.xdr` suffix) into `bucket_dir`. `list_buckets()` keys
/// off the filename pattern, so GC will see this as an on-disk bucket that is
/// not a GC root — it is deleted when GC is enabled and retained when disabled.
/// Returns the file path.
fn write_stale_bucket_file(bucket_dir: &std::path::Path) -> std::path::PathBuf {
    std::fs::create_dir_all(bucket_dir).expect("create bucket dir");
    // A 64-char hex hash that will not appear among the genesis GC roots.
    let stale_hash = "ab".repeat(32);
    let path = bucket_dir.join(format!("{stale_hash}.bucket.xdr"));
    std::fs::write(&path, b"stale-not-a-real-bucket").expect("write stale bucket file");
    path
}

/// Spin until `flag` reaches `expected` or the deadline elapses. Returns whether
/// it reached the expected value.
async fn await_flag(flag: &Arc<std::sync::atomic::AtomicBool>, expected: bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if flag.load(Ordering::Acquire) == expected {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    flag.load(Ordering::Acquire) == expected
}

/// Primary coverage for #3028: the re-entrancy guard coalesces a second
/// invocation while a GC is already in flight, and the flag resets after a real
/// run completes so a subsequent ledger close re-arms GC.
#[tokio::test(flavor = "multi_thread")]
async fn test_gc_reentrancy_guard_coalesces() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let app = build_test_app(tmp.path()).await;

    let flag = app.bucket_gc_in_flight().clone();

    // Precondition: no GC in flight at startup.
    assert!(
        !flag.load(Ordering::Acquire),
        "bucket_gc_in_flight must start false"
    );

    // Simulate a GC already running by setting the flag, then invoke. The guard's
    // compare_exchange(false -> true) must fail, so no new task is launched and
    // the call coalesces (returns false). The flag stays set (we own it).
    flag.store(true, Ordering::Release);
    assert!(
        !app.cleanup_stale_bucket_files_background(),
        "a second GC invocation while one is in flight must coalesce (return false)"
    );
    assert!(
        flag.load(Ordering::Acquire),
        "coalescing must not clear the in-flight flag held by the running GC"
    );

    // Release the simulated in-flight GC. Now a real invocation must acquire the
    // guard and launch a task (return true)...
    flag.store(false, Ordering::Release);
    assert!(
        app.cleanup_stale_bucket_files_background(),
        "with no GC in flight, the invocation must launch a GC task (return true)"
    );

    // ...and once that real run completes, the flag must reset to false so the
    // next ledger close re-arms GC. The reset lives in a Drop guard on the
    // detached awaiter task, so it fires regardless of the run's outcome.
    assert!(
        await_flag(&flag, false).await,
        "bucket_gc_in_flight must reset to false after the GC run completes"
    );

    // Re-arm check: a subsequent close can launch GC again.
    assert!(
        app.cleanup_stale_bucket_files_background(),
        "GC must re-arm after a completed run — a later close launches a new task"
    );
    assert!(
        await_flag(&flag, false).await,
        "flag must reset again after the second run"
    );
}

/// #3153 behavioral guarantee: with `disable_bucket_gc = true`, a stale
/// (unreferenced) on-disk bucket file SURVIVES a call to
/// `cleanup_stale_bucket_files_background()`. The kill-switch early-returns
/// before any blocking task spawns, so no file is deleted — mirroring
/// stellar-core's `!mConfig.DISABLE_BUCKET_GC` guard.
#[tokio::test(flavor = "multi_thread")]
async fn test_disable_bucket_gc_retains_stale_files() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let app = build_test_app_with(tmp.path(), /* disable_bucket_gc */ true).await;

    assert!(
        !app.bucket_gc_enabled(),
        "bucket_gc_enabled() must be false when disable_bucket_gc = true"
    );

    let stale = write_stale_bucket_file(&tmp.path().join("buckets"));
    assert!(stale.exists(), "precondition: stale file written");

    // GC is disabled: the call must coalesce-out (return false, no task) without
    // ever touching the in-flight guard.
    let flag = app.bucket_gc_in_flight().clone();
    assert!(
        !app.cleanup_stale_bucket_files_background(),
        "with GC disabled, cleanup must not launch a task (return false)"
    );
    assert!(
        !flag.load(Ordering::Acquire),
        "GC-disabled early return must not acquire the in-flight guard"
    );

    // Give any (erroneously) spawned task a chance to run, then assert survival.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        stale.exists(),
        "stale bucket file must SURVIVE cleanup when disable_bucket_gc = true"
    );
}

/// Companion control: with GC enabled (the default), the same stale,
/// unreferenced bucket file IS removed — proving the survival above is the
/// kill-switch's doing, not an inert no-op.
#[tokio::test(flavor = "multi_thread")]
async fn test_enabled_bucket_gc_removes_stale_files() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let app = build_test_app_with(tmp.path(), /* disable_bucket_gc */ false).await;

    assert!(
        app.bucket_gc_enabled(),
        "bucket_gc_enabled() must be true by default"
    );

    let stale = write_stale_bucket_file(&tmp.path().join("buckets"));
    assert!(stale.exists(), "precondition: stale file written");

    let flag = app.bucket_gc_in_flight().clone();
    assert!(
        app.cleanup_stale_bucket_files_background(),
        "with GC enabled, cleanup must launch a task (return true)"
    );

    // Wait for the detached GC task to finish (flag resets on completion).
    assert!(
        await_flag(&flag, false).await,
        "GC task must complete and reset the in-flight flag"
    );
    assert!(
        !stale.exists(),
        "unreferenced stale bucket file must be removed when GC is enabled"
    );
}
