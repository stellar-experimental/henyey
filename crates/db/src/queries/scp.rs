//! SCP (Stellar Consensus Protocol) history queries.
//!
//! This module provides database operations for SCP consensus state, including:
//!
//! - SCP envelopes: The signed consensus messages exchanged by validators
//! - Quorum sets: The trust configurations that define consensus requirements
//! - Slot state persistence: Crash recovery for ongoing consensus
//!
//! SCP history is used for:
//! - Catchup verification (proving ledger agreement)
//! - Debugging consensus issues
//! - History archive publishing
//! - Crash recovery (resuming consensus after restart)

use henyey_common::xdr_stream::XdrOutputStream;
use henyey_common::Hash256;
use rusqlite::{params, Connection, OptionalExtension};
use stellar_xdr::curr::{
    Hash, LedgerScpMessages, Limits, NodeId, PublicKey, ReadXdr, ScpEnvelope, ScpHistoryEntry,
    ScpHistoryEntryV0, ScpQuorumSet, Uint256, WriteXdr,
};

use crate::error::DbError;
use crate::schema::state_keys;

const TX_SET_KEY_PREFIX: &str = "txset:";

/// Query trait for SCP consensus state operations.
///
/// Provides methods for persisting and retrieving SCP envelopes and quorum sets.
pub trait ScpQueries {
    /// Stores SCP envelopes for a ledger.
    ///
    /// Replaces any existing envelopes for the ledger. Envelopes are stored
    /// sorted by node ID for deterministic ordering.
    fn store_scp_history(&self, ledger_seq: u32, envelopes: &[ScpEnvelope]) -> Result<(), DbError>;

    /// Loads SCP envelopes for a ledger.
    ///
    /// Returns envelopes sorted by node ID.
    fn load_scp_history(&self, ledger_seq: u32) -> Result<Vec<ScpEnvelope>, DbError>;

    /// Stores a quorum set by its hash.
    ///
    /// If the quorum set already exists, only updates the last-seen ledger
    /// sequence if the new value is higher. This allows garbage collection
    /// of old quorum sets that are no longer referenced.
    fn store_scp_quorum_set(
        &self,
        hash: &Hash256,
        last_ledger_seq: u32,
        quorum_set: &ScpQuorumSet,
    ) -> Result<(), DbError>;

    /// Loads a quorum set by its hash.
    ///
    /// Returns `None` if no quorum set with the given hash is stored.
    fn load_scp_quorum_set(&self, hash: &Hash256) -> Result<Option<ScpQuorumSet>, DbError>;

    /// Copy SCP history entries to an XDR output stream.
    ///
    /// Builds `ScpHistoryEntry::V0` records for ledger sequences
    /// `[begin, begin + count)` by combining SCP envelopes and their
    /// referenced quorum sets. Returns the number of entries written.
    fn copy_scp_history_to_stream(
        &self,
        begin: u32,
        count: u32,
        stream: &mut XdrOutputStream,
    ) -> Result<usize, DbError>;

    /// Deletes old SCP history entries up to and including `max_ledger`.
    ///
    /// Removes at most `count` entries from both the scphistory and scpquorums
    /// tables to limit the amount of work per call.
    /// Returns the total number of entries deleted (history + quorums).
    ///
    /// This is used by the Maintainer to garbage collect old SCP state.
    fn delete_old_scp_entries(&self, max_ledger: u32, count: u32) -> Result<u32, DbError>;
}

impl ScpQueries for Connection {
    fn store_scp_history(&self, ledger_seq: u32, envelopes: &[ScpEnvelope]) -> Result<(), DbError> {
        self.execute(
            "DELETE FROM scphistory WHERE ledgerseq = ?1",
            params![ledger_seq],
        )?;

        if envelopes.is_empty() {
            return Ok(());
        }

        let mut ordered = envelopes.to_vec();
        ordered.sort_by_key(|env| node_id_hex(&env.statement.node_id));

        for envelope in ordered {
            let node_id = node_id_hex(&envelope.statement.node_id);
            let data = envelope.to_xdr(Limits::none())?;
            self.execute(
                "INSERT INTO scphistory (nodeid, ledgerseq, envelope) VALUES (?1, ?2, ?3)",
                params![node_id, ledger_seq, data],
            )?;
        }

        Ok(())
    }

