//! Transaction set utility functions.
//!
//! This module provides utility functions for filtering invalid transactions
//! from candidate transaction sets, plus common envelope accessors used
//! throughout the herder crate.
//!
//! # Parity
//!
//! Mirrors `TxSetUtils::getInvalidTxList()` and `TxSetUtils::trimInvalid()`
//! from stellar-core `src/herder/TxSetUtils.cpp`.

use std::collections::{HashMap, HashSet};

use henyey_common::protocol::{protocol_version_starts_from, ProtocolVersion};
use henyey_common::{Hash256, NetworkId};
use henyey_ledger::SorobanNetworkInfo;
use henyey_tx::{
    check_valid_pre_seq_num, soroban_disk_read_entries, validate_basic, LedgerContext,
    TransactionFrame,
};
use stellar_xdr::curr::{
    AccountId, GeneralizedTransactionSet, LedgerHeader, SorobanTransactionDataExt,
    TransactionEnvelope, TransactionPhase, TxSetComponent,
};
use tracing::debug;

use crate::tx_queue::FeeBalanceProvider;

/// Get the declared fee from a transaction envelope.
///
/// For fee-bump transactions, returns the outer (bumped) fee.
pub(crate) fn envelope_fee(env: &TransactionEnvelope) -> i64 {
    match env {
        TransactionEnvelope::TxV0(e) => e.tx.fee as i64,
        TransactionEnvelope::Tx(e) => e.tx.fee as i64,
        TransactionEnvelope::TxFeeBump(e) => e.tx.fee,
    }
}

/// Get the fee bid used for transaction ordering and surge pricing.
///
/// For Soroban transactions this is the inclusion fee (full fee minus resource fee),
/// matching stellar-core `TransactionFrameBase::getInclusionFee()`.
pub(crate) fn envelope_inclusion_fee(env: &TransactionEnvelope) -> i64 {
    let resource_fee = match env {
        TransactionEnvelope::TxV0(_) => 0,
        TransactionEnvelope::Tx(env) => match &env.tx.ext {
            stellar_xdr::curr::TransactionExt::V0 => 0,
            stellar_xdr::curr::TransactionExt::V1(data) => data.resource_fee,
        },
        TransactionEnvelope::TxFeeBump(env) => match &env.tx.inner_tx {
            stellar_xdr::curr::FeeBumpTransactionInnerTx::Tx(inner) => match &inner.tx.ext {
                stellar_xdr::curr::TransactionExt::V0 => 0,
                stellar_xdr::curr::TransactionExt::V1(data) => data.resource_fee,
            },
        },
    };
    envelope_fee(env).saturating_sub(resource_fee)
}

/// Get the number of operations from a transaction envelope.
///
/// For fee-bump transactions, returns the inner transaction's operation count
/// plus 1 for the fee-bump wrapper itself, matching stellar-core's
/// `FeeBumpTransactionFrame::getNumOperations()`.
pub(crate) fn envelope_num_ops(env: &TransactionEnvelope) -> usize {
    match env {
        TransactionEnvelope::TxV0(e) => e.tx.operations.len(),
        TransactionEnvelope::Tx(e) => e.tx.operations.len(),
        TransactionEnvelope::TxFeeBump(e) => match &e.tx.inner_tx {
            stellar_xdr::curr::FeeBumpTransactionInnerTx::Tx(inner) => {
                inner.tx.operations.len() + 1
            }
        },
    }
}

/// Parameters for close-time bounds validation.
///
/// In upstream stellar-core, `lowerBoundCloseTimeOffset` and
/// `upperBoundCloseTimeOffset` are used to create a range of possible close
/// times during nomination (since the exact close time is not yet known).
/// The lower bound is `closeTime + lowerOffset` and the upper bound is
/// `closeTime + upperOffset`.
///
/// For post-ledger-close queue cleanup, use `with_offsets(0, upper_bound)`
/// where `upper_bound` is computed like stellar-core's
/// `getUpperBoundCloseTimeOffset()` to avoid prematurely banning transactions
/// whose `max_time` may still be valid for the next ledger.
#[derive(Debug, Clone, Copy)]
pub struct CloseTimeBounds {
    /// Offset added to close time for the lower bound check (min_time validation).
    pub lower_bound_offset: u64,
    /// Offset added to close time for the upper bound check (max_time validation).
    pub upper_bound_offset: u64,
}

impl CloseTimeBounds {
    /// Create bounds with no offset (exact close time).
    pub fn exact() -> Self {
        Self {
            lower_bound_offset: 0,
            upper_bound_offset: 0,
        }
    }

    /// Create bounds with the given offsets.
    pub fn with_offsets(lower_bound_offset: u64, upper_bound_offset: u64) -> Self {
        Self {
            lower_bound_offset,
            upper_bound_offset,
        }
    }
}

impl Default for CloseTimeBounds {
    fn default() -> Self {
        Self::exact()
    }
}

/// Validation context for transaction set filtering.
///
/// This provides the ledger state information needed to validate transactions
/// against the next ledger (LCL + 1), matching upstream's approach of creating
/// a `LedgerSnapshot` with `ledgerSeq = lastClosedLedgerNum + 1`.
#[derive(Debug, Clone)]
pub struct TxSetValidationContext {
    /// Next ledger sequence (LCL + 1).
    pub next_ledger_seq: u32,
    /// Close time for the next ledger.
    pub close_time: u64,
    /// Base fee per operation in stroops.
    pub base_fee: u32,
    /// Base reserve per ledger entry in stroops.
    pub base_reserve: u32,
    /// Protocol version number.
    pub protocol_version: u32,
    /// Network identifier.
    pub network_id: NetworkId,
    /// Ledger header flags (LP disable flags etc.).
    pub ledger_flags: u32,
}

impl TxSetValidationContext {
    /// Create a context for validating against the next ledger.
    ///
    /// # Arguments
    ///
    /// * `last_closed_ledger_seq` - The last closed ledger sequence number (LCL).
    ///   The validation will use `LCL + 1` as the ledger sequence, matching upstream.
    /// * `close_time` - Close time for the next ledger.
    /// * `base_fee` - Base fee per operation.
    /// * `base_reserve` - Base reserve per ledger entry.
    /// * `protocol_version` - Protocol version.
    /// * `network_id` - Network identifier.
    /// * `ledger_flags` - Ledger header flags (LP disable flags etc.).
    pub fn new(
        last_closed_ledger_seq: u32,
        close_time: u64,
        base_fee: u32,
        base_reserve: u32,
        protocol_version: u32,
        network_id: NetworkId,
        ledger_flags: u32,
    ) -> Self {
        Self {
            next_ledger_seq: last_closed_ledger_seq.saturating_add(1),
            close_time,
            base_fee,
            base_reserve,
            protocol_version,
            network_id,
            ledger_flags,
        }
    }

    /// Build a `LedgerContext` from this validation context.
    fn to_ledger_context(&self, close_time: u64) -> LedgerContext {
        let mut ctx = LedgerContext::new(
            self.next_ledger_seq,
            close_time,
            self.base_fee,
            self.base_reserve,
            self.protocol_version,
            self.network_id,
        );
        ctx.ledger_flags = self.ledger_flags;
        ctx
    }
}

/// Returns the list of invalid transactions from the given set.
///
/// Each transaction is validated using `validate_basic` against a ledger context
/// constructed for the next ledger (LCL + 1). Transactions that fail validation
/// are collected and returned.
///
/// When a `fee_balance_provider` is supplied, the function also performs a
/// second pass that groups valid transactions by fee source, accumulates their
/// total fees, and marks **all** transactions from a fee source as invalid if
/// the account's available balance is insufficient to cover the total fees.
///
/// # Parity
///
/// Mirrors `TxSetUtils::getInvalidTxList()` in stellar-core
/// (`src/herder/TxSetUtils.cpp`). The upstream validates against a
/// `LedgerSnapshot` with `ledgerSeq = lastClosedLedgerNum + 1` and performs
/// the fee-source affordability check in the same function.
///
/// # Arguments
///
/// * `txs` - List of candidate transaction envelopes.
/// * `ctx` - Validation context (next ledger seq, close time, fees, network).
/// * `close_time_bounds` - Offsets for close-time range during nomination.
/// * `fee_balance_provider` - Optional provider for account balance lookups.
///   When `None`, the fee-source affordability check is skipped.
///
/// # Returns
///
/// A vector of transaction envelopes that failed validation.
pub fn get_invalid_tx_list(
    txs: &[TransactionEnvelope],
    ctx: &TxSetValidationContext,
    close_time_bounds: &CloseTimeBounds,
    fee_balance_provider: Option<&dyn FeeBalanceProvider>,
) -> Vec<TransactionEnvelope> {
    let mut account_fee_map: HashMap<AccountId, i64> = HashMap::new();
    get_invalid_tx_list_with_fee_map(
        txs,
        ctx,
        close_time_bounds,
        fee_balance_provider,
        &mut account_fee_map,
    )
}

