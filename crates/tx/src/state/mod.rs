//! Ledger state management for transaction execution.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use parking_lot::Mutex;
use stellar_xdr::curr::{
    AccountEntry, AccountEntryExt, AccountEntryExtensionV1, AccountEntryExtensionV1Ext,
    AccountEntryExtensionV2, AccountEntryExtensionV2Ext, AccountEntryExtensionV3, AccountId, Asset,
    ClaimableBalanceEntry, ClaimableBalanceId, ContractCodeEntry, ContractDataDurability,
    ContractDataEntry, DataEntry, ExtensionPoint, Hash, LedgerEntry, LedgerEntryData,
    LedgerEntryExt, LedgerEntryExtensionV1, LedgerEntryExtensionV1Ext, LedgerKey, LedgerKeyAccount,
    LedgerKeyClaimableBalance, LedgerKeyContractCode, LedgerKeyContractData, LedgerKeyData,
    LedgerKeyLiquidityPool, LedgerKeyOffer, LedgerKeyTrustLine, LedgerKeyTtl, Liabilities,
    LiquidityPoolEntry, OfferEntry, PoolId, Price, ScAddress, ScVal, SponsorshipDescriptor,
    TimePoint, TrustLineAsset, TrustLineEntry, TtlEntry, VecM,
};

use offer_store::{OfferRecord, OfferStore};

use crate::apply::{ChangeLogLengths, TxChangeLog};
use crate::{Result, TxError};
pub(crate) use henyey_common::asset::asset_to_trustline_asset;

/// Callback type for lazily loading ledger entries from the bucket list.
type EntryLoaderFn = dyn Fn(&LedgerKey) -> Result<Option<LedgerEntry>> + Send + Sync;
type BatchEntryLoaderFn = dyn Fn(&[LedgerKey]) -> Result<Vec<LedgerEntry>> + Send + Sync;
/// Callback type for loading pool share trustline pool IDs by account from the
/// secondary index.  Used by `find_pool_share_trustlines_for_asset` to discover
/// pool share trustlines that may only exist in the bucket list (not yet in memory).
/// Mirrors stellar-core `loadPoolShareTrustLinesByAccountAndAsset` SQL query.
type PoolShareTlsByAccountLoaderFn = dyn Fn(&AccountId) -> Result<Vec<PoolId>> + Send + Sync;

/// Key for trustline entries: (account_id, trustline asset).
pub type TrustlineKey = (AccountId, TrustLineAsset);
/// Key for data entries: (account_id, data name).
pub type DataKey = (AccountId, String);

/// Soroban state extracted from LedgerStateManager for cheap cloning.
///
/// Path payment operations need to clone the entire state for speculative
/// orderbook exchange comparison against liquidity pools. By temporarily
/// extracting the large Soroban collections (which are never accessed during
/// orderbook exchange), the clone becomes much cheaper.
mod entries;
pub(crate) mod entry_store;
pub mod offer_index;
pub mod offer_store;
mod savepoint;
pub mod signers;
mod sponsorship;
mod ttl;

use entry_store::{EntryStore, EntryStoreSavepoint};
pub use savepoint::Savepoint;

pub use offer_index::{AssetPair, OfferDescriptor, OfferIndex, OfferKey};

/// Lightweight delta snapshot for TX-level rollback.
/// Instead of cloning the entire TxChangeLog (O(N) entries), we capture just
/// the vector lengths and fee_charged value. Since the delta is append-only
/// between snapshot and rollback, truncating to these lengths restores the
/// pre-TX state in O(1).
#[derive(Clone)]
struct DeltaSnapshot {
    lengths: ChangeLogLengths,
    fee_charged: i64,
}

pub struct SorobanState {
    pub contract_data: EntryStore<StorageKey, ContractDataEntry>,
    pub contract_code: EntryStore<Hash, ContractCodeEntry>,
    pub ttl_entries: HashMap<Hash, TtlEntry>,
    pub ttl_bucket_list_snapshot: HashMap<Hash, u32>,
}

/// Re-export `StorageKey` from the soroban module as the canonical key type
/// for contract data entries.
pub use crate::soroban::StorageKey;

// ==================== Offer Index ====================
//
// The OfferIndex provides O(log n) lookups for best offers by asset pair,
// similar to stellar-core's MultiOrderBook. This is critical for
// performance when executing path payments and manage offer operations.

/// Ledger state manager for transaction execution.
///
/// This provides read/write access to ledger entries during transaction
/// execution, tracking all changes for later persistence.
#[derive(Clone)]
pub struct LedgerStateManager {
    /// Current ledger sequence.
    ledger_seq: u32,
    /// Base reserve in stroops (minimum balance per sub-entry).
    base_reserve: i64,
    /// ID pool for generating offer IDs.
    id_pool: u64,
    /// Account entries by account ID.
    accounts: HashMap<AccountId, AccountEntry>,
    /// Trustline entries by (account, asset).
    trustlines: HashMap<TrustlineKey, TrustLineEntry>,
    /// Shared reference to the canonical offer store. `None` for fresh/Soroban cluster
    /// executors that don't touch offers.
    offer_store: Option<Arc<Mutex<OfferStore>>>,
    /// Data entries by (account, name).
    data_entries: EntryStore<DataKey, DataEntry>,
    /// Contract data entries by (contract, key, durability).
    contract_data: EntryStore<StorageKey, ContractDataEntry>,
    /// Contract code entries by hash.
    contract_code: EntryStore<Hash, ContractCodeEntry>,
    /// TTL entries by key hash.
    ttl_entries: HashMap<Hash, TtlEntry>,
    /// TTL values at ledger start (for Soroban execution).
    /// This is captured at the start of each ledger and remains read-only during execution.
    /// Soroban uses these values instead of ttl_entries to match stellar-core behavior where
    /// transactions see the bucket list state at ledger start, not changes from previous txs.
    ttl_bucket_list_snapshot: HashMap<Hash, u32>,
    /// Claimable balance entries by balance ID.
    claimable_balances: EntryStore<ClaimableBalanceId, ClaimableBalanceEntry>,
    /// Liquidity pool entries by pool ID.
    liquidity_pools: EntryStore<PoolId, LiquidityPoolEntry>,
    /// Sponsoring account IDs for non-offer ledger entries (only when sponsored).
    entry_sponsorships: HashMap<LedgerKey, AccountId>,
    /// Non-offer entries that have a sponsorship extension (even if not currently sponsored).
    entry_sponsorship_ext: HashSet<LedgerKey>,
    /// Last modified ledger sequence for non-offer entries.
    entry_last_modified: HashMap<LedgerKey, u32>,
    /// Per-operation snapshot of entries before mutation.
    op_entry_snapshots: HashMap<LedgerKey, LedgerEntry>,
    /// Whether op-level snapshots are active.
    op_snapshots_active: bool,
    /// Whether we're in a multi-operation transaction.
    /// When true, flush_modified_entries records STATE/UPDATED for every access,
    /// even if values are identical.
    multi_op_mode: bool,
    /// Active sponsorship stack for the current transaction.
    sponsorship_stack: Vec<SponsorshipContext>,
    /// Changes made during execution.
    delta: TxChangeLog,
    /// Track which entries have been modified for rollback.
    modified_accounts: Vec<AccountId>,
    /// Track which trustlines have been modified.
    modified_trustlines: Vec<TrustlineKey>,
    /// Track which offers have been modified.
    modified_offers: Vec<OfferKey>,
    // Data entries use EntryStore — modified tracking is internal.
    /// Track which TTL entries have been modified.
    modified_ttl: Vec<Hash>,
    /// Deferred read-only TTL bumps. These are TTL updates for read-only entries
    /// where only the TTL changed. Per stellar-core behavior:
    /// - They should NOT appear in transaction meta
    /// - They should be flushed to the delta at end of ledger (for bucket list)
    ///   Key is TTL key hash, value is the new live_until_ledger_seq.
    deferred_ro_ttl_bumps: HashMap<Hash, u32>,
    /// Snapshot of deferred RO TTL bumps at TX start (for rollback).
    deferred_ro_ttl_bumps_snapshot: Option<HashMap<Hash, u32>>,

    /// Snapshot of accounts for rollback.
    account_snapshots: HashMap<AccountId, Option<AccountEntry>>,
    /// Snapshot of trustlines for rollback.
    trustline_snapshots: HashMap<TrustlineKey, Option<TrustLineEntry>>,
    /// Snapshot of offers for rollback (captures full OfferRecord with metadata).
    offer_snapshots: HashMap<OfferKey, Option<OfferRecord>>,
    // Data entries use EntryStore — snapshots are internal.
    /// Snapshot of TTL entries for rollback.
    ttl_snapshots: HashMap<Hash, Option<TtlEntry>>,

    /// Snapshot of entry sponsorships for rollback.
    entry_sponsorship_snapshots: HashMap<LedgerKey, Option<AccountId>>,
    /// Snapshot of sponsorship extension presence for rollback.
    entry_sponsorship_ext_snapshots: HashMap<LedgerKey, bool>,
    /// Snapshot of last modified ledger sequence for rollback.
    entry_last_modified_snapshots: HashMap<LedgerKey, Option<u32>>,
    /// Track accounts created in this transaction (for rollback).
    created_accounts: HashSet<AccountId>,
    /// Track trustlines created in this transaction (for rollback).
    created_trustlines: HashSet<TrustlineKey>,
    /// Track offers created in this transaction (for rollback).
    created_offers: HashSet<OfferKey>,
    // Data entries use EntryStore — created tracking is internal.
    /// Track TTL entries created in this transaction (for rollback).
    created_ttl: HashSet<Hash>,

    /// Track TTL entries deleted in this ledger.
    deleted_ttl: HashSet<Hash>,
    /// Snapshot of id_pool for rollback. When an ID is generated during a transaction
    /// that later fails, the id_pool must be restored to its pre-transaction value.
    id_pool_snapshot: Option<u64>,
    /// Lightweight snapshot of delta for rollback. Instead of cloning the entire
    /// delta (which grows with each TX), we save just the vector lengths + fee.
    /// On rollback, we truncate the delta vectors to these saved lengths.
    /// This reduces snapshot cost from O(N entries) to O(1).
    delta_snapshot: Option<DeltaSnapshot>,
    /// Optional callback to lazily load ledger entries from the bucket list.
    /// Used during offer crossing to load seller accounts and trustlines
    /// on demand instead of preloading all offer dependencies upfront.
    entry_loader: Option<Arc<EntryLoaderFn>>,
    /// Optional batch callback for loading multiple entries in a single pass
    /// through the bucket list. Used by `ensure_offer_entries_loaded` to batch
    /// account + trustline lookups for offer sellers.
    batch_entry_loader: Option<Arc<BatchEntryLoaderFn>>,
    /// Optional callback to load pool share trustline pool IDs for a given account
    /// from the secondary index.  Mirrors stellar-core
    /// `loadPoolShareTrustLinesByAccountAndAsset` which queries SQL for all pool
    /// share trustlines owned by an account.  Without this loader,
    /// `find_pool_share_trustlines_for_asset` would only find pool shares already
    /// in memory, missing those only in the bucket list (VE-02).
    pool_share_tls_by_account_loader: Option<Arc<PoolShareTlsByAccountLoaderFn>>,
    /// Per-source-account maximum sequence number across all transactions in the
    /// current tx set.  Populated by `run_transactions_on_executor` when any
    /// transaction contains an AccountMerge operation.  `MergeOpFrame::isSeqnumTooFar`
    /// in stellar-core uses this to prevent merges that could allow sequence-number
    /// reuse after account re-creation.
    max_seq_num_to_apply: HashMap<AccountId, i64>,
}

#[derive(Debug, Clone)]
pub struct SponsorshipContext {
    pub sponsoring: AccountId,
    pub sponsored: AccountId,
}

impl LedgerStateManager {
    /// Create a new ledger state manager for the given ledger sequence.
    ///
    /// # Arguments
    ///
    /// * `base_reserve` - Base reserve in stroops (e.g., 5_000_000 for 0.5 XLM)
    /// * `ledger_seq` - The current ledger sequence number
    pub fn new(base_reserve: i64, ledger_seq: u32) -> Self {
        Self {
            ledger_seq,
            base_reserve,
            id_pool: 0,
            accounts: HashMap::new(),
            trustlines: HashMap::new(),
            offer_store: Some(Arc::new(Mutex::new(OfferStore::new()))),
            data_entries: EntryStore::new(),
            contract_data: EntryStore::new_with_deleted_tracking(),
            contract_code: EntryStore::new_with_deleted_tracking(),
            ttl_entries: HashMap::new(),
            ttl_bucket_list_snapshot: HashMap::new(),
            claimable_balances: EntryStore::new(),
            liquidity_pools: EntryStore::new(),
            entry_sponsorships: HashMap::new(),
            entry_sponsorship_ext: HashSet::new(),
            entry_last_modified: HashMap::new(),
            op_entry_snapshots: HashMap::new(),
            op_snapshots_active: false,
            multi_op_mode: false,
            sponsorship_stack: Vec::new(),
            delta: TxChangeLog::new(ledger_seq),
            modified_accounts: Vec::new(),
            modified_trustlines: Vec::new(),
            modified_offers: Vec::new(),
            // modified_data is internal to EntryStore
            modified_ttl: Vec::new(),
            deferred_ro_ttl_bumps: HashMap::new(),
            deferred_ro_ttl_bumps_snapshot: None,

            account_snapshots: HashMap::new(),
            trustline_snapshots: HashMap::new(),
            offer_snapshots: HashMap::new(),
            // data_snapshots is internal to EntryStore
            ttl_snapshots: HashMap::new(),

            entry_sponsorship_snapshots: HashMap::new(),
            entry_sponsorship_ext_snapshots: HashMap::new(),
            entry_last_modified_snapshots: HashMap::new(),
            created_accounts: HashSet::new(),
            created_trustlines: HashSet::new(),
            created_offers: HashSet::new(),
            // created_data is internal to EntryStore
            created_ttl: HashSet::new(),

            deleted_ttl: HashSet::new(),
            id_pool_snapshot: None,
            delta_snapshot: None,
            entry_loader: None,
            batch_entry_loader: None,
            pool_share_tls_by_account_loader: None,
            max_seq_num_to_apply: HashMap::new(),
        }
    }

    // ========================================================================
    // Offer/non-offer routed metadata accessors
    // ========================================================================

    fn is_offer_key(key: &LedgerKey) -> bool {
        matches!(key, LedgerKey::Offer(_))
    }

