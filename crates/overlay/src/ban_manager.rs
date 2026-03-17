//! Ban manager for persistent peer banning.
//!
//! This module implements the BanManager from stellar-core, which maintains
//! a persistent list of banned nodes in a SQLite database.
//!
//! # Overview
//!
//! - Nodes are identified by their Ed25519 public key (NodeID)
//! - Bans are stored in SQLite for persistence across restarts
//! - The ban list is checked during peer connection acceptance
//! - Supports both permanent bans (manual) and time-limited bans (automatic)
//!
//! # Database Schema
//!
//! ```sql
//! CREATE TABLE ban (
//!     nodeid CHARACTER(56) NOT NULL PRIMARY KEY
//! );
//! ```

use crate::{OverlayError, PeerId, Result};
use parking_lot::{Mutex, RwLock};
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info};

/// Default failure threshold before a peer is auto-banned.
pub const AUTO_BAN_FAILURE_THRESHOLD: u32 = 10;

/// Default duration for an auto-ban (5 minutes).
pub const AUTO_BAN_DURATION: Duration = Duration::from_secs(300);

/// Ban manager for maintaining a persistent ban list.
///
/// Thread-safe manager that stores banned node IDs in SQLite.
/// Supports both permanent bans (manual) and time-limited bans (automatic).
pub struct BanManager {
    /// In-memory cache of banned nodes with optional expiry time.
    /// `None` expiry means permanent ban (manual).
    cache: RwLock<HashMap<PeerId, Option<Instant>>>,
    /// Database connection (optional - if None, bans are in-memory only).
    db: Option<Arc<Mutex<Connection>>>,
}

impl BanManager {
    /// Create a new ban manager with in-memory storage only.
    pub fn new_in_memory() -> Self {
        Self {
            cache: RwLock::new(HashMap::new()),
            db: None,
        }
    }

    /// Create a new ban manager with SQLite persistence.
    pub fn new_with_db(db_path: &Path) -> Result<Self> {
        let conn = Connection::open(db_path).map_err(|e| {
            OverlayError::DatabaseError(format!("Failed to open ban database: {}", e))
        })?;

        let manager = Self::from_connection(conn)?;
        let num_loaded = manager.cache.read().len();
        if num_loaded > 0 {
            info!("Loaded {} banned nodes from database", num_loaded);
        }
        Ok(manager)
    }

    /// Create a ban manager using an existing database connection.
    pub fn from_connection(conn: Connection) -> Result<Self> {
        // Create the ban table if it doesn't exist
        conn.execute(
            "CREATE TABLE IF NOT EXISTS ban (
                nodeid CHARACTER(56) NOT NULL PRIMARY KEY
            )",
            [],
        )
        .map_err(|e| OverlayError::DatabaseError(format!("Failed to create ban table: {}", e)))?;

        // Load existing bans into cache (all DB bans are permanent)
        let mut cache = HashMap::new();
        {
            let mut stmt = conn.prepare("SELECT nodeid FROM ban").map_err(|e| {
                OverlayError::DatabaseError(format!("Failed to prepare query: {}", e))
            })?;

            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|e| OverlayError::DatabaseError(format!("Failed to query bans: {}", e)))?;

