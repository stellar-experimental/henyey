//! BucketEntry implementation for bucket storage.
//!
//! This module defines the [`BucketEntry`] type and associated utilities for
//! working with entries stored in Stellar buckets. Bucket entries wrap ledger
//! entries with additional metadata for merge semantics.
//!
//! # Entry Types
//!
//! | Type       | Description                                      | Merge Behavior              |
//! |------------|--------------------------------------------------|-----------------------------|
//! | `Live`     | An active ledger entry                           | Newer shadows older         |
//! | `Dead`     | A tombstone marking deletion                     | Shadows any older entry     |
//! | `Init`     | Entry created in this merge window (CAP-0020)    | Special annihilation rules  |
//! | `Metadata` | Bucket metadata (protocol version, type)         | Merged by taking max version|
//!
//! # Key Ordering
//!
//! Entries in a bucket must be sorted by key for correct merge behavior.
//! The ordering is determined by:
//!
//! 1. Entry type discriminant (Account < Trustline < Offer < ...)
//! 2. Type-specific fields in lexicographic order
//!
//! See [`compare_keys`] for the detailed ordering rules.
//!
//! # Eviction Helpers
//!
//! This module also provides helper functions for Soroban state archival:
//!
//! - [`is_soroban_entry`]: Check if an entry is ContractData or ContractCode
//! - [`is_temporary_entry`]: Check if a ContractData entry is temporary
//! - [`is_persistent_entry`]: Check if an entry is persistent (archived on eviction)
//! - [`get_ttl_key`]: Get the TTL key for a Soroban entry
//! - [`is_ttl_expired`]: Check if a TTL entry has expired

use std::cmp::Ordering;

use sha2::{Digest, Sha256};
use stellar_xdr::curr::{
    BucketEntryType, ContractDataDurability, Hash, LedgerEntry, LedgerEntryData, LedgerKey,
    LedgerKeyTtl, Limits, ReadXdr, WriteXdr,
};

use crate::{BucketError, Result};

/// Re-export XDR BucketEntry as the canonical type.
///
/// Bucket entries wrap ledger entries with additional type information
/// that controls merge semantics. Entries are sorted by key for efficient
/// merging and binary search lookup.
///
/// # Variants
///
/// | Variant      | Description                                      | Merge Behavior              |
/// |--------------|--------------------------------------------------|-----------------------------|
/// | `Liveentry`  | An active ledger entry                           | Newer shadows older         |
/// | `Deadentry`  | A tombstone marking deletion                     | Shadows any older entry     |
/// | `Initentry`  | Entry created in this merge window (CAP-0020)    | Special annihilation rules  |
/// | `Metaentry`  | Bucket metadata (protocol version, type)         | Merged by taking max version|
pub type BucketEntry = stellar_xdr::curr::BucketEntry;

/// Extension trait adding bucket-specific convenience methods to the XDR `BucketEntry`.
pub trait BucketEntryExt {
    /// Parse a BucketEntry from XDR bytes.
    fn from_xdr_bytes(bytes: &[u8]) -> Result<BucketEntry>;

    /// Serialize to XDR bytes.
    fn to_xdr_bytes(&self) -> Result<Vec<u8>>;

    /// Get the LedgerKey for this entry.
    ///
    /// Returns None for metadata entries since they don't have a key.
    fn key(&self) -> Option<LedgerKey>;

    /// Check if this entry is a metadata entry.
    fn is_metadata(&self) -> bool;

    /// Check if this is a dead entry (tombstone).
    fn is_dead(&self) -> bool;

    /// Check if this is a live entry.
    fn is_live(&self) -> bool;

    /// Check if this is an init entry.
    fn is_init(&self) -> bool;

    /// Get the ledger entry if this is a live or init entry.
    fn as_ledger_entry(&self) -> Option<&LedgerEntry>;

    /// Get the bucket entry type.
    fn entry_type(&self) -> BucketEntryType;
}

impl BucketEntryExt for BucketEntry {
    fn from_xdr_bytes(bytes: &[u8]) -> Result<BucketEntry> {
        BucketEntry::from_xdr(bytes, Limits::none())
            .map_err(|e| BucketError::Serialization(format!("Failed to parse XDR: {}", e)))
    }

