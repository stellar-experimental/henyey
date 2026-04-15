//! Contract event queries.
//!
//! This module provides database operations for contract events,
//! used by the `getEvents` RPC endpoint.

use rusqlite::{params, Connection};
use stellar_xdr::curr::ContractEventType;

use crate::error::DbError;

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
}

/// Query trait for contract event operations.
pub trait EventQueries {
    /// Stores a batch of contract events for a ledger.
    fn store_events(&self, events: &[EventRecord]) -> Result<(), DbError>;

    /// Queries events with filters.
    fn query_events(&self, params: &EventQueryParams) -> Result<Vec<EventRecord>, DbError>;

    /// Deletes events at or below the given ledger sequence.
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
             in_successful_contract_call FROM events WHERE ledgerseq >= ?",
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
        // contract_id(5), event_type(6), topic1..4(7..10), xdr(11), success(12)
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

        let mut stmt = self.prepare(&sql)?;
        let rows = stmt.query_map(params_refs.as_slice(), |row| {
            let mut topics = Vec::new();
            for i in COL_TOPIC_START..=COL_TOPIC_END {
                if let Some(t) = row.get::<_, Option<String>>(i)? {
                    topics.push(t);
                }
            }

            let in_success: i32 = row.get(COL_SUCCESS)?;

            Ok(EventRecord {
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
            })
        })?;

        let results: Result<Vec<EventRecord>, _> = rows.collect();
        Ok(results?)
    }

    fn delete_old_events(&self, max_ledger: u32, count: u32) -> Result<u32, DbError> {
        let deleted = self.execute(
            "DELETE FROM events WHERE rowid IN (SELECT rowid FROM events WHERE ledgerseq <= ?1 LIMIT ?2)",
            params![max_ledger, count],
        )?;
        Ok(deleted as u32)
    }
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
        });
        assert!(result.is_err(), "should reject unknown event type filter");
    }
}
