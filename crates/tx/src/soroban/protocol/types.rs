//! Shared types for protocol-versioned host implementations.

use stellar_xdr::{LedgerEntry, LedgerEntryData, LedgerKey, LedgerKeyTtl};

/// An entry restored from the live BucketList (had expired TTL but wasn't yet evicted).
///
/// This type enforces structural pairing invariants at construction time:
/// - The key must be a persistent Soroban key (ContractCode or persistent ContractData)
/// - The entry must correspond to the key
/// - The TTL key must be the correctly derived TTL key for the data key
/// - The TTL entry must be a TTL entry that corresponds to the TTL key
///
/// Note: This type enforces only structural pairing, not expiry/liveness semantics
/// (whether the entry is actually expired is contextual to the ledger state).
#[derive(Debug, Clone)]
pub struct LiveBucketListRestore {
    key: LedgerKey,
    entry: LedgerEntry,
    ttl_key: LedgerKey,
    ttl_entry: LedgerEntry,
}

impl LiveBucketListRestore {
    /// Create a new validated `LiveBucketListRestore`.
    ///
    /// # Panics
    ///
    /// Panics if any structural invariant is violated:
    /// - `key` is not a persistent Soroban key
    /// - `entry` does not correspond to `key`
    /// - `ttl_key` is not the correctly derived TTL key for `key`
    /// - `ttl_entry` is not a TTL entry
    /// - `ttl_entry` does not correspond to `ttl_key`
    pub fn new(
        key: LedgerKey,
        entry: LedgerEntry,
        ttl_key: LedgerKey,
        ttl_entry: LedgerEntry,
    ) -> Self {
        assert!(
            henyey_common::is_persistent_key(&key),
            "LiveBucketListRestore::new: key must be a persistent Soroban key, got: {:?}",
            key
        );

        assert_eq!(
            henyey_common::entry_to_key(&entry),
            key,
            "LiveBucketListRestore::new: entry does not correspond to key"
        );

        let expected_ttl_key = LedgerKey::Ttl(LedgerKeyTtl {
            key_hash: crate::soroban::compute_key_hash(&key),
        });
        assert_eq!(
            ttl_key, expected_ttl_key,
            "LiveBucketListRestore::new: ttl_key does not match derived TTL key for data key"
        );

        assert!(
            matches!(ttl_entry.data, LedgerEntryData::Ttl(_)),
            "LiveBucketListRestore::new: ttl_entry must be a TTL entry, got: {:?}",
            ttl_entry.data
        );

        assert_eq!(
            henyey_common::entry_to_key(&ttl_entry),
            ttl_key,
            "LiveBucketListRestore::new: ttl_entry does not correspond to ttl_key"
        );

        Self {
            key,
            entry,
            ttl_key,
            ttl_entry,
        }
    }

    /// The ledger key of the restored entry (ContractData or ContractCode).
    pub fn key(&self) -> &LedgerKey {
        &self.key
    }

    /// The entry that was restored (pre-modification state).
    pub fn entry(&self) -> &LedgerEntry {
        &self.entry
    }

    /// The TTL key for this entry.
    pub fn ttl_key(&self) -> &LedgerKey {
        &self.ttl_key
    }