    fn load_scp_history(&self, ledger_seq: u32) -> Result<Vec<ScpEnvelope>, DbError> {
        let mut stmt =
            self.prepare("SELECT envelope FROM scphistory WHERE ledgerseq = ?1 ORDER BY nodeid")?;
        let rows = stmt.query_map(params![ledger_seq], |row| row.get::<_, Vec<u8>>(0))?;
        let mut envelopes = Vec::new();
        for row in rows {
            let data = row?;
            let envelope = ScpEnvelope::from_xdr(data.as_slice(), Limits::none())?;
            envelopes.push(envelope);
        }
        Ok(envelopes)
    }

    fn store_scp_quorum_set(
        &self,
        hash: &Hash256,
        last_ledger_seq: u32,
        quorum_set: &ScpQuorumSet,
    ) -> Result<(), DbError> {
        let hash_hex = hash.to_hex();
        let existing: Option<u32> = self
            .query_row(
                "SELECT lastledgerseq FROM scpquorums WHERE qsethash = ?1",
                params![hash_hex],
                |row| row.get(0),
            )
            .optional()?;

        if let Some(last_seen) = existing {
            if last_seen >= last_ledger_seq {
                return Ok(());
            }
            self.execute(
                "UPDATE scpquorums SET lastledgerseq = ?1 WHERE qsethash = ?2",
                params![last_ledger_seq, hash_hex],
            )?;
            return Ok(());
        }

        let data = quorum_set.to_xdr(Limits::none())?;
        self.execute(
            "INSERT INTO scpquorums (qsethash, lastledgerseq, qset) VALUES (?1, ?2, ?3)",
            params![hash_hex, last_ledger_seq, data],
        )?;
        Ok(())
    }

    fn load_scp_quorum_set(&self, hash: &Hash256) -> Result<Option<ScpQuorumSet>, DbError> {
        let hash_hex = hash.to_hex();
        let result: Option<Vec<u8>> = self
            .query_row(
                "SELECT qset FROM scpquorums WHERE qsethash = ?1",
                params![hash_hex],
                |row| row.get(0),
            )
            .optional()?;
        result
            .map(|data| {
                ScpQuorumSet::from_xdr(data.as_slice(), Limits::none()).map_err(DbError::from)
            })
            .transpose()
    }

