//! Database query implementations.
//!
//! This module provides typed query traits for each data domain in the
//! stellar-core database. Each trait extends [`rusqlite::Connection`],
//! allowing query methods to be called directly on database connections.
//!
//! # Architecture
//!
//! Queries are organized by domain:
//!
//! - [`BanQueries`]: Node ban list management
//! - [`BucketListQueries`]: Bucket list snapshot storage
//! - [`HistoryQueries`]: Transaction history and results
//! - [`LedgerQueries`]: Ledger header storage and retrieval
//! - [`PeerQueries`]: Network peer management
//! - [`PublishQueueQueries`]: History archive publish queue
//! - [`ScpQueries`]: SCP consensus state persistence
//! - [`StateQueries`]: Generic key-value state storage
//!
//! # Usage
//!
//! Query traits are implemented on `rusqlite::Connection`, so they can be
//! used directly with any connection:
//!
//! ```ignore
//! use henyey_db::queries::LedgerQueries;
//!
//! db.with_connection(|conn| {
//!     let header = conn.load_ledger_header(100)?;
//!     Ok(header)
//! })?;
//! ```

pub mod ban;
pub mod bucket_list;
pub mod events;
pub mod history;
pub mod ledger;
pub mod ledger_close_meta;
pub mod peers;
pub mod publish_queue;
pub mod scp;
pub mod state;

pub use ban::BanQueries;
pub use bucket_list::BucketListQueries;
pub use events::{EventQueries, EventQueryParams, EventRecord};
pub use history::{HistoryQueries, StoreTxParams, TxRecord, TxStatus};
pub use ledger::LedgerQueries;
pub use ledger_close_meta::LedgerCloseMetaQueries;
pub use peers::{PeerFilter, PeerQueries, PeerRecord, PeerTypeFilter};
pub use publish_queue::PublishQueueQueries;
pub use scp::{ScpQueries, ScpStatePersistenceQueries};
pub use state::StateQueries;

/// Build a SQL `IN` placeholder list: `"?,?,?"` for the given count.
pub(crate) fn sql_placeholder_list(count: usize) -> String {
    vec!["?"; count].join(",")
}

// ── Query budget helpers ─────────────────────────────────────────────────────

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use rusqlite::Connection;

/// RAII guard that clears the connection-wide SQLite progress handler on drop.
///
/// # Safety invariant
///
/// SQLite supports only **one** progress handler per connection at a time.
/// Callers must not nest or overlap guards on the same connection. This type
/// is intentionally `pub(crate)` to limit its use to query implementations
/// within this crate.
pub(crate) struct QueryBudgetGuard<'a> {
    conn: &'a Connection,
}

impl Drop for QueryBudgetGuard<'_> {
    fn drop(&mut self) {
        self.conn.progress_handler(0, None::<fn() -> bool>);
    }
}

/// Installs a SQLite progress handler that interrupts the query after
/// approximately `max_ops` VM opcodes.
///
/// The handler fires every 1000 opcodes and checks a counter against
/// `ceil(max_ops / 1000)`. When the counter reaches the threshold the
/// handler returns `true`, causing SQLite to raise `SQLITE_INTERRUPT`.
///
/// Returns `None` (no-op) when `max_ops == 0`, meaning "unlimited".
///
/// The returned [`QueryBudgetGuard`] **must** be held for the duration of
/// the query; dropping it clears the handler so pooled connections are
/// never left with a stale callback.
pub(crate) fn install_query_budget(
    conn: &Connection,
    max_ops: u32,
) -> Option<QueryBudgetGuard<'_>> {
    if max_ops == 0 {
        return None;
    }
    let max_callbacks = (max_ops as u64).div_ceil(1000) as u32;
    let counter = Arc::new(AtomicU32::new(0));
    let counter_clone = counter.clone();
    conn.progress_handler(
        1000,
        Some(move || {
            let count = counter_clone.fetch_add(1, Ordering::Relaxed);
            count + 1 >= max_callbacks
        }),
    );
    Some(QueryBudgetGuard { conn })
}

/// Returns `true` if the error is an `SQLITE_INTERRUPT`, which indicates
/// the progress handler budget was exceeded.
pub(crate) fn is_query_interrupted(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ffi::ErrorCode::OperationInterrupted,
                ..
            },
            _
        )
    )
}
