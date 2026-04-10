//! Peer manager for persistent peer storage.
//!
//! This module implements the PeerManager from stellar-core, which maintains
//! a persistent list of known peers in a SQLite database with failure tracking
//! and backoff scheduling.
//!
//! # Overview
//!
//! - Peers are stored with their IP:port address
//! - Failure counts track how reliable a peer is
//! - Next attempt time implements exponential backoff
//! - Peer types: INBOUND, OUTBOUND, PREFERRED
//!
//! # Database Schema
//!
//! ```sql
//! CREATE TABLE peers (
//!     ip VARCHAR(15) NOT NULL,
//!     port INT NOT NULL CHECK (port > 0 AND port <= 65535),
//!     nextattempt TIMESTAMP NOT NULL,
//!     numfailures INT DEFAULT 0 CHECK (numfailures >= 0) NOT NULL,
//!     type INT NOT NULL,
//!     PRIMARY KEY (ip, port)
//! );
//! ```

use crate::{OverlayError, PeerAddress, Result};
use parking_lot::{Mutex, RwLock};
use rand::seq::SliceRandom;
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{info, trace};

/// Maximum number of failures before a peer is considered unreliable.
pub const MAX_FAILURES: u32 = 10;

/// Seconds per backoff unit.
const SECONDS_PER_BACKOFF: u64 = 10;

/// Maximum backoff exponent.
const MAX_BACKOFF_EXPONENT: u32 = 10;

pub use henyey_common::StoredPeerType;

/// Filter for querying peers.
#[derive(Debug, Clone, Copy)]
pub enum PeerTypeFilter {
    /// Only inbound peers.
    InboundOnly,
    /// Only outbound peers.
    OutboundOnly,
    /// Only preferred peers.
    PreferredOnly,
    /// Any outbound peer (outbound or preferred).
    AnyOutbound,
}

/// Record of a peer stored in the database.
#[derive(Debug, Clone)]
pub struct PeerRecord {
    /// IP address.
    pub ip: String,
    /// Port number.
    pub port: u16,
    /// Next attempt time (Unix timestamp).
    pub next_attempt: i64,
    /// Number of connection failures.
    pub num_failures: u32,
    /// Peer type.
    pub peer_type: StoredPeerType,
}

impl PeerRecord {
    /// Create a new peer record with default values.
    pub fn new(ip: String, port: u16) -> Self {
        Self {
            ip,
            port,
            next_attempt: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64,
            num_failures: 0,
            peer_type: StoredPeerType::Inbound,
        }
    }

    /// Check if it's time to attempt connecting to this peer.
    pub fn is_ready(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        self.next_attempt <= now
    }

    /// Convert to PeerAddress.
    pub fn to_peer_address(&self) -> PeerAddress {
        PeerAddress::new(self.ip.clone(), self.port)
    }
}

/// Type of update to apply to a peer's type.
#[derive(Debug, Clone, Copy)]
pub enum TypeUpdate {
    /// Ensure peer is at least outbound.
    EnsureOutbound,
    /// Set peer to preferred.
    SetPreferred,
    /// Ensure peer is not preferred (downgrade to outbound if needed).
    EnsureNotPreferred,
}

/// Type of update to apply to a peer's backoff.
#[derive(Debug, Clone, Copy)]
pub enum BackOffUpdate {
    /// Hard reset - set failures to 0 and next attempt to now.
    HardReset,
    /// Soft reset - set failures to 0 but apply backoff.
    Reset,
    /// Increase failure count and apply backoff.
    Increase,
}

/// Query parameters for loading peers.
#[derive(Debug, Clone)]
pub struct PeerQuery {
    /// Only return peers ready for next attempt.
    pub use_next_attempt: bool,
    /// Maximum number of failures allowed.
    pub max_num_failures: Option<u32>,
    /// Type filter.
    pub type_filter: PeerTypeFilter,
}

impl Default for PeerQuery {
    fn default() -> Self {
        Self {
            use_next_attempt: true,
            max_num_failures: Some(MAX_FAILURES),
            type_filter: PeerTypeFilter::AnyOutbound,
        }
    }
}

/// Peer manager for persistent peer storage.
///
/// Thread-safe manager that stores peers in SQLite with failure tracking
/// and exponential backoff scheduling.
pub struct PeerManager {
    /// In-memory cache of peers for fast lookups.
    cache: RwLock<HashMap<(String, u16), PeerRecord>>,
    /// Database connection (optional - if None, peers are in-memory only).
    db: Option<Arc<Mutex<Connection>>>,
}

