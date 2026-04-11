//! Peer record queries for the peers table.
//!
//! This module provides database operations for network peer management.
//! The peer table tracks known network peers with their connection state,
//! enabling persistent peer discovery and connection retry logic.

use henyey_common::StoredPeerType;
use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::error::DbError;

/// Extracts a `(host, port, PeerRecord)` tuple from a row.
fn peer_row(row: &Row<'_>) -> rusqlite::Result<(String, u16, PeerRecord)> {
    let host: String = row.get(0)?;
    let port: i64 = row.get(1)?;
    let raw_type: i32 = row.get(4)?;
    let record = PeerRecord {
        next_attempt: row.get(2)?,
        num_failures: row.get::<_, i64>(3)? as u32,
        peer_type: StoredPeerType::try_from(raw_type).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Integer, e.into())
        })?,
    };
    Ok((host, port as u16, record))
}

/// Database representation of a peer record.
///
/// Tracks connection metadata for a network peer, including retry
/// scheduling and failure counting for connection management.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerRecord {
    /// Unix timestamp of when to next attempt connection.
    ///
    /// Used for exponential backoff on connection failures.
    pub next_attempt: i64,
    /// Number of consecutive connection failures.
    ///
    /// Used for pruning persistently unreachable peers.
    pub num_failures: u32,
    /// The type/category of this peer (application-defined).
    pub peer_type: StoredPeerType,
}

impl PeerRecord {
    /// Creates a new peer record with the given parameters.
    pub fn new(next_attempt: i64, num_failures: u32, peer_type: StoredPeerType) -> Self {
        Self {
            next_attempt,
            num_failures,
            peer_type,
        }
    }
}

/// Filter criteria for random peer queries.
///
/// Composes optional predicates into a single SQL query.
/// All specified conditions are ANDed together.
#[derive(Debug, Clone, Default)]
pub struct PeerFilter {
    /// Maximum allowed failure count (always applied).
    pub max_failures: u32,
    /// If set, only return peers with `next_attempt <= now`.
    pub before_time: Option<i64>,
    /// Type filter for the query.
    pub type_filter: Option<PeerTypeFilter>,
}

/// How to filter peers by type.
#[derive(Debug, Clone, Copy)]
pub enum PeerTypeFilter {
    /// Only return peers matching this type (`type = ?`).
    Equals(StoredPeerType),
    /// Exclude peers of this type (`type != ?`).
    NotEquals(StoredPeerType),
}

/// Query trait for peer management operations.
///
/// Provides methods for CRUD operations on the `peers` table, as well as
/// specialized queries for peer selection during connection attempts.
pub trait PeerQueries {
    /// Loads a peer record by host and port.
    ///
    /// Returns `None` if the peer is not in the database.
    fn load_peer(&self, host: &str, port: u16) -> Result<Option<PeerRecord>, DbError>;

    /// Stores or updates a peer record.
    ///
    /// If a peer with the same host/port exists, it is replaced.
    fn store_peer(&self, host: &str, port: u16, record: PeerRecord) -> Result<(), DbError>;

    /// Loads peer records, optionally limited to a maximum count.
    fn load_peers(&self, limit: Option<usize>) -> Result<Vec<(String, u16, PeerRecord)>, DbError>;

    /// Loads random peers matching the specified filter criteria.
    ///
    /// Results are randomized to distribute connection attempts.
    fn query_random_peers(
        &self,
        limit: usize,
        filter: &PeerFilter,
    ) -> Result<Vec<(String, u16, PeerRecord)>, DbError>;

    /// Removes peers that have exceeded the failure threshold.
    ///
    /// Used for garbage collection of persistently unreachable peers.
    fn remove_peers_with_failures(&self, min_failures: u32) -> Result<(), DbError>;
}

