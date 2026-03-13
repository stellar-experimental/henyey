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
    pub(crate) fn get_unfiltered(&self, key: &LedgerKey) -> Option<(LedgerEntry, Option<u32>)> {
        let live_until = get_entry_ttl(&self.snapshot, key);
        let mut entry = self.snapshot.load(key)?;
        normalize_entry(&mut entry);
        Some((entry, live_until))
    }
}

impl SnapshotSource for BucketListSnapshotSource {
    fn get(&self, key: &Rc<LedgerKey>) -> Result<Option<EntryWithLiveUntil>, HostError> {
        // For contract data/code entries, we need to check TTL
        let live_until = get_entry_ttl(&self.snapshot, key.as_ref());

        // Check TTL expiration for contract entries
        if matches!(
            key.as_ref(),
            LedgerKey::ContractData(_) | LedgerKey::ContractCode(_)
        ) {
            match live_until {
                Some(ttl) if ttl >= self.current_ledger => {} // live, proceed
                _ => return Ok(None),                         // expired or no TTL
            }
        }

        // Look up the entry in the bucket list
        match self.snapshot.load(key.as_ref()) {
            Some(mut entry) => {
                normalize_entry(&mut entry);
                Ok(Some((Rc::new(entry), live_until)))
            }
            None => Ok(None),
        }
    }
}

/// Get the TTL (live_until_ledger) for a ledger entry from the bucket list.
fn get_entry_ttl(snapshot: &SearchableBucketListSnapshot, key: &LedgerKey) -> Option<u32> {
    let ttl_key = ttl_key_for_ledger_key(key)?;

    // Look up the TTL entry
    let ttl_entry = snapshot.load(&ttl_key)?;
    match ttl_entry.data {
        stellar_xdr::curr::LedgerEntryData::Ttl(ttl_data) => Some(ttl_data.live_until_ledger_seq),
        _ => None,
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