impl std::fmt::Debug for PeerManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerManager")
            .field("peer_count", &self.cache.read().len())
            .field("has_db", &self.db.is_some())
            .finish()
    }
}

impl PeerManager {
    /// Create a new peer manager with in-memory storage only.
    pub fn new_in_memory() -> Self {
        Self {
            cache: RwLock::new(HashMap::new()),
            db: None,
        }
    }

    /// Create a new peer manager with SQLite persistence.
    pub fn new_with_db(db_path: &Path) -> Result<Self> {
        let conn = Connection::open(db_path).map_err(|e| {
            OverlayError::DatabaseError(format!("Failed to open peer database: {}", e))
        })?;

        Self::init_db(&conn)?;

        // Load existing peers into cache
        let cache = Self::load_all_from_db(&conn)?;
        let num_loaded = cache.len();
        if num_loaded > 0 {
            info!("Loaded {} peers from database", num_loaded);
        }

        let db = Arc::new(Mutex::new(conn));

        Ok(Self {
            cache: RwLock::new(cache),
            db: Some(db),
        })
    }

    /// Create a peer manager using an existing database connection.
    pub fn from_connection(conn: Connection) -> Result<Self> {
        Self::init_db(&conn)?;

        let cache = Self::load_all_from_db(&conn)?;

        let db = Arc::new(Mutex::new(conn));

        Ok(Self {
            cache: RwLock::new(cache),
            db: Some(db),
        })
    }

    /// Initialize the database schema.
    fn init_db(conn: &Connection) -> Result<()> {
        conn.execute(
            "CREATE TABLE IF NOT EXISTS peers (
                ip VARCHAR(15) NOT NULL,
                port INT NOT NULL CHECK (port > 0 AND port <= 65535),
                nextattempt INTEGER NOT NULL,
                numfailures INT DEFAULT 0 CHECK (numfailures >= 0) NOT NULL,
                type INT NOT NULL,
                PRIMARY KEY (ip, port)
            )",
            [],
        )
        .map_err(|e| OverlayError::DatabaseError(format!("Failed to create peers table: {}", e)))?;

