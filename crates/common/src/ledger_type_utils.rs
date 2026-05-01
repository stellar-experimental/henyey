//! Soroban ledger entry classification helpers.
//!
//! Partial Rust analogue of stellar-core's `LedgerTypeUtils.h` (lines 60-86).
//! These are pure XDR-type predicates with no state, config, or protocol
//! dependence. Each concept has both an entry-based and key-based variant.
//!
//! The `LedgerEntry` variants dispatch on `entry.data` because in Rust the
//! XDR discriminant lives inside the `.data` field (whereas stellar-core's
//! C++ templates operate on the discriminant directly via `.type()`).

use stellar_xdr::curr::*;

/// Returns `true` if the entry is a Soroban entry (`ContractData` or `ContractCode`).
///
/// Mirrors stellar-core `LedgerTypeUtils.h:isSorobanEntry`.
pub fn is_soroban_entry(entry: &LedgerEntry) -> bool {
    matches!(
        entry.data,
        LedgerEntryData::ContractData(_) | LedgerEntryData::ContractCode(_)
    )
}

/// Returns `true` if the key refers to a Soroban entry (`ContractData` or `ContractCode`).
///
/// Key-based variant of [`is_soroban_entry`].
pub fn is_soroban_key(key: &LedgerKey) -> bool {
    matches!(key, LedgerKey::ContractData(_) | LedgerKey::ContractCode(_))
}

/// Returns `true` if the entry is a temporary Soroban entry
/// (`ContractData` with `Temporary` durability).
///
/// Mirrors stellar-core `LedgerTypeUtils.h:isTemporaryEntry`.
pub fn is_temporary_entry(entry: &LedgerEntry) -> bool {
    matches!(
        &entry.data,
        LedgerEntryData::ContractData(data) if data.durability == ContractDataDurability::Temporary
    )
}

/// Returns `true` if the key refers to a temporary Soroban entry
/// (`ContractData` with `Temporary` durability).
///
/// Key-based variant of [`is_temporary_entry`].
pub fn is_temporary_key(key: &LedgerKey) -> bool {
    matches!(
        key,
        LedgerKey::ContractData(data) if data.durability == ContractDataDurability::Temporary
    )
}

/// Returns `true` if the entry is a persistent Soroban entry
/// (`ContractCode`, or `ContractData` with `Persistent` durability).
///
/// Mirrors stellar-core `LedgerTypeUtils.h:isPersistentEntry`.
pub fn is_persistent_entry(entry: &LedgerEntry) -> bool {
    match &entry.data {
        LedgerEntryData::ContractCode(_) => true,
        LedgerEntryData::ContractData(data) => {
            data.durability == ContractDataDurability::Persistent
        }
        _ => false,
    }
}

