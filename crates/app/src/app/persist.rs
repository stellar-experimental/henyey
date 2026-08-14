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

use henyey_bucket::{BucketError, HotArchiveBucket};
use henyey_db::Database;
use henyey_history::{checkpoint_ledger, is_checkpoint_ledger, GENESIS_LEDGER_SEQ};
use henyey_ledger::LedgerManager;

use super::types::PendingPersist;

/// Handle that lets a detached persist task request a **clean, recoverable**
/// process shutdown on a transient environmental IO failure (ENOSPC/EDQUOT),
/// instead of `std::process::abort()` (#3478).
///
/// Persist work runs on a detached blocking thread with no `App` reference, so
/// the shutdown signal is threaded in via this thin `Clone` handle (wrapping
/// the app's broadcast shutdown sender). The semantics mirror
/// [`App::trigger_recoverable_shutdown`](crate::App::trigger_recoverable_shutdown):
/// a clean shutdown WITHOUT `fatal_wipe_required` and WITHOUT setting
/// `fatal_state_failure` — the on-disk state is intact and the operator just
/// frees space and restarts.
#[derive(Clone)]
pub(crate) struct RecoverableShutdownHandle {
    shutdown_tx: tokio::sync::broadcast::Sender<()>,
}

impl RecoverableShutdownHandle {
    pub(crate) fn new(shutdown_tx: tokio::sync::broadcast::Sender<()>) -> Self {
        Self { shutdown_tx }
    }

    /// Emit the recoverable-shutdown log (no `fatal_wipe_required`) and signal
    /// the main loop to exit cleanly.
    fn trigger(&self, context: &str, error: &dyn std::fmt::Display) {
        tracing::error!(
            context,
            error = %error,
            "RECOVERABLE: transient local IO failure during persist. \
             Node will shut down cleanly. Free disk space and restart; \
             no state wipe required (on-disk state is intact)."
        );
        let _ = self.shutdown_tx.send(());
    }
}

/// Data needed to persist catchup state to SQLite after catchup completes.
///
/// Prepared inside `catchup_with_mode`, persisted on the event loop as a
/// [`PendingPersist`] task to avoid blocking inside `tokio::spawn`.
#[derive(Clone)]
pub(super) struct CatchupPersistData {
    pub header: stellar_xdr::LedgerHeader,
    pub header_xdr: Vec<u8>,
    pub has_json: String,
    /// Whether checkpoint publishing is enabled for this run. Drives the
    /// `skipFirstCheckpointSinceItIsIncomplete` parity logic: when true
    /// and the catchup terminus is mid-checkpoint, a marker is persisted
    /// so the first post-catchup checkpoint close skips its
    /// `enqueue_publish` (it would otherwise publish an incomplete
    /// checkpoint, since ledgers prior to the catchup LCL are absent
    /// from the local DB).
    pub publish_enabled: bool,
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
            // Clear the deferred-persist sentinel (AUDIT-226 / §14.5). This
            // runs atomically with the state update, closing the second crash
            // window. A crash before this transaction leaves the sentinel set
            // for startup detection via `check_catchup_persist_pending`.
            conn.delete_state(henyey_db::schema::state_keys::CATCHUP_PERSIST_PENDING)?;