        Ok(())
    }

    /// Load all peers from database into a HashMap.
    fn load_all_from_db(conn: &Connection) -> Result<HashMap<(String, u16), PeerRecord>> {
        let mut stmt = conn
            .prepare("SELECT ip, port, nextattempt, numfailures, type FROM peers")
            .map_err(|e| OverlayError::DatabaseError(format!("Failed to prepare query: {}", e)))?;

        let rows = stmt
            .query_map([], |row| {
                Ok(PeerRecord {
                    ip: row.get::<_, String>(0)?,
                    port: row.get::<_, i32>(1)? as u16,
                    next_attempt: row.get::<_, i64>(2)?,
                    num_failures: row.get::<_, i32>(3)? as u32,
                    peer_type: StoredPeerType::try_from(row.get::<_, i32>(4)?).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            4,
                            rusqlite::types::Type::Integer,
                            e.into(),
                        )
                    })?,
                })
            })
            .map_err(|e| OverlayError::DatabaseError(format!("Failed to query peers: {}", e)))?;

        let mut cache = HashMap::new();
        for result in rows {
            match result {
                Ok(record) => {
                    cache.insert((record.ip.clone(), record.port), record);
                }
                Err(e) => {
                    tracing::warn!("Skipping corrupt peer record: {e}");
                }
            }
        }

        Ok(cache)
    }

    /// Ensure a peer exists in the database.
    // SECURITY: peer store bounded by authenticated peer cap and connection limits
    pub fn ensure_exists(&self, address: &PeerAddress) -> Result<()> {
        let key = (address.host.clone(), address.port);

        // Check cache first
        {
            let cache = self.cache.read();
            if cache.contains_key(&key) {
                return Ok(());
            }
        }

        trace!("Learned peer {}", address);

        let record = PeerRecord::new(address.host.clone(), address.port);

        // Insert into database
        if let Some(ref db) = self.db {
            let conn = db.lock();
            conn.execute(
                "INSERT OR IGNORE INTO peers (ip, port, nextattempt, numfailures, type) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    record.ip,
                    record.port as i32,
                    record.next_attempt,
                    record.num_failures as i32,
                    record.peer_type as i32
                ],
            )
            .map_err(|e| OverlayError::DatabaseError(format!("Failed to insert peer: {}", e)))?;
        }

        // Update cache
        {
            let mut cache = self.cache.write();
            cache.insert(key, record);
        }

        Ok(())
    }

    /// Load a peer record by address.
    pub fn load(&self, address: &PeerAddress) -> Option<PeerRecord> {
        let key = (address.host.clone(), address.port);
        let cache = self.cache.read();
        cache.get(&key).cloned()
    }

    /// Store a peer record.
    pub fn store(&self, record: &PeerRecord) -> Result<()> {
        let key = (record.ip.clone(), record.port);

        // Update database
        if let Some(ref db) = self.db {
            let conn = db.lock();
            conn.execute(
                "INSERT OR REPLACE INTO peers (ip, port, nextattempt, numfailures, type) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    record.ip,
                    record.port as i32,
                    record.next_attempt,
                    record.num_failures as i32,
                    record.peer_type as i32
                ],
            )
            .map_err(|e| OverlayError::DatabaseError(format!("Failed to store peer: {}", e)))?;
        }

        // Update cache
        {
            let mut cache = self.cache.write();
            cache.insert(key, record.clone());
        }

        Ok(())
    }

    /// Update a peer's type.
    pub fn update_type(
        &self,
        address: &PeerAddress,
        observed_type: StoredPeerType,
        preferred_type_known: bool,
    ) -> Result<()> {
        let key = (address.host.clone(), address.port);

        let mut record = {
            let cache = self.cache.read();
            match cache.get(&key) {
                Some(r) => r.clone(),
                None => PeerRecord::new(address.host.clone(), address.port),
            }
        };

        // Determine type update
        let type_update = get_type_update(&record, observed_type, preferred_type_known);
        apply_type_update(&mut record, type_update);

        self.store(&record)
    }

    /// Update a peer's backoff.
    // INVARIANT: no concurrent writers to backoff state; single-threaded message handling
    pub fn update_backoff(&self, address: &PeerAddress, backoff: BackOffUpdate) -> Result<()> {
        let key = (address.host.clone(), address.port);

        let mut record = {
            let cache = self.cache.read();
            match cache.get(&key) {
                Some(r) => r.clone(),
                None => PeerRecord::new(address.host.clone(), address.port),
            }
        };

        apply_backoff_update(&mut record, backoff);

        self.store(&record)
    }

    /// Update both type and backoff.
    // INVARIANT: no concurrent writers to backoff state; single-threaded message handling
    pub fn update(
        &self,
        address: &PeerAddress,
        observed_type: StoredPeerType,
        preferred_type_known: bool,
        backoff: BackOffUpdate,
    ) -> Result<()> {
        let key = (address.host.clone(), address.port);

        let mut record = {
            let cache = self.cache.read();
            match cache.get(&key) {
                Some(r) => r.clone(),
                None => PeerRecord::new(address.host.clone(), address.port),
            }
        };

        let type_update = get_type_update(&record, observed_type, preferred_type_known);
        apply_type_update(&mut record, type_update);
        apply_backoff_update(&mut record, backoff);

        self.store(&record)
    }

    /// Load random peers matching the query.
    pub fn load_random_peers(&self, query: &PeerQuery, size: usize) -> Vec<PeerAddress> {
        self.load_random_peers_filtered(query, size, |_| true)
    }

    /// Load random peers matching the query, applying an additional predicate.
    ///
    /// Matches stellar-core `RandomPeerSource::getRandomPeers(size, pred)` which
    /// uses a `keep` callback to skip already-connected peers.  The `pred`
    /// closure is called for each candidate and only addresses for which it
    /// returns `true` are included.
    pub fn load_random_peers_filtered(
        &self,
        query: &PeerQuery,
        size: usize,
        pred: impl Fn(&PeerAddress) -> bool,
    ) -> Vec<PeerAddress> {
        let cache = self.cache.read();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let mut candidates: Vec<&PeerRecord> = cache
            .values()
            .filter(|r| {
                // Check next attempt time
                if query.use_next_attempt && r.next_attempt > now {
                    return false;
                }

                // Check failure count
                if let Some(max_failures) = query.max_num_failures {
                    if r.num_failures > max_failures {
                        return false;
                    }
                }

                // Check type filter
                match query.type_filter {
                    PeerTypeFilter::InboundOnly => r.peer_type == StoredPeerType::Inbound,
                    PeerTypeFilter::OutboundOnly => r.peer_type == StoredPeerType::Outbound,
                    PeerTypeFilter::PreferredOnly => r.peer_type == StoredPeerType::Preferred,
                    PeerTypeFilter::AnyOutbound => r.peer_type != StoredPeerType::Inbound,
                }
            })
            .collect();

        // Shuffle and take up to size, applying the predicate
        candidates.shuffle(&mut rand::thread_rng());
        candidates
            .into_iter()
            .map(|r| r.to_peer_address())
            .filter(|addr| pred(addr))
            .take(size)
            .collect()
    }

    /// Remove peers with many failures.
    pub fn remove_peers_with_many_failures(&self, min_num_failures: u32) -> Result<()> {
        // Remove from database
        if let Some(ref db) = self.db {
            let conn = db.lock();
            conn.execute(
                "DELETE FROM peers WHERE numfailures >= ?1",
                params![min_num_failures as i32],
            )
            .map_err(|e| {
                OverlayError::DatabaseError(format!("Failed to remove failed peers: {}", e))
            })?;
        }

        // Remove from cache
        {
            let mut cache = self.cache.write();
            cache.retain(|_, r| r.num_failures < min_num_failures);
        }

        Ok(())
    }

    /// Get peers to send to another peer.
    pub fn get_peers_to_send(&self, size: usize, exclude: &PeerAddress) -> Vec<PeerAddress> {
        let cache = self.cache.read();

        let mut candidates: Vec<&PeerRecord> = cache
            .values()
            .filter(|r| {
                // Don't send peer back to itself
                if r.ip == exclude.host && r.port == exclude.port {
                    return false;
                }

                // Don't send private addresses
                let addr = PeerAddress::new(r.ip.clone(), r.port);
                !addr.is_private()
            })
            .collect();

        // Prefer outbound peers
        candidates.sort_by_key(|r| match r.peer_type {
            StoredPeerType::Preferred => 0,
            StoredPeerType::Outbound => 1,
            StoredPeerType::Inbound => 2,
        });

        candidates
            .into_iter()
            .take(size)
            .map(|r| r.to_peer_address())
            .collect()
    }

    /// Get all peer records.
    pub fn get_all_peers(&self) -> Vec<PeerRecord> {
        let cache = self.cache.read();
        cache.values().cloned().collect()
    }

    /// Get the number of stored peers.
    pub fn peer_count(&self) -> usize {
        let cache = self.cache.read();
        cache.len()
    }

    /// Clear all peers.
    pub fn clear_all(&self) -> Result<()> {
        if let Some(ref db) = self.db {
            let conn = db.lock();
            conn.execute("DELETE FROM peers", []).map_err(|e| {
                OverlayError::DatabaseError(format!("Failed to clear peers: {}", e))
            })?;
        }

        {
            let mut cache = self.cache.write();
            cache.clear();
        }

        Ok(())
    }
}

