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
