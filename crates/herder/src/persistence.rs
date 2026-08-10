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
use stellar_xdr::{
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

    /// Return the hashes of all persisted transaction sets.
    fn get_all_tx_set_hashes(&self) -> Result<Vec<Hash>>;

    /// Delete persisted transaction sets by their hashes.
    fn delete_tx_sets_by_hashes(&self, hashes: &[Hash]) -> Result<()>;

    /// Atomic purge of unreferenced persisted transaction sets.
    ///
    /// Implementations that have transactional storage (e.g. SQLite) should
    /// override this to wrap the read-then-delete sequence in a single
    /// transaction, so a concurrent `save_tx_set` cannot have its tx-set
    /// deleted as an orphan between the read and the delete.
    ///
    /// The default implementation is the historical three-call sequence
    /// (read hashes, read SCP states, delete orphans). It is safe for
    /// in-process backends whose individual calls already serialize on a
    /// single lock (e.g. `InMemoryScpPersistence`); it is **not** safe for
    /// backends that check out a fresh connection per call.
    ///
    /// Mirrors stellar-core `HerderImpl::purgeOldPersistedTxSets()`
    /// (HerderImpl.cpp:2448-2487).
    fn purge_unreferenced_tx_sets_atomic(&self) -> Result<()> {
        let all_hashes: std::collections::HashSet<Hash> =
            self.get_all_tx_set_hashes()?.into_iter().collect();
        if all_hashes.is_empty() {
            return Ok(());
        }

        let mut referenced = std::collections::HashSet::new();
        for (slot, state) in self.load_all_scp_states()? {
            match extract_tx_set_hashes_from_state(&state) {
                Ok(hashes) => referenced.extend(hashes),
                Err(e) => warn!(
                    slot,
                    "Error extracting tx set hashes from persisted state, skipping: {}", e
                ),
            }
        }

        let unreferenced: Vec<Hash> = all_hashes.difference(&referenced).cloned().collect();
        if !unreferenced.is_empty() {
            debug!(
                count = unreferenced.len(),
                "Purging unreferenced persisted tx sets"
            );
            self.delete_tx_sets_by_hashes(&unreferenced)?;
        }
        Ok(())
    }
}