/// Returns the list of invalid transactions, using a shared fee-source map.
///
/// For Protocol 26+, the `account_fee_map` should be shared across both
/// Classic and Soroban phases so that a fee source appearing in both phases
/// has its total fees summed correctly.
///
/// # Parity
///
/// Mirrors `TxSetUtils::getInvalidTxListWithErrors()` in stellar-core,
/// which accepts `accountFeeMap` by reference.
pub fn get_invalid_tx_list_with_fee_map(
    txs: &[TransactionEnvelope],
    ctx: &TxSetValidationContext,
    close_time_bounds: &CloseTimeBounds,
    fee_balance_provider: Option<&dyn FeeBalanceProvider>,
    account_fee_map: &mut HashMap<AccountId, i64>,
) -> Vec<TransactionEnvelope> {
    let mut invalid_txs = Vec::new();
    let mut seen_invalid: HashSet<Hash256> = HashSet::new();

    // For time bounds validation during nomination, upstream uses the
    // upper bound close time for max_time checks and lower bound for
    // min_time checks. We approximate this by validating with the upper
    // bound close time (which is the more permissive direction for max_time)
    // and then checking again with the lower bound for min_time.
    //
    // When both offsets are 0 (post-close validation), this simplifies to
    // a single validation with the exact close time.
    let upper_close_time = ctx
        .close_time
        .saturating_add(close_time_bounds.upper_bound_offset);
    let lower_close_time = ctx
        .close_time
        .saturating_add(close_time_bounds.lower_bound_offset);

    let upper_ledger_ctx = ctx.to_ledger_context(upper_close_time);
    // Only build lower context if offsets differ (optimization for common case).
    let need_lower_check = lower_close_time != upper_close_time;

    for tx in txs {
        let frame = TransactionFrame::from_owned_with_network(tx.clone(), ctx.network_id);

        // Stateless structural + per-op validation (shared with queue admission).
        if check_valid_pre_seq_num(&frame, ctx.protocol_version, ctx.ledger_flags).is_err() {
            if let Ok(h) = Hash256::hash_xdr(tx) {
                seen_invalid.insert(h);
            }
            invalid_txs.push(tx.clone());
            continue;
        }

        // Validate with upper bound close time (catches max_time violations).
        let upper_result = validate_basic(&frame, &upper_ledger_ctx);

        if upper_result.is_err() {
            if let Ok(h) = Hash256::hash_xdr(tx) {
                seen_invalid.insert(h);
            }
            invalid_txs.push(tx.clone());
            continue;
        }

        // If offsets differ, also validate with lower bound close time
        // (catches min_time violations).
        if need_lower_check {
            let lower_ledger_ctx = ctx.to_ledger_context(lower_close_time);
            if validate_basic(&frame, &lower_ledger_ctx).is_err() {
                if let Ok(h) = Hash256::hash_xdr(tx) {
                    seen_invalid.insert(h);
                }
                invalid_txs.push(tx.clone());
                continue;
            }
        }

        // Transaction passed basic validation — accumulate fee for fee source.
        if fee_balance_provider.is_some() {
            let fee_source = frame.fee_source_account_id();
            let full_fee = frame.total_fee();
            let entry = account_fee_map.entry(fee_source).or_insert(0i64);
            // Saturating add to avoid overflow (matches stellar-core).
            *entry = entry.saturating_add(full_fee);
        }
    }

    // --- Pass 2: fee-source affordability check ---
    if let Some(provider) = fee_balance_provider {
        for tx in txs {
            // Skip transactions already marked invalid.
            if let Ok(h) = Hash256::hash_xdr(tx) {
                if seen_invalid.contains(&h) {
                    continue;
                }
            }

            let frame = TransactionFrame::from_owned_with_network(tx.clone(), ctx.network_id);
            let fee_source = frame.fee_source_account_id();

            let available = provider.get_available_balance(&fee_source).unwrap_or(0);
            let total_fee = account_fee_map.get(&fee_source).copied().unwrap_or(0);

            if available < total_fee {
                invalid_txs.push(tx.clone());
                if let Ok(h) = Hash256::hash_xdr(tx) {
                    seen_invalid.insert(h);
                }
                tracing::debug!(
                    fee_source = ?fee_source,
                    available_balance = available,
                    total_fee = total_fee,
                    "tx-set validation: account can't pay fee"
                );
            }
        }
    }

    invalid_txs
}

/// Filter invalid transactions from a candidate set.
///
/// Finds all invalid transactions using [`get_invalid_tx_list`], then removes
/// them from the input set using hash comparison.
///
/// # Parity
///
/// Mirrors `TxSetUtils::trimInvalid()` in stellar-core
/// (`src/herder/TxSetUtils.cpp`).
///
/// # Arguments
///
/// * `txs` - List of candidate transaction envelopes.
/// * `ctx` - Validation context (next ledger seq, close time, fees, network).
/// * `close_time_bounds` - Offsets for close-time range during nomination.
/// * `fee_balance_provider` - Optional provider for fee-source affordability checks.
///
/// # Returns
///
/// A tuple of `(valid_txs, invalid_txs)` where:
/// - `valid_txs` - Transactions that passed validation.
/// - `invalid_txs` - Transactions that failed validation.
pub fn trim_invalid(
    txs: &[TransactionEnvelope],
    ctx: &TxSetValidationContext,
    close_time_bounds: &CloseTimeBounds,
    fee_balance_provider: Option<&dyn FeeBalanceProvider>,
) -> (Vec<TransactionEnvelope>, Vec<TransactionEnvelope>) {
    let invalid_txs = get_invalid_tx_list(txs, ctx, close_time_bounds, fee_balance_provider);

    if invalid_txs.is_empty() {
        return (txs.to_vec(), Vec::new());
    }

    let valid_txs = remove_txs(txs, &invalid_txs);
    (valid_txs, invalid_txs)
}

/// Remove a subset of transactions from a list using hash comparison.
///
/// This is equivalent to the upstream `removeTxs()` helper in `TxSetUtils.cpp`.
fn remove_txs(
    txs: &[TransactionEnvelope],
    txs_to_remove: &[TransactionEnvelope],
) -> Vec<TransactionEnvelope> {
    let remove_set: HashSet<Hash256> = txs_to_remove
        .iter()
        .filter_map(|tx| Hash256::hash_xdr(tx).ok())
        .collect();

    txs.iter()
        .filter(|tx| {
            Hash256::hash_xdr(*tx)
                .map(|h| !remove_set.contains(&h))
                .unwrap_or(true)
        })
        .cloned()
        .collect()
}

/// Trim invalid transactions from two phases (Classic + Soroban), sharing the
/// fee-source map across phases for Protocol 26+.
///
/// For V26+, the `account_fee_map` is accumulated across both phases so that
/// a fee source appearing in both Classic and Soroban phases has its total fees
/// summed correctly. For pre-V26, the map is cleared between phases.
///
/// # Parity
///
/// Mirrors `makeTxSetFromTransactions` in stellar-core `TxSetFrame.cpp:836-860`
/// where `trimInvalid` is called per-phase with a shared `accountFeeMap`.
///
/// Also mirrors `checkValidInternalWithResult` at `TxSetFrame.cpp:2168-2183`
/// where `accountFeeMap` is conditionally cleared between phases based on V26.
pub fn trim_invalid_two_phase(
    classic_txs: &[TransactionEnvelope],
    soroban_txs: &[TransactionEnvelope],
    ctx: &TxSetValidationContext,
    close_time_bounds: &CloseTimeBounds,
    fee_balance_provider: Option<&dyn FeeBalanceProvider>,
) -> (Vec<TransactionEnvelope>, Vec<TransactionEnvelope>) {
    use henyey_common::protocol::{protocol_version_starts_from, ProtocolVersion};

    let use_cross_phase_fee_map =
        protocol_version_starts_from(ctx.protocol_version, ProtocolVersion::V26);

    let mut account_fee_map: HashMap<AccountId, i64> = HashMap::new();

    // Phase 0: Classic
    let classic_invalid = get_invalid_tx_list_with_fee_map(
        classic_txs,
        ctx,
        close_time_bounds,
        fee_balance_provider,
        &mut account_fee_map,
    );
    let valid_classic = if classic_invalid.is_empty() {
        classic_txs.to_vec()
    } else {
        remove_txs(classic_txs, &classic_invalid)
    };

    // For pre-V26, clear the fee map between phases (each phase is independent).
    if !use_cross_phase_fee_map {
        account_fee_map.clear();
    }

    // Phase 1: Soroban
    let soroban_invalid = get_invalid_tx_list_with_fee_map(
        soroban_txs,
        ctx,
        close_time_bounds,
        fee_balance_provider,
        &mut account_fee_map,
    );
    let valid_soroban = if soroban_invalid.is_empty() {
        soroban_txs.to_vec()
    } else {
        remove_txs(soroban_txs, &soroban_invalid)
    };

    (valid_classic, valid_soroban)
}

// ---------------------------------------------------------------------------
// TX set content validation functions (AUDIT-033)
// ---------------------------------------------------------------------------

/// Check if a transaction envelope is a Soroban transaction.
fn is_soroban_envelope(env: &TransactionEnvelope) -> bool {
    let ops = match env {
        TransactionEnvelope::TxV0(e) => &e.tx.operations,
        TransactionEnvelope::Tx(e) => &e.tx.operations,
        TransactionEnvelope::TxFeeBump(e) => match &e.tx.inner_tx {
            stellar_xdr::curr::FeeBumpTransactionInnerTx::Tx(inner) => &inner.tx.operations,
        },
    };
    ops.iter().any(|op| {
        matches!(
            op.body,
            stellar_xdr::curr::OperationBody::InvokeHostFunction(_)
                | stellar_xdr::curr::OperationBody::ExtendFootprintTtl(_)
                | stellar_xdr::curr::OperationBody::RestoreFootprint(_)
        )
    })
}

/// Extract `SorobanResources` from a transaction envelope (for Soroban TXs).
fn envelope_soroban_resources(
    env: &TransactionEnvelope,
) -> Option<&stellar_xdr::curr::SorobanResources> {
    let ext = match env {
        TransactionEnvelope::Tx(e) => &e.tx.ext,
        TransactionEnvelope::TxFeeBump(e) => match &e.tx.inner_tx {
            stellar_xdr::curr::FeeBumpTransactionInnerTx::Tx(inner) => &inner.tx.ext,
        },
        TransactionEnvelope::TxV0(_) => return None,
    };
    match ext {
        stellar_xdr::curr::TransactionExt::V1(data) => Some(&data.resources),
        _ => None,
    }
}

