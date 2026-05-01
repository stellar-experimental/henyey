//! Soroban storage adapter.
//!
//! Provides a storage interface for contract state that integrates with
//! our LedgerStateManager.

use stellar_xdr::curr::{
    ContractDataDurability, ContractDataEntry, Hash, LedgerKey, LedgerKeyContractData, ScAddress,
    ScVal,
};

/// A storage key for contract data.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StorageKey {
    /// The contract address.
    pub contract: ScAddress,
    /// The storage key within the contract.
    pub key: ScVal,
    /// The durability of the storage entry.
    pub durability: ContractDataDurability,
}

impl StorageKey {
    /// Create a new storage key.
    pub fn new(contract: ScAddress, key: ScVal, durability: ContractDataDurability) -> Self {
        Self {
            contract,
            key,
            durability,
        }
    }

    /// Convert to a LedgerKey.
    pub fn to_ledger_key(&self) -> LedgerKey {
        LedgerKey::ContractData(LedgerKeyContractData {
            contract: self.contract.clone(),
            key: self.key.clone(),
            durability: self.durability,
        })
    }

    /// Compute the hash of this storage key.
    pub fn hash(&self) -> Hash {
        henyey_common::Hash256::hash_xdr(&self.to_ledger_key()).into()
    }
}

/// A storage entry (key-value pair).
#[derive(Debug, Clone)]
pub struct StorageEntry {
    /// The storage key.
    pub key: StorageKey,
    /// The stored value.
    pub value: ScVal,
    /// Time-to-live (ledger sequence when entry expires).
    pub live_until: u32,
}

impl StorageEntry {
    /// Create a new storage entry.
    pub fn new(key: StorageKey, value: ScVal, live_until: u32) -> Self {
        Self {
            key,
            value,
            live_until,
        }
    }

    /// Check if the entry has expired.
    pub fn is_expired(&self, current_ledger: u32) -> bool {
        self.live_until < current_ledger
    }

    /// Convert to a ContractDataEntry.
    pub fn to_contract_data_entry(&self) -> ContractDataEntry {
        ContractDataEntry {
            ext: stellar_xdr::curr::ExtensionPoint::V0,
            contract: self.key.contract.clone(),
            key: self.key.key.clone(),
            durability: self.key.durability,
            val: self.value.clone(),
        }
    }
}

/// Storage snapshot for contract execution.
///
/// This captures the initial state of all entries in the footprint
/// before execution begins.
#[derive(Debug, Clone, Default)]
pub struct SorobanStorage {
    /// Entries that were read during execution.
    read_entries: std::collections::HashMap<StorageKey, Option<StorageEntry>>,
    /// Entries that were written during execution.
    write_entries: std::collections::HashMap<StorageKey, Option<StorageEntry>>,
    /// Contract code that was read.
    code_entries: std::collections::HashMap<Hash, Option<Vec<u8>>>,
}

impl SorobanStorage {
    /// Create a new empty storage.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a read of an entry.
    pub fn record_read(&mut self, key: StorageKey, entry: Option<StorageEntry>) {
        self.read_entries.entry(key).or_insert(entry);
    }

    /// Record a write to an entry.
    pub fn record_write(&mut self, key: StorageKey, entry: Option<StorageEntry>) {
        self.write_entries.insert(key, entry);
    }

    /// Get a storage entry (from writes first, then reads).
    pub fn get(&self, key: &StorageKey) -> Option<&StorageEntry> {
        // Check writes first (most recent value)
        if let Some(entry) = self.write_entries.get(key) {
            return entry.as_ref();
        }
        // Fall back to reads
        if let Some(entry) = self.read_entries.get(key) {
            return entry.as_ref();
        }
        None
    }

    /// Check if an entry exists.
    pub fn has(&self, key: &StorageKey) -> bool {
        self.get(key).is_some()
    }

    /// Put a storage entry.
    pub fn put(&mut self, key: StorageKey, value: ScVal, live_until: u32) {
        let entry = StorageEntry::new(key.clone(), value, live_until);
        self.record_write(key, Some(entry));
    }

    /// Delete a storage entry.
    pub fn del(&mut self, key: &StorageKey) {
        self.record_write(key.clone(), None);
    }

    /// Record a contract code read.
    pub fn record_code_read(&mut self, hash: Hash, code: Option<Vec<u8>>) {
        self.code_entries.entry(hash).or_insert(code);
    }

    /// Get contract code.
    pub fn get_code(&self, hash: &Hash) -> Option<&Vec<u8>> {
        self.code_entries.get(hash).and_then(|c| c.as_ref())
    }

    // SECURITY: HashMap iteration in test helpers only; production path uses Vec with deterministic ordering
    /// Get all written entries.
    pub fn written_entries(&self) -> impl Iterator<Item = (&StorageKey, &Option<StorageEntry>)> {
        self.write_entries.iter()
    }