    fn copy_scp_history_to_stream(
        &self,
        begin: u32,
        count: u32,
        stream: &mut XdrOutputStream,
    ) -> Result<usize, DbError> {
        let end = begin.saturating_add(count);
        let mut written = 0usize;

        // Get distinct ledger sequences in range that have SCP history
        let mut seq_stmt = self.prepare(
            "SELECT DISTINCT ledgerseq FROM scphistory WHERE ledgerseq >= ?1 AND ledgerseq < ?2 ORDER BY ledgerseq ASC",
        )?;
        let ledger_seqs: Vec<u32> = seq_stmt
            .query_map(params![begin, end], |row| row.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        for ledger_seq in ledger_seqs {
            // Load envelopes for this ledger
            let envelopes = self.load_scp_history(ledger_seq)?;
            if envelopes.is_empty() {
                continue;
            }

            // Collect referenced quorum set hashes
            let mut qset_hashes = std::collections::HashSet::new();
            for env in &envelopes {
                let hash = scp_envelope_quorum_set_hash(env);
                qset_hashes.insert(hash);
            }

            // Load referenced quorum sets
            let mut quorum_sets = Vec::new();
            for hash in &qset_hashes {
                if let Some(qset) = self.load_scp_quorum_set(hash)? {
                    quorum_sets.push(qset);
                }
            }

            // Build ScpHistoryEntry::V0
            let entry = ScpHistoryEntry::V0(ScpHistoryEntryV0 {
                quorum_sets: quorum_sets.try_into().map_err(|_| {
                    DbError::Integrity("too many quorum sets for XDR vec".to_string())
                })?,
                ledger_messages: LedgerScpMessages {
                    ledger_seq,
                    messages: envelopes.try_into().map_err(|_| {
                        DbError::Integrity("too many SCP messages for XDR vec".to_string())
                    })?,
                },
            });

            stream
                .write_one(&entry)
                .map_err(|e| DbError::Integrity(format!("Failed to write SCP entry: {}", e)))?;
            written += 1;
        }

        Ok(written)
    }

    fn delete_old_scp_entries(&self, max_ledger: u32, count: u32) -> Result<u32, DbError> {
        // Delete old SCP history entries
        // Note: scphistory may have multiple rows per ledger (one per node)
        let history_deleted = self.execute(
            r#"
            DELETE FROM scphistory
            WHERE rowid IN (
                SELECT rowid FROM scphistory
                WHERE ledgerseq <= ?1
                ORDER BY ledgerseq ASC
                LIMIT ?2
            )
            "#,
            params![max_ledger, count],
        )?;

        // Delete old quorum sets that are no longer needed
        // Only delete quorums whose lastledgerseq is below the threshold
        let quorums_deleted = self.execute(
            r#"
            DELETE FROM scpquorums
            WHERE qsethash IN (
                SELECT qsethash FROM scpquorums
                WHERE lastledgerseq <= ?1
                ORDER BY lastledgerseq ASC
                LIMIT ?2
            )
            "#,
            params![max_ledger, count],
        )?;

        Ok((history_deleted + quorums_deleted) as u32)
    }
}

/// Extract the quorum set hash from an SCP envelope's statement.
fn scp_envelope_quorum_set_hash(envelope: &ScpEnvelope) -> Hash256 {
    let hash = match &envelope.statement.pledges {
        stellar_xdr::curr::ScpStatementPledges::Nominate(nom) => &nom.quorum_set_hash,
        stellar_xdr::curr::ScpStatementPledges::Prepare(prep) => &prep.quorum_set_hash,
        stellar_xdr::curr::ScpStatementPledges::Confirm(conf) => &conf.quorum_set_hash,
        stellar_xdr::curr::ScpStatementPledges::Externalize(ext) => &ext.commit_quorum_set_hash,
    };
    Hash256::from_bytes(hash.0)
}

/// Converts a NodeId to a hex string for database storage and sorting.
fn node_id_hex(node_id: &NodeId) -> String {
    match &node_id.0 {
        PublicKey::PublicKeyTypeEd25519(Uint256(bytes)) => hex::encode(bytes),
    }
}

fn scp_slot_state_key(slot: u64) -> String {
    format!("{}:{slot}", state_keys::SCP_STATE)
}

fn tx_set_key(hash: &Hash) -> String {
    format!("{TX_SET_KEY_PREFIX}{}", hex::encode(hash.0))
}

fn set_storestate_value(conn: &Connection, key: &str, value: &str) -> Result<(), DbError> {
    conn.execute(
        "INSERT OR REPLACE INTO storestate (statename, state) VALUES (?1, ?2)",
        params![key, value],
    )?;
    Ok(())
}

fn load_storestate_value(conn: &Connection, key: &str) -> Result<Option<String>, DbError> {
    let result = conn
        .query_row(
            "SELECT state FROM storestate WHERE statename = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()?;
    Ok(result)
}

fn delete_storestate_value(conn: &Connection, key: &str) -> Result<(), DbError> {
    conn.execute("DELETE FROM storestate WHERE statename = ?1", params![key])?;
    Ok(())
}

fn parse_slot_key(key: &str) -> Option<u64> {
    key.strip_prefix(&format!("{}:", state_keys::SCP_STATE))?
        .parse()
        .ok()
}

fn decode_tx_set_data(encoded: &str) -> Result<Vec<u8>, DbError> {
    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded)
        .map_err(|e| DbError::Integrity(format!("Invalid base64 tx set data: {}", e)))
}

/// Extended query trait for SCP state persistence (crash recovery).
///
/// These methods support the herder's SCP state persistence for crash recovery.
pub trait ScpStatePersistenceQueries {
    /// Save SCP state for a slot as JSON.
    fn save_scp_slot_state(&self, slot: u64, state_json: &str) -> Result<(), DbError>;

    /// Load SCP state for a slot.
    fn load_scp_slot_state(&self, slot: u64) -> Result<Option<String>, DbError>;

    /// Load all SCP slot states.
    fn load_all_scp_slot_states(&self) -> Result<Vec<(u64, String)>, DbError>;

    /// Delete SCP state for slots below the given threshold.
    fn delete_scp_slot_states_below(&self, slot: u64) -> Result<(), DbError>;

    /// Save a transaction set by hash.
    fn save_tx_set_data(&self, hash: &Hash, data: &[u8]) -> Result<(), DbError>;

    /// Load a transaction set by hash.
    fn load_tx_set_data(&self, hash: &Hash) -> Result<Option<Vec<u8>>, DbError>;

    /// Load all transaction sets.
    fn load_all_tx_set_data(&self) -> Result<Vec<(Hash, Vec<u8>)>, DbError>;

    /// Check if a transaction set exists.
    fn has_tx_set_data(&self, hash: &Hash) -> Result<bool, DbError>;