/// Returns `true` if the key refers to a persistent Soroban entry
/// (`ContractCode`, or `ContractData` with `Persistent` durability).
///
/// Key-based variant of [`is_persistent_entry`].
pub fn is_persistent_key(key: &LedgerKey) -> bool {
    match key {
        LedgerKey::ContractCode(_) => true,
        LedgerKey::ContractData(data) => data.durability == ContractDataDurability::Persistent,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Helpers to construct minimal XDR values ──────────────────────

    fn contract_data_key(durability: ContractDataDurability) -> LedgerKey {
        LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(Hash([0; 32]))),
            key: ScVal::Void,
            durability,
        })
    }

    fn contract_code_key() -> LedgerKey {
        LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: Hash([0; 32]),
        })
    }

    fn account_key() -> LedgerKey {
        LedgerKey::Account(LedgerKeyAccount {
            account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0; 32]))),
        })
    }

    fn ttl_key() -> LedgerKey {
        LedgerKey::Ttl(LedgerKeyTtl {
            key_hash: Hash([0; 32]),
        })
    }

    fn make_entry(data: LedgerEntryData) -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 0,
            data,
            ext: LedgerEntryExt::V0,
        }
    }

    fn contract_data_entry(durability: ContractDataDurability) -> LedgerEntry {
        make_entry(LedgerEntryData::ContractData(ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: ScAddress::Contract(ContractId(Hash([0; 32]))),
            key: ScVal::Void,
            durability,
            val: ScVal::Void,
        }))
    }

    fn contract_code_entry() -> LedgerEntry {
        make_entry(LedgerEntryData::ContractCode(ContractCodeEntry {
            ext: ContractCodeEntryExt::V0,
            hash: Hash([0; 32]),
            code: Default::default(),
        }))
    }

    fn account_entry() -> LedgerEntry {
        make_entry(LedgerEntryData::Account(AccountEntry {
            account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0; 32]))),
            balance: 0,
            seq_num: SequenceNumber(0),
            num_sub_entries: 0,
            inflation_dest: None,
            flags: 0,
            home_domain: Default::default(),
            thresholds: Thresholds([0; 4]),
            signers: Default::default(),
            ext: AccountEntryExt::V0,
        }))
    }

    // ── is_soroban_key ──────────────────────────────────────────────

    #[test]
    fn test_is_soroban_key_contract_data() {
        assert!(is_soroban_key(&contract_data_key(
            ContractDataDurability::Persistent
        )));
        assert!(is_soroban_key(&contract_data_key(
            ContractDataDurability::Temporary
        )));
    }

    #[test]
    fn test_is_soroban_key_contract_code() {
        assert!(is_soroban_key(&contract_code_key()));
    }

    #[test]
    fn test_is_soroban_key_rejects_account() {
        assert!(!is_soroban_key(&account_key()));
    }

    #[test]
    fn test_is_soroban_key_rejects_ttl() {
        assert!(!is_soroban_key(&ttl_key()));
    }

    // ── is_soroban_entry ────────────────────────────────────────────

    #[test]
    fn test_is_soroban_entry_contract_data() {
        assert!(is_soroban_entry(&contract_data_entry(
            ContractDataDurability::Persistent
        )));
        assert!(is_soroban_entry(&contract_data_entry(
            ContractDataDurability::Temporary
        )));
    }

    #[test]
    fn test_is_soroban_entry_contract_code() {
        assert!(is_soroban_entry(&contract_code_entry()));
    }

    #[test]
    fn test_is_soroban_entry_rejects_account() {
        assert!(!is_soroban_entry(&account_entry()));
    }

    // ── is_temporary_key ────────────────────────────────────────────

    #[test]
    fn test_is_temporary_key() {
        assert!(is_temporary_key(&contract_data_key(
            ContractDataDurability::Temporary
        )));
        assert!(!is_temporary_key(&contract_data_key(
            ContractDataDurability::Persistent
        )));
        assert!(!is_temporary_key(&contract_code_key()));
        assert!(!is_temporary_key(&account_key()));
    }

    // ── is_temporary_entry ──────────────────────────────────────────

    #[test]
    fn test_is_temporary_entry() {
        assert!(is_temporary_entry(&contract_data_entry(
            ContractDataDurability::Temporary
        )));
        assert!(!is_temporary_entry(&contract_data_entry(
            ContractDataDurability::Persistent
        )));
        assert!(!is_temporary_entry(&contract_code_entry()));
        assert!(!is_temporary_entry(&account_entry()));
    }

    // ── is_persistent_key ───────────────────────────────────────────

    #[test]
    fn test_is_persistent_key() {
        assert!(is_persistent_key(&contract_data_key(
            ContractDataDurability::Persistent
        )));
        assert!(is_persistent_key(&contract_code_key()));
        assert!(!is_persistent_key(&contract_data_key(
            ContractDataDurability::Temporary
        )));
        assert!(!is_persistent_key(&account_key()));
        assert!(!is_persistent_key(&ttl_key()));
    }

    // ── is_persistent_entry ─────────────────────────────────────────

    #[test]
    fn test_is_persistent_entry() {
        assert!(is_persistent_entry(&contract_data_entry(
            ContractDataDurability::Persistent
        )));
        assert!(is_persistent_entry(&contract_code_entry()));
        assert!(!is_persistent_entry(&contract_data_entry(
            ContractDataDurability::Temporary
        )));
        assert!(!is_persistent_entry(&account_entry()));
    }
}
