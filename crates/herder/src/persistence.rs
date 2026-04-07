//! SCP state persistence for crash recovery.
//!
//! This module implements SCP state persistence to enable recovery after a crash
//! or restart. The persisted state includes:
//!
//! - SCP envelopes for recent slots
//! - Transaction sets referenced by those envelopes
//! - Quorum sets referenced by those envelopes
//!
//! # Persistence Model
//!
//! State is persisted to SQLite database with the following schema:
//!
//! - `scpstate`: Stores encoded SCP state per slot
//! - `txsethistory`: Stores transaction sets by hash
//! - `quorumsets`: Stores quorum sets by hash (in scpstate structure)
//!
//! # Recovery Flow
//!
//! On startup, `restore_scp_state()` is called to:
//!
//! 1. Load persisted transaction sets and add them to the pending cache
//! 2. Load persisted quorum sets and add them to the pending cache
//! 3. Load persisted SCP envelopes and restore SCP state via `set_state_from_envelope()`
//! 4. Rebuild quorum tracker state
//!
//! # Persistence Timing
//!
//! `persist_scp_state()` is called after each envelope emission to ensure the
//! latest SCP state is durable. This enables recovery to the last externalized slot.

use serde::{Deserialize, Serialize};
use stellar_xdr::curr::{
    Hash, Limits, ReadXdr, ScpEnvelope, ScpQuorumSet, ScpStatementPledges, StellarValue, Value,
    WriteXdr,
};
use tracing::{debug, info, warn};

use crate::{HerderError, Result};

// Re-export Database for users who want to construct SqliteScpPersistence
pub use henyey_db::Database;

/// Persisted SCP state for a single slot.
///
/// This structure captures all the data needed to restore SCP state for a slot:
/// - The SCP envelopes that were emitted
/// - The quorum sets referenced by those envelopes
///
/// Note: Named `PersistedSlotState` to avoid conflict with the XDR `PersistedSlotState` type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSlotState {
    /// Version of the persisted state format.
    pub version: u32,
    /// SCP envelopes for this slot.
    pub envelopes: Vec<Vec<u8>>,
    /// Quorum sets referenced by the envelopes.
    pub quorum_sets: Vec<Vec<u8>>,
}

impl PersistedSlotState {
    /// Current version of the persisted state format.
    pub(crate) const CURRENT_VERSION: u32 = 1;

    /// Create a new empty persisted state.
    pub fn new() -> Self {
        Self {
            version: Self::CURRENT_VERSION,
            envelopes: Vec::new(),
            quorum_sets: Vec::new(),
        }
    }

    /// Add an envelope to the persisted state.
    pub fn add_envelope(&mut self, envelope: &ScpEnvelope) -> Result<()> {
        let bytes = envelope.to_xdr(Limits::none()).map_err(|e| {
            HerderError::InvalidEnvelope(format!("failed to serialize envelope: {}", e))
        })?;
        self.envelopes.push(bytes);
        Ok(())
    }

    /// Add a quorum set to the persisted state.
    pub fn add_quorum_set(&mut self, quorum_set: &ScpQuorumSet) -> Result<()> {
        let bytes = quorum_set.to_xdr(Limits::none()).map_err(|e| {
            HerderError::InvalidEnvelope(format!("failed to serialize quorum set: {}", e))
        })?;
        self.quorum_sets.push(bytes);
        Ok(())
    }

    /// Get the envelopes from the persisted state.
    pub(crate) fn get_envelopes(&self) -> Vec<Result<ScpEnvelope>> {
        self.envelopes
            .iter()
            .map(|bytes| {
                ScpEnvelope::from_xdr(bytes.as_slice(), Limits::none()).map_err(|e| {
                    HerderError::InvalidEnvelope(format!("failed to deserialize envelope: {}", e))
                })
            })
            .collect()
    }