    fn to_xdr_bytes(&self) -> Result<Vec<u8>> {
        self.to_xdr(Limits::none())
            .map_err(|e| BucketError::Serialization(format!("Failed to serialize XDR: {}", e)))
    }

    fn key(&self) -> Option<LedgerKey> {
        match self {
            BucketEntry::Liveentry(entry) | BucketEntry::Initentry(entry) => {
                Some(henyey_common::entry_to_key(entry))
            }
            BucketEntry::Deadentry(key) => Some(key.clone()),
            BucketEntry::Metaentry(_) => None,
        }
    }

    fn is_metadata(&self) -> bool {
        matches!(self, BucketEntry::Metaentry(_))
    }

    fn is_dead(&self) -> bool {
        matches!(self, BucketEntry::Deadentry(_))
    }

    fn is_live(&self) -> bool {
        matches!(self, BucketEntry::Liveentry(_))
    }

    fn is_init(&self) -> bool {
        matches!(self, BucketEntry::Initentry(_))
    }

    fn as_ledger_entry(&self) -> Option<&LedgerEntry> {
        match self {
            BucketEntry::Liveentry(entry) | BucketEntry::Initentry(entry) => Some(entry),
            _ => None,
        }
    }

    fn entry_type(&self) -> BucketEntryType {
        self.discriminant()
    }
}

/// Compare two LedgerKeys for ordering.
///
/// Delegates to `LedgerKey`'s derived `Ord`, which matches stellar-core's
/// xdrpp comparison: discriminant first, then fields in XDR declaration order.
/// The rs-stellar-xdr crate declares enum variants in discriminant order and
/// struct fields in XDR order, so Rust's `derive(Ord)` produces identical
/// ordering to xdrpp's recursive field-by-field `operator<`.
pub fn compare_keys(a: &LedgerKey, b: &LedgerKey) -> Ordering {
    a.cmp(b)
}

/// Returns the ledger entry type for a given ledger key.
///
/// Delegates to the XDR crate's inherent `LedgerKey::discriminant()` method.
pub fn ledger_key_type(key: &LedgerKey) -> stellar_xdr::curr::LedgerEntryType {
    key.discriminant()
}

/// Returns the ledger entry type for a given entry data variant.
///
/// Delegates to the XDR crate's inherent `LedgerEntryData::discriminant()` method.
pub fn ledger_entry_data_type(
    data: &stellar_xdr::curr::LedgerEntryData,
) -> stellar_xdr::curr::LedgerEntryType {
    data.discriminant()
}

/// Compare two BucketEntry values by key.
///
/// Metadata entries are always sorted first.
/// Returns None if either entry is metadata and the other is not.
pub fn compare_entries(a: &BucketEntry, b: &BucketEntry) -> Ordering {
    match (a.key(), b.key()) {
        (Some(key_a), Some(key_b)) => compare_keys(&key_a, &key_b),
        (None, Some(_)) => Ordering::Less, // Metadata comes first
        (Some(_), None) => Ordering::Greater,
        (None, None) => Ordering::Equal, // Both metadata
    }
}

// ============================================================================
// Eviction helper functions (Soroban State Archival)
// ============================================================================
//
// These functions support the incremental eviction scan for Soroban entries.
// Soroban uses a time-to-live (TTL) mechanism where entries expire and must
// be either deleted (temporary) or archived (persistent).

/// Check if a ledger entry is a Soroban entry (ContractData or ContractCode).
///
/// Soroban entries are the only entry types subject to eviction and state
/// archival. They have associated TTL entries that track when they expire.
///
/// # Returns
///
/// `true` if the entry is `ContractData` or `ContractCode`, `false` otherwise.
pub fn is_soroban_entry(entry: &LedgerEntry) -> bool {
    matches!(
        entry.data,
        LedgerEntryData::ContractData(_) | LedgerEntryData::ContractCode(_)
    )
}

/// Check if a ledger key is for a Soroban entry.
pub fn is_soroban_key(key: &LedgerKey) -> bool {
    matches!(key, LedgerKey::ContractData(_) | LedgerKey::ContractCode(_))
}