    /// Get all created entries (new writes that weren't in reads).
    pub fn created_entries(&self) -> impl Iterator<Item = &StorageEntry> {
        self.write_entries.iter().filter_map(|(key, entry)| {
            if entry.is_some() && !self.read_entries.contains_key(key) {
                entry.as_ref()
            } else {
                None
            }
        })
    }

    /// Get all updated entries (writes that were in reads).
    pub fn updated_entries(&self) -> impl Iterator<Item = &StorageEntry> {
        self.write_entries.iter().filter_map(|(key, entry)| {
            if entry.is_some() && self.read_entries.contains_key(key) {
                entry.as_ref()
            } else {
                None
            }
        })
    }

    /// Get all deleted entries (writes of None that were in reads).
    pub fn deleted_entries(&self) -> impl Iterator<Item = &StorageKey> {
        self.write_entries.iter().filter_map(|(key, entry)| {
            if entry.is_none() && self.read_entries.contains_key(key) {
                Some(key)
            } else {
                None
            }
        })
    }

    /// Clear all storage state.
    pub fn clear(&mut self) {
        self.read_entries.clear();
        self.write_entries.clear();
        self.code_entries.clear();
    }

    /// Get the number of read entries.
    pub fn read_count(&self) -> usize {
        self.read_entries.len()
    }

    /// Get the number of write entries.
    pub fn write_count(&self) -> usize {
        self.write_entries.len()
    }
}

/// Footprint tracking for Soroban transactions.
#[derive(Debug, Clone, Default)]
#[cfg(test)]
pub struct Footprint {
    /// Keys that are read-only.
    pub read_only: Vec<LedgerKey>,
    /// Keys that are read-write.
    pub read_write: Vec<LedgerKey>,
}

#[cfg(test)]
impl Footprint {
    /// Create a new footprint.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a read-only key.
    pub fn add_read_only(&mut self, key: LedgerKey) {
        if !self.read_only.contains(&key) && !self.read_write.contains(&key) {
            self.read_only.push(key);
        }
    }

    /// Add a read-write key.
    pub fn add_read_write(&mut self, key: LedgerKey) {
        // Remove from read_only if present
        self.read_only.retain(|k| k != &key);
        if !self.read_write.contains(&key) {
            self.read_write.push(key);
        }
    }

    /// Check if a key is in the footprint.
    pub fn contains(&self, key: &LedgerKey) -> bool {
        self.read_only.contains(key) || self.read_write.contains(key)
    }

