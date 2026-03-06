//! Transaction metadata construction for ledger close.
//!
//! Builds `TransactionMeta` and `LedgerCloseMeta` structures that record
//! the before/after state of every entry touched by a transaction, including
//! hot-archive restoration tracking and classic operation event emission.

use super::*;

pub(super) fn asset_to_trustline_asset(
    asset: &stellar_xdr::curr::Asset,
) -> Option<stellar_xdr::curr::TrustLineAsset> {
    match asset {
        stellar_xdr::curr::Asset::Native => None,
        stellar_xdr::curr::Asset::CreditAlphanum4(a) => Some(
            stellar_xdr::curr::TrustLineAsset::CreditAlphanum4(a.clone()),
        ),
        stellar_xdr::curr::Asset::CreditAlphanum12(a) => Some(
            stellar_xdr::curr::TrustLineAsset::CreditAlphanum12(a.clone()),
        ),
    }
}

pub(super) fn asset_issuer_id(asset: &stellar_xdr::curr::Asset) -> Option<AccountId> {
    match asset {
        stellar_xdr::curr::Asset::Native => None,
        stellar_xdr::curr::Asset::CreditAlphanum4(a) => Some(a.issuer.clone()),
        stellar_xdr::curr::Asset::CreditAlphanum12(a) => Some(a.issuer.clone()),
    }
}

pub(super) fn make_account_key(account_id: &AccountId) -> LedgerKey {
    LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
        account_id: account_id.clone(),
    })
}

pub(super) fn make_trustline_key(
    account_id: &AccountId,
    asset: &stellar_xdr::curr::TrustLineAsset,
) -> LedgerKey {
    LedgerKey::Trustline(LedgerKeyTrustLine {
        account_id: account_id.clone(),
        asset: asset.clone(),
    })
}

pub(super) fn delta_snapshot(state: &LedgerStateManager) -> DeltaSnapshot {
    let delta = state.delta();
    DeltaSnapshot {
        created: delta.created_entries().len(),
        updated: delta.updated_entries().len(),
        deleted: delta.deleted_keys().len(),
        change_order: delta.change_order().len(),
    }
}

pub(super) fn delta_changes_between(
    delta: &henyey_tx::LedgerDelta,
    start: DeltaSnapshot,
    end: DeltaSnapshot,
) -> DeltaChanges {
    let created = delta.created_entries()[start.created..end.created].to_vec();
    let updated = delta.updated_entries()[start.updated..end.updated].to_vec();
    let update_states = delta.update_states()[start.updated..end.updated].to_vec();
    let deleted = delta.deleted_keys()[start.deleted..end.deleted].to_vec();
    let delete_states = delta.delete_states()[start.deleted..end.deleted].to_vec();

    // Adjust change_order indices to be relative to the sliced vectors
    // Global indices need to be converted to local (sliced) indices
    let change_order: Vec<henyey_tx::ChangeRef> = delta.change_order()
        [start.change_order..end.change_order]
        .iter()
        .filter_map(|change_ref| {
            match change_ref {
                henyey_tx::ChangeRef::Created(idx) => {
                    // Convert global index to local: subtract start offset
                    if *idx >= start.created && *idx < end.created {
                        Some(henyey_tx::ChangeRef::Created(*idx - start.created))
                    } else {
                        None // Index out of range for this slice
                    }
                }
                henyey_tx::ChangeRef::Updated(idx) => {
                    if *idx >= start.updated && *idx < end.updated {
                        Some(henyey_tx::ChangeRef::Updated(*idx - start.updated))
                    } else {
                        None
                    }
                }
                henyey_tx::ChangeRef::Deleted(idx) => {
                    if *idx >= start.deleted && *idx < end.deleted {
                        Some(henyey_tx::ChangeRef::Deleted(*idx - start.deleted))
                    } else {
                        None
                    }
                }
            }
        })
        .collect();

    DeltaChanges {
        created,
        updated,
        update_states,
        deleted,
        delete_states,
        change_order,
    }
}

pub(super) fn allow_trust_asset(op: &AllowTrustOp, issuer: &AccountId) -> Asset {
    match &op.asset {
        AssetCode::CreditAlphanum4(code) => Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: code.clone(),
            issuer: issuer.clone(),
        }),
        AssetCode::CreditAlphanum12(code) => Asset::CreditAlphanum12(AlphaNum12 {
            asset_code: code.clone(),
            issuer: issuer.clone(),
        }),
    }
}

pub(super) fn pool_reserves(pool: &LiquidityPoolEntry) -> Option<(Asset, Asset, i64, i64)> {
    match &pool.body {
        LiquidityPoolEntryBody::LiquidityPoolConstantProduct(cp) => Some((
            cp.params.asset_a.clone(),
            cp.params.asset_b.clone(),
            cp.reserve_a,
            cp.reserve_b,
        )),
    }
}