            // stellar-core parity: `skipFirstCheckpointSinceItIsIncomplete`.
            // If publish is enabled and the catchup terminus is mid-checkpoint,
            // record the target checkpoint seq so the next ledger close at
            // that checkpoint suppresses its publish-queue enqueue. The
            // set-or-delete pattern guarantees we never leak a stale marker
            // from a prior run: every code path through `write_to_db` either
            // sets the key to a fresh value or deletes it outright.
            let lcl = self.header.ledger_seq;
            if self.publish_enabled && lcl > GENESIS_LEDGER_SEQ && !is_checkpoint_ledger(lcl) {
                conn.set_state(
                    henyey_db::schema::state_keys::PUBLISH_SKIP_FIRST_CHECKPOINT,
                    &checkpoint_ledger(lcl).to_string(),
                )?;
            } else {
                conn.delete_state(henyey_db::schema::state_keys::PUBLISH_SKIP_FIRST_CHECKPOINT)?;
            }
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
        shutdown: RecoverableShutdownHandle,
    ) -> Self {
        let (job, ledger_seq) = PersistJob::catchup(data, db, ledger_manager, shutdown);
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
///
/// `Fn` (not `FnOnce`) so the ledger-close commit can be re-invoked by the
/// bounded retry-with-backoff on a transient SQLite busy (#3640). The
/// production closure (`move |db| data.serialize_and_write_to_db(db)`) only
/// borrows its captured `data` via `&self`, so it is naturally re-callable; the
/// underlying commit is a single atomic SQLite transaction (all-or-nothing), so
/// re-running it after a busy that never committed is safe.
type PersistWriteFn = Box<dyn Fn(&Database) -> anyhow::Result<()> + Send>;

/// Describes the work to be done by a persist task.
///
/// Created by `handle_close_complete` (ledger close) or the catchup
/// completion handler, then dispatched via [`spawn_persist_task`].
pub(super) enum PersistJob {
    /// Post-catchup: flush buckets + write catchup state to DB.
    Catchup {
        data: Box<CatchupPersistData>,
        db: Database,
        ledger_manager: Arc<LedgerManager>,
        /// Clean recoverable shutdown on transient-IO persist failure (#3478).
        shutdown: RecoverableShutdownHandle,
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
        /// Clean recoverable shutdown on transient-IO persist failure (#3478).
        shutdown: RecoverableShutdownHandle,
    },
}

impl PersistJob {
    /// Construct a catchup persist job from the prepared data.
    ///
    /// Returns the job and the ledger sequence for logging/tracking.
    /// Called only by [`CatchupPersistReady::new`].
    fn catchup(
        data: CatchupPersistData,
        db: Database,
        ledger_manager: Arc<LedgerManager>,
        shutdown: RecoverableShutdownHandle,
    ) -> (Self, u32) {
        let seq = data.header.ledger_seq;
        (
            PersistJob::Catchup {
                data: Box::new(data),
                db,
                ledger_manager,
                shutdown,
            },
            seq,
        )
    }

    /// Run the entire persist pipeline synchronously on the calling thread.
    ///
    /// Every persist operation is blocking (file I/O, thread join, SQLite
    /// transaction). This is the single source of truth for all persist
    /// work — called only by [`spawn_persist_task`]. Any failure on the
    /// critical path aborts the process via [`fatal_persist_error`];
    /// LedgerCloseMeta write failures are non-fatal (warned only).
    fn run_blocking(self, ledger_seq: u32) {
        match self {
            PersistJob::Catchup {
                data,
                db,
                ledger_manager,
                shutdown,
            } => {
                flush_bucket_persist_sync(&ledger_manager, &shutdown);

                // #3640: bounded retry-with-backoff around the commit so a
                // *transient* SQLite busy/locked is retried briefly before
                // escalating. #3497: a persistent transient busy still routes
                // to a clean recoverable shutdown (no wipe); a non-transient
                // error aborts immediately (no retry consumed).
                if let PersistOutcome::Fatal(e) = commit_with_busy_retry(
                    "catchup DB write",
                    crate::metrics::SITE_CATCHUP_PERSIST,
                    || data.write_to_db(&db),
                    is_transient_db_busy,
                    &shutdown,
                ) {
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
                shutdown,
            } => {
                let persist_start = std::time::Instant::now();

                // LEDGER_SPEC §4.11 Step 3 (#3066): "finalize checkpoint files".
                // Henyey finalizes the on-disk bucket and hot-archive files via
                // this flush, which runs *before* the DB write below so the
                // bucket files referenced by the about-to-be-committed HAS are
                // durable on disk first. (The publish-queue enqueue itself, the
                // §4.11 Step 1 "queue checkpoint", is co-transactional inside
                // `write_fn` — see `LedgerPersistInputs::serialize_and_write_to_db`.)
                flush_hot_archive_and_buckets_sync(&ledger_manager, &bucket_dir, &shutdown);

                // LEDGER_SPEC §4.11 Step 2 (#3066): "commit LedgerTxn". This is
                // the single atomic SQLite transaction that co-writes header,
                // HAS, and LCL (the INV-L13 agreement guarantee). Ordering is
                // unchanged by #3066; this comment is traceability only.
                // #3640: bounded retry-with-backoff around the commit (see
                // `commit_with_busy_retry`). #3497: `write_fn` returns anyhow,
                // but `db.transaction(..)?` preserves the typed `DbError`
                // through `From`, so a transient SQLite busy/locked is
                // recoverable via downcast → retry, then clean no-wipe shutdown
                // on budget exhaustion; a non-transient error aborts (unchanged,
                // no retry consumed).
                if let PersistOutcome::Fatal(e) = commit_with_busy_retry(
                    "ledger close DB write",
                    crate::metrics::SITE_LEDGER_CLOSE_PERSIST,
                    || write_fn(&db),
                    is_transient_db_busy_anyhow,
                    &shutdown,
                ) {
                    fatal_persist_error("ledger close DB write", &e);
                }

                // LedgerCloseMeta for RPC (non-fatal).
                if let Some(meta) = meta_xdr {
                    if let Err(e) = db.store_ledger_close_meta(ledger_seq, &meta) {
                        // #3802: this arm is log-and-continue — the row is
                        // permanently lost and RPC sees a hole. Count the
                        // transient-busy subset so the loss is Prometheus-
                        // visible instead of log-archaeology only. Behaviour
                        // (including the warn! below) is unchanged.
                        crate::metrics::record_db_busy_drop_if_transient(
                            crate::metrics::SITE_LEDGER_CLOSE_META,
                            &e,
                        );
                        tracing::warn!(
                            error = %e,
                            ledger_seq,
                            "Failed to persist LedgerCloseMeta"
                        );
                    }
                }

                crate::metrics::PERSIST_LEDGER_CLOSE_SECONDS
                    .record(persist_start.elapsed().as_secs_f64());
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
    let dispatch_time = std::time::Instant::now();
    let handle = tokio::task::spawn_blocking(move || job.run_blocking(ledger_seq));
    PendingPersist {
        handle,
        ledger_seq,
        dispatch_time,
    }
}

/// Flush the pending bucket persist handle synchronously.
///
/// Takes the pending persist handle from the bucket list (brief write lock),
/// then joins the background thread WITHOUT holding the lock. This prevents
/// blocking concurrent `bucket_list()` reads on the event loop.
fn flush_bucket_persist_sync(ledger_manager: &LedgerManager, shutdown: &RecoverableShutdownHandle) {
    let pending_handle = ledger_manager.bucket_list_mut().take_pending_persist();
    if let Some(handle) = pending_handle {
        if let Err(e) = handle.join().expect("bucket persist thread panicked") {
            // `e` is a classified `MergeError` (errno preserved through the
            // persist thread), re-materialized to a classified `BucketError`
            // so a transient ENOSPC routes to a clean recoverable shutdown
            // rather than `abort()` (#3478).
            handle_persist_error("bucket flush", &e.to_bucket_error(), shutdown);
        }
    }
}

/// Persist hot archive buckets to disk, then flush the pending bucket persist.
///
/// Used by the post-close path where hot archive persist and bucket flush
/// both run on the calling blocking thread.
fn flush_hot_archive_and_buckets_sync(
    ledger_manager: &LedgerManager,
    bucket_dir: &Path,
    shutdown: &RecoverableShutdownHandle,
) {
    // Persist hot archive buckets to disk.
    let habl_guard = ledger_manager.hot_archive_bucket_list();
    if let Some(habl) = habl_guard.as_ref() {
        if let Err(e) = persist_hot_archive_to_dir(habl.levels(), bucket_dir) {
            handle_persist_error("hot archive persist", &e, shutdown);
        }
    }
    drop(habl_guard);

    // Flush pending bucket persist (take-then-join without holding the lock).
    flush_bucket_persist_sync(ledger_manager, shutdown);
}

/// Write hot archive bucket files to the bucket directory.
///
/// Iterates all levels and persists any in-memory buckets that don't
/// already have a backing file on disk. Returns an error if any bucket
/// file fails to write — the caller must not proceed to write HAS or
/// publish state that references missing bucket files.
///
/// Returns the structured [`BucketError`] (preserving the `errno`) so the
/// caller can distinguish a transient ENOSPC from genuine corruption (#3478).
fn persist_hot_archive_to_dir(
    levels: &[henyey_bucket::HotArchiveBucketLevel],
    bucket_dir: &Path,
) -> Result<(), BucketError> {
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
                    bucket.save_to_xdr_file(&path)?;
                }
            }
        }
    }
    Ok(())
}

/// Route a persist failure to the correct shutdown path based on its
/// classification (#3478).
///
/// - **Transient IO** (ENOSPC/EDQUOT): environmental, no partial state
///   committed → a clean recoverable shutdown WITHOUT a state wipe. Matches
///   stellar-core, which throws `POSSIBLY_CORRUPTED_LOCAL_FS` ("ensure enough
///   space") → clean exit; operator frees space + restarts; no wipe.
/// - **Anything else** (corruption / non-free-space IO): keeps the existing
///   `abort()` behavior — the node's on-disk state may diverge from in-memory
///   state, violating determinism guarantees.
fn handle_persist_error(context: &str, error: &BucketError, shutdown: &RecoverableShutdownHandle) {
    if error.is_transient_io() {
        shutdown.trigger(context, error);
    } else {
        fatal_persist_error(context, error);
    }
}

/// Returns `true` iff the DB error is a transient SQLite busy/locked
/// (`SQLITE_BUSY` / `SQLITE_LOCKED`, "database is locked"), #3497.
///
/// SQLite write transactions are atomic: a `DatabaseBusy`/`DatabaseLocked`
/// means the transaction NEVER committed, so the on-disk state is consistent
/// (one ledger behind) and a plain restart recovers it cleanly. This is the
/// recoverable, environmental class — analogous to the bucket-IO ENOSPC class
/// handled by [`BucketError::is_transient_io`] (#3478).
///
/// Delegates to the canonical [`henyey_db::DbError::is_transient_busy`] so
/// exactly one definition of the (NARROW, consensus-safety-critical) busy/locked
/// predicate exists in the workspace (#3806 / #3871). Kept as a `pub(crate)`
/// free function so the ~20 call sites and `crate::metrics` need not change.
pub(crate) fn is_transient_db_busy(error: &henyey_db::DbError) -> bool {
    error.is_transient_busy()
}

/// `&anyhow::Error` sibling of [`is_transient_db_busy`] for the ledger-close
/// write path (#3497).
///
/// The ledger-close `write_fn` returns `anyhow::Result<()>`, but
/// `db.transaction(...)?` propagates the typed [`DbError`] through
/// `From<DbError> for anyhow::Error`, so the structured rusqlite code survives
/// and can be recovered via `downcast_ref::<DbError>()` — NO string-matching
/// on the rendered "database is locked" message. A non-`DbError` anyhow error
/// (downcast fails) is NOT transient → stays fatal.
///
/// `pub(crate)` (re-exported from `crate::app`) because `crate::metrics`
/// consumes it for the busy-drop telemetry helpers (#3802). `pub(super)` would
/// NOT reach there: `mod persist` is a private module of `mod app`, so
/// `pub(super)` == `pub(in crate::app)`, which excludes `crate::metrics`.
pub(crate) fn is_transient_db_busy_anyhow(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<henyey_db::DbError>()
        .is_some_and(is_transient_db_busy)
}

/// Bounded retry policy for a consensus-critical persist DB commit (#3640).
///
/// Total attempts: 1 initial + 3 retries = `MAX_PERSIST_ATTEMPTS`.
const MAX_PERSIST_ATTEMPTS: u32 = 4;

/// Backoff sleeps *between* attempts (so `MAX_PERSIST_ATTEMPTS - 1` entries).
/// 50ms / 200ms / 500ms — total added backoff ≈ 0.75s in the persistent case.
const PERSIST_RETRY_BACKOFF: [std::time::Duration; (MAX_PERSIST_ATTEMPTS - 1) as usize] = [
    std::time::Duration::from_millis(50),
    std::time::Duration::from_millis(200),
    std::time::Duration::from_millis(500),
];

/// Outcome of [`commit_with_busy_retry`] — lets the caller route the
/// non-transient (fatal) case to [`fatal_persist_error`] WITHOUT the retry
/// helper itself calling `abort()`, which keeps the loop unit-testable.
#[derive(Debug)]
enum PersistOutcome<E> {
    /// The commit succeeded (possibly after one or more retries).
    Survived,
    /// A transient busy persisted past the retry bound; the recoverable
    /// shutdown was already signalled by this helper.
    RecoverableShutdown,
    /// The error is NOT a transient busy — the caller must route this to
    /// [`fatal_persist_error`] (the helper never aborts).
    Fatal(E),
}

/// Render the SQLite extended error code from a typed [`henyey_db::DbError`],
/// for the retry/shutdown diagnostic log line (#3640). Distinguishes
/// `SQLITE_BUSY` (5) / `SQLITE_BUSY_SNAPSHOT` (517) / `SQLITE_LOCKED` (6) so the
/// next incident captures the exact class. Returns `None` for non-SQLite errors.
fn sqlite_extended_code(error: &henyey_db::DbError) -> Option<i32> {
    match error {
        henyey_db::DbError::Sqlite(rusqlite::Error::SqliteFailure(e, _)) => Some(e.extended_code),
        _ => None,
    }
}

/// `anyhow` sibling of [`sqlite_extended_code`] (ledger-close `write_fn` path).
fn sqlite_extended_code_anyhow(error: &anyhow::Error) -> Option<i32> {
    error
        .downcast_ref::<henyey_db::DbError>()
        .and_then(sqlite_extended_code)
}

/// Bounded retry-with-backoff around a consensus-critical persist DB commit
/// (#3640).
///
/// A *transient* `SQLITE_BUSY`/`database is locked` on the ledger-close (or
/// catchup) commit must not instantly halt the node: this wrapper retries the
/// commit up to [`MAX_PERSIST_ATTEMPTS`] times with [`PERSIST_RETRY_BACKOFF`]
/// sleeps before falling back to the existing clean recoverable shutdown
/// (#3497, no wipe). Classification is delegated to `is_transient` (the
/// existing [`is_transient_db_busy`] / [`is_transient_db_busy_anyhow`] gates),
/// so genuine corruption can NEVER be retried into a wipe-deferral — a
/// non-transient error short-circuits to [`PersistOutcome::Fatal`] after a
/// single attempt for the caller to route to [`fatal_persist_error`].
///
/// ## Threading / latency
///
/// The ONLY caller is [`PersistJob::run_blocking`], which runs on the
/// `spawn_blocking` persist thread (`spawn_persist_task`), NOT the event loop —
/// so the blocking `std::thread::sleep` backoff is safe and never freezes the
/// `select!` loop. The next ledger close is already gated on persist
/// completion, so the small added latency only delays the (already-failing)
/// close; it reorders nothing.
///
/// Each attempt internally waits up to SQLite `busy_timeout=30s`, so a transient
/// spike that clears in <1s succeeds on attempt 2 with ~50ms added (the common
/// case). A *persistent* busy stalls ≈ 4×30s + 0.75s ≈ 120s before the clean
/// shutdown — the deliberate, accepted worst case (the node was going to shut
/// down anyway; we trade ≤2min for surviving the far-more-common transient
/// spike). This effective tolerance (~120s) is intentionally far beyond
/// stellar-core's single 10s `busy_timeout` window: henyey is deliberately more
/// graceful (core simply crashes on busy_timeout exhaustion) and this divergence
/// is consensus-neutral (it changes only failure-handling).
///
/// ## Deliberate exclusion
///
/// Only the two persist commits that gate consensus continuation
/// (ledger-close + catchup) are retried. The publish-queue dequeue
/// (`remove_publish`, warn-only) and the SCP tx-set purge (best-effort) are
/// intentionally NOT retried — they do not halt consensus.
///
/// ## Telemetry (#3802)
///
/// `site` is a **stable Prometheus label value** from the closed
/// `crate::metrics::DB_BUSY_SITES` vocabulary — deliberately a separate
/// parameter from `context`, which is a human-readable log string that someone
/// will eventually rephrase. Deriving the label from `context` would silently
/// rename a production series on a log-wording edit.
fn commit_with_busy_retry<E: std::fmt::Display>(
    context: &str,
    site: &'static str,
    mut attempt: impl FnMut() -> Result<(), E>,
    is_transient: impl Fn(&E) -> bool,
    shutdown: &RecoverableShutdownHandle,
) -> PersistOutcome<E>
where
    E: ExtendedSqliteCode,
{
    for n in 1..=MAX_PERSIST_ATTEMPTS {
        match attempt() {
            Ok(()) => {
                if n > 1 {
                    tracing::info!(
                        context,
                        attempt = n,
                        "Persist DB commit succeeded after retrying a transient SQLite busy"
                    );
                }
                return PersistOutcome::Survived;
            }
            Err(e) => {
                if !is_transient(&e) {
                    // Non-transient (corruption/integrity/non-free-space IO):
                    // never retried — hand back to the caller for the fatal path.
                    return PersistOutcome::Fatal(e);
                }
                if n < MAX_PERSIST_ATTEMPTS {
                    // #3640 watch signal — kept, unchanged, still emitted. It
                    // is the one series with continuous scraped history for the
                    // 2026-06-26 mainnet outage, so it is NOT superseded away.
                    // The deliberate double-count against the #3802 sibling
                    // below is documented in both HELP strings.
                    crate::metrics::DB_BUSY_RETRY_TOTAL.increment(1);
                    crate::metrics::record_db_busy_retry_attempt(site);
                    let backoff = PERSIST_RETRY_BACKOFF[(n - 1) as usize];
                    tracing::warn!(
                        context,
                        attempt = n,
                        max_attempts = MAX_PERSIST_ATTEMPTS,
                        extended_code = e.extended_sqlite_code(),
                        backoff_ms = backoff.as_millis() as u64,
                        error = %e,
                        "Transient SQLite busy on persist DB commit; retrying after backoff"
                    );
                    std::thread::sleep(backoff);
                } else {
                    // Budget exhausted on a persistent busy → existing clean
                    // recoverable shutdown (no wipe). Log the extended error
                    // code (BUSY 5 / BUSY_SNAPSHOT 517 / LOCKED 6) for
                    // next-incident diagnosis (#3640).
                    tracing::error!(
                        context,
                        attempts = MAX_PERSIST_ATTEMPTS,
                        extended_code = e.extended_sqlite_code(),
                        error = %e,
                        "Persistent SQLite busy on persist DB commit after exhausting \
                         the bounded retry budget; escalating to recoverable shutdown"
                    );
                    // #3802: the commit is abandoned here — count it as a
                    // busy-caused write loss. `is_transient(&e)` already
                    // returned true above, so no re-classification is needed.
                    crate::metrics::record_db_busy_drop(site);
                    shutdown.trigger(context, &e);
                    return PersistOutcome::RecoverableShutdown;
                }
            }
        }
    }
    // Unreachable: the loop always returns inside the final iteration.
    unreachable!("retry loop must return within MAX_PERSIST_ATTEMPTS");
}

/// Lets [`commit_with_busy_retry`] render the SQLite extended error code
/// uniformly across the typed-`DbError` and `anyhow` commit paths.
trait ExtendedSqliteCode {
    /// The SQLite extended error code, or `-1` if not a SQLite error.
    fn extended_sqlite_code(&self) -> i32;
}

impl ExtendedSqliteCode for henyey_db::DbError {
    fn extended_sqlite_code(&self) -> i32 {
        sqlite_extended_code(self).unwrap_or(-1)
    }
}

impl ExtendedSqliteCode for anyhow::Error {
    fn extended_sqlite_code(&self) -> i32 {
        sqlite_extended_code_anyhow(self).unwrap_or(-1)
    }
}

/// Log a fatal persist error and abort the process.
///
/// Used for persist failures that are NOT a recoverable transient-IO condition
/// (corruption, non-free-space IO): the node's on-disk state would diverge
/// from in-memory state, violating determinism guarantees. (A transient
/// ENOSPC/EDQUOT routes to a clean recoverable shutdown instead — see
/// [`handle_persist_error`]; a transient SQLite busy/locked routes to
/// [`handle_db_persist_error`] — #3497.)
pub(super) fn fatal_persist_error(context: &str, error: &dyn std::fmt::Display) -> ! {
    tracing::error!(context, error = %error, "Fatal persist failure, aborting");
    std::process::abort();
}

#[cfg(test)]
mod tests {
    use super::*;
    use henyey_db::queries::StateQueries;
    use stellar_xdr::{Hash, LedgerHeader, LedgerHeaderExt, StellarValue, StellarValueExt};

    /// A detached `RecoverableShutdownHandle` for tests (no live App).
    fn test_recoverable_shutdown() -> RecoverableShutdownHandle {
        let (tx, _rx) = tokio::sync::broadcast::channel(1);
        RecoverableShutdownHandle::new(tx)
    }

    /// #3478: the persist-error classifier must route a transient ENOSPC
    /// (errno 28) to a clean recoverable shutdown (broadcast a shutdown
    /// signal), NOT to `std::process::abort()` — the on-disk state is intact.
    #[test]
    fn transient_io_persist_error_routes_to_recoverable_shutdown_3478() {
        let (tx, mut rx) = tokio::sync::broadcast::channel(1);
        let shutdown = RecoverableShutdownHandle::new(tx);

        let enospc = BucketError::Io(std::io::Error::from_raw_os_error(28));
        assert!(enospc.is_transient_io(), "errno 28 must be transient IO");

        // handle_persist_error must NOT abort for a transient-IO error; it must
        // signal a clean shutdown. (If it aborted, the test process would die.)
        handle_persist_error("test transient flush", &enospc, &shutdown);

        // The recoverable shutdown signal was sent (no wipe, no abort).
        assert!(
            rx.try_recv().is_ok(),
            "transient-IO persist error must trigger a clean recoverable shutdown signal"
        );
    }

    /// #3478: EDQUOT (errno 122) is also a transient free-space class and must
    /// route to recoverable shutdown.
    #[test]
    fn edquot_persist_error_is_transient_3478() {
        let edquot = BucketError::Io(std::io::Error::from_raw_os_error(122));
        assert!(
            edquot.is_transient_io(),
            "errno 122 (EDQUOT) must be transient IO"
        );
    }

    /// #3478: a corruption (or non-free-space IO) persist error must NOT be
    /// classified transient — it stays on the fatal `abort()` path. We assert
    /// the classifier branch (the `abort()` itself is not unit-testable).
    #[test]
    fn corruption_persist_error_stays_fatal_3478() {
        let corruption = BucketError::Corruption("bucket entries unsorted".to_string());
        assert!(
            !corruption.is_transient_io(),
            "corruption must NOT be classified transient (stays fatal/abort)"
        );

        // EIO (errno 5) is conservatively fatal (could be hardware corruption).
        let eio = BucketError::Io(std::io::Error::from_raw_os_error(5));
        assert!(
            !eio.is_transient_io(),
            "EIO must NOT be classified transient (stays fatal/abort)"
        );

        // A transient handle that is never triggered for these classes: assert
        // no shutdown signal is produced by the classifier when we DON'T call
        // handle_persist_error (the fatal path would abort, which we can't run
        // here — so we only verify classification above).
    }

    /// Build a `DbError::Sqlite(SqliteFailure(ffi::Error { code, .. }, msg))`
    /// for the given primary code, mirroring how rusqlite materializes a
    /// SQLite error in the persist write path.
    fn db_sqlite_error(
        code: rusqlite::ffi::ErrorCode,
        extended_code: std::os::raw::c_int,
        msg: &str,
    ) -> henyey_db::DbError {
        henyey_db::DbError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code,
                extended_code,
            },
            Some(msg.to_string()),
        ))
    }

    /// #3497: a transient `SQLITE_BUSY`/`DatabaseBusy` ("database is locked")
    /// at the ledger-close DB-write persist site must route to a clean
    /// recoverable shutdown (broadcast a shutdown signal), NOT to
    /// `std::process::abort()` — the SQLite write transaction never committed,
    /// so the on-disk state is intact and a plain restart recovers it.
    #[test]
    fn transient_db_busy_persist_error_routes_to_recoverable_shutdown_3497() {
        let (tx, mut rx) = tokio::sync::broadcast::channel(1);
        let shutdown = RecoverableShutdownHandle::new(tx);

        // extended_code 5 is documented for traceability; the gate is the
        // primary `DatabaseBusy` code, not the extended code.
        let busy = db_sqlite_error(
            rusqlite::ffi::ErrorCode::DatabaseBusy,
            5,
            "database is locked",
        );
        assert!(
            is_transient_db_busy(&busy),
            "DatabaseBusy must be classified transient"
        );

        // The retry helper must NOT abort for a transient busy error; on a
        // persistent busy it must escalate to a clean recoverable shutdown
        // after exhausting the retry budget. (If it aborted, the test process
        // would die.)
        let outcome = commit_with_busy_retry(
            "ledger close DB write",
            crate::metrics::SITE_LEDGER_CLOSE_PERSIST,
            || {
                Err(db_sqlite_error(
                    rusqlite::ffi::ErrorCode::DatabaseBusy,
                    5,
                    "database is locked",
                ))
            },
            is_transient_db_busy,
            &shutdown,
        );

        assert!(matches!(outcome, PersistOutcome::RecoverableShutdown));
        assert!(
            rx.try_recv().is_ok(),
            "transient DB-busy persist error must trigger a clean recoverable shutdown signal"
        );
    }

    /// #3497: `DatabaseLocked` is the sibling transient code and must also
    /// classify transient + route to recoverable shutdown.
    #[test]
    fn transient_db_locked_persist_error_is_transient_3497() {
        let locked = db_sqlite_error(
            rusqlite::ffi::ErrorCode::DatabaseLocked,
            6,
            "database table is locked",
        );
        assert!(
            is_transient_db_busy(&locked),
            "DatabaseLocked must be classified transient"
        );
    }

    /// #3497: the ledger-close `write_fn` returns `anyhow::Result<()>`, but
    /// `db.transaction(...)?` propagates the typed `DbError` through
    /// `From<DbError> for anyhow::Error`, so the structured rusqlite code
    /// survives a `downcast_ref::<DbError>()`. The anyhow-typed classifier
    /// must recover the code and route a transient busy to recoverable
    /// shutdown — with NO string-matching on "database is locked". Also guards
    /// against a future `anyhow!("…{e}")` wrap erasing the type.
    #[test]
    fn transient_db_busy_via_anyhow_downcast_routes_to_recoverable_3497() {
        let (tx, mut rx) = tokio::sync::broadcast::channel(1);
        let shutdown = RecoverableShutdownHandle::new(tx);

        let busy = db_sqlite_error(
            rusqlite::ffi::ErrorCode::DatabaseBusy,
            5,
            "database is locked",
        );
        // Mirror `write_fn` `?`-propagation: typed DbError → anyhow::Error.
        let anyhow_err: anyhow::Error = anyhow::Error::new(busy);
        assert!(
            is_transient_db_busy_anyhow(&anyhow_err),
            "anyhow-wrapped DatabaseBusy must be recovered via downcast and classified transient"
        );

        let outcome = commit_with_busy_retry(
            "ledger close DB write",
            crate::metrics::SITE_LEDGER_CLOSE_PERSIST,
            || -> Result<(), anyhow::Error> {
                Err(anyhow::Error::new(db_sqlite_error(
                    rusqlite::ffi::ErrorCode::DatabaseBusy,
                    5,
                    "database is locked",
                )))
            },
            is_transient_db_busy_anyhow,
            &shutdown,
        );

        assert!(matches!(outcome, PersistOutcome::RecoverableShutdown));
        assert!(
            rx.try_recv().is_ok(),
            "transient DB-busy via anyhow downcast must trigger a clean recoverable shutdown"
        );
    }

    /// #3497 (load-bearing consensus-safety guard, mirroring
    /// `corruption_persist_error_stays_fatal_3478`): genuine corruption /
    /// integrity errors must NOT be classified transient — they stay on the
    /// fatal `abort()`+wipe path. We assert the classifier branch (the
    /// `abort()` itself is not unit-testable). The predicate must be NARROW:
    /// only `DatabaseBusy`/`DatabaseLocked` are recoverable.
    #[test]
    fn corruption_db_persist_error_stays_fatal_3497() {
        // SQLITE_CORRUPT (extended_code 11 = SQLITE_CORRUPT_VTAB) → fatal.
        let corruption = db_sqlite_error(
            rusqlite::ffi::ErrorCode::DatabaseCorrupt,
            11,
            "database disk image is malformed",
        );
        assert!(
            !is_transient_db_busy(&corruption),
            "DatabaseCorrupt must NOT be classified transient (stays fatal/abort+wipe)"
        );

        // A non-Sqlite DbError (integrity violation) → fatal.
        let integrity = henyey_db::DbError::Integrity("bucket list hash mismatch".to_string());
        assert!(
            !is_transient_db_busy(&integrity),
            "Integrity error must NOT be classified transient (stays fatal/abort+wipe)"
        );

        // Other Sqlite codes (e.g. SQLITE_IOERR) → fatal.
        let ioerr = db_sqlite_error(
            rusqlite::ffi::ErrorCode::SystemIoFailure,
            266,
            "disk I/O error",
        );
        assert!(
            !is_transient_db_busy(&ioerr),
            "SystemIoFailure must NOT be classified transient (stays fatal/abort+wipe)"
        );

        // A non-DbError anyhow error → fatal (downcast fails).
        let other: anyhow::Error = anyhow::anyhow!("some non-db failure");
        assert!(
            !is_transient_db_busy_anyhow(&other),
            "non-DbError anyhow error must NOT be classified transient (stays fatal)"
        );
    }

    use std::cell::Cell;

    /// #3640 (1): a transient `SQLITE_BUSY` that clears within the retry
    /// window must cause the persist commit to RETRY and SURVIVE — no
    /// shutdown signal is sent. FAILS on `origin/main`: no retry helper
    /// exists, so the first BUSY routes straight to `shutdown.trigger`.
    #[test]
    fn transient_db_busy_retries_then_survives_3640() {
        let (tx, mut rx) = tokio::sync::broadcast::channel(1);
        let shutdown = RecoverableShutdownHandle::new(tx);

        // Busy on attempt 1 and 2, Ok on attempt 3.
        let calls = Cell::new(0u32);
        let attempt = || {
            let n = calls.get() + 1;
            calls.set(n);
            if n < 3 {
                Err(db_sqlite_error(
                    rusqlite::ffi::ErrorCode::DatabaseBusy,
                    5,
                    "database is locked",
                ))
            } else {
                Ok(())
            }
        };

        let outcome = commit_with_busy_retry(
            "test ledger close DB write",
            crate::metrics::SITE_LEDGER_CLOSE_PERSIST,
            attempt,
            is_transient_db_busy,
            &shutdown,
        );

        assert!(
            matches!(outcome, PersistOutcome::Survived),
            "a transient busy that clears in-window must survive"
        );
        assert_eq!(calls.get(), 3, "must retry until the busy clears (3 calls)");
        assert!(
            rx.try_recv().is_err(),
            "no recoverable-shutdown signal must be sent when the retry succeeds"
        );
    }

    /// #3640 (4): the anyhow-typed path (ledger-close `write_fn` arm) must
    /// also retry a transient busy and survive. FAILS on main (no helper).
    #[test]
    fn transient_db_busy_via_anyhow_retries_then_survives_3640() {
        let (tx, mut rx) = tokio::sync::broadcast::channel(1);
        let shutdown = RecoverableShutdownHandle::new(tx);

        let calls = Cell::new(0u32);
        let attempt = || -> Result<(), anyhow::Error> {
            let n = calls.get() + 1;
            calls.set(n);
            if n < 2 {
                Err(anyhow::Error::new(db_sqlite_error(
                    rusqlite::ffi::ErrorCode::DatabaseBusy,
                    5,
                    "database is locked",
                )))
            } else {
                Ok(())
            }
        };

        let outcome = commit_with_busy_retry(
            "test ledger close DB write (anyhow)",
            crate::metrics::SITE_LEDGER_CLOSE_PERSIST,
            attempt,
            is_transient_db_busy_anyhow,
            &shutdown,
        );

        assert!(matches!(outcome, PersistOutcome::Survived));
        assert_eq!(calls.get(), 2, "must retry once then succeed");
        assert!(rx.try_recv().is_err(), "no shutdown signal on success");
    }

    /// #3640 (2): a PERSISTENT busy that never clears must exhaust the
    /// bounded retry budget (`MAX_PERSIST_ATTEMPTS` calls), then escalate to
    /// the existing clean recoverable shutdown — NOT the fatal/abort path.
    /// FAILS on main (no attempt-bound concept exists).
    #[test]
    fn transient_db_busy_exhausts_retries_then_recoverable_shutdown_3640() {
        let (tx, mut rx) = tokio::sync::broadcast::channel(1);
        let shutdown = RecoverableShutdownHandle::new(tx);

        let calls = Cell::new(0u32);
        let attempt = || {
            calls.set(calls.get() + 1);
            Err(db_sqlite_error(
                rusqlite::ffi::ErrorCode::DatabaseBusy,
                5,
                "database is locked",
            ))
        };

        let outcome = commit_with_busy_retry(
            "test ledger close DB write",
            crate::metrics::SITE_LEDGER_CLOSE_PERSIST,
            attempt,
            is_transient_db_busy,
            &shutdown,
        );

        assert!(
            matches!(outcome, PersistOutcome::RecoverableShutdown),
            "a persistent busy past the bound must escalate to recoverable shutdown"
        );
        assert_eq!(
            calls.get(),
            MAX_PERSIST_ATTEMPTS,
            "must try exactly MAX_PERSIST_ATTEMPTS times before escalating"
        );
        assert!(
            rx.try_recv().is_ok(),
            "a recoverable-shutdown signal must be sent after the budget is exhausted"
        );
    }

    /// #3640 (3): a NON-transient DB error (corruption/integrity) must NOT
    /// consume any retry — it is classified non-transient and returns the
    /// `Fatal` outcome after exactly ONE call, for the caller to route to
    /// `fatal_persist_error`. The test asserts on call-count + outcome
    /// WITHOUT invoking the real `fatal_persist_error` (which would abort),
    /// mirroring `corruption_db_persist_error_stays_fatal_3497`.
    #[test]
    fn non_transient_db_error_does_not_retry_3640() {
        let (tx, mut rx) = tokio::sync::broadcast::channel(1);
        let shutdown = RecoverableShutdownHandle::new(tx);

        let calls = Cell::new(0u32);
        let attempt = || {
            calls.set(calls.get() + 1);
            Err(db_sqlite_error(
                rusqlite::ffi::ErrorCode::DatabaseCorrupt,
                11,
                "database disk image is malformed",
            ))
        };

        let outcome = commit_with_busy_retry(
            "test ledger close DB write",
            crate::metrics::SITE_LEDGER_CLOSE_PERSIST,
            attempt,
            is_transient_db_busy,
            &shutdown,
        );

        match outcome {
            PersistOutcome::Fatal(_) => {}
            other => panic!("non-transient error must yield Fatal, got {other:?}"),
        }
        assert_eq!(
            calls.get(),
            1,
            "a non-transient error must NOT consume any retry (exactly one call)"
        );
        assert!(
            rx.try_recv().is_err(),
            "no recoverable-shutdown signal must be sent for a non-transient (fatal) error"
        );
    }

    // ── #3802 busy telemetry ───────────────────────────────────────────
    //
    // Counters are process-global, so every one of these runs inside
    // `metrics::with_local_recorder` on a pristine recorder, in a synchronous
    // `#[test]` (the helper is thread-local — never `#[tokio::test]`), and
    // calls `register_label_series()` before any `== 0` assertion, because an
    // unincremented labelled series is simply absent from the render output.

    /// #3802: a transient busy that clears in-window increments the per-site
    /// `attempts` series once per retried attempt and the `dropped` series NOT
    /// AT ALL. This pair — `attempts > 0`, `dropped == 0` — is the signal that
    /// the bounded retry absorbed the contention. Also asserts the legacy
    /// `henyey_db_busy_retry_total` is still emitted (decision 3: the #3640
    /// watch signal is kept, and the double-count is deliberate).
    #[test]
    fn busy_retry_counts_attempts_and_no_drop_on_success_3802() {
        let (recorder, handle) = crate::metrics::fresh_local_recorder();
        ::metrics::with_local_recorder(&recorder, || {
            crate::metrics::describe_metrics();
            crate::metrics::register_label_series();

            let (tx, _rx) = tokio::sync::broadcast::channel(1);
            let shutdown = RecoverableShutdownHandle::new(tx);

            // Busy on attempts 1 and 2, Ok on attempt 3 → 2 retried attempts.
            let calls = Cell::new(0u32);
            let outcome = commit_with_busy_retry(
                "test ledger close DB write",
                crate::metrics::SITE_LEDGER_CLOSE_PERSIST,
                || {
                    let n = calls.get() + 1;
                    calls.set(n);
                    if n < 3 {
                        Err(db_sqlite_error(
                            rusqlite::ffi::ErrorCode::DatabaseBusy,
                            5,
                            "database is locked",
                        ))
                    } else {
                        Ok(())
                    }
                },
                is_transient_db_busy,
                &shutdown,
            );
            assert!(matches!(outcome, PersistOutcome::Survived));

            let out = handle.render();
            assert!(
                out.contains(
                    "henyey_db_busy_retry_attempts_total{site=\"ledger_close_persist\"} 2"
                ),
                "two retried attempts must be counted for the site; got:\n{out}"
            );
            assert!(
                out.contains("henyey_db_busy_write_dropped_total{site=\"ledger_close_persist\"} 0"),
                "a surviving retry must not count as a dropped write; got:\n{out}"
            );
            assert!(
                out.contains("henyey_db_busy_retry_total 2"),
                "the legacy #3640 counter must still be emitted (kept, unchanged); got:\n{out}"
            );
        });
    }

    /// #3802: a persistent busy that exhausts the retry budget increments
    /// `dropped{site}` exactly once (the commit really was abandoned) and
    /// `attempts{site}` `MAX_PERSIST_ATTEMPTS - 1` times — the final, giving-up
    /// attempt is a drop, not a retry.
    #[test]
    fn busy_retry_counts_drop_on_budget_exhaustion_3802() {
        let (recorder, handle) = crate::metrics::fresh_local_recorder();
        ::metrics::with_local_recorder(&recorder, || {
            crate::metrics::describe_metrics();
            crate::metrics::register_label_series();

            let (tx, _rx) = tokio::sync::broadcast::channel(1);
            let shutdown = RecoverableShutdownHandle::new(tx);

            let outcome = commit_with_busy_retry(
                "test catchup DB write",
                crate::metrics::SITE_CATCHUP_PERSIST,
                || {
                    Err(db_sqlite_error(
                        rusqlite::ffi::ErrorCode::DatabaseBusy,
                        5,
                        "database is locked",
                    ))
                },
                is_transient_db_busy,
                &shutdown,
            );
            assert!(matches!(outcome, PersistOutcome::RecoverableShutdown));

            let out = handle.render();
            assert!(
                out.contains("henyey_db_busy_write_dropped_total{site=\"catchup_persist\"} 1"),
                "budget exhaustion must count exactly one dropped write; got:\n{out}"
            );
            assert!(
                out.contains(&format!(
                    "henyey_db_busy_retry_attempts_total{{site=\"catchup_persist\"}} {}",
                    MAX_PERSIST_ATTEMPTS - 1
                )),
                "only the non-final attempts are retries; got:\n{out}"
            );
        });
    }

    /// #3802: a non-transient error short-circuits without consuming a retry,
    /// so neither busy series moves. Genuine corruption must never appear as
    /// SQLite contention.
    #[test]
    fn non_transient_persist_error_counts_nothing_3802() {
        let (recorder, handle) = crate::metrics::fresh_local_recorder();
        ::metrics::with_local_recorder(&recorder, || {
            crate::metrics::describe_metrics();
            crate::metrics::register_label_series();

            let (tx, _rx) = tokio::sync::broadcast::channel(1);
            let shutdown = RecoverableShutdownHandle::new(tx);

            let outcome = commit_with_busy_retry(
                "test ledger close DB write",
                crate::metrics::SITE_LEDGER_CLOSE_PERSIST,
                || Err(henyey_db::DbError::Integrity("boom".into())),
                is_transient_db_busy,
                &shutdown,
            );
            assert!(matches!(outcome, PersistOutcome::Fatal(_)));

            let out = handle.render();
            assert!(
                out.contains(
                    "henyey_db_busy_retry_attempts_total{site=\"ledger_close_persist\"} 0"
                ),
                "a non-transient error must not count a retry attempt; got:\n{out}"
            );
            assert!(
                out.contains("henyey_db_busy_write_dropped_total{site=\"ledger_close_persist\"} 0"),
                "a non-transient error must not count a busy-caused drop; got:\n{out}"
            );
        });
    }

    fn make_header(seq: u32) -> (LedgerHeader, Vec<u8>) {
        use stellar_xdr::{LedgerHeaderExtensionV1, Limits, WriteXdr};
        let header = LedgerHeader {
            ledger_version: 24,
            previous_ledger_hash: Hash([0; 32]),
            scp_value: StellarValue {
                tx_set_hash: Hash([0; 32]),
                close_time: stellar_xdr::TimePoint(0),
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
                ext: stellar_xdr::LedgerHeaderExtensionV1Ext::V0,
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
            publish_enabled: false,
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
            publish_enabled: false,
        };
        let ready = CatchupPersistReady::new(data, db, lm, test_recoverable_shutdown());
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
            publish_enabled: false,
        };
        let ready = CatchupPersistReady::new(data, db, lm, test_recoverable_shutdown());
        let result_ok = Ok(crate::app::types::CatchupResult {
            ledger_seq: 50,
            ledger_hash: henyey_common::Hash256::default(),
            buckets_applied: 1,
            ledgers_replayed: 0,
        });
        let mut result =
            crate::app::types::PendingCatchupResult::new(result_ok, Some(ready), false);
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
        let err_result = crate::app::types::PendingCatchupResult::new(
            Err(anyhow::anyhow!("test error")),
            None,
            false,
        );
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
            false,
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
            false,
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
            let write_fn: PersistWriteFn = Box::new(move |db| {
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
                shutdown: test_recoverable_shutdown(),
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

    /// AUDIT-226: `CatchupPersistData::write_to_db` must clear the
    /// CATCHUP_PERSIST_PENDING sentinel atomically with the state update.
    #[test]
    fn write_to_db_clears_catchup_persist_sentinel() {
        let db = Database::open_in_memory().unwrap();

        // Set the sentinel (simulating deferred catchup path).
        db.with_connection(|conn| {
            conn.set_state(henyey_db::schema::state_keys::CATCHUP_PERSIST_PENDING, "1")
        })
        .unwrap();

        // Verify sentinel is set.
        let val = db
            .with_connection(|conn| {
                conn.get_state(henyey_db::schema::state_keys::CATCHUP_PERSIST_PENDING)
            })
            .unwrap();
        assert!(val.is_some(), "sentinel must be set before write_to_db");

        // Run write_to_db.
        let (header, header_xdr) = make_header(42);
        let data = CatchupPersistData {
            header,
            header_xdr,
            has_json: "{\"version\":1}".to_string(),
            publish_enabled: false,
        };
        data.write_to_db(&db).unwrap();

        // Verify sentinel is cleared.
        let val = db
            .with_connection(|conn| {
                conn.get_state(henyey_db::schema::state_keys::CATCHUP_PERSIST_PENDING)
            })
            .unwrap();
        assert!(val.is_none(), "sentinel must be cleared after write_to_db");
    }

    /// AUDIT-226: sentinel persists if write_to_db is never called.
    #[test]
    fn sentinel_persists_without_write_to_db() {
        let db = Database::open_in_memory().unwrap();

        db.with_connection(|conn| {
            conn.set_state(henyey_db::schema::state_keys::CATCHUP_PERSIST_PENDING, "1")
        })
        .unwrap();

        // Simulate a crash: no write_to_db call. Just re-read.
        let val = db
            .with_connection(|conn| {
                conn.get_state(henyey_db::schema::state_keys::CATCHUP_PERSIST_PENDING)
            })
            .unwrap();
        assert!(
            val.is_some(),
            "sentinel must persist when write_to_db is never called"
        );
    }

    // ---------------------------------------------------------------------
    // #2681: skipFirstCheckpointSinceItIsIncomplete parity tests
    //
    // The marker key (`PUBLISH_SKIP_FIRST_CHECKPOINT`) records the target
    // checkpoint seq whose enqueue must be suppressed because the catchup
    // terminus is mid-checkpoint. These tests pin down the set-or-delete
    // contract in `CatchupPersistData::write_to_db` for the full grid of
    // (publish_enabled, LCL-position) inputs, including stale-marker
    // cleanup. The companion enqueue-time skip tests live in
    // `app::ledger_close::tests`.
    // ---------------------------------------------------------------------

    fn skip_marker(db: &Database) -> Option<String> {
        db.with_connection(|c| {
            c.get_state(henyey_db::schema::state_keys::PUBLISH_SKIP_FIRST_CHECKPOINT)
        })
        .unwrap()
    }

    fn set_skip_marker(db: &Database, value: &str) {
        db.with_connection(|c| {
            c.set_state(
                henyey_db::schema::state_keys::PUBLISH_SKIP_FIRST_CHECKPOINT,
                value,
            )
        })
        .unwrap();
    }

    /// LCL=80 is mid-checkpoint (the checkpoint covers 64..=127), and
    /// publish is enabled → marker must point at 127.
    #[test]
    fn catchup_persist_mid_checkpoint_sets_skip_marker() {
        let db = Database::open_in_memory().unwrap();
        let (header, header_xdr) = make_header(80);
        let data = CatchupPersistData {
            header,
            header_xdr,
            has_json: "{}".to_string(),
            publish_enabled: true,
        };
        data.write_to_db(&db).unwrap();
        assert_eq!(skip_marker(&db).as_deref(), Some("127"));
    }

    /// LCL=127 is a checkpoint boundary (the checkpoint just completed
    /// at this LCL) → no skip is required and any stale marker must be
    /// cleared.
    #[test]
    fn catchup_persist_at_checkpoint_boundary_does_not_set_skip_marker() {
        let db = Database::open_in_memory().unwrap();
        let (header, header_xdr) = make_header(127);
        let data = CatchupPersistData {
            header,
            header_xdr,
            has_json: "{}".to_string(),
            publish_enabled: true,
        };
        data.write_to_db(&db).unwrap();
        assert!(skip_marker(&db).is_none());
    }

    /// Publish disabled → marker must never be set, regardless of LCL.
    #[test]
    fn catchup_persist_with_publish_disabled_does_not_set_skip_marker() {
        let db = Database::open_in_memory().unwrap();
        let (header, header_xdr) = make_header(80);
        let data = CatchupPersistData {
            header,
            header_xdr,
            has_json: "{}".to_string(),
            publish_enabled: false,
        };
        data.write_to_db(&db).unwrap();
        assert!(skip_marker(&db).is_none());
    }

    /// A marker left over from a previous catchup must be cleared when a
    /// new catchup ends at a checkpoint boundary — the set-or-delete
    /// pattern guarantees this.
    #[test]
    fn catchup_persist_clears_stale_skip_marker_when_lcl_complete() {
        let db = Database::open_in_memory().unwrap();
        set_skip_marker(&db, "127");

        let (header, header_xdr) = make_header(127);
        let data = CatchupPersistData {
            header,
            header_xdr,
            has_json: "{}".to_string(),
            publish_enabled: true,
        };
        data.write_to_db(&db).unwrap();
        assert!(skip_marker(&db).is_none());
    }

    /// A stale marker must also be cleared when publish is disabled,
    /// even if the LCL is mid-checkpoint (the "publish was on then went
    /// off across runs" path).
    #[test]
    fn catchup_persist_clears_stale_skip_marker_when_publish_disabled() {
        let db = Database::open_in_memory().unwrap();
        set_skip_marker(&db, "127");

        let (header, header_xdr) = make_header(80);
        let data = CatchupPersistData {
            header,
            header_xdr,
            has_json: "{}".to_string(),
            publish_enabled: false,
        };
        data.write_to_db(&db).unwrap();
        assert!(skip_marker(&db).is_none());
    }

    /// LCL=GENESIS_LEDGER_SEQ (1) → marker must not be set. Mirrors
    /// stellar-core's flag init (false) and ensures we never skip
    /// publishing checkpoint 63 on a fresh node.
    #[test]
    fn catchup_persist_at_genesis_does_not_set_marker() {
        let db = Database::open_in_memory().unwrap();
        let (header, header_xdr) = make_header(GENESIS_LEDGER_SEQ);
        let data = CatchupPersistData {
            header,
            header_xdr,
            has_json: "{}".to_string(),
            publish_enabled: true,
        };
        data.write_to_db(&db).unwrap();
        assert!(skip_marker(&db).is_none());
    }
}
