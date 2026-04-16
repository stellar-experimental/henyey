//! Shared persist utilities for deferred I/O tasks.
//!
//! Both post-close and catchup paths need to flush bucket persist handles
//! and write to SQLite on background threads. This module consolidates
//! the common patterns to avoid duplication.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use henyey_bucket::HotArchiveBucket;
use henyey_ledger::LedgerManager;

/// Flush the pending bucket persist handle on a blocking thread.
///
/// Takes the pending persist handle from the bucket list (brief write lock),
/// then joins the background thread WITHOUT holding the lock. This prevents
/// blocking concurrent `bucket_list()` reads from `prepare_persist_data` on
/// the event loop.
pub(super) async fn flush_bucket_persist(ledger_manager: &Arc<LedgerManager>) {
    let pending_handle = ledger_manager.bucket_list_mut().take_pending_persist();
    if let Some(handle) = pending_handle {
        if let Err(e) = tokio::task::spawn_blocking(move || {
            handle
                .join()
                .expect("bucket persist thread panicked")
                .map_err(|e| format!("flush_pending_persist: {e}"))
        })
        .await
        .unwrap_or_else(|e| Err(format!("flush task panicked: {e}")))
        {
            fatal_persist_error("bucket flush", &e);
        }
    }
}

/// Persist hot archive buckets to disk, then flush the pending bucket persist.
///
/// Used by the post-close path where hot archive persist must happen on
/// the blocking thread (not on the event loop).
pub(super) async fn flush_hot_archive_and_buckets(
    ledger_manager: &Arc<LedgerManager>,
    bucket_dir: PathBuf,
) {
    let lm = ledger_manager.clone();
    let bd = bucket_dir;
    if let Err(e) = tokio::task::spawn_blocking(move || {
        // Persist hot archive buckets to disk.
        let habl_guard = lm.hot_archive_bucket_list();
        if let Some(habl) = habl_guard.as_ref() {
            persist_hot_archive_to_dir(habl.levels(), &bd)?;
        }
        drop(habl_guard);

        // Flush pending bucket persist (take-then-join without holding the lock).
        let pending_handle = lm.bucket_list_mut().take_pending_persist();
        if let Some(handle) = pending_handle {
            handle
                .join()
                .expect("bucket persist thread panicked")
                .map_err(|e| format!("flush_pending_persist: {e}"))?;
        }
        Ok(())
    })
    .await
    .unwrap_or_else(|e| Err(format!("flush task panicked: {e}")))
    {
        fatal_persist_error("hot archive + bucket flush", &e);
    }
}

/// Write hot archive bucket files to the bucket directory.
///
/// Iterates all levels and persists any in-memory buckets that don't
/// already have a backing file on disk. Returns an error if any bucket
/// file fails to write — the caller must not proceed to write HAS or
/// publish state that references missing bucket files.
fn persist_hot_archive_to_dir(
    levels: &[henyey_bucket::HotArchiveBucketLevel],
    bucket_dir: &Path,
) -> Result<(), String> {
    for level in levels {
        let mut buckets: Vec<&HotArchiveBucket> = vec![level.curr(), level.snap_bucket()];
        if let Some(next) = level.next() {
            buckets.push(next);
        }
        for bucket in buckets {
            if bucket.backing_file_path().is_none() && !bucket.hash().is_zero() {
                let path =
                    bucket_dir.join(henyey_bucket::canonical_bucket_filename(&bucket.hash()));
                if !path.exists() {
                    bucket.save_to_xdr_file(&path).map_err(|e| {
                        format!(
                            "Failed to persist hot archive bucket {} to disk: {}",
                            bucket.hash().to_hex(),
                            e
                        )
                    })?;
                }
            }
        }
    }
    Ok(())
}

/// Log a fatal persist error and abort the process.
///
/// All persist failures are unrecoverable — the node's on-disk state would
/// diverge from in-memory state, violating determinism guarantees.
pub(super) fn fatal_persist_error(context: &str, error: &dyn std::fmt::Display) -> ! {
    tracing::error!(context, error = %error, "Fatal persist failure, aborting");
    std::process::abort();
}
