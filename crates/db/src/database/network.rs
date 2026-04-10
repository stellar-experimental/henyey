//! High-level database methods for peer, publish queue, and ban-list state.

use crate::{pool::Database, queries, Result};
use henyey_common::LedgerSeq;

impl Database {
    /// Loads peer records from the database.
    ///
    /// Returns a list of (host, port, record) tuples. Optionally limited to
    /// the specified number of peers.
    pub fn load_peers(
        &self,
        limit: Option<usize>,
    ) -> Result<Vec<(String, u16, queries::PeerRecord)>> {
        self.with_connection(|conn| {
            use queries::PeerQueries;
            conn.load_peers(limit)
        })
    }

    /// Adds a checkpoint ledger to the publish queue with its HAS JSON.
    ///
    /// Checkpoint ledgers (every 64 ledgers) need to be published to history
    /// archives. This queue tracks which checkpoints are pending publication.
    /// The HAS JSON is captured at checkpoint time to preserve the exact
    /// bucket list state (including hot archive hashes) for publishing.
    pub fn enqueue_publish(&self, ledger_seq: LedgerSeq, has_json: &str) -> Result<()> {
        self.with_connection(|conn| {
            use queries::PublishQueueQueries;
            conn.enqueue_publish(ledger_seq, has_json)
        })
    }

    /// Removes a checkpoint ledger from the publish queue.
    ///
    /// Called after successful publication to a history archive.
    pub fn remove_publish(&self, ledger_seq: LedgerSeq) -> Result<()> {
        self.with_connection(|conn| {
            use queries::PublishQueueQueries;
            conn.remove_publish(ledger_seq)
        })
    }

    /// Removes all publish queue entries above the given LCL.
    ///
    /// Called during startup recovery to clean up stale entries that
    /// refer to checkpoints beyond what has been committed to the database.
    /// Returns the number of entries removed.
    pub fn remove_publish_above_lcl(&self, lcl: u32) -> Result<u64> {
        self.with_connection(|conn| {
            use queries::PublishQueueQueries;
            conn.remove_above_lcl(lcl)
        })
    }

    /// Loads queued checkpoint ledgers pending publication.
    ///
    /// Returns ledger sequence numbers in ascending order.
    pub fn load_publish_queue(&self, limit: Option<usize>) -> Result<Vec<u32>> {
        self.with_connection(|conn| {
            use queries::PublishQueueQueries;
            conn.load_publish_queue(limit)
        })
    }

    /// Loads the HAS JSON for a specific queued checkpoint.
    ///
    /// Returns the History Archive State JSON that was stored at enqueue
    /// time, or `None` if the checkpoint is not in the queue.
    pub fn load_publish_has(&self, ledger_seq: LedgerSeq) -> Result<Option<String>> {
        self.with_connection(|conn| {
            use queries::PublishQueueQueries;
            conn.load_publish_has(ledger_seq)
        })
    }

    /// Adds a node ID to the ban list.
    ///
    /// Banned nodes are excluded from consensus and peer connections.
    pub fn ban_node(&self, node_id: &str) -> Result<()> {
        self.with_connection(|conn| {
            use queries::BanQueries;
            conn.ban_node(node_id)
        })
    }

    /// Removes a node ID from the ban list.
    pub fn unban_node(&self, node_id: &str) -> Result<()> {
        self.with_connection(|conn| {
            use queries::BanQueries;
            conn.unban_node(node_id)
        })
    }

    /// Checks if a node ID is banned.
    pub fn is_banned(&self, node_id: &str) -> Result<bool> {
        self.with_connection(|conn| {
            use queries::BanQueries;
            conn.is_banned(node_id)
        })
    }

    /// Loads all banned node IDs.
    pub fn load_bans(&self) -> Result<Vec<String>> {
        self.with_connection(|conn| {
            use queries::BanQueries;
            conn.load_bans()
        })
    }

    /// Stores or updates a peer record.
    ///
    /// The peer record tracks connection metadata including failure count,
    /// next retry time, and peer type (inbound/outbound).
    pub fn store_peer(&self, host: &str, port: u16, record: queries::PeerRecord) -> Result<()> {
        self.with_connection(|conn| {
            use queries::PeerQueries;
            conn.store_peer(host, port, record)
        })
    }

    /// Loads a peer record by host and port.
    ///
    /// Returns `None` if the peer is not in the database.
    pub fn load_peer(&self, host: &str, port: u16) -> Result<Option<queries::PeerRecord>> {
        self.with_connection(|conn| {
            use queries::PeerQueries;
            conn.load_peer(host, port)
        })
    }

    /// Removes peers that have exceeded the failure threshold.
    ///
    /// This is used to garbage collect peers that consistently fail to connect.
    pub fn remove_peers_with_failures(&self, min_failures: u32) -> Result<()> {
        self.with_connection(|conn| {
            use queries::PeerQueries;
            conn.remove_peers_with_failures(min_failures)
        })
    }

    /// Loads random peers matching the specified constraints.
    ///
    /// Filters by maximum failures, next attempt time, and optionally peer type.
    /// Results are randomized to distribute connection attempts.
    pub fn load_random_peers(
        &self,
        limit: usize,
        max_failures: u32,
        now: i64,
        peer_type: Option<henyey_common::StoredPeerType>,
    ) -> Result<Vec<(String, u16, queries::PeerRecord)>> {
        self.with_connection(|conn| {
            use queries::PeerQueries;
            conn.load_random_peers(limit, max_failures, now, peer_type)
        })
    }

    /// Loads random outbound peers (excludes the specified inbound type).
    ///
    /// Filters by maximum failures and next attempt time.
    pub fn load_random_peers_any_outbound(
        &self,
        limit: usize,
        max_failures: u32,
        now: i64,
        inbound_type: henyey_common::StoredPeerType,
    ) -> Result<Vec<(String, u16, queries::PeerRecord)>> {
        self.with_connection(|conn| {
            use queries::PeerQueries;
            conn.load_random_peers_any_outbound(limit, max_failures, now, inbound_type)
        })
    }

    /// Loads random outbound peers by failure count only.
    ///
    /// Similar to [`Self::load_random_peers_any_outbound`] but ignores the next
    /// attempt time, useful for aggressive peer discovery.
    pub fn load_random_peers_any_outbound_max_failures(
        &self,
        limit: usize,
        max_failures: u32,
        inbound_type: henyey_common::StoredPeerType,
    ) -> Result<Vec<(String, u16, queries::PeerRecord)>> {
        self.with_connection(|conn| {
            use queries::PeerQueries;
            conn.load_random_peers_any_outbound_max_failures(limit, max_failures, inbound_type)
        })
    }

    /// Loads random peers of a specific type by failure count only.
    ///
    /// Ignores next attempt time, useful for targeted peer type queries.
    pub fn load_random_peers_by_type_max_failures(
        &self,
        limit: usize,
        max_failures: u32,
        peer_type: henyey_common::StoredPeerType,
    ) -> Result<Vec<(String, u16, queries::PeerRecord)>> {
        self.with_connection(|conn| {
            use queries::PeerQueries;
            conn.load_random_peers_by_type_max_failures(limit, max_failures, peer_type)
        })
    }
}
