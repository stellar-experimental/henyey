//! Contract event queries.
//!
//! This module provides database operations for contract events,
//! used by the `getEvents` RPC endpoint.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use rusqlite::{params, Connection};
use stellar_xdr::curr::ContractEventType;

use crate::error::DbError;

/// RAII guard that clears the SQLite progress handler on drop.
/// Ensures pooled connections are never left with a stale handler.
struct ProgressHandlerGuard<'a> {
    conn: &'a Connection,
}

impl Drop for ProgressHandlerGuard<'_> {
    fn drop(&mut self) {
        self.conn.progress_handler(0, None::<fn() -> bool>);
    }
}

/// A stored contract event record.
#[derive(Debug, Clone)]
pub struct EventRecord {
    /// TOID-based event ID (e.g. "0006320903369523200-0000000001")
    pub id: String,
    /// Ledger sequence where the event occurred.
    pub ledger_seq: u32,
    /// Transaction index within the ledger.
    pub tx_index: u32,
    /// Operation index within the transaction.
    pub op_index: u32,
    /// Transaction hash (hex).
    pub tx_hash: String,
    /// Contract ID (strkey C...), None for system events.
    pub contract_id: Option<String>,
    /// Event type (contract, system, or diagnostic).
    pub event_type: ContractEventType,
    /// Base64 XDR of topic ScVals (up to 4).
    pub topics: Vec<String>,
    /// Base64 XDR of full ContractEvent.
    pub event_xdr: String,
    /// Whether this event was in a successful contract call.
    pub in_successful_contract_call: bool,
}

/// Parameters for querying contract events.
///
/// Groups the filter and pagination fields needed by
/// [`EventQueries::query_events`] to avoid a long parameter list.
pub struct EventQueryParams<'a> {
    pub start_ledger: u32,
    pub end_ledger: Option<u32>,
    pub event_type: Option<&'a str>,
    pub contract_ids: &'a [String],
    pub topics: &'a [Vec<String>],
    pub cursor: Option<&'a str>,
    pub limit: u32,
    /// Maximum cumulative stored bytes (event_xdr + topics) to load.
    /// The first row is always returned regardless of size.
    pub max_total_bytes: usize,
    /// Maximum SQLite VM opcodes allowed for this query (0 = no limit).
    /// When exceeded, the query is interrupted and `DbError::QueryBudgetExceeded`
    /// is returned.
    pub max_query_ops: u32,
}

/// Query trait for contract event operations.
pub trait EventQueries {
    /// Stores a batch of contract events for a ledger.
    fn store_events(&self, events: &[EventRecord]) -> Result<(), DbError>;

    /// Queries events with filters.
    fn query_events(&self, params: &EventQueryParams) -> Result<Vec<EventRecord>, DbError>;

    /// Deletes events at or below the given ledger sequence.
    /// `count` limits the number of distinct ledgers deleted per call.
    fn delete_old_events(&self, max_ledger: u32, count: u32) -> Result<u32, DbError>;
}

impl EventQueries for Connection {
    fn store_events(&self, events: &[EventRecord]) -> Result<(), DbError> {
        let mut stmt = self.prepare(
            r#"INSERT OR REPLACE INTO events
               (id, ledgerseq, tx_index, op_index, tx_hash, contract_id,
                event_type, topic1, topic2, topic3, topic4, event_xdr,
                in_successful_contract_call)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"#,
        )?;

        for event in events {
            let topics = &event.topics;
            stmt.execute(params![
                event.id,
                event.ledger_seq,
                event.tx_index,
                event.op_index,
                event.tx_hash,
                event.contract_id,
                event.event_type as i32,
                topics.first().map(|s| s.as_str()),
                topics.get(1).map(|s| s.as_str()),
                topics.get(2).map(|s| s.as_str()),
                topics.get(3).map(|s| s.as_str()),
                event.event_xdr,
                event.in_successful_contract_call as i32,
            ])?;
        }

        Ok(())
    }