/// Check if a ledger entry is a temporary Soroban entry.
///
/// Temporary entries (ContractData with `Temporary` durability) are deleted
/// immediately on eviction and are NOT archived to the hot archive bucket list.
///
/// # Use Case
///
/// Temporary data is used for values that don't need to persist across
/// archival, such as caches or ephemeral state.
pub fn is_temporary_entry(entry: &LedgerEntry) -> bool {
    if let LedgerEntryData::ContractData(data) = &entry.data {
        data.durability == ContractDataDurability::Temporary
    } else {
        false
    }
}

/// Check if a ledger entry is a persistent Soroban entry.
///
/// Persistent entries (ContractCode or ContractData with `Persistent` durability)
/// are archived to the hot archive bucket list on eviction. They can later be
/// restored to the live bucket list by paying for additional TTL.
///
/// # Persistent Entry Types
///
/// - All `ContractCode` entries (WASM code is always persistent)
/// - `ContractData` entries with `Persistent` durability
pub fn is_persistent_entry(entry: &LedgerEntry) -> bool {
    match &entry.data {
        LedgerEntryData::ContractCode(_) => true,
        LedgerEntryData::ContractData(data) => {
            data.durability == ContractDataDurability::Persistent
        }
        _ => false,
    }
}

/// Check if a ledger key is for a persistent Soroban entry.
///
/// This is the key-based counterpart to [`is_persistent_entry`]. It checks
/// whether the key refers to a `ContractCode` entry (always persistent) or a
/// `ContractData` entry with `Persistent` durability.
///
/// # Returns
///
/// `true` if the key is for a persistent Soroban entry, `false` otherwise.
/// Returns `false` for `Temporary` contract data and all non-Soroban keys.
pub fn is_persistent_key(key: &LedgerKey) -> bool {
    match key {
        LedgerKey::ContractCode(_) => true,
        LedgerKey::ContractData(data) => data.durability == ContractDataDurability::Persistent,
        _ => false,
    }
}

/// Get the TTL key for a Soroban entry.
///
/// Each Soroban entry has an associated TTL entry that tracks its expiration.
/// The TTL key is derived by hashing the original key with SHA-256.
///
/// # How TTL Works
///
/// TTL entries contain a `live_until_ledger_seq` field. When the current
/// ledger exceeds this value, the entry is considered expired and will be
/// evicted during the next eviction scan that encounters it.
///
/// # Returns
///
/// - `Some(LedgerKey::Ttl)` with the key hash for Soroban entries
/// - `None` for non-Soroban entries (they don't have TTL)
pub fn get_ttl_key(key: &LedgerKey) -> Option<LedgerKey> {
    if !is_soroban_key(key) {
        return None;
    }

    // Serialize the key to XDR and hash it
    let key_bytes = key.to_xdr(Limits::none()).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(&key_bytes);
    let hash_result = hasher.finalize();

    let mut hash_bytes = [0u8; 32];
    hash_bytes.copy_from_slice(&hash_result);

    Some(LedgerKey::Ttl(LedgerKeyTtl {
        key_hash: Hash(hash_bytes),
    }))
}

/// Check if a TTL entry is expired at the given ledger sequence.
///
/// An entry is expired when its `live_until_ledger_seq` is less than the current ledger.
/// Returns None if the entry is not a TTL entry.
pub fn is_ttl_expired(ttl_entry: &LedgerEntry, current_ledger: u32) -> Option<bool> {
    get_ttl_live_until(ttl_entry).map(|live_until| live_until < current_ledger)
}

