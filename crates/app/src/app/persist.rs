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
pub(super) struct CatchupPersistData {
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
    /// Send a ready-to-spawn persist job to the caller over a oneshot.
    /// The caller is responsible for calling `.spawn()` on the received
    /// [`CatchupPersistReady`] on its own timeline (typically from the
    /// event loop, where `spawn_blocking` is safe to call directly).
    Deferred {
        db: Database,
        ledger_manager: Arc<LedgerManager>,
        persist_tx: tokio::sync::oneshot::Sender<CatchupPersistReady>,
    },
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

    /// Send a ready-to-spawn [`CatchupPersistReady`] to the caller via a
    /// oneshot. The caller calls `.spawn()` to start the persist task on a
    /// blocking thread (e.g. from the event loop's select branches, where
    /// `spawn_blocking` is safe to call directly).
    pub(crate) fn deferred(
        db: Database,
        ledger_manager: Arc<LedgerManager>,
        persist_tx: tokio::sync::oneshot::Sender<CatchupPersistReady>,
    ) -> Self {
        Self(CatchupFinalizerInner::Deferred {
            db,
            ledger_manager,
            persist_tx,
        })
    }
}

/// Ready-to-spawn catchup persist job. Constructed inside `catchup_with_mode`
/// and sent through the `Deferred` finalizer's oneshot.
///
/// This is a **risk-reduction** measure — `#[must_use]` on both the type and
/// `.spawn()` makes silent drops produce compiler warnings, and private fields
/// prevent callers from destructuring around the safety layer. However,
/// `let _ = ready` still compiles; Rust's `#[must_use]` is advisory.
///
/// ## Send-failure semantics
///
/// If the oneshot receiver is dropped before the send (catchup task
/// cancellation), the `CatchupPersistReady` drops with it — no persist task
/// is spawned, no untracked work exists.
#[must_use = "catchup persist job must be spawned via .spawn()"]
pub(crate) struct CatchupPersistReady {
    job: PersistJob,
    ledger_seq: u32,
}

impl CatchupPersistReady {
    /// Construct from persist data + resources.
    ///
    /// `ledger_seq` is derived from `data.header.ledger_seq` to prevent
    /// divergence between the job's data and the tracked sequence number.
    pub(super) fn new(
        data: CatchupPersistData,
        db: Database,
        ledger_manager: Arc<LedgerManager>,
    ) -> Self {
        let (job, ledger_seq) = PersistJob::catchup(data, db, ledger_manager);
        Self { job, ledger_seq }
    }

    /// Spawn the persist job on a blocking thread.
    #[must_use = "the returned PendingPersist handle must be tracked"]
    pub(super) fn spawn(self) -> PendingPersist {
        spawn_persist_task(self.job, self.ledger_seq)
    }