/// Extract `SorobanTransactionDataExt` from a transaction envelope.
fn envelope_soroban_data_ext(env: &TransactionEnvelope) -> Option<&SorobanTransactionDataExt> {
    let ext = match env {
        TransactionEnvelope::Tx(e) => &e.tx.ext,
        TransactionEnvelope::TxFeeBump(e) => match &e.tx.inner_tx {
            stellar_xdr::curr::FeeBumpTransactionInnerTx::Tx(inner) => &inner.tx.ext,
        },
        _ => return None,
    };
    match ext {
        stellar_xdr::curr::TransactionExt::V1(data) => Some(&data.ext),
        _ => None,
    }
}

/// Check if a transaction is a RestoreFootprint operation.
fn is_restore_footprint_envelope(env: &TransactionEnvelope) -> bool {
    let ops = match env {
        TransactionEnvelope::TxV0(e) => &e.tx.operations,
        TransactionEnvelope::Tx(e) => &e.tx.operations,
        TransactionEnvelope::TxFeeBump(e) => match &e.tx.inner_tx {
            stellar_xdr::curr::FeeBumpTransactionInnerTx::Tx(inner) => &inner.tx.operations,
        },
    };
    ops.len() == 1
        && matches!(
            ops[0].body,
            stellar_xdr::curr::OperationBody::RestoreFootprint(_)
        )
}

/// Validate that component base fees and per-TX inclusion fees meet minimums.
///
/// Mirrors stellar-core's `checkFeeMap()` (TxSetFrame.cpp:722-751).
///
/// For V0 (sequential) phases: iterates components, checks each component's
/// `base_fee` (if present) >= `lcl_base_fee`, and verifies each TX's inclusion
/// fee >= the minimum inclusion fee for that component.
///
/// For V1 (parallel) phases: checks the phase-level `base_fee` >= `lcl_base_fee`,
/// then verifies each TX's inclusion fee.
pub(crate) fn check_fee_map(phase: &TransactionPhase, lcl_base_fee: u32) -> bool {
    match phase {
        TransactionPhase::V0(components) => {
            for component in components.iter() {
                let TxSetComponent::TxsetCompTxsMaybeDiscountedFee(comp) = component;
                if let Some(base_fee) = comp.base_fee {
                    if (base_fee as u32) < lcl_base_fee {
                        debug!(
                            "Got bad txSet: component base fee {} < lcl base fee {}",
                            base_fee, lcl_base_fee
                        );
                        return false;
                    }
                }
                // Check each TX's inclusion fee meets the minimum
                let component_base_fee = comp.base_fee;
                for tx in comp.txs.iter() {
                    let tx_inclusion_fee = envelope_inclusion_fee(tx);
                    let min_fee = get_min_inclusion_fee(tx, lcl_base_fee, component_base_fee);
                    if tx_inclusion_fee < min_fee {
                        debug!(
                            "Got bad txSet: tx fee bid ({}) lower than base fee ({})",
                            tx_inclusion_fee, min_fee
                        );
                        return false;
                    }
                }
            }
            true
        }
        TransactionPhase::V1(parallel) => {
            if let Some(base_fee) = parallel.base_fee {
                if (base_fee as u32) < lcl_base_fee {
                    debug!(
                        "Got bad txSet: parallel base fee {} < lcl base fee {}",
                        base_fee, lcl_base_fee
                    );
                    return false;
                }
            }
            let component_base_fee = parallel.base_fee;
            for stage in parallel.execution_stages.iter() {
                for cluster in stage.iter() {
                    for tx in cluster.iter() {
                        let tx_inclusion_fee = envelope_inclusion_fee(tx);
                        let min_fee = get_min_inclusion_fee(tx, lcl_base_fee, component_base_fee);
                        if tx_inclusion_fee < min_fee {
                            debug!(
                                "Got bad txSet: tx fee bid ({}) lower than base fee ({})",
                                tx_inclusion_fee, min_fee
                            );
                            return false;
                        }
                    }
                }
            }
            true
        }
    }
}

/// Compute the minimum inclusion fee for a transaction.
///
/// Mirrors stellar-core's `getMinInclusionFee()` (TransactionUtils.cpp:1961-1971).
/// effectiveBaseFee = max(header.baseFee, componentBaseFee)
/// minFee = effectiveBaseFee * max(1, numOps)
fn get_min_inclusion_fee(
    env: &TransactionEnvelope,
    lcl_base_fee: u32,
    component_base_fee: Option<i64>,
) -> i64 {
    let effective_base_fee = match component_base_fee {
        Some(bf) => std::cmp::max(lcl_base_fee as i64, bf),
        None => lcl_base_fee as i64,
    };
    let num_ops = std::cmp::max(1, envelope_num_ops(env) as i64);
    effective_base_fee.saturating_mul(num_ops)
}

/// Validate the classic (non-Soroban) transaction phase.
///
/// Mirrors stellar-core's `TxSetPhaseFrame::checkValidClassic()` (TxSetFrame.cpp:1802-1816).
///
/// - Rejects if the phase is V1 (parallel) — classic can only be V0/sequential
/// - Counts total operations and verifies <= `max_tx_set_size`
/// - Verifies all TXs are non-Soroban
pub(crate) fn check_valid_classic(phase: &TransactionPhase, max_tx_set_size: u32) -> bool {
    // Classic phase must not be parallel
    if matches!(phase, TransactionPhase::V1(_)) {
        debug!("Got bad txSet: classic phase can't be parallel");
        return false;
    }

    let TransactionPhase::V0(components) = phase else {
        return false;
    };

    let mut total_ops: u64 = 0;
    for component in components.iter() {
        let TxSetComponent::TxsetCompTxsMaybeDiscountedFee(comp) = component;
        for tx in comp.txs.iter() {
            // Verify all TXs are non-Soroban
            if is_soroban_envelope(tx) {
                debug!("Got bad txSet: Soroban transaction found in classic phase");
                return false;
            }
            total_ops += envelope_num_ops(tx) as u64;
        }
    }

    if total_ops > max_tx_set_size as u64 {
        debug!(
            "Got bad txSet: too many classic ops {} > {}",
            total_ops, max_tx_set_size
        );
        return false;
    }

    true
}

