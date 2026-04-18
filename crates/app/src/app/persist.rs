//! Shared persist utilities for deferred I/O tasks.
//!
//! Both post-close and catchup paths need to flush bucket persist handles
//! and write to SQLite on blocking threads. This module consolidates
//! the common patterns to avoid duplication.
//!
//! # Architecture
//!
//! All persist work runs through [`PersistJob::run_blocking`], a single
//! synchronous method that encapsulates the entire persist pipeline (bucket
//! flush, hot-archive file I/O, SQLite writes). The event loop dispatches
//! persist work via [`spawn_persist_task`], which wraps `run_blocking` in
//! a single `tokio::task::spawn_blocking` call and returns a
//! [`PendingPersist`] tracked in the select loop. The next ledger close
//! is gated on persist completion.
//!
//! This design avoids the nested `tokio::spawn(async { spawn_blocking })`
//! pattern that caused a 662-second deadlock on mainnet (#1735).

use std::path::Path;
use std::sync::Arc;

use henyey_bucket::HotArchiveBucket;
use henyey_db::Database;
use henyey_ledger::LedgerManager;

use super::types::PendingPersist;

/// Data needed to persist catchup state to SQLite after catchup completes.
///
/// Prepared inside `catchup_with_mode`, persisted on the event loop as a
/// [`PendingPersist`] task to avoid blocking inside `tokio::spawn`.
#[derive(Clone)]
pub struct CatchupPersistData {
    pub header: stellar_xdr::curr::LedgerHeader,
    pub header_xdr: Vec<u8>,
    pub has_json: String,
}

impl CatchupPersistData {
    /// Write catchup state to SQLite (header + HAS + last closed ledger).
    pub fn write_to_db(&self, db: &Database) -> Result<(), henyey_db::DbError> {
        use henyey_db::queries::*;
        db.transaction(|conn| {
            conn.store_ledger_header(&self.header, &self.header_xdr)?;
            conn.set_state(
                henyey_db::schema::state_keys::HISTORY_ARCHIVE_STATE,
                &self.has_json,
            )?;
            conn.set_last_closed_ledger(self.header.ledger_seq)?;
            Ok(())
        })
    }
}

/// How [`App::catchup_with_mode`] finalizes state after catchup completes.
///
/// This is a required argument — there is no "drop on the floor" option.
/// Construction is through [`CatchupFinalizer::inline`] (for top-level /
/// pre-event-loop callers) or the crate-private [`CatchupFinalizer::deferred`]
/// (for the runtime event-loop path that must not block inside `tokio::spawn`).
pub struct CatchupFinalizer(pub(super) CatchupFinalizerInner);

pub(super) enum CatchupFinalizerInner {
    /// Block on bucket flush + DB write before `catchup_with_mode` returns.
    /// Safe when not inside a `tokio::spawn` with a saturated blocking pool
    /// (e.g. CLI, `run_cmd::run_node` before `app.run()` is spawned).
    Inline {
        db: Database,
        ledger_manager: Arc<LedgerManager>,
    },
    /// Send persist data to the caller over a oneshot. The caller is
    /// responsible for driving the finalize on its own timeline (typically
    /// as a [`PersistJob::Catchup`] task in the event loop).
    Deferred(tokio::sync::oneshot::Sender<CatchupPersistData>),
}

impl CatchupFinalizer {
    /// Finalize catchup synchronously before returning.
    ///
    /// Uses `spawn_blocking` + `.await` internally, so the calling tokio
    /// worker yields while the blocking thread runs. Safe for top-level
    /// callers (CLI, `run_cmd::run_node` before `app.run()` is spawned)
    /// where the blocking pool is not saturated. Must not be used from
    /// inside the event loop's `select!` branches — use
    /// [`CatchupFinalizer::deferred`] there instead (see #1713, #1735).
    pub fn inline(db: Database, ledger_manager: Arc<LedgerManager>) -> Self {
        Self(CatchupFinalizerInner::Inline { db, ledger_manager })
    }

    /// Hand persist data off to the caller via a oneshot. The caller
    /// drives the actual persist on its own (e.g. via
    /// [`spawn_persist_task`] + [`PersistJob::Catchup`]).
    pub(crate) fn deferred(tx: tokio::sync::oneshot::Sender<CatchupPersistData>) -> Self {
        Self(CatchupFinalizerInner::Deferred(tx))
    }
}

/// How [`App::handle_close_complete`] finalizes post-close persistence.
///
/// Required argument — construction is compile-time mandatory so callers
/// cannot silently drop the [`PersistJob::LedgerClose`] handle. Mirrors
/// [`CatchupFinalizer`] for the ledger-close path (#1751 follow-up to #1749).
pub struct LedgerCloseFinalizer(pub(super) LedgerCloseFinalizerInner);