            for row in rows.flatten() {
                if let Ok(peer_id) = PeerId::from_strkey(&row) {
                    cache.insert(peer_id, None); // permanent
                }
            }
        }

        let db = Arc::new(Mutex::new(conn));

        Ok(Self {
            cache: RwLock::new(cache),
            db: Some(db),
        })
    }

    /// Ban a node permanently by its ID.
    ///
    /// If the node is already permanently banned, this is a no-op.
    /// If the node has a time-limited ban, it is upgraded to permanent.
    pub fn ban_node(&self, node_id: &PeerId) -> Result<()> {
        // Check cache first
        {
            let cache = self.cache.read();
            if let Some(expiry) = cache.get(node_id) {
                if expiry.is_none() {
                    // Already permanently banned
                    debug!("Node {} is already permanently banned", node_id);
                    return Ok(());
                }
                // Upgrade from time-limited to permanent below
            }
        }

        let node_id_str = node_id.to_strkey();
        info!("Banning node {}", node_id_str);

        // Insert into database if available
        if let Some(ref db) = self.db {
            let conn = db.lock();
            conn.execute(
                "INSERT OR IGNORE INTO ban (nodeid) VALUES (?1)",
                [&node_id_str],
            )
            .map_err(|e| OverlayError::DatabaseError(format!("Failed to insert ban: {}", e)))?;
        }

        // Update cache (permanent = None expiry)
        {
            let mut cache = self.cache.write();
            cache.insert(node_id.clone(), None);
        }

        Ok(())
    }

    /// Ban a node for a limited duration.
    ///
    /// Time-limited bans are stored in memory only (not persisted to DB).
    /// If the node is already permanently banned, this is a no-op.
    pub fn ban_node_for(&self, node_id: &PeerId, duration: Duration) {
        let mut cache = self.cache.write();
        if let Some(existing) = cache.get(node_id) {
            if existing.is_none() {
                // Already permanently banned — don't downgrade
                return;
            }
        }
        let expires_at = Instant::now() + duration;
        info!("Auto-banning node {} for {}s", node_id, duration.as_secs());
        cache.insert(node_id.clone(), Some(expires_at));
    }

    /// Automatically ban a peer if its failure count exceeds the threshold.
    ///
    /// Called on peer disconnect. If `num_failures >= AUTO_BAN_FAILURE_THRESHOLD`,
    /// bans the peer for `AUTO_BAN_DURATION`. Returns `true` if a ban was applied.
    pub fn maybe_auto_ban(&self, node_id: &PeerId, num_failures: u32) -> bool {
        if num_failures < AUTO_BAN_FAILURE_THRESHOLD {
            return false;
        }
        if self.is_banned(node_id) {
            return false;
        }
        self.ban_node_for(node_id, AUTO_BAN_DURATION);
        true
    }

    /// Remove expired time-limited bans.
    ///
    /// Should be called periodically from the tick loop.
    /// Returns the number of bans that expired.
    pub fn cleanup_expired_bans(&self) -> usize {
        let now = Instant::now();
        let mut cache = self.cache.write();
        let before = cache.len();
        cache.retain(|_, expiry| match expiry {
            None => true, // permanent bans never expire
            Some(expires_at) => *expires_at > now,
        });
        let removed = before - cache.len();
        if removed > 0 {
            debug!("Cleaned up {} expired time-limited bans", removed);
        }
        removed
    }

    /// Unban a node by its ID.
    ///
    /// If the node is not banned, this is a no-op.
    pub fn unban_node(&self, node_id: &PeerId) -> Result<()> {
        let node_id_str = node_id.to_strkey();
        info!("Unbanning node {}", node_id_str);

        // Delete from database if available
        if let Some(ref db) = self.db {
            let conn = db.lock();
            conn.execute("DELETE FROM ban WHERE nodeid = ?1", [&node_id_str])
                .map_err(|e| OverlayError::DatabaseError(format!("Failed to delete ban: {}", e)))?;
        }

        // Update cache
        {
            let mut cache = self.cache.write();
            cache.remove(node_id);
        }

        Ok(())
    }

    /// Check if a node is banned (either permanently or time-limited).
    pub fn is_banned(&self, node_id: &PeerId) -> bool {
        let cache = self.cache.read();
        match cache.get(node_id) {
            None => false,
            Some(None) => true, // permanent
            Some(Some(expires_at)) => Instant::now() < *expires_at,
        }
    }

    /// Get a list of all banned nodes as strkey strings.
    pub fn get_bans(&self) -> Vec<String> {
        let cache = self.cache.read();
        let now = Instant::now();
        cache
            .iter()
            .filter(|(_, expiry)| match expiry {
                None => true,
                Some(expires_at) => now < *expires_at,
            })
            .map(|(id, _)| id.to_strkey())
            .collect()
    }

    /// Get a list of all banned node IDs.
    pub fn get_banned_ids(&self) -> Vec<PeerId> {
        let cache = self.cache.read();
        let now = Instant::now();
        cache
            .iter()
            .filter(|(_, expiry)| match expiry {
                None => true,
                Some(expires_at) => now < *expires_at,
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Get the number of banned nodes (including unexpired time-limited bans).
    pub fn ban_count(&self) -> usize {
        let cache = self.cache.read();
        let now = Instant::now();
        cache
            .values()
            .filter(|expiry| match expiry {
                None => true,
                Some(expires_at) => now < *expires_at,
            })
            .count()
    }

    /// Clear all bans.
    pub fn clear_all(&self) -> Result<()> {
        info!("Clearing all bans");

        // Delete from database if available
        if let Some(ref db) = self.db {
            let conn = db.lock();
            conn.execute("DELETE FROM ban", [])
                .map_err(|e| OverlayError::DatabaseError(format!("Failed to clear bans: {}", e)))?;
        }

        // Clear cache
        {
            let mut cache = self.cache.write();
            cache.clear();
        }

        Ok(())
    }

    /// Drop and recreate the ban table.
    ///
    /// This is useful for database migrations or resetting state.
    pub fn drop_and_create(&self) -> Result<()> {
        if let Some(ref db) = self.db {
            let conn = db.lock();
            conn.execute("DROP TABLE IF EXISTS ban", []).map_err(|e| {
                OverlayError::DatabaseError(format!("Failed to drop ban table: {}", e))
            })?;
            conn.execute(
                "CREATE TABLE ban (
                    nodeid CHARACTER(56) NOT NULL PRIMARY KEY
                )",
                [],
            )
            .map_err(|e| {
                OverlayError::DatabaseError(format!("Failed to create ban table: {}", e))
            })?;
        }

        // Clear cache
        {
            let mut cache = self.cache.write();
            cache.clear();
        }

        Ok(())
    }
}

impl Default for BanManager {
    fn default() -> Self {
        Self::new_in_memory()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_peer_id(id: u8) -> PeerId {
        PeerId::from_bytes([id; 32])
    }

    #[test]
    fn test_ban_manager_in_memory() {
        let manager = BanManager::new_in_memory();
        let peer = make_peer_id(1);

        assert!(!manager.is_banned(&peer));
        assert_eq!(manager.ban_count(), 0);

        manager.ban_node(&peer).unwrap();
        assert!(manager.is_banned(&peer));
        assert_eq!(manager.ban_count(), 1);

        manager.unban_node(&peer).unwrap();
        assert!(!manager.is_banned(&peer));
        assert_eq!(manager.ban_count(), 0);
    }

    #[test]
    fn test_ban_manager_idempotent() {
        let manager = BanManager::new_in_memory();
        let peer = make_peer_id(1);

        // Banning twice should be idempotent
        manager.ban_node(&peer).unwrap();
        manager.ban_node(&peer).unwrap();
        assert_eq!(manager.ban_count(), 1);

        // Unbanning twice should be safe
        manager.unban_node(&peer).unwrap();
        manager.unban_node(&peer).unwrap();
        assert_eq!(manager.ban_count(), 0);
    }

    #[test]
    fn test_ban_manager_multiple_peers() {
        let manager = BanManager::new_in_memory();
        let peer1 = make_peer_id(1);
        let peer2 = make_peer_id(2);
        let peer3 = make_peer_id(3);

        manager.ban_node(&peer1).unwrap();
        manager.ban_node(&peer2).unwrap();
        manager.ban_node(&peer3).unwrap();

        assert!(manager.is_banned(&peer1));
        assert!(manager.is_banned(&peer2));
        assert!(manager.is_banned(&peer3));
        assert_eq!(manager.ban_count(), 3);

        let bans = manager.get_bans();
        assert_eq!(bans.len(), 3);
    }

    #[test]
    fn test_ban_manager_clear_all() {
        let manager = BanManager::new_in_memory();
        let peer1 = make_peer_id(1);
        let peer2 = make_peer_id(2);

        manager.ban_node(&peer1).unwrap();
        manager.ban_node(&peer2).unwrap();
        assert_eq!(manager.ban_count(), 2);

        manager.clear_all().unwrap();
        assert_eq!(manager.ban_count(), 0);
        assert!(!manager.is_banned(&peer1));
        assert!(!manager.is_banned(&peer2));
    }

    #[test]
    fn test_ban_manager_with_db() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("bans.db");

        let peer1 = make_peer_id(1);
        let peer2 = make_peer_id(2);

        // Create manager and add bans
        {
            let manager = BanManager::new_with_db(&db_path).unwrap();
            manager.ban_node(&peer1).unwrap();
            manager.ban_node(&peer2).unwrap();
            assert_eq!(manager.ban_count(), 2);
        }

        // Create new manager and verify bans persist
        {
            let manager = BanManager::new_with_db(&db_path).unwrap();
            assert_eq!(manager.ban_count(), 2);
            assert!(manager.is_banned(&peer1));
            assert!(manager.is_banned(&peer2));
        }
    }

    #[test]
    fn test_ban_manager_persistence_unban() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("bans.db");

        let peer1 = make_peer_id(1);
        let peer2 = make_peer_id(2);

        // Create manager, add bans, then unban one
        {
            let manager = BanManager::new_with_db(&db_path).unwrap();
            manager.ban_node(&peer1).unwrap();
            manager.ban_node(&peer2).unwrap();
            manager.unban_node(&peer1).unwrap();
            assert_eq!(manager.ban_count(), 1);
        }

        // Verify unban persisted
        {
            let manager = BanManager::new_with_db(&db_path).unwrap();
            assert_eq!(manager.ban_count(), 1);
            assert!(!manager.is_banned(&peer1));
            assert!(manager.is_banned(&peer2));
        }
    }

    #[test]
    fn test_ban_manager_drop_and_create() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("bans.db");

        let peer = make_peer_id(1);

        let manager = BanManager::new_with_db(&db_path).unwrap();
        manager.ban_node(&peer).unwrap();
        assert_eq!(manager.ban_count(), 1);

        manager.drop_and_create().unwrap();
        assert_eq!(manager.ban_count(), 0);
    }

    // --- G10 tests: time-limited bans and auto-ban escalation ---

    #[test]
    fn test_ban_node_for_time_limited() {
        let manager = BanManager::new_in_memory();
        let peer = make_peer_id(10);

        assert!(!manager.is_banned(&peer));

        // Ban for 1 hour (will still be active during this test)
        manager.ban_node_for(&peer, Duration::from_secs(3600));
        assert!(manager.is_banned(&peer));
        assert_eq!(manager.ban_count(), 1);
    }

    #[test]
    fn test_ban_node_for_does_not_downgrade_permanent() {
        let manager = BanManager::new_in_memory();
        let peer = make_peer_id(10);

        // Permanently ban
        manager.ban_node(&peer).unwrap();
        assert!(manager.is_banned(&peer));

        // Time-limited ban should not downgrade a permanent ban
        manager.ban_node_for(&peer, Duration::from_millis(1));
        assert!(manager.is_banned(&peer));

        // Even after cleanup, permanent ban persists
        manager.cleanup_expired_bans();
        assert!(manager.is_banned(&peer));
    }

    #[test]
    fn test_ban_node_permanent_upgrades_time_limited() {
        let manager = BanManager::new_in_memory();
        let peer = make_peer_id(10);

        // Time-limited ban first
        manager.ban_node_for(&peer, Duration::from_millis(1));
        assert!(manager.is_banned(&peer));

        // Upgrade to permanent
        manager.ban_node(&peer).unwrap();

        // Even after cleanup, permanent ban persists
        std::thread::sleep(Duration::from_millis(5));
        manager.cleanup_expired_bans();
        assert!(manager.is_banned(&peer));
    }

    #[test]
    fn test_cleanup_expired_bans() {
        let manager = BanManager::new_in_memory();
        let peer1 = make_peer_id(1);
        let peer2 = make_peer_id(2);
        let peer3 = make_peer_id(3);

        // peer1: permanent ban
        manager.ban_node(&peer1).unwrap();
        // peer2: expires immediately
        manager.ban_node_for(&peer2, Duration::from_millis(1));
        // peer3: expires in the future
        manager.ban_node_for(&peer3, Duration::from_secs(3600));

        assert_eq!(manager.ban_count(), 3);

        // Wait for peer2's ban to expire
        std::thread::sleep(Duration::from_millis(5));

        let removed = manager.cleanup_expired_bans();
        assert_eq!(removed, 1);
        assert_eq!(manager.ban_count(), 2);
        assert!(manager.is_banned(&peer1)); // permanent
        assert!(!manager.is_banned(&peer2)); // expired
        assert!(manager.is_banned(&peer3)); // still active
    }

    #[test]
    fn test_maybe_auto_ban_below_threshold() {
        let manager = BanManager::new_in_memory();
        let peer = make_peer_id(1);

        // Below threshold: no ban
        assert!(!manager.maybe_auto_ban(&peer, AUTO_BAN_FAILURE_THRESHOLD - 1));
        assert!(!manager.is_banned(&peer));
    }

    #[test]
    fn test_maybe_auto_ban_at_threshold() {
        let manager = BanManager::new_in_memory();
        let peer = make_peer_id(1);

        // At threshold: banned
        assert!(manager.maybe_auto_ban(&peer, AUTO_BAN_FAILURE_THRESHOLD));
        assert!(manager.is_banned(&peer));
        assert_eq!(manager.ban_count(), 1);
    }

    #[test]
    fn test_maybe_auto_ban_already_banned() {
        let manager = BanManager::new_in_memory();
        let peer = make_peer_id(1);

        // Already permanently banned
        manager.ban_node(&peer).unwrap();

        // Should not re-ban (returns false)
        assert!(!manager.maybe_auto_ban(&peer, AUTO_BAN_FAILURE_THRESHOLD + 5));
    }

    #[test]
    fn test_time_limited_ban_expires_naturally() {
        let manager = BanManager::new_in_memory();
        let peer = make_peer_id(1);

        // Ban for 1ms
        manager.ban_node_for(&peer, Duration::from_millis(1));
        assert!(manager.is_banned(&peer));

        // Wait for expiry
        std::thread::sleep(Duration::from_millis(5));

        // is_banned checks expiry lazily
        assert!(!manager.is_banned(&peer));
        // ban_count also filters expired
        assert_eq!(manager.ban_count(), 0);
    }
}