    /// Lock the shared offer store. Panics if no store is set.
    fn offer_store_lock(&self) -> parking_lot::MutexGuard<'_, OfferStore> {
        self.offer_store
            .as_ref()
            .expect("offer_store not set")
            .lock()
    }

    /// Set the shared offer store reference.
    pub fn set_offer_store(&mut self, store: Arc<Mutex<OfferStore>>) {
        self.offer_store = Some(store);
    }

    /// Returns true if an offer store is set.
    pub fn has_offer_store(&self) -> bool {
        self.offer_store.is_some()
    }

    fn get_entry_sponsorship(&self, key: &LedgerKey) -> Option<AccountId> {
        if Self::is_offer_key(key) {
            if let LedgerKey::Offer(ok) = key {
                let store = self.offer_store_lock();
                let from_store = store
                    .get_by_seller(&ok.seller_id, ok.offer_id)
                    .and_then(|r| r.sponsor.clone());
                // Also check fallback map (sponsorship set before offer exists).
                from_store.or_else(|| self.entry_sponsorships.get(key).cloned())
            } else {
                None
            }
        } else {
            self.entry_sponsorships.get(key).cloned()
        }
    }

    fn insert_entry_sponsorship(&mut self, key: LedgerKey, sponsor: AccountId) {
        if Self::is_offer_key(&key) {
            if let LedgerKey::Offer(ok) = &key {
                let offer_key = OfferKey::new(ok.seller_id.clone(), ok.offer_id);
                let mut store = self.offer_store_lock();
                if let Some(record) = store.get_mut(&offer_key) {
                    record.sponsor = Some(sponsor);
                    record.has_ext = true;
                } else {
                    // Offer doesn't exist yet (sponsorship set before create_offer).
                    // Store in entry_sponsorships; create_offer will pick it up.
                    drop(store);
                    self.entry_sponsorships.insert(key, sponsor);
                }
            }
        } else {
            self.entry_sponsorships.insert(key, sponsor);
        }
    }

    fn remove_entry_sponsorship(&mut self, key: &LedgerKey) -> Option<AccountId> {
        if Self::is_offer_key(key) {
            if let LedgerKey::Offer(ok) = key {
                let offer_key = OfferKey::new(ok.seller_id.clone(), ok.offer_id);
                let mut store = self.offer_store_lock();
                if let Some(record) = store.get_mut(&offer_key) {
                    record.sponsor.take()
                } else {
                    // Check fallback map (sponsorship set before offer existed).
                    drop(store);
                    self.entry_sponsorships.remove(key)
                }
            } else {
                None
            }
        } else {
            self.entry_sponsorships.remove(key)
        }
    }

    fn contains_sponsorship_ext(&self, key: &LedgerKey) -> bool {
        if Self::is_offer_key(key) {
            if let LedgerKey::Offer(ok) = key {
                let store = self.offer_store_lock();
                let in_store = store
                    .get_by_seller(&ok.seller_id, ok.offer_id)
                    .map(|r| r.has_ext)
                    .unwrap_or(false);
                // Also check fallback set (ext set before offer exists).
                in_store || self.entry_sponsorship_ext.contains(key)
            } else {
                false
            }
        } else {
            self.entry_sponsorship_ext.contains(key)
        }
    }

    fn insert_sponsorship_ext(&mut self, key: LedgerKey) {
        if Self::is_offer_key(&key) {
            if let LedgerKey::Offer(ok) = &key {
                let offer_key = OfferKey::new(ok.seller_id.clone(), ok.offer_id);
                let mut store = self.offer_store_lock();
                if let Some(record) = store.get_mut(&offer_key) {
                    record.has_ext = true;
                } else {
                    // Offer doesn't exist yet; store in ext set for create_offer to pick up.
                    drop(store);
                    self.entry_sponsorship_ext.insert(key);
                }
            }
        } else {
            self.entry_sponsorship_ext.insert(key);
        }
    }

    fn remove_sponsorship_ext(&mut self, key: &LedgerKey) -> bool {
        if Self::is_offer_key(key) {
            if let LedgerKey::Offer(ok) = key {
                let offer_key = OfferKey::new(ok.seller_id.clone(), ok.offer_id);
                let mut store = self.offer_store_lock();
                if let Some(record) = store.get_mut(&offer_key) {
                    let was = record.has_ext;
                    record.has_ext = false;
                    was
                } else {
                    // Check fallback set (ext set before offer existed).
                    drop(store);
                    self.entry_sponsorship_ext.remove(key)
                }
            } else {
                false
            }
        } else {
            self.entry_sponsorship_ext.remove(key)
        }
    }

    fn get_last_modified(&self, key: &LedgerKey) -> Option<u32> {
        if Self::is_offer_key(key) {
            if let LedgerKey::Offer(ok) = key {
                let store = self.offer_store_lock();
                store
                    .get_by_seller(&ok.seller_id, ok.offer_id)
                    .map(|r| r.last_modified)
            } else {
                None
            }
        } else {
            self.entry_last_modified.get(key).copied()
        }
    }

    fn insert_last_modified(&mut self, key: LedgerKey, seq: u32) {
        if Self::is_offer_key(&key) {
            if let LedgerKey::Offer(ok) = &key {
                let offer_key = OfferKey::new(ok.seller_id.clone(), ok.offer_id);
                let mut store = self.offer_store_lock();
                if let Some(record) = store.get_mut(&offer_key) {
                    record.last_modified = seq;
                }
            }
        } else {
            self.entry_last_modified.insert(key, seq);
        }
    }

    fn remove_last_modified(&mut self, key: &LedgerKey) {
        if !Self::is_offer_key(key) {
            self.entry_last_modified.remove(key);
        }
        // For offers, metadata is inline in OfferRecord — removed when the offer is removed.
    }

    pub fn begin_op_snapshot(&mut self) {
        self.op_entry_snapshots.clear();
        self.op_snapshots_active = true;
    }

    pub fn end_op_snapshot(&mut self) -> HashMap<LedgerKey, LedgerEntry> {
        self.op_snapshots_active = false;
        std::mem::take(&mut self.op_entry_snapshots)
    }

    /// Set multi-operation mode.
    ///
    /// When enabled, flush_modified_entries records STATE/UPDATED for every
    /// accessed entry even if values are identical.
    pub fn set_multi_op_mode(&mut self, enabled: bool) {
        self.multi_op_mode = enabled;
    }

    /// Compute the starting sequence number for new accounts.
    pub fn starting_sequence_number(&self) -> crate::Result<i64> {
        if self.ledger_seq > i32::MAX as u32 {
            return Err(crate::TxError::Internal(
                "overflowed starting sequence number".to_string(),
            ));
        }
        Ok((self.ledger_seq as i64) << 32)
    }

    /// Set the per-account maximum sequence numbers for the current tx set.
    /// Called by the tx-set execution loop when any transaction contains an
    /// AccountMerge operation.
    pub fn set_max_seq_num_to_apply(&mut self, map: HashMap<AccountId, i64>) {
        self.max_seq_num_to_apply = map;
    }

    /// Look up the maximum sequence number that any transaction in the current
    /// tx set uses for the given source account.
    pub fn get_max_seq_num_to_apply(&self, account_id: &AccountId) -> Option<&i64> {
        self.max_seq_num_to_apply.get(account_id)
    }

    /// Calculate the minimum balance required for an account.
    pub fn minimum_balance_for_account(
        &self,
        account: &AccountEntry,
        protocol_version: u32,
        additional_subentries: i64,
    ) -> Result<i64> {
        let num_sub_entries = account.num_sub_entries as i64 + additional_subentries;
        if num_sub_entries < 0 {
            return Err(TxError::Internal(
                "negative subentry count while computing minimum balance".to_string(),
            ));
        }
        let (num_sponsoring, num_sponsored) = sponsorship_counts(account);
        self.minimum_balance_with_counts(
            protocol_version,
            num_sub_entries,
            num_sponsoring,
            num_sponsored,
        )
    }

    /// Calculate the minimum balance for an account with sponsorship deltas.
    pub fn minimum_balance_for_account_with_deltas(
        &self,
        account: &AccountEntry,
        protocol_version: u32,
        additional_subentries: i64,
        delta_sponsoring: i64,
        delta_sponsored: i64,
    ) -> Result<i64> {
        let num_sub_entries = account.num_sub_entries as i64 + additional_subentries;
        if num_sub_entries < 0 {
            return Err(TxError::Internal(
                "negative subentry count while computing minimum balance".to_string(),
            ));
        }
        let (num_sponsoring, num_sponsored) = sponsorship_counts(account);
        let num_sponsoring = num_sponsoring + delta_sponsoring;
        let num_sponsored = num_sponsored + delta_sponsored;
        if num_sponsoring < 0 || num_sponsored < 0 {
            return Err(TxError::Internal(
                "negative sponsorship count while computing minimum balance".to_string(),
            ));
        }
        self.minimum_balance_with_counts(
            protocol_version,
            num_sub_entries,
            num_sponsoring,
            num_sponsored,
        )
    }

    /// Calculate the minimum balance for a hypothetical account state.
    pub fn minimum_balance_with_counts(
        &self,
        _protocol_version: u32,
        num_sub_entries: i64,
        num_sponsoring: i64,
        num_sponsored: i64,
    ) -> Result<i64> {
        let effective_entries = 2 + num_sub_entries + num_sponsoring - num_sponsored;

        if effective_entries < 0 {
            return Err(TxError::Internal(
                "unexpected account state while computing minimum balance".to_string(),
            ));
        }

        Ok(effective_entries * self.base_reserve)
    }

    /// Get the base reserve.
    pub fn base_reserve(&self) -> i64 {
        self.base_reserve
    }

    /// Get the current ID pool.
    pub fn id_pool(&self) -> u64 {
        self.id_pool
    }

    /// Set the current ID pool.
    pub fn set_id_pool(&mut self, id_pool: u64) {
        self.id_pool = id_pool;
    }

    /// Generate the next ID from the pool.
    ///
    /// The first call within a transaction snapshots the id_pool so it can be
    /// restored on rollback. This ensures that failed transactions don't consume
    /// offer IDs.
    pub fn next_id(&mut self) -> i64 {
        // Snapshot id_pool before first modification in this transaction
        if self.id_pool_snapshot.is_none() {
            self.id_pool_snapshot = Some(self.id_pool);
        }
        self.id_pool = self.id_pool.checked_add(1).expect("id_pool overflow");
        i64::try_from(self.id_pool).expect("id_pool exceeds i64::MAX")
    }

    /// Get the current ledger sequence.
    pub fn ledger_seq(&self) -> u32 {
        self.ledger_seq
    }

    /// Set the current ledger sequence.
    pub fn set_ledger_seq(&mut self, ledger_seq: u32) {
        self.ledger_seq = ledger_seq;
    }

    /// Set the entry loader callback for lazy loading from the bucket list.
    ///
    /// When set, `ensure_account_loaded` and `ensure_trustline_loaded` can
    /// fetch entries on demand during offer crossing instead of requiring
    /// all dependencies to be preloaded upfront.
    pub fn set_entry_loader(&mut self, loader: Arc<EntryLoaderFn>) {
        self.entry_loader = Some(loader);
    }

    /// Set a batch entry loader for loading multiple entries in one bucket list pass.
    pub fn set_batch_entry_loader(&mut self, loader: Arc<BatchEntryLoaderFn>) {
        self.batch_entry_loader = Some(loader);
    }

    /// Set the loader that returns pool IDs for pool share trustlines owned by an account.
    ///
    /// This mirrors stellar-core `loadPoolShareTrustLinesByAccountAndAsset` and is used
    /// by `find_pool_share_trustlines_for_asset` (called during authorization revocation)
    /// to ensure ALL matching pool share trustlines are found, not just those that
    /// happened to be loaded into memory during prior TX execution.
    pub fn set_pool_share_tls_by_account_loader(
        &mut self,
        loader: Arc<PoolShareTlsByAccountLoaderFn>,
    ) {
        self.pool_share_tls_by_account_loader = Some(loader);
    }

    /// Clear all snapshot-backed loaders.
    ///
    /// Called after TX execution completes, before storing the executor back
    /// for reuse. This drops Arc references to the SnapshotHandle clones
    /// (and transitively to the soroban state snapshot), allowing
    /// `Arc::make_mut` on the live soroban state to mutate in-place during
    /// `commit()` instead of deep-cloning the entire HashMap.
    pub fn clear_entry_loaders(&mut self) {
        self.entry_loader = None;
        self.batch_entry_loader = None;
        self.pool_share_tls_by_account_loader = None;
    }

    /// Batch-load all entries needed to cross an offer (seller account + trustlines).
    ///
    /// This is significantly faster than separate `ensure_account_loaded` +
    /// `ensure_trustline_loaded` calls because it performs a single pass through
    /// the bucket list for all needed entries instead of 2-3 separate passes.
    pub fn ensure_offer_entries_loaded(
        &mut self,
        seller: &AccountId,
        selling: &Asset,
        buying: &Asset,
    ) -> Result<()> {
        let mut needed_keys = Vec::new();

        if !self.accounts.contains_key(seller) {
            needed_keys.push(LedgerKey::Account(LedgerKeyAccount {
                account_id: seller.clone(),
            }));
        }
        if !matches!(selling, Asset::Native) {
            let tl_key = (seller.clone(), asset_to_trustline_asset(selling));
            if !self.trustlines.contains_key(&tl_key) {
                needed_keys.push(LedgerKey::Trustline(LedgerKeyTrustLine {
                    account_id: tl_key.0,
                    asset: tl_key.1,
                }));
            }
        }
        if !matches!(buying, Asset::Native) {
            let tl_key = (seller.clone(), asset_to_trustline_asset(buying));
            if !self.trustlines.contains_key(&tl_key) {
                needed_keys.push(LedgerKey::Trustline(LedgerKeyTrustLine {
                    account_id: tl_key.0,
                    asset: tl_key.1,
                }));
            }
        }

        if needed_keys.is_empty() {
            return Ok(());
        }

        // Use batch loader for single-pass bucket list traversal
        if let Some(loader) = self.batch_entry_loader.take() {
            let entries = loader(&needed_keys);
            self.batch_entry_loader = Some(loader);
            for entry in entries? {
                self.load_entry(entry);
            }
        } else if let Some(loader) = self.entry_loader.take() {
            // Fallback to individual lookups
            for key in &needed_keys {
                if let Some(entry) = loader(key)? {
                    self.load_entry(entry);
                }
            }
            self.entry_loader = Some(loader);
        }

        Ok(())
    }

    /// Ensure an account is loaded in state, fetching lazily if needed.
    ///
    /// Returns `Ok(true)` if the account is available (already loaded or
    /// successfully fetched), `Ok(false)` if it doesn't exist.
    pub fn ensure_account_loaded(&mut self, account_id: &AccountId) -> Result<bool> {
        if self.accounts.contains_key(account_id) {
            return Ok(true);
        }
        if let Some(loader) = self.entry_loader.take() {
            let ledger_key = LedgerKey::Account(LedgerKeyAccount {
                account_id: account_id.clone(),
            });
            let result = loader(&ledger_key);
            self.entry_loader = Some(loader); // restore before handling result
            if let Some(entry) = result? {
                self.load_entry(entry);
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Ensure a trustline is loaded in state, fetching lazily if needed.
    ///
    /// Returns `Ok(true)` if the trustline is available (already loaded or
    /// successfully fetched), `Ok(false)` if it doesn't exist.
    pub fn ensure_trustline_loaded(
        &mut self,
        account_id: &AccountId,
        asset: &Asset,
    ) -> Result<bool> {
        let tl_key = (account_id.clone(), asset_to_trustline_asset(asset));
        if self.trustlines.contains_key(&tl_key) {
            return Ok(true);
        }
        if let Some(loader) = self.entry_loader.take() {
            let ledger_key = LedgerKey::Trustline(LedgerKeyTrustLine {
                account_id: tl_key.0,
                asset: tl_key.1,
            });
            let result = loader(&ledger_key);
            self.entry_loader = Some(loader); // restore before handling result
            if let Some(entry) = result? {
                self.load_entry(entry);
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Temporarily remove large Soroban collections to make clone() cheap.
    ///
    /// Used by path payment operations that need to clone state for speculative
    /// orderbook exchange. The Soroban collections (contract data, code, TTL entries)
    /// are never accessed during orderbook exchange, so removing them before cloning
    /// and restoring after avoids copying millions of entries.
    pub fn take_soroban_state(&mut self) -> SorobanState {
        SorobanState {
            contract_data: std::mem::replace(
                &mut self.contract_data,
                EntryStore::new_with_deleted_tracking(),
            ),
            contract_code: std::mem::replace(
                &mut self.contract_code,
                EntryStore::new_with_deleted_tracking(),
            ),
            ttl_entries: std::mem::take(&mut self.ttl_entries),
            ttl_bucket_list_snapshot: std::mem::take(&mut self.ttl_bucket_list_snapshot),
        }
    }

    /// Restore previously extracted Soroban collections.
    pub fn restore_soroban_state(&mut self, soroban: SorobanState) {
        self.contract_data = soroban.contract_data;
        self.contract_code = soroban.contract_code;
        self.ttl_entries = soroban.ttl_entries;
        self.ttl_bucket_list_snapshot = soroban.ttl_bucket_list_snapshot;
    }

    /// Snapshot the delta before starting a transaction.
    ///
    /// This preserves committed changes from previous transactions so they're not lost
    /// if the current transaction fails and rolls back. Call this at the start of each
    /// transaction before any modifications.
    pub fn snapshot_delta(&mut self) {
        self.delta_snapshot = Some(DeltaSnapshot {
            lengths: self.delta.snapshot_lengths(),
            fee_charged: self.delta.fee_charged(),
        });
        self.deferred_ro_ttl_bumps_snapshot = Some(self.deferred_ro_ttl_bumps.clone());
    }

    /// Clear all cached ledger entries.
    ///
    /// This clears all entry storage (accounts, trustlines, offers, etc.) while
    /// preserving ledger-level state like id_pool. Use this at the start of a new
    /// ledger in verification mode to ensure entries are reloaded from the
    /// authoritative bucket list state.
    pub fn clear_cached_entries(&mut self) {
        self.clear_cached_entries_inner(false);
    }

    /// Clear cached entries but preserve offers and the offer index.
    ///
    /// Offers are expensive to reload (~911K entries on mainnet, ~2.7s). Since
    /// the in-memory offer store is maintained incrementally, the executor's
    /// offer cache at the end of a ledger already reflects the correct post-ledger
    /// state. This method clears everything else so non-offer entries are reloaded
    /// from the authoritative bucket list.
    pub fn clear_cached_entries_preserving_offers(&mut self) {
        self.clear_cached_entries_inner(true);
    }

    fn clear_cached_entries_inner(&mut self, _preserve_offers: bool) {
        // Offers live in the shared OfferStore (always preserved).
        // Only non-offer entries and transaction-level state are cleared here.
        self.accounts.clear();
        self.trustlines.clear();
        self.data_entries.clear(); // EntryStore::clear()
        self.contract_data.clear();
        self.contract_code.clear();
        self.ttl_entries.clear();
        self.ttl_bucket_list_snapshot.clear();
        self.claimable_balances.clear();
        self.liquidity_pools.clear();
        self.entry_sponsorships.clear();
        self.entry_sponsorship_ext.clear();
        self.entry_last_modified.clear();
        self.entry_loader = None;
        self.batch_entry_loader = None;
        self.pool_share_tls_by_account_loader = None;

        // Clear all transaction-level state
        self.op_entry_snapshots.clear();
        self.op_snapshots_active = false;
        self.multi_op_mode = false;
        self.sponsorship_stack.clear();
        self.delta = TxChangeLog::new(self.ledger_seq);
        self.delta_snapshot = None;

        self.modified_accounts.clear();
        self.modified_trustlines.clear();
        self.modified_offers.clear();
        // modified_data is cleared by data_entries.clear() above
        self.modified_ttl.clear();

        self.account_snapshots.clear();
        self.trustline_snapshots.clear();
        self.offer_snapshots.clear();
        // data_snapshots is cleared by data_entries.clear() above
        self.ttl_snapshots.clear();
        self.entry_sponsorship_snapshots.clear();
        self.entry_sponsorship_ext_snapshots.clear();
        self.entry_last_modified_snapshots.clear();

        self.created_accounts.clear();
        self.created_trustlines.clear();
        self.created_offers.clear();
        // created_data is cleared by data_entries.clear() above
        self.created_ttl.clear();
    }

    // ========================================================================
    // One-time (Pre-Auth TX) Signer Removal
    // ========================================================================

    // ==================== Account Operations ====================

    // ==================== Trustline Operations ====================

    // ==================== Offer Operations ====================

    // ==================== Data Entry Operations ====================

    // ==================== Contract Data Operations ====================

    // ==================== Contract Code Operations ====================

    // ==================== TTL Entry Operations ====================

    // ==================== Claimable Balance Operations ====================

    // ==================== Liquidity Pool Operations ====================

    // ==================== Generic Entry Operations ====================

    /// Get the pre-modification entry snapshot by LedgerKey.
    pub fn snapshot_entry(&self, key: &LedgerKey) -> Option<LedgerEntry> {
        let last_modified = self
            .last_modified_snapshot_for_key(key)
            .unwrap_or_else(|| self.last_modified_for_key(key));
        let ext = self.ledger_entry_ext_for_snapshot(key);

        match key {
            LedgerKey::Account(k) => self
                .account_snapshots
                .get(&k.account_id)
                .cloned()
                .flatten()
                .map(|entry| LedgerEntry {
                    last_modified_ledger_seq: last_modified,
                    data: LedgerEntryData::Account(entry),
                    ext,
                }),
            LedgerKey::Trustline(k) => self
                .trustline_snapshots
                .get(&(k.account_id.clone(), k.asset.clone()))
                .cloned()
                .flatten()
                .map(|entry| LedgerEntry {
                    last_modified_ledger_seq: last_modified,
                    data: LedgerEntryData::Trustline(entry),
                    ext,
                }),
            LedgerKey::Offer(k) => self
                .offer_snapshots
                .get(&OfferKey::new(k.seller_id.clone(), k.offer_id))
                .cloned()
                .flatten()
                .map(|record| record.to_ledger_entry()),
            LedgerKey::Data(k) => {
                let name = data_name_to_string(&k.data_name);
                self.data_entries
                    .snapshot_value(&(k.account_id.clone(), name))
                    .cloned()
                    .flatten()
                    .map(|entry| LedgerEntry {
                        last_modified_ledger_seq: last_modified,
                        data: LedgerEntryData::Data(entry),
                        ext,
                    })
            }
            LedgerKey::ContractData(k) => {
                let lookup_key = StorageKey::new(k.contract.clone(), k.key.clone(), k.durability);
                self.contract_data
                    .snapshot_value(&lookup_key)
                    .cloned()
                    .flatten()
                    .map(|entry| LedgerEntry {
                        last_modified_ledger_seq: last_modified,
                        data: LedgerEntryData::ContractData(entry),
                        ext,
                    })
            }
            LedgerKey::ContractCode(k) => self
                .contract_code
                .snapshot_value(&k.hash)
                .cloned()
                .flatten()
                .map(|entry| LedgerEntry {
                    last_modified_ledger_seq: last_modified,
                    data: LedgerEntryData::ContractCode(entry),
                    ext,
                }),
            LedgerKey::Ttl(k) => {
                self.ttl_snapshots
                    .get(&k.key_hash)
                    .cloned()
                    .flatten()
                    .map(|entry| LedgerEntry {
                        last_modified_ledger_seq: last_modified,
                        data: LedgerEntryData::Ttl(entry),
                        ext,
                    })
            }
            LedgerKey::ClaimableBalance(k) => self
                .claimable_balances
                .snapshot_value(&k.balance_id)
                .cloned()
                .flatten()
                .map(|entry| LedgerEntry {
                    last_modified_ledger_seq: last_modified,
                    data: LedgerEntryData::ClaimableBalance(entry),
                    ext,
                }),
            LedgerKey::LiquidityPool(k) => self
                .liquidity_pools
                .snapshot_value(&k.liquidity_pool_id)
                .cloned()
                .flatten()
                .map(|entry| LedgerEntry {
                    last_modified_ledger_seq: last_modified,
                    data: LedgerEntryData::LiquidityPool(entry),
                    ext,
                }),
            _ => None,
        }
    }

    // ==================== Delta Operations ====================

    /// Get the current delta (read-only).
    pub fn delta(&self) -> &TxChangeLog {
        &self.delta
    }

    /// Get the current delta (mutable).
    pub fn delta_mut(&mut self) -> &mut TxChangeLog {
        &mut self.delta
    }

    /// Consume self and return the delta.
    pub fn take_delta(self) -> TxChangeLog {
        self.delta
    }

    /// Check if there are any pending changes.
    pub fn has_changes(&self) -> bool {
        self.delta.has_changes()
    }

    /// Estimate total heap bytes used by this state manager.
    ///
    /// Returns (total_bytes, offer_bytes) where offer_bytes is the portion
    /// from offer-related maps that are preserved across ledger closes.
    pub fn estimate_heap_bytes(&self) -> (usize, usize) {
        use henyey_common::memory::{hashmap_heap_bytes, hashset_heap_bytes, vec_heap_bytes};

        // Sizes of key/value types (approximate for XDR types)
        let account_id_size = 36; // PublicKey enum
        let trustline_key_size = 100; // (AccountId, TrustLineAsset)
        let offer_key_size = 44; // (AccountId, i64)
        let hash_size = 32;
        let ledger_key_size = 80; // enum with variants
        let account_entry_size = 200;
        let trustline_entry_size = 150;
        let offer_entry_size = 200;
        let ttl_entry_size = 16;

        // Core entry maps
        let accounts = hashmap_heap_bytes(
            self.accounts.capacity(),
            account_id_size,
            account_entry_size,
        );
        let trustlines = hashmap_heap_bytes(
            self.trustlines.capacity(),
            trustline_key_size,
            trustline_entry_size,
        );
        // Offers now live in the shared OfferStore; no local offer map.
        let ttl_entries =
            hashmap_heap_bytes(self.ttl_entries.capacity(), hash_size, ttl_entry_size);
        let ttl_snapshot =
            hashmap_heap_bytes(self.ttl_bucket_list_snapshot.capacity(), hash_size, 4);

        // EntryStore-based maps
        let data_entries = self.data_entries.estimate_heap_bytes(100, 200);
        let contract_data = self.contract_data.estimate_heap_bytes(120, 300);
        let contract_code = self.contract_code.estimate_heap_bytes(hash_size, 500);
        let claimable_balances = self.claimable_balances.estimate_heap_bytes(hash_size, 300);
        let liquidity_pools = self.liquidity_pools.estimate_heap_bytes(hash_size, 300);

        // Sponsorship maps (non-offer only; offer metadata is in OfferStore)
        let entry_sponsorships = hashmap_heap_bytes(
            self.entry_sponsorships.capacity(),
            ledger_key_size,
            account_id_size,
        );
        let entry_sponsorship_ext =
            hashset_heap_bytes(self.entry_sponsorship_ext.capacity(), ledger_key_size);
        let entry_last_modified =
            hashmap_heap_bytes(self.entry_last_modified.capacity(), ledger_key_size, 4);

        // Snapshot maps (typically empty between TXs)
        let op_entry_snapshots =
            hashmap_heap_bytes(self.op_entry_snapshots.capacity(), ledger_key_size, 200);
        let account_snapshots = hashmap_heap_bytes(
            self.account_snapshots.capacity(),
            account_id_size,
            account_entry_size,
        );
        let trustline_snapshots = hashmap_heap_bytes(
            self.trustlines.capacity(),
            trustline_key_size,
            trustline_entry_size,
        );
        let offer_snapshots = hashmap_heap_bytes(
            self.offer_snapshots.capacity(),
            offer_key_size,
            offer_entry_size + 48,
        );
        let ttl_snapshots =
            hashmap_heap_bytes(self.ttl_snapshots.capacity(), hash_size, ttl_entry_size);
        let sponsorship_snapshots = hashmap_heap_bytes(
            self.entry_sponsorship_snapshots.capacity(),
            ledger_key_size,
            account_id_size,
        );
        let ext_snapshots = hashmap_heap_bytes(
            self.entry_sponsorship_ext_snapshots.capacity(),
            ledger_key_size,
            1,
        );
        let lm_snapshots = hashmap_heap_bytes(
            self.entry_last_modified_snapshots.capacity(),
            ledger_key_size,
            4,
        );

        // Tracking vecs and sets
        let modified_accounts = vec_heap_bytes(self.modified_accounts.capacity(), account_id_size);
        let modified_trustlines =
            vec_heap_bytes(self.modified_trustlines.capacity(), trustline_key_size);
        let modified_offers = vec_heap_bytes(self.modified_offers.capacity(), offer_key_size);
        let modified_ttl = vec_heap_bytes(self.modified_ttl.capacity(), hash_size);
        let created_accounts =
            hashset_heap_bytes(self.created_accounts.capacity(), account_id_size);
        let created_trustlines =
            hashset_heap_bytes(self.created_trustlines.capacity(), trustline_key_size);
        let created_offers = hashset_heap_bytes(self.created_offers.capacity(), offer_key_size);
        let created_ttl = hashset_heap_bytes(self.created_ttl.capacity(), hash_size);
        let deleted_ttl = hashset_heap_bytes(self.deleted_ttl.capacity(), hash_size);

        // Deferred TTL bumps
        let deferred = hashmap_heap_bytes(self.deferred_ro_ttl_bumps.capacity(), hash_size, 4);

        let max_seq = hashmap_heap_bytes(self.max_seq_num_to_apply.capacity(), account_id_size, 8);

        // Offer-related bytes are now in OfferStore (reported separately by LedgerManager)
        let offer_bytes = 0;

        let total = accounts
            + trustlines
            + ttl_entries
            + ttl_snapshot
            + data_entries
            + contract_data
            + contract_code
            + claimable_balances
            + liquidity_pools
            + entry_sponsorships
            + entry_sponsorship_ext
            + entry_last_modified
            + op_entry_snapshots
            + account_snapshots
            + trustline_snapshots
            + offer_snapshots
            + ttl_snapshots
            + sponsorship_snapshots
            + ext_snapshots
            + lm_snapshots
            + modified_accounts
            + modified_trustlines
            + modified_offers
            + modified_ttl
            + created_accounts
            + created_trustlines
            + created_offers
            + created_ttl
            + deleted_ttl
            + deferred
            + max_seq;

        (total, offer_bytes)
    }

    /// Number of offers in the shared offer store.
    pub fn offer_count(&self) -> usize {
        if let Some(store) = &self.offer_store {
            store.lock().len()
        } else {
            0
        }
    }

    /// Apply a fee refund to the most recent account update in the delta.
    ///
    /// In stellar-core, fee refunds are NOT separate meta changes - they're
    /// incorporated into the final account balance of the existing update.
    /// This method finds the most recent update to the account and adds the refund.
    /// Uses stellar-core `addBalance` semantics: skips the refund on overflow
    /// or buying-liabilities violation (TransactionUtils.cpp:561-592).
    pub fn apply_refund_to_delta(&mut self, account_id: &AccountId, refund: i64) -> bool {
        use henyey_common::asset::try_add_account_balance;

        // Apply refund to the delta's account entry.
        let applied = self.delta.apply_refund_to_account(account_id, refund);
        if !applied {
            return false;
        }
        // Also update the in-memory account state (without recording a new delta)
        if let Some(acc) = self.accounts.get_mut(account_id) {
            let _ = try_add_account_balance(acc, refund);
        }
        true
    }

    /// Commit changes by clearing snapshots (changes become permanent).
    pub fn commit(&mut self) {
        // Clear id_pool snapshot (the incremented value is now committed)
        self.id_pool_snapshot = None;

        // NOTE: Do NOT clear delta_snapshot here. The delta_snapshot is used to preserve
        // committed changes from PREVIOUS transactions in a ledger, so it should only
        // be set/cleared at transaction boundaries (via snapshot_delta()) or when
        // starting a new ledger (via clear_cached_entries()).

        // Clear all snapshots
        self.account_snapshots.clear();
        self.trustline_snapshots.clear();
        self.offer_snapshots.clear();
        // data_snapshots cleared by data_entries.commit() below
        self.ttl_snapshots.clear();
        self.entry_sponsorship_snapshots.clear();
        self.entry_sponsorship_ext_snapshots.clear();
        self.entry_last_modified_snapshots.clear();

        // Commit EntryStore-based types
        self.claimable_balances.commit();
        self.liquidity_pools.commit();
        self.contract_code.commit();
        self.contract_data.commit();
        self.data_entries.commit();

        // Clear modification tracking
        self.modified_accounts.clear();
        self.modified_trustlines.clear();
        self.modified_offers.clear();
        // modified_data cleared by data_entries.commit() above
        self.modified_ttl.clear();

        // Clear created entry tracking
        self.created_accounts.clear();
        self.created_trustlines.clear();
        self.created_offers.clear();
        // created_data cleared by data_entries.commit() above
        self.created_ttl.clear();
    }

    /// Flush all pending account changes to the delta, excluding a specific account.
    ///
    /// This flushes only accounts, not trustlines or other entry types.
    /// Used when an operation needs to ensure all account changes are recorded
    /// before recording trustline changes (e.g., before deleting a trustline).
    ///
    /// The `exclude` parameter specifies an account to skip (e.g., an account
    /// that's about to be deleted). If None, all accounts are flushed.
    pub fn flush_all_accounts_except(&mut self, exclude: Option<&AccountId>) {
        let modified_accounts = std::mem::take(&mut self.modified_accounts);
        let mut remaining = Vec::new();
        for key in modified_accounts {
            if exclude == Some(&key) {
                // Keep excluded account in modified list (will be handled by delete)
                remaining.push(key);
                continue;
            }
            if let Some(Some(snapshot_entry)) = self.account_snapshots.get(&key) {
                if let Some(entry) = self.accounts.get(&key).cloned() {
                    // Build ledger key for op_snapshot lookup
                    let ledger_key = LedgerKey::Account(LedgerKeyAccount {
                        account_id: entry.account_id.clone(),
                    });
                    // Record update if:
                    // 1. Entry was accessed during operation (in op_entry_snapshots) - stellar-core records all loadAccount calls
                    // 2. Entry actually changed - always record real value changes
                    // 3. multi_op_mode is enabled - record every access for multi-op transactions
                    let accessed_in_op = self.op_snapshots_active
                        && self.op_entry_snapshots.contains_key(&ledger_key);
                    let should_record =
                        accessed_in_op || self.multi_op_mode || &entry != snapshot_entry;
                    if should_record {
                        let pre_state = if self.op_snapshots_active {
                            self.op_entry_snapshots
                                .get(&ledger_key)
                                .cloned()
                                .unwrap_or_else(|| self.account_to_ledger_entry(snapshot_entry))
                        } else {
                            self.account_to_ledger_entry(snapshot_entry)
                        };
                        self.set_last_modified_key(ledger_key, self.ledger_seq);
                        let post_state = self.account_to_ledger_entry(&entry);
                        self.delta.record_update(pre_state, post_state);
                        // Do NOT update account_snapshots here. The original pre-tx
                        // snapshot must be preserved for transaction-level rollback().
                        // Per-op STATE values are tracked via op_entry_snapshots.
                    }
                }
            }
        }
        self.modified_accounts = remaining;
    }

    /// Flush all pending account changes to the delta.
    ///
    /// This flushes only accounts, not trustlines or other entry types.
    /// Used when an operation needs to ensure all account changes are recorded
    /// before recording trustline changes (e.g., before deleting a trustline).
    pub fn flush_all_accounts(&mut self) {
        self.flush_all_accounts_except(None);
    }

    /// Flush a specific account's changes to the delta.
    ///
    /// Returns true if the account was in the modified list and was flushed.
    pub fn flush_account(&mut self, account_id: &AccountId) -> bool {
        let pos = self.modified_accounts.iter().position(|k| k == account_id);
        if let Some(pos) = pos {
            self.modified_accounts.remove(pos);
            if let Some(Some(snapshot_entry)) = self.account_snapshots.get(account_id) {
                if let Some(entry) = self.accounts.get(account_id).cloned() {
                    // For single-operation transactions, only record if entry actually changed.
                    // For multi-operation transactions, record for every access (even if no change)
                    // because stellar-core records per-operation entries for multi-op txs.
                    let should_record = self.multi_op_mode || &entry != snapshot_entry;
                    if should_record {
                        let pre_state = self.account_to_ledger_entry(snapshot_entry);
                        let ledger_key = LedgerKey::Account(LedgerKeyAccount {
                            account_id: account_id.clone(),
                        });
                        self.set_last_modified_key(ledger_key, self.ledger_seq);
                        let post_state = self.account_to_ledger_entry(&entry);
                        self.delta.record_update(pre_state, post_state);
                        // Do NOT update account_snapshots — preserve for rollback().
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Record updates for mutated entries into the delta and clear modification tracking.
    ///
    /// This is needed for operations that mutate entries through `get_*_mut`,
    /// which do not call `record_update` directly.
    ///
    /// For per-operation STATE values, we use op_entry_snapshots (captured at access time)
    /// rather than transaction-level snapshots. This allows correct per-op change recording
    /// while preserving transaction-level snapshots for rollback.
    pub fn flush_modified_entries(&mut self) {
        let modified_accounts = std::mem::take(&mut self.modified_accounts);
        for key in modified_accounts {
            if let Some(Some(snapshot_entry)) = self.account_snapshots.get(&key) {
                if let Some(entry) = self.accounts.get(&key).cloned() {
                    let ledger_key = LedgerKey::Account(LedgerKeyAccount {
                        account_id: entry.account_id.clone(),
                    });
                    let accessed_in_op = self.op_snapshots_active
                        && self.op_entry_snapshots.contains_key(&ledger_key);
                    if accessed_in_op || &entry != snapshot_entry {
                        let fallback = self.account_to_ledger_entry(snapshot_entry);
                        let post = self.account_to_ledger_entry(&entry);
                        self.record_flush_update(ledger_key, fallback, post);
                    }
                }
            }
        }

        let modified_trustlines = std::mem::take(&mut self.modified_trustlines);
        for key in modified_trustlines {
            if let Some(Some(snapshot_entry)) = self.trustline_snapshots.get(&key) {
                if let Some(entry) = self.trustlines.get(&key).cloned() {
                    let ledger_key = LedgerKey::Trustline(LedgerKeyTrustLine {
                        account_id: entry.account_id.clone(),
                        asset: entry.asset.clone(),
                    });
                    let accessed_in_op = self.op_snapshots_active
                        && self.op_entry_snapshots.contains_key(&ledger_key);
                    if accessed_in_op || &entry != snapshot_entry {
                        let fallback = self.trustline_to_ledger_entry(snapshot_entry);
                        let post = self.trustline_to_ledger_entry(&entry);
                        self.record_flush_update(ledger_key, fallback, post);
                    }
                }
            }
        }

        let modified_offers = std::mem::take(&mut self.modified_offers);
        let offer_updates: Vec<_> = {
            let store = self.offer_store_lock();
            modified_offers
                .into_iter()
                .filter_map(|key| {
                    let snapshot_record = self.offer_snapshots.get(&key)?.as_ref()?;
                    let record = store.get(&key)?;
                    let ledger_key = LedgerKey::Offer(LedgerKeyOffer {
                        seller_id: record.entry.seller_id.clone(),
                        offer_id: record.entry.offer_id,
                    });
                    let accessed_in_op = self.op_entry_snapshots.contains_key(&ledger_key);
                    if accessed_in_op || record.entry != snapshot_record.entry {
                        let fallback = snapshot_record.to_ledger_entry();
                        let post = record.to_ledger_entry();
                        Some((ledger_key, fallback, post))
                    } else {
                        None
                    }
                })
                .collect()
        };
        for (ledger_key, fallback, post) in offer_updates {
            self.record_flush_update(ledger_key, fallback, post);
        }

        let modified_data = self.data_entries.take_modified();
        for key in modified_data {
            if let Some(Some(snapshot_entry)) = self.data_entries.snapshot_value(&key) {
                if let Some(entry) = self.data_entries.get(&key).cloned() {
                    let ledger_key = LedgerKey::Data(LedgerKeyData {
                        account_id: entry.account_id.clone(),
                        data_name: entry.data_name.clone(),
                    });
                    let accessed_in_op = self.op_snapshots_active
                        && self.op_entry_snapshots.contains_key(&ledger_key);
                    if accessed_in_op || &entry != snapshot_entry {
                        let fallback = self.data_to_ledger_entry(snapshot_entry);
                        let post = self.data_to_ledger_entry(&entry);
                        self.record_flush_update(ledger_key, fallback, post);
                    }
                }
            }
        }

        let modified_contract_data = self.contract_data.take_modified();
        for key in modified_contract_data {
            if let Some(Some(snapshot_entry)) = self.contract_data.snapshot_value(&key) {
                if let Some(entry) = self.contract_data.get(&key).cloned() {
                    if &entry != snapshot_entry {
                        let ledger_key = LedgerKey::ContractData(LedgerKeyContractData {
                            contract: entry.contract.clone(),
                            key: entry.key.clone(),
                            durability: entry.durability,
                        });
                        let fallback = self.contract_data_to_ledger_entry(snapshot_entry);
                        let post = self.contract_data_to_ledger_entry(&entry);
                        self.record_flush_update(ledger_key, fallback, post);
                    }
                }
            }
        }

        let modified_contract_code = self.contract_code.take_modified();
        for key in modified_contract_code {
            if let Some(Some(snapshot_entry)) = self.contract_code.snapshot_value(&key) {
                if let Some(entry) = self.contract_code.get(&key).cloned() {
                    if &entry != snapshot_entry {
                        let ledger_key = LedgerKey::ContractCode(LedgerKeyContractCode {
                            hash: entry.hash.clone(),
                        });
                        let fallback = self.contract_code_to_ledger_entry(snapshot_entry);
                        let post = self.contract_code_to_ledger_entry(&entry);
                        self.record_flush_update(ledger_key, fallback, post);
                    }
                }
            }
        }

        let modified_ttl = std::mem::take(&mut self.modified_ttl);
        for key in modified_ttl {
            // Skip entries that were created in this transaction - they already have CREATED recorded
            // and should not get an additional STATE+UPDATED pair
            if self.created_ttl.contains(&key) {
                continue;
            }
            tracing::debug!(?key, "flush_modified_entries: processing TTL");
            if let Some(snapshot) = self.ttl_snapshots.get(&key).cloned() {
                tracing::debug!(
                    ?key,
                    has_snapshot_value = snapshot.is_some(),
                    "flush_modified_entries: TTL snapshot state"
                );
                if let Some(snapshot_entry) = snapshot {
                    if let Some(entry) = self.ttl_entries.get(&key).cloned() {
                        if entry != snapshot_entry {
                            let ledger_key = LedgerKey::Ttl(LedgerKeyTtl {
                                key_hash: entry.key_hash.clone(),
                            });
                            tracing::debug!(
                                ?key,
                                pre_live_until = snapshot_entry.live_until_ledger_seq,
                                post_live_until = entry.live_until_ledger_seq,
                                "flush_modified_entries: TTL record_update"
                            );
                            let fallback = self.ttl_to_ledger_entry(&snapshot_entry);
                            let post = self.ttl_to_ledger_entry(&entry);
                            self.record_flush_update(ledger_key, fallback, post);
                        }
                    }
                }
            }
        }

        let modified_claimable_balances = self.claimable_balances.take_modified();
        for key in modified_claimable_balances {
            if let Some(Some(snapshot_entry)) = self.claimable_balances.snapshot_value(&key) {
                if let Some(entry) = self.claimable_balances.get(&key).cloned() {
                    let ledger_key = LedgerKey::ClaimableBalance(LedgerKeyClaimableBalance {
                        balance_id: entry.balance_id.clone(),
                    });
                    let accessed_in_op = self.op_entry_snapshots.contains_key(&ledger_key);
                    if accessed_in_op || &entry != snapshot_entry {
                        let fallback = self.claimable_balance_to_ledger_entry(snapshot_entry);
                        let post = self.claimable_balance_to_ledger_entry(&entry);
                        self.record_flush_update(ledger_key, fallback, post);
                    }
                }
            }
        }

        let modified_liquidity_pools = self.liquidity_pools.take_modified();
        for key in modified_liquidity_pools {
            if let Some(Some(snapshot_entry)) = self.liquidity_pools.snapshot_value(&key) {
                if let Some(entry) = self.liquidity_pools.get(&key).cloned() {
                    let ledger_key = LedgerKey::LiquidityPool(LedgerKeyLiquidityPool {
                        liquidity_pool_id: entry.liquidity_pool_id.clone(),
                    });
                    let accessed_in_op = self.op_entry_snapshots.contains_key(&ledger_key);
                    if accessed_in_op || &entry != snapshot_entry {
                        let fallback = self.liquidity_pool_to_ledger_entry(snapshot_entry);
                        let post = self.liquidity_pool_to_ledger_entry(&entry);
                        self.record_flush_update(ledger_key, fallback, post);
                    }
                }
            }
        }
    }

    // ==================== Helper Methods ====================

    /// Resolve the pre-state for a modified entry during flush.
    ///
    /// If an op-level snapshot exists for this key, use it (captures per-op
    /// state correctly). Otherwise fall back to the provided transaction-level
    /// snapshot converted to a `LedgerEntry`.
    fn resolve_pre_state(&self, ledger_key: &LedgerKey, fallback: LedgerEntry) -> LedgerEntry {
        self.op_entry_snapshots
            .get(ledger_key)
            .cloned()
            .unwrap_or(fallback)
    }

    /// Record a delete using the snapshot pre-state (not the current mutated state).
    ///
    /// Must be called AFTER `capture_op_snapshot_for_key` and BEFORE removing
    /// the entry from the live map / clearing metadata. Prefers the per-op
    /// snapshot (`op_entry_snapshots`) over the per-tx snapshot
    /// (`snapshot_entry`), matching stellar-core's parent-scope semantics
    /// where `getChanges` emits `mParent.getNewestVersion(key)` for deleted
    /// entries (LedgerTxn.cpp:1356-1367).
    fn record_snapshot_delete(&mut self, key: &LedgerKey) {
        let pre_state = self
            .op_entry_snapshots
            .get(key)
            .cloned()
            .or_else(|| self.snapshot_entry(key));
        if let Some(pre) = pre_state {
            self.delta.record_delete(key.clone(), pre);
        }
    }

    /// Record a flush update: resolve pre-state, set last_modified, and record delta.
    ///
    /// Common tail shared by every entry-type block in `flush_modified_entries`.
    fn record_flush_update(
        &mut self,
        ledger_key: LedgerKey,
        fallback_pre: LedgerEntry,
        mut post_state: LedgerEntry,
    ) {
        let pre_state = self.resolve_pre_state(&ledger_key, fallback_pre);
        self.set_last_modified_key(ledger_key, self.ledger_seq);
        // Stamp post_state with the current ledger sequence, matching
        // stellar-core's maybeUpdateLastModified which sets
        // lastModifiedLedgerSeq on every entry committed via LedgerTxn.
        // The post_state was built before set_last_modified_key updated
        // the entry_last_modified map, so it still carries the old value.
        post_state.last_modified_ledger_seq = self.ledger_seq;
        self.delta.record_update(pre_state, post_state);
    }
}

// ==================== Helper Functions ====================

/// Convert a String64 data name to a String.
fn data_name_to_string(name: &stellar_xdr::curr::String64) -> String {
    String::from_utf8_lossy(name.as_vec()).to_string()
}

fn sponsorship_counts(account: &AccountEntry) -> (i64, i64) {
    match &account.ext {
        AccountEntryExt::V0 => (0, 0),
        AccountEntryExt::V1(v1) => match &v1.ext {
            AccountEntryExtensionV1Ext::V0 => (0, 0),
            AccountEntryExtensionV1Ext::V2(v2) => {
                (v2.num_sponsoring as i64, v2.num_sponsored as i64)
            }
        },
    }
}

fn sponsorship_from_entry_ext(entry: &LedgerEntry) -> Option<AccountId> {
    match &entry.ext {
        LedgerEntryExt::V0 => None,
        LedgerEntryExt::V1(v1) => v1.sponsoring_id.0.clone(),
    }
}

pub(crate) fn ensure_account_ext_v2(account: &mut AccountEntry) -> &mut AccountEntryExtensionV2 {
    let liabilities = match &account.ext {
        AccountEntryExt::V1(v1) => v1.liabilities.clone(),
        AccountEntryExt::V0 => Liabilities {
            buying: 0,
            selling: 0,
        },
    };

    match &account.ext {
        AccountEntryExt::V0 => {
            account.ext = AccountEntryExt::V1(AccountEntryExtensionV1 {
                liabilities,
                ext: AccountEntryExtensionV1Ext::V2(AccountEntryExtensionV2 {
                    num_sponsored: 0,
                    num_sponsoring: 0,
                    signer_sponsoring_i_ds: build_signer_sponsoring_ids(account.signers.len()),
                    ext: AccountEntryExtensionV2Ext::V0,
                }),
            });
        }
        AccountEntryExt::V1(v1) => {
            if matches!(v1.ext, AccountEntryExtensionV1Ext::V0) {
                account.ext = AccountEntryExt::V1(AccountEntryExtensionV1 {
                    liabilities,
                    ext: AccountEntryExtensionV1Ext::V2(AccountEntryExtensionV2 {
                        num_sponsored: 0,
                        num_sponsoring: 0,
                        signer_sponsoring_i_ds: build_signer_sponsoring_ids(account.signers.len()),
                        ext: AccountEntryExtensionV2Ext::V0,
                    }),
                });
            }
        }
    }

    if let AccountEntryExt::V1(v1) = &mut account.ext {
        if let AccountEntryExtensionV1Ext::V2(v2) = &mut v1.ext {
            return v2;
        }
    }

    unreachable!("account ext v2 should exist after ensure_account_ext_v2")
}

/// Update sequence metadata when an account's sequence number changes.
pub fn update_account_seq_info(account: &mut AccountEntry, ledger_seq: u32, close_time: u64) {
    let ext_v2 = ensure_account_ext_v2(account);
    let seq_time = TimePoint(close_time);
    match &mut ext_v2.ext {
        AccountEntryExtensionV2Ext::V0 => {
            ext_v2.ext = AccountEntryExtensionV2Ext::V3(AccountEntryExtensionV3 {
                ext: ExtensionPoint::V0,
                seq_ledger: ledger_seq,
                seq_time,
            });
        }
        AccountEntryExtensionV2Ext::V3(v3) => {
            v3.seq_ledger = ledger_seq;
            v3.seq_time = seq_time;
        }
    }
}

/// Get the sequence time from an account's extension V3.
/// Returns 0 if the account doesn't have extension V3.
pub fn get_account_seq_time(account: &AccountEntry) -> u64 {
    match &account.ext {
        AccountEntryExt::V0 => 0,
        AccountEntryExt::V1(ext_v1) => match &ext_v1.ext {
            AccountEntryExtensionV1Ext::V0 => 0,
            AccountEntryExtensionV1Ext::V2(ext_v2) => match &ext_v2.ext {
                AccountEntryExtensionV2Ext::V0 => 0,
                AccountEntryExtensionV2Ext::V3(ext_v3) => ext_v3.seq_time.0,
            },
        },
    }
}

/// Get the sequence ledger from an account's extension V3.
/// Returns 0 if the account doesn't have extension V3.
pub fn get_account_seq_ledger(account: &AccountEntry) -> u32 {
    match &account.ext {
        AccountEntryExt::V0 => 0,
        AccountEntryExt::V1(ext_v1) => match &ext_v1.ext {
            AccountEntryExtensionV1Ext::V0 => 0,
            AccountEntryExtensionV1Ext::V2(ext_v2) => match &ext_v2.ext {
                AccountEntryExtensionV2Ext::V0 => 0,
                AccountEntryExtensionV2Ext::V3(ext_v3) => ext_v3.seq_ledger,
            },
        },
    }
}

fn build_signer_sponsoring_ids(count: usize) -> VecM<SponsorshipDescriptor, 20> {
    let ids = vec![SponsorshipDescriptor(None); count];
    ids.try_into()
        .expect("account signers are bounded to 20 descriptors")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::create_test_account_id;
    use henyey_common::LIQUIDITY_POOL_FEE_V18;
    use stellar_xdr::curr::*;

    /// Create a LedgerStateManager with a shared OfferStore pre-configured.
    fn new_manager_with_offers(base_reserve: i64, ledger_seq: u32) -> LedgerStateManager {
        let mut manager = LedgerStateManager::new(base_reserve, ledger_seq);
        manager.set_offer_store(Arc::new(Mutex::new(OfferStore::new())));
        manager
    }

    fn create_test_account_entry(seed: u8) -> AccountEntry {
        AccountEntry {
            account_id: create_test_account_id(seed),
            balance: 1000000000,
            seq_num: SequenceNumber(1),
            num_sub_entries: 0,
            inflation_dest: None,
            flags: 0,
            home_domain: String32::default(),
            thresholds: Thresholds([1, 0, 0, 0]),
            signers: vec![].try_into().unwrap(),
            ext: AccountEntryExt::V0,
        }
    }

    #[test]
    fn test_state_manager_creation() {
        let manager = new_manager_with_offers(5_000_000, 100);
        assert_eq!(manager.ledger_seq(), 100);
        assert_eq!(manager.base_reserve(), 5_000_000);
        assert!(!manager.has_changes());
    }

    #[test]
    fn test_minimum_balance() {
        let manager = new_manager_with_offers(5_000_000, 100);
        let account = create_test_account_entry(1);
        // 0 sub-entries: (2 + 0) * 5_000_000 = 10_000_000
        assert_eq!(
            manager
                .minimum_balance_for_account(&account, 25, 0)
                .unwrap(),
            10_000_000
        );
        // 3 sub-entries: (2 + 3) * 5_000_000 = 25_000_000
        let mut account_with_subentries = account;
        account_with_subentries.num_sub_entries = 3;
        assert_eq!(
            manager
                .minimum_balance_for_account(&account_with_subentries, 25, 0)
                .unwrap(),
            25_000_000
        );
    }

    #[test]
    fn test_account_operations() {
        let mut manager = new_manager_with_offers(5_000_000, 100);
        let account = create_test_account_entry(1);
        let account_id = account.account_id.clone();

        // Create account
        manager.create_account(account.clone());
        assert!(manager.has_changes());
        assert!(manager.get_account(&account_id).is_some());

        // Update account
        let mut updated = account.clone();
        updated.balance = 2000000000;
        manager.update_account(updated);

        let stored = manager.get_account(&account_id).unwrap();
        assert_eq!(stored.balance, 2000000000);

        // Delete account
        manager.delete_account(&account_id);
        assert!(manager.get_account(&account_id).is_none());
    }

    #[test]
    fn test_rollback() {
        let mut manager = new_manager_with_offers(5_000_000, 100);
        let account = create_test_account_entry(1);
        let account_id = account.account_id.clone();

        // Create account
        manager.create_account(account.clone());
        assert!(manager.get_account(&account_id).is_some());

        // Rollback
        manager.rollback();
        assert!(manager.get_account(&account_id).is_none());
        assert!(!manager.has_changes());
    }

    #[test]
    fn test_rollback_preserves_fee_charged() {
        let mut manager = new_manager_with_offers(5_000_000, 100);

        // Add fee from first transaction
        manager.delta_mut().add_fee(400);
        assert_eq!(manager.delta().fee_charged(), 400);

        // Create an account (simulating transaction changes)
        let account1 = create_test_account_entry(1);
        manager.create_account(account1.clone());

        // Rollback first transaction (simulating failed tx)
        manager.rollback();
        // After rollback, fee should be preserved
        assert_eq!(
            manager.delta().fee_charged(),
            400,
            "Fee from first tx should be preserved after rollback"
        );

        // Add fee from second transaction
        manager.delta_mut().add_fee(100);
        assert_eq!(manager.delta().fee_charged(), 500, "Fee should accumulate");

        // Create another account
        let account2 = create_test_account_entry(2);
        manager.create_account(account2.clone());

        // Rollback second transaction
        manager.rollback();
        // After rollback, fees from both transactions should be preserved
        assert_eq!(
            manager.delta().fee_charged(),
            500,
            "Fees from both failed txs should be preserved"
        );

        // Add fee from third transaction (successful)
        manager.delta_mut().add_fee(100);
        assert_eq!(
            manager.delta().fee_charged(),
            600,
            "Total fees should be 600"
        );

        // Commit third transaction
        manager.commit();
        // Commit doesn't reset fee_charged
        assert_eq!(
            manager.delta().fee_charged(),
            600,
            "Total fees should remain after commit"
        );
    }

    #[test]
    fn test_commit() {
        let mut manager = new_manager_with_offers(5_000_000, 100);
        let account = create_test_account_entry(1);
        let account_id = account.account_id.clone();

        // Create account
        manager.create_account(account.clone());

        // Commit
        manager.commit();

        // Account should still exist
        assert!(manager.get_account(&account_id).is_some());

        // But snapshots should be cleared (can't rollback anymore)
        manager.rollback();
        assert!(manager.get_account(&account_id).is_some()); // Still there because commit cleared snapshots
    }

    #[test]
    fn test_take_delta() {
        let mut manager = new_manager_with_offers(5_000_000, 100);
        let account = create_test_account_entry(1);

        manager.create_account(account);

        let delta = manager.take_delta();
        assert_eq!(delta.ledger_seq(), 100);
        assert!(delta.has_changes());
        assert_eq!(delta.created_entries().len(), 1);
    }

    #[test]
    fn test_asset_to_trustline_asset() {
        let native_key = asset_to_trustline_asset(&Asset::Native);
        assert!(matches!(native_key, TrustLineAsset::Native));

        let alphanum4 = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
            issuer: create_test_account_id(1),
        });
        let key4 = asset_to_trustline_asset(&alphanum4);
        assert!(matches!(key4, TrustLineAsset::CreditAlphanum4(_)));
    }

    /// Test that ClaimableBalance sponsorship-only changes are recorded in delta.
    /// Regression test for ledger 80382 where RevokeSponsorship changed only the
    /// sponsor of a ClaimableBalance entry but the modification was not recorded.
    #[test]
    fn test_claimable_balance_sponsorship_only_change_recorded_in_delta() {
        let mut manager = new_manager_with_offers(5_000_000, 100);

        // Create a claimable balance ID
        let balance_id = ClaimableBalanceId::ClaimableBalanceIdTypeV0(Hash([1u8; 32]));

        // Create a claimable balance entry
        let cb_entry = ClaimableBalanceEntry {
            balance_id: balance_id.clone(),
            claimants: vec![Claimant::ClaimantTypeV0(ClaimantV0 {
                destination: create_test_account_id(1),
                predicate: ClaimPredicate::Unconditional,
            })]
            .try_into()
            .unwrap(),
            asset: Asset::Native,
            amount: 1000000,
            ext: ClaimableBalanceEntryExt::V0,
        };

        // Create the claimable balance and set up initial sponsor
        manager.create_claimable_balance(cb_entry.clone());
        let ledger_key = LedgerKey::ClaimableBalance(LedgerKeyClaimableBalance {
            balance_id: balance_id.clone(),
        });
        let initial_sponsor = create_test_account_id(2);
        manager.set_entry_sponsor(ledger_key.clone(), initial_sponsor);
        manager.commit();

        // Reset delta by creating a new manager with the same state
        // (simulating the start of a new transaction)
        manager.delta = TxChangeLog::new(100);

        // Now simulate what happens during RevokeSponsorship:
        // 1. Start operation snapshot mode
        manager.begin_op_snapshot();

        // 2. Access the claimable balance via get_claimable_balance_mut
        //    This puts it into op_entry_snapshots
        let _ = manager.get_claimable_balance_mut(&balance_id);

        // 3. Change the sponsor (but not the entry data itself)
        let new_sponsor = create_test_account_id(3);
        manager.set_entry_sponsor(ledger_key.clone(), new_sponsor);

        // 4. Apply modifications to delta (before end_op_snapshot, as in real execution)
        manager.flush_modified_entries();

        // 5. End operation snapshot
        let _ = manager.end_op_snapshot();

        // The claimable balance should be recorded in delta as updated
        // even though only the sponsor changed (not the entry data)
        let delta = manager.take_delta();

        // Check that an update was recorded
        assert!(
            !delta.updated_entries().is_empty(),
            "ClaimableBalance sponsorship-only change should be recorded in delta"
        );

        // Verify it's the claimable balance that was updated
        let has_cb_update = delta.updated_entries().iter().any(|entry| {
            matches!(
                &entry.data,
                LedgerEntryData::ClaimableBalance(cb) if cb.balance_id == balance_id
            )
        });
        assert!(
            has_cb_update,
            "The updated entry should be the ClaimableBalance"
        );
    }

    /// Test that LiquidityPool sponsorship-only changes are recorded in delta.
    /// Same fix as ClaimableBalance, applied for consistency.
    #[test]
    fn test_liquidity_pool_sponsorship_only_change_recorded_in_delta() {
        let mut manager = new_manager_with_offers(5_000_000, 100);

        // Create a liquidity pool ID
        let pool_id = PoolId(Hash([2u8; 32]));

        // Create a liquidity pool entry
        let lp_entry = LiquidityPoolEntry {
            liquidity_pool_id: pool_id.clone(),
            body: LiquidityPoolEntryBody::LiquidityPoolConstantProduct(
                LiquidityPoolEntryConstantProduct {
                    params: LiquidityPoolConstantProductParameters {
                        asset_a: Asset::Native,
                        asset_b: Asset::CreditAlphanum4(AlphaNum4 {
                            asset_code: AssetCode4([b'U', b'S', b'D', 0]),
                            issuer: create_test_account_id(1),
                        }),
                        fee: LIQUIDITY_POOL_FEE_V18,
                    },
                    reserve_a: 1000000,
                    reserve_b: 1000000,
                    total_pool_shares: 1000000,
                    pool_shares_trust_line_count: 1,
                },
            ),
        };

        // Create the liquidity pool and set up initial sponsor
        manager.create_liquidity_pool(lp_entry.clone());
        let ledger_key = LedgerKey::LiquidityPool(LedgerKeyLiquidityPool {
            liquidity_pool_id: pool_id.clone(),
        });
        let initial_sponsor = create_test_account_id(2);
        manager.set_entry_sponsor(ledger_key.clone(), initial_sponsor);
        manager.commit();

        // Reset delta by creating a new one (simulating the start of a new transaction)
        manager.delta = TxChangeLog::new(100);

        // Now simulate what happens during RevokeSponsorship:
        // 1. Start operation snapshot mode
        manager.begin_op_snapshot();

        // 2. Access the liquidity pool via get_liquidity_pool_mut
        //    This puts it into op_entry_snapshots
        let _ = manager.get_liquidity_pool_mut(&pool_id);

        // 3. Change the sponsor (but not the entry data itself)
        let new_sponsor = create_test_account_id(3);
        manager.set_entry_sponsor(ledger_key.clone(), new_sponsor);

        // 4. Apply modifications to delta (before end_op_snapshot, as in real execution)
        manager.flush_modified_entries();

        // 5. End operation snapshot
        let _ = manager.end_op_snapshot();

        // The liquidity pool should be recorded in delta as updated
        // even though only the sponsor changed (not the entry data)
        let delta = manager.take_delta();

        // Check that an update was recorded
        assert!(
            !delta.updated_entries().is_empty(),
            "LiquidityPool sponsorship-only change should be recorded in delta"
        );

        // Verify it's the liquidity pool that was updated
        let has_lp_update = delta.updated_entries().iter().any(|entry| {
            matches!(
                &entry.data,
                LedgerEntryData::LiquidityPool(lp) if lp.liquidity_pool_id == pool_id
            )
        });
        assert!(
            has_lp_update,
            "The updated entry should be the LiquidityPool"
        );
    }

    /// Test that Offer sponsorship-only changes are recorded in delta.
    /// Regression test for ledger 80387 where RevokeSponsorship changed only the
    /// sponsor of an Offer entry but the modification was not recorded, causing
    /// bucket list hash mismatch and a num_sponsoring underflow at ledger 80388.
    #[test]
    fn test_offer_sponsorship_only_change_recorded_in_delta() {
        let mut manager = new_manager_with_offers(5_000_000, 100);

        // Create an offer entry
        let seller_id = create_test_account_id(1);
        let offer_entry = OfferEntry {
            seller_id: seller_id.clone(),
            offer_id: 1254, // Same offer ID as in the real testnet case
            selling: Asset::Native,
            buying: Asset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'X', b'X', b'X', b'Y']),
                issuer: create_test_account_id(2),
            }),
            amount: 1000000,
            price: Price { n: 1, d: 1 },
            flags: 0,
            ext: OfferEntryExt::V0,
        };

        // Create the offer and set up initial sponsor
        manager.create_offer(offer_entry.clone());
        let ledger_key = LedgerKey::Offer(LedgerKeyOffer {
            seller_id: seller_id.clone(),
            offer_id: 1254,
        });
        let initial_sponsor = create_test_account_id(2);
        manager.set_entry_sponsor(ledger_key.clone(), initial_sponsor);
        manager.commit();

        // Reset delta by creating a new manager with the same state
        // (simulating the start of a new transaction)
        manager.delta = TxChangeLog::new(100);

        // Now simulate what happens during RevokeSponsorship:
        // 1. Start operation snapshot mode
        manager.begin_op_snapshot();

        // 2. Access the offer via get_offer_mut
        //    This puts it into op_entry_snapshots
        let _ = manager.get_offer_mut(&seller_id, 1254);

        // 3. Change the sponsor (but not the entry data itself)
        let new_sponsor = create_test_account_id(3);
        manager.set_entry_sponsor(ledger_key.clone(), new_sponsor);

        // 4. Apply modifications to delta (before end_op_snapshot, as in real execution)
        manager.flush_modified_entries();

        // 5. End operation snapshot
        let _ = manager.end_op_snapshot();

        // The offer should be recorded in delta as updated
        // even though only the sponsor changed (not the entry data)
        let delta = manager.take_delta();

        // Check that an update was recorded
        assert!(
            !delta.updated_entries().is_empty(),
            "Offer sponsorship-only change should be recorded in delta"
        );

        // Verify it's the offer that was updated
        let has_offer_update = delta.updated_entries().iter().any(|entry| {
            matches!(
                &entry.data,
                LedgerEntryData::Offer(offer) if offer.offer_id == 1254
            )
        });
        assert!(has_offer_update, "The updated entry should be the Offer");
    }

    /// Test that Data-entry sponsorship-only changes are recorded in delta and
    /// stamped with the current ledger sequence.
    #[test]
    fn test_data_sponsorship_only_change_recorded_in_delta() {
        let mut manager = new_manager_with_offers(5_000_000, 100);

        let owner_id = create_test_account_id(1);
        manager.create_account(create_test_account_entry(1));

        let data_name = String64::try_from(b"test".to_vec()).unwrap();
        let data_entry = DataEntry {
            account_id: owner_id.clone(),
            data_name: data_name.clone(),
            data_value: vec![1, 2, 3].try_into().unwrap(),
            ext: DataEntryExt::V0,
        };

        manager.create_data(data_entry);
        let ledger_key = LedgerKey::Data(LedgerKeyData {
            account_id: owner_id.clone(),
            data_name: data_name.clone(),
        });
        let initial_sponsor = create_test_account_id(2);
        manager.set_entry_sponsor(ledger_key.clone(), initial_sponsor);
        manager.commit();

        let new_ledger = 200;
        manager.set_ledger_seq(new_ledger);
        manager.delta = TxChangeLog::new(new_ledger);

        manager.begin_op_snapshot();

        let _ = manager.get_data_mut(&owner_id, "test");

        let new_sponsor = create_test_account_id(3);
        manager.set_entry_sponsor(ledger_key.clone(), new_sponsor.clone());

        manager.flush_modified_entries();
        let _ = manager.end_op_snapshot();

        let delta = manager.take_delta();
        let updated = delta
            .updated_entries()
            .iter()
            .find(|entry| matches!(&entry.data, LedgerEntryData::Data(data) if data.account_id == owner_id))
            .expect("data entry should be recorded as updated");

        assert_eq!(updated.last_modified_ledger_seq, new_ledger);
        assert!(matches!(
            &updated.ext,
            LedgerEntryExt::V1(v1) if v1.sponsoring_id.0 == Some(new_sponsor)
        ));
    }

    // ==================== OfferIndex Tests ====================

    fn create_test_offer(seller_seed: u8, offer_id: i64, price_n: i32, price_d: i32) -> OfferEntry {
        OfferEntry {
            seller_id: create_test_account_id(seller_seed),
            offer_id,
            selling: Asset::Native,
            buying: Asset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', 0]),
                issuer: create_test_account_id(99),
            }),
            amount: 1000000,
            price: Price {
                n: price_n,
                d: price_d,
            },
            flags: 0,
            ext: OfferEntryExt::V0,
        }
    }

    #[test]
    fn test_offer_index_add_and_best_offer() {
        let mut index = OfferIndex::new();

        // Add offers with different prices
        let offer1 = create_test_offer(1, 100, 3, 1); // price = 3.0
        let offer2 = create_test_offer(2, 200, 1, 1); // price = 1.0 (best)
        let offer3 = create_test_offer(3, 300, 2, 1); // price = 2.0

        index.add_offer(&offer1);
        index.add_offer(&offer2);
        index.add_offer(&offer3);

        assert_eq!(index.len(), 3);

        // Best offer should be the one with lowest price (offer2)
        let best_key = index
            .best_offer_key(&offer1.buying, &offer1.selling)
            .unwrap();
        assert_eq!(best_key.offer_id, 200);
    }

    #[test]
    fn test_offer_index_best_offer_same_price_uses_offer_id() {
        let mut index = OfferIndex::new();

        // Add offers with same price, different offer IDs
        let offer1 = create_test_offer(1, 300, 1, 1); // price = 1.0, id = 300
        let offer2 = create_test_offer(2, 100, 1, 1); // price = 1.0, id = 100 (best)
        let offer3 = create_test_offer(3, 200, 1, 1); // price = 1.0, id = 200

        index.add_offer(&offer1);
        index.add_offer(&offer2);
        index.add_offer(&offer3);

        // Best offer should be the one with lowest offer ID (offer2)
        let best_key = index
            .best_offer_key(&offer1.buying, &offer1.selling)
            .unwrap();
        assert_eq!(best_key.offer_id, 100);
    }

    #[test]
    fn test_offer_index_remove_offer() {
        let mut index = OfferIndex::new();

        let offer1 = create_test_offer(1, 100, 2, 1); // price = 2.0
        let offer2 = create_test_offer(2, 200, 1, 1); // price = 1.0 (best)

        index.add_offer(&offer1);
        index.add_offer(&offer2);

        // Remove best offer
        index.remove_with_data(&offer2);

        assert_eq!(index.len(), 1);

        // Now offer1 should be best
        let best_key = index
            .best_offer_key(&offer1.buying, &offer1.selling)
            .unwrap();
        assert_eq!(best_key.offer_id, 100);
    }

    #[test]
    fn test_offer_index_update_offer() {
        let mut index = OfferIndex::new();

        let offer1 = create_test_offer(1, 100, 2, 1); // price = 2.0
        index.add_offer(&offer1);

        // Update to better price
        let mut updated_offer = offer1.clone();
        updated_offer.price = Price { n: 1, d: 2 }; // price = 0.5
        index.update_offer(&offer1, &updated_offer);

        assert_eq!(index.len(), 1);

        // Verify the offer is still there with updated price
        let best_key = index
            .best_offer_key(&offer1.buying, &offer1.selling)
            .unwrap();
        assert_eq!(best_key.offer_id, 100);
    }

    #[test]
    fn test_offer_index_different_asset_pairs() {
        let mut index = OfferIndex::new();

        let offer1 = create_test_offer(1, 100, 1, 1);

        // Create offer for different asset pair
        let mut offer2 = create_test_offer(2, 200, 1, 1);
        offer2.buying = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'E', b'U', b'R', 0]),
            issuer: create_test_account_id(98),
        });

        index.add_offer(&offer1);
        index.add_offer(&offer2);

        assert_eq!(index.len(), 2);
        assert_eq!(index.num_asset_pairs(), 2);

        // Each asset pair should have its own best offer
        let best1 = index
            .best_offer_key(&offer1.buying, &offer1.selling)
            .unwrap();
        assert_eq!(best1.offer_id, 100);

        let best2 = index
            .best_offer_key(&offer2.buying, &offer2.selling)
            .unwrap();
        assert_eq!(best2.offer_id, 200);
    }

    #[test]
    fn test_offer_index_empty() {
        let index = OfferIndex::new();

        let buying = Asset::Native;
        let selling = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', b'S', b'D', 0]),
            issuer: create_test_account_id(99),
        });

        assert!(index.is_empty());
        assert!(!index.has_offers(&buying, &selling));
        assert!(index.best_offer_key(&buying, &selling).is_none());
    }

    #[test]
    fn test_offer_index_update_across_asset_pairs() {
        let mut index = OfferIndex::new();

        let offer = create_test_offer(1, 100, 1, 1);
        index.add_offer(&offer);
        assert_eq!(index.num_asset_pairs(), 1);
        assert_eq!(index.len(), 1);

        // Update to a different asset pair
        let mut updated = offer.clone();
        updated.buying = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'E', b'U', b'R', 0]),
            issuer: create_test_account_id(98),
        });
        index.update_offer(&offer, &updated);

        assert_eq!(index.len(), 1);
        assert_eq!(index.num_asset_pairs(), 1);

        // Old pair should have no offers
        assert!(!index.has_offers(&offer.buying, &offer.selling));
        // New pair should have the offer
        assert!(index.has_offers(&updated.buying, &updated.selling));
    }

    #[test]
    fn test_offer_index_remove_last_cleans_order_book() {
        let mut index = OfferIndex::new();

        let offer = create_test_offer(1, 100, 1, 1);
        index.add_offer(&offer);
        assert_eq!(index.num_asset_pairs(), 1);

        index.remove_with_data(&offer);
        assert_eq!(index.len(), 0);
        assert_eq!(index.num_asset_pairs(), 0);
        assert!(index.is_empty());
    }

    #[test]
    fn test_offer_index_len_correct_across_operations() {
        let mut index = OfferIndex::new();
        assert_eq!(index.len(), 0);

        // Insert 3 offers
        let o1 = create_test_offer(1, 100, 1, 1);
        let o2 = create_test_offer(2, 200, 2, 1);
        let o3 = create_test_offer(3, 300, 3, 1);
        index.add_offer(&o1);
        index.add_offer(&o2);
        index.add_offer(&o3);
        assert_eq!(index.len(), 3);

        // Update one (count stays the same)
        let mut o2_updated = o2.clone();
        o2_updated.price = Price { n: 1, d: 2 };
        index.update_offer(&o2, &o2_updated);
        assert_eq!(index.len(), 3);

        // Remove one
        index.remove_with_data(&o1);
        assert_eq!(index.len(), 2);

        // Remove another
        index.remove_with_data(&o3);
        assert_eq!(index.len(), 1);

        // Remove last
        index.remove_with_data(&o2_updated);
        assert_eq!(index.len(), 0);
        assert!(index.is_empty());
    }

    #[test]
    fn test_state_manager_best_offer_uses_index() {
        let mut manager = new_manager_with_offers(5_000_000, 100);

        // Create offers with different prices
        let offer1 = create_test_offer(1, 100, 3, 1); // price = 3.0
        let offer2 = create_test_offer(2, 200, 1, 1); // price = 1.0 (best)
        let offer3 = create_test_offer(3, 300, 2, 1); // price = 2.0

        manager.create_offer(offer1.clone());
        manager.create_offer(offer2.clone());
        manager.create_offer(offer3.clone());

        // Verify index stats
        assert_eq!(manager.offer_index_size(), 3);

        // Best offer should use the index
        let best = manager.best_offer(&offer1.buying, &offer1.selling).unwrap();
        assert_eq!(best.offer_id, 200);
    }

    #[test]
    fn test_state_manager_best_offer_filtered() {
        let mut manager = new_manager_with_offers(5_000_000, 100);

        let seller1 = create_test_account_id(1);
        let seller2 = create_test_account_id(2);

        // Create offers
        let mut offer1 = create_test_offer(1, 100, 1, 1); // price = 1.0 (best but excluded)
        offer1.seller_id = seller1.clone();
        let mut offer2 = create_test_offer(2, 200, 2, 1); // price = 2.0 (second best)
        offer2.seller_id = seller2.clone();

        manager.create_offer(offer1.clone());
        manager.create_offer(offer2.clone());

        // Get best offer excluding seller1
        let best = manager
            .best_offer_filtered(&offer1.buying, &offer1.selling, |o| o.seller_id != seller1)
            .unwrap();

        assert_eq!(best.offer_id, 200);
    }

    #[test]
    fn test_state_manager_offer_index_rollback() {
        let mut manager = new_manager_with_offers(5_000_000, 100);

        let offer1 = create_test_offer(1, 100, 1, 1);
        manager.create_offer(offer1.clone());
        assert_eq!(manager.offer_index_size(), 1);

        // Rollback should restore the index
        manager.rollback();
        assert_eq!(manager.offer_index_size(), 0);
        assert!(manager
            .best_offer(&offer1.buying, &offer1.selling)
            .is_none());
    }

    #[test]
    fn test_state_manager_offer_index_delete() {
        let mut manager = new_manager_with_offers(5_000_000, 100);

        let offer1 = create_test_offer(1, 100, 2, 1);
        let offer2 = create_test_offer(2, 200, 1, 1); // best

        manager.create_offer(offer1.clone());
        manager.create_offer(offer2.clone());

        // Delete best offer
        manager.delete_offer(&offer2.seller_id, offer2.offer_id);

        assert_eq!(manager.offer_index_size(), 1);

        // Now offer1 should be best
        let best = manager.best_offer(&offer1.buying, &offer1.selling).unwrap();
        assert_eq!(best.offer_id, 100);
    }

    // ==================== Account-Asset Secondary Index Tests ====================

    fn usd_asset() -> Asset {
        Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', b'S', b'D', 0]),
            issuer: create_test_account_id(99),
        })
    }

    fn eur_asset() -> Asset {
        Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'E', b'U', b'R', 0]),
            issuer: create_test_account_id(98),
        })
    }

    fn create_test_offer_with_assets(
        seller_seed: u8,
        offer_id: i64,
        selling: Asset,
        buying: Asset,
    ) -> OfferEntry {
        OfferEntry {
            seller_id: create_test_account_id(seller_seed),
            offer_id,
            selling,
            buying,
            amount: 1000000,
            price: Price { n: 1, d: 1 },
            flags: 0,
            ext: OfferEntryExt::V0,
        }
    }

    /// Helper to query the OfferStore for offers matching a (seller, asset) pair.
    fn aa_index_get(manager: &LedgerStateManager, seller_seed: u8, asset: &Asset) -> HashSet<i64> {
        let seller = create_test_account_id(seller_seed);
        manager
            .get_offers_by_account_and_asset(&seller, asset)
            .into_iter()
            .map(|e| e.offer_id)
            .collect()
    }

    #[test]
    fn test_account_asset_index_create_offer() {
        let mut manager = new_manager_with_offers(5_000_000, 100);

        // 2 offers for seller_1 (Native→USD)
        let offer1 = create_test_offer_with_assets(1, 100, Asset::Native, usd_asset());
        let offer2 = create_test_offer_with_assets(1, 200, Asset::Native, usd_asset());
        // 1 offer for seller_2 (Native→USD)
        let offer3 = create_test_offer_with_assets(2, 300, Asset::Native, usd_asset());

        manager.create_offer(offer1);
        manager.create_offer(offer2);
        manager.create_offer(offer3);

        // seller_1's Native key should have both offer IDs
        let s1_native = aa_index_get(&manager, 1, &Asset::Native);
        assert_eq!(s1_native, HashSet::from([100, 200]));

        // seller_1's USD key should have both offer IDs
        let s1_usd = aa_index_get(&manager, 1, &usd_asset());
        assert_eq!(s1_usd, HashSet::from([100, 200]));

        // seller_2's keys should have just one
        let s2_native = aa_index_get(&manager, 2, &Asset::Native);
        assert_eq!(s2_native, HashSet::from([300]));

        let s2_usd = aa_index_get(&manager, 2, &usd_asset());
        assert_eq!(s2_usd, HashSet::from([300]));

        // Verify offers are in the store (4 unique (account, asset) pairs)
        // The OfferStore maintains this internally; we verify via queries above.
    }

    #[test]
    fn test_account_asset_index_multi_asset() {
        let mut manager = new_manager_with_offers(5_000_000, 100);

        let offer1 = create_test_offer_with_assets(1, 100, Asset::Native, usd_asset());
        let offer2 = create_test_offer_with_assets(1, 200, Asset::Native, eur_asset());

        manager.create_offer(offer1);
        manager.create_offer(offer2);

        // Both offers sell Native, so (seller_1, Native) should have both
        let s1_native = aa_index_get(&manager, 1, &Asset::Native);
        assert_eq!(s1_native, HashSet::from([100, 200]));

        // Each buying asset only has its own offer
        let s1_usd = aa_index_get(&manager, 1, &usd_asset());
        assert_eq!(s1_usd, HashSet::from([100]));

        let s1_eur = aa_index_get(&manager, 1, &eur_asset());
        assert_eq!(s1_eur, HashSet::from([200]));
    }

    #[test]
    fn test_account_asset_index_delete_offer() {
        let mut manager = new_manager_with_offers(5_000_000, 100);

        let offer1 = create_test_offer_with_assets(1, 100, Asset::Native, usd_asset());
        let offer2 = create_test_offer_with_assets(1, 200, Asset::Native, usd_asset());
        let seller1 = create_test_account_id(1);

        manager.create_offer(offer1);
        manager.create_offer(offer2);

        // Delete offer1
        manager.delete_offer(&seller1, 100);

        // offer1 should be removed from both keys
        let s1_native = aa_index_get(&manager, 1, &Asset::Native);
        assert_eq!(s1_native, HashSet::from([200]));

        let s1_usd = aa_index_get(&manager, 1, &usd_asset());
        assert_eq!(s1_usd, HashSet::from([200]));
    }

    #[test]
    fn test_account_asset_index_update_offer() {
        let mut manager = new_manager_with_offers(5_000_000, 100);

        let offer = create_test_offer_with_assets(1, 100, Asset::Native, usd_asset());
        manager.create_offer(offer);

        // Verify initial state
        assert_eq!(
            aa_index_get(&manager, 1, &usd_asset()),
            HashSet::from([100])
        );
        assert_eq!(aa_index_get(&manager, 1, &eur_asset()), HashSet::new());

        // Update offer to change buying asset from USD to EUR
        let updated = create_test_offer_with_assets(1, 100, Asset::Native, eur_asset());
        manager.update_offer(updated);

        // offer_id should be removed from USD and added to EUR
        assert_eq!(aa_index_get(&manager, 1, &usd_asset()), HashSet::new());
        assert_eq!(
            aa_index_get(&manager, 1, &eur_asset()),
            HashSet::from([100])
        );

        // Native should still have the offer
        assert_eq!(
            aa_index_get(&manager, 1, &Asset::Native),
            HashSet::from([100])
        );
    }

    #[test]
    fn test_remove_offers_by_account_and_asset() {
        let mut manager = new_manager_with_offers(5_000_000, 100);

        // offer1: Native→USD, offer2: Native→EUR, offer3: EUR→USD
        let offer1 = create_test_offer_with_assets(1, 100, Asset::Native, usd_asset());
        let offer2 = create_test_offer_with_assets(1, 200, Asset::Native, eur_asset());
        let offer3 = create_test_offer_with_assets(1, 300, eur_asset(), usd_asset());
        let seller1 = create_test_account_id(1);

        manager.create_offer(offer1);
        manager.create_offer(offer2);
        manager.create_offer(offer3);

        // Remove all offers that touch USD for seller_1
        let removed = manager.remove_offers_by_account_and_asset(&seller1, &usd_asset());

        // Should return offer1 (Native→USD) and offer3 (EUR→USD)
        let removed_ids: HashSet<i64> = removed.iter().map(|o| o.offer_id).collect();
        assert_eq!(removed_ids, HashSet::from([100, 300]));

        // offer2 (Native→EUR) should remain
        let s1_native = aa_index_get(&manager, 1, &Asset::Native);
        assert_eq!(s1_native, HashSet::from([200]));

        let s1_eur = aa_index_get(&manager, 1, &eur_asset());
        assert_eq!(s1_eur, HashSet::from([200]));

        // USD index should be empty
        let s1_usd = aa_index_get(&manager, 1, &usd_asset());
        assert!(s1_usd.is_empty());
    }

    #[test]
    fn test_remove_offers_by_account_and_asset_isolation() {
        let mut manager = new_manager_with_offers(5_000_000, 100);

        let offer1 = create_test_offer_with_assets(1, 100, Asset::Native, usd_asset());
        let offer2 = create_test_offer_with_assets(2, 200, Asset::Native, usd_asset());
        let seller1 = create_test_account_id(1);

        manager.create_offer(offer1);
        manager.create_offer(offer2);

        // Remove only seller_1's offers touching USD
        let removed = manager.remove_offers_by_account_and_asset(&seller1, &usd_asset());
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].offer_id, 100);

        // seller_2's offer should be untouched
        let s2_native = aa_index_get(&manager, 2, &Asset::Native);
        assert_eq!(s2_native, HashSet::from([200]));

        let s2_usd = aa_index_get(&manager, 2, &usd_asset());
        assert_eq!(s2_usd, HashSet::from([200]));

        // seller_1's index should be empty
        let s1_native = aa_index_get(&manager, 1, &Asset::Native);
        assert!(s1_native.is_empty());
    }

    #[test]
    fn test_account_asset_index_rollback() {
        let mut manager = new_manager_with_offers(5_000_000, 100);

        let offer = create_test_offer_with_assets(1, 100, Asset::Native, usd_asset());
        manager.create_offer(offer);

        // Index should be populated
        assert_eq!(
            aa_index_get(&manager, 1, &Asset::Native),
            HashSet::from([100])
        );
        assert_eq!(
            aa_index_get(&manager, 1, &usd_asset()),
            HashSet::from([100])
        );

        // Rollback should remove the offer (it was created in this tx)
        manager.rollback();

        assert!(aa_index_get(&manager, 1, &Asset::Native).is_empty());
        assert!(aa_index_get(&manager, 1, &usd_asset()).is_empty());
    }

    #[test]
    fn test_account_asset_index_rollback_restore() {
        let mut manager = new_manager_with_offers(5_000_000, 100);

        // Create and commit offer1
        let offer1 = create_test_offer_with_assets(1, 100, Asset::Native, usd_asset());
        manager.create_offer(offer1);
        manager.commit();

        // Create offer2 in a new transaction, then rollback
        let offer2 = create_test_offer_with_assets(1, 200, Asset::Native, usd_asset());
        manager.create_offer(offer2);

        // Both should be in the index before rollback
        assert_eq!(
            aa_index_get(&manager, 1, &Asset::Native),
            HashSet::from([100, 200])
        );

        manager.rollback();

        // After rollback, only offer1 should remain
        assert_eq!(
            aa_index_get(&manager, 1, &Asset::Native),
            HashSet::from([100])
        );
        assert_eq!(
            aa_index_get(&manager, 1, &usd_asset()),
            HashSet::from([100])
        );
    }

    #[test]
    fn test_account_asset_index_savepoint() {
        let mut manager = new_manager_with_offers(5_000_000, 100);

        // Create and commit offer1
        let offer1 = create_test_offer_with_assets(1, 100, Asset::Native, usd_asset());
        manager.create_offer(offer1);
        manager.commit();

        // Create savepoint
        let sp = manager.create_savepoint();

        // Create offer2 after savepoint
        let offer2 = create_test_offer_with_assets(1, 200, Asset::Native, usd_asset());
        manager.create_offer(offer2);

        // Both should be in the index
        assert_eq!(
            aa_index_get(&manager, 1, &Asset::Native),
            HashSet::from([100, 200])
        );

        // Rollback to savepoint
        manager.rollback_to_savepoint(sp);

        // Only offer1 should remain
        assert_eq!(
            aa_index_get(&manager, 1, &Asset::Native),
            HashSet::from([100])
        );
        assert_eq!(
            aa_index_get(&manager, 1, &usd_asset()),
            HashSet::from([100])
        );
    }

    #[test]
    fn test_savepoint_rollback_accounts() {
        let mut manager = new_manager_with_offers(5_000_000, 100);

        // Create and commit an account
        let account = create_test_account_entry(1);
        let account_id = account.account_id.clone();
        manager.create_account(account);
        manager.commit();

        // Modify account balance
        if let Some(acc) = manager.get_account_mut(&account_id) {
            acc.balance = 500_000_000;
        }

        // Create savepoint after first modification
        let sp = manager.create_savepoint();

        // Modify account again after savepoint
        if let Some(acc) = manager.get_account_mut(&account_id) {
            acc.balance = 100_000;
        }
        assert_eq!(manager.get_account(&account_id).unwrap().balance, 100_000);

        // Rollback — should restore to pre-savepoint value
        manager.rollback_to_savepoint(sp);
        assert_eq!(
            manager.get_account(&account_id).unwrap().balance,
            500_000_000
        );
    }

    #[test]
    fn test_savepoint_rollback_data_entries() {
        let mut manager = new_manager_with_offers(5_000_000, 100);

        // Create savepoint on empty state
        let sp = manager.create_savepoint();

        // Create a data entry after savepoint
        let data_entry = DataEntry {
            account_id: create_test_account_id(1),
            data_name: "test_key".as_bytes().to_vec().try_into().unwrap(),
            data_value: DataValue(vec![1, 2, 3].try_into().unwrap()),
            ext: DataEntryExt::V0,
        };
        manager.create_data(data_entry);

        assert!(manager
            .get_data(&create_test_account_id(1), "test_key")
            .is_some());

        // Rollback — data entry should be gone
        manager.rollback_to_savepoint(sp);
        assert!(manager
            .get_data(&create_test_account_id(1), "test_key")
            .is_none());
    }

    #[test]
    fn test_savepoint_rollback_claimable_balances() {
        let mut manager = new_manager_with_offers(5_000_000, 100);

        // Create savepoint
        let sp = manager.create_savepoint();

        // Create a claimable balance after savepoint
        let cb_id = ClaimableBalanceId::ClaimableBalanceIdTypeV0(Hash([42; 32]));
        let cb_entry = ClaimableBalanceEntry {
            balance_id: cb_id.clone(),
            claimants: vec![].try_into().unwrap(),
            asset: Asset::Native,
            amount: 1_000_000,
            ext: ClaimableBalanceEntryExt::V0,
        };
        manager.create_claimable_balance(cb_entry);

        assert!(manager.get_claimable_balance(&cb_id).is_some());

        // Rollback — claimable balance should be gone
        manager.rollback_to_savepoint(sp);
        assert!(manager.get_claimable_balance(&cb_id).is_none());
    }

    #[test]
    fn test_savepoint_rollback_preserves_pre_savepoint_changes() {
        let mut manager = new_manager_with_offers(5_000_000, 100);

        // Create account and commit
        let account = create_test_account_entry(1);
        let account_id = account.account_id.clone();
        manager.create_account(account);
        manager.commit();

        // Modify balance to 500M (pre-savepoint change)
        if let Some(acc) = manager.get_account_mut(&account_id) {
            acc.balance = 500_000_000;
        }

        // Create savepoint
        let sp = manager.create_savepoint();

        // Create a new account and modify original after savepoint
        let account2 = create_test_account_entry(2);
        let account2_id = account2.account_id.clone();
        manager.create_account(account2);

        if let Some(acc) = manager.get_account_mut(&account_id) {
            acc.balance = 100;
        }

        // Rollback
        manager.rollback_to_savepoint(sp);

        // Original account should have pre-savepoint balance
        assert_eq!(
            manager.get_account(&account_id).unwrap().balance,
            500_000_000
        );
        // New account created after savepoint should be gone
        assert!(manager.get_account(&account2_id).is_none());
    }

    #[test]
    fn test_savepoint_rollback_restores_id_pool() {
        let mut manager = new_manager_with_offers(5_000_000, 100);

        let initial_id = manager.id_pool;
        let sp = manager.create_savepoint();

        // Advance id_pool (simulating offer creation)
        manager.id_pool += 5;
        assert_eq!(manager.id_pool, initial_id + 5);

        // Rollback — id_pool should be restored
        manager.rollback_to_savepoint(sp);
        assert_eq!(manager.id_pool, initial_id);
    }

    // ==================== Deleted Entry Tracking Tests ====================
    // These tests verify that deleted entries are tracked to prevent reloading
    // from the bucket list. This is essential for correct within-cluster
    // visibility in parallel Soroban execution.

    fn create_test_contract_address(seed: u8) -> ScAddress {
        ScAddress::Contract(Hash([seed; 32]).into())
    }

    fn create_test_contract_data_entry(seed: u8) -> ContractDataEntry {
        ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: create_test_contract_address(seed),
            key: ScVal::U32(seed as u32),
            durability: ContractDataDurability::Persistent,
            val: ScVal::U64(1000),
        }
    }

    fn create_test_contract_code_entry(seed: u8) -> ContractCodeEntry {
        ContractCodeEntry {
            ext: ContractCodeEntryExt::V0,
            hash: Hash([seed; 32]),
            code: vec![0, 1, 2, 3].try_into().unwrap(),
        }
    }

    fn create_test_ttl_entry(seed: u8) -> TtlEntry {
        TtlEntry {
            key_hash: Hash([seed; 32]),
            live_until_ledger_seq: 1000000,
        }
    }

    #[test]
    fn test_deleted_contract_data_tracking() {
        let mut manager = new_manager_with_offers(5_000_000, 100);
        let cd = create_test_contract_data_entry(1);
        let key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: cd.contract.clone(),
            key: cd.key.clone(),
            durability: cd.durability,
        });

        // Initially not deleted
        assert!(!manager.is_entry_deleted(&key));

        // Create and verify exists
        manager.create_contract_data(cd.clone());
        assert!(manager
            .get_contract_data(&cd.contract, &cd.key, cd.durability)
            .is_some());
        assert!(!manager.is_entry_deleted(&key));

        // Delete and verify tracking
        manager.delete_contract_data(&cd.contract, &cd.key, cd.durability);
        assert!(manager
            .get_contract_data(&cd.contract, &cd.key, cd.durability)
            .is_none());
        assert!(manager.is_entry_deleted(&key));
    }

    #[test]
    fn test_deleted_contract_code_tracking() {
        let mut manager = new_manager_with_offers(5_000_000, 100);
        let cc = create_test_contract_code_entry(2);
        let key = LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: cc.hash.clone(),
        });

        // Initially not deleted
        assert!(!manager.is_entry_deleted(&key));

        // Create and verify exists
        manager.create_contract_code(cc.clone());
        assert!(manager.get_contract_code(&cc.hash).is_some());
        assert!(!manager.is_entry_deleted(&key));

        // Delete and verify tracking
        manager.delete_contract_code(&cc.hash);
        assert!(manager.get_contract_code(&cc.hash).is_none());
        assert!(manager.is_entry_deleted(&key));
    }

    #[test]
    fn test_deleted_ttl_tracking() {
        let mut manager = new_manager_with_offers(5_000_000, 100);
        let ttl = create_test_ttl_entry(3);
        let key = LedgerKey::Ttl(LedgerKeyTtl {
            key_hash: ttl.key_hash.clone(),
        });

        // Initially not deleted
        assert!(!manager.is_entry_deleted(&key));

        // Create and verify exists
        manager.create_ttl(ttl.clone());
        assert!(manager.get_ttl(&ttl.key_hash).is_some());
        assert!(!manager.is_entry_deleted(&key));

        // Delete and verify tracking
        manager.delete_ttl(&ttl.key_hash);
        assert!(manager.get_ttl(&ttl.key_hash).is_none());
        assert!(manager.is_entry_deleted(&key));
    }

    #[test]
    fn test_non_soroban_entries_not_tracked_as_deleted() {
        let manager = new_manager_with_offers(5_000_000, 100);

        // Non-Soroban entry types should always return false for is_entry_deleted
        let account_key = LedgerKey::Account(LedgerKeyAccount {
            account_id: create_test_account_id(1),
        });
        assert!(!manager.is_entry_deleted(&account_key));

        let offer_key = LedgerKey::Offer(LedgerKeyOffer {
            seller_id: create_test_account_id(1),
            offer_id: 123,
        });
        assert!(!manager.is_entry_deleted(&offer_key));
    }

    #[test]
    fn test_deleted_entry_tracking_regression_842789() {
        // Regression test for ledger 842789 TX 5 mismatch.
        // When TX 4 deletes an entry, TX 5 should NOT reload it from bucket list.
        // This test verifies that deleted entries are properly tracked.
        let mut manager = new_manager_with_offers(5_000_000, 842789);

        // Simulate TX 4 creating and then deleting a contract data entry
        let cd = create_test_contract_data_entry(4);
        let key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: cd.contract.clone(),
            key: cd.key.clone(),
            durability: cd.durability,
        });

        // TX 4: Create entry
        manager.create_contract_data(cd.clone());
        assert!(manager.get_entry(&key).is_some());

        // TX 4: Delete entry
        manager.delete_contract_data(&cd.contract, &cd.key, cd.durability);

        // TX 5: Should see entry as deleted, not reload from bucket list
        assert!(manager.get_entry(&key).is_none());
        assert!(manager.is_entry_deleted(&key));

        // This is the key check: is_entry_deleted prevents footprint loading
        // from reloading the entry from bucket list
    }

    /// Regression test for VE-11: deleted entries must block BL reload across stages.
    ///
    /// When a Soroban TX in stage 0 deletes an entry from its RW footprint
    /// (host doesn't return a new value), subsequent stages must know not to
    /// reload the entry from the bucket list. Without mark_entry_deleted(),
    /// stage 1 would reload the orphaned entry from BL and the host would
    /// incorrectly succeed where it should trap.
    ///
    /// This matches stellar-core's cleanEmpty propagation via
    /// collectClusterFootprintEntriesFromGlobal.
    #[test]
    fn test_mark_entry_deleted_blocks_bl_reload_ve11() {
        // Simulate stage 1 executor: fresh state, no prior knowledge of deletions.
        let mut manager = new_manager_with_offers(5_000_000, 61430400);

        let cd = create_test_contract_data_entry(42);
        let cd_key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: cd.contract.clone(),
            key: cd.key.clone(),
            durability: cd.durability,
        });
        let ttl_key = LedgerKey::Ttl(LedgerKeyTtl {
            key_hash: Hash([42; 32]),
        });

        // Before marking: entry is not in state and not deleted.
        // load_soroban_footprint would fall through to BL.
        assert!(manager.get_entry(&cd_key).is_none());
        assert!(!manager.is_entry_deleted(&cd_key));
        assert!(!manager.is_entry_deleted(&ttl_key));

        // Mark as deleted (propagated from stage 0 delta.dead_entries()).
        manager.mark_entry_deleted(&cd_key);
        manager.mark_entry_deleted(&ttl_key);

        // After marking: entry is still not in state, but IS marked deleted.
        // load_soroban_footprint will skip BL load.
        assert!(manager.get_entry(&cd_key).is_none());
        assert!(manager.is_entry_deleted(&cd_key));
        assert!(manager.is_entry_deleted(&ttl_key));

        // Verify ContractCode works too.
        let cc_key = LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: Hash([99; 32]),
        });
        assert!(!manager.is_entry_deleted(&cc_key));
        manager.mark_entry_deleted(&cc_key);
        assert!(manager.is_entry_deleted(&cc_key));
    }

    /// Regression test for clear_cached_entries_preserving_offers.
    ///
    /// Verifies that clearing cached entries with preserve_offers=true retains
    /// the offer HashMap, OfferIndex, and account_asset_offers secondary index
    /// while clearing everything else (accounts, trustlines, etc.).
    ///
    /// This is critical for the offer cache persistence optimization: after a
    /// ledger close, the executor calls this to prepare for the next ledger
    /// without reloading ~911K offers.
    #[test]
    fn test_clear_cached_entries_preserving_offers() {
        let mut manager = new_manager_with_offers(5_000_000, 100);

        // Add an account
        let account = create_test_account_entry(1);
        manager.create_account(account.clone());

        // Add a trustline
        let trustline = TrustLineEntry {
            account_id: create_test_account_id(1),
            asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', 0]),
                issuer: create_test_account_id(99),
            }),
            balance: 1000,
            limit: 10000,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V0,
        };
        manager.create_trustline(trustline);

        // Add two offers
        let offer1 = create_test_offer(1, 100, 1, 1);
        let offer2 = create_test_offer(2, 200, 2, 1);
        manager.create_offer(offer1.clone());
        manager.create_offer(offer2.clone());

        // Commit so entries are in the base state
        manager.commit();

        // Verify everything is present before clear
        assert!(manager.get_account(&create_test_account_id(1)).is_some());
        assert!(manager.get_offer(&create_test_account_id(1), 100).is_some());
        assert!(manager.get_offer(&create_test_account_id(2), 200).is_some());

        // Clear preserving offers
        manager.clear_cached_entries_preserving_offers();

        // Accounts and trustlines should be cleared
        assert!(
            manager.get_account(&create_test_account_id(1)).is_none(),
            "Accounts should be cleared"
        );

        // Offers should be preserved
        assert!(
            manager.get_offer(&create_test_account_id(1), 100).is_some(),
            "Offer 1 should be preserved"
        );
        assert!(
            manager.get_offer(&create_test_account_id(2), 200).is_some(),
            "Offer 2 should be preserved"
        );

        // Offer index should still work (best_offer lookup)
        let best = manager.best_offer(&offer1.buying, &offer1.selling);
        assert!(best.is_some(), "OfferIndex should be preserved");
    }

    /// Test: clear_cached_entries clears non-offer entries but preserves the shared OfferStore.
    ///
    /// With the shared OfferStore architecture, offers are always preserved across
    /// clear_cached_entries calls because the store is shared with other components.
    #[test]
    fn test_clear_cached_entries_preserves_shared_offer_store() {
        let mut manager = new_manager_with_offers(5_000_000, 100);

        let offer1 = create_test_offer(1, 100, 1, 1);
        manager.create_offer(offer1);
        manager.commit();

        assert!(manager.get_offer(&create_test_account_id(1), 100).is_some());

        manager.clear_cached_entries();

        // Offers are preserved in the shared OfferStore
        assert!(
            manager.get_offer(&create_test_account_id(1), 100).is_some(),
            "Offers in the shared OfferStore should be preserved across clear_cached_entries"
        );
    }

    /// Test: remove_offers_by_account_and_asset discovers all offers in the shared OfferStore.
    ///
    /// With the shared OfferStore, all offers are always visible regardless of whether
    /// they were explicitly loaded by this state manager. This replaces the old loader-based
    /// test since the OfferStore is the single authoritative source.
    #[test]
    fn test_remove_offers_by_account_and_asset_uses_store() {
        let mut manager = new_manager_with_offers(5_000_000, 100);
        let seller1 = create_test_account_id(1);

        // Create offer1 via the state manager.
        let offer1 = create_test_offer_with_assets(1, 100, Asset::Native, usd_asset());
        manager.create_offer(offer1);
        manager.commit();

        // Insert offer2 directly into the shared OfferStore (simulating it being
        // present from a prior ledger close, not loaded via this state manager).
        let offer2 = create_test_offer_with_assets(1, 200, eur_asset(), usd_asset());
        {
            let mut store = manager.offer_store_lock();
            store.insert_record(OfferRecord {
                entry: offer2,
                last_modified: 100,
                sponsor: None,
                has_ext: false,
            });
        }

        // remove_offers_by_account_and_asset should find both offers via the OfferStore.
        let removed = manager.remove_offers_by_account_and_asset(&seller1, &usd_asset());

        let removed_ids: HashSet<i64> = removed.iter().map(|o| o.offer_id).collect();
        assert_eq!(
            removed_ids,
            HashSet::from([100, 200]),
            "Both offers should be removed from the shared OfferStore"
        );
    }

    /// Regression test: rollback must restore accounts to pre-tx state even after
    /// flush_all_accounts() was called between operations. Before the fix,
    /// flush_all_accounts_except() updated account_snapshots to the post-flush
    /// value, so rollback() would "restore" to the mid-transaction value instead
    /// of the original pre-tx value.
    ///
    /// This bug caused a bucket_list_hash mismatch at mainnet L59504399 where a
    /// multi-op transaction (3 ChangeTrust + AccountMerge) failed but the sponsor
    /// account's num_sponsoring was not correctly rolled back.
    #[test]
    fn test_rollback_after_flush_all_accounts_restores_original() {
        let mut manager = new_manager_with_offers(5_000_000, 100);

        // Create an account that will be modified across multiple "operations"
        let account = create_test_account_entry(1);
        let account_id = account.account_id.clone();
        manager.create_account(account.clone());
        manager.commit(); // commit the creation so it's permanent

        let original_balance = manager.get_account(&account_id).unwrap().balance;
        assert_eq!(original_balance, 1_000_000_000);

        // Simulate start of a new transaction: snapshot delta
        manager.snapshot_delta();

        // Operation 1: modify account and flush
        {
            let acc = manager.get_account_mut(&account_id).unwrap();
            acc.balance -= 100;
        }
        manager.flush_all_accounts();

        // Operation 2: modify account again and flush
        {
            let acc = manager.get_account_mut(&account_id).unwrap();
            acc.balance -= 200;
        }
        manager.flush_all_accounts();

        // Operation 3: modify account again and flush
        {
            let acc = manager.get_account_mut(&account_id).unwrap();
            acc.balance -= 300;
        }
        manager.flush_all_accounts();

        // Current state should reflect all modifications
        let current_balance = manager.get_account(&account_id).unwrap().balance;
        assert_eq!(current_balance, 1_000_000_000 - 100 - 200 - 300);

        // Transaction fails — rollback should restore to original pre-tx state
        manager.rollback();

        let restored_balance = manager.get_account(&account_id).unwrap().balance;
        assert_eq!(
            restored_balance, original_balance,
            "rollback() must restore to original pre-tx balance, not mid-tx flushed value"
        );
    }

    /// Regression test: remove_offers_by_account_and_asset must skip offers
    /// that were already deleted in a previous transaction within the same ledger.
    ///
    /// With the shared OfferStore, deleted offers are immediately removed, so
    /// they cannot be re-discovered. This test verifies correctness.
    ///
    /// Originally caused by mainnet L59517076 where offer 1799030633 was deleted
    /// by TX 81 (offer crossing), then re-loaded and re-deleted by TX 105
    /// (AllowTrust authorization revocation).
    #[test]
    fn test_remove_offers_skips_already_deleted_offers() {
        let mut manager = new_manager_with_offers(5_000_000, 100);
        let seller = create_test_account_id(1);

        // Create account with sub_entries tracking
        let mut account = create_test_account_entry(1);
        account.num_sub_entries = 2; // has 2 offers
        manager.create_account(account);

        // Create and load two offers for the same account
        let offer1 = create_test_offer_with_assets(1, 100, Asset::Native, usd_asset());
        let offer2 = create_test_offer_with_assets(1, 200, eur_asset(), usd_asset());
        manager.create_offer(offer1.clone());
        manager.create_offer(offer2.clone());
        manager.commit(); // permanent state

        // --- TX1: Delete offer1 via offer crossing ---
        manager.snapshot_delta();
        manager.delete_offer(&seller, 100);
        manager.flush_modified_entries();
        // TX1 succeeds
        manager.commit();

        // Verify offer1 is deleted from the shared OfferStore
        assert!(manager.get_offer(&seller, 100).is_none());
        // Verify offer1 deletion is in the delta
        let offer1_key = LedgerKey::Offer(LedgerKeyOffer {
            seller_id: seller.clone(),
            offer_id: 100,
        });
        assert!(
            manager.delta().deleted_keys().contains(&offer1_key),
            "offer1 deletion must be in the delta"
        );

        // --- TX2: AllowTrust revokes authorization, triggering remove_offers ---
        manager.snapshot_delta();

        // With the shared OfferStore, offer1 is already gone.
        let removed = manager.remove_offers_by_account_and_asset(&seller, &usd_asset());

        // Only offer2 should be removed. offer1 was already deleted in TX1.
        let removed_ids: HashSet<i64> = removed.iter().map(|o| o.offer_id).collect();
        assert_eq!(
            removed_ids,
            HashSet::from([200]),
            "Only offer2 should be removed; offer1 was already deleted in TX1"
        );

        // Verify offer1's deletion appears exactly once in the delta
        let delete_count = manager
            .delta()
            .deleted_keys()
            .iter()
            .filter(|k| *k == &offer1_key)
            .count();
        assert_eq!(
            delete_count, 1,
            "offer1 must be deleted exactly once in the delta, not twice"
        );
    }

    /// Regression test for L59548531: TTL rollback must restore original value.
    ///
    /// When a Soroban TX extends a TTL entry and then fails (e.g.
    /// InsufficientRefundableFee), the TTL must be rolled back to its
    /// pre-transaction value so that subsequent TXs in the same cluster
    /// see the original TTL.  A bug in `update_ttl` overwrote the snapshot
    /// with the post-modification value, making rollback a no-op and
    /// leaking extended TTLs into the next transaction's state.
    #[test]
    fn test_ttl_rollback_restores_original_value() {
        let mut manager = new_manager_with_offers(5_000_000, 100);
        let key_hash = Hash([7; 32]);
        let original_ttl = TtlEntry {
            key_hash: key_hash.clone(),
            live_until_ledger_seq: 100_000,
        };

        // Load entry (simulating load_soroban_footprint)
        manager
            .ttl_entries
            .insert(key_hash.clone(), original_ttl.clone());

        // Commit (simulating the commit before operations)
        manager.commit();

        // Snapshot delta (simulating TX start)
        manager.snapshot_delta();

        // update_ttl extends TTL to 500_000 (simulating Soroban host extending TTL)
        let extended_ttl = TtlEntry {
            key_hash: key_hash.clone(),
            live_until_ledger_seq: 500_000,
        };
        manager.update_ttl(extended_ttl);
        assert_eq!(
            manager.get_ttl(&key_hash).unwrap().live_until_ledger_seq,
            500_000
        );

        // TX fails — rollback must restore original TTL
        manager.rollback();
        assert_eq!(
            manager.get_ttl(&key_hash).unwrap().live_until_ledger_seq,
            100_000,
            "TTL must be restored to original value after rollback"
        );
    }

    /// Regression test for L59548531: extend_ttl rollback via savepoint.
    ///
    /// Same scenario as above but using extend_ttl and savepoint rollback
    /// (simulating per-operation rollback within a transaction).
    #[test]
    fn test_extend_ttl_savepoint_rollback_restores_original_value() {
        let mut manager = new_manager_with_offers(5_000_000, 100);
        let key_hash = Hash([8; 32]);
        let original_ttl = TtlEntry {
            key_hash: key_hash.clone(),
            live_until_ledger_seq: 200_000,
        };

        // Load and commit entry
        manager
            .ttl_entries
            .insert(key_hash.clone(), original_ttl.clone());
        manager.commit();

        // Create savepoint (simulating per-operation savepoint)
        let sp = manager.create_savepoint();

        // extend_ttl to 700_000
        manager.extend_ttl(&key_hash, 700_000);
        assert_eq!(
            manager.get_ttl(&key_hash).unwrap().live_until_ledger_seq,
            700_000
        );

        // Rollback to savepoint (simulating failed operation)
        manager.rollback_to_savepoint(sp);
        assert_eq!(
            manager.get_ttl(&key_hash).unwrap().live_until_ledger_seq,
            200_000,
            "TTL must be restored to original value after savepoint rollback"
        );
    }

    /// Regression test for L59548531: cross-TX TTL isolation in a cluster.
    ///
    /// Verifies that when TX 1 extends a TTL (RW) and fails, TX 2 in the same
    /// cluster sees the original TTL value (not the extended one from TX 1).
    #[test]
    fn test_ttl_cross_tx_isolation_after_failed_tx() {
        let mut manager = new_manager_with_offers(5_000_000, 100);
        let key_hash = Hash([9; 32]);
        let original_ttl = TtlEntry {
            key_hash: key_hash.clone(),
            live_until_ledger_seq: 100_000,
        };

        // Load entry and commit
        manager
            .ttl_entries
            .insert(key_hash.clone(), original_ttl.clone());
        manager.commit();

        // === TX 1 ===
        manager.snapshot_delta();

        // TX 1 extends TTL (RW path — uses update_ttl which modifies ttl_entries directly)
        let extended_ttl = TtlEntry {
            key_hash: key_hash.clone(),
            live_until_ledger_seq: 500_000,
        };
        manager.update_ttl(extended_ttl);

        // TX 1 fails
        manager.rollback();

        // === TX 2 ===
        // TX 2 should see the ORIGINAL TTL, not the extended one from TX 1
        let ttl = manager.get_ttl(&key_hash).unwrap();
        assert_eq!(
            ttl.live_until_ledger_seq, 100_000,
            "TX 2 must see original TTL (100_000), not leaked extended TTL (500_000) from failed TX 1"
        );
    }

    /// Regression test: RO TTL bumps are rolled back when a TX fails.
    ///
    /// In stellar-core, `commitChangesFromSuccessfulOp` is only called for
    /// successful TXs. Failed TXs do not commit their RO TTL bumps to
    /// `mRoTTLBumps`. Our code matches this by snapshotting/restoring
    /// `deferred_ro_ttl_bumps` on rollback.
    #[test]
    fn test_ro_ttl_bumps_rolled_back_on_tx_failure() {
        let mut manager = new_manager_with_offers(5_000_000, 100);
        let key_hash = Hash([10; 32]);
        let original_ttl = TtlEntry {
            key_hash: key_hash.clone(),
            live_until_ledger_seq: 100_000,
        };

        // Load entry and commit
        manager
            .ttl_entries
            .insert(key_hash.clone(), original_ttl.clone());
        manager.commit();

        // === TX 1 (will fail) ===
        manager.snapshot_delta();

        // TX 1 records an RO TTL bump (deferred — does NOT update ttl_entries)
        manager.record_ro_ttl_bump_for_meta(&key_hash, 500_000);

        // Verify the bump is stored in deferred_ro_ttl_bumps
        assert_eq!(
            manager.deferred_ro_ttl_bumps.get(&key_hash),
            Some(&500_000),
            "RO TTL bump must be stored in deferred_ro_ttl_bumps"
        );

        // TX 1 fails — rollback
        manager.rollback();

        // RO TTL bumps are rolled back (matches stellar-core behavior)
        assert_eq!(
            manager.deferred_ro_ttl_bumps.get(&key_hash),
            None,
            "RO TTL bump must be rolled back on TX failure"
        );

        // === TX 2 (succeeds) ===
        manager.snapshot_delta();

        // TX 2 records an RO TTL bump for the same key
        manager.record_ro_ttl_bump_for_meta(&key_hash, 600_000);

        // TX 2 commits (no rollback)
        manager.commit();

        // Only TX 2's bump should be present
        assert_eq!(
            manager.deferred_ro_ttl_bumps.get(&key_hash),
            Some(&600_000),
            "Only successful TX's RO TTL bump should be present"
        );
    }

    /// Regression test: RO TTL bumps are flushed to ttl_entries for keys in
    /// a subsequent TX's write footprint before that TX executes.
    ///
    /// This matches stellar-core's `flushRoTTLBumpsInTxWriteFootprint`:
    /// when TX A (read-only) bumps a TTL, and TX B has that key in its
    /// write footprint, TX B must see the bumped TTL value for correct
    /// rent fee calculation. Without this flush, TX B would see the old
    /// (lower) TTL and compute higher rent fees.
    #[test]
    fn test_flush_ro_ttl_bumps_for_write_footprint() {
        use stellar_xdr::curr::{
            ContractDataDurability, LedgerKey, LedgerKeyContractData, ScAddress, ScVal,
        };

        let mut manager = new_manager_with_offers(5_000_000, 100);
        let key_hash = Hash([20; 32]);
        let original_ttl = TtlEntry {
            key_hash: key_hash.clone(),
            live_until_ledger_seq: 100_000,
        };

        // Load TTL entry
        manager
            .ttl_entries
            .insert(key_hash.clone(), original_ttl.clone());
        manager.commit();

        // === TX 1 (succeeds): records an RO TTL bump ===
        manager.snapshot_delta();
        manager.record_ro_ttl_bump_for_meta(&key_hash, 500_000);
        manager.commit();

        // Bump is deferred — ttl_entries still has old value
        assert_eq!(
            manager
                .ttl_entries
                .get(&key_hash)
                .unwrap()
                .live_until_ledger_seq,
            100_000,
            "TTL entry must not be updated yet (deferred)"
        );
        assert_eq!(
            manager.deferred_ro_ttl_bumps.get(&key_hash),
            Some(&500_000),
            "Deferred bump must be stored"
        );

        // === TX 2 is about to execute with this key in its write footprint ===
        // Build a ContractData key that hashes to key_hash.
        // We can't easily control the hash, so we use the flush method directly
        // with a key whose hash matches. Instead, let's test by inserting the
        // bump manually and verifying flush behavior with known hash.

        // For this test, call flush_ro_ttl_bumps_for_write_footprint with
        // a ContractData key. The function hashes the key to find the TTL entry.
        // Since we can't make a key hash to exactly [20; 32], let's test the
        // mechanism: create a real ContractData key, compute its actual hash,
        // and set up state accordingly.
        let contract_key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(stellar_xdr::curr::ContractId(Hash([99; 32]))),
            key: ScVal::Bool(true),
            durability: ContractDataDurability::Persistent,
        });

        // Compute the actual hash this key produces
        let actual_hash = {
            use sha2::{Digest, Sha256};
            use stellar_xdr::curr::WriteXdr;
            let mut hasher = Sha256::new();
            let bytes = contract_key
                .to_xdr(stellar_xdr::curr::Limits::none())
                .unwrap();
            hasher.update(&bytes);
            let result: [u8; 32] = hasher.finalize().into();
            Hash(result)
        };

        // Set up the TTL entry and deferred bump using the ACTUAL hash
        let real_key_hash = actual_hash.clone();
        let real_ttl = TtlEntry {
            key_hash: real_key_hash.clone(),
            live_until_ledger_seq: 200_000,
        };
        manager.ttl_entries.insert(actual_hash.clone(), real_ttl);
        manager
            .deferred_ro_ttl_bumps
            .insert(actual_hash.clone(), 800_000);

        // Flush for write footprint containing this key
        manager.flush_ro_ttl_bumps_for_write_footprint(&[contract_key]);

        // After flush: TTL entry should be updated to the bumped value
        assert_eq!(
            manager
                .ttl_entries
                .get(&actual_hash)
                .unwrap()
                .live_until_ledger_seq,
            800_000,
            "TTL must be flushed to bumped value for write footprint key"
        );

        // Deferred bump should be removed (erased, like stellar-core)
        assert_eq!(
            manager.deferred_ro_ttl_bumps.get(&actual_hash),
            None,
            "Deferred bump must be erased after flush"
        );
    }

    /// Test that flush_ro_ttl_bumps_for_write_footprint skips non-Soroban keys.
    #[test]
    fn test_flush_ro_ttl_bumps_skips_non_soroban_keys() {
        let mut manager = new_manager_with_offers(5_000_000, 100);

        // Add a deferred bump
        let key_hash = Hash([30; 32]);
        let ttl = TtlEntry {
            key_hash: key_hash.clone(),
            live_until_ledger_seq: 100_000,
        };
        manager.ttl_entries.insert(key_hash.clone(), ttl);
        manager
            .deferred_ro_ttl_bumps
            .insert(key_hash.clone(), 500_000);

        // Flush with a non-Soroban key (Account)
        let account_key = LedgerKey::Account(LedgerKeyAccount {
            account_id: AccountId(PublicKey::PublicKeyTypeEd25519(stellar_xdr::curr::Uint256(
                [1; 32],
            ))),
        });
        manager.flush_ro_ttl_bumps_for_write_footprint(&[account_key]);

        // Deferred bump should NOT be erased (Account keys are skipped)
        assert_eq!(
            manager.deferred_ro_ttl_bumps.get(&key_hash),
            Some(&500_000),
            "Non-Soroban keys must not trigger flush"
        );
        // TTL entry should be unchanged
        assert_eq!(
            manager
                .ttl_entries
                .get(&key_hash)
                .unwrap()
                .live_until_ledger_seq,
            100_000,
            "TTL must remain unchanged for non-Soroban keys"
        );
    }

    /// Regression test: flush_modified_entries must stamp
    /// last_modified_ledger_seq on the post-state LedgerEntry recorded into
    /// the delta.
    ///
    /// In stellar-core, LedgerTxn::Impl::commitChild calls
    /// maybeUpdateLastModified which sets lastModifiedLedgerSeq = ledgerSeq on
    /// every entry in mEntry (i.e. loaded with record).  Our
    /// record_flush_update was building the post_state LedgerEntry *before*
    /// calling set_last_modified_key, so the delta entry carried the stale
    /// (original) LML.  This caused bucket-list-hash mismatches starting at
    /// mainnet L59658048 where trustlines were touched via get_trustline_mut
    /// during offer crossing but their data was unchanged.
    #[test]
    fn test_flush_modified_entries_stamps_current_ledger_on_post_state() {
        let ledger_seq = 500;
        let mut manager = new_manager_with_offers(5_000_000, ledger_seq);

        // Create an account (committed at ledger_seq)
        let account = create_test_account_entry(1);
        let account_id = account.account_id.clone();
        manager.create_account(account);
        manager.commit();

        // Create a trustline (committed at ledger_seq)
        let trustline = TrustLineEntry {
            account_id: create_test_account_id(1),
            asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', 0]),
                issuer: create_test_account_id(99),
            }),
            balance: 1000,
            limit: 10000,
            flags: TrustLineFlags::AuthorizedFlag as u32,
            ext: TrustLineEntryExt::V0,
        };
        let usd_asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', b'S', b'D', 0]),
            issuer: create_test_account_id(99),
        });
        manager.create_trustline(trustline);
        manager.commit();

        // Advance to a new ledger
        let new_ledger = 600;
        manager.set_ledger_seq(new_ledger);
        manager.delta = TxChangeLog::new(new_ledger);

        // Simulate beginning a transaction: snapshot the delta
        manager.snapshot_delta();

        // Begin an operation snapshot
        manager.begin_op_snapshot();

        // Touch the trustline via get_trustline_mut (no data change)
        // This is what happens during offer crossing: the trustline is loaded
        // with record (get_trustline_mut) but its balance doesn't change.
        let tl = manager.get_trustline_mut(&account_id, &usd_asset).unwrap();
        let original_balance = tl.balance;
        // Don't change the balance — the entry is touched but not modified

        // Also touch the account via get_account_mut (no data change)
        let acc = manager.get_account_mut(&account_id).unwrap();
        let _original_acc_balance = acc.balance;
        // Don't change the balance

        // Flush modified entries (this is where the bug was)
        manager.flush_modified_entries();

        // End op snapshot
        manager.end_op_snapshot();

        // Verify the delta contains the updated entries with current ledger LML
        // Find the trustline in the delta's updated entries
        let updated = manager.delta().updated_entries();
        let tl_post = updated
            .iter()
            .find(|e| matches!(&e.data, LedgerEntryData::Trustline(tl) if tl.account_id == account_id))
            .expect("trustline should be in delta updated entries");
        assert_eq!(
            tl_post.last_modified_ledger_seq, new_ledger,
            "Trustline post-state LML must be stamped to current ledger ({}), \
             not the original ledger when the entry was created. \
             Before the fix, it would be {} (the creation ledger).",
            new_ledger, ledger_seq
        );

        // Verify data was NOT changed
        if let LedgerEntryData::Trustline(tl_data) = &tl_post.data {
            assert_eq!(
                tl_data.balance, original_balance,
                "Trustline balance should be unchanged"
            );
        } else {
            panic!("Expected trustline entry in delta");
        }

        // Verify account too
        let acc_post = updated
            .iter()
            .find(|e| matches!(&e.data, LedgerEntryData::Account(a) if a.account_id == account_id))
            .expect("account should be in delta updated entries");
        assert_eq!(
            acc_post.last_modified_ledger_seq, new_ledger,
            "Account post-state LML must be stamped to current ledger ({})",
            new_ledger
        );
    }

    /// Regression test: update_contract_data used to overwrite contract_data_snapshots with the
    /// new (modified) value, causing rollback_to_savepoint to restore the wrong entry.
    ///
    /// Scenario: a Pail entry with zeros=6 exists in the committed state. A TX invokes a contract
    /// that updates it to zeros=7, then InsufficientRefundableFee triggers rollback_to_savepoint.
    /// After the fix, the entry must be restored to zeros=6.
    #[test]
    fn test_rollback_to_savepoint_restores_contract_data_after_update() {
        let ledger_seq = 100u32;
        let mut manager = new_manager_with_offers(5_000_000, ledger_seq);

        let contract = ScAddress::Contract(ContractId(Hash([1u8; 32])));
        let key = ScVal::I32(42);

        // Create the entry (simulating a bucket-list value) and commit it.
        let original_entry = ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: contract.clone(),
            key: key.clone(),
            durability: ContractDataDurability::Temporary,
            val: ScVal::I32(6),
        };
        manager.create_contract_data(original_entry);
        manager.commit();

        // Start the failing TX: take savepoint before the operation.
        manager.snapshot_delta();
        let savepoint = manager.create_savepoint();

        // Contract invocation updates the entry (zeros: 6 -> 7).
        let modified_entry = ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: contract.clone(),
            key: key.clone(),
            durability: ContractDataDurability::Temporary,
            val: ScVal::I32(7),
        };
        manager.update_contract_data(modified_entry);

        // Modification is visible before rollback.
        assert_eq!(
            manager
                .get_contract_data(&contract, &key, ContractDataDurability::Temporary)
                .map(|e| e.val.clone()),
            Some(ScVal::I32(7)),
            "Entry should be modified before rollback"
        );

        // InsufficientRefundableFee: roll back the operation.
        manager.rollback_to_savepoint(savepoint);

        // Entry must be restored to the original value.
        assert_eq!(
            manager
                .get_contract_data(&contract, &key, ContractDataDurability::Temporary)
                .map(|e| e.val.clone()),
            Some(ScVal::I32(6)),
            "Entry must be restored to original value after rollback_to_savepoint"
        );
    }

    /// Regression test: update_account must not overwrite the snapshot with the
    /// modified value. If it did, rollback_to_savepoint Phase 1 would restore
    /// the wrong (post-update) balance instead of the original.
    #[test]
    fn test_rollback_to_savepoint_restores_account_after_update() {
        let mut manager = new_manager_with_offers(5_000_000, 100);

        let mut account = create_test_account_entry(1);
        account.balance = 1_000_000;
        let account_id = account.account_id.clone();

        // Create entry and commit (simulates bucket-list state).
        manager.create_account(account);
        manager.commit();

        // Start TX: take savepoint before the operation.
        manager.snapshot_delta();
        let savepoint = manager.create_savepoint();

        // Update account (e.g., fee deduction or transfer).
        let mut modified = manager.get_account(&account_id).unwrap().clone();
        modified.balance = 900_000;
        manager.update_account(modified);

        assert_eq!(
            manager.get_account(&account_id).unwrap().balance,
            900_000,
            "Entry should be modified before rollback"
        );

        // Operation fails: rollback.
        manager.rollback_to_savepoint(savepoint);

        assert_eq!(
            manager.get_account(&account_id).unwrap().balance,
            1_000_000,
            "Account balance must be restored to original after rollback_to_savepoint"
        );
    }

    /// Regression test: update_data must not overwrite the snapshot.
    #[test]
    fn test_rollback_to_savepoint_restores_data_after_update() {
        let mut manager = new_manager_with_offers(5_000_000, 100);

        let account_id = create_test_account_id(1);
        let name = "my_key";
        let original_entry = DataEntry {
            account_id: account_id.clone(),
            data_name: name.as_bytes().to_vec().try_into().unwrap(),
            data_value: DataValue(vec![1u8].try_into().unwrap()),
            ext: DataEntryExt::V0,
        };

        manager.create_data(original_entry);
        manager.commit();

        manager.snapshot_delta();
        let savepoint = manager.create_savepoint();

        let modified_entry = DataEntry {
            account_id: account_id.clone(),
            data_name: name.as_bytes().to_vec().try_into().unwrap(),
            data_value: DataValue(vec![2u8].try_into().unwrap()),
            ext: DataEntryExt::V0,
        };
        manager.update_data(modified_entry);

        let modified_val: DataValue = vec![2u8].try_into().unwrap();
        assert_eq!(
            manager
                .get_data(&account_id, name)
                .map(|e| e.data_value.clone()),
            Some(modified_val),
            "Entry should be modified before rollback"
        );

        manager.rollback_to_savepoint(savepoint);

        let original_val: DataValue = vec![1u8].try_into().unwrap();
        assert_eq!(
            manager
                .get_data(&account_id, name)
                .map(|e| e.data_value.clone()),
            Some(original_val),
            "Data entry must be restored to original after rollback_to_savepoint"
        );
    }

    /// Regression test: update_claimable_balance must not overwrite the snapshot.
    #[test]
    fn test_rollback_to_savepoint_restores_claimable_balance_after_update() {
        let mut manager = new_manager_with_offers(5_000_000, 100);

        let balance_id = ClaimableBalanceId::ClaimableBalanceIdTypeV0(Hash([5u8; 32]));
        let original_entry = ClaimableBalanceEntry {
            balance_id: balance_id.clone(),
            claimants: vec![Claimant::ClaimantTypeV0(ClaimantV0 {
                destination: create_test_account_id(1),
                predicate: ClaimPredicate::Unconditional,
            })]
            .try_into()
            .unwrap(),
            asset: Asset::Native,
            amount: 1_000_000,
            ext: ClaimableBalanceEntryExt::V0,
        };

        manager.create_claimable_balance(original_entry);
        manager.commit();

        manager.snapshot_delta();
        let savepoint = manager.create_savepoint();

        let modified_entry = ClaimableBalanceEntry {
            balance_id: balance_id.clone(),
            claimants: vec![Claimant::ClaimantTypeV0(ClaimantV0 {
                destination: create_test_account_id(1),
                predicate: ClaimPredicate::Unconditional,
            })]
            .try_into()
            .unwrap(),
            asset: Asset::Native,
            amount: 500_000,
            ext: ClaimableBalanceEntryExt::V0,
        };
        manager.update_claimable_balance(modified_entry);

        assert_eq!(
            manager.get_claimable_balance(&balance_id).map(|e| e.amount),
            Some(500_000),
            "Entry should be modified before rollback"
        );

        manager.rollback_to_savepoint(savepoint);

        assert_eq!(
            manager.get_claimable_balance(&balance_id).map(|e| e.amount),
            Some(1_000_000),
            "Claimable balance amount must be restored to original after rollback_to_savepoint"
        );
    }

    #[test]
    fn test_clear_cached_entries_preserving_offers_clears_batch_entry_loader() {
        let mut manager = new_manager_with_offers(100, 1);
        manager.set_entry_loader(Arc::new(|_| Ok(None)));
        manager.set_batch_entry_loader(Arc::new(|_| Ok(vec![])));
        manager.set_pool_share_tls_by_account_loader(Arc::new(|_| Ok(vec![])));

        // All three loaders should be set
        assert!(manager.entry_loader.is_some());
        assert!(manager.batch_entry_loader.is_some());
        assert!(manager.pool_share_tls_by_account_loader.is_some());

        manager.clear_cached_entries_preserving_offers();

        // All three loaders should be cleared
        assert!(manager.entry_loader.is_none());
        assert!(
            manager.batch_entry_loader.is_none(),
            "batch_entry_loader must be cleared to avoid cross-ledger retention"
        );
        assert!(manager.pool_share_tls_by_account_loader.is_none());
    }

    #[test]
    fn test_clear_entry_loaders() {
        let mut manager = new_manager_with_offers(100, 1);
        manager.set_entry_loader(Arc::new(|_| Ok(None)));
        manager.set_batch_entry_loader(Arc::new(|_| Ok(vec![])));
        manager.set_pool_share_tls_by_account_loader(Arc::new(|_| Ok(vec![])));

        manager.clear_entry_loaders();

        assert!(manager.entry_loader.is_none());
        assert!(manager.batch_entry_loader.is_none());
        assert!(manager.pool_share_tls_by_account_loader.is_none());
    }

    /// Regression test: delete_account must record the pre-op snapshot as STATE,
    /// not the current mutated state. When remove_account_merge_sponsorship
    /// decrements num_sponsored before delete_account, the delete STATE must
    /// still contain the original num_sponsored value.
    #[test]
    fn test_delete_account_uses_snapshot_pre_state() {
        let mut state = new_manager_with_offers(5_000_000, 100);

        let source_id = create_test_account_id(0);
        let sponsor_id = create_test_account_id(1);

        // Source account: sponsored with num_sponsored = 2
        let mut source = create_test_account_entry(0);
        source.ext = AccountEntryExt::V1(AccountEntryExtensionV1 {
            liabilities: Liabilities {
                buying: 0,
                selling: 0,
            },
            ext: AccountEntryExtensionV1Ext::V2(AccountEntryExtensionV2 {
                num_sponsored: 2,
                num_sponsoring: 0,
                signer_sponsoring_i_ds: vec![].try_into().unwrap(),
                ext: AccountEntryExtensionV2Ext::V0,
            }),
        });
        state.create_account(source);

        // Sponsor with num_sponsoring = 2
        let sponsor = AccountEntry {
            account_id: sponsor_id.clone(),
            balance: 500_000_000,
            seq_num: SequenceNumber(1),
            num_sub_entries: 0,
            inflation_dest: None,
            flags: 0,
            home_domain: String32::default(),
            thresholds: Thresholds([1, 0, 0, 0]),
            signers: vec![].try_into().unwrap(),
            ext: AccountEntryExt::V1(AccountEntryExtensionV1 {
                liabilities: Liabilities {
                    buying: 0,
                    selling: 0,
                },
                ext: AccountEntryExtensionV1Ext::V2(AccountEntryExtensionV2 {
                    num_sponsored: 0,
                    num_sponsoring: 2,
                    signer_sponsoring_i_ds: vec![].try_into().unwrap(),
                    ext: AccountEntryExtensionV2Ext::V0,
                }),
            }),
        };
        state.create_account(sponsor);

        // Mark source as sponsored by sponsor
        let source_key = LedgerKey::Account(LedgerKeyAccount {
            account_id: source_id.clone(),
        });
        state.set_entry_sponsor(source_key, sponsor_id.clone());

        // Save snapshot of source — override the None from create_account
        // to simulate the account existing before this tx
        let snapshot = state.accounts.get(&source_id).cloned();
        state.account_snapshots.insert(source_id.clone(), snapshot);

        // Simulate account-merge: sponsorship removal mutates num_sponsored
        state
            .remove_account_merge_sponsorship(&source_id, 2)
            .unwrap();

        // Verify the in-memory mutation happened (num_sponsored = 0)
        let acc = state.get_account(&source_id).unwrap();
        match &acc.ext {
            AccountEntryExt::V1(v1) => match &v1.ext {
                AccountEntryExtensionV1Ext::V2(v2) => {
                    assert_eq!(
                        v2.num_sponsored, 0,
                        "mutation should have set num_sponsored=0"
                    );
                }
                _ => panic!("expected V2 ext"),
            },
            _ => panic!("expected V1 ext"),
        }

        // Now delete — the pre-state in delete_states should have num_sponsored = 2
        state.delete_account(&source_id);

        let delta = state.delta();
        let delete_states = delta.delete_states();
        // Find the source account's delete state
        let source_delete = delete_states
            .iter()
            .find(|e| matches!(&e.data, LedgerEntryData::Account(a) if a.account_id == source_id))
            .expect("should have a delete state for source account");

        if let LedgerEntryData::Account(acc) = &source_delete.data {
            match &acc.ext {
                AccountEntryExt::V1(v1) => match &v1.ext {
                    AccountEntryExtensionV1Ext::V2(v2) => {
                        assert_eq!(
                            v2.num_sponsored, 2,
                            "delete STATE must use pre-op snapshot (num_sponsored=2), \
                             not mutated state (num_sponsored=0)"
                        );
                    }
                    _ => panic!("expected V2 ext"),
                },
                _ => panic!("expected V1 ext"),
            }
        } else {
            panic!("expected Account entry in delete_states");
        }
    }

    /// Regression test: in multi-op transactions, delete STATE must use the
    /// per-op snapshot (start of current op), not the per-tx snapshot (start
    /// of transaction). Op1 modifies balance, op2 deletes — delete STATE
    /// should reflect op1's result.
    #[test]
    fn test_delete_account_op_snapshot_precedence_over_tx_snapshot() {
        let mut state = new_manager_with_offers(5_000_000, 100);
        let account_id = create_test_account_id(0);

        let account = create_test_account_entry(0);
        let original_balance = account.balance;
        state.create_account(account);

        // Save tx-level snapshot — override the None from create_account
        let snapshot = state.accounts.get(&account_id).cloned();
        state.account_snapshots.insert(account_id.clone(), snapshot);

        // Op1: modify balance
        state.begin_op_snapshot();
        let mut modified = state.get_account(&account_id).unwrap().clone();
        modified.balance = 999_999;
        state.update_account(modified);
        // Flush op1 changes
        state.flush_all_accounts();
        let _op1_snapshots = state.end_op_snapshot();

        // Op2: begin new op snapshot, then delete
        state.begin_op_snapshot();

        // Access the account to capture per-op snapshot (balance=999_999)
        let ledger_key = LedgerKey::Account(LedgerKeyAccount {
            account_id: account_id.clone(),
        });
        state.capture_op_snapshot_for_key(&ledger_key);

        // Delete the account
        state.delete_account(&account_id);

        let delta = state.delta();
        let delete_states = delta.delete_states();
        let delete_state = delete_states
            .iter()
            .find(|e| matches!(&e.data, LedgerEntryData::Account(a) if a.account_id == account_id))
            .expect("should have a delete state for account");

        if let LedgerEntryData::Account(acc) = &delete_state.data {
            assert_eq!(
                acc.balance, 999_999,
                "delete STATE must use per-op snapshot (balance=999_999 from op1), \
                 not per-tx snapshot (balance={})",
                original_balance
            );
        } else {
            panic!("expected Account entry in delete_states");
        }
    }

    /// Regression test: delete STATE must preserve the pre-mutation
    /// sponsorship ext (last_modified_ledger_seq and ext fields).
    #[test]
    fn test_delete_preserves_snapshot_ext_and_last_modified() {
        let mut state = new_manager_with_offers(5_000_000, 100);
        let account_id = create_test_account_id(0);

        let account = create_test_account_entry(0);
        state.create_account(account);

        let ledger_key = LedgerKey::Account(LedgerKeyAccount {
            account_id: account_id.clone(),
        });
        let sponsor_id = create_test_account_id(1);

        // Set up sponsorship (this snapshots the pre-sponsor ext as V0)
        state.set_entry_sponsor(ledger_key.clone(), sponsor_id.clone());

        // Override snapshots to simulate: account had sponsor BEFORE this tx.
        // Account snapshot: current account state (with sponsor applied).
        let snapshot = state.accounts.get(&account_id).cloned();
        state.account_snapshots.insert(account_id.clone(), snapshot);
        // Ext snapshot: sponsor existed pre-tx.
        state
            .entry_sponsorship_snapshots
            .insert(ledger_key.clone(), Some(sponsor_id.clone()));
        state
            .entry_sponsorship_ext_snapshots
            .insert(ledger_key.clone(), true);

        // Verify entry has sponsor ext before mutation
        let pre_entry = state.get_entry(&ledger_key).unwrap();
        assert!(
            matches!(pre_entry.ext, LedgerEntryExt::V1(_)),
            "entry should have V1 ext with sponsor"
        );

        // Mutate: remove the sponsor metadata (simulates account-merge)
        state.remove_entry_sponsor(&ledger_key);

        // Delete the account
        state.delete_account(&account_id);

        let delta = state.delta();
        let delete_states = delta.delete_states();
        let delete_state = delete_states
            .iter()
            .find(|e| matches!(&e.data, LedgerEntryData::Account(a) if a.account_id == account_id))
            .expect("should have a delete state for account");

        // The delete STATE must have the pre-mutation ext (with sponsor)
        match &delete_state.ext {
            LedgerEntryExt::V1(ext_v1) => {
                assert_eq!(
                    ext_v1.sponsoring_id,
                    SponsorshipDescriptor(Some(sponsor_id)),
                    "delete STATE ext must preserve pre-mutation sponsor"
                );
            }
            _ => panic!(
                "delete STATE must have V1 ext with sponsor, got {:?}",
                delete_state.ext
            ),
        }
    }
}