    /// The TTL entry that was restored (pre-modification state with old expired TTL).
    pub fn ttl_entry(&self) -> &LedgerEntry {
        &self.ttl_entry
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::{
        ContractCodeEntry, ContractDataDurability, ContractDataEntry, ContractId, ExtensionPoint,
        Hash, LedgerEntryExt, LedgerKeyContractCode, LedgerKeyContractData, ScAddress, ScVal,
        TtlEntry,
    };

    fn persistent_data_key() -> LedgerKey {
        LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(Hash([1u8; 32]))),
            key: ScVal::Void,
            durability: ContractDataDurability::Persistent,
        })
    }

    fn persistent_data_entry() -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::ContractData(ContractDataEntry {
                ext: ExtensionPoint::V0,
                contract: ScAddress::Contract(ContractId(Hash([1u8; 32]))),
                key: ScVal::Void,
                durability: ContractDataDurability::Persistent,
                val: ScVal::I32(42),
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    fn contract_code_key() -> LedgerKey {
        LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: Hash([2u8; 32]),
        })
    }

    fn contract_code_entry() -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::ContractCode(ContractCodeEntry {
                ext: stellar_xdr::ContractCodeEntryExt::V0,
                hash: Hash([2u8; 32]),
                code: vec![0u8; 10].try_into().unwrap(),
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    fn make_ttl_key(data_key: &LedgerKey) -> LedgerKey {
        LedgerKey::Ttl(LedgerKeyTtl {
            key_hash: crate::soroban::compute_key_hash(data_key),
        })
    }

    fn make_ttl_entry(data_key: &LedgerKey) -> LedgerEntry {
        let key_hash = crate::soroban::compute_key_hash(data_key);
        LedgerEntry {
            last_modified_ledger_seq: 50,
            data: LedgerEntryData::Ttl(TtlEntry {
                key_hash,
                live_until_ledger_seq: 99,
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    fn make_valid_restore() -> LiveBucketListRestore {
        let key = persistent_data_key();
        let entry = persistent_data_entry();
        let ttl_key = make_ttl_key(&key);
        let ttl_entry = make_ttl_entry(&key);
        LiveBucketListRestore::new(key, entry, ttl_key, ttl_entry)
    }

    // --- Happy path tests ---

    #[test]
    fn test_new_persistent_contract_data() {
        let restore = make_valid_restore();
        assert_eq!(*restore.key(), persistent_data_key());
        assert_eq!(restore.entry().last_modified_ledger_seq, 100);
        assert_eq!(*restore.ttl_key(), make_ttl_key(&persistent_data_key()));
        assert_eq!(restore.ttl_entry().last_modified_ledger_seq, 50);
    }

    #[test]
    fn test_new_contract_code() {
        let key = contract_code_key();
        let entry = contract_code_entry();
        let ttl_key = make_ttl_key(&key);
        let ttl_entry = make_ttl_entry(&key);
        let restore = LiveBucketListRestore::new(key.clone(), entry, ttl_key, ttl_entry);
        assert_eq!(*restore.key(), key);
    }

    #[test]
    fn test_debug_format() {
        let restore = make_valid_restore();
        let debug_str = format!("{:?}", restore);
        assert!(debug_str.contains("LiveBucketListRestore"));
    }

    // --- Constructor panic tests ---

    #[test]
    #[should_panic(expected = "key must be a persistent Soroban key")]
    fn test_new_panics_on_ttl_key() {
        let data_key = persistent_data_key();
        let ttl_key = make_ttl_key(&data_key);
        let ttl_entry = make_ttl_entry(&data_key);
        // Pass the TTL key as the data key — should panic
        LiveBucketListRestore::new(ttl_key.clone(), ttl_entry.clone(), ttl_key, ttl_entry);
    }

    #[test]
    #[should_panic(expected = "key must be a persistent Soroban key")]
    fn test_new_panics_on_account_key() {
        let account_key = LedgerKey::Account(stellar_xdr::LedgerKeyAccount {
            account_id: stellar_xdr::AccountId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
                stellar_xdr::Uint256([0u8; 32]),
            )),
        });
        let entry = persistent_data_entry();
        let data_key = persistent_data_key();
        let ttl_key = make_ttl_key(&data_key);
        let ttl_entry = make_ttl_entry(&data_key);
        LiveBucketListRestore::new(account_key, entry, ttl_key, ttl_entry);
    }

    #[test]
    #[should_panic(expected = "key must be a persistent Soroban key")]
    fn test_new_panics_on_temporary_contract_data() {
        let temp_key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(Hash([1u8; 32]))),
            key: ScVal::Void,
            durability: ContractDataDurability::Temporary,
        });
        let entry = persistent_data_entry();
        let data_key = persistent_data_key();
        let ttl_key = make_ttl_key(&data_key);
        let ttl_entry = make_ttl_entry(&data_key);
        LiveBucketListRestore::new(temp_key, entry, ttl_key, ttl_entry);
    }

    #[test]
    #[should_panic(expected = "entry does not correspond to key")]
    fn test_new_panics_on_entry_key_mismatch() {
        let key = persistent_data_key();
        // Entry with different contract address
        let mismatched_entry = LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::ContractData(ContractDataEntry {
                ext: ExtensionPoint::V0,
                contract: ScAddress::Contract(ContractId(Hash([99u8; 32]))),
                key: ScVal::Void,
                durability: ContractDataDurability::Persistent,
                val: ScVal::I32(42),
            }),
            ext: LedgerEntryExt::V0,
        };
        let ttl_key = make_ttl_key(&key);
        let ttl_entry = make_ttl_entry(&key);
        LiveBucketListRestore::new(key, mismatched_entry, ttl_key, ttl_entry);
    }

    #[test]
    #[should_panic(expected = "ttl_key does not match derived TTL key")]
    fn test_new_panics_on_ttl_key_hash_mismatch() {
        let key = persistent_data_key();
        let entry = persistent_data_entry();
        // Wrong TTL key (hash from a different key)
        let wrong_ttl_key = LedgerKey::Ttl(LedgerKeyTtl {
            key_hash: Hash([0u8; 32]),
        });
        let ttl_entry = make_ttl_entry(&key);
        LiveBucketListRestore::new(key, entry, wrong_ttl_key, ttl_entry);
    }

    #[test]
    #[should_panic(expected = "ttl_entry must be a TTL entry")]
    fn test_new_panics_on_non_ttl_entry_data() {
        let key = persistent_data_key();
        let entry = persistent_data_entry();
        let ttl_key = make_ttl_key(&key);
        // Use a ContractData entry as ttl_entry — wrong type
        let wrong_ttl_entry = persistent_data_entry();
        LiveBucketListRestore::new(key, entry, ttl_key, wrong_ttl_entry);
    }

    #[test]
    #[should_panic(expected = "ttl_entry does not correspond to ttl_key")]
    fn test_new_panics_on_ttl_entry_key_mismatch() {
        let key = persistent_data_key();
        let entry = persistent_data_entry();
        let ttl_key = make_ttl_key(&key);
        // TTL entry with wrong key_hash
        let wrong_ttl_entry = LedgerEntry {
            last_modified_ledger_seq: 50,
            data: LedgerEntryData::Ttl(TtlEntry {
                key_hash: Hash([0u8; 32]),
                live_until_ledger_seq: 99,
            }),
            ext: LedgerEntryExt::V0,
        };
        LiveBucketListRestore::new(key, entry, ttl_key, wrong_ttl_entry);
    }
}
