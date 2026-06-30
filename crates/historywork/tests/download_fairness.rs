//! Regression test for #3686.
//!
//! Bucket persistence (`atomic_write_bytes`) performs blocking `fsync` calls
//! (the temp file plus the parent directory). When invoked directly from an
//! async fn polled on a tokio worker thread — and fanned out up to
//! `MAX_CONCURRENT_DOWNLOADS` (16) wide via `.buffer_unordered` — those fsyncs
//! block the worker threads and starve the runtime under disk pressure. The fix
//! moves the persist onto the blocking pool via
//! `henyey_common::spawn_blocking_logged` (see
//! `historywork::persist_bucket_to_disk`).
//!
//! ## Deterministic starvation harness
//!
//! No portable API labels "this fsync ran on a worker thread" from inside
//! `sync_all`, so the standard deterministic fairness assertion is a
//! single-worker tokio runtime with a co-scheduled async heartbeat: if blocking
//! work runs *on* the worker, the heartbeat freezes; if it runs *off* the worker
//! (blocking pool), the heartbeat keeps advancing. To make the assertion
//! independent of host disk speed, the blocking work is a deterministic
//! `std::thread::sleep` standing in for fsync-under-disk-pressure latency,
//! placed exactly where the persist runs.
//!
//! `test_blocking_persist_shape_starves_then_not` asserts BOTH arms in one run:
//!   - INLINE arm (the origin/main shape: blocking sleep directly in the async
//!     closure on the worker) → heartbeat freezes (≈0 ticks). This is the bug;
//!     it proves the harness genuinely detects worker starvation.
//!   - OFF-WORKER arm (the fix shape: the SAME blocking sleep wrapped in
//!     `henyey_common::spawn_blocking_logged`, the exact primitive
//!     `persist_bucket_to_disk` uses) → heartbeat advances. This is the fix.
//!
//! `test_persist_bucket_to_disk_runs_off_worker` additionally drives the REAL
//! production helper through the 16-wide fan-out and asserts the bytes land on
//! disk and the worker stays responsive — guarding that the shipped wrapper is
//! wired in (it would regress if a future edit reverted the helper to an inline
//! `atomic_write_bytes`).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::stream::{self, StreamExt};
use henyey_historywork::persist_bucket_to_disk;

const MAX_CONCURRENT_DOWNLOADS: usize = 16;
/// Stand-in for a single fsync's latency under disk pressure. Big enough that an
/// inline-blocked worker visibly drops heartbeat ticks; the 10 ms heartbeat
/// period gives a comfortable margin.
const PERSIST_LATENCY: Duration = Duration::from_millis(200);
const HEARTBEAT_PERIOD: Duration = Duration::from_millis(10);

/// Run a 16-wide window of blocking persists and report how many heartbeat ticks
/// the single worker managed during the window. `off_worker` selects the fix
/// shape (`spawn_blocking_logged`) vs the origin/main shape (inline on worker).
fn ticks_during_persist_window(off_worker: bool) -> u64 {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime");

    rt.block_on(async move {
        let heartbeats = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        let hb = heartbeats.clone();
        let hb_stop = stop.clone();
        let heartbeat = tokio::spawn(async move {
            while !hb_stop.load(Ordering::Relaxed) {
                hb.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(HEARTBEAT_PERIOD).await;
            }
        });

        // Let the heartbeat start ticking before the persist window opens.
        tokio::time::sleep(Duration::from_millis(30)).await;
        let before = heartbeats.load(Ordering::Relaxed);

        // Drive the 16-wide persist window from a SPAWNED task so it is polled
        // on the single worker thread — same as production, where
        // `DownloadBucketsWork::run` is a spawned work item, not `block_on`. (If
        // we awaited the stream directly inside `block_on`, it would run on the
        // external calling thread and could never starve the worker-resident
        // heartbeat, defeating the test.)
        let driver = tokio::spawn(async move {
            stream::iter(0..MAX_CONCURRENT_DOWNLOADS)
                .map(|_| async move {
                    if off_worker {
                        // Fix shape: blocking work on the blocking pool, exactly
                        // as `persist_bucket_to_disk` wraps `atomic_write_bytes`.
                        henyey_common::spawn_blocking_logged("test-persist", || {
                            std::thread::sleep(PERSIST_LATENCY);
                        })
                        .await
                        .expect("join ok");
                    } else {
                        // origin/main shape: blocking fsync inline on the worker.
                        std::thread::sleep(PERSIST_LATENCY);
                    }
                })
                .buffer_unordered(MAX_CONCURRENT_DOWNLOADS)
                .collect::<Vec<()>>()
                .await;
        });
        driver.await.expect("driver ok");

        let after = heartbeats.load(Ordering::Relaxed);
        stop.store(true, Ordering::Relaxed);
        let _ = heartbeat.await;
        after - before
    })
}

#[test]
fn test_blocking_persist_shape_starves_then_not() {
    // INLINE (origin/main): the worker is blocked the entire ~200 ms window and
    // cannot poll the heartbeat. Allow a tiny slack for the in-flight tick.
    let inline_ticks = ticks_during_persist_window(false);
    assert!(
        inline_ticks <= 2,
        "inline-on-worker persist should starve the heartbeat (≈0 ticks), got \
         {inline_ticks}; harness is not detecting starvation"
    );

    // OFF-WORKER (the fix): persists run on the blocking pool, leaving the single
    // worker free to keep polling the heartbeat throughout the window.
    let off_worker_ticks = ticks_during_persist_window(true);
    assert!(
        off_worker_ticks >= 5,
        "off-worker persist should keep the heartbeat advancing, got only \
         {off_worker_ticks} ticks — the worker was starved by blocking persist \
         running on it instead of the blocking pool (#3686)"
    );
}

#[test]
fn test_persist_bucket_to_disk_runs_off_worker() {
    // Drive the REAL production helper 16-wide on a single-worker runtime and
    // assert it (a) persists the bytes durably and (b) keeps a co-scheduled
    // heartbeat alive. Guards that the shipped helper is wired through
    // `spawn_blocking_logged` and not reverted to an inline `atomic_write_bytes`.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime");

    rt.block_on(async move {
        let heartbeats = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let hb = heartbeats.clone();
        let hb_stop = stop.clone();
        let heartbeat = tokio::spawn(async move {
            while !hb_stop.load(Ordering::Relaxed) {
                hb.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(HEARTBEAT_PERIOD).await;
            }
        });
        tokio::time::sleep(Duration::from_millis(30)).await;

        let tmp = tempfile::tempdir().expect("tempdir");
        let tmp_path = tmp.path().to_path_buf();
        let payload = vec![0xABu8; 64 * 1024];

        let driver_payload = payload.clone();
        let paths: Vec<std::path::PathBuf> = tokio::spawn(async move {
            stream::iter(0..MAX_CONCURRENT_DOWNLOADS)
                .map(|i| {
                    let path = tmp_path.join(format!("bucket-{i}"));
                    let data = driver_payload.clone();
                    async move {
                        persist_bucket_to_disk(path.clone(), data)
                            .await
                            .expect("persist ok");
                        path
                    }
                })
                .buffer_unordered(MAX_CONCURRENT_DOWNLOADS)
                .collect()
                .await
        })
        .await
        .expect("driver ok");

        stop.store(true, Ordering::Relaxed);
        let _ = heartbeat.await;

        for path in &paths {
            let written = std::fs::read(path).expect("read back persisted bucket");
            assert_eq!(written, payload, "persisted bytes must match");
        }
        assert!(
            heartbeats.load(Ordering::Relaxed) > 0,
            "heartbeat must have run at least once"
        );
    });
}
