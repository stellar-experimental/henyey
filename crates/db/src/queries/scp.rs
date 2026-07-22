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
use stellar_xdr::{
    Hash, LedgerScpMessages, Limits, NodeId, PublicKey, ReadXdr, ScpEnvelope, ScpHistoryEntry,
    ScpHistoryEntryV0, ScpQuorumSet, Uint256, WriteXdr,
};

use crate::error::DbError;
use crate::schema::state_keys;

use super::state::StateQueries;

const TX_SET_KEY_PREFIX: &str = "txset:";

/// SQL for [`ScpStatePersistenceQueries::load_all_scp_slot_states`].
///
/// Shared with its query-plan regression test
/// (`test_scpstate_prefix_query_uses_index_not_scan`) so the two can never
/// drift onto different SQL.
const LOAD_ALL_SCP_SLOT_STATES_SQL: &str =
    "SELECT statename, state FROM storestate WHERE statename LIKE ?1 ORDER BY statename";

/// SQL for [`ScpStatePersistenceQueries::load_all_tx_set_data`].
const LOAD_ALL_TX_SET_DATA_SQL: &str =
    "SELECT statename, state FROM storestate WHERE statename LIKE ?1";

/// SQL for [`ScpStatePersistenceQueries::get_all_tx_set_hashes`].
///
/// Shared with its query-plan regression test
/// (`test_txset_prefix_query_uses_index_not_scan`).
const GET_ALL_TX_SET_HASHES_SQL: &str =
    "SELECT statename FROM storestate WHERE statename LIKE ?1";

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

        let mut indices: Vec<usize> = (0..envelopes.len()).collect();
        indices.sort_by_key(|&i| node_id_hex(&envelopes[i].statement.node_id));

        for i in indices {
            let envelope = &envelopes[i];
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
        rows.map(|row| {
            let data = row?;
            Ok(ScpEnvelope::from_xdr(data.as_slice(), Limits::none())?)
        })
        .collect()
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

            // Collect referenced quorum set hashes (BTreeSet for deterministic ordering,
            // matching stellar-core's std::set<Hash> in copySCPHistoryToStream)
            let mut qset_hashes = std::collections::BTreeSet::new();
            for env in &envelopes {
                let hash = scp_envelope_quorum_set_hash(env);
                qset_hashes.insert(hash);
            }

            // Load referenced quorum sets — fail if any are missing
            let mut quorum_sets = Vec::new();
            for hash in &qset_hashes {
                match self.load_scp_quorum_set(hash)? {
                    Some(qset) => quorum_sets.push(qset),
                    None => {
                        return Err(DbError::Integrity(format!(
                            "Missing quorum set {} referenced by SCP history at ledger {}",
                            hash, ledger_seq
                        )));
                    }
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
        // Delete old SCP history entries by complete ledger boundary.
        // scphistory may have multiple rows per ledger (one per validator),
        // so row-limit deletion can split a ledger's data. Instead, select
        // distinct ledger sequences within the count budget, then delete ALL
        // rows for those complete ledgers.
        let history_deleted = self.execute(
            r#"
            DELETE FROM scphistory
            WHERE ledgerseq IN (
                SELECT DISTINCT ledgerseq FROM scphistory
                WHERE ledgerseq <= ?1
                ORDER BY ledgerseq ASC
                LIMIT ?2
            )
            "#,
            params![max_ledger, count],
        )?;

        // Delete old quorum sets by ledger boundary.
        // Same approach: select distinct lastledgerseq values within budget,
        // then delete all quorum sets for those complete ledger boundaries.
        let quorums_deleted = self.execute(
            r#"
            DELETE FROM scpquorums
            WHERE lastledgerseq IN (
                SELECT DISTINCT lastledgerseq FROM scpquorums
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
        stellar_xdr::ScpStatementPledges::Nominate(nom) => &nom.quorum_set_hash,
        stellar_xdr::ScpStatementPledges::Prepare(prep) => &prep.quorum_set_hash,
        stellar_xdr::ScpStatementPledges::Confirm(conf) => &conf.quorum_set_hash,
        stellar_xdr::ScpStatementPledges::Externalize(ext) => &ext.commit_quorum_set_hash,
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
/// They delegate to [`StateQueries`] methods to avoid duplicating storestate SQL.
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

    /// Return the hashes of all persisted transaction sets.
    fn get_all_tx_set_hashes(&self) -> Result<Vec<Hash>, DbError>;

    /// Delete persisted transaction sets by their hashes.
    fn delete_tx_sets_by_hashes(&self, hashes: &[Hash]) -> Result<(), DbError>;

    /// Atomic purge of unreferenced persisted transaction sets.
    ///
    /// Reads all persisted tx-set hashes, all persisted SCP slot states,
    /// computes the set of orphan hashes (those not referenced by any
    /// stored envelope), and deletes the orphans — all on the same
    /// connection. The caller is expected to wrap the call in a transaction
    /// (e.g. via [`Database::transaction`]) so the read-then-delete sequence
    /// is observed atomically with respect to concurrent writers.
    ///
    /// SCP envelopes are stored as JSON-encoded `PersistedSlotState`. The
    /// JSON shape (`{"envelopes": [[byte, byte, ...], ...], ...}`) is
    /// defined by the herder's `PersistedSlotState` Serde representation;
    /// this method parses it without depending on the herder crate.
    ///
    /// Per-state error handling: if a stored SCP state fails to parse, the
    /// state is logged and skipped (its tx-set references can't be proven,
    /// so its tx-sets may be deleted as orphans). Matches stellar-core
    /// `HerderImpl::purgeOldPersistedTxSets()` (HerderImpl.cpp:2456-2478).
    fn purge_unreferenced_tx_sets_atomic(&self) -> Result<(), DbError>;
}

impl ScpStatePersistenceQueries for Connection {
    fn save_scp_slot_state(&self, slot: u64, state_json: &str) -> Result<(), DbError> {
        self.set_state(&scp_slot_state_key(slot), state_json)
    }

    fn load_scp_slot_state(&self, slot: u64) -> Result<Option<String>, DbError> {
        self.get_state(&scp_slot_state_key(slot))
    }

    fn load_all_scp_slot_states(&self) -> Result<Vec<(u64, String)>, DbError> {
        let mut stmt = self.prepare(LOAD_ALL_SCP_SLOT_STATES_SQL)?;
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
        let keys_to_delete: Vec<String> = self
            .load_all_scp_slot_states()?
            .into_iter()
            .filter(|(stored_slot, _)| *stored_slot < slot)
            .map(|(s, _)| scp_slot_state_key(s))
            .collect();

        if keys_to_delete.is_empty() {
            return Ok(());
        }

        let placeholders = super::sql_placeholder_list(keys_to_delete.len());
        let sql = format!("DELETE FROM storestate WHERE statename IN ({placeholders})");
        let params: Vec<&dyn rusqlite::types::ToSql> = keys_to_delete
            .iter()
            .map(|k| k as &dyn rusqlite::types::ToSql)
            .collect();
        self.execute(&sql, params.as_slice())?;
        Ok(())
    }

    fn save_tx_set_data(&self, hash: &Hash, data: &[u8]) -> Result<(), DbError> {
        let encoded = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, data);
        self.set_state(&tx_set_key(hash), &encoded)
    }

    fn load_tx_set_data(&self, hash: &Hash) -> Result<Option<Vec<u8>>, DbError> {
        self.get_state(&tx_set_key(hash))?
            .map(|encoded| decode_tx_set_data(&encoded))
            .transpose()
    }

    fn load_all_tx_set_data(&self) -> Result<Vec<(Hash, Vec<u8>)>, DbError> {
        let mut stmt = self.prepare(LOAD_ALL_TX_SET_DATA_SQL)?;
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

    fn get_all_tx_set_hashes(&self) -> Result<Vec<Hash>, DbError> {
        let mut stmt = self.prepare(GET_ALL_TX_SET_HASHES_SQL)?;
        let pattern = format!("{TX_SET_KEY_PREFIX}%");
        let rows = stmt.query_map(params![pattern], |row| {
            let key: String = row.get(0)?;
            Ok(key)
        })?;

        let mut hashes = Vec::new();
        for row in rows {
            let key = row?;
            let Some(hash_hex) = key.strip_prefix(TX_SET_KEY_PREFIX) else {
                tracing::warn!(key, "Skipping malformed tx set key");
                continue;
            };
            let Ok(hash_bytes) = hex::decode(hash_hex) else {
                tracing::warn!(key, "Skipping tx set key with invalid hex");
                continue;
            };
            let Ok(hash_arr): Result<[u8; 32], _> = hash_bytes.try_into() else {
                tracing::warn!(key, "Skipping tx set key with wrong hash length");
                continue;
            };
            hashes.push(Hash(hash_arr));
        }
        Ok(hashes)
    }

    fn delete_tx_sets_by_hashes(&self, hashes: &[Hash]) -> Result<(), DbError> {
        if hashes.is_empty() {
            return Ok(());
        }

        let keys: Vec<String> = hashes.iter().map(|h| tx_set_key(h)).collect();
        let placeholders = super::sql_placeholder_list(keys.len());
        let sql = format!("DELETE FROM storestate WHERE statename IN ({placeholders})");
        let params: Vec<&dyn rusqlite::types::ToSql> = keys
            .iter()
            .map(|k| k as &dyn rusqlite::types::ToSql)
            .collect();
        self.execute(&sql, params.as_slice())?;
        Ok(())
    }

    fn purge_unreferenced_tx_sets_atomic(&self) -> Result<(), DbError> {
        // Step 1: read all stored tx-set hashes.
        let all_hashes_vec = self.get_all_tx_set_hashes()?;
        if all_hashes_vec.is_empty() {
            // Nothing to do; avoid a needless write lock when caller wraps
            // this in a transaction.
            return Ok(());
        }
        let all_hashes: std::collections::HashSet<Hash> = all_hashes_vec.into_iter().collect();

        // Step 2: read all stored SCP slot states and compute the set of
        // referenced tx-set hashes. Per-state error handling matches
        // stellar-core (HerderImpl.cpp:2456-2478) and the in-memory path
        // (`ScpPersistenceManager::purge_unreferenced_tx_sets`).
        let mut referenced: std::collections::HashSet<Hash> = std::collections::HashSet::new();
        for (slot, state_json) in self.load_all_scp_slot_states()? {
            match referenced_tx_set_hashes_from_state_json(&state_json) {
                Ok(hashes) => referenced.extend(hashes),
                Err(e) => tracing::warn!(
                    slot,
                    error = %e,
                    "Error extracting tx set hashes from persisted state, skipping"
                ),
            }
        }

        // Step 3: delete orphans (stored but not referenced).
        let unreferenced: Vec<Hash> = all_hashes.difference(&referenced).cloned().collect();
        if !unreferenced.is_empty() {
            tracing::debug!(
                count = unreferenced.len(),
                "Purging unreferenced persisted tx sets (atomic)"
            );
            self.delete_tx_sets_by_hashes(&unreferenced)?;
        }
        Ok(())
    }
}

/// Parse a JSON-encoded `PersistedSlotState` and return all tx-set hashes
/// referenced by its envelopes.
///
/// Mirrors the herder's `PersistedSlotState` Serde shape:
/// `{ "version": u32, "envelopes": [[u8, ...], ...], "quorum_sets": [...] }`.
/// Each envelope is the raw XDR bytes of an `ScpEnvelope`. We decode the
/// XDR and extract the tx-set hash from the embedded `StellarValue` in
/// the pledges. This duplicates `henyey_herder::persistence::get_tx_set_hashes`
/// — kept in sync by the regression tests in this module.
fn referenced_tx_set_hashes_from_state_json(state_json: &str) -> Result<Vec<Hash>, DbError> {
    let value: serde_json::Value = serde_json::from_str(state_json)
        .map_err(|e| DbError::Integrity(format!("invalid persisted state JSON: {}", e)))?;
    let envelopes_json = value
        .get("envelopes")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            DbError::Integrity("persisted state JSON missing 'envelopes' array".to_string())
        })?;

    let mut hashes = Vec::new();
    for env_json in envelopes_json {
        let bytes: Vec<u8> = env_json
            .as_array()
            .ok_or_else(|| {
                DbError::Integrity("persisted envelope JSON is not a byte array".to_string())
            })?
            .iter()
            .map(|n| {
                n.as_u64()
                    .and_then(|x| u8::try_from(x).ok())
                    .ok_or_else(|| {
                        DbError::Integrity("invalid byte in persisted envelope".to_string())
                    })
            })
            .collect::<Result<Vec<u8>, _>>()?;
        let envelope = ScpEnvelope::from_xdr(bytes.as_slice(), Limits::none())?;
        for hash in extract_tx_set_hashes_from_envelope(&envelope) {
            hashes.push(hash);
        }
    }
    Ok(hashes)
}

/// Extract the tx-set hashes referenced by an SCP envelope.
///
/// Mirrors `henyey_herder::persistence::get_tx_set_hashes`. The envelope's
/// pledges carry an `ScpBallot` (in Prepare/Confirm/Externalize) or
/// `Vec<Value>` (in Nominate); each `Value` decodes into a `StellarValue`
/// whose `tx_set_hash` is the referenced hash.
fn extract_tx_set_hashes_from_envelope(envelope: &ScpEnvelope) -> Vec<Hash> {
    use stellar_xdr::{ScpStatementPledges, StellarValue, Value as XdrValue};

    fn from_value(value: &XdrValue) -> Option<Hash> {
        let sv = StellarValue::from_xdr(value.as_slice(), Limits::none()).ok()?;
        Some(sv.tx_set_hash)
    }

    let mut hashes = Vec::new();
    match &envelope.statement.pledges {
        ScpStatementPledges::Nominate(nom) => {
            for value in nom.votes.iter().chain(nom.accepted.iter()) {
                if let Some(h) = from_value(value) {
                    hashes.push(h);
                }
            }
        }
        ScpStatementPledges::Prepare(prep) => {
            if let Some(h) = from_value(&prep.ballot.value) {
                hashes.push(h);
            }
            if let Some(p) = &prep.prepared {
                if let Some(h) = from_value(&p.value) {
                    hashes.push(h);
                }
            }
            if let Some(pp) = &prep.prepared_prime {
                if let Some(h) = from_value(&pp.value) {
                    hashes.push(h);
                }
            }
        }
        ScpStatementPledges::Confirm(conf) => {
            if let Some(h) = from_value(&conf.ballot.value) {
                hashes.push(h);
            }
        }
        ScpStatementPledges::Externalize(ext) => {
            if let Some(h) = from_value(&ext.commit.value) {
                hashes.push(h);
            }
        }
    }
    hashes
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

    /// Return the `detail` column of every `EXPLAIN QUERY PLAN` row for `sql`,
    /// joined by newlines. Binds dummy TEXT values for any `?N` placeholders so
    /// the statement prepares regardless of how many parameters it declares
    /// (LIKE = 1, range = 2).
    fn query_plan(conn: &Connection, sql: &str) -> String {
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let n = stmt.parameter_count();
        let dummies: Vec<String> = (0..n).map(|_| String::from("txset:")).collect();
        let params: Vec<&dyn rusqlite::types::ToSql> = dummies
            .iter()
            .map(|s| s as &dyn rusqlite::types::ToSql)
            .collect();
        let rows = stmt
            .query_map(params.as_slice(), |row| row.get::<_, String>(3))
            .unwrap();
        rows.map(|r| r.unwrap()).collect::<Vec<_>>().join("\n")
    }

    /// The `txset:` prefix scan on `storestate` (run inside the
    /// `BEGIN IMMEDIATE` write lock during near-tip back-fill) must resolve to
    /// an index SEARCH, not a full-table SCAN. Regression for #3702: the
    /// `LIKE 'txset:%'` form scanned the whole table under the WAL write lock,
    /// starving SCP intake on the post-restart tracking path.
    #[test]
    fn test_txset_prefix_query_uses_index_not_scan() {
        let conn = setup_db();
        let plan = query_plan(&conn, GET_ALL_TX_SET_HASHES_SQL);
        assert!(
            plan.contains("SEARCH") && !plan.contains("SCAN storestate"),
            "txset prefix query must use an index SEARCH, not a full-table SCAN; plan was:\n{plan}"
        );
    }

    /// The `scpstate:` prefix scan on `storestate` (also run inside the
    /// `BEGIN IMMEDIATE` write lock) must resolve to an index SEARCH, not a
    /// full-table SCAN. Regression for #3702.
    #[test]
    fn test_scpstate_prefix_query_uses_index_not_scan() {
        let conn = setup_db();
        let plan = query_plan(&conn, LOAD_ALL_SCP_SLOT_STATES_SQL);
        assert!(
            plan.contains("SEARCH") && !plan.contains("SCAN storestate"),
            "scpstate prefix query must use an index SEARCH, not a full-table SCAN; plan was:\n{plan}"
        );
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
    fn test_get_all_tx_set_hashes() {
        let conn = setup_db();

        // Empty DB
        let hashes = conn.get_all_tx_set_hashes().unwrap();
        assert!(hashes.is_empty());

        // Save some tx sets
        let hash1 = Hash([1u8; 32]);
        let hash2 = Hash([2u8; 32]);
        let hash3 = Hash([3u8; 32]);
        conn.save_tx_set_data(&hash1, &[10]).unwrap();
        conn.save_tx_set_data(&hash2, &[20]).unwrap();
        conn.save_tx_set_data(&hash3, &[30]).unwrap();

        let mut hashes = conn.get_all_tx_set_hashes().unwrap();
        hashes.sort_by_key(|h| h.0);
        assert_eq!(hashes.len(), 3);
        assert_eq!(hashes[0], hash1);
        assert_eq!(hashes[1], hash2);
        assert_eq!(hashes[2], hash3);
    }

    #[test]
    fn test_delete_tx_sets_by_hashes() {
        let conn = setup_db();

        let hash1 = Hash([1u8; 32]);
        let hash2 = Hash([2u8; 32]);
        let hash3 = Hash([3u8; 32]);
        conn.save_tx_set_data(&hash1, &[10]).unwrap();
        conn.save_tx_set_data(&hash2, &[20]).unwrap();
        conn.save_tx_set_data(&hash3, &[30]).unwrap();

        // Delete only hash1 and hash3
        conn.delete_tx_sets_by_hashes(&[hash1.clone(), hash3.clone()])
            .unwrap();

        // hash2 should remain
        let remaining = conn.get_all_tx_set_hashes().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0], hash2);
        assert!(conn.has_tx_set_data(&hash2).unwrap());
        assert!(!conn.has_tx_set_data(&hash1).unwrap());
        assert!(!conn.has_tx_set_data(&hash3).unwrap());
    }

    #[test]
    fn test_delete_tx_sets_by_hashes_empty() {
        let conn = setup_db();

        let hash1 = Hash([1u8; 32]);
        conn.save_tx_set_data(&hash1, &[10]).unwrap();

        // Delete with empty list should be a no-op
        conn.delete_tx_sets_by_hashes(&[]).unwrap();
        assert!(conn.has_tx_set_data(&hash1).unwrap());
    }

    /// Builds a JSON-encoded `PersistedSlotState` whose single envelope
    /// references `tx_set_hash`. Mirrors the herder-side
    /// `PersistedSlotState::to_json` output, so the on-disk encoding here
    /// matches what `ScpPersistenceManager` would write at runtime.
    fn persisted_state_json_referencing(tx_set_hash: &Hash) -> String {
        // Construct a NOMINATE envelope referencing `tx_set_hash` via a
        // StellarValue.
        let stellar_value = stellar_xdr::StellarValue {
            tx_set_hash: tx_set_hash.clone(),
            close_time: stellar_xdr::TimePoint(0),
            upgrades: vec![].try_into().unwrap(),
            ext: stellar_xdr::StellarValueExt::Basic,
        };
        let value_xdr = stellar_value.to_xdr(Limits::none()).unwrap();
        let envelope = ScpEnvelope {
            statement: stellar_xdr::ScpStatement {
                node_id: NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32]))),
                slot_index: 100,
                pledges: stellar_xdr::ScpStatementPledges::Nominate(stellar_xdr::ScpNomination {
                    quorum_set_hash: Hash([0u8; 32]),
                    votes: vec![stellar_xdr::Value(value_xdr.try_into().unwrap())]
                        .try_into()
                        .unwrap(),
                    accepted: vec![].try_into().unwrap(),
                }),
            },
            signature: stellar_xdr::Signature::default(),
        };
        let env_xdr: Vec<u8> = envelope.to_xdr(Limits::none()).unwrap();
        // Match the herder's PersistedSlotState JSON shape: serde_json renders
        // `Vec<u8>` as a JSON array of numbers. Build that shape manually so
        // we don't need a serde_json dev-dep on the db crate.
        let env_array = env_xdr
            .iter()
            .map(|b| b.to_string())
            .collect::<Vec<_>>()
            .join(",");
        format!(
            r#"{{"version":1,"envelopes":[[{}]],"quorum_sets":[]}}"#,
            env_array
        )
    }

    #[test]
    fn test_purge_unreferenced_tx_sets_atomic_basic() {
        let conn = setup_db();

        let referenced = Hash([0xAA; 32]);
        let orphan = Hash([0xBB; 32]);

        // Two tx sets persisted: one referenced, one orphan.
        conn.save_tx_set_data(&referenced, &[1, 2, 3]).unwrap();
        conn.save_tx_set_data(&orphan, &[4, 5, 6]).unwrap();

        // One SCP state references `referenced`.
        let state_json = persisted_state_json_referencing(&referenced);
        conn.save_scp_slot_state(100, &state_json).unwrap();

        // Atomic purge.
        conn.purge_unreferenced_tx_sets_atomic().unwrap();

        assert!(conn.has_tx_set_data(&referenced).unwrap());
        assert!(!conn.has_tx_set_data(&orphan).unwrap());
    }

    #[test]
    fn test_purge_unreferenced_tx_sets_atomic_empty_is_noop() {
        let conn = setup_db();
        // Empty DB — should be a no-op (and not error).
        conn.purge_unreferenced_tx_sets_atomic().unwrap();
    }

    #[test]
    fn test_purge_unreferenced_tx_sets_atomic_all_referenced() {
        let conn = setup_db();

        let hash = Hash([0xCC; 32]);
        conn.save_tx_set_data(&hash, &[1]).unwrap();
        let state_json = persisted_state_json_referencing(&hash);
        conn.save_scp_slot_state(100, &state_json).unwrap();

        conn.purge_unreferenced_tx_sets_atomic().unwrap();
        assert!(conn.has_tx_set_data(&hash).unwrap());
    }

    #[test]
    fn test_purge_unreferenced_tx_sets_atomic_skips_corrupt_state() {
        let conn = setup_db();

        let valid_hash = Hash([0xAA; 32]);
        let orphan_from_corrupt = Hash([0xBB; 32]);
        conn.save_tx_set_data(&valid_hash, &[1]).unwrap();
        conn.save_tx_set_data(&orphan_from_corrupt, &[2]).unwrap();

        // Valid state references valid_hash.
        let valid_state = persisted_state_json_referencing(&valid_hash);
        conn.save_scp_slot_state(100, &valid_state).unwrap();

        // Corrupt JSON for slot 101 — invalid bytes that won't deserialize.
        conn.save_scp_slot_state(101, "{not valid json").unwrap();

        // Should NOT panic; corrupt state is logged + skipped. valid_hash
        // survives; orphan_from_corrupt is deleted (can't prove it's
        // referenced).
        conn.purge_unreferenced_tx_sets_atomic().unwrap();
        assert!(conn.has_tx_set_data(&valid_hash).unwrap());
        assert!(!conn.has_tx_set_data(&orphan_from_corrupt).unwrap());
    }

    #[test]
    fn test_copy_scp_history_to_stream_basic() {
        let conn = setup_db();

        // Store SCP history for ledger 100
        let node_id = NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32])));
        let qset_hash = Hash([0u8; 32]);
        let envelope = ScpEnvelope {
            statement: stellar_xdr::ScpStatement {
                node_id: node_id.clone(),
                slot_index: 100,
                pledges: stellar_xdr::ScpStatementPledges::Prepare(
                    stellar_xdr::ScpStatementPrepare {
                        quorum_set_hash: qset_hash.clone(),
                        ballot: stellar_xdr::ScpBallot {
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
            signature: stellar_xdr::Signature::default(),
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
                statement: stellar_xdr::ScpStatement {
                    node_id: node_id.clone(),
                    slot_index: seq as u64,
                    pledges: stellar_xdr::ScpStatementPledges::Prepare(
                        stellar_xdr::ScpStatementPrepare {
                            quorum_set_hash: qset_hash.clone(),
                            ballot: stellar_xdr::ScpBallot {
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
                signature: stellar_xdr::Signature::default(),
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
    fn test_delete_old_scp_entries_preserves_ledger_boundary() {
        let conn = setup_db();

        // Store 3 ledgers with 3 envelopes each (simulating 3 validators)
        for seq in 1..=3u32 {
            let mut envelopes = Vec::new();
            for validator in 0..3u8 {
                let node_id = NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([validator; 32])));
                envelopes.push(ScpEnvelope {
                    statement: stellar_xdr::ScpStatement {
                        node_id,
                        slot_index: seq as u64,
                        pledges: stellar_xdr::ScpStatementPledges::Prepare(
                            stellar_xdr::ScpStatementPrepare {
                                quorum_set_hash: Hash([0u8; 32]),
                                ballot: stellar_xdr::ScpBallot {
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
                    signature: stellar_xdr::Signature::default(),
                });
            }
            conn.store_scp_history(seq, &envelopes).unwrap();
        }

        // 3 ledgers × 3 validators = 9 rows total.
        // Delete with count=1 (1 distinct ledger). Should delete ALL 3 rows
        // for ledger 1, not just 1 row.
        let deleted = conn.delete_old_scp_entries(3, 1).unwrap();
        assert_eq!(deleted, 3, "should delete all 3 rows for ledger 1");

        // Ledger 1 should be completely gone
        assert!(conn.load_scp_history(1).unwrap().is_empty());
        // Ledgers 2 and 3 should still have all 3 envelopes
        assert_eq!(conn.load_scp_history(2).unwrap().len(), 3);
        assert_eq!(conn.load_scp_history(3).unwrap().len(), 3);
    }

    #[test]
    fn test_delete_old_scp_entries() {
        let conn = setup_db();

        // Store some SCP history and quorum sets
        for seq in 1..=10u32 {
            // Create a simple test envelope
            let node_id = NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([seq as u8; 32])));
            let envelope = ScpEnvelope {
                statement: stellar_xdr::ScpStatement {
                    node_id: node_id.clone(),
                    slot_index: seq as u64,
                    pledges: stellar_xdr::ScpStatementPledges::Prepare(
                        stellar_xdr::ScpStatementPrepare {
                            quorum_set_hash: Hash([0u8; 32]),
                            ballot: stellar_xdr::ScpBallot {
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
                signature: stellar_xdr::Signature::default(),
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