    fn query_events(&self, params: &EventQueryParams) -> Result<Vec<EventRecord>, DbError> {
        let mut sql = String::from(
            "SELECT id, ledgerseq, tx_index, op_index, tx_hash, contract_id, \
             event_type, topic1, topic2, topic3, topic4, event_xdr, \
             in_successful_contract_call, \
             length(event_xdr) + COALESCE(length(topic1),0) + COALESCE(length(topic2),0) \
             + COALESCE(length(topic3),0) + COALESCE(length(topic4),0) \
             FROM events WHERE ledgerseq >= ?",
        );
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        param_values.push(Box::new(params.start_ledger));

        if let Some(end) = params.end_ledger {
            sql.push_str(" AND ledgerseq <= ?");
            param_values.push(Box::new(end));
        }

        // Event type filter
        if let Some(et) = params.event_type {
            let type_code = match et {
                "contract" => ContractEventType::Contract as i32,
                "system" => ContractEventType::System as i32,
                "diagnostic" => ContractEventType::Diagnostic as i32,
                other => {
                    return Err(DbError::Integrity(format!(
                        "unknown event type filter: {other}"
                    )));
                }
            };
            sql.push_str(" AND event_type = ?");
            param_values.push(Box::new(type_code));
        }

        // Contract ID filter
        if !params.contract_ids.is_empty() {
            let placeholders = super::sql_placeholder_list(params.contract_ids.len());
            sql.push_str(&format!(" AND contract_id IN ({placeholders})"));
            for cid in params.contract_ids {
                param_values.push(Box::new(cid.clone()));
            }
        }

        // Topic filters - each topic array is an OR of possible values for that position
        // "*" means wildcard (skip that topic position)
        // "**" means match all remaining positions (stop adding SQL constraints)
        if !params.topics.is_empty() {
            for (i, topic_alternatives) in params.topics.iter().enumerate() {
                // "**" means "match all remaining positions" — stop filtering here
                if topic_alternatives.iter().any(|t| t.as_str() == "**") {
                    break;
                }
                let col = match i {
                    0 => "topic1",
                    1 => "topic2",
                    2 => "topic3",
                    3 => "topic4",
                    _ => continue,
                };
                // Filter out single-segment wildcards
                let non_wildcard: Vec<&String> = topic_alternatives
                    .iter()
                    .filter(|t| t.as_str() != "*")
                    .collect();
                if non_wildcard.is_empty() {
                    continue; // all wildcards, no filter needed
                }
                let placeholders = super::sql_placeholder_list(non_wildcard.len());
                sql.push_str(&format!(" AND {} IN ({})", col, placeholders));
                for t in non_wildcard {
                    param_values.push(Box::new(t.clone()));
                }
            }
        }

        // Cursor-based pagination
        if let Some(cursor_val) = params.cursor {
            sql.push_str(" AND id > ?");
            param_values.push(Box::new(cursor_val.to_string()));
        }

        sql.push_str(" ORDER BY id ASC LIMIT ?");
        param_values.push(Box::new(params.limit));

        let params_refs: Vec<&dyn rusqlite::types::ToSql> =
            param_values.iter().map(|p| p.as_ref()).collect();

        // Column indices matching the SELECT:
        // id(0), ledger(1), tx_idx(2), op_idx(3), tx_hash(4),
        // contract_id(5), event_type(6), topic1..4(7..10), xdr(11),
        // success(12), row_bytes(13)
        const COL_ID: usize = 0;
        const COL_LEDGER: usize = 1;
        const COL_TX_INDEX: usize = 2;
        const COL_OP_INDEX: usize = 3;
        const COL_TX_HASH: usize = 4;
        const COL_CONTRACT_ID: usize = 5;
        const COL_EVENT_TYPE: usize = 6;
        const COL_TOPIC_START: usize = 7;
        const COL_TOPIC_END: usize = 10;
        const COL_EVENT_XDR: usize = 11;
        const COL_SUCCESS: usize = 12;
        const COL_ROW_BYTES: usize = 13;

        // Install progress handler if budget is configured.
        let _guard = if params.max_query_ops > 0 {
            let budget = params.max_query_ops;
            let counter = Arc::new(AtomicU32::new(0));
            let counter_clone = counter.clone();
            // Check every 1000 VM opcodes; interrupt when budget exceeded.
            self.progress_handler(
                1000,
                Some(move || {
                    let count = counter_clone.fetch_add(1, Ordering::Relaxed);
                    // Each callback = 1000 ops, so total ops = (count+1) * 1000
                    (count + 1) * 1000 >= budget
                }),
            );
            Some(ProgressHandlerGuard { conn: self })
        } else {
            None
        };

        let mut stmt = match self.prepare(&sql) {
            Ok(s) => s,
            Err(e) if is_interrupted(&e) => return Err(DbError::QueryBudgetExceeded),
            Err(e) => return Err(e.into()),
        };
        let mut rows = match stmt.query(params_refs.as_slice()) {
            Ok(r) => r,
            Err(e) if is_interrupted(&e) => return Err(DbError::QueryBudgetExceeded),
            Err(e) => return Err(e.into()),
        };
        let mut results = Vec::new();
        let mut cumulative_bytes: usize = 0;

        loop {
            match rows.next() {
                Ok(Some(row)) => {
                    let row_bytes: i64 = row.get(COL_ROW_BYTES)?;
                    let row_bytes = row_bytes.max(0) as usize;

                    // First row always returned for forward progress; subsequent
                    // rows are subject to the cumulative byte budget.
                    if !results.is_empty() && cumulative_bytes + row_bytes > params.max_total_bytes
                    {
                        break;
                    }

                    let mut topics = Vec::new();
                    for i in COL_TOPIC_START..=COL_TOPIC_END {
                        if let Some(t) = row.get::<_, Option<String>>(i)? {
                            topics.push(t);
                        }
                    }

                    let in_success: i32 = row.get(COL_SUCCESS)?;

                    let record = EventRecord {
                        id: row.get(COL_ID)?,
                        ledger_seq: row.get(COL_LEDGER)?,
                        tx_index: row.get(COL_TX_INDEX)?,
                        op_index: row.get(COL_OP_INDEX)?,
                        tx_hash: row.get(COL_TX_HASH)?,
                        contract_id: row.get(COL_CONTRACT_ID)?,
                        event_type: {
                            let raw: i32 = row.get(COL_EVENT_TYPE)?;
                            ContractEventType::try_from(raw).map_err(|_| {
                                rusqlite::Error::FromSqlConversionFailure(
                                    COL_EVENT_TYPE,
                                    rusqlite::types::Type::Integer,
                                    format!("invalid event type: {raw}").into(),
                                )
                            })?
                        },
                        topics,
                        event_xdr: row.get(COL_EVENT_XDR)?,
                        in_successful_contract_call: in_success != 0,
                    };
                    cumulative_bytes += row_bytes;
                    results.push(record);
                }
                Ok(None) => break,
                Err(e) if is_interrupted(&e) => return Err(DbError::QueryBudgetExceeded),
                Err(e) => return Err(e.into()),
            }
        }

        Ok(results)
    }