/// Validate the Soroban transaction phase.
///
/// Mirrors stellar-core's `TxSetPhaseFrame::checkValidSoroban()` (TxSetFrame.cpp:1819-1982).
///
/// Checks:
/// 1. Parallel/sequential match against protocol version
/// 2. Total resource aggregation <= ledger limits
/// 3. All TXs are Soroban
/// 4. If parallel: cluster count per stage <= `ledger_max_dependent_tx_clusters`
/// 5. Sequential instruction limit: sum(max(cluster_instructions)) per stage <= ledger max
/// 6. RW conflict detection between clusters within each stage
pub(crate) fn check_valid_soroban(
    phase: &TransactionPhase,
    lcl_header: &LedgerHeader,
    soroban_info: &SorobanNetworkInfo,
) -> bool {
    let protocol = lcl_header.ledger_version;
    let need_parallel = protocol_version_starts_from(protocol, ProtocolVersion::V23);

    let is_parallel = matches!(phase, TransactionPhase::V1(_));
    if is_parallel != need_parallel {
        debug!(
            "Got bad txSet: Soroban phase parallel support mismatch; expected {}",
            need_parallel
        );
        return false;
    }

    // Aggregate total resources across all TXs
    let mut total_instructions: i64 = 0;
    let mut total_read_entries: i64 = 0;
    let mut total_read_bytes: i64 = 0;
    let mut total_write_entries: i64 = 0;
    let mut total_write_bytes: i64 = 0;

    let all_txs = collect_phase_txs(phase);

    for tx in &all_txs {
        if !is_soroban_envelope(tx) {
            debug!("Got bad txSet: non-Soroban transaction found in Soroban phase");
            return false;
        }
        if let Some(resources) = envelope_soroban_resources(tx) {
            total_instructions = total_instructions.saturating_add(resources.instructions as i64);
            // Parity: For protocol >= 23, use disk read entries (only classic + archived)
            // matching stellar-core's getNumDiskReadEntries().
            let ext = envelope_soroban_data_ext(tx);
            let is_restore = is_restore_footprint_envelope(tx);
            let disk_read = soroban_disk_read_entries(resources, ext, is_restore, protocol);
            total_read_entries = total_read_entries.saturating_add(disk_read);
            total_read_bytes = total_read_bytes.saturating_add(resources.disk_read_bytes as i64);
            total_write_entries =
                total_write_entries.saturating_add(resources.footprint.read_write.len() as i64);
            total_write_bytes = total_write_bytes.saturating_add(resources.write_bytes as i64);
        }
    }

    // Check resource limits (skip instructions for parallel — handled below)
    if !is_parallel && total_instructions > soroban_info.ledger_max_instructions {
        debug!(
            "Got bad txSet: Soroban instructions {} > ledger max {}",
            total_instructions, soroban_info.ledger_max_instructions
        );
        return false;
    }
    if total_read_entries > soroban_info.ledger_max_read_ledger_entries as i64 {
        debug!(
            "Got bad txSet: Soroban read entries {} > ledger max {}",
            total_read_entries, soroban_info.ledger_max_read_ledger_entries
        );
        return false;
    }
    if total_read_bytes > soroban_info.ledger_max_read_bytes as i64 {
        debug!(
            "Got bad txSet: Soroban read bytes {} > ledger max {}",
            total_read_bytes, soroban_info.ledger_max_read_bytes
        );
        return false;
    }
    if total_write_entries > soroban_info.ledger_max_write_ledger_entries as i64 {
        debug!(
            "Got bad txSet: Soroban write entries {} > ledger max {}",
            total_write_entries, soroban_info.ledger_max_write_ledger_entries
        );
        return false;
    }
    if total_write_bytes > soroban_info.ledger_max_write_bytes as i64 {
        debug!(
            "Got bad txSet: Soroban write bytes {} > ledger max {}",
            total_write_bytes, soroban_info.ledger_max_write_bytes
        );
        return false;
    }

    // Sequential phase is done
    if !is_parallel {
        return true;
    }

    // Parallel-specific validation
    let TransactionPhase::V1(parallel) = phase else {
        return false;
    };

    // Check cluster count per stage
    for stage in parallel.execution_stages.iter() {
        if stage.len() as u32 > soroban_info.ledger_max_dependent_tx_clusters {
            debug!(
                "Got bad txSet: too many clusters in Soroban stage {} > {}",
                stage.len(),
                soroban_info.ledger_max_dependent_tx_clusters
            );
            return false;
        }
    }

    // Sequential instruction limit: sum of max(cluster_instructions) per stage
    let mut sequential_instructions: i64 = 0;
    for stage in parallel.execution_stages.iter() {
        let mut stage_max_instructions: i64 = 0;
        for cluster in stage.iter() {
            let mut cluster_instructions: i64 = 0;
            for tx in cluster.iter() {
                if let Some(resources) = envelope_soroban_resources(tx) {
                    // Check overflow
                    if cluster_instructions > i64::MAX - resources.instructions as i64 {
                        debug!("Got bad txSet: Soroban sequential instructions overflow");
                        return false;
                    }
                    cluster_instructions += resources.instructions as i64;
                }
            }
            stage_max_instructions = std::cmp::max(stage_max_instructions, cluster_instructions);
        }
        if sequential_instructions > i64::MAX - stage_max_instructions {
            debug!("Got bad txSet: Soroban total instructions overflow");
            return false;
        }
        sequential_instructions += stage_max_instructions;
    }
    if sequential_instructions > soroban_info.ledger_max_instructions {
        debug!(
            "Got bad txSet: Soroban total instructions exceed limit: {} > {}",
            sequential_instructions, soroban_info.ledger_max_instructions
        );
        return false;
    }

    // RW conflict detection between clusters within each stage
    for stage in parallel.execution_stages.iter() {
        let mut stage_read_only_keys: HashSet<Vec<u8>> = HashSet::new();
        let mut stage_read_write_keys: HashSet<Vec<u8>> = HashSet::new();

        for cluster in stage.iter() {
            let mut cluster_read_only_keys: Vec<Vec<u8>> = Vec::new();
            let mut cluster_read_write_keys: Vec<Vec<u8>> = Vec::new();

            for tx in cluster.iter() {
                if let Some(resources) = envelope_soroban_resources(tx) {
                    for key in resources.footprint.read_only.iter() {
                        let key_bytes = key_to_bytes(key);
                        if stage_read_write_keys.contains(&key_bytes) {
                            debug!(
                                "Got bad txSet: cluster footprint conflicts with another cluster within stage"
                            );
                            return false;
                        }
                        cluster_read_only_keys.push(key_bytes);
                    }
                    for key in resources.footprint.read_write.iter() {
                        let key_bytes = key_to_bytes(key);
                        if stage_read_only_keys.contains(&key_bytes)
                            || stage_read_write_keys.contains(&key_bytes)
                        {
                            debug!(
                                "Got bad txSet: cluster footprint conflicts with another cluster within stage"
                            );
                            return false;
                        }
                        cluster_read_write_keys.push(key_bytes);
                    }
                }
            }

            stage_read_only_keys.extend(cluster_read_only_keys);
            stage_read_write_keys.extend(cluster_read_write_keys);
        }
    }

    true
}

/// Serialize a LedgerKey to bytes for use as a hash set key.
fn key_to_bytes(key: &stellar_xdr::curr::LedgerKey) -> Vec<u8> {
    use stellar_xdr::curr::{Limits, WriteXdr};
    key.to_xdr(Limits::none()).unwrap_or_default()
}

/// Collect all transaction envelopes from a phase.
fn collect_phase_txs(phase: &TransactionPhase) -> Vec<&TransactionEnvelope> {
    let mut txs = Vec::new();
    match phase {
        TransactionPhase::V0(components) => {
            for component in components.iter() {
                let TxSetComponent::TxsetCompTxsMaybeDiscountedFee(comp) = component;
                for tx in comp.txs.iter() {
                    txs.push(tx);
                }
            }
        }
        TransactionPhase::V1(parallel) => {
            for stage in parallel.execution_stages.iter() {
                for cluster in stage.iter() {
                    for tx in cluster.iter() {
                        txs.push(tx);
                    }
                }
            }
        }
    }
    txs
}