/// Extract keys of entries being restored from the hot archive.
///
/// For InvokeHostFunction: Uses `actual_restored_indices` from the execution result,
/// which filters out entries that were already restored by a previous transaction
/// in the same ledger. This is crucial for correctness - entries listed in
/// `archived_soroban_entries` in the envelope may have been restored by a prior TX.
///
/// For RestoreFootprint: entries are from hot archive if they don't exist in live BL,
/// otherwise they're from live BL (detected separately).
///
/// Per CAP-0066, these entries should be emitted as RESTORED (not CREATED or STATE/UPDATED)
/// in the transaction meta. Both the data/code entry AND its associated TTL entry are restored.
pub(super) fn extract_hot_archive_restored_keys(
    soroban_data: Option<&SorobanTransactionData>,
    op_type: OperationType,
    actual_restored_indices: &[u32],
) -> HashSet<LedgerKey> {
    let mut keys = HashSet::new();

    let Some(data) = soroban_data else {
        return keys;
    };

    // For InvokeHostFunction: extract archived entry indices from the extension
    // For RestoreFootprint: hot archive keys are those that will be CREATED (not UPDATED)
    // We'll handle RestoreFootprint detection at change-building time
    if op_type == OperationType::RestoreFootprint {
        // Don't add all keys here - we'll detect at change-building time
        // based on whether entries are CREATED (hot archive) or UPDATED (live BL)
        return keys;
    }

    // Use actual_restored_indices instead of raw archived_soroban_entries.
    // The actual_restored_indices is filtered during host invocation to only
    // include entries that are ACTUALLY being restored in THIS transaction,
    // excluding entries already restored by a previous transaction in this ledger.
    if actual_restored_indices.is_empty() {
        return keys;
    }

    // Get the corresponding keys from the read_write footprint
    // NOTE: Only add the main entry keys (ContractData/ContractCode), NOT the TTL keys.
    // stellar-core's HotArchiveBucketList::add_batch only receives the main entry keys,
    // not TTL keys. TTL entries are handled separately in the live bucket list.
    let read_write = &data.resources.footprint.read_write;
    for index in actual_restored_indices {
        if let Some(key) = read_write.get(*index as usize) {
            keys.insert(key.clone());
        }
    }

    keys
}

pub(super) fn emit_classic_events_for_operation(
    op_event_manager: &mut OpEventManager,
    op: &Operation,
    op_result: &OperationResult,
    op_source: &MuxedAccount,
    state: &LedgerStateManager,
    pre_claimable_balance: Option<&ClaimableBalanceEntry>,
    pre_pool: Option<&LiquidityPoolEntry>,
) {
    if !op_event_manager.is_enabled() {
        return;
    }

    let source_address = make_muxed_account_address(op_source);
    match &op.body {
        OperationBody::CreateAccount(op_data) => {
            op_event_manager.new_transfer_event(
                &Asset::Native,
                &source_address,
                &make_account_address(&op_data.destination),
                op_data.starting_balance,
                true,
            );
        }
        OperationBody::Payment(op_data) => {
            op_event_manager.event_for_transfer_with_issuer_check(
                &op_data.asset,
                &source_address,
                &make_muxed_account_address(&op_data.destination),
                op_data.amount,
                true,
            );
        }
        OperationBody::PathPaymentStrictSend(op_data) => {
            if let OperationResult::OpInner(OperationResultTr::PathPaymentStrictSend(
                PathPaymentStrictSendResult::Success(success),
            )) = op_result
            {
                op_event_manager.events_for_claim_atoms(op_source, &success.offers);
                op_event_manager.event_for_transfer_with_issuer_check(
                    &op_data.dest_asset,
                    &source_address,
                    &make_muxed_account_address(&op_data.destination),
                    success.last.amount,
                    true,
                );
            }
        }
        OperationBody::PathPaymentStrictReceive(op_data) => {
            if let OperationResult::OpInner(OperationResultTr::PathPaymentStrictReceive(
                PathPaymentStrictReceiveResult::Success(success),
            )) = op_result
            {
                op_event_manager.events_for_claim_atoms(op_source, &success.offers);
                op_event_manager.event_for_transfer_with_issuer_check(
                    &op_data.dest_asset,
                    &source_address,
                    &make_muxed_account_address(&op_data.destination),
                    op_data.dest_amount,
                    true,
                );
            }
        }
        OperationBody::ManageSellOffer(_) | OperationBody::CreatePassiveSellOffer(_) => {
            if let OperationResult::OpInner(
                OperationResultTr::ManageSellOffer(ManageSellOfferResult::Success(success))
                | OperationResultTr::CreatePassiveSellOffer(ManageSellOfferResult::Success(success)),
            ) = op_result
            {
                op_event_manager.events_for_claim_atoms(op_source, &success.offers_claimed);
            }
        }
        OperationBody::ManageBuyOffer(_) => {
            if let OperationResult::OpInner(OperationResultTr::ManageBuyOffer(
                ManageBuyOfferResult::Success(success),
            )) = op_result
            {
                op_event_manager.events_for_claim_atoms(op_source, &success.offers_claimed);
            }
        }
        OperationBody::AccountMerge(dest) => {
            if let OperationResult::OpInner(OperationResultTr::AccountMerge(
                AccountMergeResult::Success(balance),
            )) = op_result
            {
                op_event_manager.new_transfer_event(
                    &Asset::Native,
                    &source_address,
                    &make_muxed_account_address(dest),
                    *balance,
                    true,
                );
            }
        }
        OperationBody::CreateClaimableBalance(op_data) => {
            if let OperationResult::OpInner(OperationResultTr::CreateClaimableBalance(
                CreateClaimableBalanceResult::Success(balance_id),
            )) = op_result
            {
                op_event_manager.event_for_transfer_with_issuer_check(
                    &op_data.asset,
                    &source_address,
                    &make_claimable_balance_address(balance_id),
                    op_data.amount,
                    true,
                );
            }
        }
        OperationBody::ClaimClaimableBalance(op_data) => {
            if let Some(entry) = pre_claimable_balance {
                op_event_manager.event_for_transfer_with_issuer_check(
                    &entry.asset,
                    &make_claimable_balance_address(&op_data.balance_id),
                    &source_address,
                    entry.amount,
                    true,
                );
            }
        }
        OperationBody::Clawback(op_data) => {
            op_event_manager.new_clawback_event(
                &op_data.asset,
                &make_muxed_account_address(&op_data.from),
                op_data.amount,
            );
        }
        OperationBody::ClawbackClaimableBalance(op_data) => {
            if let Some(entry) = pre_claimable_balance {
                op_event_manager.new_clawback_event(
                    &entry.asset,
                    &make_claimable_balance_address(&op_data.balance_id),
                    entry.amount,
                );
            }
        }
        OperationBody::AllowTrust(op_data) => {
            let issuer = henyey_tx::muxed_to_account_id(op_source);
            let asset = allow_trust_asset(op_data, &issuer);
            if let Some(trustline) = state.get_trustline(&op_data.trustor, &asset) {
                let authorize = trustline.flags & AUTHORIZED_FLAG != 0;
                op_event_manager.new_set_authorized_event(&asset, &op_data.trustor, authorize);
            }
        }
        OperationBody::SetTrustLineFlags(op_data) => {
            if let Some(trustline) = state.get_trustline(&op_data.trustor, &op_data.asset) {
                let authorize = trustline.flags & AUTHORIZED_FLAG != 0;
                op_event_manager.new_set_authorized_event(
                    &op_data.asset,
                    &op_data.trustor,
                    authorize,
                );
            }
        }
        OperationBody::LiquidityPoolDeposit(op_data) => {
            let (asset_a, asset_b, pre_a, pre_b) = match pre_pool.and_then(pool_reserves) {
                Some(values) => values,
                None => return,
            };
            let Some(post_pool) = state.get_liquidity_pool(&op_data.liquidity_pool_id) else {
                return;
            };
            let Some((_, _, post_a, post_b)) = pool_reserves(post_pool) else {
                return;
            };
            if post_a < pre_a || post_b < pre_b {
                return;
            }
            let amount_a = post_a - pre_a;
            let amount_b = post_b - pre_b;
            let pool_address = ScAddress::LiquidityPool(op_data.liquidity_pool_id.clone());
            op_event_manager.event_for_transfer_with_issuer_check(
                &asset_a,
                &source_address,
                &pool_address,
                amount_a,
                false,
            );
            op_event_manager.event_for_transfer_with_issuer_check(
                &asset_b,
                &source_address,
                &pool_address,
                amount_b,
                false,
            );
        }
        OperationBody::LiquidityPoolWithdraw(op_data) => {
            let (asset_a, asset_b, pre_a, pre_b) = match pre_pool.and_then(pool_reserves) {
                Some(values) => values,
                None => return,
            };
            let Some(post_pool) = state.get_liquidity_pool(&op_data.liquidity_pool_id) else {
                return;
            };
            let Some((_, _, post_a, post_b)) = pool_reserves(post_pool) else {
                return;
            };
            if pre_a < post_a || pre_b < post_b {
                return;
            }
            let amount_a = pre_a - post_a;
            let amount_b = pre_b - post_b;
            let pool_address = ScAddress::LiquidityPool(op_data.liquidity_pool_id.clone());
            op_event_manager.event_for_transfer_with_issuer_check(
                &asset_a,
                &pool_address,
                &source_address,
                amount_a,
                true,
            );
            op_event_manager.event_for_transfer_with_issuer_check(
                &asset_b,
                &pool_address,
                &source_address,
                amount_b,
                true,
            );
        }
        OperationBody::Inflation => {
            if let OperationResult::OpInner(OperationResultTr::Inflation(
                InflationResult::Success(payouts),
            )) = op_result
            {
                for payout in payouts.iter() {
                    op_event_manager.new_mint_event(
                        &Asset::Native,
                        &make_account_address(&payout.destination),
                        payout.amount,
                        false,
                    );
                }
            }
        }
        _ => {}
    }
}