impl Default for PeerManager {
    fn default() -> Self {
        Self::new_in_memory()
    }
}

/// Determine what type update to apply.
fn get_type_update(
    record: &PeerRecord,
    observed_type: StoredPeerType,
    preferred_type_known: bool,
) -> TypeUpdate {
    let is_preferred_in_db = record.peer_type == StoredPeerType::Preferred;

    match observed_type {
        StoredPeerType::Preferred => TypeUpdate::SetPreferred,
        StoredPeerType::Outbound => {
            if is_preferred_in_db && preferred_type_known {
                TypeUpdate::EnsureNotPreferred
            } else {
                TypeUpdate::EnsureOutbound
            }
        }
        StoredPeerType::Inbound => TypeUpdate::EnsureNotPreferred,
    }
}

/// Apply a type update to a peer record.
fn apply_type_update(record: &mut PeerRecord, update: TypeUpdate) {
    match update {
        TypeUpdate::EnsureOutbound => {
            if record.peer_type == StoredPeerType::Inbound {
                record.peer_type = StoredPeerType::Outbound;
            }
        }
        TypeUpdate::SetPreferred => {
            record.peer_type = StoredPeerType::Preferred;
        }
        TypeUpdate::EnsureNotPreferred => {
            if record.peer_type == StoredPeerType::Preferred {
                record.peer_type = StoredPeerType::Outbound;
            }
        }
    }
}