    /// Get the quorum sets from the persisted state.
    pub(crate) fn get_quorum_sets(&self) -> Vec<Result<ScpQuorumSet>> {
        self.quorum_sets
            .iter()
            .map(|bytes| {
                ScpQuorumSet::from_xdr(bytes.as_slice(), Limits::none()).map_err(|e| {
                    HerderError::InvalidEnvelope(format!("failed to deserialize quorum set: {}", e))
                })
            })
            .collect()
    }

    /// Encode to JSON for storage.
    pub(crate) fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).map_err(|e| {
            HerderError::InvalidEnvelope(format!("failed to encode persisted state: {}", e))
        })
    }

    /// Decode from JSON.
    pub(crate) fn from_json(json: &str) -> Result<Self> {
        serde_json::from_str(json).map_err(|e| {
            HerderError::InvalidEnvelope(format!("failed to decode persisted state: {}", e))
        })
    }

    /// Encode to base64 for storage.
    pub fn to_base64(&self) -> Result<String> {
        let json = self.to_json()?;
        Ok(base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            json.as_bytes(),
        ))
    }

    /// Decode from base64.
    pub fn from_base64(encoded: &str) -> Result<Self> {
        let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded)
            .map_err(|e| HerderError::InvalidEnvelope(format!("failed to decode base64: {}", e)))?;
        let json = String::from_utf8(bytes).map_err(|e| {
            HerderError::InvalidEnvelope(format!("invalid UTF-8 in persisted state: {}", e))
        })?;
        Self::from_json(&json)
    }
}

impl Default for PersistedSlotState {
    fn default() -> Self {
        Self::new()
    }
}

/// Trait for SCP state persistence storage.
///
/// Implement this trait to provide a storage backend for SCP state persistence.
/// The default implementation uses SQLite via stellar-core-db.
pub trait ScpStatePersistence: Send + Sync {
    /// Save SCP state for a slot.
    fn save_scp_state(&self, slot: u64, state: &PersistedSlotState) -> Result<()>;

    /// Load SCP state for a slot.
    fn load_scp_state(&self, slot: u64) -> Result<Option<PersistedSlotState>>;

    /// Load SCP state for all slots.
    fn load_all_scp_states(&self) -> Result<Vec<(u64, PersistedSlotState)>>;

    /// Delete SCP state for slots below the given threshold.
    fn delete_scp_state_below(&self, slot: u64) -> Result<()>;

    /// Save a transaction set.
    fn save_tx_set(&self, hash: &Hash, tx_set: &[u8]) -> Result<()>;

    /// Load a transaction set.
    fn load_tx_set(&self, hash: &Hash) -> Result<Option<Vec<u8>>>;

    /// Load all transaction sets.
    fn load_all_tx_sets(&self) -> Result<Vec<(Hash, Vec<u8>)>>;

    /// Check if a transaction set exists.
    fn has_tx_set(&self, hash: &Hash) -> Result<bool>;

    /// Delete transaction sets for slots below the given threshold.
    fn delete_tx_sets_below(&self, slot: u64) -> Result<()>;
}

/// In-memory implementation of SCP state persistence for testing.
pub struct InMemoryScpPersistence {
    /// SCP states by slot.
    states: parking_lot::RwLock<std::collections::HashMap<u64, PersistedSlotState>>,
    /// Transaction sets by hash.
    tx_sets: parking_lot::RwLock<std::collections::HashMap<Hash, Vec<u8>>>,
}

impl InMemoryScpPersistence {
    /// Create a new in-memory persistence instance.
    pub fn new() -> Self {
        Self {
            states: parking_lot::RwLock::new(std::collections::HashMap::new()),
            tx_sets: parking_lot::RwLock::new(std::collections::HashMap::new()),
        }
    }
}

impl Default for InMemoryScpPersistence {
    fn default() -> Self {
        Self::new()
    }
}

impl ScpStatePersistence for InMemoryScpPersistence {
    fn save_scp_state(&self, slot: u64, state: &PersistedSlotState) -> Result<()> {
        self.states.write().insert(slot, state.clone());
        Ok(())
    }