/// Restore delta entries after a rollback.
///
/// This is used when a transaction fails - we restore the fee/seq changes
/// that were already committed before the operation rollback.
/// For updates, we use the entry as both pre-state and post-state since
/// we're just tracking the final state (the pre-state is not relevant
/// for bucket updates which is what the delta is used for).
pub(super) fn restore_delta_entries(
    state: &mut LedgerStateManager,
    created: &[LedgerEntry],
    updated: &[LedgerEntry],
    deleted: &[LedgerKey],
) {
    let delta = state.delta_mut();
    for entry in created {
        delta.record_create(entry.clone());
    }
    for entry in updated {
        // Use the entry as both pre and post state - this is a restore after rollback
        delta.record_update(entry.clone(), entry.clone());
    }
    for (i, key) in deleted.iter().enumerate() {
        // For deleted entries, we need a pre-state but don't have one
        // Try to find it from updated entries, otherwise skip
        // (In practice, fee/seq changes rarely delete entries)
        if i < updated.len() {
            delta.record_delete(key.clone(), updated[i].clone());
        }
    }
}

pub(super) fn build_entry_changes_with_state(
    state: &LedgerStateManager,
    created: &[LedgerEntry],
    updated: &[LedgerEntry],
    deleted: &[LedgerKey],
) -> LedgerEntryChanges {
    build_entry_changes_with_state_overrides(state, created, updated, deleted, &HashMap::new())
}

pub(super) fn build_entry_changes_with_state_overrides(
    state: &LedgerStateManager,
    created: &[LedgerEntry],
    updated: &[LedgerEntry],
    deleted: &[LedgerKey],
    state_overrides: &HashMap<LedgerKey, LedgerEntry>,
) -> LedgerEntryChanges {
    // Call with empty change_order and restored set for non-operation changes
    // Empty change_order triggers the fallback type-grouped ordering
    // Empty update_states/delete_states - we'll use snapshot lookup for these cases
    let empty_restored = RestoredEntries::default();
    let ledger_changes = LedgerChanges {
        created,
        updated,
        update_states: &[], // empty, will use snapshot fallback
        deleted,
        delete_states: &[], // empty, will use snapshot fallback
        change_order: &[],
        state_overrides,
        restored: &empty_restored,
    };
    build_entry_changes_with_hot_archive(
        state,
        &ledger_changes,
        None,
        0, // ledger_seq not used for non-operation changes
    )
}

