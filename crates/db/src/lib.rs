//! Database abstraction layer for rs-stellar-core.
//!
//! This crate provides SQLite-based persistence for the Stellar blockchain node,
//! handling storage and retrieval of:
//!
//! - **Ledger headers**: Block metadata including sequence numbers, hashes, and timestamps
//! - **Transaction history**: Transaction bodies, results, and metadata
//! - **SCP state**: Stellar Consensus Protocol envelopes and quorum sets
//! - **Bucket list snapshots**: Merkle tree state at checkpoint ledgers
//! - **Peer records**: Network peer discovery and connection tracking
//! - **Operational state**: Configuration and runtime state persistence
//!
//! # Architecture
//!
//! The crate is organized into several modules:
//!
//! - [`pool`]: Connection pool management using r2d2
//! - [`schema`]: Database schema definitions and table layouts
//! - [`migrations`]: Schema versioning and migration system
//! - [`queries`]: Typed query traits for each data domain
//! - [`error`]: Error types for database operations
//!
//! # Usage
//!
//! ```no_run
//! use henyey_db::Database;
//!
//! // Open a database (creates if it doesn't exist)
//! let db = Database::open("path/to/stellar.db")?;
//!
//! // Or use an in-memory database for testing
//! let test_db = Database::open_in_memory()?;
//!
//! // Query the latest ledger
//! if let Some(seq) = db.get_latest_ledger_seq()? {
//!     println!("Latest ledger: {}", seq);
//! }
//! # Ok::<(), henyey_db::DbError>(())
//! ```
//!
//! # Query Traits
//!
//! Query functionality is organized into domain-specific traits that extend
//! [`rusqlite::Connection`]. The [`Database`] type provides convenience methods
//! that wrap these traits for common operations.
//!
//! For advanced use cases, you can obtain a connection and use the traits directly:
//!
//! ```no_run
//! use henyey_db::{Database, queries::LedgerQueries};
//!
//! let db = Database::open_in_memory()?;
//! db.with_connection(|conn| {
//!     // Use trait methods directly on the connection
//!     let seq = conn.get_latest_ledger_seq()?;
//!     Ok(seq)
//! })?;
//! # Ok::<(), henyey_db::DbError>(())
//! ```

mod database;
pub mod error;
pub(crate) mod migrations;
pub(crate) mod pool;
pub mod queries;
pub mod schema;
pub mod scp_persistence;

pub use error::DbError;

pub use pool::Database;
pub use queries::*;
pub use scp_persistence::SqliteScpPersistence;

/// Result type for database operations.
pub type Result<T> = std::result::Result<T, DbError>;
