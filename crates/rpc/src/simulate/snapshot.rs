//! Bucket list snapshot adapter for Soroban simulation.

use std::rc::Rc;

use henyey_bucket::SearchableBucketListSnapshot;
use soroban_env_host_p25 as soroban_host;
use stellar_xdr::curr::{
    AccountEntryExt, AccountEntryExtensionV1, AccountEntryExtensionV1Ext, AccountEntryExtensionV2,
    AccountEntryExtensionV2Ext, AccountEntryExtensionV3, ExtensionPoint, LedgerEntry,
    LedgerEntryData, LedgerKey, Liabilities, SponsorshipDescriptor, TimePoint,
};

use soroban_host::storage::{EntryWithLiveUntil, SnapshotSource};
use soroban_host::HostError;

use crate::util::ttl_key_for_ledger_key;

/// Adapter that provides snapshot access to the bucket list for Soroban simulation.
///
/// Implements `SnapshotSource` from `soroban-env-host-p25` by wrapping a
/// `SearchableBucketListSnapshot`. This is used for `simulateTransaction`
/// where we need read-only access to the current ledger state.
///
/// Account entries are normalized to V3 extensions on load, matching the
/// upstream `SimulationSnapshotSource` from `soroban-simulation`. This ensures
/// the host sees the same entry sizes as validators (which always store accounts
/// with full V3 extensions), producing correct `disk_read_bytes` and
/// `write_bytes` resource estimates.
pub(crate) struct BucketListSnapshotSource {
    snapshot: SearchableBucketListSnapshot,
    current_ledger: u32,
}

// Safety: BucketListSnapshotSource contains only owned, immutable data.
// SearchableBucketListSnapshot holds cloned data from the bucket list.
// It is safe to send across threads.
unsafe impl Send for BucketListSnapshotSource {}

impl BucketListSnapshotSource {
    pub(crate) fn new(snapshot: SearchableBucketListSnapshot, current_ledger: u32) -> Self {
        Self {
            snapshot,
            current_ledger,
        }
    }

    /// Look up a ledger entry without TTL filtering.
    ///
    /// Returns the entry and its TTL regardless of whether the entry is expired.
    /// Used for ExtendTTL and Restore simulation where we need access to
    /// archived/expired entries.
    pub(crate) fn get_unfiltered(
        &self,
        key: &LedgerKey,
    ) -> henyey_bucket::Result<Option<(LedgerEntry, Option<u32>)>> {
        let live_until = get_entry_ttl(&self.snapshot, key)?;
        let Some(mut entry) = self.snapshot.load_result(key)? else {
            return Ok(None);
        };
        normalize_entry(&mut entry);
        Ok(Some((entry, live_until)))
    }
}

impl SnapshotSource for BucketListSnapshotSource {
    fn get(
        &self,
        key: &Rc<soroban_host::xdr::LedgerKey>,
    ) -> Result<Option<EntryWithLiveUntil>, HostError> {
        // Convert P25 LedgerKey to workspace LedgerKey for bucket list lookup.
        // Conversion failure is an internal error (structurally valid P25 keys
        // should always convert), not "key not found".
        let ws_key: LedgerKey = super::convert::p25_to_ws(key.as_ref()).map_err(|_| {
            HostError::from(soroban_host::Error::from_type_and_code(
                soroban_host::xdr::ScErrorType::Value,
                soroban_host::xdr::ScErrorCode::InternalError,
            ))
        })?;

        let make_err = || {
            HostError::from(soroban_host::Error::from_type_and_code(
                soroban_host::xdr::ScErrorType::Storage,
                soroban_host::xdr::ScErrorCode::InternalError,
            ))
        };

        // For contract data/code entries, we need to check TTL
        let live_until = get_entry_ttl(&self.snapshot, &ws_key).map_err(|_| make_err())?;

        // Check TTL expiration for contract entries
        if henyey_common::is_soroban_key(&ws_key) {
            match live_until {
                Some(ttl) if ttl >= self.current_ledger => {} // live, proceed
                _ => return Ok(None),                         // expired or no TTL
            }
        }

        // Look up the entry in the bucket list
        match self.snapshot.load_result(&ws_key) {
            Ok(Some(mut entry)) => {
                normalize_entry(&mut entry);
                // Convert workspace LedgerEntry to P25 LedgerEntry
                let p25_entry: soroban_host::xdr::LedgerEntry = super::convert::ws_to_p25(&entry)
                    .map_err(|_| {
                    HostError::from(soroban_host::Error::from_type_and_code(
                        soroban_host::xdr::ScErrorType::Value,
                        soroban_host::xdr::ScErrorCode::InternalError,
                    ))
                })?;
                Ok(Some((Rc::new(p25_entry), live_until)))
            }
            Ok(None) => Ok(None),
            Err(_) => Err(make_err()),
        }
    }
}