impl PeerQueries for Connection {
    fn load_peer(&self, host: &str, port: u16) -> Result<Option<PeerRecord>, DbError> {
        let result = self
            .query_row(
                "SELECT nextattempt, numfailures, type FROM peers WHERE ip = ?1 AND port = ?2",
                params![host, port as i64],
                |row| {
                    let raw_type: i32 = row.get(2)?;
                    Ok(PeerRecord {
                        next_attempt: row.get(0)?,
                        num_failures: row.get::<_, i64>(1)? as u32,
                        peer_type: StoredPeerType::try_from(raw_type).map_err(|e| {
                            rusqlite::Error::FromSqlConversionFailure(
                                2,
                                rusqlite::types::Type::Integer,
                                e.into(),
                            )
                        })?,
                    })
                },
            )
            .optional()?;
        Ok(result)
    }

    fn store_peer(&self, host: &str, port: u16, record: PeerRecord) -> Result<(), DbError> {
        self.execute(
            "INSERT OR REPLACE INTO peers (ip, port, nextattempt, numfailures, type) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                host,
                port as i64,
                record.next_attempt,
                record.num_failures as i64,
                record.peer_type as i32,
            ],
        )?;
        Ok(())
    }

    fn load_peers(&self, limit: Option<usize>) -> Result<Vec<(String, u16, PeerRecord)>, DbError> {
        let mut sql = String::from("SELECT ip, port, nextattempt, numfailures, type FROM peers");
        if limit.is_some() {
            sql.push_str(" LIMIT ?1");
        }

        let mut stmt = self.prepare(&sql)?;
        let rows = if let Some(limit) = limit {
            stmt.query_map(params![limit as i64], peer_row)?
        } else {
            stmt.query_map([], peer_row)?
        };
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DbError::from)
    }

    fn query_random_peers(
        &self,
        limit: usize,
        filter: &PeerFilter,
    ) -> Result<Vec<(String, u16, PeerRecord)>, DbError> {
        let mut sql = String::from(
            "SELECT ip, port, nextattempt, numfailures, type FROM peers WHERE numfailures <= ?1",
        );
        let mut param_idx = 2;

        // Build dynamic SQL based on filter
        let time_val;
        let type_val;

        if let Some(now) = filter.before_time {
            sql.push_str(&format!(" AND nextattempt <= ?{param_idx}"));
            time_val = Some(now);
            param_idx += 1;
        } else {
            time_val = None;
        }

        if let Some(ref tf) = filter.type_filter {
            let op = match tf {
                PeerTypeFilter::Equals(_) => "=",
                PeerTypeFilter::NotEquals(_) => "!=",
            };
            sql.push_str(&format!(" AND type {op} ?{param_idx}"));
            type_val = Some(match tf {
                PeerTypeFilter::Equals(t) | PeerTypeFilter::NotEquals(t) => *t as i32,
            });
            param_idx += 1;
        } else {
            type_val = None;
        }

        sql.push_str(&format!(" ORDER BY RANDOM() LIMIT ?{param_idx}"));

        // Execute with dynamic params
        let mut stmt = self.prepare(&sql)?;
        let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        params_vec.push(Box::new(filter.max_failures as i64));
        if let Some(t) = time_val {
            params_vec.push(Box::new(t));
        }
        if let Some(t) = type_val {
            params_vec.push(Box::new(t));
        }
        params_vec.push(Box::new(limit as i64));

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params_vec.iter().map(|p| p.as_ref()).collect();
        let rows = stmt.query_map(param_refs.as_slice(), peer_row)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DbError::from)
    }

    fn remove_peers_with_failures(&self, min_failures: u32) -> Result<(), DbError> {
        self.execute(
            "DELETE FROM peers WHERE numfailures >= ?1",
            params![min_failures as i64],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use henyey_common::StoredPeerType::{Inbound, Outbound, Preferred};
    use rusqlite::Connection;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE peers (ip TEXT NOT NULL, port INTEGER NOT NULL, nextattempt INTEGER NOT NULL, numfailures INTEGER NOT NULL, type INTEGER NOT NULL, PRIMARY KEY (ip, port));",
        )
        .unwrap();
        conn
    }

    #[test]
    fn test_store_and_load_peer() {
        let conn = setup_db();
        let record = PeerRecord::new(123, 2, Outbound);
        conn.store_peer("1.2.3.4", 11625, record).unwrap();

        let loaded = conn.load_peer("1.2.3.4", 11625).unwrap();
        assert_eq!(loaded, Some(record));
    }

    #[test]
    fn test_load_peers_limit() {
        let conn = setup_db();
        conn.store_peer("1.2.3.4", 1, PeerRecord::new(1, 0, Outbound))
            .unwrap();
        conn.store_peer("1.2.3.5", 2, PeerRecord::new(2, 0, Outbound))
            .unwrap();

        let peers = conn.load_peers(Some(1)).unwrap();
        assert_eq!(peers.len(), 1);
    }

    #[test]
    fn test_query_random_peers_not_equals_type() {
        let conn = setup_db();
        conn.store_peer("1.2.3.4", 1, PeerRecord::new(1, 0, Outbound))
            .unwrap();
        conn.store_peer("1.2.3.5", 2, PeerRecord::new(1, 0, Preferred))
            .unwrap();
        conn.store_peer("1.2.3.6", 3, PeerRecord::new(1, 5, Inbound))
            .unwrap();
        conn.store_peer("1.2.3.7", 4, PeerRecord::new(1, 11, Preferred))
            .unwrap();

        let filter = PeerFilter {
            max_failures: 10,
            type_filter: Some(PeerTypeFilter::NotEquals(Inbound)),
            ..Default::default()
        };
        let peers = conn.query_random_peers(10, &filter).unwrap();
        assert!(peers.iter().all(|(_, _, rec)| rec.peer_type != Inbound));
        assert!(peers.iter().all(|(_, _, rec)| rec.num_failures <= 10));
    }

    #[test]
    fn test_query_random_peers_equals_type() {
        let conn = setup_db();
        conn.store_peer("1.2.3.4", 1, PeerRecord::new(1, 0, Outbound))
            .unwrap();
        conn.store_peer("1.2.3.5", 2, PeerRecord::new(1, 0, Preferred))
            .unwrap();
        conn.store_peer("1.2.3.6", 3, PeerRecord::new(1, 5, Preferred))
            .unwrap();
        conn.store_peer("1.2.3.7", 4, PeerRecord::new(1, 11, Preferred))
            .unwrap();

        let filter = PeerFilter {
            max_failures: 10,
            type_filter: Some(PeerTypeFilter::Equals(Preferred)),
            ..Default::default()
        };
        let peers = conn.query_random_peers(10, &filter).unwrap();
        assert!(peers.iter().all(|(_, _, rec)| rec.peer_type == Preferred));
        assert!(peers.iter().all(|(_, _, rec)| rec.num_failures <= 10));
    }

    #[test]
    fn test_remove_peers_with_failures() {
        let conn = setup_db();
        conn.store_peer("1.2.3.4", 1, PeerRecord::new(1, 1, Outbound))
            .unwrap();
        conn.store_peer("1.2.3.5", 2, PeerRecord::new(2, 10, Outbound))
            .unwrap();

        conn.remove_peers_with_failures(5).unwrap();
        let peers = conn.load_peers(None).unwrap();
        assert_eq!(peers.len(), 1);
    }

    #[test]
    fn test_query_random_peers_with_time() {
        let conn = setup_db();
        conn.store_peer("1.2.3.4", 1, PeerRecord::new(1, 0, Outbound))
            .unwrap();
        conn.store_peer("1.2.3.5", 2, PeerRecord::new(2, 0, Preferred))
            .unwrap();

        let filter = PeerFilter {
            max_failures: 10,
            before_time: Some(10),
            type_filter: Some(PeerTypeFilter::Equals(Outbound)),
        };
        let peers = conn.query_random_peers(1, &filter).unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].0, "1.2.3.4");
    }

    #[test]
    fn test_invalid_peer_type_errors() {
        let conn = setup_db();
        // Insert a row with an invalid peer type directly via SQL
        conn.execute(
            "INSERT INTO peers (ip, port, nextattempt, numfailures, type) VALUES (?1, ?2, ?3, ?4, ?5)",
            params!["1.2.3.4", 11625i64, 0i64, 0i64, 99i32],
        )
        .unwrap();
        let result = conn.load_peer("1.2.3.4", 11625);
        assert!(result.is_err(), "should reject invalid peer type");
    }
}
