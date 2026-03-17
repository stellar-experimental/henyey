//! Contract event queries.
//!
//! This module provides database operations for contract events,
//! used by the `getEvents` RPC endpoint.

use rusqlite::{params, Connection};

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
    /// Event type: 0=contract, 1=system, 2=diagnostic.
    pub event_type: i32,
    /// Base64 XDR of topic ScVals (up to 4).
    pub topics: Vec<String>,
    /// Base64 XDR of full ContractEvent.
    pub event_xdr: String,
    /// Whether this event was in a successful contract call.
    pub in_successful_contract_call: bool,
}

/// Query trait for contract event operations.
pub trait EventQueries {
    /// Stores a batch of contract events for a ledger.
    fn store_events(&self, events: &[EventRecord]) -> Result<(), DbError>;

    /// Queries events with filters.
    #[allow(clippy::too_many_arguments)]
    fn query_events(
        &self,
        start_ledger: u32,
        end_ledger: Option<u32>,
        event_type: Option<&str>,
        contract_ids: &[String],
        topics: &[Vec<String>],
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Vec<EventRecord>, DbError>;

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
                event.event_type,
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

    fn query_events(
        &self,
        start_ledger: u32,
        end_ledger: Option<u32>,
        event_type: Option<&str>,
        contract_ids: &[String],
        topics: &[Vec<String>],
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Vec<EventRecord>, DbError> {
        let mut sql = String::from(
            "SELECT id, ledgerseq, tx_index, op_index, tx_hash, contract_id, \
             event_type, topic1, topic2, topic3, topic4, event_xdr, \
             in_successful_contract_call FROM events WHERE ledgerseq >= ?",
        );
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        param_values.push(Box::new(start_ledger));

        if let Some(end) = end_ledger {
            sql.push_str(" AND ledgerseq <= ?");
            param_values.push(Box::new(end));
        }

        // Event type filter
        if let Some(et) = event_type {
            let type_code = match et {
                "contract" => 0,
                "system" => 1,
                "diagnostic" => 2,
                _ => 0,
            };
            sql.push_str(" AND event_type = ?");
            param_values.push(Box::new(type_code));
        }

        // Contract ID filter
        if !contract_ids.is_empty() {
            let placeholders: Vec<String> = contract_ids
                .iter()
                .enumerate()
                .map(|_| "?".to_string())
                .collect();
            sql.push_str(&format!(" AND contract_id IN ({})", placeholders.join(",")));
            for cid in contract_ids {
                param_values.push(Box::new(cid.clone()));
            }
        }

        // Topic filters - each topic array is an OR of possible values for that position
        // "*" means wildcard (skip that topic position)
        // "**" means match all remaining positions (stop adding SQL constraints)
        if !topics.is_empty() {
            for (i, topic_alternatives) in topics.iter().enumerate() {
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
                let placeholders: Vec<String> =
                    non_wildcard.iter().map(|_| "?".to_string()).collect();
                sql.push_str(&format!(" AND {} IN ({})", col, placeholders.join(",")));
                for t in non_wildcard {
                    param_values.push(Box::new(t.clone()));
                }
            }
        }

        // Cursor-based pagination
        if let Some(cursor_val) = cursor {
            sql.push_str(" AND id > ?");
            param_values.push(Box::new(cursor_val.to_string()));
        }

        sql.push_str(" ORDER BY id ASC LIMIT ?");
        param_values.push(Box::new(limit));

        let params_refs: Vec<&dyn rusqlite::types::ToSql> =
            param_values.iter().map(|p| p.as_ref()).collect();

        let mut stmt = self.prepare(&sql)?;
        let rows = stmt.query_map(params_refs.as_slice(), |row| {
            let topic1: Option<String> = row.get(7)?;
            let topic2: Option<String> = row.get(8)?;
            let topic3: Option<String> = row.get(9)?;
            let topic4: Option<String> = row.get(10)?;

            let mut topics = Vec::new();
            if let Some(t) = topic1 {
                topics.push(t);
            }
            if let Some(t) = topic2 {
                topics.push(t);
            }
            if let Some(t) = topic3 {
                topics.push(t);
            }
            if let Some(t) = topic4 {
                topics.push(t);
            }

            let in_success: i32 = row.get(12)?;

            Ok(EventRecord {
                id: row.get(0)?,
                ledger_seq: row.get(1)?,
                tx_index: row.get(2)?,
                op_index: row.get(3)?,
                tx_hash: row.get(4)?,
                contract_id: row.get(5)?,
                event_type: row.get(6)?,
                topics,
                event_xdr: row.get(11)?,
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
            event_type: 0,
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
}