    /// The ledger sequence being persisted (for logging/assertions).
    #[allow(dead_code)]
    pub(super) fn ledger_seq(&self) -> u32 {
        self.ledger_seq
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
    /// Construct a catchup persist job from the prepared data.
    ///
    /// Returns the job and the ledger sequence for logging/tracking.
    /// Used by both the Inline and Deferred finalization paths in
    /// `catchup_with_mode` to centralize `PersistJob::Catchup` construction.
    pub(super) fn catchup(
        data: CatchupPersistData,
        db: Database,
        ledger_manager: Arc<LedgerManager>,
    ) -> (Self, u32) {
        let seq = data.header.ledger_seq;
        (
            PersistJob::Catchup {
                data: Box::new(data),
                db,
                ledger_manager,
            },
            seq,
        )
    }

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

    /// Shape-level regression for #1750: `CatchupFinalizer::deferred` must
    /// produce the `Deferred` variant carrying `db`, `ledger_manager`, and
    /// `persist_tx`. This ensures the Deferred path has everything it needs
    /// to construct a `CatchupPersistReady` inside `catchup_with_mode`.
    #[test]
    fn catchup_finalizer_deferred_shape() {
        let db = Database::open_in_memory().unwrap();
        let lm = Arc::new(LedgerManager::new(
            "Test Network".to_string(),
            Default::default(),
        ));
        let (tx, _rx) = tokio::sync::oneshot::channel::<CatchupPersistReady>();
        let finalizer = CatchupFinalizer::deferred(db, lm, tx);
        assert!(matches!(
            finalizer.0,
            CatchupFinalizerInner::Deferred {
                db: _,
                ledger_manager: _,
                persist_tx: _,
            }
        ));
    }

    /// #1750: `CatchupPersistReady::new` derives `ledger_seq` from the
    /// persist data's header, and `.spawn()` returns a `PendingPersist`
    /// with the correct `ledger_seq`.
    #[tokio::test]
    async fn catchup_persist_ready_spawn_returns_correct_seq() {
        let db = Database::open_in_memory().unwrap();
        let lm = Arc::new(LedgerManager::new(
            "Test Network".to_string(),
            Default::default(),
        ));
        let (header, header_xdr) = make_header(99);
        let data = CatchupPersistData {
            header,
            header_xdr,
            has_json: "{}".to_string(),
        };
        let ready = CatchupPersistReady::new(data, db, lm);
        assert_eq!(ready.ledger_seq(), 99);
        let pending = ready.spawn();
        assert_eq!(pending.ledger_seq, 99);
        // Let the persist task complete (it will attempt write_to_db on
        // the in-memory DB and abort on failure, but the test tokio
        // runtime won't observe that — the task runs on a blocking thread).
        let _ = pending.handle.await;
    }

    /// #1750: when no persist data is produced (no-work catchup), the
    /// oneshot sender is dropped without sending. The receiver must
    /// observe `TryRecvError::Closed`.
    #[test]
    fn no_work_catchup_drops_sender() {
        let db = Database::open_in_memory().unwrap();
        let lm = Arc::new(LedgerManager::new(
            "Test Network".to_string(),
            Default::default(),
        ));
        let (tx, mut rx) = tokio::sync::oneshot::channel::<CatchupPersistReady>();
        // Construct the finalizer but never send on it (simulating
        // a no-work catchup that doesn't produce persist data).
        let _finalizer = CatchupFinalizer::deferred(db, lm, tx);
        drop(_finalizer);
        assert!(rx.try_recv().is_err());
    }

    /// #1750: `PendingCatchupResult::take_persist_ready` returns `Some`
    /// on the first call and `None` on subsequent calls.
    #[test]
    fn take_persist_ready_is_take_once() {
        let db = Database::open_in_memory().unwrap();
        let lm = Arc::new(LedgerManager::new(
            "Test Network".to_string(),
            Default::default(),
        ));
        let (header, header_xdr) = make_header(50);
        let data = CatchupPersistData {
            header,
            header_xdr,
            has_json: "{}".to_string(),
        };
        let ready = CatchupPersistReady::new(data, db, lm);
        let result_ok = Ok(crate::app::types::CatchupResult {
            ledger_seq: 50,
            ledger_hash: henyey_common::Hash256::default(),
            buckets_applied: 1,
            ledgers_replayed: 0,
        });
        let mut result = crate::app::types::PendingCatchupResult::new(result_ok, Some(ready));
        assert!(result.made_progress, "buckets_applied > 0 → made_progress");
        assert!(
            result.take_persist_ready().is_some(),
            "first take should be Some"
        );
        assert!(
            result.take_persist_ready().is_none(),
            "second take should be None"
        );
    }

    /// #1750: `PendingCatchupResult::new` derives `made_progress` correctly.
    #[test]
    fn pending_catchup_result_derives_made_progress() {
        // Error → no progress
        let err_result =
            crate::app::types::PendingCatchupResult::new(Err(anyhow::anyhow!("test error")), None);
        assert!(!err_result.made_progress);

        // Success with no work → no progress
        let no_work = crate::app::types::PendingCatchupResult::new(
            Ok(crate::app::types::CatchupResult {
                ledger_seq: 1,
                ledger_hash: henyey_common::Hash256::default(),
                buckets_applied: 0,
                ledgers_replayed: 0,
            }),
            None,
        );
        assert!(!no_work.made_progress);

        // Success with ledgers replayed → progress
        let with_progress = crate::app::types::PendingCatchupResult::new(
            Ok(crate::app::types::CatchupResult {
                ledger_seq: 10,
                ledger_hash: henyey_common::Hash256::default(),
                buckets_applied: 0,
                ledgers_replayed: 5,
            }),
            None,
        );
        assert!(with_progress.made_progress);
    }

    /// Regression test for #1735 / commit 48d878b8.
    ///
    /// The old `spawn_persist_task` used `tokio::spawn(async { ... })` with
    /// multiple sequential `spawn_blocking` calls inside — one for hot-archive
    /// + bucket flush, one for the DB write, and optionally one for
    /// LedgerCloseMeta. Under blocking-pool saturation, each `spawn_blocking`
    /// independently competes for a pool slot, multiplying queueing latency.
    ///
    /// The fix consolidated all persist work into `PersistJob::run_blocking`,
    /// dispatched via a single `tokio::task::spawn_blocking`. This test
    /// exercises that path under a saturated blocking pool (1 thread, fully
    /// occupied) and verifies the persist completes once the slot frees up,
    /// with correct DB side effects.
    ///
    /// # Architectural invariant
    ///
    /// `spawn_persist_task` must use exactly one `spawn_blocking` call that
    /// runs the entire pipeline synchronously. `run_blocking` is a synchronous
    /// `fn` (not `async fn`), which makes nested `spawn_blocking` unnatural
    /// but not impossible — the companion test
    /// [`test_multi_spawn_blocking_stalls_with_interleaved_contention`]
    /// provides the runtime discriminator.
    ///
    /// # Flush paths
    ///
    /// The test uses a LedgerManager with no pending bucket persist and no
    /// hot archive, so `flush_hot_archive_and_buckets_sync` and
    /// `flush_bucket_persist_sync` are no-ops. This is intentional: the
    /// regression target is the dispatch pattern (single vs. multiple
    /// `spawn_blocking`), not the flush logic itself. Testing real flushes
    /// requires bucket infrastructure that is out of scope here.
    #[test]
    fn test_spawn_persist_task_completes_under_pool_saturation() {
        use std::sync::{mpsc, Barrier};

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            // -- Phase 1: saturate the single blocking thread --
            let started_signal = {
                let (tx, rx) = mpsc::channel::<()>();
                let release_barrier = Arc::new(Barrier::new(2));
                let barrier_clone = release_barrier.clone();

                tokio::task::spawn_blocking(move || {
                    tx.send(()).unwrap();
                    barrier_clone.wait();
                });

                // Wait for the blocker to actually start (owns the slot).
                rx.recv().unwrap();

                release_barrier
            };

            // -- Phase 2: queue a persist job (LedgerClose path) --
            let db = Database::open_in_memory().unwrap();
            let lm = Arc::new(LedgerManager::new(
                "Test Network ; September 2015".to_string(),
                Default::default(),
            ));
            let bucket_dir = tempfile::tempdir().unwrap();

            let test_seq: u32 = 77;
            let test_meta = vec![0xCA, 0xFE];
            let (header, header_xdr) = make_header(test_seq);

            // Clone db for the write_fn closure (it receives &Database from
            // run_blocking, but we need our handle for assertions later).
            let db_for_write = db.clone();
            let write_fn: Box<dyn FnOnce(&Database) -> anyhow::Result<()> + Send> =
                Box::new(move |db| {
                    use henyey_db::queries::*;
                    db.with_connection(|conn| {
                        conn.store_ledger_header(&header, &header_xdr)?;
                        conn.set_last_closed_ledger(test_seq)?;
                        Ok(())
                    })
                    .map_err(|e| anyhow::anyhow!(e))
                });

            let job = PersistJob::LedgerClose {
                write_fn,
                meta_xdr: Some(test_meta.clone()),
                db: db_for_write,
                ledger_manager: lm,
                bucket_dir: bucket_dir.path().to_path_buf(),
            };

            let pending = spawn_persist_task(job, test_seq);

            // -- Phase 3: release the blocker from a thread (not async) --
            std::thread::spawn(move || {
                started_signal.wait();
            });

            // -- Phase 4: assert completion with timeout --
            let result =
                tokio::time::timeout(std::time::Duration::from_secs(10), pending.handle).await;
            assert!(
                result.is_ok(),
                "persist task must complete within 10s once blocking slot frees"
            );
            result.unwrap().expect("persist task must not panic");

            // -- Phase 5: verify DB side effects --
            let lcl: u32 = db
                .with_connection(|c| c.get_last_closed_ledger())
                .unwrap()
                .unwrap();
            assert_eq!(lcl, test_seq, "LCL must be persisted");

            let meta = db
                .with_connection(|c| {
                    use henyey_db::queries::LedgerCloseMetaQueries;
                    c.load_ledger_close_meta(test_seq)
                })
                .unwrap();
            assert_eq!(
                meta.as_deref(),
                Some(test_meta.as_slice()),
                "LedgerCloseMeta must be persisted"
            );
        });
    }

    /// Discriminator test: proves the pre-fix multi-`spawn_blocking` pattern
    /// stalls when an interloper task grabs the blocking slot between steps.
    ///
    /// With `max_blocking_threads(1)`, the blocking-pool queue is roughly
    /// FIFO. The old persist pattern queued step 1, then after step 1
    /// completed and the async task resumed, queued step 2. An interloper
    /// task queued between steps 1 and 2 gets the slot before step 2,
    /// stalling the pipeline. The current single-`spawn_blocking` design
    /// runs all work atomically on one thread, so no interloper can
    /// interleave.
    ///
    /// This test exercises the old pattern directly: if someone reverts
    /// `spawn_persist_task` to use multiple sequential `spawn_blocking`
    /// calls, this test would also need updating (making the regression
    /// visible in code review).
    #[test]
    fn test_multi_spawn_blocking_stalls_with_interleaved_contention() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{mpsc, Barrier};

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            // Phase 1: saturate the single blocking thread.
            let (blocker_started_tx, blocker_started_rx) = mpsc::channel::<()>();
            let blocker_release = Arc::new(Barrier::new(2));
            let br = blocker_release.clone();
            tokio::task::spawn_blocking(move || {
                blocker_started_tx.send(()).unwrap();
                br.wait();
            });
            blocker_started_rx.recv().unwrap();

            // Phase 2: queue the old pattern — 2 sequential spawn_blocking
            // calls inside tokio::spawn. Step 1 is queued immediately;
            // step 2 is queued only after step 1 completes and the async
            // task resumes.
            let step_counter = Arc::new(AtomicUsize::new(0));
            let sc = step_counter.clone();
            let (step1_queued_tx, step1_queued_rx) = tokio::sync::oneshot::channel::<()>();
            let mut old_pattern = tokio::spawn(async move {
                let sc1 = sc.clone();
                let h = tokio::task::spawn_blocking(move || {
                    sc1.fetch_add(1, Ordering::SeqCst);
                });
                // Signal that step 1 is queued before awaiting it.
                let _ = step1_queued_tx.send(());
                h.await.unwrap();
                // Step 2: queued AFTER step 1 completes.
                let sc2 = sc;
                tokio::task::spawn_blocking(move || {
                    sc2.fetch_add(1, Ordering::SeqCst);
                })
                .await
                .unwrap();
            });

            // Wait for step 1 to be queued, then queue the interloper.
            step1_queued_rx.await.unwrap();

            // Phase 3: queue an interloper that blocks on its own barrier.
            // Queue order is now: [blocker(running), step1, interloper].
            // After blocker releases → step1 runs → interloper runs (before
            // step2, which hasn't been queued yet).
            let (interloper_started_tx, interloper_started_rx) = mpsc::channel::<()>();
            let interloper_release = Arc::new(Barrier::new(2));
            let ir = interloper_release.clone();
            tokio::task::spawn_blocking(move || {
                interloper_started_tx.send(()).unwrap();
                ir.wait();
            });

            // Phase 4: release the initial blocker.
            std::thread::spawn(move || {
                blocker_release.wait();
            });

            // Wait for the interloper to actually start (it holds the slot).
            interloper_started_rx.recv().unwrap();

            // Step 1 ran, step 2 is stuck behind the interloper.
            assert_eq!(
                step_counter.load(Ordering::SeqCst),
                1,
                "step 1 completed but step 2 must be blocked by the interloper"
            );

            // Phase 5: the old pattern must NOT complete while interloper
            // holds the slot — this is the discriminator.
            let timed_out =
                tokio::time::timeout(std::time::Duration::from_millis(200), &mut old_pattern)
                    .await
                    .is_err();
            assert!(
                timed_out,
                "multi-spawn_blocking pattern must stall when interloper holds \
                 the slot between steps — this is the pre-fix regression scenario"
            );

            // Phase 6: release interloper, let step 2 complete.
            std::thread::spawn(move || {
                interloper_release.wait();
            });
            old_pattern.await.unwrap();
            assert_eq!(
                step_counter.load(Ordering::SeqCst),
                2,
                "both steps must complete after interloper releases"
            );
        });
    }
}
