//! High-level database methods for history and RPC retention data.

use henyey_common::LedgerSeq;
use stellar_xdr::curr::{TransactionHistoryEntry, TransactionHistoryResultEntry};

use crate::{pool::Database, queries, Result};

impl Database {
    /// Returns the transaction set for a ledger.
    ///
    /// The transaction history entry contains all transactions that were
    /// included in the specified ledger.
    pub fn get_tx_history_entry(&self, seq: LedgerSeq) -> Result<Option<TransactionHistoryEntry>> {
        self.with_connection(|conn| {
            use queries::HistoryQueries;
            conn.load_tx_history_entry(seq)
        })
    }

    /// Returns the transaction results for a ledger.
    ///
    /// Contains the execution results of all transactions in the ledger.
    pub fn get_tx_result_entry(
        &self,
        seq: LedgerSeq,
    ) -> Result<Option<TransactionHistoryResultEntry>> {
        self.with_connection(|conn| {
            use queries::HistoryQueries;
            conn.load_tx_result_entry(seq)
        })
    }

    /// Deletes old contract events up to and including `max_ledger`.
    ///
    /// Removes at most `count` entries. Used by the Maintainer for garbage
    /// collection of old event history.
    pub fn delete_old_events(&self, max_ledger: LedgerSeq, count: u32) -> Result<u32> {
        self.with_connection(|conn| {
            use queries::EventQueries;
            conn.delete_old_events(max_ledger, count)
        })
    }

    /// Stores a serialized `LedgerCloseMeta` for the given ledger sequence.
    ///
    /// Used at ledger close time to persist the full metadata blob for
    /// RPC serving (getTransactions, getLedgers).
    pub fn store_ledger_close_meta(&self, sequence: u32, meta: &[u8]) -> Result<()> {
        self.with_connection(|conn| {
            use queries::LedgerCloseMetaQueries;
            conn.store_ledger_close_meta(sequence, meta)
        })
    }

    /// Deletes old ledger close metadata entries up to and including `max_ledger`.
    ///
    /// Removes at most `count` entries. Used by the Maintainer for garbage
    /// collection within the RPC retention window.
    pub fn delete_old_ledger_close_meta(&self, max_ledger: LedgerSeq, count: u32) -> Result<u32> {
        self.with_connection(|conn| {
            use queries::LedgerCloseMetaQueries;
            conn.delete_old_ledger_close_meta(max_ledger, count)
        })
    }

    /// Deletes old transaction history entries up to and including `max_ledger`.
    ///
    /// Removes at most `count` entries from `txhistory`, `txsets`, and `txresults`.
    /// Used by the Maintainer for garbage collection.
    pub fn delete_old_tx_history(&self, max_ledger: LedgerSeq, count: u32) -> Result<u32> {
        self.with_connection(|conn| {
            use queries::HistoryQueries;
            conn.delete_old_tx_history(max_ledger, count)
        })
    }
}
