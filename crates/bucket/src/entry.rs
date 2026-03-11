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
                ledger_entry_to_key(entry)
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

/// Extract a LedgerKey from a LedgerEntry.
pub fn ledger_entry_to_key(entry: &LedgerEntry) -> Option<LedgerKey> {
    use stellar_xdr::curr::*;

    let key = match &entry.data {
        LedgerEntryData::Account(account) => LedgerKey::Account(LedgerKeyAccount {
            account_id: account.account_id.clone(),
        }),
        LedgerEntryData::Trustline(trustline) => LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: trustline.account_id.clone(),
            asset: trustline.asset.clone(),
        }),
        LedgerEntryData::Offer(offer) => LedgerKey::Offer(LedgerKeyOffer {
            seller_id: offer.seller_id.clone(),
            offer_id: offer.offer_id,
        }),
        LedgerEntryData::Data(data) => LedgerKey::Data(LedgerKeyData {
            account_id: data.account_id.clone(),
            data_name: data.data_name.clone(),
        }),
        LedgerEntryData::ClaimableBalance(cb) => {
            LedgerKey::ClaimableBalance(LedgerKeyClaimableBalance {
                balance_id: cb.balance_id.clone(),
            })
        }
        LedgerEntryData::LiquidityPool(pool) => LedgerKey::LiquidityPool(LedgerKeyLiquidityPool {
            liquidity_pool_id: pool.liquidity_pool_id.clone(),
        }),
        LedgerEntryData::ContractData(contract_data) => {
            LedgerKey::ContractData(LedgerKeyContractData {
                contract: contract_data.contract.clone(),
                key: contract_data.key.clone(),
                durability: contract_data.durability,
            })
        }
        LedgerEntryData::ContractCode(contract_code) => {
            LedgerKey::ContractCode(LedgerKeyContractCode {
                hash: contract_code.hash.clone(),
            })
        }
        LedgerEntryData::ConfigSetting(config) => {
            LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
                config_setting_id: config.discriminant(),
            })
        }
        LedgerEntryData::Ttl(ttl) => LedgerKey::Ttl(LedgerKeyTtl {
            key_hash: ttl.key_hash.clone(),
        }),
    };

    Some(key)
}

/// Compare two LedgerKeys for ordering.
///
/// Keys are sorted first by type discriminant, then by type-specific fields.
/// This ordering is critical for bucket merging to work correctly and must
/// match stellar-core's comparison exactly.
///
/// # Ordering Rules
///
/// 1. **By type discriminant** (as defined in `Stellar-ledger-entries.x`):
///    - Account (0) < Trustline (1) < Offer (2) < Data (3) < ...
///
/// 2. **Within each type**, by type-specific fields in XDR order:
///    - Account: by `account_id`
///    - Trustline: by `account_id`, then `asset`
///    - Offer: by `seller_id`, then `offer_id`
///    - ContractData: by `contract`, then `key`, then `durability`
///    - etc.
///
/// # Determinism
///
/// This function is deterministic and must produce the same ordering as
/// stellar-core's implementation to ensure bucket hashes match.
pub fn compare_keys(a: &LedgerKey, b: &LedgerKey) -> Ordering {
    let a_type = ledger_key_type(a);
    let b_type = ledger_key_type(b);
    match a_type.cmp(&b_type) {
        Ordering::Equal => compare_keys_same_type(a, b),
        other => other,
    }
}

/// Returns the ledger entry type for a given ledger key.
pub fn ledger_key_type(key: &LedgerKey) -> stellar_xdr::curr::LedgerEntryType {
    match key {
        LedgerKey::Account(_) => stellar_xdr::curr::LedgerEntryType::Account,
        LedgerKey::Trustline(_) => stellar_xdr::curr::LedgerEntryType::Trustline,
        LedgerKey::Offer(_) => stellar_xdr::curr::LedgerEntryType::Offer,
        LedgerKey::Data(_) => stellar_xdr::curr::LedgerEntryType::Data,
        LedgerKey::ClaimableBalance(_) => stellar_xdr::curr::LedgerEntryType::ClaimableBalance,
        LedgerKey::LiquidityPool(_) => stellar_xdr::curr::LedgerEntryType::LiquidityPool,
        LedgerKey::ContractData(_) => stellar_xdr::curr::LedgerEntryType::ContractData,
        LedgerKey::ContractCode(_) => stellar_xdr::curr::LedgerEntryType::ContractCode,
        LedgerKey::ConfigSetting(_) => stellar_xdr::curr::LedgerEntryType::ConfigSetting,
        LedgerKey::Ttl(_) => stellar_xdr::curr::LedgerEntryType::Ttl,
    }
}

