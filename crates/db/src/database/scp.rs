//! High-level database methods for SCP and bucket-list persistence.

use henyey_common::LedgerSeq;
use stellar_xdr::curr::{ScpEnvelope, ScpQuorumSet};

use crate::{pool::Database, queries, Result};

impl Database {
    /// Stores SCP envelopes for a ledger.
    ///
    /// SCP envelopes contain the consensus messages from validators that
    /// were used to agree on this ledger's contents.
    pub fn store_scp_history(&self, seq: LedgerSeq, envelopes: &[ScpEnvelope]) -> Result<()> {
        self.with_connection(|conn| {
            use queries::ScpQueries;
            conn.store_scp_history(seq, envelopes)
        })
    }

    /// Deletes old SCP history entries up to and including `max_ledger`.
    ///
    /// Removes at most `count` entries from both scphistory and scpquorums
    /// tables. Used by the Maintainer for garbage collection.
    pub fn delete_old_scp_entries(&self, max_ledger: LedgerSeq, count: u32) -> Result<u32> {
        self.with_connection(|conn| {
            use queries::ScpQueries;
            conn.delete_old_scp_entries(max_ledger, count)
        })
    }

    /// Loads SCP envelopes for a ledger.
    ///
    /// Returns the consensus messages that were recorded for the specified ledger.
    pub fn load_scp_history(&self, seq: LedgerSeq) -> Result<Vec<ScpEnvelope>> {
        self.with_connection(|conn| {
            use queries::ScpQueries;
            conn.load_scp_history(seq)
        })
    }

    /// Stores a quorum set by its hash.
    ///
    /// Quorum sets define the trust configuration for SCP consensus.
    /// They are stored by hash and associated with the last ledger where
    /// they were seen, allowing for garbage collection of old quorum sets.
    pub fn store_scp_quorum_set(
        &self,
        hash: &henyey_common::Hash256,
        last_ledger_seq: u32,
        quorum_set: &ScpQuorumSet,
    ) -> Result<()> {
        self.with_connection(|conn| {
            use queries::ScpQueries;
            conn.store_scp_quorum_set(hash, last_ledger_seq, quorum_set)
        })
    }

    /// Loads a quorum set by its hash.
    ///
    /// Returns `None` if no quorum set with the given hash is stored.
    pub fn load_scp_quorum_set(
        &self,
        hash: &henyey_common::Hash256,
    ) -> Result<Option<ScpQuorumSet>> {
        self.with_connection(|conn| {
            use queries::ScpQueries;
            conn.load_scp_quorum_set(hash)
        })
    }

    /// Stores bucket list snapshot levels for a ledger.
    ///
    /// The bucket list is a Merkle tree structure that stores all ledger entries.
    /// At checkpoint ledgers (every 64 ledgers), the bucket hashes are stored
    /// to enable state reconstruction during catchup.
    ///
    /// Each level contains a pair of hashes: (current bucket hash, snap bucket hash).
    pub fn store_bucket_list(
        &self,
        seq: LedgerSeq,
        levels: &[(henyey_common::Hash256, henyey_common::Hash256)],
    ) -> Result<()> {
        self.with_connection(|conn| {
            use queries::BucketListQueries;
            conn.store_bucket_list(seq, levels)
        })
    }

    /// Loads bucket list snapshot levels for a ledger.
    ///
    /// Returns `None` if no bucket list snapshot exists for the given ledger.
    pub fn load_bucket_list(
        &self,
        seq: LedgerSeq,
    ) -> Result<Option<Vec<(henyey_common::Hash256, henyey_common::Hash256)>>> {
        self.with_connection(|conn| {
            use queries::BucketListQueries;
            conn.load_bucket_list(seq)
        })
    }
}