    fn load_scp_state(&self, slot: u64) -> Result<Option<PersistedSlotState>> {
        Ok(self.states.read().get(&slot).cloned())
    }

    fn load_all_scp_states(&self) -> Result<Vec<(u64, PersistedSlotState)>> {
        Ok(self
            .states
            .read()
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect())
    }

    fn delete_scp_state_below(&self, slot: u64) -> Result<()> {
        self.states.write().retain(|k, _| *k >= slot);
        Ok(())
    }

    fn save_tx_set(&self, hash: &Hash, tx_set: &[u8]) -> Result<()> {
        self.tx_sets.write().insert(hash.clone(), tx_set.to_vec());
        Ok(())
    }

    fn load_tx_set(&self, hash: &Hash) -> Result<Option<Vec<u8>>> {
        Ok(self.tx_sets.read().get(hash).cloned())
    }

    fn load_all_tx_sets(&self) -> Result<Vec<(Hash, Vec<u8>)>> {
        Ok(self
            .tx_sets
            .read()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect())
    }

    fn has_tx_set(&self, hash: &Hash) -> Result<bool> {
        Ok(self.tx_sets.read().contains_key(hash))
    }

    fn delete_tx_sets_below(&self, _slot: u64) -> Result<()> {
        // In-memory persistence doesn't track which tx sets belong to which slots
        // This is a limitation of the in-memory implementation
        Ok(())
    }
}

/// Extract transaction set hashes from an SCP envelope.
///
/// Returns the hashes of transaction sets referenced by the envelope's statement.
pub fn get_tx_set_hashes(envelope: &ScpEnvelope) -> Vec<Hash> {
    let mut hashes = Vec::new();

    match &envelope.statement.pledges {
        ScpStatementPledges::Nominate(nom) => {
            // Extract from votes and accepted
            for value in nom.votes.iter().chain(nom.accepted.iter()) {
                if let Some(hash) = extract_tx_set_hash_from_value(value) {
                    hashes.push(hash);
                }
            }
        }
        ScpStatementPledges::Prepare(prep) => {
            if let Some(hash) = extract_tx_set_hash_from_value(&prep.ballot.value) {
                hashes.push(hash);
            }
            if let Some(prepared) = &prep.prepared {
                if let Some(hash) = extract_tx_set_hash_from_value(&prepared.value) {
                    hashes.push(hash);
                }
            }
            if let Some(prepared_prime) = &prep.prepared_prime {
                if let Some(hash) = extract_tx_set_hash_from_value(&prepared_prime.value) {
                    hashes.push(hash);
                }
            }
        }
        ScpStatementPledges::Confirm(conf) => {
            if let Some(hash) = extract_tx_set_hash_from_value(&conf.ballot.value) {
                hashes.push(hash);
            }
        }
        ScpStatementPledges::Externalize(ext) => {
            if let Some(hash) = extract_tx_set_hash_from_value(&ext.commit.value) {
                hashes.push(hash);
            }
        }
    }

    // Deduplicate
    hashes.sort();
    hashes.dedup();
    hashes
}

/// Extract the transaction set hash from a StellarValue.
fn extract_tx_set_hash_from_value(value: &Value) -> Option<Hash> {
    // Value contains a StellarValue which has txSetHash
    let stellar_value = StellarValue::from_xdr(value.as_slice(), Limits::none()).ok()?;
    Some(stellar_value.tx_set_hash.clone())
}

/// Get the quorum set hash from an SCP statement.
///
/// This extracts the quorum set hash from the statement pledges.
pub fn get_quorum_set_hash(envelope: &ScpEnvelope) -> Option<Hash> {
    match &envelope.statement.pledges {
        ScpStatementPledges::Nominate(nom) => Some(nom.quorum_set_hash.clone()),
        ScpStatementPledges::Prepare(prep) => Some(prep.quorum_set_hash.clone()),
        ScpStatementPledges::Confirm(conf) => Some(conf.quorum_set_hash.clone()),
        ScpStatementPledges::Externalize(ext) => Some(ext.commit_quorum_set_hash.clone()),
    }
}