/// Orchestrate full TX set content validation.
///
/// Mirrors stellar-core's `ApplicableTxSetFrame::checkValidInternalWithResult()`
/// (TxSetFrame.cpp:2107-2187) and per-phase `TxSetPhaseFrame::checkValidWithResult()`
/// (TxSetFrame.cpp:1742-1799).
///
/// Performs:
/// 1. Verify generalized vs legacy matches protocol version
/// 2. For generalized sets: verify no duplicate source accounts across ALL phases
/// 3. Per-phase: fee map validation, phase-type checks, phase-specific limits
/// 4. Per-TX content validation (time bounds, fees) via `get_invalid_tx_list_with_fee_map`
///
/// For Phase 1, `fee_balance_provider` may be `None` to skip per-account balance checks.
pub(crate) fn check_tx_set_valid(
    gen_tx_set: &GeneralizedTransactionSet,
    lcl_header: &LedgerHeader,
    close_time_offset: u64,
    network_id: NetworkId,
    soroban_info: Option<&SorobanNetworkInfo>,
    fee_balance_provider: Option<&dyn FeeBalanceProvider>,
) -> bool {
    let GeneralizedTransactionSet::V1(v1) = gen_tx_set;

    // Verify generalized tx set is expected for this protocol
    let need_generalized =
        protocol_version_starts_from(lcl_header.ledger_version, ProtocolVersion::V20);
    if !need_generalized {
        debug!("Got bad txSet: generalized tx set not expected for protocol < 20");
        return false;
    }

    // Generalized sets should always have 2 phases
    if v1.phases.len() != 2 {
        debug!("Got bad txSet: expected 2 phases, got {}", v1.phases.len());
        return false;
    }

    // Cross-phase fee map handling (Protocol 26+)
    let use_cross_phase_fee_map =
        protocol_version_starts_from(lcl_header.ledger_version, ProtocolVersion::V26);

    // Build validation context for per-TX checks
    let ledger_flags = match &lcl_header.ext {
        stellar_xdr::curr::LedgerHeaderExt::V1(ext) => ext.flags,
        _ => 0,
    };
    let ctx = TxSetValidationContext::new(
        lcl_header.ledger_seq,
        lcl_header.scp_value.close_time.0,
        lcl_header.base_fee,
        lcl_header.base_reserve,
        lcl_header.ledger_version,
        network_id,
        ledger_flags,
    );
    let close_time_bounds = CloseTimeBounds::with_offsets(close_time_offset, close_time_offset);

    let mut account_fee_map: HashMap<AccountId, i64> = HashMap::new();

    for (phase_idx, phase) in v1.phases.iter().enumerate() {
        if !use_cross_phase_fee_map {
            account_fee_map.clear();
        }

        // 1. Check fee map
        if !check_fee_map(phase, lcl_header.base_fee) {
            return false;
        }

        let is_soroban = phase_idx == 1;

        // 2. Verify phase TX types
        let phase_txs = collect_phase_txs(phase);
        for tx in &phase_txs {
            if is_soroban_envelope(tx) != is_soroban {
                debug!(
                    "Got bad txSet: invalid phase {} transaction type",
                    phase_idx
                );
                return false;
            }
        }

        // 3. Phase-specific validation
        if is_soroban {
            if let Some(info) = soroban_info {
                if !check_valid_soroban(phase, lcl_header, info) {
                    return false;
                }
            }
        } else if !check_valid_classic(phase, lcl_header.max_tx_set_size) {
            return false;
        }

        // 4. Per-TX content validation (time bounds, fees, etc.)
        let tx_envelopes: Vec<TransactionEnvelope> =
            phase_txs.iter().map(|tx| (*tx).clone()).collect();
        let invalid = get_invalid_tx_list_with_fee_map(
            &tx_envelopes,
            &ctx,
            &close_time_bounds,
            fee_balance_provider,
            &mut account_fee_map,
        );
        if !invalid.is_empty() {
            debug!(
                "Got bad txSet: {} invalid transactions in phase {}",
                invalid.len(),
                phase_idx
            );
            return false;
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tx_queue::FeeBalanceProvider;
    use henyey_common::NetworkId;
    use stellar_xdr::curr::{
        AccountId, Asset, DecoratedSignature, LedgerBounds, Memo, MuxedAccount, Operation,
        OperationBody, PaymentOp, Preconditions, PreconditionsV2, PublicKey, SequenceNumber,
        Signature as XdrSignature, SignatureHint, TimeBounds, TimePoint, Transaction,
        TransactionEnvelope, TransactionExt, TransactionV1Envelope, Uint256, VecM,
    };

    /// Mock fee balance provider for testing.
    struct MockFeeBalanceProvider {
        balances: HashMap<AccountId, i64>,
    }

    impl MockFeeBalanceProvider {
        fn new() -> Self {
            Self {
                balances: HashMap::new(),
            }
        }

        fn set_balance(&mut self, key_bytes: [u8; 32], balance: i64) {
            let account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(key_bytes)));
            self.balances.insert(account_id, balance);
        }
    }

    impl FeeBalanceProvider for MockFeeBalanceProvider {
        fn get_available_balance(&self, account_id: &AccountId) -> Option<i64> {
            self.balances.get(account_id).copied()
        }
    }

    fn make_valid_envelope(fee: u32, seq: i64) -> TransactionEnvelope {
        let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
        let dest = MuxedAccount::Ed25519(Uint256([1u8; 32]));

        let op = Operation {
            source_account: None,
            body: OperationBody::Payment(PaymentOp {
                destination: dest,
                asset: Asset::Native,
                amount: 1000,
            }),
        };

        let tx = Transaction {
            source_account: source,
            fee,
            seq_num: SequenceNumber(seq),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![op].try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        })
    }

    fn make_low_fee_envelope(seq: i64) -> TransactionEnvelope {
        let source = MuxedAccount::Ed25519(Uint256([2u8; 32]));
        let dest = MuxedAccount::Ed25519(Uint256([3u8; 32]));

        let op = Operation {
            source_account: None,
            body: OperationBody::Payment(PaymentOp {
                destination: dest,
                asset: Asset::Native,
                amount: 500,
            }),
        };

        let tx = Transaction {
            source_account: source,
            fee: 10, // Too low (min is 100 per op)
            seq_num: SequenceNumber(seq),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![op].try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        })
    }

    fn make_expired_time_envelope(seq: i64) -> TransactionEnvelope {
        let source = MuxedAccount::Ed25519(Uint256([4u8; 32]));
        let dest = MuxedAccount::Ed25519(Uint256([5u8; 32]));

        let op = Operation {
            source_account: None,
            body: OperationBody::Payment(PaymentOp {
                destination: dest,
                asset: Asset::Native,
                amount: 500,
            }),
        };

        // max_time = 500, but close_time will be 1000 -> too late
        let time_bounds = TimeBounds {
            min_time: TimePoint(100),
            max_time: TimePoint(500),
        };

        let tx = Transaction {
            source_account: source,
            fee: 100,
            seq_num: SequenceNumber(seq),
            cond: Preconditions::Time(time_bounds),
            memo: Memo::None,
            operations: vec![op].try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        })
    }

    fn make_bad_ledger_bounds_envelope(seq: i64) -> TransactionEnvelope {
        let source = MuxedAccount::Ed25519(Uint256([6u8; 32]));
        let dest = MuxedAccount::Ed25519(Uint256([7u8; 32]));

        let op = Operation {
            source_account: None,
            body: OperationBody::Payment(PaymentOp {
                destination: dest,
                asset: Asset::Native,
                amount: 500,
            }),
        };

        // max_ledger = 50, but next ledger seq will be 101 -> too late
        let ledger_bounds = LedgerBounds {
            min_ledger: 10,
            max_ledger: 50,
        };

        let preconditions = Preconditions::V2(PreconditionsV2 {
            time_bounds: None,
            ledger_bounds: Some(ledger_bounds),
            min_seq_num: None,
            min_seq_age: stellar_xdr::curr::Duration(0),
            min_seq_ledger_gap: 0,
            extra_signers: VecM::default(),
        });

        let tx = Transaction {
            source_account: source,
            fee: 100,
            seq_num: SequenceNumber(seq),
            cond: preconditions,
            memo: Memo::None,
            operations: vec![op].try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        })
    }

    fn test_context() -> TxSetValidationContext {
        TxSetValidationContext::new(
            100,       // LCL = 100, so next ledger = 101
            1000,      // close time
            100,       // base fee
            5_000_000, // base reserve
            21,        // protocol version
            NetworkId::testnet(),
            0, // ledger flags
        )
    }

    // --- get_invalid_tx_list tests ---

    #[test]
    fn test_get_invalid_tx_list_all_valid() {
        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();

        let txs = vec![
            make_valid_envelope(100, 1),
            make_valid_envelope(200, 2),
            make_valid_envelope(300, 3),
        ];

        let invalid = get_invalid_tx_list(&txs, &ctx, &bounds, None);
        assert!(
            invalid.is_empty(),
            "all valid transactions should produce no invalid list"
        );
    }

    #[test]
    fn test_get_invalid_tx_list_all_invalid() {
        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();

        let txs = vec![make_low_fee_envelope(1), make_low_fee_envelope(2)];

        let invalid = get_invalid_tx_list(&txs, &ctx, &bounds, None);
        assert_eq!(
            invalid.len(),
            2,
            "all invalid transactions should be returned"
        );
    }

    #[test]
    fn test_get_invalid_tx_list_mixed() {
        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();

        let valid = make_valid_envelope(100, 1);
        let invalid_fee = make_low_fee_envelope(2);
        let expired = make_expired_time_envelope(3);

        let txs = vec![valid, invalid_fee, expired];

        let invalid = get_invalid_tx_list(&txs, &ctx, &bounds, None);
        assert_eq!(
            invalid.len(),
            2,
            "should find 2 invalid transactions (low fee + expired time)"
        );
    }

    #[test]
    fn test_get_invalid_tx_list_empty_input() {
        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();

        let invalid = get_invalid_tx_list(&[], &ctx, &bounds, None);
        assert!(
            invalid.is_empty(),
            "empty input should produce empty invalid list"
        );
    }

    #[test]
    fn test_get_invalid_tx_list_bad_ledger_bounds() {
        let ctx = test_context(); // next ledger = 101
        let bounds = CloseTimeBounds::exact();

        // This tx has max_ledger = 50, but next ledger is 101
        let bad_bounds = make_bad_ledger_bounds_envelope(1);
        let txs = vec![bad_bounds];

        let invalid = get_invalid_tx_list(&txs, &ctx, &bounds, None);
        assert_eq!(
            invalid.len(),
            1,
            "transaction with expired ledger bounds should be invalid"
        );
    }

    // --- trim_invalid tests ---

    #[test]
    fn test_trim_invalid_all_valid() {
        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();

        let txs = vec![
            make_valid_envelope(100, 1),
            make_valid_envelope(200, 2),
            make_valid_envelope(300, 3),
        ];

        let (valid, invalid) = trim_invalid(&txs, &ctx, &bounds, None);
        assert_eq!(valid.len(), 3, "all transactions should be valid");
        assert!(invalid.is_empty(), "no transactions should be invalid");
    }

    #[test]
    fn test_trim_invalid_all_invalid() {
        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();

        let txs = vec![make_low_fee_envelope(1), make_low_fee_envelope(2)];

        let (valid, invalid) = trim_invalid(&txs, &ctx, &bounds, None);
        assert!(valid.is_empty(), "no transactions should be valid");
        assert_eq!(invalid.len(), 2, "all transactions should be invalid");
    }

    #[test]
    fn test_trim_invalid_mixed_set() {
        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();

        let valid1 = make_valid_envelope(100, 1);
        let valid2 = make_valid_envelope(200, 2);
        let invalid1 = make_low_fee_envelope(3);
        let invalid2 = make_expired_time_envelope(4);

        let txs = vec![
            valid1.clone(),
            invalid1.clone(),
            valid2.clone(),
            invalid2.clone(),
        ];

        let (valid, invalid) = trim_invalid(&txs, &ctx, &bounds, None);
        assert_eq!(valid.len(), 2, "should have 2 valid transactions");
        assert_eq!(invalid.len(), 2, "should have 2 invalid transactions");

        // Verify the valid transactions are the ones we expect (by hash)
        let valid_hashes: HashSet<Hash256> = valid
            .iter()
            .filter_map(|tx| Hash256::hash_xdr(tx).ok())
            .collect();
        let expected_valid1 = Hash256::hash_xdr(&valid1).unwrap();
        let expected_valid2 = Hash256::hash_xdr(&valid2).unwrap();
        assert!(
            valid_hashes.contains(&expected_valid1),
            "first valid tx should be in valid set"
        );
        assert!(
            valid_hashes.contains(&expected_valid2),
            "second valid tx should be in valid set"
        );
    }

    #[test]
    fn test_trim_invalid_empty_input() {
        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();

        let (valid, invalid) = trim_invalid(&[], &ctx, &bounds, None);
        assert!(
            valid.is_empty(),
            "empty input should produce empty valid set"
        );
        assert!(
            invalid.is_empty(),
            "empty input should produce empty invalid set"
        );
    }

    #[test]
    fn test_trim_invalid_preserves_order() {
        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();

        // Three valid transactions with different fees
        let tx1 = make_valid_envelope(100, 1);
        let tx2 = make_valid_envelope(200, 2);
        let tx3 = make_valid_envelope(300, 3);

        let txs = vec![tx1.clone(), tx2.clone(), tx3.clone()];

        let (valid, _) = trim_invalid(&txs, &ctx, &bounds, None);
        assert_eq!(valid.len(), 3);

        // Verify order is preserved
        let hash1 = Hash256::hash_xdr(&tx1).unwrap();
        let hash2 = Hash256::hash_xdr(&tx2).unwrap();
        let hash3 = Hash256::hash_xdr(&tx3).unwrap();

        assert_eq!(Hash256::hash_xdr(&valid[0]).unwrap(), hash1);
        assert_eq!(Hash256::hash_xdr(&valid[1]).unwrap(), hash2);
        assert_eq!(Hash256::hash_xdr(&valid[2]).unwrap(), hash3);
    }

    // --- CloseTimeBounds tests ---

    #[test]
    fn test_close_time_bounds_exact() {
        let bounds = CloseTimeBounds::exact();
        assert_eq!(bounds.lower_bound_offset, 0);
        assert_eq!(bounds.upper_bound_offset, 0);
    }

    #[test]
    fn test_close_time_bounds_with_offsets() {
        let bounds = CloseTimeBounds::with_offsets(5, 10);
        assert_eq!(bounds.lower_bound_offset, 5);
        assert_eq!(bounds.upper_bound_offset, 10);
    }

    #[test]
    fn test_close_time_bounds_default() {
        let bounds = CloseTimeBounds::default();
        assert_eq!(bounds.lower_bound_offset, 0);
        assert_eq!(bounds.upper_bound_offset, 0);
    }

    // --- TxSetValidationContext tests ---

    #[test]
    fn test_validation_context_next_ledger_seq() {
        let ctx =
            TxSetValidationContext::new(100, 1000, 100, 5_000_000, 21, NetworkId::testnet(), 0);
        assert_eq!(ctx.next_ledger_seq, 101, "next ledger should be LCL + 1");
    }

    #[test]
    fn test_validation_context_saturating_add() {
        // Edge case: LCL at u32::MAX should not overflow
        let ctx = TxSetValidationContext::new(
            u32::MAX,
            1000,
            100,
            5_000_000,
            21,
            NetworkId::testnet(),
            0,
        );
        assert_eq!(ctx.next_ledger_seq, u32::MAX, "should saturate at u32::MAX");
    }

    // --- remove_txs tests ---

    #[test]
    fn test_remove_txs_empty_removal_set() {
        let txs = vec![make_valid_envelope(100, 1)];
        let result = remove_txs(&txs, &[]);
        assert_eq!(result.len(), 1, "no txs should be removed");
    }

    #[test]
    fn test_remove_txs_removes_correct_txs() {
        let tx1 = make_valid_envelope(100, 1);
        let tx2 = make_valid_envelope(200, 2);
        let tx3 = make_valid_envelope(300, 3);

        let txs = vec![tx1.clone(), tx2.clone(), tx3.clone()];
        let to_remove = vec![tx2.clone()];

        let result = remove_txs(&txs, &to_remove);
        assert_eq!(result.len(), 2);

        let result_hashes: HashSet<Hash256> = result
            .iter()
            .filter_map(|tx| Hash256::hash_xdr(tx).ok())
            .collect();
        assert!(result_hashes.contains(&Hash256::hash_xdr(&tx1).unwrap()));
        assert!(!result_hashes.contains(&Hash256::hash_xdr(&tx2).unwrap()));
        assert!(result_hashes.contains(&Hash256::hash_xdr(&tx3).unwrap()));
    }

    // --- Integration-style test: time bounds with offsets ---

    #[test]
    fn test_get_invalid_tx_list_with_close_time_offsets() {
        // Create a tx that is valid at close_time=1000 but invalid at close_time=1010
        let source = MuxedAccount::Ed25519(Uint256([8u8; 32]));
        let dest = MuxedAccount::Ed25519(Uint256([9u8; 32]));

        let op = Operation {
            source_account: None,
            body: OperationBody::Payment(PaymentOp {
                destination: dest,
                asset: Asset::Native,
                amount: 500,
            }),
        };

        // max_time = 1005, valid at close_time=1000 but not at 1000+10=1010
        let time_bounds = TimeBounds {
            min_time: TimePoint(0),
            max_time: TimePoint(1005),
        };

        let tx = Transaction {
            source_account: source,
            fee: 100,
            seq_num: SequenceNumber(1),
            cond: Preconditions::Time(time_bounds),
            memo: Memo::None,
            operations: vec![op].try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        });

        let ctx = test_context(); // close_time = 1000, next_ledger = 101

        // With no offset, should be valid
        let bounds_exact = CloseTimeBounds::exact();
        let invalid =
            get_invalid_tx_list(std::slice::from_ref(&envelope), &ctx, &bounds_exact, None);
        assert!(
            invalid.is_empty(),
            "tx should be valid with exact close time"
        );

        // With upper offset of 10 (close_time + 10 = 1010 > max_time 1005), should be invalid
        let bounds_offset = CloseTimeBounds::with_offsets(0, 10);
        let invalid =
            get_invalid_tx_list(std::slice::from_ref(&envelope), &ctx, &bounds_offset, None);
        assert_eq!(
            invalid.len(),
            1,
            "tx should be invalid with upper close time offset"
        );
    }

    // --- Fee-source affordability tests ---

    /// Helper to make a valid envelope with a specific source key and fee.
    fn make_envelope_with_source(source_key: [u8; 32], fee: u32, seq: i64) -> TransactionEnvelope {
        let source = MuxedAccount::Ed25519(Uint256(source_key));
        let dest = MuxedAccount::Ed25519(Uint256([0xFF; 32]));

        let op = Operation {
            source_account: None,
            body: OperationBody::Payment(PaymentOp {
                destination: dest,
                asset: Asset::Native,
                amount: 1000,
            }),
        };

        let tx = Transaction {
            source_account: source,
            fee,
            seq_num: SequenceNumber(seq),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![op].try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        })
    }

    #[test]
    fn test_fee_source_affordability_sufficient_balance() {
        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();

        // Source [10u8; 32] has fee=200, and balance is 500 -> sufficient
        let tx = make_envelope_with_source([10u8; 32], 200, 1);

        let mut provider = MockFeeBalanceProvider::new();
        provider.set_balance([10u8; 32], 500);

        let invalid = get_invalid_tx_list(&[tx], &ctx, &bounds, Some(&provider));
        assert!(
            invalid.is_empty(),
            "tx should be valid when balance covers fee"
        );
    }

    #[test]
    fn test_fee_source_affordability_insufficient_balance() {
        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();

        // Source [10u8; 32] has fee=200, and balance is 100 -> insufficient
        let tx = make_envelope_with_source([10u8; 32], 200, 1);

        let mut provider = MockFeeBalanceProvider::new();
        provider.set_balance([10u8; 32], 100);

        let invalid = get_invalid_tx_list(&[tx], &ctx, &bounds, Some(&provider));
        assert_eq!(
            invalid.len(),
            1,
            "tx should be invalid when balance can't cover fee"
        );
    }

    #[test]
    fn test_fee_source_affordability_multiple_txs_same_source() {
        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();

        // Two txs from same source [10u8; 32], each fee=200, total=400
        // Balance is 300 -> insufficient for total
        let tx1 = make_envelope_with_source([10u8; 32], 200, 1);
        let tx2 = make_envelope_with_source([10u8; 32], 200, 2);

        let mut provider = MockFeeBalanceProvider::new();
        provider.set_balance([10u8; 32], 300);

        let invalid = get_invalid_tx_list(&[tx1, tx2], &ctx, &bounds, Some(&provider));
        assert_eq!(
            invalid.len(),
            2,
            "both txs should be invalid when cumulative fees exceed balance"
        );
    }

    #[test]
    fn test_fee_source_affordability_cumulative_exactly_at_balance() {
        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();

        // Two txs from same source, total fee = balance exactly
        let tx1 = make_envelope_with_source([10u8; 32], 200, 1);
        let tx2 = make_envelope_with_source([10u8; 32], 200, 2);

        let mut provider = MockFeeBalanceProvider::new();
        provider.set_balance([10u8; 32], 400); // exactly covers total

        let invalid = get_invalid_tx_list(&[tx1, tx2], &ctx, &bounds, Some(&provider));
        assert!(
            invalid.is_empty(),
            "txs should be valid when balance exactly covers cumulative fees"
        );
    }

    #[test]
    fn test_fee_source_affordability_multiple_sources_mixed() {
        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();

        // Source A ([10u8; 32]) has 2 txs, fee=200 each, total=400, balance=500 -> OK
        // Source B ([20u8; 32]) has 1 tx, fee=300, balance=100 -> insufficient
        let tx_a1 = make_envelope_with_source([10u8; 32], 200, 1);
        let tx_a2 = make_envelope_with_source([10u8; 32], 200, 2);
        let tx_b = make_envelope_with_source([20u8; 32], 300, 1);

        let mut provider = MockFeeBalanceProvider::new();
        provider.set_balance([10u8; 32], 500);
        provider.set_balance([20u8; 32], 100);

        let invalid = get_invalid_tx_list(&[tx_a1, tx_a2, tx_b], &ctx, &bounds, Some(&provider));
        assert_eq!(invalid.len(), 1, "only source B's tx should be invalid");
    }

    #[test]
    fn test_fee_source_affordability_unknown_account() {
        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();

        // Source [10u8; 32] not in provider (returns None -> treated as 0 balance)
        let tx = make_envelope_with_source([10u8; 32], 200, 1);

        let provider = MockFeeBalanceProvider::new(); // empty

        let invalid = get_invalid_tx_list(&[tx], &ctx, &bounds, Some(&provider));
        assert_eq!(
            invalid.len(),
            1,
            "tx from unknown account should be invalid (balance defaults to 0)"
        );
    }

    #[test]
    fn test_fee_source_affordability_skips_already_invalid_txs() {
        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();

        // tx1 is invalid (low fee), tx2 is valid
        // Both from same source [2u8; 32] (make_low_fee_envelope uses [2u8; 32])
        let tx1 = make_low_fee_envelope(1);
        // Valid tx from same source
        let tx2 = make_envelope_with_source([2u8; 32], 200, 2);

        let mut provider = MockFeeBalanceProvider::new();
        // Balance of 200 would cover tx2 alone, but not tx1+tx2 if tx1 wasn't filtered out
        provider.set_balance([2u8; 32], 200);

        let invalid = get_invalid_tx_list(&[tx1, tx2], &ctx, &bounds, Some(&provider));
        // tx1 is invalid due to low fee; tx2 alone has fee=200, balance=200, so it passes
        assert_eq!(
            invalid.len(),
            1,
            "only the low-fee tx should be invalid; the valid tx's fee is affordable alone"
        );
    }

    #[test]
    fn test_fee_source_affordability_with_none_provider_skips_check() {
        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();

        // Even with 0 balance, without a provider, fee check is skipped
        let tx = make_envelope_with_source([10u8; 32], 200, 1);

        let invalid = get_invalid_tx_list(&[tx], &ctx, &bounds, None);
        assert!(
            invalid.is_empty(),
            "without provider, fee affordability check should be skipped"
        );
    }

    #[test]
    fn test_trim_invalid_with_fee_provider() {
        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();

        let tx_affordable = make_envelope_with_source([10u8; 32], 200, 1);
        let tx_unaffordable = make_envelope_with_source([20u8; 32], 500, 1);

        let mut provider = MockFeeBalanceProvider::new();
        provider.set_balance([10u8; 32], 1000);
        provider.set_balance([20u8; 32], 100);

        let (valid, invalid) = trim_invalid(
            &[tx_affordable.clone(), tx_unaffordable.clone()],
            &ctx,
            &bounds,
            Some(&provider),
        );
        assert_eq!(valid.len(), 1, "one tx should be valid");
        assert_eq!(invalid.len(), 1, "one tx should be invalid");
    }

    // --- AUDIT-033: check_fee_map tests ---

    fn make_v0_phase_with_fee(
        txs: Vec<TransactionEnvelope>,
        base_fee: Option<i64>,
    ) -> TransactionPhase {
        use stellar_xdr::curr::{TxSetComponent, TxSetComponentTxsMaybeDiscountedFee};
        TransactionPhase::V0(
            vec![TxSetComponent::TxsetCompTxsMaybeDiscountedFee(
                TxSetComponentTxsMaybeDiscountedFee {
                    base_fee,
                    txs: txs.try_into().unwrap(),
                },
            )]
            .try_into()
            .unwrap(),
        )
    }

    #[test]
    fn test_check_fee_map_valid_fees() {
        // TX with fee=200, 1 op. lcl_base_fee=100. component base_fee=100.
        // min_inclusion_fee = max(100, 100) * 1 = 100
        // inclusion_fee = min(200, 1*100) = 100 >= 100 -> valid
        let tx = make_valid_envelope(200, 1);
        let phase = make_v0_phase_with_fee(vec![tx], Some(100));
        assert!(check_fee_map(&phase, 100));
    }

    #[test]
    fn test_check_fee_map_component_base_fee_too_low() {
        let tx = make_valid_envelope(200, 1);
        // base_fee=50 < lcl_base_fee=100
        let phase = make_v0_phase_with_fee(vec![tx], Some(50));
        assert!(!check_fee_map(&phase, 100));
    }

    #[test]
    fn test_check_fee_map_tx_fee_bid_too_low() {
        // TX with fee=50, 1 op. base_fee=100.
        // inclusion_fee = min(50, 1*100) = 50
        // min_inclusion_fee = max(100, 100) * 1 = 100
        // 50 < 100 -> invalid
        let tx = make_valid_envelope(50, 1);
        let phase = make_v0_phase_with_fee(vec![tx], Some(100));
        assert!(!check_fee_map(&phase, 100));
    }

    #[test]
    fn test_check_fee_map_no_base_fee_valid() {
        // No component base_fee. TX fee=200, 1 op.
        // inclusion_fee = full_fee = 200
        // min_inclusion_fee = max(100, _) * 1 = 100
        // 200 >= 100 -> valid
        let tx = make_valid_envelope(200, 1);
        let phase = make_v0_phase_with_fee(vec![tx], None);
        assert!(check_fee_map(&phase, 100));
    }

    // --- AUDIT-033: check_valid_classic tests ---

    #[test]
    fn test_check_valid_classic_within_limit() {
        // 2 TXs with 1 op each, limit = 5
        let tx1 = make_valid_envelope(100, 1);
        let tx2 = make_valid_envelope(200, 2);
        let phase = make_v0_phase_with_fee(vec![tx1, tx2], Some(100));
        assert!(check_valid_classic(&phase, 5));
    }

    #[test]
    fn test_check_valid_classic_over_limit() {
        // 3 TXs with 1 op each, limit = 2
        let tx1 = make_valid_envelope(100, 1);
        let tx2 = make_valid_envelope(200, 2);
        let tx3 = make_valid_envelope(300, 3);
        let phase = make_v0_phase_with_fee(vec![tx1, tx2, tx3], Some(100));
        assert!(!check_valid_classic(&phase, 2));
    }

    #[test]
    fn test_check_valid_classic_rejects_parallel_phase() {
        use stellar_xdr::curr::ParallelTxsComponent;
        let phase = TransactionPhase::V1(ParallelTxsComponent {
            base_fee: Some(100),
            execution_stages: vec![].try_into().unwrap(),
        });
        assert!(!check_valid_classic(&phase, 100));
    }

    // --- AUDIT-033: check_valid_soroban tests ---

    fn make_soroban_envelope(
        instructions: u32,
        read_bytes: u32,
        write_bytes: u32,
        read_only_keys: Vec<stellar_xdr::curr::LedgerKey>,
        read_write_keys: Vec<stellar_xdr::curr::LedgerKey>,
    ) -> TransactionEnvelope {
        use stellar_xdr::curr::{
            InvokeHostFunctionOp, LedgerFootprint, SorobanResources, SorobanTransactionData,
            SorobanTransactionDataExt,
        };

        let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
        let op = Operation {
            source_account: None,
            body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                host_function: stellar_xdr::curr::HostFunction::InvokeContract(
                    stellar_xdr::curr::InvokeContractArgs {
                        contract_address: stellar_xdr::curr::ScAddress::Contract(
                            stellar_xdr::curr::ContractId(stellar_xdr::curr::Hash([0u8; 32])),
                        ),
                        function_name: stellar_xdr::curr::ScSymbol("test".try_into().unwrap()),
                        args: vec![].try_into().unwrap(),
                    },
                ),
                auth: vec![].try_into().unwrap(),
            }),
        };

        let footprint = LedgerFootprint {
            read_only: read_only_keys.try_into().unwrap(),
            read_write: read_write_keys.try_into().unwrap(),
        };
        let resources = SorobanResources {
            footprint,
            instructions,
            disk_read_bytes: read_bytes,
            write_bytes,
        };
        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources,
            resource_fee: 1000,
        };

        let tx = Transaction {
            source_account: source,
            fee: 10000,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![op].try_into().unwrap(),
            ext: stellar_xdr::curr::TransactionExt::V1(soroban_data),
        };

        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        })
    }

    fn make_soroban_network_info() -> henyey_ledger::SorobanNetworkInfo {
        henyey_ledger::SorobanNetworkInfo {
            ledger_max_instructions: 1_000_000,
            ledger_max_read_ledger_entries: 100,
            ledger_max_read_bytes: 100_000,
            ledger_max_write_ledger_entries: 50,
            ledger_max_write_bytes: 50_000,
            ledger_max_dependent_tx_clusters: 4,
            ..Default::default()
        }
    }

    fn make_soroban_lcl_header(protocol: u32) -> LedgerHeader {
        use stellar_xdr::curr::{Hash, LedgerHeaderExt, StellarValue, StellarValueExt, TimePoint};
        LedgerHeader {
            ledger_version: protocol,
            previous_ledger_hash: Hash([0u8; 32]),
            scp_value: StellarValue {
                tx_set_hash: Hash([0u8; 32]),
                close_time: TimePoint(1000),
                upgrades: vec![].try_into().unwrap(),
                ext: StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash([0u8; 32]),
            bucket_list_hash: Hash([0u8; 32]),
            ledger_seq: 100,
            total_coins: 0,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 5000000,
            max_tx_set_size: 100,
            skip_list: [Hash([0; 32]), Hash([0; 32]), Hash([0; 32]), Hash([0; 32])],
            ext: LedgerHeaderExt::V0,
        }
    }

    #[test]
    fn test_check_valid_soroban_sequential_valid() {
        let info = make_soroban_network_info();
        // Protocol 22 (pre-parallel) requires sequential V0 phase
        let header = make_soroban_lcl_header(22);
        let tx = make_soroban_envelope(100_000, 1000, 500, vec![], vec![]);
        let phase = make_v0_phase_with_fee(vec![tx], Some(100));
        assert!(check_valid_soroban(&phase, &header, &info));
    }

    #[test]
    fn test_check_valid_soroban_instructions_exceed_limit() {
        let info = make_soroban_network_info();
        let header = make_soroban_lcl_header(22);
        // Instructions exceed ledger max (1,000,000)
        let tx = make_soroban_envelope(2_000_000, 1000, 500, vec![], vec![]);
        let phase = make_v0_phase_with_fee(vec![tx], Some(100));
        assert!(!check_valid_soroban(&phase, &header, &info));
    }

    #[test]
    fn test_check_valid_soroban_parallel_mismatch_protocol_22() {
        use stellar_xdr::curr::ParallelTxsComponent;
        let info = make_soroban_network_info();
        let header = make_soroban_lcl_header(22);
        // Protocol 22 should NOT have parallel phase
        let phase = TransactionPhase::V1(ParallelTxsComponent {
            base_fee: Some(100),
            execution_stages: vec![].try_into().unwrap(),
        });
        assert!(!check_valid_soroban(&phase, &header, &info));
    }

    #[test]
    fn test_check_valid_soroban_sequential_mismatch_protocol_23() {
        let info = make_soroban_network_info();
        let header = make_soroban_lcl_header(23);
        // Protocol 23 requires parallel V1 phase, but we provide V0
        let tx = make_soroban_envelope(100_000, 1000, 500, vec![], vec![]);
        let phase = make_v0_phase_with_fee(vec![tx], Some(100));
        assert!(!check_valid_soroban(&phase, &header, &info));
    }

    #[test]
    fn test_check_valid_soroban_parallel_too_many_clusters() {
        use stellar_xdr::curr::{
            DependentTxCluster, ParallelTxExecutionStage, ParallelTxsComponent,
        };
        let mut info = make_soroban_network_info();
        info.ledger_max_dependent_tx_clusters = 2; // Only allow 2 clusters per stage
        let header = make_soroban_lcl_header(23);

        let tx1 = make_soroban_envelope(100, 100, 100, vec![], vec![]);
        let tx2 = make_soroban_envelope(100, 100, 100, vec![], vec![]);
        let tx3 = make_soroban_envelope(100, 100, 100, vec![], vec![]);

        // 3 clusters in one stage > limit of 2
        let stage: ParallelTxExecutionStage = vec![
            DependentTxCluster(vec![tx1].try_into().unwrap()),
            DependentTxCluster(vec![tx2].try_into().unwrap()),
            DependentTxCluster(vec![tx3].try_into().unwrap()),
        ]
        .try_into()
        .unwrap();

        let phase = TransactionPhase::V1(ParallelTxsComponent {
            base_fee: Some(100),
            execution_stages: vec![stage].try_into().unwrap(),
        });
        assert!(!check_valid_soroban(&phase, &header, &info));
    }

    #[test]
    fn test_check_valid_soroban_parallel_sequential_instruction_limit() {
        use stellar_xdr::curr::{
            DependentTxCluster, ParallelTxExecutionStage, ParallelTxsComponent,
        };
        let mut info = make_soroban_network_info();
        info.ledger_max_instructions = 1_000;
        info.ledger_max_dependent_tx_clusters = 10;
        let header = make_soroban_lcl_header(23);

        // Stage 1: cluster with 600 instructions
        // Stage 2: cluster with 500 instructions
        // Sequential total = 600 + 500 = 1100 > 1000 limit
        let tx1 = make_soroban_envelope(600, 100, 100, vec![], vec![]);
        let tx2 = make_soroban_envelope(500, 100, 100, vec![], vec![]);

        let stage1: ParallelTxExecutionStage =
            vec![DependentTxCluster(vec![tx1].try_into().unwrap())]
                .try_into()
                .unwrap();
        let stage2: ParallelTxExecutionStage =
            vec![DependentTxCluster(vec![tx2].try_into().unwrap())]
                .try_into()
                .unwrap();

        let phase = TransactionPhase::V1(ParallelTxsComponent {
            base_fee: Some(100),
            execution_stages: vec![stage1, stage2].try_into().unwrap(),
        });
        assert!(!check_valid_soroban(&phase, &header, &info));
    }

    #[test]
    fn test_check_valid_soroban_parallel_rw_conflict() {
        use stellar_xdr::curr::{
            DependentTxCluster, LedgerKey, LedgerKeyAccount, ParallelTxExecutionStage,
            ParallelTxsComponent,
        };
        let mut info = make_soroban_network_info();
        info.ledger_max_instructions = 10_000_000;
        info.ledger_max_dependent_tx_clusters = 10;
        let header = make_soroban_lcl_header(23);

        // Create a shared key
        let shared_key = LedgerKey::Account(LedgerKeyAccount {
            account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([99u8; 32]))),
        });

        // Two clusters in the same stage both writing to the same key
        let tx1 = make_soroban_envelope(100, 100, 100, vec![], vec![shared_key.clone()]);
        let tx2 = make_soroban_envelope(100, 100, 100, vec![], vec![shared_key.clone()]);

        let stage: ParallelTxExecutionStage = vec![
            DependentTxCluster(vec![tx1].try_into().unwrap()),
            DependentTxCluster(vec![tx2].try_into().unwrap()),
        ]
        .try_into()
        .unwrap();

        let phase = TransactionPhase::V1(ParallelTxsComponent {
            base_fee: Some(100),
            execution_stages: vec![stage].try_into().unwrap(),
        });
        assert!(!check_valid_soroban(&phase, &header, &info));
    }

    #[test]
    fn test_check_valid_soroban_parallel_no_conflict_different_keys() {
        use stellar_xdr::curr::{
            DependentTxCluster, LedgerKey, LedgerKeyAccount, ParallelTxExecutionStage,
            ParallelTxsComponent,
        };
        let mut info = make_soroban_network_info();
        info.ledger_max_instructions = 10_000_000;
        info.ledger_max_dependent_tx_clusters = 10;
        let header = make_soroban_lcl_header(23);

        let key1 = LedgerKey::Account(LedgerKeyAccount {
            account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32]))),
        });
        let key2 = LedgerKey::Account(LedgerKeyAccount {
            account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32]))),
        });

        // Two clusters writing to different keys — no conflict
        let tx1 = make_soroban_envelope(100, 100, 100, vec![], vec![key1]);
        let tx2 = make_soroban_envelope(100, 100, 100, vec![], vec![key2]);

        let stage: ParallelTxExecutionStage = vec![
            DependentTxCluster(vec![tx1].try_into().unwrap()),
            DependentTxCluster(vec![tx2].try_into().unwrap()),
        ]
        .try_into()
        .unwrap();

        let phase = TransactionPhase::V1(ParallelTxsComponent {
            base_fee: Some(100),
            execution_stages: vec![stage].try_into().unwrap(),
        });
        assert!(check_valid_soroban(&phase, &header, &info));
    }

    #[test]
    fn test_check_valid_soroban_rejects_classic_tx_in_soroban_phase() {
        let info = make_soroban_network_info();
        let header = make_soroban_lcl_header(22);
        // Classic TX in Soroban phase
        let tx = make_valid_envelope(100, 1);
        let phase = make_v0_phase_with_fee(vec![tx], Some(100));
        assert!(!check_valid_soroban(&phase, &header, &info));
    }

    #[test]
    fn test_check_valid_classic_rejects_soroban_tx() {
        let tx = make_soroban_envelope(100, 100, 100, vec![], vec![]);
        let phase = make_v0_phase_with_fee(vec![tx], Some(100));
        assert!(!check_valid_classic(&phase, 100));
    }

    /// Regression: get_invalid_tx_list must reject txs that fail check_valid_pre_seq_num
    /// even when they would pass validate_basic.
    #[test]
    fn test_get_invalid_tx_list_rejects_inflation_via_pre_seq_num() {
        let ctx = test_context(); // protocol 21
        let bounds = CloseTimeBounds::exact();

        // Build an Inflation tx — structurally valid for validate_basic but
        // rejected by check_valid_pre_seq_num (OpNotSupported on p12+).
        let source = MuxedAccount::Ed25519(Uint256([10u8; 32]));
        let tx = Transaction {
            source_account: source,
            fee: 100,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![Operation {
                source_account: None,
                body: OperationBody::Inflation,
            }]
            .try_into()
            .unwrap(),
            ext: TransactionExt::V0,
        };
        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        });

        let invalid = get_invalid_tx_list(&[envelope], &ctx, &bounds, None);
        assert_eq!(invalid.len(), 1, "Inflation tx should be rejected at p21");
    }

    /// Helper to build a multi-op V1 transaction envelope.
    fn make_multi_op_envelope(num_ops: usize, fee: u32) -> TransactionEnvelope {
        let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
        let dest = MuxedAccount::Ed25519(Uint256([1u8; 32]));
        let op = Operation {
            source_account: None,
            body: OperationBody::Payment(PaymentOp {
                destination: dest,
                asset: Asset::Native,
                amount: 1000,
            }),
        };
        let ops: Vec<Operation> = (0..num_ops).map(|_| op.clone()).collect();
        let tx = Transaction {
            source_account: source,
            fee,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: ops.try_into().unwrap(),
            ext: TransactionExt::V0,
        };
        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: VecM::default(),
        })
    }

    /// Helper to wrap a V1 envelope in a fee-bump.
    fn make_fee_bump_envelope(inner: TransactionEnvelope, bumped_fee: i64) -> TransactionEnvelope {
        let inner_v1 = match inner {
            TransactionEnvelope::Tx(e) => e,
            _ => panic!("expected V1 envelope"),
        };
        use stellar_xdr::curr::{
            FeeBumpTransaction, FeeBumpTransactionEnvelope, FeeBumpTransactionExt,
            FeeBumpTransactionInnerTx,
        };
        TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
            tx: FeeBumpTransaction {
                fee_source: MuxedAccount::Ed25519(Uint256([9u8; 32])),
                fee: bumped_fee,
                inner_tx: FeeBumpTransactionInnerTx::Tx(inner_v1),
                ext: FeeBumpTransactionExt::V0,
            },
            signatures: VecM::default(),
        })
    }

    /// Regression test for #1497: fee-bump op count must include +1 for wrapper.
    #[test]
    fn test_envelope_num_ops_v1() {
        let env = make_multi_op_envelope(3, 300);
        assert_eq!(envelope_num_ops(&env), 3);
    }

    #[test]
    fn test_envelope_num_ops_fee_bump_includes_wrapper() {
        let inner = make_multi_op_envelope(3, 300);
        let fee_bump = make_fee_bump_envelope(inner, 600);
        // stellar-core: FeeBumpTransactionFrame::getNumOperations() = inner ops + 1
        assert_eq!(envelope_num_ops(&fee_bump), 4);
    }

    #[test]
    fn test_envelope_num_ops_fee_bump_single_op() {
        let inner = make_multi_op_envelope(1, 100);
        let fee_bump = make_fee_bump_envelope(inner, 200);
        assert_eq!(envelope_num_ops(&fee_bump), 2);
    }

    #[test]
    fn test_envelope_fee_bump_returns_outer_fee() {
        let inner = make_multi_op_envelope(2, 200);
        let fee_bump = make_fee_bump_envelope(inner, 500);
        assert_eq!(envelope_fee(&fee_bump), 500);
    }

    #[test]
    fn test_envelope_inclusion_fee_fee_bump_classic() {
        // Classic fee-bump: no resource_fee, so inclusion_fee = full fee
        let inner = make_multi_op_envelope(2, 200);
        let fee_bump = make_fee_bump_envelope(inner, 500);
        assert_eq!(envelope_inclusion_fee(&fee_bump), 500);
    }
}