/// Get the TTL (live_until_ledger) for a ledger entry from the bucket list.
///
/// Returns `Ok(None)` if the entry has no TTL key or the TTL entry is not found.
/// Returns `Err` on I/O or deserialization errors from disk-backed buckets.
fn get_entry_ttl(
    snapshot: &SearchableBucketListSnapshot,
    key: &LedgerKey,
) -> henyey_bucket::Result<Option<u32>> {
    let ttl_key = match ttl_key_for_ledger_key(key) {
        Some(k) => k,
        None => return Ok(None),
    };

    match snapshot.load_result(&ttl_key)? {
        Some(entry) => match entry.data {
            stellar_xdr::curr::LedgerEntryData::Ttl(ttl_data) => {
                Ok(Some(ttl_data.live_until_ledger_seq))
            }
            _ => Ok(None),
        },
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Account entry normalization (V0 → V3)
// ---------------------------------------------------------------------------

/// Normalize a ledger entry for simulation.
///
/// stellar-core always stores account entries with full V3 extensions. When the
/// bucket list stores entries that were never touched by operations requiring
/// extensions (e.g. freshly friendbot-funded accounts), they may still have V0
/// extensions. The upstream `SimulationSnapshotSource` in `soroban-simulation`
/// normalizes all account entries to V3 before the host sees them. We do the
/// same here so that the host computes the same entry sizes as validators,
/// producing correct resource estimates.
fn normalize_entry(entry: &mut LedgerEntry) {
    if let LedgerEntryData::Account(ref mut acc) = entry.data {
        update_account_entry(acc);
    }
}

/// Upgrade an `AccountEntry`'s extension chain to V3.
///
/// Mirrors `update_account_entry` from `soroban-simulation/src/snapshot_source.rs`.
fn update_account_entry(account_entry: &mut stellar_xdr::curr::AccountEntry) {
    match &mut account_entry.ext {
        AccountEntryExt::V0 => {
            let mut ext = AccountEntryExtensionV1 {
                liabilities: Liabilities {
                    buying: 0,
                    selling: 0,
                },
                ext: AccountEntryExtensionV1Ext::V0,
            };
            fill_account_ext_v2(&mut ext, account_entry.signers.len());
            account_entry.ext = AccountEntryExt::V1(ext);
        }
        AccountEntryExt::V1(ext) => {
            fill_account_ext_v2(ext, account_entry.signers.len());
        }
    }
}

fn fill_account_ext_v2(account_ext_v1: &mut AccountEntryExtensionV1, signers_count: usize) {
    match &mut account_ext_v1.ext {
        AccountEntryExtensionV1Ext::V0 => {
            let mut ext = AccountEntryExtensionV2 {
                num_sponsored: 0,
                num_sponsoring: 0,
                signer_sponsoring_i_ds: vec![SponsorshipDescriptor(None); signers_count]
                    .try_into()
                    .unwrap_or_default(),
                ext: AccountEntryExtensionV2Ext::V0,
            };
            fill_account_ext_v3(&mut ext);
            account_ext_v1.ext = AccountEntryExtensionV1Ext::V2(ext);
        }
        AccountEntryExtensionV1Ext::V2(ext) => fill_account_ext_v3(ext),
    }
}

fn fill_account_ext_v3(account_ext_v2: &mut AccountEntryExtensionV2) {
    match account_ext_v2.ext {
        AccountEntryExtensionV2Ext::V0 => {
            account_ext_v2.ext = AccountEntryExtensionV2Ext::V3(AccountEntryExtensionV3 {
                ext: ExtensionPoint::V0,
                seq_ledger: 0,
                seq_time: TimePoint(0),
            });
        }
        AccountEntryExtensionV2Ext::V3(_) => (),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{
        AccountEntry, AccountEntryExt, AccountEntryExtensionV1, AccountEntryExtensionV1Ext,
        AccountEntryExtensionV2, AccountEntryExtensionV2Ext, AccountEntryExtensionV3, AccountId,
        ContractDataDurability, ContractDataEntry, ContractId, ExtensionPoint, Hash, Int128Parts,
        LedgerEntry, LedgerEntryData, LedgerEntryExt, Limits, PublicKey, ScAddress, ScVal,
        SequenceNumber, Signer, SignerKey, Thresholds, Uint256, WriteXdr,
    };

    /// Build a minimal account entry with the given extension variant.
    fn make_account_entry(ext: AccountEntryExt) -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32]))),
                balance: 100_000_000_000,
                seq_num: SequenceNumber(42),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: Default::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: Default::default(),
                ext,
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    fn make_account_entry_with_signers(ext: AccountEntryExt, signers: Vec<Signer>) -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32]))),
                balance: 100_000_000_000,
                seq_num: SequenceNumber(42),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: Default::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: signers.try_into().unwrap(),
                ext,
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    // -----------------------------------------------------------------------
    // Category A: Snapshot normalization regression tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_normalize_account_v0_to_v3() {
        let mut entry = make_account_entry(AccountEntryExt::V0);
        normalize_entry(&mut entry);

        let LedgerEntryData::Account(acc) = &entry.data else {
            panic!("expected account");
        };
        let AccountEntryExt::V1(v1) = &acc.ext else {
            panic!("expected V1, got {:?}", acc.ext);
        };
        assert_eq!(v1.liabilities.buying, 0);
        assert_eq!(v1.liabilities.selling, 0);

        let AccountEntryExtensionV1Ext::V2(v2) = &v1.ext else {
            panic!("expected V2");
        };
        assert_eq!(v2.num_sponsored, 0);
        assert_eq!(v2.num_sponsoring, 0);
        assert!(v2.signer_sponsoring_i_ds.is_empty());

        let AccountEntryExtensionV2Ext::V3(v3) = &v2.ext else {
            panic!("expected V3");
        };
        assert_eq!(v3.seq_ledger, 0);
        assert_eq!(v3.seq_time, TimePoint(0));
    }

    #[test]
    fn test_normalize_account_v1_to_v3() {
        let ext = AccountEntryExt::V1(AccountEntryExtensionV1 {
            liabilities: Liabilities {
                buying: 500,
                selling: 300,
            },
            ext: AccountEntryExtensionV1Ext::V0,
        });
        let mut entry = make_account_entry(ext);
        normalize_entry(&mut entry);

        let LedgerEntryData::Account(acc) = &entry.data else {
            panic!("expected account");
        };
        let AccountEntryExt::V1(v1) = &acc.ext else {
            panic!("expected V1");
        };
        // Liabilities must be preserved
        assert_eq!(v1.liabilities.buying, 500);
        assert_eq!(v1.liabilities.selling, 300);

        // V2 and V3 should be filled in
        let AccountEntryExtensionV1Ext::V2(v2) = &v1.ext else {
            panic!("expected V2");
        };
        let AccountEntryExtensionV2Ext::V3(v3) = &v2.ext else {
            panic!("expected V3");
        };
        assert_eq!(v3.seq_ledger, 0);
    }

    #[test]
    fn test_normalize_account_v2_to_v3() {
        let ext = AccountEntryExt::V1(AccountEntryExtensionV1 {
            liabilities: Liabilities {
                buying: 100,
                selling: 200,
            },
            ext: AccountEntryExtensionV1Ext::V2(AccountEntryExtensionV2 {
                num_sponsored: 5,
                num_sponsoring: 3,
                signer_sponsoring_i_ds: Default::default(),
                ext: AccountEntryExtensionV2Ext::V0,
            }),
        });
        let mut entry = make_account_entry(ext);
        normalize_entry(&mut entry);

        let LedgerEntryData::Account(acc) = &entry.data else {
            panic!("expected account");
        };
        let AccountEntryExt::V1(v1) = &acc.ext else {
            panic!("expected V1");
        };
        assert_eq!(v1.liabilities.buying, 100);
        assert_eq!(v1.liabilities.selling, 200);

        let AccountEntryExtensionV1Ext::V2(v2) = &v1.ext else {
            panic!("expected V2");
        };
        // Sponsoring info preserved
        assert_eq!(v2.num_sponsored, 5);
        assert_eq!(v2.num_sponsoring, 3);

        // V3 filled in
        let AccountEntryExtensionV2Ext::V3(v3) = &v2.ext else {
            panic!("expected V3");
        };
        assert_eq!(v3.seq_ledger, 0);
        assert_eq!(v3.seq_time, TimePoint(0));
    }

    #[test]
    fn test_normalize_account_already_v3() {
        let ext = AccountEntryExt::V1(AccountEntryExtensionV1 {
            liabilities: Liabilities {
                buying: 42,
                selling: 99,
            },
            ext: AccountEntryExtensionV1Ext::V2(AccountEntryExtensionV2 {
                num_sponsored: 7,
                num_sponsoring: 2,
                signer_sponsoring_i_ds: Default::default(),
                ext: AccountEntryExtensionV2Ext::V3(AccountEntryExtensionV3 {
                    ext: ExtensionPoint::V0,
                    seq_ledger: 12345,
                    seq_time: TimePoint(9999),
                }),
            }),
        });
        let mut entry = make_account_entry(ext);
        normalize_entry(&mut entry);

        let LedgerEntryData::Account(acc) = &entry.data else {
            panic!("expected account");
        };
        let AccountEntryExt::V1(v1) = &acc.ext else {
            panic!("expected V1");
        };
        assert_eq!(v1.liabilities.buying, 42);
        let AccountEntryExtensionV1Ext::V2(v2) = &v1.ext else {
            panic!("expected V2");
        };
        assert_eq!(v2.num_sponsored, 7);
        let AccountEntryExtensionV2Ext::V3(v3) = &v2.ext else {
            panic!("expected V3");
        };
        // Existing V3 values must be preserved, not zeroed
        assert_eq!(v3.seq_ledger, 12345);
        assert_eq!(v3.seq_time, TimePoint(9999));
    }

    #[test]
    fn test_normalize_account_with_signers() {
        let signers = vec![
            Signer {
                key: SignerKey::Ed25519(Uint256([10u8; 32])),
                weight: 1,
            },
            Signer {
                key: SignerKey::Ed25519(Uint256([20u8; 32])),
                weight: 2,
            },
            Signer {
                key: SignerKey::Ed25519(Uint256([30u8; 32])),
                weight: 3,
            },
        ];
        let mut entry = make_account_entry_with_signers(AccountEntryExt::V0, signers);
        normalize_entry(&mut entry);

        let LedgerEntryData::Account(acc) = &entry.data else {
            panic!("expected account");
        };
        let AccountEntryExt::V1(v1) = &acc.ext else {
            panic!("expected V1");
        };
        let AccountEntryExtensionV1Ext::V2(v2) = &v1.ext else {
            panic!("expected V2");
        };
        // signer_sponsoring_ids should have exactly 3 entries (one per signer)
        assert_eq!(v2.signer_sponsoring_i_ds.len(), 3);
        for id in v2.signer_sponsoring_i_ds.iter() {
            assert_eq!(id.0, None, "all sponsoring IDs should be None");
        }
    }

    #[test]
    fn test_normalize_non_account_unchanged() {
        let mut entry = LedgerEntry {
            last_modified_ledger_seq: 50,
            data: LedgerEntryData::ContractData(ContractDataEntry {
                ext: ExtensionPoint::V0,
                contract: ScAddress::Contract(ContractId(Hash([0xAA; 32]))),
                key: ScVal::I128(Int128Parts { hi: 0, lo: 1 }),
                durability: ContractDataDurability::Persistent,
                val: ScVal::Void,
            }),
            ext: LedgerEntryExt::V0,
        };
        let before_xdr = entry.to_xdr(Limits::none()).unwrap();
        normalize_entry(&mut entry);
        let after_xdr = entry.to_xdr(Limits::none()).unwrap();
        assert_eq!(
            before_xdr, after_xdr,
            "non-account entries must be unchanged"
        );
    }

    #[test]
    fn test_normalize_preserves_other_fields() {
        let mut entry = LedgerEntry {
            last_modified_ledger_seq: 999,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0xBB; 32]))),
                balance: 42_000_000,
                seq_num: SequenceNumber(7777),
                num_sub_entries: 3,
                inflation_dest: None,
                flags: 4,
                home_domain: Default::default(),
                thresholds: Thresholds([2, 3, 4, 5]),
                signers: Default::default(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        };
        normalize_entry(&mut entry);

        assert_eq!(entry.last_modified_ledger_seq, 999);
        let LedgerEntryData::Account(acc) = &entry.data else {
            panic!("expected account");
        };
        assert_eq!(
            acc.account_id,
            AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0xBB; 32])))
        );
        assert_eq!(acc.balance, 42_000_000);
        assert_eq!(acc.seq_num, SequenceNumber(7777));
        assert_eq!(acc.num_sub_entries, 3);
        assert_eq!(acc.flags, 4);
        assert_eq!(acc.thresholds, Thresholds([2, 3, 4, 5]));
        // Ext should now be V3 (but all other fields intact)
        assert!(matches!(acc.ext, AccountEntryExt::V1(_)));
    }

    #[test]
    fn test_normalized_entry_xdr_size() {
        // A V0 account entry serializes to 84 bytes as a standalone LedgerEntryData,
        // but as a full LedgerEntry (with lastModifiedLedgerSeq + LedgerEntryExt) it
        // is 92 bytes. After V3 normalization it should be 144 bytes (the same as
        // upstream's stateChanges representation).
        let entry_v0 = make_account_entry(AccountEntryExt::V0);
        let size_before = entry_v0.to_xdr(Limits::none()).unwrap().len();
        assert_eq!(size_before, 92, "V0 LedgerEntry should be 92 bytes");

        let mut entry_v3 = entry_v0;
        normalize_entry(&mut entry_v3);
        let size_after = entry_v3.to_xdr(Limits::none()).unwrap().len();
        assert_eq!(size_after, 144, "V3 LedgerEntry should be 144 bytes");
    }
}
