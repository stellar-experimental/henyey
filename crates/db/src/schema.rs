//! Database schema definitions.
//!
//! This module contains the complete SQL schema for the stellar-core database.
//! The schema is designed to be compatible with the stellar-core database
//! structure while being optimized for Rust access patterns.
//!
//! # Tables
//!
//! The schema includes tables for:
//!
//! - **State management**: `storestate` - key-value store for node configuration
//! - **Ledger data**: `ledgerheaders` - block headers with sequence numbers and hashes
//! - **Transaction history**: `txhistory`, `txsets`, `txresults`
//! - **Bucket list**: `bucketlist` - checkpoint bucket hashes
//! - **Consensus**: `scphistory`, `scpquorums`
//! - **Networking**: `peers`, `ban`
//! - **Publishing**: `publishqueue`
//!
//! # Versioning
//!
//! The schema version is tracked in the `storestate` table and managed by
//! the [`migrations`](crate::migrations) module.

/// Complete SQL schema for initializing a fresh database.
///
/// This creates all tables and indexes needed for stellar-core operation.
/// For existing databases, use the migration system instead of re-running this.
pub(crate) const CREATE_SCHEMA: &str = r#"
-- Schema version tracking
CREATE TABLE IF NOT EXISTS storestate (
    statename TEXT PRIMARY KEY,
    state TEXT NOT NULL
);

-- Ledger headers
CREATE TABLE IF NOT EXISTS ledgerheaders (
    ledgerhash TEXT PRIMARY KEY,
    prevhash TEXT NOT NULL,
    bucketlisthash TEXT NOT NULL,
    ledgerseq INTEGER UNIQUE NOT NULL,
    closetime INTEGER NOT NULL,
    data BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS ledgerheaders_seq ON ledgerheaders(ledgerseq);

-- Transaction history
CREATE TABLE IF NOT EXISTS txhistory (
    txid TEXT PRIMARY KEY,
    ledgerseq INTEGER NOT NULL,
    txindex INTEGER NOT NULL,
    txbody BLOB NOT NULL,
    txresult BLOB NOT NULL,
    txmeta BLOB,
    status INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS txhistory_ledger ON txhistory(ledgerseq);
CREATE INDEX IF NOT EXISTS txhistory_status_ledger ON txhistory(status, ledgerseq, txindex);

-- Transaction history entries (tx sets)
CREATE TABLE IF NOT EXISTS txsets (
    ledgerseq INTEGER PRIMARY KEY,
    data BLOB NOT NULL
);

-- Transaction history result entries (tx results)
CREATE TABLE IF NOT EXISTS txresults (
    ledgerseq INTEGER PRIMARY KEY,
    data BLOB NOT NULL
);

-- Bucket list snapshots (checkpoint ledgers only)
CREATE TABLE IF NOT EXISTS bucketlist (
    ledgerseq INTEGER NOT NULL,
    level INTEGER NOT NULL,
    currhash TEXT NOT NULL,
    snaphash TEXT NOT NULL,
    PRIMARY KEY (ledgerseq, level)
);
CREATE INDEX IF NOT EXISTS bucketlist_ledger ON bucketlist(ledgerseq);

-- SCP state
CREATE TABLE IF NOT EXISTS scphistory (
    nodeid TEXT NOT NULL,
    ledgerseq INTEGER NOT NULL,
    envelope BLOB NOT NULL
);
CREATE INDEX IF NOT EXISTS scphistory_ledger ON scphistory(ledgerseq);

-- SCP quorum information
CREATE TABLE IF NOT EXISTS scpquorums (
    qsethash TEXT PRIMARY KEY,
    lastledgerseq INTEGER NOT NULL,
    qset BLOB NOT NULL
);

-- Peers
CREATE TABLE IF NOT EXISTS peers (
    ip TEXT NOT NULL,
    port INTEGER NOT NULL,
    nextattempt INTEGER NOT NULL,
    numfailures INTEGER NOT NULL DEFAULT 0,
    type INTEGER NOT NULL,
    PRIMARY KEY (ip, port)
);

-- Ban list
CREATE TABLE IF NOT EXISTS ban (
    nodeid TEXT PRIMARY KEY
);

-- Publish queue (for history publishing)
CREATE TABLE IF NOT EXISTS publishqueue (
    ledgerseq INTEGER PRIMARY KEY,
    state TEXT NOT NULL
);

-- Contract events (for getEvents RPC)
CREATE TABLE IF NOT EXISTS events (
    id TEXT PRIMARY KEY NOT NULL,
    ledgerseq INTEGER NOT NULL,
    tx_index INTEGER NOT NULL,
    op_index INTEGER NOT NULL,
    tx_hash TEXT NOT NULL,
    contract_id TEXT,
    event_type INTEGER NOT NULL,
    topic1 TEXT,
    topic2 TEXT,
    topic3 TEXT,
    topic4 TEXT,
    event_xdr TEXT NOT NULL,
    in_successful_contract_call INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS events_ledger ON events(ledgerseq);
CREATE INDEX IF NOT EXISTS events_contract ON events(contract_id, ledgerseq);
CREATE INDEX IF NOT EXISTS events_topic1_id ON events(topic1, id);

-- Ledger close metadata (full LedgerCloseMeta XDR, for RPC serving)
CREATE TABLE IF NOT EXISTS ledger_close_meta (
    sequence INTEGER PRIMARY KEY,
    meta BLOB NOT NULL
);
"#;

/// Well-known keys for the `storestate` table.
///
/// The `storestate` table is a key-value store for persistent node state.
/// These constants define the standard keys used by stellar-core.
pub mod state_keys {
    /// The sequence number of the last closed ledger.
    ///
    /// This is the primary indicator of the node's progress through the chain.
    pub const LAST_CLOSED_LEDGER: &str = "lastclosedledger";

    /// JSON-encoded history archive state.
    ///
    /// Contains information about the last published checkpoint and the
    /// state of the bucket list at that point.
    pub const HISTORY_ARCHIVE_STATE: &str = "historyarchivestate";

    /// Current database schema version.
    ///
    /// Used by the migration system to track schema upgrades.
    pub const DATABASE_SCHEMA: &str = "databaseschema";

    /// Network passphrase for transaction signing.
    ///
    /// Identifies which Stellar network this node is connected to
    /// (e.g., "Public Global Stellar Network ; September 2015" for mainnet).
    pub const NETWORK_PASSPHRASE: &str = "networkpassphrase";

    /// Target ledger version for protocol upgrades.
    ///
    /// Set when a protocol upgrade is pending.
    pub const LEDGER_UPGRADE_VERSION: &str = "ledgerupgradeversion";

    /// Serialized SCP state for crash recovery.
    ///
    /// Contains the last known SCP state to resume consensus after restart.
    pub const LAST_SCP_DATA: &str = "lastscpdata";

    /// Current SCP nomination/ballot state.
    ///
    /// Used for consensus state persistence.
    pub const SCP_STATE: &str = "scpstate";

    /// Force SCP flag for standalone bootstrap.
    ///
    /// When set to "true", the node will skip catchup on next `run` and
    /// bootstrap consensus from the current LCL. Used for single-node
    /// networks (e.g., quickstart local mode). Cleared after first use.
    pub const FORCE_SCP: &str = "forcescp";
}