    /// Delete old transaction set data.
    /// Note: This is a no-op in the simple implementation since tx sets
    /// aren't directly linked to slots. Use a separate cleanup mechanism.
    fn delete_old_tx_set_data(&self, slot: u64) -> Result<(), DbError>;
}

impl ScpStatePersistenceQueries for Connection {
    fn save_scp_slot_state(&self, slot: u64, state_json: &str) -> Result<(), DbError> {
        set_storestate_value(self, &scp_slot_state_key(slot), state_json)
    }

    fn load_scp_slot_state(&self, slot: u64) -> Result<Option<String>, DbError> {
        load_storestate_value(self, &scp_slot_state_key(slot))
    }

    fn load_all_scp_slot_states(&self) -> Result<Vec<(u64, String)>, DbError> {
        let mut stmt = self.prepare(
            "SELECT statename, state FROM storestate WHERE statename LIKE ?1 ORDER BY statename",
        )?;
        let pattern = format!("{}:%", state_keys::SCP_STATE);
        let rows = stmt.query_map(params![pattern], |row| {
            let key: String = row.get(0)?;
            let state: String = row.get(1)?;
            Ok((key, state))
        })?;

        let mut results = Vec::new();
        for row in rows {
            let (key, state) = row?;
            let Some(slot) = parse_slot_key(&key) else {
                continue;
            };
            results.push((slot, state));
        }
        Ok(results)
    }

    fn delete_scp_slot_states_below(&self, slot: u64) -> Result<(), DbError> {
        for (stored_slot, _) in self.load_all_scp_slot_states()? {
            if stored_slot < slot {
                delete_storestate_value(self, &scp_slot_state_key(stored_slot))?;
            }
        }
        Ok(())
    }

    fn save_tx_set_data(&self, hash: &Hash, data: &[u8]) -> Result<(), DbError> {
        let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, data);
        set_storestate_value(self, &tx_set_key(hash), &encoded)
    }

    fn load_tx_set_data(&self, hash: &Hash) -> Result<Option<Vec<u8>>, DbError> {
        load_storestate_value(self, &tx_set_key(hash))?
            .map(|encoded| decode_tx_set_data(&encoded))
            .transpose()
    }

    fn load_all_tx_set_data(&self) -> Result<Vec<(Hash, Vec<u8>)>, DbError> {
        let mut stmt =
            self.prepare("SELECT statename, state FROM storestate WHERE statename LIKE ?1")?;
        let pattern = format!("{TX_SET_KEY_PREFIX}%");
        let rows = stmt.query_map(params![pattern], |row| {
            let key: String = row.get(0)?;
            let state: String = row.get(1)?;
            Ok((key, state))
        })?;

        let mut results = Vec::new();
        for row in rows {
            let (key, encoded) = row?;
            let Some(hash_hex) = key.strip_prefix(TX_SET_KEY_PREFIX) else {
                continue;
            };
            let Ok(hash_bytes) = hex::decode(hash_hex) else {
                continue;
            };
            let Ok(hash_arr): Result<[u8; 32], _> = hash_bytes.try_into() else {
                continue;
            };
            let Ok(data) = decode_tx_set_data(&encoded) else {
                continue;
            };
            results.push((Hash(hash_arr), data));
        }
        Ok(results)
    }