/// Compute backoff duration based on failure count.
fn compute_backoff(num_failures: u32) -> Duration {
    let backoff_count = num_failures.min(MAX_BACKOFF_EXPONENT);
    let max_seconds = (1u64 << backoff_count) * SECONDS_PER_BACKOFF;
    let random_seconds = rand::random::<u64>() % max_seconds + 1;
    Duration::from_secs(random_seconds)
}

/// Apply a backoff update to a peer record.
fn apply_backoff_update(record: &mut PeerRecord, update: BackOffUpdate) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    match update {
        BackOffUpdate::HardReset => {
            record.num_failures = 0;
            record.next_attempt = now;
        }
        BackOffUpdate::Reset => {
            record.num_failures = 0;
            let backoff = compute_backoff(0);
            record.next_attempt = now + backoff.as_secs() as i64;
        }
        BackOffUpdate::Increase => {
            record.num_failures += 1;
            let backoff = compute_backoff(record.num_failures);
            record.next_attempt = now + backoff.as_secs() as i64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_peer_manager_in_memory() {
        let manager = PeerManager::new_in_memory();
        let addr = PeerAddress::new("1.2.3.4".to_string(), 11625);

        assert_eq!(manager.peer_count(), 0);

        manager.ensure_exists(&addr).unwrap();
        assert_eq!(manager.peer_count(), 1);

        let record = manager.load(&addr).unwrap();
        assert_eq!(record.ip, "1.2.3.4");
        assert_eq!(record.port, 11625);
        assert_eq!(record.num_failures, 0);
    }

    #[test]
    fn test_peer_manager_update_backoff() {
        let manager = PeerManager::new_in_memory();
        let addr = PeerAddress::new("1.2.3.4".to_string(), 11625);

        manager.ensure_exists(&addr).unwrap();

        // Increase failures
        manager
            .update_backoff(&addr, BackOffUpdate::Increase)
            .unwrap();
        let record = manager.load(&addr).unwrap();
        assert_eq!(record.num_failures, 1);

        manager
            .update_backoff(&addr, BackOffUpdate::Increase)
            .unwrap();
        let record = manager.load(&addr).unwrap();
        assert_eq!(record.num_failures, 2);

        // Hard reset
        manager
            .update_backoff(&addr, BackOffUpdate::HardReset)
            .unwrap();
        let record = manager.load(&addr).unwrap();
        assert_eq!(record.num_failures, 0);
    }

    #[test]
    fn test_peer_manager_update_type() {
        let manager = PeerManager::new_in_memory();
        let addr = PeerAddress::new("1.2.3.4".to_string(), 11625);

        manager.ensure_exists(&addr).unwrap();

        // Default is inbound
        let record = manager.load(&addr).unwrap();
        assert_eq!(record.peer_type, StoredPeerType::Inbound);

        // Upgrade to outbound
        manager
            .update_type(&addr, StoredPeerType::Outbound, false)
            .unwrap();
        let record = manager.load(&addr).unwrap();
        assert_eq!(record.peer_type, StoredPeerType::Outbound);

        // Upgrade to preferred
        manager
            .update_type(&addr, StoredPeerType::Preferred, true)
            .unwrap();
        let record = manager.load(&addr).unwrap();
        assert_eq!(record.peer_type, StoredPeerType::Preferred);
    }

    #[test]
    fn test_peer_manager_load_random() {
        let manager = PeerManager::new_in_memory();

        // Add several peers
        for i in 1..10 {
            let addr = PeerAddress::new(format!("1.2.3.{}", i), 11625);
            manager.ensure_exists(&addr).unwrap();
            manager
                .update_type(&addr, StoredPeerType::Outbound, false)
                .unwrap();
        }

        let query = PeerQuery {
            use_next_attempt: true,
            max_num_failures: Some(MAX_FAILURES),
            type_filter: PeerTypeFilter::AnyOutbound,
        };

        let peers = manager.load_random_peers(&query, 5);
        assert_eq!(peers.len(), 5);
    }

    #[test]
    fn test_peer_manager_remove_failed() {
        let manager = PeerManager::new_in_memory();

        // Add peers with varying failure counts
        for i in 1..5 {
            let addr = PeerAddress::new(format!("1.2.3.{}", i), 11625);
            manager.ensure_exists(&addr).unwrap();
            for _ in 0..i {
                manager
                    .update_backoff(&addr, BackOffUpdate::Increase)
                    .unwrap();
            }
        }

        assert_eq!(manager.peer_count(), 4);

        // Remove peers with 3+ failures
        manager.remove_peers_with_many_failures(3).unwrap();

        assert_eq!(manager.peer_count(), 2);
    }

    #[test]
    fn test_peer_manager_with_db() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("peers.db");

        let addr = PeerAddress::new("1.2.3.4".to_string(), 11625);

        // Create manager and add peer
        {
            let manager = PeerManager::new_with_db(&db_path).unwrap();
            manager.ensure_exists(&addr).unwrap();
            manager
                .update_backoff(&addr, BackOffUpdate::Increase)
                .unwrap();
            assert_eq!(manager.peer_count(), 1);
        }

        // Verify persistence
        {
            let manager = PeerManager::new_with_db(&db_path).unwrap();
            assert_eq!(manager.peer_count(), 1);
            let record = manager.load(&addr).unwrap();
            assert_eq!(record.num_failures, 1);
        }
    }

    #[test]
    fn test_get_peers_to_send() {
        let manager = PeerManager::new_in_memory();
        let exclude = PeerAddress::new("1.2.3.1".to_string(), 11625);

        // Add several peers
        for i in 1..5 {
            let addr = PeerAddress::new(format!("1.2.3.{}", i), 11625);
            manager.ensure_exists(&addr).unwrap();
        }

        let peers = manager.get_peers_to_send(10, &exclude);
        // Should exclude the "exclude" address
        assert_eq!(peers.len(), 3);
        assert!(!peers.contains(&exclude));
    }

    #[test]
    fn test_load_random_peers_filtered_excludes_connected() {
        // G13: load_random_peers_filtered applies a predicate to skip
        // already-connected peers, matching stellar-core's keep callback.
        let manager = PeerManager::new_in_memory();

        for i in 1..=5 {
            let addr = PeerAddress::new(format!("1.2.3.{}", i), 11625);
            manager.ensure_exists(&addr).unwrap();
            manager
                .update_type(&addr, StoredPeerType::Outbound, false)
                .unwrap();
        }

        let connected = PeerAddress::new("1.2.3.3".to_string(), 11625);
        let query = PeerQuery {
            use_next_attempt: true,
            max_num_failures: Some(MAX_FAILURES),
            type_filter: PeerTypeFilter::AnyOutbound,
        };

        // Without filter: all 5 are returned
        let all = manager.load_random_peers(&query, 10);
        assert_eq!(all.len(), 5);

        // With filter: exclude the connected peer
        let filtered = manager.load_random_peers_filtered(&query, 10, |addr| *addr != connected);
        assert_eq!(filtered.len(), 4);
        assert!(!filtered.contains(&connected));
    }

    #[test]
    fn test_load_random_peers_filtered_by_direction() {
        // G13: direction-aware peer loading — only return outbound-typed
        // peers when filling outbound slots, only inbound when promoting.
        let manager = PeerManager::new_in_memory();

        // Add 3 inbound and 3 outbound peers
        for i in 1..=3 {
            let addr = PeerAddress::new(format!("10.0.0.{}", i), 11625);
            manager.ensure_exists(&addr).unwrap();
            // stays Inbound (default)
        }
        for i in 4..=6 {
            let addr = PeerAddress::new(format!("10.0.0.{}", i), 11625);
            manager.ensure_exists(&addr).unwrap();
            manager
                .update_type(&addr, StoredPeerType::Outbound, false)
                .unwrap();
        }

        // Outbound-only query should return only the 3 outbound peers
        let outbound_query = PeerQuery {
            use_next_attempt: false,
            max_num_failures: None,
            type_filter: PeerTypeFilter::OutboundOnly,
        };
        let outbound = manager.load_random_peers(&outbound_query, 10);
        assert_eq!(outbound.len(), 3);
        for addr in &outbound {
            let port: u8 = addr.host.split('.').next_back().unwrap().parse().unwrap();
            assert!(port >= 4, "expected outbound peer, got {}", addr);
        }

        // Inbound-only query should return only the 3 inbound peers
        let inbound_query = PeerQuery {
            use_next_attempt: false,
            max_num_failures: None,
            type_filter: PeerTypeFilter::InboundOnly,
        };
        let inbound = manager.load_random_peers(&inbound_query, 10);
        assert_eq!(inbound.len(), 3);
        for addr in &inbound {
            let port: u8 = addr.host.split('.').next_back().unwrap().parse().unwrap();
            assert!(port <= 3, "expected inbound peer, got {}", addr);
        }
    }
}