/// Manager for SCP state persistence.
///
/// This struct coordinates persistence of SCP state, providing methods to
/// save and restore SCP state for crash recovery.
pub struct ScpPersistenceManager {
    /// The persistence backend.
    storage: Box<dyn ScpStatePersistence>,
    /// Last slot that was persisted.
    last_slot_saved: parking_lot::RwLock<u64>,
}

impl ScpPersistenceManager {
    /// Create a new persistence manager with the given storage backend.
    pub fn new(storage: Box<dyn ScpStatePersistence>) -> Self {
        Self {
            storage,
            last_slot_saved: parking_lot::RwLock::new(0),
        }
    }

    /// Create a new persistence manager with in-memory storage (for testing).
    pub fn in_memory() -> Self {
        Self::new(Box::new(InMemoryScpPersistence::new()))
    }

    /// Get the last slot that was persisted.
    pub fn last_slot_saved(&self) -> u64 {
        *self.last_slot_saved.read()
    }

    /// Persist SCP state for a slot.
    ///
    /// This saves the given envelopes, their referenced transaction sets, and
    /// quorum sets to the persistence backend.
    pub fn persist_scp_state(
        &self,
        slot: u64,
        envelopes: &[ScpEnvelope],
        tx_sets: &[(Hash, Vec<u8>)],
        quorum_sets: &[(Hash, ScpQuorumSet)],
    ) -> Result<()> {
        let mut last_saved = self.last_slot_saved.write();

        if slot < *last_saved {
            return Ok(());
        }

        *last_saved = slot;

        // Build persisted state
        let mut state = PersistedSlotState::new();

        for envelope in envelopes {
            state.add_envelope(envelope)?;
        }

        for (_, quorum_set) in quorum_sets {
            state.add_quorum_set(quorum_set)?;
        }

        // Save transaction sets
        for (hash, tx_set) in tx_sets {
            if !self.storage.has_tx_set(hash)? {
                self.storage.save_tx_set(hash, tx_set)?;
            }
        }

        // Save SCP state
        self.storage.save_scp_state(slot, &state)?;

        debug!("Persisted SCP state for slot {}", slot);

        Ok(())
    }

    /// Restore SCP state from persistence.
    ///
    /// Returns the loaded states which should be used to restore SCP state.
    pub fn restore_scp_state(&self) -> Result<RestoredScpState> {
        let mut restored = RestoredScpState::default();

        // Load transaction sets
        let tx_sets = self.storage.load_all_tx_sets()?;
        for (hash, tx_set) in tx_sets {
            restored.tx_sets.push((hash, tx_set));
        }

        // Load SCP states
        let states = self.storage.load_all_scp_states()?;

        for (slot, state) in states {
            // Process quorum sets
            for qs_result in state.get_quorum_sets() {
                match qs_result {
                    Ok(qs) => {
                        let hash = henyey_common::Hash256::hash_xdr(&qs)
                            .map(|h| Hash(*h.as_bytes()))
                            .map_err(|e| {
                                HerderError::InvalidEnvelope(format!(
                                    "failed to hash quorum set: {}",
                                    e
                                ))
                            })?;
                        restored.quorum_sets.push((hash, qs));
                    }
                    Err(e) => {
                        warn!("Failed to restore quorum set for slot {}: {}", slot, e);
                    }
                }
            }

            // Process envelopes
            for env_result in state.get_envelopes() {
                match env_result {
                    Ok(env) => {
                        let env_slot = env.statement.slot_index;
                        restored.envelopes.push((env_slot, env));

                        // Update last slot saved
                        let mut last_saved = self.last_slot_saved.write();
                        *last_saved = (*last_saved).max(env_slot);
                    }
                    Err(e) => {
                        warn!("Failed to restore envelope for slot {}: {}", slot, e);
                    }
                }
            }
        }

        info!(
            "Restored SCP state: {} envelopes, {} tx sets, {} quorum sets",
            restored.envelopes.len(),
            restored.tx_sets.len(),
            restored.quorum_sets.len()
        );

        Ok(restored)
    }