    fn has_tx_set_data(&self, hash: &Hash) -> Result<bool, DbError> {
        let key = tx_set_key(hash);
        let count: i32 = self.query_row(
            "SELECT COUNT(*) FROM storestate WHERE statename = ?1",
            params![key],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    fn delete_old_tx_set_data(&self, _slot: u64) -> Result<(), DbError> {
        // In a more sophisticated implementation, we would track which slots
        // reference which tx sets. For now, this is a no-op.
        // TX sets will be cleaned up when they're no longer referenced by any
        // persisted slot state.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE storestate (statename TEXT PRIMARY KEY, state TEXT NOT NULL);
            CREATE TABLE scphistory (nodeid TEXT, ledgerseq INTEGER, envelope BLOB);
            CREATE TABLE scpquorums (qsethash TEXT PRIMARY KEY, lastledgerseq INTEGER, qset BLOB);
            "#,
        )
        .unwrap();
        conn
    }

    #[test]
    fn test_scp_slot_state_roundtrip() {
        let conn = setup_db();
        let state_json = r#"{"version":1,"envelopes":[],"quorum_sets":[]}"#;

        // Save
        conn.save_scp_slot_state(100, state_json).unwrap();

        // Load
        let loaded = conn.load_scp_slot_state(100).unwrap();
        assert_eq!(loaded, Some(state_json.to_string()));

        // Load non-existent
        let not_found = conn.load_scp_slot_state(999).unwrap();
        assert!(not_found.is_none());
    }

    #[test]
    fn test_load_all_scp_slot_states() {
        let conn = setup_db();

        conn.save_scp_slot_state(100, "state100").unwrap();
        conn.save_scp_slot_state(101, "state101").unwrap();
        conn.save_scp_slot_state(102, "state102").unwrap();

        let all = conn.load_all_scp_slot_states().unwrap();
        assert_eq!(all.len(), 3);

        // Should be ordered by slot
        assert_eq!(all[0].0, 100);
        assert_eq!(all[1].0, 101);
        assert_eq!(all[2].0, 102);
    }

    #[test]
    fn test_delete_scp_slot_states_below() {
        let conn = setup_db();

        conn.save_scp_slot_state(100, "state100").unwrap();
        conn.save_scp_slot_state(101, "state101").unwrap();
        conn.save_scp_slot_state(102, "state102").unwrap();

        // Delete slots < 102
        conn.delete_scp_slot_states_below(102).unwrap();

        let remaining = conn.load_all_scp_slot_states().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].0, 102);
    }

    #[test]
    fn test_tx_set_data_roundtrip() {
        let conn = setup_db();
        let hash = Hash([1u8; 32]);
        let data = vec![1, 2, 3, 4, 5];

        // Save
        conn.save_tx_set_data(&hash, &data).unwrap();

        // Has
        assert!(conn.has_tx_set_data(&hash).unwrap());
        assert!(!conn.has_tx_set_data(&Hash([2u8; 32])).unwrap());

        // Load
        let loaded = conn.load_tx_set_data(&hash).unwrap();
        assert_eq!(loaded, Some(data));

        // Load all
        let all = conn.load_all_tx_set_data().unwrap();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn test_copy_scp_history_to_stream_basic() {
        let conn = setup_db();

        // Store SCP history for ledger 100
        let node_id = NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32])));
        let qset_hash = Hash([0u8; 32]);
        let envelope = ScpEnvelope {
            statement: stellar_xdr::curr::ScpStatement {
                node_id: node_id.clone(),
                slot_index: 100,
                pledges: stellar_xdr::curr::ScpStatementPledges::Prepare(
                    stellar_xdr::curr::ScpStatementPrepare {
                        quorum_set_hash: qset_hash.clone(),
                        ballot: stellar_xdr::curr::ScpBallot {
                            counter: 1,
                            value: vec![].try_into().unwrap(),
                        },
                        prepared: None,
                        prepared_prime: None,
                        n_c: 0,
                        n_h: 0,
                    },
                ),
            },
            signature: stellar_xdr::curr::Signature::default(),
        };
        conn.store_scp_history(100, &[envelope]).unwrap();

        // Store the referenced quorum set
        let hash = Hash256::from([0u8; 32]);
        let qset = ScpQuorumSet {
            threshold: 1,
            validators: vec![].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        conn.store_scp_quorum_set(&hash, 100, &qset).unwrap();

        // Write to stream
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for SharedBuf {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let writer = SharedBuf(buf.clone());
        let mut stream = XdrOutputStream::from_writer(Box::new(writer));

        let written = conn
            .copy_scp_history_to_stream(100, 1, &mut stream)
            .unwrap();
        assert_eq!(written, 1);

        // Verify data was written
        let data = buf.lock().unwrap().clone();
        assert!(!data.is_empty());
    }

    #[test]
    fn test_copy_scp_history_to_stream_readback() {
        let conn = setup_db();

        // Store SCP history for ledgers 100 and 101
        for seq in 100..=101u32 {
            let node_id = NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([seq as u8; 32])));
            let qset_hash = Hash([seq as u8; 32]);
            let envelope = ScpEnvelope {
                statement: stellar_xdr::curr::ScpStatement {
                    node_id: node_id.clone(),
                    slot_index: seq as u64,
                    pledges: stellar_xdr::curr::ScpStatementPledges::Prepare(
                        stellar_xdr::curr::ScpStatementPrepare {
                            quorum_set_hash: qset_hash.clone(),
                            ballot: stellar_xdr::curr::ScpBallot {
                                counter: 1,
                                value: vec![].try_into().unwrap(),
                            },
                            prepared: None,
                            prepared_prime: None,
                            n_c: 0,
                            n_h: 0,
                        },
                    ),
                },
                signature: stellar_xdr::curr::Signature::default(),
            };
            conn.store_scp_history(seq, &[envelope]).unwrap();

            let hash = Hash256::from([seq as u8; 32]);
            let qset = ScpQuorumSet {
                threshold: 1,
                validators: vec![].try_into().unwrap(),
                inner_sets: vec![].try_into().unwrap(),
            };
            conn.store_scp_quorum_set(&hash, seq, &qset).unwrap();
        }

        // Write to stream
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        struct SharedBufR(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for SharedBufR {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let writer = SharedBufR(buf.clone());
        let mut stream = XdrOutputStream::from_writer(Box::new(writer));

        let written = conn
            .copy_scp_history_to_stream(100, 2, &mut stream)
            .unwrap();
        assert_eq!(written, 2);

        // Read back with XdrInputStream
        let data = buf.lock().unwrap().clone();
        let cursor = std::io::Cursor::new(data);
        let mut input = henyey_common::xdr_stream::XdrInputStream::from_reader(Box::new(cursor));
        let entries: Vec<ScpHistoryEntry> = input.read_all().unwrap();
        assert_eq!(entries.len(), 2);

        // Verify ledger sequences
        let ScpHistoryEntry::V0(ref v0_100) = entries[0];
        assert_eq!(v0_100.ledger_messages.ledger_seq, 100);
        let ScpHistoryEntry::V0(ref v0_101) = entries[1];
        assert_eq!(v0_101.ledger_messages.ledger_seq, 101);
    }

    #[test]
    fn test_copy_scp_history_to_stream_empty_range() {
        let conn = setup_db();

        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        struct SharedBuf2(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for SharedBuf2 {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let writer = SharedBuf2(buf.clone());
        let mut stream = XdrOutputStream::from_writer(Box::new(writer));

        let written = conn
            .copy_scp_history_to_stream(200, 10, &mut stream)
            .unwrap();
        assert_eq!(written, 0);
    }

    #[test]
    fn test_delete_old_scp_entries() {
        let conn = setup_db();

        // Store some SCP history and quorum sets
        for seq in 1..=10u32 {
            // Create a simple test envelope
            let node_id = NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([seq as u8; 32])));
            let envelope = ScpEnvelope {
                statement: stellar_xdr::curr::ScpStatement {
                    node_id: node_id.clone(),
                    slot_index: seq as u64,
                    pledges: stellar_xdr::curr::ScpStatementPledges::Prepare(
                        stellar_xdr::curr::ScpStatementPrepare {
                            quorum_set_hash: Hash([0u8; 32]),
                            ballot: stellar_xdr::curr::ScpBallot {
                                counter: 1,
                                value: vec![].try_into().unwrap(),
                            },
                            prepared: None,
                            prepared_prime: None,
                            n_c: 0,
                            n_h: 0,
                        },
                    ),
                },
                signature: stellar_xdr::curr::Signature::default(),
            };
            conn.store_scp_history(seq, &[envelope]).unwrap();

            // Also store a quorum set
            let hash = Hash256::from([seq as u8; 32]);
            let qset = ScpQuorumSet {
                threshold: 1,
                validators: vec![].try_into().unwrap(),
                inner_sets: vec![].try_into().unwrap(),
            };
            conn.store_scp_quorum_set(&hash, seq, &qset).unwrap();
        }

        // Verify we have 10 entries
        for seq in 1..=10 {
            let history = conn.load_scp_history(seq).unwrap();
            assert_eq!(history.len(), 1);
        }

        // Delete entries up to ledger 5, with count limit of 3
        let deleted = conn.delete_old_scp_entries(5, 3).unwrap();
        assert!(deleted > 0);

        // Delete remaining old entries
        let _deleted = conn.delete_old_scp_entries(5, 100).unwrap();
        // May have deleted more from both tables

        // Verify old entries are gone (1-5)
        for seq in 1..=5 {
            let history = conn.load_scp_history(seq).unwrap();
            assert!(history.is_empty(), "ledger {} should have no history", seq);
        }

        // Verify recent entries remain (6-10)
        for seq in 6..=10 {
            let history = conn.load_scp_history(seq).unwrap();
            assert_eq!(
                history.len(),
                1,
                "ledger {} should have 1 history entry",
                seq
            );
        }
    }
}