pub(super) enum LedgerCloseFinalizerInner {
    /// Drive persist to completion before `handle_close_complete` returns.
    /// Used by the manual-close path (admin HTTP + simulation) and the
    /// `try_apply_buffered_ledgers` test helper. Persist-task panics are
    /// silently discarded to preserve the prior `let _ = pt.handle.await`
    /// semantics at those sites.
    Inline,
    /// Hand the spawned [`PendingPersist`] back over a oneshot. Used by
    /// the event loop, which stores the handle in its local
    /// `pending_persist` slot and gates the next close on its completion.
    Deferred(tokio::sync::oneshot::Sender<PendingPersist>),
}

impl LedgerCloseFinalizer {
    /// Drive the persist task inline before returning.
    pub fn inline() -> Self {
        Self(LedgerCloseFinalizerInner::Inline)
    }

    /// Hand the [`PendingPersist`] back to the caller via a oneshot for
    /// event-loop-driven completion. Matches the send-failure tolerance
    /// of [`CatchupFinalizer::deferred`]: if the receiver was dropped
    /// (caller cancellation), the persist task runs detached and reports
    /// its own errors via [`fatal_persist_error`].
    pub(crate) fn deferred(tx: tokio::sync::oneshot::Sender<PendingPersist>) -> Self {
        Self(LedgerCloseFinalizerInner::Deferred(tx))
    }
}

/// Type alias for the boxed persist write function.
type PersistWriteFn = Box<dyn FnOnce(&Database) -> anyhow::Result<()> + Send>;

/// Describes the work to be done by a persist task.
///
/// Created by `handle_close_complete` (ledger close) or the catchup
/// completion handler, then dispatched via [`spawn_persist_task`] or
/// called directly via [`PersistJob::run_blocking`].
pub(super) enum PersistJob {
    /// Post-catchup: flush buckets + write catchup state to DB.
    Catchup {
        data: Box<CatchupPersistData>,
        db: Database,
        ledger_manager: Arc<LedgerManager>,
    },
    /// Post-close: flush hot archive + buckets + write full ledger data to DB,
    /// then optionally store LedgerCloseMeta for RPC.
    LedgerClose {
        /// Closure that writes the full ledger close data to SQLite.
        /// Boxed because `LedgerPersistData` is private to `ledger_close`.
        write_fn: PersistWriteFn,
        meta_xdr: Option<Vec<u8>>,
        db: Database,
        ledger_manager: Arc<LedgerManager>,
        bucket_dir: std::path::PathBuf,
    },
}

impl PersistJob {
    /// Run the entire persist pipeline synchronously on the calling thread.
    ///
    /// Every persist operation is blocking (file I/O, thread join, SQLite
    /// transaction). This is the single source of truth for all persist
    /// work — both the Deferred path (via [`spawn_persist_task`]) and the
    /// Inline path call this method. Any failure on the critical path
    /// aborts the process via [`fatal_persist_error`]; LedgerCloseMeta
    /// write failures are non-fatal (warned only).
    pub(super) fn run_blocking(self, ledger_seq: u32) {
        match self {
            PersistJob::Catchup {
                data,
                db,
                ledger_manager,
            } => {
                flush_bucket_persist_sync(&ledger_manager);

                if let Err(e) = data.write_to_db(&db) {
                    fatal_persist_error("catchup DB write", &e);
                }

                tracing::info!(ledger_seq, "Catchup persist completed");
            }
            PersistJob::LedgerClose {
                write_fn,
                meta_xdr,
                db,
                ledger_manager,
                bucket_dir,
            } => {
                flush_hot_archive_and_buckets_sync(&ledger_manager, &bucket_dir);

                if let Err(e) = write_fn(&db) {
                    fatal_persist_error("ledger close DB write", &e);
                }

                // LedgerCloseMeta for RPC (non-fatal).
                if let Some(meta) = meta_xdr {
                    if let Err(e) = db.store_ledger_close_meta(ledger_seq, &meta) {
                        tracing::warn!(
                            error = %e,
                            ledger_seq,
                            "Failed to persist LedgerCloseMeta"
                        );
                    }
                }
            }
        }
    }
}

/// Spawn a persist task on a blocking thread and return a [`PendingPersist`]
/// handle.
///
/// The task runs as a single `tokio::task::spawn_blocking` call that
/// executes [`PersistJob::run_blocking`] — all persist work (bucket flush,
/// hot-archive file I/O, SQLite writes) happens on one blocking thread
/// with no nested `spawn_blocking` calls. This avoids the deadlock pattern
/// from #1735 where `tokio::spawn(async { spawn_blocking })` nested
/// blocking-pool dispatch under pool saturation.
///
/// Cancellation note: `spawn_blocking` tasks are non-abortable — the
/// blocking thread runs to completion even if the handle is dropped.
/// This is acceptable because persist work must complete to maintain
/// on-disk/in-memory consistency, and no caller ever aborts the handle.
pub(super) fn spawn_persist_task(job: PersistJob, ledger_seq: u32) -> PendingPersist {
    let handle = tokio::task::spawn_blocking(move || job.run_blocking(ledger_seq));
    PendingPersist { handle, ledger_seq }
}