    /// Clean up old persisted state.
    ///
    /// Deletes SCP state and transaction sets for slots below the given threshold.
    pub fn cleanup(&self, min_slot: u64) -> Result<()> {
        self.storage.delete_scp_state_below(min_slot)?;
        self.storage.delete_tx_sets_below(min_slot)?;
        debug!("Cleaned up SCP state below slot {}", min_slot);
        Ok(())
    }
}

/// Restored SCP state from persistence.
#[derive(Debug, Default)]
pub struct RestoredScpState {
    /// Restored envelopes by slot.
    pub envelopes: Vec<(u64, ScpEnvelope)>,
    /// Restored transaction sets.
    pub tx_sets: Vec<(Hash, Vec<u8>)>,
    /// Restored quorum sets.
    pub quorum_sets: Vec<(Hash, ScpQuorumSet)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::*;

    pub(crate) fn make_test_envelope(slot: u64) -> ScpEnvelope {
        ScpEnvelope {
            statement: ScpStatement {
                node_id: NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32]))),
                slot_index: slot,
                pledges: ScpStatementPledges::Nominate(ScpNomination {
                    quorum_set_hash: Hash([0u8; 32]),
                    votes: vec![].try_into().unwrap(),
                    accepted: vec![].try_into().unwrap(),
                }),
            },
            signature: Signature::default(),
        }
    }

    fn make_test_quorum_set() -> ScpQuorumSet {
        ScpQuorumSet {
            threshold: 1,
            validators: vec![NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32])))]
                .try_into()
                .unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        }
    }

    #[test]
    fn test_persisted_scp_state_roundtrip() {
        let mut state = PersistedSlotState::new();
        let envelope = make_test_envelope(100);
        let quorum_set = make_test_quorum_set();

        state.add_envelope(&envelope).unwrap();
        state.add_quorum_set(&quorum_set).unwrap();

        // JSON roundtrip
        let json = state.to_json().unwrap();
        let restored = PersistedSlotState::from_json(&json).unwrap();

        assert_eq!(restored.envelopes.len(), 1);
        assert_eq!(restored.quorum_sets.len(), 1);

        let envelopes: Vec<_> = restored.get_envelopes().into_iter().collect();
        assert!(envelopes[0].is_ok());
        assert_eq!(envelopes[0].as_ref().unwrap().statement.slot_index, 100);
    }

    #[test]
    fn test_persisted_scp_state_base64() {
        let mut state = PersistedSlotState::new();
        let envelope = make_test_envelope(200);

        state.add_envelope(&envelope).unwrap();

        let base64 = state.to_base64().unwrap();
        let restored = PersistedSlotState::from_base64(&base64).unwrap();

        let envelopes: Vec<_> = restored.get_envelopes().into_iter().collect();
        assert_eq!(envelopes.len(), 1);
        assert!(envelopes[0].is_ok());
    }

    #[test]
    fn test_in_memory_persistence() {
        let storage = InMemoryScpPersistence::new();

        let mut state = PersistedSlotState::new();
        let envelope = make_test_envelope(100);
        state.add_envelope(&envelope).unwrap();

        // Save
        storage.save_scp_state(100, &state).unwrap();

        // Load
        let loaded = storage.load_scp_state(100).unwrap();
        assert!(loaded.is_some());

        // Load all
        let all = storage.load_all_scp_states().unwrap();
        assert_eq!(all.len(), 1);

        // Delete below
        storage.delete_scp_state_below(50).unwrap();
        let remaining = storage.load_all_scp_states().unwrap();
        assert_eq!(remaining.len(), 1);

        storage.delete_scp_state_below(101).unwrap();
        let remaining = storage.load_all_scp_states().unwrap();
        assert!(remaining.is_empty());
    }

    #[test]
    fn test_tx_set_persistence() {
        let storage = InMemoryScpPersistence::new();

        let hash = Hash([1u8; 32]);
        let tx_set = vec![1, 2, 3, 4];

        // Save
        storage.save_tx_set(&hash, &tx_set).unwrap();

        // Has
        assert!(storage.has_tx_set(&hash).unwrap());
        assert!(!storage.has_tx_set(&Hash([2u8; 32])).unwrap());

        // Load
        let loaded = storage.load_tx_set(&hash).unwrap();
        assert_eq!(loaded, Some(tx_set.clone()));

        // Load all
        let all = storage.load_all_tx_sets().unwrap();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn test_persistence_manager() {
        let manager = ScpPersistenceManager::in_memory();

        let envelope = make_test_envelope(100);
        let quorum_set = make_test_quorum_set();
        let qs_hash = Hash([1u8; 32]);
        let tx_hash = Hash([2u8; 32]);
        let tx_set = vec![1, 2, 3];

        // Persist
        manager
            .persist_scp_state(
                100,
                &[envelope.clone()],
                &[(tx_hash.clone(), tx_set.clone())],
                &[(qs_hash.clone(), quorum_set.clone())],
            )
            .unwrap();

        assert_eq!(manager.last_slot_saved(), 100);

        // Restore
        let restored = manager.restore_scp_state().unwrap();
        assert_eq!(restored.envelopes.len(), 1);
        assert_eq!(restored.tx_sets.len(), 1);
        assert_eq!(restored.quorum_sets.len(), 1);
    }

    #[test]
    fn test_persistence_manager_skips_old_slots() {
        let manager = ScpPersistenceManager::in_memory();

        let envelope1 = make_test_envelope(100);
        let envelope2 = make_test_envelope(50);

        manager
            .persist_scp_state(100, &[envelope1], &[], &[])
            .unwrap();
        manager
            .persist_scp_state(50, &[envelope2], &[], &[])
            .unwrap(); // Should be skipped

        // Only slot 100 should be saved
        assert_eq!(manager.last_slot_saved(), 100);
    }

    #[test]
    fn test_get_quorum_set_hash() {
        let envelope = make_test_envelope(100);
        let hash = get_quorum_set_hash(&envelope);
        assert!(hash.is_some());
        assert_eq!(hash.unwrap(), Hash([0u8; 32]));
    }
}