/// Extract all tx set hashes from a persisted slot state.
///
/// Returns an error if any envelope in the state cannot be decoded,
/// allowing the caller to skip the entire state (per-state error granularity).
fn extract_tx_set_hashes_from_state(state: &PersistedSlotState) -> Result<Vec<Hash>> {
    let mut hashes = Vec::new();
    for env_result in state.get_envelopes() {
        let envelope = env_result?;
        hashes.extend(get_tx_set_hashes(&envelope));
    }
    Ok(hashes)
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

    fn get_all_tx_set_hashes(&self) -> Result<Vec<Hash>> {
        Ok(self.tx_sets.read().keys().cloned().collect())
    }

    fn delete_tx_sets_by_hashes(&self, hashes: &[Hash]) -> Result<()> {
        let mut tx_sets = self.tx_sets.write();
        for hash in hashes {
            tx_sets.remove(hash);
        }
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
    /// Number of persist worker threads spawned by [`crate::herder::Herder`]'s
    /// SCP-emit callback that have not yet returned. The callback defers the
    /// gather + persist work onto a dedicated thread to avoid deadlocking on
    /// SCP's internal slots lock (which is write-held while `emit` fires).
    ///
    /// Tests use [`Self::wait_for_pending_persists`] to synchronize on the
    /// background work completing before asserting `last_slot_saved`.
    pending_persists: std::sync::Mutex<usize>,
    /// Condvar signaled when `pending_persists` reaches zero.
    pending_persists_cv: std::sync::Condvar,
    /// Lock-free mirror of `pending_persists`, published under the same mutex
    /// (see `pending_persist_begin`/`_end`). The `/metrics` scrape path reads
    /// this with `Ordering::Relaxed` and never touches the mutex, so it can
    /// never contend with a persist worker. `pending_persists` remains the
    /// single source of truth (it is paired with `pending_persists_cv`).
    persist_pending: std::sync::atomic::AtomicU64,
    /// Monotonic high-water mark of `persist_pending`. Process-lifetime; resets
    /// on restart. Mirrors the accepted `SCP_VERIFY_INPUT_BACKLOG_PEAK`
    /// precedent — a live gauge alone would read `0` on the large majority of
    /// scrapes (see #3796 §2: concurrency was `0` for 82.9% of wall-clock).
    persist_pending_peak: std::sync::atomic::AtomicU64,
    /// Total persist worker spawns attempted (incremented in
    /// `pending_persist_begin`, which runs BEFORE the thread is spawned).
    /// `attempted - spawn_failed = threads actually spawned`.
    persist_attempted_total: std::sync::atomic::AtomicU64,
    /// Total persist worker spawns that failed (e.g. `EAGAIN` / thread-table
    /// exhaustion). Bumped only by `record_spawn_failure`.
    persist_spawn_failed_total: std::sync::atomic::AtomicU64,
}

/// Snapshot of the SCP persist-worker backlog counters for the `/metrics`
/// scrape path (issue #3796).
///
/// - `pending`: persist workers currently in flight (live gauge).
/// - `pending_peak`: process-lifetime high-water mark of `pending`.
/// - `attempted_total`: cumulative persist spawns attempted.
/// - `spawn_failed_total`: cumulative persist spawns that failed to launch.
///
/// `attempted_total - spawn_failed_total` is the number of persist worker
/// threads actually spawned.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScpPersistStats {
    /// Persist workers currently in flight.
    pub pending: u64,
    /// Process-lifetime high-water mark of `pending`.
    pub pending_peak: u64,
    /// Cumulative persist spawns attempted (before the spawn).
    pub attempted_total: u64,
    /// Cumulative persist spawns that failed to launch.
    pub spawn_failed_total: u64,
}