    fn delete_old_events(&self, max_ledger: u32, count: u32) -> Result<u32, DbError> {
        // Use DISTINCT ledgerseq to ensure we never split a single ledger's
        // events across GC boundaries. `count` limits distinct ledgers deleted.
        let deleted = self.execute(
            "DELETE FROM events WHERE ledgerseq IN (\
                SELECT DISTINCT ledgerseq FROM events \
                WHERE ledgerseq <= ?1 \
                ORDER BY ledgerseq ASC LIMIT ?2\
            )",
            params![max_ledger, count],
        )?;
        Ok(deleted as u32)
    }
}

/// Check if a rusqlite error is an SQLITE_INTERRUPT (from progress handler).
fn is_interrupted(err: &rusqlite::Error) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE events (
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
            CREATE INDEX events_ledger ON events(ledgerseq);
            "#,
        )
        .unwrap();
        conn
    }

    fn make_event(ledger_seq: u32, index: u32) -> EventRecord {
        EventRecord {
            id: format!("{ledger_seq}-{index}"),
            ledger_seq,
            tx_index: 0,
            op_index: index,
            tx_hash: "aabb".to_string(),
            contract_id: Some("CABC".to_string()),
            event_type: ContractEventType::Contract,
            topics: vec!["t1".to_string()],
            event_xdr: "deadbeef".to_string(),
            in_successful_contract_call: true,
        }
    }

    fn count_events(conn: &Connection) -> u32 {
        conn.query_row("SELECT COUNT(*) FROM events", [], |r| r.get::<_, u32>(0))
            .unwrap()
    }

    #[test]
    fn test_delete_old_events_inclusive_boundary() {
        // Regression: delete_old_events must use <= (not <) so events at
        // exactly max_ledger are deleted, matching all other delete functions.
        let conn = setup_db();
        let events: Vec<EventRecord> = (10..=14).map(|s| make_event(s, 0)).collect();
        conn.store_events(&events).unwrap();
        assert_eq!(count_events(&conn), 5);

        // Delete events at or below ledger 12
        let deleted = conn.delete_old_events(12, 1000).unwrap();
        assert_eq!(deleted, 3); // ledgers 10, 11, 12
        assert_eq!(count_events(&conn), 2); // ledgers 13, 14 remain
    }

    #[test]
    fn test_delete_old_events_respects_limit() {
        let conn = setup_db();
        let events: Vec<EventRecord> = (1..=10).map(|s| make_event(s, 0)).collect();
        conn.store_events(&events).unwrap();

        // Delete at most 3 events
        let deleted = conn.delete_old_events(10, 3).unwrap();
        assert_eq!(deleted, 3);
        assert_eq!(count_events(&conn), 7);
    }

    #[test]
    fn test_delete_old_events_empty_table() {
        let conn = setup_db();
        let deleted = conn.delete_old_events(100, 1000).unwrap();
        assert_eq!(deleted, 0);
    }

    #[test]
    fn test_delete_old_events_none_below_threshold() {
        let conn = setup_db();
        let events: Vec<EventRecord> = (100..=105).map(|s| make_event(s, 0)).collect();
        conn.store_events(&events).unwrap();

        let deleted = conn.delete_old_events(50, 1000).unwrap();
        assert_eq!(deleted, 0);
        assert_eq!(count_events(&conn), 6);
    }

    #[test]
    fn test_delete_old_events_no_partial_ledger() {
        // Regression for #1724: GC must never split a single ledger's events.
        let conn = setup_db();
        // Insert multiple events per ledger
        let mut events = Vec::new();
        for i in 0..3 {
            events.push(make_event(10, i)); // 3 events at ledger 10
        }
        for i in 0..2 {
            events.push(make_event(11, i)); // 2 events at ledger 11
        }
        for i in 0..4 {
            events.push(make_event(12, i)); // 4 events at ledger 12
        }
        conn.store_events(&events).unwrap();
        assert_eq!(count_events(&conn), 9);

        // Delete with count=1 (1 distinct ledger). Oldest ledger (10) should be
        // fully removed — all 3 events — with no partial remnants.
        let deleted = conn.delete_old_events(12, 1).unwrap();
        assert_eq!(deleted, 3); // all 3 events from ledger 10
        assert_eq!(count_events(&conn), 6); // ledgers 11 + 12 intact

        // Delete another ledger
        let deleted = conn.delete_old_events(12, 1).unwrap();
        assert_eq!(deleted, 2); // all 2 events from ledger 11
        assert_eq!(count_events(&conn), 4); // ledger 12 intact
    }

    #[test]
    fn test_invalid_event_type_errors() {
        let conn = setup_db();
        // Insert a row with an invalid event type directly via SQL
        conn.execute(
            "INSERT INTO events (id, ledgerseq, tx_index, op_index, tx_hash, contract_id, event_type, \
             topic1, topic2, topic3, topic4, event_xdr, in_successful_contract_call) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, NULL, NULL, ?9, ?10)",
            params!["bad-1", 100u32, 0u32, 0u32, "aabb", "CABC", 99i32, "t1", "deadbeef", 1i32],
        )
        .unwrap();
        let result = conn.query_events(&EventQueryParams {
            start_ledger: 99,
            end_ledger: Some(101),
            event_type: None,
            contract_ids: &[],
            topics: &[],
            cursor: None,
            limit: 100,
            max_total_bytes: usize::MAX,
            max_query_ops: 0,
        });
        assert!(result.is_err(), "should reject invalid event type from DB");
    }

    #[test]
    fn test_unknown_event_type_filter_errors() {
        let conn = setup_db();
        let result = conn.query_events(&EventQueryParams {
            start_ledger: 1,
            end_ledger: Some(100),
            event_type: Some("bogus"),
            contract_ids: &[],
            topics: &[],
            cursor: None,
            limit: 100,
            max_total_bytes: usize::MAX,
            max_query_ops: 0,
        });
        assert!(result.is_err(), "should reject unknown event type filter");
    }

    /// Helper to create an event with a specific XDR payload size.
    fn make_event_sized(ledger_seq: u32, index: u32, xdr_len: usize) -> EventRecord {
        let xdr = "x".repeat(xdr_len);
        EventRecord {
            id: format!("{ledger_seq}-{index:04}"),
            ledger_seq,
            tx_index: 0,
            op_index: index,
            tx_hash: "aabb".to_string(),
            contract_id: Some("CABC".to_string()),
            event_type: ContractEventType::Contract,
            topics: vec!["topic1".to_string()],
            event_xdr: xdr,
            in_successful_contract_call: true,
        }
    }

    /// Compute the stored row bytes for a make_event_sized event.
    fn row_bytes_for_sized(xdr_len: usize) -> usize {
        // event_xdr length + topic1 length ("topic1" = 6 chars)
        xdr_len + 6
    }

    #[test]
    fn test_query_events_bounded_truncation() {
        let conn = setup_db();
        let xdr_len = 100;
        let events: Vec<EventRecord> = (0..5).map(|i| make_event_sized(10, i, xdr_len)).collect();
        conn.store_events(&events).unwrap();

        let per_row = row_bytes_for_sized(xdr_len);
        // Budget fits 2 rows but not 3
        let budget = per_row * 2 + per_row / 2;

        let result = conn
            .query_events(&EventQueryParams {
                start_ledger: 1,
                end_ledger: Some(100),
                event_type: None,
                contract_ids: &[],
                topics: &[],
                cursor: None,
                limit: 100,
                max_total_bytes: budget,
                max_query_ops: 0,
            })
            .unwrap();
        assert_eq!(result.len(), 2, "should truncate to 2 events");
        assert_eq!(result[0].id, "10-0000");
        assert_eq!(result[1].id, "10-0001");
    }

    #[test]
    fn test_query_events_bounded_first_row_always_returned() {
        let conn = setup_db();
        let events = vec![make_event_sized(10, 0, 500)];
        conn.store_events(&events).unwrap();

        let result = conn
            .query_events(&EventQueryParams {
                start_ledger: 1,
                end_ledger: Some(100),
                event_type: None,
                contract_ids: &[],
                topics: &[],
                cursor: None,
                limit: 100,
                max_total_bytes: 1, // budget smaller than event
                max_query_ops: 0,
            })
            .unwrap();
        assert_eq!(
            result.len(),
            1,
            "first row always returned for forward progress"
        );
    }

    #[test]
    fn test_query_events_bounded_unlimited() {
        let conn = setup_db();
        let events: Vec<EventRecord> = (0..10).map(|i| make_event_sized(10, i, 100)).collect();
        conn.store_events(&events).unwrap();

        let result = conn
            .query_events(&EventQueryParams {
                start_ledger: 1,
                end_ledger: Some(100),
                event_type: None,
                contract_ids: &[],
                topics: &[],
                cursor: None,
                limit: 100,
                max_total_bytes: usize::MAX,
                max_query_ops: 0,
            })
            .unwrap();
        assert_eq!(result.len(), 10, "unlimited budget returns all events");
    }

    #[test]
    fn test_query_events_bounded_preserves_filters() {
        let conn = setup_db();
        // Mix of event types and contract IDs
        let mut e1 = make_event_sized(10, 0, 50);
        e1.event_type = ContractEventType::System;
        e1.contract_id = Some("CSYS".to_string());

        let mut e2 = make_event_sized(10, 1, 50);
        e2.contract_id = Some("COTHER".to_string());

        let e3 = make_event_sized(10, 2, 50); // Contract type, CABC

        conn.store_events(&[e1, e2, e3]).unwrap();

        // Filter by event type = contract (should skip e1 which is system)
        let result = conn
            .query_events(&EventQueryParams {
                start_ledger: 1,
                end_ledger: Some(100),
                event_type: Some("contract"),
                contract_ids: &[],
                topics: &[],
                cursor: None,
                limit: 100,
                max_total_bytes: usize::MAX,
                max_query_ops: 0,
            })
            .unwrap();
        assert_eq!(result.len(), 2);

        // Filter by contract ID
        let cid = vec!["CABC".to_string()];
        let result = conn
            .query_events(&EventQueryParams {
                start_ledger: 1,
                end_ledger: Some(100),
                event_type: None,
                contract_ids: &cid,
                topics: &[],
                cursor: None,
                limit: 100,
                max_total_bytes: usize::MAX,
                max_query_ops: 0,
            })
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "10-0002");

        // Cursor-based pagination
        let result = conn
            .query_events(&EventQueryParams {
                start_ledger: 1,
                end_ledger: Some(100),
                event_type: None,
                contract_ids: &[],
                topics: &[],
                cursor: Some("10-0001"),
                limit: 100,
                max_total_bytes: usize::MAX,
                max_query_ops: 0,
            })
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "10-0002");
    }

    #[test]
    fn test_query_events_budget_interrupts_expensive_scan() {
        let conn = setup_db();
        // Insert many events so a full scan is expensive
        let events: Vec<EventRecord> = (0..500)
            .map(|i| {
                let mut e = make_event_sized(10, i, 100);
                e.topics = vec![format!("topic-{}", i % 50)]; // diverse topics
                e
            })
            .collect();
        conn.store_events(&events).unwrap();

        // Query with a nonexistent topic1 — forces full scan
        let topics = vec![vec!["nonexistent-topic-xyz".to_string()]];
        let result = conn.query_events(&EventQueryParams {
            start_ledger: 1,
            end_ledger: Some(100),
            event_type: None,
            contract_ids: &[],
            topics: &topics,
            cursor: None,
            limit: 100,
            max_total_bytes: usize::MAX,
            max_query_ops: 100, // very low budget — will be exceeded
        });
        assert!(
            matches!(result, Err(DbError::QueryBudgetExceeded)),
            "expected QueryBudgetExceeded, got {:?}",
            result
        );
    }

    #[test]
    fn test_query_events_budget_normal_query_succeeds() {
        let conn = setup_db();
        let events: Vec<EventRecord> = (0..5).map(|i| make_event_sized(10, i, 50)).collect();
        conn.store_events(&events).unwrap();

        // Normal query with generous budget succeeds
        let result = conn
            .query_events(&EventQueryParams {
                start_ledger: 1,
                end_ledger: Some(100),
                event_type: None,
                contract_ids: &[],
                topics: &[],
                cursor: None,
                limit: 100,
                max_total_bytes: usize::MAX,
                max_query_ops: 5_000_000,
            })
            .unwrap();
        assert_eq!(result.len(), 5);
    }

    #[test]
    fn test_query_events_budget_guard_cleanup() {
        let conn = setup_db();
        let events: Vec<EventRecord> = (0..500)
            .map(|i| {
                let mut e = make_event_sized(10, i, 100);
                e.topics = vec![format!("topic-{}", i % 50)];
                e
            })
            .collect();
        conn.store_events(&events).unwrap();

        // First query: exceeds budget
        let topics = vec![vec!["nonexistent-topic-xyz".to_string()]];
        let result = conn.query_events(&EventQueryParams {
            start_ledger: 1,
            end_ledger: Some(100),
            event_type: None,
            contract_ids: &[],
            topics: &topics,
            cursor: None,
            limit: 100,
            max_total_bytes: usize::MAX,
            max_query_ops: 100,
        });
        assert!(matches!(result, Err(DbError::QueryBudgetExceeded)));

        // Second query on same connection: should work fine (handler cleared)
        let result = conn
            .query_events(&EventQueryParams {
                start_ledger: 1,
                end_ledger: Some(100),
                event_type: None,
                contract_ids: &[],
                topics: &[],
                cursor: None,
                limit: 10,
                max_total_bytes: usize::MAX,
                max_query_ops: 0, // no budget
            })
            .unwrap();
        assert!(
            !result.is_empty(),
            "connection should be usable after budget exceeded"
        );
    }

    #[test]
    fn test_query_events_budget_zero_means_unlimited() {
        let conn = setup_db();
        let events: Vec<EventRecord> = (0..100).map(|i| make_event_sized(10, i, 50)).collect();
        conn.store_events(&events).unwrap();

        // max_query_ops = 0 means no limit
        let result = conn
            .query_events(&EventQueryParams {
                start_ledger: 1,
                end_ledger: Some(100),
                event_type: None,
                contract_ids: &[],
                topics: &[],
                cursor: None,
                limit: 200,
                max_total_bytes: usize::MAX,
                max_query_ops: 0,
            })
            .unwrap();
        assert_eq!(result.len(), 100);
    }
}