// ============================================================================
// SQLite Implementation
// ============================================================================

/// SQLite-backed implementation of SCP state persistence.
///
/// This implementation stores SCP state in the SQLite database for crash recovery.
/// It implements the `ScpStatePersistence` trait, enabling the herder to persist
/// and restore SCP state across restarts.
///
/// # Example
///
/// ```ignore
/// use henyey_herder::persistence::{SqliteScpPersistence, ScpPersistenceManager, Database};
///
/// let db = Database::open("stellar.db")?;
/// let persistence = SqliteScpPersistence::new(db);
/// let manager = ScpPersistenceManager::new(Box::new(persistence));
///
/// // Persist SCP state
/// manager.persist_scp_state(slot, &envelopes, &tx_sets, &quorum_sets)?;
///
/// // Restore on startup
/// let restored = manager.restore_scp_state()?;
/// ```
pub struct SqliteScpPersistence {
    inner: henyey_db::SqliteScpPersistence,
}

impl SqliteScpPersistence {
    /// Create a new SQLite SCP persistence instance.
    pub fn new(db: henyey_db::Database) -> Self {
        Self {
            inner: henyey_db::SqliteScpPersistence::new(db),
        }
    }
}

impl ScpStatePersistence for SqliteScpPersistence {
    fn save_scp_state(&self, slot: u64, state: &PersistedSlotState) -> Result<()> {
        let json = state.to_json()?;
        self.inner
            .save_scp_state(slot, &json)
            .map_err(HerderError::Internal)
    }