/// Returns the ledger entry type for a given entry data variant.
///
/// This is the canonical implementation used across the bucket crate. It maps
/// each `LedgerEntryData` variant to its corresponding `LedgerEntryType`
/// discriminant.
pub fn ledger_entry_data_type(
    data: &stellar_xdr::curr::LedgerEntryData,
) -> stellar_xdr::curr::LedgerEntryType {
    use stellar_xdr::curr::LedgerEntryType;
    match data {
        LedgerEntryData::Account(_) => LedgerEntryType::Account,
        LedgerEntryData::Trustline(_) => LedgerEntryType::Trustline,
        LedgerEntryData::Offer(_) => LedgerEntryType::Offer,
        LedgerEntryData::Data(_) => LedgerEntryType::Data,
        LedgerEntryData::ClaimableBalance(_) => LedgerEntryType::ClaimableBalance,
        LedgerEntryData::LiquidityPool(_) => LedgerEntryType::LiquidityPool,
        LedgerEntryData::ContractData(_) => LedgerEntryType::ContractData,
        LedgerEntryData::ContractCode(_) => LedgerEntryType::ContractCode,
        LedgerEntryData::ConfigSetting(_) => LedgerEntryType::ConfigSetting,
        LedgerEntryData::Ttl(_) => LedgerEntryType::Ttl,
    }
}

fn compare_keys_same_type(a: &LedgerKey, b: &LedgerKey) -> Ordering {
    match (a, b) {
        (LedgerKey::Account(a), LedgerKey::Account(b)) => a.account_id.cmp(&b.account_id),
        (LedgerKey::Trustline(a), LedgerKey::Trustline(b)) => a
            .account_id
            .cmp(&b.account_id)
            .then_with(|| a.asset.cmp(&b.asset)),
        (LedgerKey::Offer(a), LedgerKey::Offer(b)) => a
            .seller_id
            .cmp(&b.seller_id)
            .then_with(|| a.offer_id.cmp(&b.offer_id)),
        (LedgerKey::Data(a), LedgerKey::Data(b)) => a
            .account_id
            .cmp(&b.account_id)
            .then_with(|| a.data_name.cmp(&b.data_name)),
        (LedgerKey::ClaimableBalance(a), LedgerKey::ClaimableBalance(b)) => {
            a.balance_id.cmp(&b.balance_id)
        }
        (LedgerKey::LiquidityPool(a), LedgerKey::LiquidityPool(b)) => {
            a.liquidity_pool_id.cmp(&b.liquidity_pool_id)
        }
        (LedgerKey::ContractData(a), LedgerKey::ContractData(b)) => {
            compare_sc_address(&a.contract, &b.contract)
                .then_with(|| compare_sc_val(&a.key, &b.key))
                .then_with(|| a.durability.cmp(&b.durability))
        }
        (LedgerKey::ContractCode(a), LedgerKey::ContractCode(b)) => a.hash.cmp(&b.hash),
        (LedgerKey::ConfigSetting(a), LedgerKey::ConfigSetting(b)) => {
            a.config_setting_id.cmp(&b.config_setting_id)
        }
        (LedgerKey::Ttl(a), LedgerKey::Ttl(b)) => a.key_hash.cmp(&b.key_hash),
        _ => Ordering::Equal,
    }
}

fn compare_sc_address(
    a: &stellar_xdr::curr::ScAddress,
    b: &stellar_xdr::curr::ScAddress,
) -> Ordering {
    // Compare by type discriminant first, then by content
    // Use XDR byte comparison for correctness matching stellar-core xdrpp
    use stellar_xdr::curr::Limits;
    let a_bytes = a.to_xdr(Limits::none()).unwrap_or_default();
    let b_bytes = b.to_xdr(Limits::none()).unwrap_or_default();
    a_bytes.cmp(&b_bytes)
}