/// Ledger state changes for building entry change metadata.
pub struct LedgerChanges<'a> {
    pub created: &'a [LedgerEntry],
    pub updated: &'a [LedgerEntry],
    pub update_states: &'a [LedgerEntry],
    pub deleted: &'a [LedgerKey],
    pub delete_states: &'a [LedgerEntry],
    pub change_order: &'a [henyey_tx::ChangeRef],
    pub state_overrides: &'a HashMap<LedgerKey, LedgerEntry>,
    pub restored: &'a RestoredEntries,
}

/// Build entry changes with support for hot archive and live BL restoration tracking.
///
/// For entries in `restored.hot_archive`:
/// - Emit RESTORED instead of CREATED (entry was restored from hot archive per CAP-0066)
/// - For deleted entries that were restored, emit RESTORED then REMOVED
///
/// For entries in `restored.live_bucket_list`:
/// - Convert STATE+UPDATED to RESTORED (entry had expired TTL in live BL)
/// - Emit RESTORED for associated data/code entries even if not directly modified
///
/// When `footprint` is provided (for Soroban operations), entries are ordered according to
/// the footprint's read_write order to match stellar-core behavior.
/// For classic operations, entries are ordered according to the execution order tracked
/// in `change_order` to match stellar-core behavior, emitting STATE/UPDATED pairs
/// for EACH modification (not deduplicated).
pub(super) fn build_entry_changes_with_hot_archive(
    state: &LedgerStateManager,
    changes: &LedgerChanges<'_>,
    footprint: Option<&stellar_xdr::curr::LedgerFootprint>,
    current_ledger_seq: u32,
) -> LedgerEntryChanges {
    let &LedgerChanges {
        created,
        updated,
        update_states,
        deleted,
        delete_states,
        change_order,
        state_overrides,
        restored,
    } = changes;
    // Debug: log all vectors sizes and entry types
    fn entry_type_name(entry: &LedgerEntry) -> &'static str {
        match &entry.data {
            stellar_xdr::curr::LedgerEntryData::Account(_) => "Account",
            stellar_xdr::curr::LedgerEntryData::Trustline(_) => "Trustline",
            stellar_xdr::curr::LedgerEntryData::Offer(_) => "Offer",
            stellar_xdr::curr::LedgerEntryData::Data(_) => "Data",
            stellar_xdr::curr::LedgerEntryData::ClaimableBalance(_) => "ClaimableBalance",
            stellar_xdr::curr::LedgerEntryData::LiquidityPool(_) => "LiquidityPool",
            stellar_xdr::curr::LedgerEntryData::ContractData(_) => "ContractData",
            stellar_xdr::curr::LedgerEntryData::ContractCode(_) => "ContractCode",
            stellar_xdr::curr::LedgerEntryData::ConfigSetting(_) => "ConfigSetting",
            stellar_xdr::curr::LedgerEntryData::Ttl(_) => "Ttl",
        }
    }

    // Log all created entries
    tracing::debug!(
        "build_entry_changes: VECTORS created={}, updated={}, deleted={}, change_order={}",
        created.len(),
        updated.len(),
        deleted.len(),
        change_order.len()
    );
    for (i, entry) in created.iter().enumerate() {
        tracing::debug!(
            "build_entry_changes: created[{}] = {}",
            i,
            entry_type_name(entry)
        );
    }
    for (i, entry) in updated.iter().enumerate() {
        tracing::debug!(
            "build_entry_changes: updated[{}] = {}",
            i,
            entry_type_name(entry)
        );
    }
    // Log change_order
    for (i, change_ref) in change_order.iter().enumerate() {
        tracing::debug!(
            "build_entry_changes: change_order[{}] = {:?}",
            i,
            change_ref
        );
    }

    // Debug: log all created entries to see TTL
    for (i, entry) in created.iter().enumerate() {
        if let stellar_xdr::curr::LedgerEntryData::Ttl(ttl) = &entry.data {
            tracing::debug!(
                "build_entry_changes: CREATED vector contains TTL at idx={}, key_hash={:x?}, live_until={}",
                i,
                &ttl.key_hash.0[..8],
                ttl.live_until_ledger_seq
            );
        }
    }
    // Debug: log all updated entries to see if any are TTL
    for (i, entry) in updated.iter().enumerate() {
        if let stellar_xdr::curr::LedgerEntryData::Ttl(ttl) = &entry.data {
            tracing::debug!(
                "build_entry_changes: UPDATED vector contains TTL at idx={}, key_hash={:x?}, live_until={}",
                i,
                &ttl.key_hash.0[..8],
                ttl.live_until_ledger_seq
            );
        }
    }
    // Debug: log change_order
    for (i, change_ref) in change_order.iter().enumerate() {
        match change_ref {
            henyey_tx::ChangeRef::Created(idx) => {
                if *idx < created.len() {
                    if let stellar_xdr::curr::LedgerEntryData::Ttl(ttl) = &created[*idx].data {
                        tracing::debug!(
                            "build_entry_changes: change_order[{}] = Created({}) -> TTL key_hash={:x?}, live_until={}",
                            i, idx, &ttl.key_hash.0[..8], ttl.live_until_ledger_seq
                        );
                    }
                }
            }
            henyey_tx::ChangeRef::Updated(idx) => {
                if *idx < updated.len() {
                    if let stellar_xdr::curr::LedgerEntryData::Ttl(ttl) = &updated[*idx].data {
                        tracing::debug!(
                            "build_entry_changes: change_order[{}] = Updated({}) -> TTL key_hash={:x?}, live_until={}",
                            i, idx, &ttl.key_hash.0[..8], ttl.live_until_ledger_seq
                        );
                    }
                }
            }
            _ => {}
        }
    }
    fn entry_key_bytes(entry: &LedgerEntry) -> Vec<u8> {
        crate::delta::entry_to_key(entry)
            .ok()
            .and_then(|key| key.to_xdr(Limits::none()).ok())
            .unwrap_or_default()
    }

    fn push_created_or_restored(
        changes: &mut Vec<LedgerEntryChange>,
        entry: &LedgerEntry,
        restored: &RestoredEntries,
    ) {
        if let Ok(key) = crate::delta::entry_to_key(entry) {
            // For hot archive restores and live bucket list restores (expired TTL),
            // emit RESTORED instead of CREATED.
            // This matches stellar-core's processOpLedgerEntryChanges behavior.
            if restored.hot_archive.contains(&key) || restored.live_bucket_list.contains(&key) {
                changes.push(LedgerEntryChange::Restored(entry.clone()));
                return;
            }
        }
        changes.push(LedgerEntryChange::Created(entry.clone()));
    }

    let mut changes: Vec<LedgerEntryChange> = Vec::new();

    // Build final values for each updated key (used for Soroban deduplication)
    let mut final_updated: HashMap<Vec<u8>, LedgerEntry> = HashMap::new();
    for entry in updated {
        let key_bytes = entry_key_bytes(entry);
        final_updated.insert(key_bytes, entry.clone());
    }

    // For Soroban operations with footprint, use change_order but sort consecutive Soroban creates by key_hash.
    // For classic operations, use change_order to preserve execution order.
    // Key insight: change_order captures the execution sequence. For Soroban, we must preserve
    // the positions of classic entry changes (Account, Trustline) while sorting Soroban creates
    // (TTL, ContractData, ContractCode) by their associated key_hash to match stellar-core behavior.
    if footprint.is_some() {
        use std::collections::HashSet;

        fn is_soroban_entry(entry: &LedgerEntry) -> bool {
            matches!(
                &entry.data,
                stellar_xdr::curr::LedgerEntryData::Ttl(_)
                    | stellar_xdr::curr::LedgerEntryData::ContractData(_)
                    | stellar_xdr::curr::LedgerEntryData::ContractCode(_)
            )
        }

        // Track which keys have been created (for deduplication)
        let mut created_keys: HashSet<Vec<u8>> = HashSet::new();

        // Process change_order to preserve execution sequence
        // Collect groups of changes: either single updates/deletes or consecutive Soroban creates
        enum ChangeGroup {
            SingleUpdate { idx: usize },
            SingleDelete { idx: usize },
            SorobanCreates { indices: Vec<usize> },
            ClassicCreate { idx: usize },
        }

        let mut groups: Vec<ChangeGroup> = Vec::new();
        let mut pending_soroban_creates: Vec<usize> = Vec::new();

        for change_ref in change_order {
            match change_ref {
                henyey_tx::ChangeRef::Created(idx) => {
                    if *idx < created.len() {
                        let entry = &created[*idx];
                        if is_soroban_entry(entry) {
                            pending_soroban_creates.push(*idx);
                        } else {
                            // Flush any pending Soroban creates before this classic create
                            if !pending_soroban_creates.is_empty() {
                                groups.push(ChangeGroup::SorobanCreates {
                                    indices: std::mem::take(&mut pending_soroban_creates),
                                });
                            }
                            groups.push(ChangeGroup::ClassicCreate { idx: *idx });
                        }
                    }
                }
                henyey_tx::ChangeRef::Updated(idx) => {
                    // Flush any pending Soroban creates before this update
                    if !pending_soroban_creates.is_empty() {
                        groups.push(ChangeGroup::SorobanCreates {
                            indices: std::mem::take(&mut pending_soroban_creates),
                        });
                    }
                    groups.push(ChangeGroup::SingleUpdate { idx: *idx });
                }
                henyey_tx::ChangeRef::Deleted(idx) => {
                    // Flush any pending Soroban creates before this delete
                    if !pending_soroban_creates.is_empty() {
                        groups.push(ChangeGroup::SorobanCreates {
                            indices: std::mem::take(&mut pending_soroban_creates),
                        });
                    }
                    groups.push(ChangeGroup::SingleDelete { idx: *idx });
                }
            }
        }

        // Flush any remaining Soroban creates
        if !pending_soroban_creates.is_empty() {
            groups.push(ChangeGroup::SorobanCreates {
                indices: pending_soroban_creates,
            });
        }

        // Process each group
        for group in groups {
            match group {
                ChangeGroup::SorobanCreates { indices } => {
                    // stellar-core groups TTL entries with their associated ContractData/ContractCode.
                    // Sort by (associated_key_hash, type_order) where TTL comes before its data.
                    use sha2::{Digest, Sha256};

                    fn get_associated_hash_and_type(entry: &LedgerEntry) -> (Vec<u8>, u8) {
                        match &entry.data {
                            stellar_xdr::curr::LedgerEntryData::Ttl(ttl) => {
                                // TTL: associated_hash is key_hash, type_order=0 (first)
                                (ttl.key_hash.0.to_vec(), 0)
                            }
                            stellar_xdr::curr::LedgerEntryData::ContractData(_)
                            | stellar_xdr::curr::LedgerEntryData::ContractCode(_) => {
                                // Data/Code: associated_hash is SHA256 of key XDR, type_order=1 (second)
                                if let Ok(key) = crate::delta::entry_to_key(entry) {
                                    if let Ok(key_bytes) = key.to_xdr(Limits::none()) {
                                        let key_hash = Sha256::digest(&key_bytes);
                                        return (key_hash.to_vec(), 1);
                                    }
                                }
                                (Vec::new(), 1)
                            }
                            _ => (Vec::new(), 2),
                        }
                    }

                    let mut entries_with_sort: Vec<(usize, (Vec<u8>, u8))> = indices
                        .into_iter()
                        .map(|idx| (idx, get_associated_hash_and_type(&created[idx])))
                        .collect();

                    // Sort by associated_hash (groups TTL with its data), then type_order (TTL=0 first)
                    entries_with_sort.sort_by(|(_, a), (_, b)| a.cmp(b));

                    for (idx, _) in entries_with_sort {
                        let entry = &created[idx];
                        let key_bytes = entry_key_bytes(entry);
                        if !created_keys.contains(&key_bytes) {
                            created_keys.insert(key_bytes);
                            push_created_or_restored(&mut changes, entry, restored);
                        }
                    }
                }
                ChangeGroup::ClassicCreate { idx } => {
                    let entry = &created[idx];
                    let key_bytes = entry_key_bytes(entry);
                    if !created_keys.contains(&key_bytes) {
                        created_keys.insert(key_bytes);
                        push_created_or_restored(&mut changes, entry, restored);
                    }
                }
                ChangeGroup::SingleUpdate { idx } => {
                    if idx < updated.len() {
                        let post_state = &updated[idx];
                        // Debug: trace when processing TTL update
                        if let stellar_xdr::curr::LedgerEntryData::Ttl(ttl) = &post_state.data {
                            tracing::debug!(
                                "build_entry_changes: SingleUpdate processing TTL idx={}, key_hash={:x?}, live_until={}",
                                idx, &ttl.key_hash.0[..8], ttl.live_until_ledger_seq
                            );
                        }
                        if let Ok(key) = crate::delta::entry_to_key(post_state) {
                            // NOTE: RO TTL bumps ARE included in transaction meta (per stellar-core
                            // setLedgerChangesFromSuccessfulOp which uses raw res.getModifiedEntryMap()).
                            // The filtering to mRoTTLBumps only affects STATE updates (commitChangesFromSuccessfulOp),
                            // not transaction meta. Do NOT skip ro_ttl_keys here.
                            if restored.hot_archive.contains(&key)
                                || restored.live_bucket_list.contains(&key)
                            {
                                changes.push(LedgerEntryChange::Restored(post_state.clone()));
                            } else {
                                // Get pre-state from update_states or snapshot
                                let pre_state = if idx < update_states.len() {
                                    Some(update_states[idx].clone())
                                } else {
                                    state_overrides
                                        .get(&key)
                                        .cloned()
                                        .or_else(|| state.snapshot_entry(&key))
                                };
                                // Debug: trace TTL pre_state
                                if let stellar_xdr::curr::LedgerEntryData::Ttl(ttl) =
                                    &post_state.data
                                {
                                    if let Some(ref state_entry) = pre_state {
                                        if let stellar_xdr::curr::LedgerEntryData::Ttl(pre_ttl) =
                                            &state_entry.data
                                        {
                                            tracing::debug!(
                                                "build_entry_changes: TTL STATE+UPDATED: pre live_until={}, post live_until={}",
                                                pre_ttl.live_until_ledger_seq, ttl.live_until_ledger_seq
                                            );
                                        }
                                    } else {
                                        tracing::debug!(
                                            "build_entry_changes: TTL UPDATED without pre_state! key_hash={:x?}",
                                            &ttl.key_hash.0[..8]
                                        );
                                    }
                                }
                                if let Some(state_entry) = pre_state {
                                    changes.push(LedgerEntryChange::State(state_entry));
                                }
                                changes.push(LedgerEntryChange::Updated(post_state.clone()));
                            }
                        }
                    }
                }
                ChangeGroup::SingleDelete { idx } => {
                    if idx < deleted.len() {
                        let key = &deleted[idx];
                        if restored.hot_archive.contains(key)
                            || restored.live_bucket_list.contains(key)
                        {
                            let pre_state = if idx < delete_states.len() {
                                Some(delete_states[idx].clone())
                            } else {
                                state_overrides
                                    .get(key)
                                    .cloned()
                                    .or_else(|| state.snapshot_entry(key))
                            };
                            if let Some(state_entry) = pre_state {
                                changes.push(LedgerEntryChange::Restored(state_entry));
                            }
                            changes.push(LedgerEntryChange::Removed(key.clone()));
                        } else {
                            let pre_state = if idx < delete_states.len() {
                                Some(delete_states[idx].clone())
                            } else {
                                state_overrides
                                    .get(key)
                                    .cloned()
                                    .or_else(|| state.snapshot_entry(key))
                            };
                            if let Some(state_entry) = pre_state {
                                changes.push(LedgerEntryChange::State(state_entry));
                            }
                            changes.push(LedgerEntryChange::Removed(key.clone()));
                        }
                    }
                }
            }
        }
    } else if !change_order.is_empty() {
        // For classic operations with change_order, use it to preserve execution order.
        // Only deduplicate creates - once an entry is created, subsequent references are updates.
        // Updates are NOT deduplicated - each update in change_order gets its own STATE/UPDATED pair.
        use std::collections::HashSet;

        // Track which keys have been created to avoid duplicate creates
        let mut created_keys: HashSet<Vec<u8>> = HashSet::new();

        for change_ref in change_order {
            match change_ref {
                henyey_tx::ChangeRef::Created(idx) => {
                    if *idx < created.len() {
                        let entry = &created[*idx];
                        let key_bytes = entry_key_bytes(entry);
                        // Only emit create once per key
                        if !created_keys.contains(&key_bytes) {
                            created_keys.insert(key_bytes);
                            push_created_or_restored(&mut changes, entry, restored);
                        }
                    }
                }
                henyey_tx::ChangeRef::Updated(idx) => {
                    if *idx < updated.len() {
                        let post_state = &updated[*idx];

                        if let Ok(key) = crate::delta::entry_to_key(post_state) {
                            if restored.hot_archive.contains(&key)
                                || restored.live_bucket_list.contains(&key)
                            {
                                // Use entry value for hot archive restored entries
                                changes.push(LedgerEntryChange::Restored(post_state.clone()));
                            } else {
                                // Normal update: STATE (pre-state) then UPDATED (post-state)
                                // Use the pre-state stored in the delta at the same index
                                let pre_state = if *idx < update_states.len() {
                                    Some(update_states[*idx].clone())
                                } else {
                                    // Fallback to snapshot lookup if pre-state not available
                                    state_overrides
                                        .get(&key)
                                        .cloned()
                                        .or_else(|| state.snapshot_entry(&key))
                                };
                                if let Some(state_entry) = pre_state {
                                    changes.push(LedgerEntryChange::State(state_entry));
                                }
                                changes.push(LedgerEntryChange::Updated(post_state.clone()));
                            }
                        }
                    }
                }
                henyey_tx::ChangeRef::Deleted(idx) => {
                    if *idx < deleted.len() {
                        let key = &deleted[*idx];
                        if restored.hot_archive.contains(key)
                            || restored.live_bucket_list.contains(key)
                        {
                            // Use the pre-state stored in the delta at the same index
                            let pre_state = if *idx < delete_states.len() {
                                Some(delete_states[*idx].clone())
                            } else {
                                state_overrides
                                    .get(key)
                                    .cloned()
                                    .or_else(|| state.snapshot_entry(key))
                            };
                            if let Some(state_entry) = pre_state {
                                changes.push(LedgerEntryChange::Restored(state_entry));
                            }
                            changes.push(LedgerEntryChange::Removed(key.clone()));
                        } else {
                            // Use the pre-state stored in the delta at the same index
                            let pre_state = if *idx < delete_states.len() {
                                Some(delete_states[*idx].clone())
                            } else {
                                state_overrides
                                    .get(key)
                                    .cloned()
                                    .or_else(|| state.snapshot_entry(key))
                            };
                            if let Some(state_entry) = pre_state {
                                changes.push(LedgerEntryChange::State(state_entry));
                            }
                            changes.push(LedgerEntryChange::Removed(key.clone()));
                        }
                    }
                }
            }
        }
    } else {
        // Fallback: no change_order available (e.g., fee/seq changes)
        // Use type-grouped order: deleted -> updated -> created
        for key in deleted {
            if restored.hot_archive.contains(key) || restored.live_bucket_list.contains(key) {
                if let Some(state_entry) = state_overrides
                    .get(key)
                    .cloned()
                    .or_else(|| state.snapshot_entry(key))
                {
                    changes.push(LedgerEntryChange::Restored(state_entry));
                }
                changes.push(LedgerEntryChange::Removed(key.clone()));
            } else {
                if let Some(state_entry) = state_overrides
                    .get(key)
                    .cloned()
                    .or_else(|| state.snapshot_entry(key))
                {
                    changes.push(LedgerEntryChange::State(state_entry));
                }
                changes.push(LedgerEntryChange::Removed(key.clone()));
            }
        }

        // Deduplicate updated entries
        use std::collections::HashSet;
        let mut seen_keys: HashSet<Vec<u8>> = HashSet::new();
        for entry in updated {
            let key_bytes = entry_key_bytes(entry);
            if !seen_keys.contains(&key_bytes) {
                seen_keys.insert(key_bytes.clone());
                if let Some(final_entry) = final_updated.get(&key_bytes) {
                    if let Ok(key) = crate::delta::entry_to_key(final_entry) {
                        if restored.hot_archive.contains(&key)
                            || restored.live_bucket_list.contains(&key)
                        {
                            changes.push(LedgerEntryChange::Restored(final_entry.clone()));
                        } else {
                            if let Some(state_entry) = state_overrides
                                .get(&key)
                                .cloned()
                                .or_else(|| state.snapshot_entry(&key))
                            {
                                changes.push(LedgerEntryChange::State(state_entry));
                            }
                            changes.push(LedgerEntryChange::Updated(final_entry.clone()));
                        }
                    } else {
                        changes.push(LedgerEntryChange::Updated(final_entry.clone()));
                    }
                }
            }
        }

        for entry in created {
            push_created_or_restored(&mut changes, entry, restored);
        }
    }

    // For live BL restores, add RESTORED changes for data/code entries that weren't
    // directly modified (only their TTL was extended). Per stellar-core TransactionMeta.cpp:
    // "RestoreOp will create both the TTL and Code/Data entry in the hot archive case.
    // However, when restoring from live BucketList, only the TTL value will be modified,
    // so we have to manually insert the RESTORED meta for the Code/Data entry here."
    for (key, entry) in &restored.live_bucket_list_entries {
        // Skip if this key was already processed (appears in updated or created)
        let key_bytes = key.to_xdr(Limits::none()).unwrap_or_default();
        let already_processed = changes.iter().any(|change| {
            let change_key = match change {
                LedgerEntryChange::State(e)
                | LedgerEntryChange::Created(e)
                | LedgerEntryChange::Updated(e)
                | LedgerEntryChange::Restored(e) => crate::delta::entry_to_key(e).ok(),
                LedgerEntryChange::Removed(k) => Some(k.clone()),
            };
            change_key
                .and_then(|k| k.to_xdr(Limits::none()).ok())
                .map(|b| b == key_bytes)
                .unwrap_or(false)
        });

        if !already_processed {
            changes.push(LedgerEntryChange::Restored(entry.clone()));
        }
    }

    // For hot archive restores (RestoreFootprint), add RESTORED changes for data/code entries
    // that weren't directly modified (the entry is prefetched from hot archive, only TTL is created).
    // This is similar to live BL restores above.
    // When emitting RESTORED, we must update last_modified_ledger_seq to the current ledger,
    // matching stellar-core behavior.
    for (key, entry) in &restored.hot_archive_entries {
        // Skip if this key was already processed (appears in updated or created)
        let key_bytes = key.to_xdr(Limits::none()).unwrap_or_default();
        let already_processed = changes.iter().any(|change| {
            let change_key = match change {
                LedgerEntryChange::State(e)
                | LedgerEntryChange::Created(e)
                | LedgerEntryChange::Updated(e)
                | LedgerEntryChange::Restored(e) => crate::delta::entry_to_key(e).ok(),
                LedgerEntryChange::Removed(k) => Some(k.clone()),
            };
            change_key
                .and_then(|k| k.to_xdr(Limits::none()).ok())
                .map(|b| b == key_bytes)
                .unwrap_or(false)
        });

        if !already_processed {
            // Clone the entry and update last_modified_ledger_seq to current ledger
            let mut restored_entry = entry.clone();
            restored_entry.last_modified_ledger_seq = current_ledger_seq;
            changes.push(LedgerEntryChange::Restored(restored_entry));
        }
    }

    // Debug: log final output
    for (i, change) in changes.iter().enumerate() {
        let change_type = match change {
            LedgerEntryChange::State(_) => "STATE",
            LedgerEntryChange::Created(_) => "CREATED",
            LedgerEntryChange::Updated(_) => "UPDATED",
            LedgerEntryChange::Restored(_) => "RESTORED",
            LedgerEntryChange::Removed(_) => "REMOVED",
        };
        tracing::debug!("build_entry_changes: OUTPUT[{}] = {}", i, change_type);
    }

    LedgerEntryChanges(changes.try_into().unwrap_or_default())
}