    fn load_scp_state(&self, slot: u64) -> Result<Option<PersistedSlotState>> {
        let json = self
            .inner
            .load_scp_state(slot)
            .map_err(HerderError::Internal)?;
        match json {
            Some(j) => Ok(Some(PersistedSlotState::from_json(&j)?)),
            None => Ok(None),
        }
    }

    fn load_all_scp_states(&self) -> Result<Vec<(u64, PersistedSlotState)>> {
        let states = self
            .inner
            .load_all_scp_states()
            .map_err(HerderError::Internal)?;
        let mut result = Vec::new();
        for (slot, json) in states {
            match PersistedSlotState::from_json(&json) {
                Ok(state) => result.push((slot, state)),
                Err(e) => {
                    warn!("Failed to parse persisted state for slot {}: {}", slot, e);
                }
            }
        }
        Ok(result)
    }

    fn delete_scp_state_below(&self, slot: u64) -> Result<()> {
        self.inner
            .delete_scp_state_below(slot)
            .map_err(HerderError::Internal)
    }

    fn save_tx_set(&self, hash: &Hash, tx_set: &[u8]) -> Result<()> {
        self.inner
            .save_tx_set(hash, tx_set)
            .map_err(HerderError::Internal)
    }

    fn load_tx_set(&self, hash: &Hash) -> Result<Option<Vec<u8>>> {
        self.inner.load_tx_set(hash).map_err(HerderError::Internal)
    }

    fn load_all_tx_sets(&self) -> Result<Vec<(Hash, Vec<u8>)>> {
        self.inner.load_all_tx_sets().map_err(HerderError::Internal)
    }

    fn has_tx_set(&self, hash: &Hash) -> Result<bool> {
        self.inner.has_tx_set(hash).map_err(HerderError::Internal)
    }

    fn delete_tx_sets_below(&self, slot: u64) -> Result<()> {
        self.inner
            .delete_tx_sets_below(slot)
            .map_err(HerderError::Internal)
    }
}

#[cfg(test)]
mod sqlite_tests {
    use super::tests::make_test_envelope;
    use super::*;

    #[test]
    fn test_sqlite_persistence_roundtrip() {
        let db = henyey_db::Database::open_in_memory().unwrap();
        let persistence = SqliteScpPersistence::new(db);

        let mut state = PersistedSlotState::new();
        let envelope = make_test_envelope(100);
        state.add_envelope(&envelope).unwrap();

        // Save
        persistence.save_scp_state(100, &state).unwrap();

        // Load
        let loaded = persistence.load_scp_state(100).unwrap();
        assert!(loaded.is_some());
        let loaded_state = loaded.unwrap();
        assert_eq!(loaded_state.envelopes.len(), 1);

        // Load all
        let all = persistence.load_all_scp_states().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].0, 100);

        // Delete below
        persistence.delete_scp_state_below(50).unwrap();
        let remaining = persistence.load_all_scp_states().unwrap();
        assert_eq!(remaining.len(), 1);

        persistence.delete_scp_state_below(101).unwrap();
        let remaining = persistence.load_all_scp_states().unwrap();
        assert!(remaining.is_empty());
    }

    #[test]
    fn test_sqlite_tx_set_persistence() {
        let db = henyey_db::Database::open_in_memory().unwrap();
        let persistence = SqliteScpPersistence::new(db);

        let hash = Hash([1u8; 32]);
        let data = vec![1, 2, 3, 4];

        // Save
        persistence.save_tx_set(&hash, &data).unwrap();

        // Has
        assert!(persistence.has_tx_set(&hash).unwrap());

        // Load
        let loaded = persistence.load_tx_set(&hash).unwrap();
        assert_eq!(loaded, Some(data.clone()));

        // Load all
        let all = persistence.load_all_tx_sets().unwrap();
        assert_eq!(all.len(), 1);
    }
}