/// Flush the pending bucket persist handle synchronously.
///
/// Takes the pending persist handle from the bucket list (brief write lock),
/// then joins the background thread WITHOUT holding the lock. This prevents
/// blocking concurrent `bucket_list()` reads on the event loop.
fn flush_bucket_persist_sync(ledger_manager: &LedgerManager) {
    let pending_handle = ledger_manager.bucket_list_mut().take_pending_persist();
    if let Some(handle) = pending_handle {
        if let Err(e) = handle
            .join()
            .expect("bucket persist thread panicked")
            .map_err(|e| format!("flush_pending_persist: {e}"))
        {
            fatal_persist_error("bucket flush", &e);
        }
    }
}

/// Persist hot archive buckets to disk, then flush the pending bucket persist.
///
/// Used by the post-close path where hot archive persist and bucket flush
/// both run on the calling blocking thread.
fn flush_hot_archive_and_buckets_sync(ledger_manager: &LedgerManager, bucket_dir: &Path) {
    // Persist hot archive buckets to disk.
    let habl_guard = ledger_manager.hot_archive_bucket_list();
    if let Some(habl) = habl_guard.as_ref() {
        if let Err(e) = persist_hot_archive_to_dir(habl.levels(), bucket_dir) {
            fatal_persist_error("hot archive persist", &e);
        }
    }
    drop(habl_guard);

    // Flush pending bucket persist (take-then-join without holding the lock).
    flush_bucket_persist_sync(ledger_manager);
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

#[cfg(test)]
mod tests {
    use super::*;
    use henyey_db::queries::StateQueries;
    use stellar_xdr::curr::{Hash, LedgerHeader, LedgerHeaderExt, StellarValue, StellarValueExt};

    fn make_header(seq: u32) -> (LedgerHeader, Vec<u8>) {
        use stellar_xdr::curr::{LedgerHeaderExtensionV1, Limits, WriteXdr};
        let header = LedgerHeader {
            ledger_version: 24,
            previous_ledger_hash: Hash([0; 32]),
            scp_value: StellarValue {
                tx_set_hash: Hash([0; 32]),
                close_time: stellar_xdr::curr::TimePoint(0),
                upgrades: vec![].try_into().unwrap(),
                ext: StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash([0; 32]),
            bucket_list_hash: Hash([0; 32]),
            ledger_seq: seq,
            total_coins: 0,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 5_000_000,
            max_tx_set_size: 1000,
            skip_list: [Hash([0; 32]), Hash([0; 32]), Hash([0; 32]), Hash([0; 32])],
            ext: LedgerHeaderExt::V1(LedgerHeaderExtensionV1 {
                flags: 0,
                ext: stellar_xdr::curr::LedgerHeaderExtensionV1Ext::V0,
            }),
        };
        let xdr = header.to_xdr(Limits::none()).unwrap();
        (header, xdr)
    }

    /// Regression for #1749: `CatchupPersistData::write_to_db` must persist
    /// the header, HAS, and last_closed_ledger so that a fresh DB reopen
    /// (the horizon captive-core scenario: catchup → exit → run) observes
    /// the catchup's terminal state.
    #[test]
    fn write_to_db_persists_header_has_and_lcl() {
        let db = Database::open_in_memory().unwrap();
        let (header, header_xdr) = make_header(42);
        let persist = CatchupPersistData {
            header,
            header_xdr,
            has_json: "{\"version\":1}".to_string(),
        };

        persist.write_to_db(&db).unwrap();

        let lcl: u32 = db
            .with_connection(|c| c.get_last_closed_ledger())
            .unwrap()
            .unwrap();
        assert_eq!(lcl, 42, "LCL must be persisted to the DB");

        let has: Option<String> = db
            .with_connection(|c| c.get_state(henyey_db::schema::state_keys::HISTORY_ARCHIVE_STATE))
            .unwrap();
        assert_eq!(has.as_deref(), Some("{\"version\":1}"));
    }

    /// Shape-level regression for #1751: `LedgerCloseFinalizer` must be
    /// constructible via both `inline()` and `deferred(tx)` and must
    /// round-trip the correct inner variant. This is the API-surface
    /// invariant that prevents silent-drop regressions — any future
    /// caller of `handle_close_complete` must construct one of these
    /// two variants, which is what the type system enforces.
    #[test]
    fn ledger_close_finalizer_construction_and_variant_shape() {
        // Inline: unit variant.
        let inline = LedgerCloseFinalizer::inline();
        assert!(matches!(inline.0, LedgerCloseFinalizerInner::Inline));

        // Deferred: carries a oneshot::Sender<PendingPersist>.
        let (tx, _rx) = tokio::sync::oneshot::channel::<crate::app::types::PendingPersist>();
        let deferred = LedgerCloseFinalizer::deferred(tx);
        assert!(matches!(deferred.0, LedgerCloseFinalizerInner::Deferred(_)));
    }
}