/// Get the live_until_ledger_seq from a TTL entry.
///
/// Returns None if the entry is not a TTL entry.
pub fn get_ttl_live_until(ttl_entry: &LedgerEntry) -> Option<u32> {
    if let LedgerEntryData::Ttl(ttl) = &ttl_entry.data {
        Some(ttl.live_until_ledger_seq)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::*;
    // Disambiguate: super::BucketEntry (type alias) vs stellar_xdr::curr::BucketEntry
    use super::BucketEntry;

    fn make_account_id(bytes: [u8; 32]) -> AccountId {
        AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(bytes)))
    }

    fn make_account_entry(bytes: [u8; 32]) -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: make_account_id(bytes),
                balance: 100,
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: Vec::new().try_into().unwrap(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    #[test]
    fn test_bucket_entry_key() {
        let entry = make_account_entry([1u8; 32]);
        let bucket_entry = BucketEntry::Liveentry(entry.clone());

        let key = bucket_entry.key().unwrap();
        if let LedgerKey::Account(account_key) = key {
            assert_eq!(account_key.account_id, make_account_id([1u8; 32]));
        } else {
            panic!("Expected Account key");
        }
    }

    #[test]
    fn test_bucket_entry_dead() {
        let key = LedgerKey::Account(LedgerKeyAccount {
            account_id: make_account_id([2u8; 32]),
        });
        let bucket_entry = BucketEntry::Deadentry(key.clone());

        assert!(bucket_entry.is_dead());
        assert!(!bucket_entry.is_live());
        assert_eq!(bucket_entry.key().unwrap(), key);
    }

    #[test]
    fn test_compare_keys_same_type() {
        let key1 = LedgerKey::Account(LedgerKeyAccount {
            account_id: make_account_id([1u8; 32]),
        });
        let key2 = LedgerKey::Account(LedgerKeyAccount {
            account_id: make_account_id([2u8; 32]),
        });

        assert_eq!(compare_keys(&key1, &key2), Ordering::Less);
        assert_eq!(compare_keys(&key2, &key1), Ordering::Greater);
        assert_eq!(compare_keys(&key1, &key1), Ordering::Equal);
    }

    #[test]
    fn test_compare_entries() {
        let entry1 = BucketEntry::Liveentry(make_account_entry([1u8; 32]));
        let entry2 = BucketEntry::Liveentry(make_account_entry([2u8; 32]));

        assert_eq!(compare_entries(&entry1, &entry2), Ordering::Less);
    }

    #[test]
    fn test_entry_type() {
        let live = BucketEntry::Liveentry(make_account_entry([1u8; 32]));
        let dead = BucketEntry::Deadentry(LedgerKey::Account(LedgerKeyAccount {
            account_id: make_account_id([1u8; 32]),
        }));
        let init = BucketEntry::Initentry(make_account_entry([1u8; 32]));

        assert_eq!(live.entry_type(), BucketEntryType::Liveentry);
        assert_eq!(dead.entry_type(), BucketEntryType::Deadentry);
        assert_eq!(init.entry_type(), BucketEntryType::Initentry);
    }

    #[test]
    fn test_ledger_entry_type_discriminants() {
        // These values MUST match stellar-core's XDR definition for correct sorting
        // See Stellar-ledger-entries.x in stellar/stellar-xdr
        assert_eq!(LedgerEntryType::Account as i32, 0);
        assert_eq!(LedgerEntryType::Trustline as i32, 1);
        assert_eq!(LedgerEntryType::Offer as i32, 2);
        assert_eq!(LedgerEntryType::Data as i32, 3);
        assert_eq!(LedgerEntryType::ClaimableBalance as i32, 4);
        assert_eq!(LedgerEntryType::LiquidityPool as i32, 5);
        assert_eq!(LedgerEntryType::ContractData as i32, 6);
        assert_eq!(LedgerEntryType::ContractCode as i32, 7);
        assert_eq!(LedgerEntryType::ConfigSetting as i32, 8);
        assert_eq!(LedgerEntryType::Ttl as i32, 9);
    }

    #[test]
    fn test_compare_keys_different_types() {
        // Ensure keys of different types are compared by type discriminant first
        let account_key = LedgerKey::Account(LedgerKeyAccount {
            account_id: make_account_id([255u8; 32]), // Highest possible account
        });
        let trustline_key = LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: make_account_id([0u8; 32]), // Lowest possible account
            asset: TrustLineAsset::Native,
        });

        // Account (type 0) should sort before Trustline (type 1), regardless of account bytes
        assert_eq!(compare_keys(&account_key, &trustline_key), Ordering::Less);
        assert_eq!(
            compare_keys(&trustline_key, &account_key),
            Ordering::Greater
        );
    }
}