    /// Check if a key is writable.
    pub fn is_writable(&self, key: &LedgerKey) -> bool {
        self.read_write.contains(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{ContractId, LedgerKeyContractCode};

    fn make_contract_address(seed: u8) -> ScAddress {
        ScAddress::Contract(ContractId(Hash([seed; 32])))
    }

    fn make_storage_key(seed: u8) -> StorageKey {
        StorageKey::new(
            make_contract_address(seed),
            ScVal::Symbol("key".try_into().unwrap()),
            ContractDataDurability::Persistent,
        )
    }

    #[test]
    fn test_storage_read_write() {
        let mut storage = SorobanStorage::new();
        let key = make_storage_key(1);

        // Initially empty
        assert!(!storage.has(&key));

        // Write a value
        storage.put(key.clone(), ScVal::I64(42), 1000);
        assert!(storage.has(&key));

        // Read the value
        let entry = storage.get(&key).unwrap();
        assert!(matches!(entry.value, ScVal::I64(42)));
    }

    #[test]
    fn test_storage_delete() {
        let mut storage = SorobanStorage::new();
        let key = make_storage_key(1);

        // Record initial read
        storage.record_read(
            key.clone(),
            Some(StorageEntry::new(key.clone(), ScVal::I64(100), 1000)),
        );

        // Delete it
        storage.del(&key);

        // Should show as deleted
        assert!(!storage.has(&key));
        assert_eq!(storage.deleted_entries().count(), 1);
    }

    #[test]
    fn test_footprint() {
        let mut footprint = Footprint::new();

        let key1 = LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: Hash([1u8; 32]),
        });
        let key2 = LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: Hash([2u8; 32]),
        });

        footprint.add_read_only(key1.clone());
        footprint.add_read_write(key2.clone());

        assert!(footprint.contains(&key1));
        assert!(footprint.contains(&key2));
        assert!(!footprint.is_writable(&key1));
        assert!(footprint.is_writable(&key2));
    }

    /// Test StorageEntry expiration.
    #[test]
    fn test_storage_entry_expiration() {
        let key = make_storage_key(1);
        let entry = StorageEntry::new(key, ScVal::I64(100), 1000);

        // Not expired at ledger 999
        assert!(!entry.is_expired(999));

        // Not expired at ledger 1000 (expires AFTER this)
        assert!(!entry.is_expired(1000));

        // Expired at ledger 1001
        assert!(entry.is_expired(1001));
    }

    /// Test storage key hash computation.
    #[test]
    fn test_storage_key_hash() {
        let key1 = make_storage_key(1);
        let key2 = make_storage_key(2);

        let hash1 = key1.hash();
        let hash2 = key2.hash();

        // Different keys should produce different hashes
        assert_ne!(hash1, hash2);

        // Same key should produce same hash
        let key1_copy = make_storage_key(1);
        let hash1_copy = key1_copy.hash();
        assert_eq!(hash1, hash1_copy);
    }

    /// Test storage key to_ledger_key conversion.
    #[test]
    fn test_storage_key_to_ledger_key() {
        let key = StorageKey::new(
            make_contract_address(5),
            ScVal::U32(42),
            ContractDataDurability::Temporary,
        );

        let ledger_key = key.to_ledger_key();
        match ledger_key {
            LedgerKey::ContractData(cd) => {
                assert_eq!(cd.durability, ContractDataDurability::Temporary);
                assert!(matches!(cd.key, ScVal::U32(42)));
            }
            _ => panic!("Expected ContractData key"),
        }
    }

    /// Test storage created_entries iterator.
    #[test]
    fn test_storage_created_entries() {
        let mut storage = SorobanStorage::new();

        let key1 = make_storage_key(1);
        let key2 = make_storage_key(2);

        // key1 is read first then written (update, not create)
        storage.record_read(
            key1.clone(),
            Some(StorageEntry::new(key1.clone(), ScVal::I64(100), 1000)),
        );
        storage.put(key1, ScVal::I64(200), 2000);

        // key2 is only written (create)
        storage.put(key2, ScVal::I64(300), 3000);

        // Should have 1 created entry (key2)
        let created: Vec<_> = storage.created_entries().collect();
        assert_eq!(created.len(), 1);
        assert!(matches!(created[0].value, ScVal::I64(300)));
    }

    /// Test storage updated_entries iterator.
    #[test]
    fn test_storage_updated_entries() {
        let mut storage = SorobanStorage::new();

        let key1 = make_storage_key(1);
        let key2 = make_storage_key(2);

        // key1 is read first then written (update)
        storage.record_read(
            key1.clone(),
            Some(StorageEntry::new(key1.clone(), ScVal::I64(100), 1000)),
        );
        storage.put(key1, ScVal::I64(200), 2000);

        // key2 is only written (create, not update)
        storage.put(key2, ScVal::I64(300), 3000);

        // Should have 1 updated entry (key1)
        let updated: Vec<_> = storage.updated_entries().collect();
        assert_eq!(updated.len(), 1);
        assert!(matches!(updated[0].value, ScVal::I64(200)));
    }

    /// Test storage clear.
    #[test]
    fn test_storage_clear() {
        let mut storage = SorobanStorage::new();

        let key = make_storage_key(1);
        storage.put(key.clone(), ScVal::I64(100), 1000);
        storage.record_code_read(Hash([1u8; 32]), Some(vec![1, 2, 3]));

        assert!(storage.has(&key));
        assert!(storage.get_code(&Hash([1u8; 32])).is_some());

        storage.clear();

        assert!(!storage.has(&key));
        assert!(storage.get_code(&Hash([1u8; 32])).is_none());
        assert_eq!(storage.read_count(), 0);
        assert_eq!(storage.write_count(), 0);
    }

    /// Test storage code entries.
    #[test]
    fn test_storage_code_entries() {
        let mut storage = SorobanStorage::new();
        let code_hash = Hash([42u8; 32]);
        let code = vec![0xDE, 0xAD, 0xBE, 0xEF];

        storage.record_code_read(code_hash.clone(), Some(code.clone()));

        let retrieved = storage.get_code(&code_hash);
        assert_eq!(retrieved.expect("expected Some"), &code);

        // Non-existent code
        let missing_hash = Hash([99u8; 32]);
        assert!(storage.get_code(&missing_hash).is_none());
    }

    /// Test footprint read_only to read_write promotion.
    #[test]
    fn test_footprint_promotion() {
        let mut footprint = Footprint::new();

        let key = LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: Hash([1u8; 32]),
        });

        // Add as read_only
        footprint.add_read_only(key.clone());
        assert!(!footprint.is_writable(&key));
        assert_eq!(footprint.read_only.len(), 1);
        assert_eq!(footprint.read_write.len(), 0);

        // Promote to read_write
        footprint.add_read_write(key.clone());
        assert!(footprint.is_writable(&key));
        assert_eq!(footprint.read_only.len(), 0); // Removed from read_only
        assert_eq!(footprint.read_write.len(), 1);
    }

    /// Test StorageEntry to_contract_data_entry conversion.
    #[test]
    fn test_storage_entry_to_contract_data() {
        let key = StorageKey::new(
            make_contract_address(10),
            ScVal::Bytes(vec![1, 2, 3, 4].try_into().unwrap()),
            ContractDataDurability::Persistent,
        );
        // Use a simple ScVal type
        let value = ScVal::I128(stellar_xdr::curr::Int128Parts { hi: 0, lo: 42 });
        let entry = StorageEntry::new(key, value.clone(), 5000);

        let contract_data = entry.to_contract_data_entry();
        assert!(matches!(contract_data.val, ScVal::I128(_)));
        assert_eq!(contract_data.durability, ContractDataDurability::Persistent);
    }
}