impl ScpPersistenceManager {
    /// Create a new persistence manager with the given storage backend.
    pub fn new(storage: Box<dyn ScpStatePersistence>) -> Self {
        Self {
            storage,
            last_slot_saved: parking_lot::RwLock::new(0),
            pending_persists: std::sync::Mutex::new(0),
            pending_persists_cv: std::sync::Condvar::new(),
            persist_pending: std::sync::atomic::AtomicU64::new(0),
            persist_pending_peak: std::sync::atomic::AtomicU64::new(0),
            persist_attempted_total: std::sync::atomic::AtomicU64::new(0),
            persist_spawn_failed_total: std::sync::atomic::AtomicU64::new(0),
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

    /// Check whether a tx set with the given hash is present in the backing
    /// store. Primarily used by tests to assert post-purge state.
    pub fn has_tx_set(&self, hash: &Hash) -> Result<bool> {
        self.storage.has_tx_set(hash)
    }

    /// Increment the pending-persist counter. Called by the emit callback's
    /// worker thread spawner BEFORE the thread is detached.
    pub(crate) fn pending_persist_begin(&self) {
        use std::sync::atomic::Ordering;
        let mut pending = self
            .pending_persists
            .lock()
            .expect("pending mutex poisoned");
        *pending += 1;
        // Publish the atomic mirrors WHILE the guard is still live, so the
        // scrape path and any `wait_for_pending_persists` waiter observe a
        // consistent view. `attempted_total` counts attempts (this runs before
        // the spawn), and the peak is exact because it is taken under the lock.
        let now = *pending as u64;
        self.persist_pending.store(now, Ordering::Relaxed);
        self.persist_pending_peak.fetch_max(now, Ordering::Relaxed);
        self.persist_attempted_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Decrement the pending-persist counter and notify waiters. Called by
    /// the worker thread on completion (success or error).
    pub(crate) fn pending_persist_end(&self) {
        use std::sync::atomic::Ordering;
        let mut pending = self
            .pending_persists
            .lock()
            .expect("pending mutex poisoned");
        *pending = pending.saturating_sub(1);
        // Store the mirror BEFORE `notify_all` and BEFORE dropping the guard —
        // otherwise a waiter released by `notify_all` could observe
        // `wait_for_pending_persists() == true` while `persist_stats().pending`
        // is still nonzero (#3796 plan, Critic A).
        self.persist_pending
            .store(*pending as u64, Ordering::Relaxed);
        if *pending == 0 {
            self.pending_persists_cv.notify_all();
        }
    }

    /// Record a persist worker spawn failure (e.g. `EAGAIN` / thread-table
    /// exhaustion). Additive to — never a replacement for —
    /// `pending_persist_end`, which the failure path must still call to release
    /// the pending count.
    pub(crate) fn record_spawn_failure(&self) {
        use std::sync::atomic::Ordering;
        self.persist_spawn_failed_total
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Snapshot the SCP persist-worker backlog counters for the metrics scrape
    /// path. Lock-free: reads the atomic mirrors with `Ordering::Relaxed` and
    /// never touches the `pending_persists` mutex.
    pub fn persist_stats(&self) -> ScpPersistStats {
        use std::sync::atomic::Ordering;
        ScpPersistStats {
            pending: self.persist_pending.load(Ordering::Relaxed),
            pending_peak: self.persist_pending_peak.load(Ordering::Relaxed),
            attempted_total: self.persist_attempted_total.load(Ordering::Relaxed),
            spawn_failed_total: self.persist_spawn_failed_total.load(Ordering::Relaxed),
        }
    }

    /// Wait for all in-flight persist worker threads to finish, up to
    /// `timeout`. Returns `true` if the queue drained, `false` on timeout.
    ///
    /// Tests use this after `trigger_next_ledger` to ensure persist threads
    /// spawned by the emit hot path have completed before asserting state.
    pub fn wait_for_pending_persists(&self, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        let mut pending = self
            .pending_persists
            .lock()
            .expect("pending mutex poisoned");
        while *pending > 0 {
            let now = std::time::Instant::now();
            if now >= deadline {
                return false;
            }
            let remaining = deadline - now;
            let (next, wait_result) = self
                .pending_persists_cv
                .wait_timeout(pending, remaining)
                .expect("pending cv poisoned");
            pending = next;
            if wait_result.timed_out() && *pending > 0 {
                return false;
            }
        }
        true
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

        // Instrument the WAL write-lock hold so a long persist is self-naming
        // in the logs (#3702). Instrumentation only — no txn-scope change.
        let _write_ctx =
            henyey_common::WriteCtxGuard::new(format!("scp-persist-write slot={slot}"));

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
    /// Purges unreferenced tx sets before loading, matching stellar-core's
    /// behavior where `purgeOldPersistedTxSets()` runs during `restoreSCPState()`.
    pub fn restore_scp_state(&self) -> Result<RestoredScpState> {
        // Purge stale tx sets before loading them into memory.
        // Parity: stellar-core HerderImpl.cpp:2189-2197.
        self.purge_unreferenced_tx_sets()?;

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
                        let hash = Hash(*henyey_common::Hash256::hash_xdr(&qs).as_bytes());
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
    /// Deletes SCP state for slots below the given threshold, then purges
    /// transaction sets no longer referenced by any remaining SCP state.
    pub fn cleanup(&self, min_slot: u64) -> Result<()> {
        self.storage.delete_scp_state_below(min_slot)?;
        self.purge_unreferenced_tx_sets()?;
        debug!("Cleaned up SCP state below slot {}", min_slot);
        Ok(())
    }

    /// Purge persisted tx sets not referenced by any persisted SCP state.
    ///
    /// Delegates to `ScpStatePersistence::purge_unreferenced_tx_sets_atomic`,
    /// which the SQLite backend overrides to wrap the read-then-delete
    /// sequence in a single transaction (preventing a concurrent
    /// `persist_scp_state` from having its freshly-inserted tx-set deleted
    /// as an orphan between the read and the delete). See #2770.
    ///
    /// Acquires the same in-process `last_slot_saved` lock that
    /// `persist_scp_state` holds, serializing purge w.r.t. all persists at
    /// the manager level. This is needed because `persist_scp_state` itself
    /// performs two separate storage calls (`save_tx_set` then
    /// `save_scp_state`), so a purge that interleaved between them would
    /// still observe a tx-set without its referencing state. Matches
    /// stellar-core (`HerderImpl.cpp`), which serializes persist and purge
    /// on the main thread.
    ///
    /// Mirrors stellar-core's `purgeOldPersistedTxSets()` (`HerderImpl.cpp:2448-2487`).
    /// Error handling is per-state: if a state fails to decode, the state is
    /// skipped (its tx-set references can't be proven, so its tx-sets may be
    /// deleted as orphans).
    pub fn purge_unreferenced_tx_sets(&self) -> Result<()> {
        // Hold the same lock `persist_scp_state` takes so the two operations
        // serialize at the manager level (in addition to the SQLite-level
        // BEGIN IMMEDIATE inside the SQLite backend's atomic implementation).
        let _guard = self.last_slot_saved.write();
        self.storage.purge_unreferenced_tx_sets_atomic()
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
    use stellar_xdr::*;

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

    // --- SCP persist-worker stats (#3796) ---

    #[test]
    fn test_persist_stats_mirror_matches_pending_counter() {
        let manager = ScpPersistenceManager::in_memory();
        manager.pending_persist_begin();
        manager.pending_persist_begin();
        manager.pending_persist_begin();
        manager.pending_persist_end();
        manager.pending_persist_end();

        let stats = manager.persist_stats();
        assert_eq!(stats.pending, 1);
        assert_eq!(stats.pending_peak, 3);
        assert_eq!(stats.attempted_total, 3);
        assert_eq!(stats.spawn_failed_total, 0);
        // The atomic mirror must agree with the mutex source of truth.
        assert_eq!(
            stats.pending as usize,
            *manager.pending_persists.lock().unwrap()
        );
    }

    #[test]
    fn test_persist_stats_peak_is_monotonic() {
        let manager = ScpPersistenceManager::in_memory();
        for _ in 0..3 {
            manager.pending_persist_begin();
        }
        for _ in 0..3 {
            manager.pending_persist_end();
        }
        assert_eq!(manager.persist_stats().pending, 0);
        assert_eq!(manager.persist_stats().pending_peak, 3);

        // Draining to zero must not reset the peak.
        manager.pending_persist_begin();
        let stats = manager.persist_stats();
        assert_eq!(stats.pending, 1);
        assert_eq!(stats.pending_peak, 3);
    }

    #[test]
    fn test_persist_stats_end_saturates_at_zero() {
        let manager = ScpPersistenceManager::in_memory();
        // End with nothing pending must not underflow (a wrapped u64 would
        // render as ~1.8e19 in the gauge).
        manager.pending_persist_end();
        assert_eq!(manager.persist_stats().pending, 0);
    }

    #[test]
    fn test_persist_attempted_minus_spawn_failed_is_actual_spawns() {
        let manager = ScpPersistenceManager::in_memory();
        manager.pending_persist_begin();
        manager.pending_persist_begin();
        manager.pending_persist_begin();
        manager.record_spawn_failure();

        let stats = manager.persist_stats();
        assert_eq!(stats.attempted_total, 3);
        assert_eq!(stats.spawn_failed_total, 1);
        // Documented arithmetic: attempted - spawn_failed == threads spawned.
        assert_eq!(stats.attempted_total - stats.spawn_failed_total, 2);
    }

    #[test]
    fn test_spawn_failure_path_does_not_leak_pending() {
        let manager = ScpPersistenceManager::in_memory();
        // Mirror the herder's `.map_err` arm: begin, then on spawn failure
        // record the failure AND still end the pending count.
        manager.pending_persist_begin();
        manager.record_spawn_failure();
        manager.pending_persist_end();

        let stats = manager.persist_stats();
        assert_eq!(stats.pending, 0);
        assert_eq!(stats.spawn_failed_total, 1);
    }

    #[test]
    fn test_persist_stats_concurrent_begin_end_settles_to_zero() {
        use std::sync::Arc;
        let manager = Arc::new(ScpPersistenceManager::in_memory());
        let mut handles = Vec::new();
        for _ in 0..8 {
            let m = Arc::clone(&manager);
            handles.push(std::thread::spawn(move || {
                for _ in 0..50 {
                    m.pending_persist_begin();
                    m.pending_persist_end();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let stats = manager.persist_stats();
        assert_eq!(stats.pending, 0);
        assert_eq!(stats.attempted_total, 400);
        assert!(
            stats.pending_peak >= 1 && stats.pending_peak <= 8,
            "peak {} out of expected 1..=8 range",
            stats.pending_peak
        );
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

        let quorum_set = make_test_quorum_set();
        let qs_hash = Hash([1u8; 32]);
        let tx_hash = Hash([2u8; 32]);
        let tx_set = vec![1, 2, 3];

        // Use an envelope that references the tx set hash
        let envelope = make_envelope_with_tx_set_hash(100, tx_hash.clone());

        // Persist
        manager
            .persist_scp_state(
                100,
                std::slice::from_ref(&envelope),
                &[(tx_hash.clone(), tx_set.clone())],
                &[(qs_hash.clone(), quorum_set.clone())],
            )
            .unwrap();

        assert_eq!(manager.last_slot_saved(), 100);

        // Restore (purges unreferenced tx sets, but tx_hash IS referenced)
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

    /// Create an envelope whose NOMINATE ballot references a known tx set hash.
    pub(super) fn make_envelope_with_tx_set_hash(slot: u64, tx_set_hash: Hash) -> ScpEnvelope {
        let stellar_value = StellarValue {
            tx_set_hash: tx_set_hash.clone(),
            close_time: TimePoint(0),
            upgrades: vec![].try_into().unwrap(),
            ext: StellarValueExt::Basic,
        };
        let value = stellar_value.to_xdr(Limits::none()).unwrap();
        ScpEnvelope {
            statement: ScpStatement {
                node_id: NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32]))),
                slot_index: slot,
                pledges: ScpStatementPledges::Nominate(ScpNomination {
                    quorum_set_hash: Hash([0u8; 32]),
                    votes: vec![Value(value.try_into().unwrap())].try_into().unwrap(),
                    accepted: vec![].try_into().unwrap(),
                }),
            },
            signature: Signature::default(),
        }
    }

    #[test]
    fn test_purge_unreferenced_tx_sets_empty_db() {
        let manager = ScpPersistenceManager::in_memory();
        // No tx sets at all — should be a no-op.
        manager.purge_unreferenced_tx_sets().unwrap();
    }

    #[test]
    fn test_purge_unreferenced_tx_sets_deletes_orphans() {
        let manager = ScpPersistenceManager::in_memory();

        let referenced_hash = Hash([1u8; 32]);
        let orphan_hash = Hash([2u8; 32]);

        // Persist an SCP state referencing `referenced_hash`
        let env = make_envelope_with_tx_set_hash(100, referenced_hash.clone());
        manager
            .persist_scp_state(100, &[env], &[(referenced_hash.clone(), vec![10])], &[])
            .unwrap();

        // Also store an orphan tx set not referenced by any envelope
        manager.storage.save_tx_set(&orphan_hash, &[20]).unwrap();

        // Purge should delete only the orphan
        manager.purge_unreferenced_tx_sets().unwrap();

        assert!(manager.storage.has_tx_set(&referenced_hash).unwrap());
        assert!(
            !manager.storage.has_tx_set(&orphan_hash).unwrap(),
            "Orphan tx set should have been purged"
        );
    }

    #[test]
    fn test_purge_unreferenced_tx_sets_all_referenced() {
        let manager = ScpPersistenceManager::in_memory();

        let hash = Hash([1u8; 32]);
        let env = make_envelope_with_tx_set_hash(100, hash.clone());
        manager
            .persist_scp_state(100, &[env], &[(hash.clone(), vec![10])], &[])
            .unwrap();

        // No orphans — purge should be a no-op
        manager.purge_unreferenced_tx_sets().unwrap();
        assert!(manager.storage.has_tx_set(&hash).unwrap());
    }

    #[test]
    fn test_purge_unreferenced_tx_sets_no_scp_states() {
        let manager = ScpPersistenceManager::in_memory();

        // Store tx sets but no SCP states — all are orphans
        let hash = Hash([1u8; 32]);
        manager.storage.save_tx_set(&hash, &[10]).unwrap();

        manager.purge_unreferenced_tx_sets().unwrap();
        assert!(
            !manager.storage.has_tx_set(&hash).unwrap(),
            "Tx set should be purged when no SCP states reference it"
        );
    }

    #[test]
    fn test_purge_shared_tx_set_across_slots() {
        let manager = ScpPersistenceManager::in_memory();

        let shared_hash = Hash([1u8; 32]);

        // Two slots reference the same tx set
        let env1 = make_envelope_with_tx_set_hash(100, shared_hash.clone());
        let env2 = make_envelope_with_tx_set_hash(101, shared_hash.clone());
        manager
            .persist_scp_state(100, &[env1], &[(shared_hash.clone(), vec![10])], &[])
            .unwrap();
        manager.persist_scp_state(101, &[env2], &[], &[]).unwrap();

        // Delete slot 100's SCP state
        manager.storage.delete_scp_state_below(101).unwrap();

        // Purge: the tx set is still referenced by slot 101
        manager.purge_unreferenced_tx_sets().unwrap();
        assert!(
            manager.storage.has_tx_set(&shared_hash).unwrap(),
            "Shared tx set should not be purged while still referenced"
        );
    }

    #[test]
    fn test_cleanup_deletes_states_and_purges_tx_sets() {
        let manager = ScpPersistenceManager::in_memory();

        let hash1 = Hash([1u8; 32]);
        let hash2 = Hash([2u8; 32]);

        // Slot 100 references hash1, slot 200 references hash2
        let env1 = make_envelope_with_tx_set_hash(100, hash1.clone());
        let env2 = make_envelope_with_tx_set_hash(200, hash2.clone());
        manager
            .persist_scp_state(100, &[env1], &[(hash1.clone(), vec![10])], &[])
            .unwrap();
        manager
            .persist_scp_state(200, &[env2], &[(hash2.clone(), vec![20])], &[])
            .unwrap();

        // Cleanup with min_slot=200 deletes slot 100's state
        manager.cleanup(200).unwrap();

        // hash1 is now orphaned (slot 100 state was deleted), hash2 is still referenced
        assert!(
            !manager.storage.has_tx_set(&hash1).unwrap(),
            "hash1 should be purged after its slot state was deleted"
        );
        assert!(
            manager.storage.has_tx_set(&hash2).unwrap(),
            "hash2 should remain referenced"
        );
    }

    #[test]
    fn test_restore_scp_state_purges_stale_tx_sets() {
        let manager = ScpPersistenceManager::in_memory();

        // Store a tx set but no SCP state referencing it
        let orphan_hash = Hash([1u8; 32]);
        manager.storage.save_tx_set(&orphan_hash, &[10]).unwrap();

        // Restore should purge the orphan before loading
        let restored = manager.restore_scp_state().unwrap();
        assert!(
            restored.tx_sets.is_empty(),
            "Orphan tx set should not be loaded"
        );
        assert!(
            !manager.storage.has_tx_set(&orphan_hash).unwrap(),
            "Orphan tx set should be purged from storage"
        );
    }
    #[test]
    fn test_purge_skips_corrupt_state_preserves_valid_refs() {
        let manager = ScpPersistenceManager::in_memory();

        // Slot 100: valid envelope referencing tx_hash_a
        let tx_hash_a = Hash([0xAA; 32]);
        let env = make_envelope_with_tx_set_hash(100, tx_hash_a.clone());
        manager
            .persist_scp_state(100, &[env], &[(tx_hash_a.clone(), vec![10])], &[])
            .unwrap();

        // Slot 101: inject a corrupt SCP state (invalid envelope bytes)
        // Also store tx_hash_b which is only "referenced" by the corrupt state
        let tx_hash_b = Hash([0xBB; 32]);
        manager.storage.save_tx_set(&tx_hash_b, &[20]).unwrap();
        let mut corrupt_state = PersistedSlotState::new();
        corrupt_state.envelopes.push(vec![0xFF, 0xFF, 0xFF]); // invalid XDR
        manager.storage.save_scp_state(101, &corrupt_state).unwrap();

        // Purge should succeed without panicking.
        // tx_hash_a survives (referenced by valid slot 100).
        // tx_hash_b is deleted — the corrupt state can't prove it's referenced.
        // This matches stellar-core behavior where corrupt states are logged and skipped.
        manager.purge_unreferenced_tx_sets().unwrap();

        assert!(
            manager.storage.has_tx_set(&tx_hash_a).unwrap(),
            "Referenced tx set should survive"
        );
        assert!(
            !manager.storage.has_tx_set(&tx_hash_b).unwrap(),
            "Unreferenced tx set from corrupt state should be purged"
        );
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

    fn get_all_tx_set_hashes(&self) -> Result<Vec<Hash>> {
        self.inner
            .get_all_tx_set_hashes()
            .map_err(HerderError::Internal)
    }

    fn delete_tx_sets_by_hashes(&self, hashes: &[Hash]) -> Result<()> {
        self.inner
            .delete_tx_sets_by_hashes(hashes)
            .map_err(HerderError::Internal)
    }

    /// Override the trait default with a SQLite-transactional implementation
    /// so the read-then-delete sequence is atomic w.r.t. concurrent writers.
    /// See `henyey_db::SqliteScpPersistence::purge_unreferenced_tx_sets_atomic`
    /// (#2770).
    fn purge_unreferenced_tx_sets_atomic(&self) -> Result<()> {
        self.inner
            .purge_unreferenced_tx_sets_atomic()
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

    /// Exercise the atomic purge through `ScpPersistenceManager` backed by
    /// `SqliteScpPersistence`. This is the path used in production once the
    /// persist timer is wired (#2768).
    #[test]
    fn test_purge_atomic_via_manager_sqlite() {
        use super::tests::make_envelope_with_tx_set_hash;

        let db = henyey_db::Database::open_in_memory().unwrap();
        let manager = ScpPersistenceManager::new(Box::new(SqliteScpPersistence::new(db)));

        let referenced = Hash([0xAA; 32]);
        let orphan = Hash([0xBB; 32]);

        // Slot 100 references `referenced`; slot 50 references `orphan` then
        // gets cleaned up so `orphan` becomes unreferenced.
        let env_r = make_envelope_with_tx_set_hash(100, referenced.clone());
        manager
            .persist_scp_state(100, &[env_r], &[(referenced.clone(), vec![1])], &[])
            .unwrap();
        let env_o = make_envelope_with_tx_set_hash(50, orphan.clone());
        manager
            .persist_scp_state(50, &[env_o], &[(orphan.clone(), vec![2])], &[])
            .unwrap();

        // Drop slot 50's state -> orphan is now unreferenced.
        // Cleanup itself calls purge — but we then call purge again to
        // confirm the atomic path is idempotent.
        manager.cleanup(100).unwrap();
        manager.purge_unreferenced_tx_sets().unwrap();

        assert!(manager.has_tx_set(&referenced).unwrap());
        assert!(!manager.has_tx_set(&orphan).unwrap());
    }
}