/// Compare two ScVal values using the same order as stellar-core.
///
/// This uses explicit type discriminant comparison followed by value comparison,
/// matching the stellar-core xdrpp library's behavior. This is critical for bucket hash
/// determinism across implementations.
fn compare_sc_val(a: &stellar_xdr::curr::ScVal, b: &stellar_xdr::curr::ScVal) -> Ordering {
    use stellar_xdr::curr::{Limits, ScVal::*};

    // Compare by type discriminant first
    let type_a = sc_val_type_discriminant(a);
    let type_b = sc_val_type_discriminant(b);
    if type_a != type_b {
        return type_a.cmp(&type_b);
    }

    // Same type, compare by value
    match (a, b) {
        (Bool(a), Bool(b)) => a.cmp(b),
        (Void, Void) => Ordering::Equal,
        (Error(a), Error(b)) => {
            // Compare by XDR bytes
            let a_bytes = a.to_xdr(Limits::none()).unwrap_or_default();
            let b_bytes = b.to_xdr(Limits::none()).unwrap_or_default();
            a_bytes.cmp(&b_bytes)
        }
        (U32(a), U32(b)) => a.cmp(b),
        (I32(a), I32(b)) => a.cmp(b),
        (U64(a), U64(b)) => a.cmp(b),
        (I64(a), I64(b)) => a.cmp(b),
        (Timepoint(a), Timepoint(b)) => a.cmp(b),
        (Duration(a), Duration(b)) => a.cmp(b),
        (U128(a), U128(b)) => match a.hi.cmp(&b.hi) {
            Ordering::Equal => a.lo.cmp(&b.lo),
            other => other,
        },
        (I128(a), I128(b)) => match a.hi.cmp(&b.hi) {
            Ordering::Equal => a.lo.cmp(&b.lo),
            other => other,
        },
        (U256(a), U256(b)) => {
            for (a_part, b_part) in [
                (a.hi_hi, b.hi_hi),
                (a.hi_lo, b.hi_lo),
                (a.lo_hi, b.lo_hi),
                (a.lo_lo, b.lo_lo),
            ] {
                match a_part.cmp(&b_part) {
                    Ordering::Equal => continue,
                    other => return other,
                }
            }
            Ordering::Equal
        }
        (I256(a), I256(b)) => {
            // I256 has mixed types for hi/lo parts, use XDR bytes
            let a_bytes = a.to_xdr(Limits::none()).unwrap_or_default();
            let b_bytes = b.to_xdr(Limits::none()).unwrap_or_default();
            a_bytes.cmp(&b_bytes)
        }
        (Bytes(a), Bytes(b)) => a.as_slice().cmp(b.as_slice()),
        (String(a), String(b)) => a.as_slice().cmp(b.as_slice()),
        (Symbol(a), Symbol(b)) => a.as_slice().cmp(b.as_slice()),
        (Vec(a_opt), Vec(b_opt)) => match (a_opt, b_opt) {
            (Some(a), Some(b)) => {
                for (a_elem, b_elem) in a.iter().zip(b.iter()) {
                    match compare_sc_val(a_elem, b_elem) {
                        Ordering::Equal => continue,
                        other => return other,
                    }
                }
                a.len().cmp(&b.len())
            }
            (Some(_), None) => Ordering::Greater,
            (None, Some(_)) => Ordering::Less,
            (None, None) => Ordering::Equal,
        },
        (Map(a_opt), Map(b_opt)) => match (a_opt, b_opt) {
            (Some(a), Some(b)) => {
                for (a_entry, b_entry) in a.iter().zip(b.iter()) {
                    match compare_sc_val(&a_entry.key, &b_entry.key) {
                        Ordering::Equal => match compare_sc_val(&a_entry.val, &b_entry.val) {
                            Ordering::Equal => continue,
                            other => return other,
                        },
                        other => return other,
                    }
                }
                a.len().cmp(&b.len())
            }
            (Some(_), None) => Ordering::Greater,
            (None, Some(_)) => Ordering::Less,
            (None, None) => Ordering::Equal,
        },
        (Address(a), Address(b)) => compare_sc_address(a, b),
        (LedgerKeyContractInstance, LedgerKeyContractInstance) => Ordering::Equal,
        (LedgerKeyNonce(a), LedgerKeyNonce(b)) => a.nonce.cmp(&b.nonce),
        (ContractInstance(a), ContractInstance(b)) => {
            // Compare by XDR bytes as fallback
            let a_bytes = a.to_xdr(Limits::none()).unwrap_or_default();
            let b_bytes = b.to_xdr(Limits::none()).unwrap_or_default();
            a_bytes.cmp(&b_bytes)
        }
        // For any remaining cases, use XDR byte comparison
        _ => {
            let a_bytes = a.to_xdr(Limits::none()).unwrap_or_default();
            let b_bytes = b.to_xdr(Limits::none()).unwrap_or_default();
            a_bytes.cmp(&b_bytes)
        }
    }
}

/// Get the XDR type discriminant for an ScVal.
///
/// These values must match the XDR ScValType enum discriminants exactly
/// for deterministic comparison across implementations.
fn sc_val_type_discriminant(v: &stellar_xdr::curr::ScVal) -> i32 {
    use stellar_xdr::curr::ScVal::*;
    match v {
        Bool(_) => 0,
        Void => 1,
        Error(_) => 2,
        U32(_) => 3,
        I32(_) => 4,
        U64(_) => 5,
        I64(_) => 6,
        Timepoint(_) => 7,
        Duration(_) => 8,
        U128(_) => 9,
        I128(_) => 10,
        U256(_) => 11,
        I256(_) => 12,
        Bytes(_) => 13,
        String(_) => 14,
        Symbol(_) => 15,
        Vec(_) => 16,
        Map(_) => 17,
        Address(_) => 18,
        ContractInstance(_) => 19,
        LedgerKeyContractInstance => 20,
        LedgerKeyNonce(_) => 21,
    }
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