pub(super) fn empty_entry_changes() -> LedgerEntryChanges {
    LedgerEntryChanges(VecM::default())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build_transaction_meta(
    tx_changes_before: LedgerEntryChanges,
    op_changes: Vec<LedgerEntryChanges>,
    op_events: Vec<Vec<ContractEvent>>,
    tx_events: Vec<TransactionEvent>,
    soroban_return_value: Option<stellar_xdr::curr::ScVal>,
    diagnostic_events: Vec<DiagnosticEvent>,
    soroban_fee_info: Option<(i64, i64, i64)>, // (non_refundable, refundable_consumed, rent_consumed)
    emit_soroban_tx_meta_ext_v1: bool,
    enable_soroban_diagnostic_events: bool,
) -> TransactionMeta {
    // Debug: log tx_changes_before and op_changes counts
    let tx_before_count = tx_changes_before.len();
    let op_counts: Vec<usize> = op_changes.iter().map(|c| c.len()).collect();
    let total_op_changes: usize = op_counts.iter().sum();
    tracing::debug!(
        "build_transaction_meta: tx_changes_before={}, op_changes={:?}, total_op_changes={}, grand_total={}",
        tx_before_count, op_counts, total_op_changes, tx_before_count + total_op_changes
    );

    let operations: Vec<OperationMetaV2> = op_changes
        .into_iter()
        .zip(op_events)
        .map(|(changes, events)| OperationMetaV2 {
            ext: ExtensionPoint::V0,
            changes,
            events: events.try_into().unwrap_or_default(),
        })
        .collect();

    // Filter diagnostic events based on config flag.
    // The Soroban host always captures diagnostic events (enable_diagnostics: true),
    // but we only include them in the meta stream when the config flag is set.
    let filtered_diagnostics = if enable_soroban_diagnostic_events {
        diagnostic_events
    } else {
        Vec::new()
    };

    let has_soroban = soroban_return_value.is_some()
        || !filtered_diagnostics.is_empty()
        || soroban_fee_info.is_some();
    let soroban_meta = if has_soroban {
        // Only emit SorobanTransactionMetaExtV1 (fee breakdown) when the flag is set.
        // This matches stellar-core's EMIT_SOROBAN_TRANSACTION_META_EXT_V1 behavior.
        let ext = if emit_soroban_tx_meta_ext_v1 {
            if let Some((non_refundable, refundable_consumed, rent_consumed)) = soroban_fee_info {
                SorobanTransactionMetaExt::V1(SorobanTransactionMetaExtV1 {
                    ext: ExtensionPoint::V0,
                    total_non_refundable_resource_fee_charged: non_refundable,
                    total_refundable_resource_fee_charged: refundable_consumed,
                    rent_fee_charged: rent_consumed,
                })
            } else {
                SorobanTransactionMetaExt::V0
            }
        } else {
            SorobanTransactionMetaExt::V0
        };
        Some(SorobanTransactionMetaV2 {
            ext,
            return_value: soroban_return_value,
        })
    } else {
        None
    };

    TransactionMeta::V4(TransactionMetaV4 {
        ext: ExtensionPoint::V0,
        tx_changes_before,
        operations: operations.try_into().unwrap_or_default(),
        tx_changes_after: empty_entry_changes(),
        soroban_meta,
        events: tx_events.try_into().unwrap_or_default(),
        diagnostic_events: filtered_diagnostics.try_into().unwrap_or_default(),
    })
}

pub(super) fn empty_transaction_meta() -> TransactionMeta {
    build_transaction_meta(
        empty_entry_changes(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        None,
        Vec::new(),
        None,
        false,
        false,
    )
}
