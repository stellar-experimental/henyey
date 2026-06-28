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
use std::sync::Arc;

use henyey_common::protocol::{protocol_version_starts_from, ProtocolVersion};
use henyey_common::resource::{ResourceError, ResourceType};
use henyey_common::{Hash256, NetworkId};
use henyey_ledger::SorobanNetworkInfo;
use henyey_tx::{
    check_valid_pre_seq_num_with_config, collect_signers_for_account, get_threshold_level,
    muxed_to_account_id, validate_basic, LedgerContext, OperationTypeExt, SignatureChecker,
    TransactionFrame,
};
use stellar_xdr::{
    AccountEntry, AccountId, GeneralizedTransactionSet, LedgerHeader, Preconditions, SignerKey,
    TransactionEnvelope, TransactionPhase, TxSetComponent,
};
use tracing::debug;

use crate::tx_queue::{AccountProvider, FeeBalanceProvider, QueuedTransaction};

/// A transaction envelope paired with its pre-computed hash.
///
/// Used by the post-close invalidation hot path to avoid redundant
/// `Hash256::hash_xdr()` calls. The hash is pre-computed at queue
/// admission time in `QueuedTransaction::new()`.
///
/// Fields are private to enforce the invariant that `hash` always
/// matches `Hash256::hash_xdr(&envelope)`.
#[derive(Debug, Clone)]
pub struct HashedTx {
    hash: Hash256,
    envelope: Arc<TransactionEnvelope>,
}

impl HashedTx {
    /// Create a new `HashedTx` by computing the hash from the envelope.
    pub fn new(envelope: TransactionEnvelope) -> Self {
        let hash = Hash256::hash_xdr(&envelope);
        Self {
            hash,
            envelope: Arc::new(envelope),
        }
    }

    /// Create a `HashedTx` from a pre-computed hash and an `Arc`'d envelope.
    ///
    /// # Safety invariant
    /// The caller MUST guarantee that `hash == Hash256::hash_xdr(&envelope)`.
    /// This is enforced by `debug_assert_eq!` in debug builds. In release builds,
    /// prefer using `HashedTx::from(&QueuedTransaction)` which guarantees
    /// correctness by construction.
    #[cfg(test)]
    pub fn from_prehashed(hash: Hash256, envelope: Arc<TransactionEnvelope>) -> Self {
        debug_assert_eq!(
            hash,
            Hash256::hash_xdr(&*envelope),
            "HashedTx::from_prehashed: hash does not match envelope"
        );
        Self { hash, envelope }
    }

    pub fn hash(&self) -> Hash256 {
        self.hash
    }

    pub fn envelope(&self) -> &TransactionEnvelope {
        &self.envelope
    }

    pub fn arc_envelope(&self) -> &Arc<TransactionEnvelope> {
        &self.envelope
    }

    pub fn into_envelope(self) -> TransactionEnvelope {
        Arc::unwrap_or_clone(self.envelope)
    }
}

/// Convert a `QueuedTransaction` into a `HashedTx` without rehashing.
///
/// This is safe because `QueuedTransaction::new()` computes the hash from the
/// envelope at construction time, and the fields are never mutated afterward.
/// Using this conversion instead of `from_prehashed` makes the invariant
/// (hash matches envelope) enforced by construction rather than by runtime check.
impl From<&QueuedTransaction> for HashedTx {
    fn from(qt: &QueuedTransaction) -> Self {
        Self {
            hash: qt.hash(),
            envelope: qt.arc_envelope().clone(),
        }
    }
}

/// Unified account + fee-balance provider backed by a single ledger snapshot.
///
/// Creates one snapshot at construction time and reuses it for all lookups,
/// Get the declared fee from a transaction envelope.
///
/// For fee-bump transactions, returns the outer (bumped) fee.
pub(crate) fn envelope_fee(env: &TransactionEnvelope) -> henyey_tx::TotalFee {
    let raw = match env {
        TransactionEnvelope::TxV0(e) => e.tx.fee as i64,
        TransactionEnvelope::Tx(e) => e.tx.fee as i64,
        TransactionEnvelope::TxFeeBump(e) => e.tx.fee,
    };
    henyey_tx::TotalFee::new(raw)
}

/// Get the fee bid used for transaction ordering and surge pricing.
///
/// For Soroban transactions this is the inclusion fee (full fee minus resource fee),
/// matching stellar-core `TransactionFrameBase::getInclusionFee()`.
pub(crate) fn envelope_inclusion_fee(env: &TransactionEnvelope) -> henyey_tx::InclusionFee {
    let resource_fee = match env {
        TransactionEnvelope::TxV0(_) => 0,
        TransactionEnvelope::Tx(env) => match &env.tx.ext {
            stellar_xdr::TransactionExt::V0 => 0,
            stellar_xdr::TransactionExt::V1(data) => data.resource_fee,
        },
        TransactionEnvelope::TxFeeBump(env) => match &env.tx.inner_tx {
            stellar_xdr::FeeBumpTransactionInnerTx::Tx(inner) => match &inner.tx.ext {
                stellar_xdr::TransactionExt::V0 => 0,
                stellar_xdr::TransactionExt::V1(data) => data.resource_fee,
            },
        },
    };
    envelope_fee(env).saturating_sub_resource(henyey_tx::ResourceFee::new(resource_fee))
}

/// Get the number of operations from a transaction envelope.
///
/// Re-exported from the canonical source in henyey_tx::envelope_utils.
pub(crate) use henyey_tx::envelope_utils::envelope_operation_count as envelope_num_ops;

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
    /// Per-TX Soroban resource limits (from Soroban network config, if available).
    pub soroban_resource_limits: Option<henyey_tx::SorobanResourceLimits>,
    /// CAP-77: Frozen ledger key configuration for tx-set validation.
    /// Pre-V26 this is empty (no frozen keys). V26+ loaded from Soroban config.
    pub frozen_key_config: henyey_tx::frozen_keys::FrozenKeyConfig,
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
            soroban_resource_limits: None,
            frozen_key_config: henyey_tx::frozen_keys::FrozenKeyConfig::empty(),
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
    account_provider: Option<&dyn AccountProvider>,
) -> Vec<TransactionEnvelope> {
    let mut account_fee_map: HashMap<AccountId, i64> = HashMap::new();
    get_invalid_tx_list_with_fee_map(
        txs,
        ctx,
        close_time_bounds,
        fee_balance_provider,
        account_provider,
        &mut account_fee_map,
    )
}

/// Validate a transaction against ledger state for tx-set acceptance.
///
/// Mirrors stellar-core's `TransactionFrame::checkValid` → `commonValid` →
/// per-op `checkValid` → `checkAllSignaturesUsed` pipeline. Handles both
/// regular and fee-bump transactions.
///
/// Returns `true` if the transaction is valid, `false` if it should be rejected.
fn validate_tx_for_tx_set(
    frame: &TransactionFrame,
    ctx: &TxSetValidationContext,
    lower_close_time: u64,
    account_provider: &dyn AccountProvider,
) -> bool {
    if frame.is_fee_bump() {
        validate_fee_bump_for_tx_set(frame, ctx, lower_close_time, account_provider)
    } else {
        validate_regular_for_tx_set(frame, ctx, lower_close_time, account_provider)
    }
}

/// Validate a non-fee-bump transaction against ledger state.
///
/// Mirrors `TransactionFrame::checkValidWithOptionallyChargedFee`:
/// 1. commonValid (account load, seq, age/gap, tx-level auth)
/// 2. per-op checkValid (op source auth at correct threshold)
/// 3. extra signers check (before unused-sig detection)
/// 4. checkAllSignaturesUsed
fn validate_regular_for_tx_set(
    frame: &TransactionFrame,
    ctx: &TxSetValidationContext,
    lower_close_time: u64,
    account_provider: &dyn AccountProvider,
) -> bool {
    // Phase A: Load source account
    let source_id = frame.source_account_id();
    let source_account = match account_provider.load_account(&source_id) {
        Ok(Some(acc)) => acc,
        Ok(None) => {
            debug!(?source_id, "tx-set validation: source account not found");
            return false;
        }
        Err(e) => {
            tracing::warn!(error = ?e, ?source_id, "tx-set validation: account lookup failed");
            return false;
        }
    };

    // Compute tx hash (needed for frozen-key bypass and signatures)
    let tx_hash = match frame.hash(&ctx.network_id) {
        Ok(h) => h,
        Err(_) => return false,
    };

    // CAP-77 frozen key check (last step of commonValidPreSeqNum)
    // Parity: stellar-core commonValidPreSeqNum:1548-1560
    // Runs after source account load but before sequence/signature checks.
    // No protocol gate — relies on has_frozen_keys() being false pre-V26.
    if ctx.frozen_key_config.has_frozen_keys() {
        let soroban_fp = frame.soroban_data().map(|d| &d.resources.footprint);
        if henyey_tx::frozen_keys::accesses_frozen_key(
            &source_id,
            frame.operations(),
            soroban_fp,
            &ctx.frozen_key_config,
        ) && !ctx.frozen_key_config.is_freeze_bypass_tx(&tx_hash)
        {
            debug!("tx-set validation: tx accesses frozen ledger key");
            return false;
        }
    }

    // Sequence validation (mirrors isBadSeq)
    if !validate_sequence(frame, &source_account, ctx.next_ledger_seq) {
        return false;
    }

    // Min seq age/gap (mirrors isTooEarlyForAccount)
    if !validate_min_seq_age_gap(
        frame,
        &source_account,
        lower_close_time,
        ctx.next_ledger_seq,
    ) {
        return false;
    }

    // Per-op structural validation (isOpSupported + doCheckValid)
    if !validate_ops_structure(frame, ctx.protocol_version, ctx.ledger_flags) {
        return false;
    }

    // TX-level signature check (LOW threshold for tx source)
    let mut checker = SignatureChecker::new(tx_hash, frame.signatures());
    let signers = collect_signers_for_account(&source_account);
    let threshold_low = source_account.thresholds.0[1] as i32;
    if !checker.check_signature(&signers, threshold_low) {
        debug!("tx-set validation: tx source signature check failed");
        return false;
    }

    // Per-op source auth (every op, including tx-source ops at correct threshold)
    if !validate_ops_auth(frame, &source_account, &mut checker, account_provider) {
        return false;
    }

    // Extra signers (must come before unused-sig check, uses same checker)
    if !validate_extra_signers(frame, &mut checker) {
        return false;
    }

    // Unused signature detection
    if !checker.check_all_signatures_used() {
        debug!("tx-set validation: unused signatures detected (txBAD_AUTH_EXTRA)");
        return false;
    }

    true
}

/// Validate a fee-bump transaction against ledger state.
///
/// Mirrors `FeeBumpTransactionFrame::checkValid`:
/// 1. Outer: load fee source, verify outer auth, check unused
/// 2. Inner: full validation via `checkValidWithOptionallyChargedFee`
fn validate_fee_bump_for_tx_set(
    frame: &TransactionFrame,
    ctx: &TxSetValidationContext,
    lower_close_time: u64,
    account_provider: &dyn AccountProvider,
) -> bool {
    // --- Outer validation ---
    let fee_source_id = frame.fee_source_account_id();
    let fee_source_account = match account_provider.load_account(&fee_source_id) {
        Ok(Some(acc)) => acc,
        Ok(None) => {
            debug!(
                ?fee_source_id,
                "tx-set validation: fee-bump fee source not found"
            );
            return false;
        }
        Err(e) => {
            tracing::warn!(
                error = ?e,
                ?fee_source_id,
                "tx-set validation: fee source lookup failed"
            );
            return false;
        }
    };

    let outer_hash = match frame.hash(&ctx.network_id) {
        Ok(h) => h,
        Err(_) => return false,
    };

    let mut outer_checker = SignatureChecker::new(outer_hash, frame.signatures());
    let outer_signers = collect_signers_for_account(&fee_source_account);
    let outer_threshold = fee_source_account.thresholds.0[1] as i32;
    if !outer_checker.check_signature(&outer_signers, outer_threshold) {
        debug!("tx-set validation: fee-bump outer auth failed");
        return false;
    }

    // CAP-77: Fee-bump fee source frozen key check (after outer auth)
    // Parity: FeeBumpTransactionFrame::checkValid:300-302 — runs after
    // commonValid (which includes outer auth) but before checkAllSignaturesUsed.
    if protocol_version_starts_from(ctx.protocol_version, ProtocolVersion::V20)
        && ctx.frozen_key_config.has_frozen_keys()
        && ctx
            .frozen_key_config
            .is_key_frozen(&henyey_tx::frozen_keys::account_key(&fee_source_id))
        && !ctx.frozen_key_config.is_freeze_bypass_tx(&outer_hash)
    {
        debug!("tx-set validation: fee-bump fee source accesses frozen key");
        return false;
    }

    if !outer_checker.check_all_signatures_used() {
        debug!("tx-set validation: fee-bump outer unused signatures (txBAD_AUTH_EXTRA)");
        return false;
    }

    // --- Inner validation ---
    let inner_source_id = frame.inner_source_account_id();
    let inner_source_account = match account_provider.load_account(&inner_source_id) {
        Ok(Some(acc)) => acc,
        Ok(None) => {
            debug!(
                ?inner_source_id,
                "tx-set validation: fee-bump inner source not found"
            );
            return false;
        }
        Err(e) => {
            tracing::warn!(
                error = ?e,
                ?inner_source_id,
                "tx-set validation: inner source lookup failed"
            );
            return false;
        }
    };

    // CAP-77: Inner tx frozen key check (last step of commonValidPreSeqNum)
    // Parity: stellar-core commonValidPreSeqNum:1548-1560
    // Runs after inner source load, before sequence/signature checks.
    // No protocol gate — relies on has_frozen_keys() being false pre-V26.
    // Bypass uses outer_hash (fee-bump contents hash), not inner hash.
    if ctx.frozen_key_config.has_frozen_keys() {
        let soroban_fp = frame.soroban_data().map(|d| &d.resources.footprint);
        if henyey_tx::frozen_keys::accesses_frozen_key(
            &inner_source_id,
            frame.operations(),
            soroban_fp,
            &ctx.frozen_key_config,
        ) && !ctx.frozen_key_config.is_freeze_bypass_tx(&outer_hash)
        {
            debug!("tx-set validation: fee-bump inner tx accesses frozen key");
            return false;
        }
    }

    // Inner sequence
    if !validate_sequence(frame, &inner_source_account, ctx.next_ledger_seq) {
        return false;
    }

    // Inner min seq age/gap
    if !validate_min_seq_age_gap(
        frame,
        &inner_source_account,
        lower_close_time,
        ctx.next_ledger_seq,
    ) {
        return false;
    }

    // Inner per-op structural validation (isOpSupported + doCheckValid)
    if !validate_ops_structure(frame, ctx.protocol_version, ctx.ledger_flags) {
        return false;
    }

    // Inner signature check
    let inner_hash = match frame.inner_hash(&ctx.network_id) {
        Ok(h) => h,
        Err(_) => return false,
    };
    let inner_sigs = frame.inner_signatures();
    let mut inner_checker = SignatureChecker::new(inner_hash, inner_sigs);
    let inner_signers = collect_signers_for_account(&inner_source_account);
    let inner_threshold = inner_source_account.thresholds.0[1] as i32;
    if !inner_checker.check_signature(&inner_signers, inner_threshold) {
        debug!("tx-set validation: fee-bump inner auth failed");
        return false;
    }

    // Inner per-op auth (every op at correct threshold)
    if !validate_ops_auth(
        frame,
        &inner_source_account,
        &mut inner_checker,
        account_provider,
    ) {
        return false;
    }

    // Inner extra signers (before unused-sig check)
    if !validate_extra_signers(frame, &mut inner_checker) {
        return false;
    }

    // Inner unused signature detection
    if !inner_checker.check_all_signatures_used() {
        debug!("tx-set validation: fee-bump inner unused signatures (txBAD_AUTH_EXTRA)");
        return false;
    }

    true
}

/// Validate sequence number (mirrors `isBadSeq`).
fn validate_sequence(
    frame: &TransactionFrame,
    account: &AccountEntry,
    next_ledger_seq: u32,
) -> bool {
    let tx_seq = frame.sequence_number();
    let account_seq = account.seq_num.0;

    // Reject if tx_seq equals the starting sequence number for this ledger.
    if next_ledger_seq <= i32::MAX as u32 {
        let starting_seq = (next_ledger_seq as i64) << 32;
        if tx_seq == starting_seq {
            debug!(
                tx_seq,
                starting_seq, "tx-set validation: bad seq (starting sequence)"
            );
            return false;
        }
    }

    let min_seq_num = match frame.preconditions() {
        Preconditions::V2(cond) => cond.min_seq_num.map(|s| s.0),
        _ => None,
    };

    let is_bad_seq = if let Some(min_seq) = min_seq_num {
        account_seq < min_seq || account_seq >= tx_seq
    } else {
        account_seq == i64::MAX || account_seq + 1 != tx_seq
    };

    if is_bad_seq {
        debug!(
            account_seq,
            tx_seq,
            min_seq_num = ?min_seq_num,
            "tx-set validation: bad sequence number"
        );
    }

    !is_bad_seq
}

/// Validate min sequence age and ledger gap (mirrors `isTooEarlyForAccount`).
///
/// Uses lower-bound close time, matching stellar-core's
/// `isTooEarlyForAccount(lowerBoundCloseTimeOffset)`.
fn validate_min_seq_age_gap(
    frame: &TransactionFrame,
    account: &AccountEntry,
    lower_close_time: u64,
    next_ledger_seq: u32,
) -> bool {
    let preconditions = frame.preconditions();
    let cond = match preconditions {
        Preconditions::V2(ref cond) => cond,
        _ => return true,
    };

    // Min seq age check
    if cond.min_seq_age.0 > 0 {
        let acc_seq_time = henyey_tx::state::get_account_seq_time(account);
        let min_seq_age = cond.min_seq_age.0;
        if min_seq_age > lower_close_time || lower_close_time - min_seq_age < acc_seq_time {
            debug!(
                min_seq_age,
                lower_close_time, acc_seq_time, "tx-set validation: min seq age not met"
            );
            return false;
        }
    }

    // Min seq ledger gap check
    if cond.min_seq_ledger_gap > 0 {
        let acc_seq_ledger = henyey_tx::state::get_account_seq_ledger(account);
        let min_seq_ledger_gap = cond.min_seq_ledger_gap;
        if min_seq_ledger_gap > next_ledger_seq
            || next_ledger_seq - min_seq_ledger_gap < acc_seq_ledger
        {
            debug!(
                min_seq_ledger_gap,
                next_ledger_seq, acc_seq_ledger, "tx-set validation: min seq ledger gap not met"
            );
            return false;
        }
    }

    true
}

/// Validate per-operation structural validity (isOpSupported + doCheckValid).
///
/// Mirrors stellar-core's per-op OperationFrame::checkValid() structural checks.
/// Returns false if any operation is unsupported or structurally invalid.
fn validate_ops_structure(
    frame: &TransactionFrame,
    protocol_version: u32,
    ledger_flags: u32,
) -> bool {
    if frame.is_soroban() {
        return true;
    }

    let inner_source_id = frame.inner_source_account_id();

    for op in frame.operations().iter() {
        let op_type = henyey_tx::OperationType::from_body(&op.body);
        if henyey_tx::is_op_supported(&op_type, protocol_version, ledger_flags).is_err() {
            debug!("tx-set validation: op not supported");
            return false;
        }

        let effective_source = match &op.source_account {
            Some(muxed) => muxed_to_account_id(muxed),
            None => inner_source_id.clone(),
        };
        if henyey_tx::validate_classic_op_structure(op, protocol_version, Some(&effective_source))
            .is_err()
        {
            debug!("tx-set validation: op structurally invalid");
            return false;
        }
    }

    true
}

/// Validate per-operation source account auth.
///
/// Mirrors `OperationFrame::checkValid(!forApply)` — for each operation,
/// check signatures at the operation's required threshold level.
///
/// IMPORTANT: We do NOT skip ops where op source == tx source. TX-level auth
/// only proves LOW threshold, but ops may require MEDIUM or HIGH threshold.
/// stellar-core's checkOperationSignatures checks every op unconditionally.
fn validate_ops_auth(
    frame: &TransactionFrame,
    tx_source_account: &AccountEntry,
    checker: &mut SignatureChecker,
    account_provider: &dyn AccountProvider,
) -> bool {
    let tx_source_id = tx_source_account.account_id.clone();

    for op in frame.operations() {
        let op_source_muxed = &op.source_account;

        // Resolve op source: use op.source_account if set, else TX source.
        // Keep any loaded account alive so we can borrow it.
        let loaded_account;
        let (op_source_id, op_account) = if let Some(ref src) = op_source_muxed {
            let id = muxed_to_account_id(src);
            if id == tx_source_id {
                // Same account, but must check at op-specific threshold
                (id, Some(tx_source_account))
            } else {
                loaded_account = match account_provider.load_account(&id) {
                    Ok(acc) => acc,
                    Err(e) => {
                        tracing::warn!(
                            error = ?e,
                            ?id,
                            "tx-set validation: op source lookup failed"
                        );
                        return false;
                    }
                };
                (id, loaded_account.as_ref())
            }
        } else {
            // No explicit op source → use tx source
            (tx_source_id.clone(), Some(tx_source_account))
        };

        let threshold_level = get_threshold_level(op);

        match op_account {
            Some(account) => {
                let needed_threshold = account.thresholds.0[threshold_level as usize] as i32;
                let signers = collect_signers_for_account(account);
                if !checker.check_signature(&signers, needed_threshold) {
                    debug!(?op_source_id, "tx-set validation: op auth failed");
                    return false;
                }
            }
            None => {
                // Account doesn't exist — checkSignatureNoAccount:
                // verify signature against the account ID's public key with weight=1, needed=0
                if op_source_muxed.is_none() {
                    // No explicit source and account doesn't exist → opNoAccount
                    debug!(
                        ?op_source_id,
                        "tx-set validation: op source not found (no explicit source)"
                    );
                    return false;
                }
                let stellar_xdr::PublicKey::PublicKeyTypeEd25519(ref key) = op_source_id.0;
                let signers = vec![stellar_xdr::Signer {
                    key: SignerKey::Ed25519(key.clone()),
                    weight: 1,
                }];
                // checkSignatureNoAccount uses needed=0 (just marks signature as used)
                if !checker.check_signature(&signers, 0) {
                    debug!(
                        ?op_source_id,
                        "tx-set validation: op auth failed (no account)"
                    );
                    return false;
                }
            }
        }
    }
    true
}

/// Validate extra signers from V2 preconditions.
///
/// Mirrors stellar-core's `extraSignersExist()` → `checkSignature(extraSigners,
/// extraSigners.size())`. Each extra signer is given weight 1, and the needed
/// weight is the count of extra signers. Must be called BEFORE
/// `check_all_signatures_used()` since it marks extra signer signatures as used.
fn validate_extra_signers(frame: &TransactionFrame, checker: &mut SignatureChecker) -> bool {
    let cond = match frame.preconditions() {
        Preconditions::V2(cond) => cond,
        _ => return true,
    };

    if cond.extra_signers.is_empty() {
        return true;
    }

    let signers: Vec<stellar_xdr::Signer> = cond
        .extra_signers
        .iter()
        .map(|key| stellar_xdr::Signer {
            key: key.clone(),
            weight: 1,
        })
        .collect();

    let needed_weight = signers.len() as i32;
    if !checker.check_signature(&signers, needed_weight) {
        debug!("tx-set validation: missing extra signer(s)");
        return false;
    }

    true
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
    account_provider: Option<&dyn AccountProvider>,
    account_fee_map: &mut HashMap<AccountId, i64>,
) -> Vec<TransactionEnvelope> {
    let hashed: Vec<HashedTx> = txs.iter().map(|tx| HashedTx::new(tx.clone())).collect();
    let invalid_hashed = get_invalid_hashed_core(
        &hashed,
        ctx,
        close_time_bounds,
        fee_balance_provider,
        account_provider,
        account_fee_map,
    );
    invalid_hashed
        .into_iter()
        .map(|htx| htx.into_envelope())
        .collect()
}

/// Invalidation for pre-hashed transactions (queue path).
///
/// Same logic as [`get_invalid_tx_list_with_fee_map`] but accepts `&[HashedTx]`
/// to avoid redundant hash computation. Returns `Vec<HashedTx>` so callers
/// get hashes without re-computation (e.g. for banning).
pub fn get_invalid_hashed_tx_list_with_fee_map(
    txs: &[HashedTx],
    ctx: &TxSetValidationContext,
    close_time_bounds: &CloseTimeBounds,
    fee_balance_provider: Option<&dyn FeeBalanceProvider>,
    account_provider: Option<&dyn AccountProvider>,
    account_fee_map: &mut HashMap<AccountId, i64>,
) -> Vec<HashedTx> {
    get_invalid_hashed_core(
        txs,
        ctx,
        close_time_bounds,
        fee_balance_provider,
        account_provider,
        account_fee_map,
    )
}

/// Convenience wrapper that creates a local fee map.
pub fn get_invalid_hashed_tx_list(
    txs: &[HashedTx],
    ctx: &TxSetValidationContext,
    close_time_bounds: &CloseTimeBounds,
    fee_balance_provider: Option<&dyn FeeBalanceProvider>,
    account_provider: Option<&dyn AccountProvider>,
) -> Vec<HashedTx> {
    let mut account_fee_map: HashMap<AccountId, i64> = HashMap::new();
    get_invalid_hashed_core(
        txs,
        ctx,
        close_time_bounds,
        fee_balance_provider,
        account_provider,
        &mut account_fee_map,
    )
}

/// Private core: validates transactions and returns invalid ones with hashes.
///
/// Accepts pre-hashed transactions. In pass 1, caches `fee_source_id` from the
/// `TransactionFrame` constructed for validation. In pass 2, looks up fee source
/// from the cache instead of constructing a new frame.
fn get_invalid_hashed_core(
    txs: &[HashedTx],
    ctx: &TxSetValidationContext,
    close_time_bounds: &CloseTimeBounds,
    fee_balance_provider: Option<&dyn FeeBalanceProvider>,
    account_provider: Option<&dyn AccountProvider>,
    account_fee_map: &mut HashMap<AccountId, i64>,
) -> Vec<HashedTx> {
    let mut invalid_txs = Vec::new();
    let mut seen_invalid: HashSet<Hash256> = HashSet::new();

    let upper_close_time = ctx
        .close_time
        .saturating_add(close_time_bounds.upper_bound_offset);
    let lower_close_time = ctx
        .close_time
        .saturating_add(close_time_bounds.lower_bound_offset);

    let upper_ledger_ctx = ctx.to_ledger_context(upper_close_time);
    let need_lower_check = lower_close_time != upper_close_time;

    // Pass-1 fee_source cache: avoids TransactionFrame construction in pass 2.
    let mut fee_source_cache: HashMap<Hash256, AccountId> = HashMap::new();

    for htx in txs {
        let frame = TransactionFrame::with_network(htx.envelope.clone(), ctx.network_id);

        // Stateless structural + per-op validation (shared with queue admission).
        if check_valid_pre_seq_num_with_config(
            &frame,
            ctx.protocol_version,
            ctx.ledger_flags,
            ctx.soroban_resource_limits.as_ref(),
        )
        .is_err()
        {
            seen_invalid.insert(htx.hash);
            invalid_txs.push(htx.clone());
            continue;
        }

        // Per-op structural validation (isOpSupported + doCheckValid).
        // This is stateless and always runs, even when account_provider is None.
        if !validate_ops_structure(&frame, ctx.protocol_version, ctx.ledger_flags) {
            seen_invalid.insert(htx.hash);
            invalid_txs.push(htx.clone());
            continue;
        }

        // Validate with upper bound close time (catches max_time violations).
        let upper_result = validate_basic(&frame, &upper_ledger_ctx);

        if upper_result.is_err() {
            seen_invalid.insert(htx.hash);
            invalid_txs.push(htx.clone());
            continue;
        }

        // If offsets differ, also validate with lower bound close time.
        if need_lower_check {
            let lower_ledger_ctx = ctx.to_ledger_context(lower_close_time);
            if validate_basic(&frame, &lower_ledger_ctx).is_err() {
                seen_invalid.insert(htx.hash);
                invalid_txs.push(htx.clone());
                continue;
            }
        }

        // Stateful validation: sequence, auth, and signature checks.
        if let Some(provider) = account_provider {
            if !validate_tx_for_tx_set(&frame, ctx, lower_close_time, provider) {
                seen_invalid.insert(htx.hash);
                invalid_txs.push(htx.clone());
                continue;
            }
        }

        // Transaction passed basic validation — accumulate fee for fee source.
        if fee_balance_provider.is_some() {
            let fee_source = frame.fee_source_account_id();
            let full_fee = frame.total_fee().as_i64();
            let entry = account_fee_map.entry(fee_source.clone()).or_insert(0i64);
            // Saturating add to avoid overflow (matches stellar-core).
            *entry = entry.saturating_add(full_fee);
            // Cache fee_source for pass-2 lookup.
            fee_source_cache.insert(htx.hash, fee_source);
        }
    }

    // --- Pass 2: fee-source affordability check ---
    if let Some(provider) = fee_balance_provider {
        for htx in txs {
            if seen_invalid.contains(&htx.hash) {
                continue;
            }

            let fee_source = match fee_source_cache.get(&htx.hash) {
                Some(fs) => fs,
                None => continue,
            };

            let available = match provider.get_available_balance(fee_source) {
                Ok(Some(v)) => v,
                Ok(None) => 0,
                Err(e) => {
                    tracing::warn!(
                        error = ?e,
                        ?fee_source,
                        "fee balance lookup failed during trim"
                    );
                    0
                }
            };
            let total_fee = account_fee_map.get(fee_source).copied().unwrap_or(0);

            if available < total_fee {
                invalid_txs.push(htx.clone());
                seen_invalid.insert(htx.hash);
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
    account_provider: Option<&dyn AccountProvider>,
) -> (Vec<TransactionEnvelope>, Vec<TransactionEnvelope>) {
    let invalid_txs = get_invalid_tx_list(
        txs,
        ctx,
        close_time_bounds,
        fee_balance_provider,
        account_provider,
    );

    if invalid_txs.is_empty() {
        return (txs.to_vec(), Vec::new());
    }

    let valid_txs = remove_txs(txs, &invalid_txs);
    (valid_txs, invalid_txs)
}

/// Remove a subset of hashed transactions using pre-computed hashes.
fn remove_hashed_txs(txs: Vec<HashedTx>, to_remove: &[HashedTx]) -> Vec<HashedTx> {
    let remove_set: HashSet<Hash256> = to_remove.iter().map(|htx| htx.hash()).collect();
    txs.into_iter()
        .filter(|htx| !remove_set.contains(&htx.hash()))
        .collect()
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
        .map(|tx| Hash256::hash_xdr(tx))
        .collect();

    txs.iter()
        .filter(|tx| !remove_set.contains(&Hash256::hash_xdr(*tx)))
        .cloned()
        .collect()
}

/// Trim invalid transactions from two phases (Classic + Soroban), sharing the
/// fee-source map across phases for Protocol 26+.
///
/// Hashed variant: accepts and returns `Vec<HashedTx>`, avoiding redundant
/// hash computation and deep clones throughout the pipeline.
///
/// # Parity
///
/// Mirrors `makeTxSetFromTransactions` in stellar-core `TxSetFrame.cpp:836-860`.
pub(crate) fn trim_invalid_two_phase_hashed(
    classic_txs: Vec<HashedTx>,
    soroban_txs: Vec<HashedTx>,
    ctx: &TxSetValidationContext,
    close_time_bounds: &CloseTimeBounds,
    fee_balance_provider: Option<&dyn FeeBalanceProvider>,
    account_provider: Option<&dyn AccountProvider>,
) -> (Vec<HashedTx>, Vec<HashedTx>) {
    use henyey_common::protocol::{protocol_version_starts_from, ProtocolVersion};

    let use_cross_phase_fee_map =
        protocol_version_starts_from(ctx.protocol_version, ProtocolVersion::V26);

    let mut account_fee_map: HashMap<AccountId, i64> = HashMap::new();

    // Phase 0: Classic
    let classic_invalid = get_invalid_hashed_tx_list_with_fee_map(
        &classic_txs,
        ctx,
        close_time_bounds,
        fee_balance_provider,
        account_provider,
        &mut account_fee_map,
    );
    let valid_classic = if classic_invalid.is_empty() {
        classic_txs
    } else {
        remove_hashed_txs(classic_txs, &classic_invalid)
    };

    // For pre-V26, clear the fee map between phases (each phase is independent).
    if !use_cross_phase_fee_map {
        account_fee_map.clear();
    }

    // Phase 1: Soroban
    let soroban_invalid = get_invalid_hashed_tx_list_with_fee_map(
        &soroban_txs,
        ctx,
        close_time_bounds,
        fee_balance_provider,
        account_provider,
        &mut account_fee_map,
    );
    let valid_soroban = if soroban_invalid.is_empty() {
        soroban_txs
    } else {
        remove_hashed_txs(soroban_txs, &soroban_invalid)
    };

    (valid_classic, valid_soroban)
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
    account_provider: Option<&dyn AccountProvider>,
) -> (Vec<TransactionEnvelope>, Vec<TransactionEnvelope>) {
    let classic_hashed: Vec<HashedTx> = classic_txs
        .iter()
        .map(|tx| HashedTx::new(tx.clone()))
        .collect();
    let soroban_hashed: Vec<HashedTx> = soroban_txs
        .iter()
        .map(|tx| HashedTx::new(tx.clone()))
        .collect();

    let (valid_classic, valid_soroban) = trim_invalid_two_phase_hashed(
        classic_hashed,
        soroban_hashed,
        ctx,
        close_time_bounds,
        fee_balance_provider,
        account_provider,
    );

    (
        valid_classic
            .into_iter()
            .map(|htx| htx.into_envelope())
            .collect(),
        valid_soroban
            .into_iter()
            .map(|htx| htx.into_envelope())
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// TX set content validation functions (AUDIT-033)
// ---------------------------------------------------------------------------

// Re-export envelope classification helpers from the canonical source in henyey_tx.
pub(crate) use henyey_tx::envelope_utils::envelope_soroban_resources;
pub(crate) use henyey_tx::envelope_utils::has_dex_operations_envelope;
pub(crate) use henyey_tx::envelope_utils::is_soroban_envelope;

/// Unified result of transaction set content validation.
///
/// Mirrors the content-validation subset of stellar-core's `TxSetValidationResult`
/// (TxSetFrame.h:52-96). Covers fee-map, classic-phase, Soroban-phase, and
/// per-TX validation results. Structural XDR validation variants (e.g.,
/// `INCORRECT_COMPONENT_ORDER`, `EMPTY_STAGE`) are not included here — they
/// belong to the structural validation path (`validate_generalized_tx_set_xdr_structure`
/// / `prepare_for_apply`).
///
/// Display strings use SCREAMING_SNAKE_CASE matching stellar-core's `toString()`
/// for log parity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TxSetValidationResult {
    Valid,

    // Structural errors (content-validation level)
    /// Protocol/format mismatch: generalized set on pre-V20 or legacy set on V20+.
    GeneralizedTxsetMismatch,
    /// Expected exactly 2 phases (classic + Soroban).
    WrongPhaseCount,

    // Classic phase errors
    ClassicPhaseParallelNotAllowed,
    TooManyClassicTxs,

    // Soroban phase errors
    SorobanParallelSupportMismatch,
    SorobanResourcesOverflow,
    SorobanResourcesExceedLimit,
    TooManySorobanClusters,
    SorobanInstructionsOverflow,
    SorobanInstructionsExceedLimit,
    SorobanSequentialInstructionsOverflow,
    /// Soroban phase present but network config unavailable. henyey-specific;
    /// stellar-core always has config for V20+ ledgers.
    SorobanConfigUnavailable,

    // Transaction-level errors
    InvalidPhaseTxType,
    TxOrderingInvalid,

    // Fee errors
    ComponentBaseFeeTooLow,
    TxFeeBidTooLow,

    // Individual transaction validation
    TxValidationFailed,

    // Missing spec codes — added for HERDER_SPEC §9 parity.
    /// Previous ledger hash does not match the expected LCL hash.
    PreviousLedgerHashMismatch,
    /// Multiple transactions from the same source account in the set.
    MultipleTxsPerSourceAccount,
}

impl std::fmt::Display for TxSetValidationResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Valid => write!(f, "VALID"),
            Self::GeneralizedTxsetMismatch => write!(f, "GENERALIZED_TXSET_MISMATCH"),
            Self::WrongPhaseCount => write!(f, "WRONG_PHASE_COUNT"),
            Self::ClassicPhaseParallelNotAllowed => {
                write!(f, "CLASSIC_PHASE_PARALLEL_NOT_ALLOWED")
            }
            Self::TooManyClassicTxs => write!(f, "TOO_MANY_CLASSIC_TXS"),
            Self::SorobanParallelSupportMismatch => {
                write!(f, "SOROBAN_PARALLEL_SUPPORT_MISMATCH")
            }
            Self::SorobanResourcesOverflow => write!(f, "SOROBAN_RESOURCES_OVERFLOW"),
            Self::SorobanResourcesExceedLimit => write!(f, "SOROBAN_RESOURCES_EXCEED_LIMIT"),
            Self::TooManySorobanClusters => write!(f, "TOO_MANY_SOROBAN_CLUSTERS"),
            Self::SorobanInstructionsOverflow => write!(f, "SOROBAN_INSTRUCTIONS_OVERFLOW"),
            Self::SorobanInstructionsExceedLimit => {
                write!(f, "SOROBAN_INSTRUCTIONS_EXCEED_LIMIT")
            }
            Self::SorobanSequentialInstructionsOverflow => {
                write!(f, "SOROBAN_SEQUENTIAL_INSTRUCTIONS_OVERFLOW")
            }
            Self::SorobanConfigUnavailable => write!(f, "SOROBAN_CONFIG_UNAVAILABLE"),
            Self::InvalidPhaseTxType => write!(f, "INVALID_PHASE_TX_TYPE"),
            Self::TxOrderingInvalid => write!(f, "TX_ORDERING_INVALID"),
            Self::ComponentBaseFeeTooLow => write!(f, "COMPONENT_BASE_FEE_TOO_LOW"),
            Self::TxFeeBidTooLow => write!(f, "TX_FEE_BID_TOO_LOW"),
            Self::TxValidationFailed => write!(f, "TX_VALIDATION_FAILED"),
            Self::PreviousLedgerHashMismatch => write!(f, "PREVIOUS_LEDGER_HASH_MISMATCH"),
            Self::MultipleTxsPerSourceAccount => {
                write!(f, "MULTIPLE_TXS_PER_SOURCE_ACCOUNT")
            }
        }
    }
}

/// Structured error for transaction set content validation.
///
/// Wraps [`TxSetValidationResult`] with diagnostic context (phase index,
/// invalid transaction count) to preserve the information previously embedded
/// in ad-hoc error strings.
#[derive(Debug)]
#[non_exhaustive]
pub struct TxSetValidationError {
    pub result: TxSetValidationResult,
    pub phase_idx: Option<usize>,
    pub invalid_tx_count: Option<usize>,
}

impl TxSetValidationError {
    pub fn new(result: TxSetValidationResult) -> Self {
        Self {
            result,
            phase_idx: None,
            invalid_tx_count: None,
        }
    }

    pub fn with_phase(mut self, phase_idx: usize) -> Self {
        self.phase_idx = Some(phase_idx);
        self
    }

    pub fn with_invalid_tx_count(mut self, count: usize) -> Self {
        self.invalid_tx_count = Some(count);
        self
    }
}

impl std::fmt::Display for TxSetValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(phase_idx) = self.phase_idx {
            write!(f, "phase {}: {}", phase_idx, self.result)?;
        } else {
            write!(f, "{}", self.result)?;
        }
        if let Some(count) = self.invalid_tx_count {
            write!(f, " ({} invalid transactions)", count)?;
        }
        Ok(())
    }
}

impl std::error::Error for TxSetValidationError {}

/// Validates fee constraints for a set of transactions with a given optional base fee.
///
/// If `base_fee` is None, returns `Valid` — stellar-core skips fee validation
/// for None-baseFee components (TxSetFrame.cpp:726-728).
///
/// Precedence matches stellar-core: base_fee vs lcl check happens first;
/// `ComponentBaseFeeTooLow` is returned before any per-tx checks.
fn validate_fee_component<'a>(
    base_fee: Option<i64>,
    txs: impl Iterator<Item = &'a TransactionEnvelope>,
    lcl_base_fee: u32,
) -> TxSetValidationResult {
    let Some(base_fee) = base_fee else {
        return TxSetValidationResult::Valid;
    };
    // Compare as signed i64 — stellar-core promotes uint32_t baseFee
    // to int64_t, so a negative XDR base_fee correctly fails.
    if base_fee < lcl_base_fee as i64 {
        debug!(
            "Got bad txSet: component base fee {} < lcl base fee {}",
            base_fee, lcl_base_fee
        );
        return TxSetValidationResult::ComponentBaseFeeTooLow;
    }
    for tx in txs {
        let tx_inclusion_fee = envelope_inclusion_fee(tx);
        let min_fee = get_min_inclusion_fee(tx, lcl_base_fee, base_fee);
        if tx_inclusion_fee < min_fee {
            debug!(
                "Got bad txSet: tx fee bid ({}) lower than base fee ({})",
                tx_inclusion_fee, min_fee
            );
            return TxSetValidationResult::TxFeeBidTooLow;
        }
    }
    TxSetValidationResult::Valid
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
pub(crate) fn check_fee_map(phase: &TransactionPhase, lcl_base_fee: u32) -> TxSetValidationResult {
    match phase {
        TransactionPhase::V0(components) => {
            for component in components.iter() {
                let TxSetComponent::TxsetCompTxsMaybeDiscountedFee(comp) = component;
                let result = validate_fee_component(comp.base_fee, comp.txs.iter(), lcl_base_fee);
                if result != TxSetValidationResult::Valid {
                    return result;
                }
            }
            TxSetValidationResult::Valid
        }
        TransactionPhase::V1(parallel) => validate_fee_component(
            parallel.base_fee,
            parallel
                .execution_stages
                .iter()
                .flat_map(|stage| stage.iter())
                .flat_map(|cluster| cluster.iter()),
            lcl_base_fee,
        ),
    }
}

/// Compute the minimum inclusion fee for a transaction.
///
/// Mirrors stellar-core's `getMinInclusionFee()` (TransactionUtils.cpp:1961-1971).
/// effectiveBaseFee = max(header.baseFee, componentBaseFee)
/// minFee = effectiveBaseFee * max(1, numOps)
///
/// Only called when `component_base_fee` is known (i.e., the component's XDR
/// `baseFee` optional was present). When `baseFee` is absent, stellar-core
/// skips fee-map validation entirely (TxSetFrame.cpp:726-728).
fn get_min_inclusion_fee(
    env: &TransactionEnvelope,
    lcl_base_fee: u32,
    component_base_fee: i64,
) -> henyey_tx::InclusionFee {
    let effective_base_fee = std::cmp::max(lcl_base_fee as i64, component_base_fee);
    let num_ops = std::cmp::max(1, envelope_num_ops(env) as i64);
    henyey_tx::InclusionFee::new(effective_base_fee.saturating_mul(num_ops))
}

/// Validate the classic (non-Soroban) transaction phase.
///
/// Mirrors stellar-core's `TxSetPhaseFrame::checkValidClassic()` (TxSetFrame.cpp:1802-1816).
///
/// - Rejects if the phase is V1 (parallel) — classic can only be V0/sequential
/// - Counts total operations and verifies <= `max_tx_set_size`
/// - Verifies all TXs are non-Soroban
pub(crate) fn check_valid_classic(
    phase: &TransactionPhase,
    max_tx_set_size: u32,
) -> TxSetValidationResult {
    let components = match phase {
        TransactionPhase::V0(components) => components,
        TransactionPhase::V1(_) => {
            debug!("Got bad txSet: classic phase can't be parallel");
            return TxSetValidationResult::ClassicPhaseParallelNotAllowed;
        }
    };

    let mut total_ops: u64 = 0;
    for component in components.iter() {
        let TxSetComponent::TxsetCompTxsMaybeDiscountedFee(comp) = component;
        for tx in comp.txs.iter() {
            if is_soroban_envelope(tx) {
                debug!("Got bad txSet: Soroban transaction found in classic phase");
                return TxSetValidationResult::InvalidPhaseTxType;
            }
            total_ops += envelope_num_ops(tx) as u64;
        }
    }

    if total_ops > max_tx_set_size as u64 {
        debug!(
            "Got bad txSet: too many classic ops {} > {}",
            total_ops, max_tx_set_size
        );
        return TxSetValidationResult::TooManyClassicTxs;
    }

    TxSetValidationResult::Valid
}

/// Accumulate Soroban resources, returning `SorobanResourcesOverflow` on overflow.
///
/// Both inputs must be canonical 7-element Soroban resources; a `SizeMismatch`
/// error from `checked_add` indicates a programmer bug.
fn accumulate_resources(
    total: henyey_common::resource::Resource,
    addition: &henyey_common::resource::Resource,
) -> Result<henyey_common::resource::Resource, TxSetValidationResult> {
    total.checked_add(addition).map_err(|e| {
        match e {
            ResourceError::Overflow { .. } => {}
            _ => unreachable!("accumulate_resources: unexpected ResourceError: {e}"),
        }
        debug!("Got bad txSet: Soroban resource overflow");
        TxSetValidationResult::SorobanResourcesOverflow
    })
}

/// Check that adding `addition` (a u32 instruction count from XDR) to the
/// running `current` total does not overflow i64. Returns the new total or
/// `SorobanSequentialInstructionsOverflow`.
fn checked_add_cluster_instructions(
    current: i64,
    addition: u32,
) -> Result<i64, TxSetValidationResult> {
    let addition = addition as i64;
    if current > i64::MAX - addition {
        debug!("Got bad txSet: Soroban sequential instructions overflow");
        return Err(TxSetValidationResult::SorobanSequentialInstructionsOverflow);
    }
    Ok(current + addition)
}

/// Check that adding `stage_max` to `sequential` does not overflow i64.
/// Returns the new total or `SorobanInstructionsOverflow`.
fn checked_add_sequential_instructions(
    sequential: i64,
    stage_max: i64,
) -> Result<i64, TxSetValidationResult> {
    debug_assert!(stage_max >= 0, "stage_max must be non-negative");
    if sequential > i64::MAX - stage_max {
        debug!("Got bad txSet: Soroban total instructions overflow");
        return Err(TxSetValidationResult::SorobanInstructionsOverflow);
    }
    Ok(sequential + stage_max)
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
) -> TxSetValidationResult {
    let protocol = lcl_header.ledger_version;
    let need_parallel = protocol_version_starts_from(protocol, ProtocolVersion::V23);

    let is_parallel = matches!(phase, TransactionPhase::V1(_));
    if is_parallel != need_parallel {
        debug!(
            "Got bad txSet: Soroban phase parallel support mismatch; expected {}",
            need_parallel
        );
        return TxSetValidationResult::SorobanParallelSupportMismatch;
    }

    // Aggregate total resources across all TXs using TransactionFrame::resources()
    // which correctly handles fee-bump tx_size and operation count.
    let mut total_resources = henyey_common::resource::Resource::make_empty_soroban();

    let all_txs = collect_phase_txs(phase);

    for tx in &all_txs {
        if !is_soroban_envelope(tx) {
            debug!("Got bad txSet: non-Soroban transaction found in Soroban phase");
            return TxSetValidationResult::InvalidPhaseTxType;
        }
        let frame = TransactionFrame::new(Arc::new((*tx).clone()));
        let res = frame.resources(false, protocol);
        match accumulate_resources(total_resources, &res) {
            Ok(sum) => total_resources = sum,
            Err(e) => return e,
        }
    }

    let total_instructions = total_resources.get_val(ResourceType::Instructions);
    let total_read_entries = total_resources.get_val(ResourceType::ReadLedgerEntries);
    let total_read_bytes = total_resources.get_val(ResourceType::DiskReadBytes);
    let total_write_entries = total_resources.get_val(ResourceType::WriteLedgerEntries);
    let total_write_bytes = total_resources.get_val(ResourceType::WriteBytes);
    let total_tx_size_bytes = total_resources.get_val(ResourceType::TxByteSize);
    let total_ops = total_resources.get_val(ResourceType::Operations);

    // Check resource limits (skip instructions for parallel — handled below)
    if !is_parallel && total_instructions > soroban_info.ledger_max_instructions {
        debug!(
            "Got bad txSet: Soroban instructions {} > ledger max {}",
            total_instructions, soroban_info.ledger_max_instructions
        );
        return TxSetValidationResult::SorobanResourcesExceedLimit;
    }
    if total_read_entries > soroban_info.ledger_max_read_ledger_entries as i64 {
        debug!(
            "Got bad txSet: Soroban read entries {} > ledger max {}",
            total_read_entries, soroban_info.ledger_max_read_ledger_entries
        );
        return TxSetValidationResult::SorobanResourcesExceedLimit;
    }
    if total_read_bytes > soroban_info.ledger_max_read_bytes as i64 {
        debug!(
            "Got bad txSet: Soroban read bytes {} > ledger max {}",
            total_read_bytes, soroban_info.ledger_max_read_bytes
        );
        return TxSetValidationResult::SorobanResourcesExceedLimit;
    }
    if total_write_entries > soroban_info.ledger_max_write_ledger_entries as i64 {
        debug!(
            "Got bad txSet: Soroban write entries {} > ledger max {}",
            total_write_entries, soroban_info.ledger_max_write_ledger_entries
        );
        return TxSetValidationResult::SorobanResourcesExceedLimit;
    }
    if total_write_bytes > soroban_info.ledger_max_write_bytes as i64 {
        debug!(
            "Got bad txSet: Soroban write bytes {} > ledger max {}",
            total_write_bytes, soroban_info.ledger_max_write_bytes
        );
        return TxSetValidationResult::SorobanResourcesExceedLimit;
    }
    if total_tx_size_bytes > soroban_info.ledger_max_tx_size_bytes as i64 {
        debug!(
            "Got bad txSet: Soroban tx size bytes {} > ledger max {}",
            total_tx_size_bytes, soroban_info.ledger_max_tx_size_bytes
        );
        return TxSetValidationResult::SorobanResourcesExceedLimit;
    }
    if total_ops > soroban_info.ledger_max_tx_count as i64 {
        debug!(
            "Got bad txSet: Soroban tx count {} > ledger max {}",
            total_ops, soroban_info.ledger_max_tx_count
        );
        return TxSetValidationResult::SorobanResourcesExceedLimit;
    }

    // Sequential phase is done
    if !is_parallel {
        return TxSetValidationResult::Valid;
    }

    // Parallel-specific validation
    let TransactionPhase::V1(parallel) = phase else {
        unreachable!("is_parallel is true but phase is not V1");
    };

    // Check cluster count per stage
    for stage in parallel.execution_stages.iter() {
        if stage.len() as u32 > soroban_info.ledger_max_dependent_tx_clusters {
            debug!(
                "Got bad txSet: too many clusters in Soroban stage {} > {}",
                stage.len(),
                soroban_info.ledger_max_dependent_tx_clusters
            );
            return TxSetValidationResult::TooManySorobanClusters;
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
                    match checked_add_cluster_instructions(
                        cluster_instructions,
                        resources.instructions,
                    ) {
                        Ok(sum) => cluster_instructions = sum,
                        Err(e) => return e,
                    }
                }
            }
            stage_max_instructions = std::cmp::max(stage_max_instructions, cluster_instructions);
        }
        match checked_add_sequential_instructions(sequential_instructions, stage_max_instructions) {
            Ok(sum) => sequential_instructions = sum,
            Err(e) => return e,
        }
    }
    if sequential_instructions > soroban_info.ledger_max_instructions {
        debug!(
            "Got bad txSet: Soroban total instructions exceed limit: {} > {}",
            sequential_instructions, soroban_info.ledger_max_instructions
        );
        return TxSetValidationResult::SorobanInstructionsExceedLimit;
    }

    // RW conflict detection between clusters within each stage
    for stage in parallel.execution_stages.iter() {
        let result = check_stage_footprint_conflicts(stage);
        if result != TxSetValidationResult::Valid {
            return result;
        }
    }

    TxSetValidationResult::Valid
}

/// Check that no cluster's footprint conflicts with another cluster within the same stage.
///
/// A read-only key in one cluster must not appear as read-write in another cluster,
/// and a read-write key in one cluster must not appear in any other cluster's footprint.
fn check_stage_footprint_conflicts(
    stage: &stellar_xdr::ParallelTxExecutionStage,
) -> TxSetValidationResult {
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
                        debug!("Got bad txSet: cluster footprint conflicts with another cluster within stage");
                        return TxSetValidationResult::TxOrderingInvalid;
                    }
                    cluster_read_only_keys.push(key_bytes);
                }
                for key in resources.footprint.read_write.iter() {
                    let key_bytes = key_to_bytes(key);
                    if stage_read_only_keys.contains(&key_bytes)
                        || stage_read_write_keys.contains(&key_bytes)
                    {
                        debug!("Got bad txSet: cluster footprint conflicts with another cluster within stage");
                        return TxSetValidationResult::TxOrderingInvalid;
                    }
                    cluster_read_write_keys.push(key_bytes);
                }
            }
        }

        stage_read_only_keys.extend(cluster_read_only_keys);
        stage_read_write_keys.extend(cluster_read_write_keys);
    }
    TxSetValidationResult::Valid
}

/// Serialize a LedgerKey to bytes for use as a hash set key.
fn key_to_bytes(key: &stellar_xdr::LedgerKey) -> Vec<u8> {
    henyey_common::xdr_stream::xdr_to_bytes(key)
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

/// Extract the ed25519 public key bytes from a transaction envelope's source account.
///
/// For fee-bump transactions, uses the *inner* transaction source (matching stellar-core's
/// `getSourceID()` which returns the inner source for fee bumps).
fn source_account_ed25519(env: &TransactionEnvelope) -> [u8; 32] {
    match env {
        TransactionEnvelope::TxV0(e) => e.tx.source_account_ed25519.0,
        TransactionEnvelope::Tx(e) => henyey_tx::muxed_to_ed25519(&e.tx.source_account).0,
        TransactionEnvelope::TxFeeBump(e) => match &e.tx.inner_tx {
            stellar_xdr::FeeBumpTransactionInnerTx::Tx(inner) => {
                henyey_tx::muxed_to_ed25519(&inner.tx.source_account).0
            }
        },
    }
}

/// Orchestrate full TX set content validation.
///
/// Mirrors stellar-core's `ApplicableTxSetFrame::checkValidInternalWithResult()`
/// (TxSetFrame.cpp:2107-2187) and per-phase `TxSetPhaseFrame::checkValidWithResult()`
/// (TxSetFrame.cpp:1742-1799).
///
/// Performs:
/// 1. Verify previousLedgerHash matches the LCL hash
/// 2. Verify generalized vs legacy matches protocol version
/// 3. Verify no duplicate source accounts across ALL phases
/// 4. Per-phase: fee map validation, phase-type checks, phase-specific limits
/// 5. Per-TX content validation (time bounds, fees) via `get_invalid_tx_list_with_fee_map`
///
/// For Phase 1, `fee_balance_provider` may be `None` to skip per-account balance checks.
///
/// **Prefer [`PreparedTransactionSet::check_valid()`]** for production call sites.
/// This function is `pub(crate)` for unit testing only.
#[allow(clippy::too_many_arguments)]
pub(crate) fn check_tx_set_valid(
    gen_tx_set: &GeneralizedTransactionSet,
    lcl_header: &LedgerHeader,
    lcl_hash: &Hash256,
    close_time_offset: u64,
    network_id: NetworkId,
    soroban_info: Option<&SorobanNetworkInfo>,
    fee_balance_provider: Option<&dyn FeeBalanceProvider>,
    account_provider: Option<&dyn AccountProvider>,
    frozen_key_config: Option<&henyey_tx::frozen_keys::FrozenKeyConfig>,
) -> Result<(), TxSetValidationError> {
    let GeneralizedTransactionSet::V1(v1) = gen_tx_set;

    // Parity: stellar-core TxSetFrame.cpp:2115-2121
    // Check previousLedgerHash matches the LCL hash first.
    let previous_ledger_hash = Hash256::from_bytes(v1.previous_ledger_hash.0);
    if previous_ledger_hash != *lcl_hash {
        debug!(
            "Got bad txSet: previousLedgerHash {} != LCL {}",
            previous_ledger_hash, lcl_hash
        );
        return Err(TxSetValidationError::new(
            TxSetValidationResult::PreviousLedgerHashMismatch,
        ));
    }

    // Verify generalized tx set is expected for this protocol
    let need_generalized =
        protocol_version_starts_from(lcl_header.ledger_version, ProtocolVersion::V20);
    if !need_generalized {
        return Err(TxSetValidationError::new(
            TxSetValidationResult::GeneralizedTxsetMismatch,
        ));
    }

    // Generalized sets should always have 2 phases
    if v1.phases.len() != 2 {
        return Err(TxSetValidationError::new(
            TxSetValidationResult::WrongPhaseCount,
        ));
    }

    // Parity: stellar-core TxSetFrame.cpp:2149-2165
    // Ensure no duplicate source accounts across all phases.
    {
        let mut seen_sources: HashSet<[u8; 32]> = HashSet::new();
        for phase in v1.phases.iter() {
            for tx in collect_phase_txs(phase) {
                let source_key = source_account_ed25519(tx);
                if !seen_sources.insert(source_key) {
                    debug!("Got bad txSet: multiple txs per source account");
                    return Err(TxSetValidationError::new(
                        TxSetValidationResult::MultipleTxsPerSourceAccount,
                    ));
                }
            }
        }
    }

    // Cross-phase fee map handling (Protocol 26+)
    let use_cross_phase_fee_map =
        protocol_version_starts_from(lcl_header.ledger_version, ProtocolVersion::V26);

    // Build validation context for per-TX checks
    let ledger_flags = match &lcl_header.ext {
        stellar_xdr::LedgerHeaderExt::V1(ext) => ext.flags,
        _ => 0,
    };
    let mut ctx = TxSetValidationContext::new(
        lcl_header.ledger_seq,
        lcl_header.scp_value.close_time.0,
        lcl_header.base_fee,
        lcl_header.base_reserve,
        lcl_header.ledger_version,
        network_id,
        ledger_flags,
    );
    if let Some(info) = soroban_info {
        ctx.soroban_resource_limits = Some(henyey_tx::SorobanResourceLimits {
            tx_max_instructions: info.tx_max_instructions as u64,
            tx_max_read_bytes: info.tx_max_read_bytes as u64,
            tx_max_write_bytes: info.tx_max_write_bytes as u64,
            tx_max_read_ledger_entries: info.tx_max_read_ledger_entries as u64,
            tx_max_write_ledger_entries: info.tx_max_write_ledger_entries as u64,
            tx_max_size_bytes: info.tx_max_size_bytes as u64,
            tx_max_footprint_entries: info.tx_max_footprint_entries as u64,
            max_contract_size_bytes: info.max_contract_size,
            max_contract_data_key_size_bytes: info.max_contract_data_key_size,
        });
    }
    if let Some(fk) = frozen_key_config {
        ctx.frozen_key_config = fk.clone();
    }
    let close_time_bounds = CloseTimeBounds::with_offsets(close_time_offset, close_time_offset);

    let mut account_fee_map: HashMap<AccountId, i64> = HashMap::new();

    for (phase_idx, phase) in v1.phases.iter().enumerate() {
        if !use_cross_phase_fee_map {
            account_fee_map.clear();
        }

        // 1. Check fee map
        let fee_result = check_fee_map(phase, lcl_header.base_fee);
        if fee_result != TxSetValidationResult::Valid {
            return Err(TxSetValidationError::new(fee_result).with_phase(phase_idx));
        }

        let is_soroban = phase_idx == 1;

        // 2. Verify phase TX types
        let phase_txs = collect_phase_txs(phase);
        for tx in &phase_txs {
            if is_soroban_envelope(tx) != is_soroban {
                return Err(
                    TxSetValidationError::new(TxSetValidationResult::InvalidPhaseTxType)
                        .with_phase(phase_idx),
                );
            }
        }

        // 3. Phase-specific validation
        if is_soroban {
            if let Some(info) = soroban_info {
                let soroban_result = check_valid_soroban(phase, lcl_header, info);
                if soroban_result != TxSetValidationResult::Valid {
                    return Err(TxSetValidationError::new(soroban_result).with_phase(phase_idx));
                }
            } else {
                // Soroban phase present but no network config — reject.
                return Err(TxSetValidationError::new(
                    TxSetValidationResult::SorobanConfigUnavailable,
                )
                .with_phase(phase_idx));
            }
        } else {
            let classic_result = check_valid_classic(phase, lcl_header.max_tx_set_size);
            if classic_result != TxSetValidationResult::Valid {
                return Err(TxSetValidationError::new(classic_result).with_phase(phase_idx));
            }
        }

        // 4. Per-TX content validation (time bounds, fees, etc.)
        let tx_envelopes: Vec<TransactionEnvelope> =
            phase_txs.iter().map(|tx| (*tx).clone()).collect();
        let invalid = get_invalid_tx_list_with_fee_map(
            &tx_envelopes,
            &ctx,
            &close_time_bounds,
            fee_balance_provider,
            account_provider,
            &mut account_fee_map,
        );
        if !invalid.is_empty() {
            return Err(
                TxSetValidationError::new(TxSetValidationResult::TxValidationFailed)
                    .with_phase(phase_idx)
                    .with_invalid_tx_count(invalid.len()),
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tx_queue::FeeBalanceProvider;
    use henyey_common::NetworkId;
    use stellar_xdr::{
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
        fn get_available_balance(
            &self,
            account_id: &AccountId,
        ) -> henyey_ledger::Result<Option<i64>> {
            Ok(self.balances.get(account_id).copied())
        }
    }

    struct MockAccountProvider {
        accounts: HashMap<AccountId, AccountEntry>,
    }

    impl MockAccountProvider {
        fn new() -> Self {
            Self {
                accounts: HashMap::new(),
            }
        }

        /// Add a simple account with the given key bytes and sequence number.
        /// Master weight = 1, thresholds = [1, 1, 1] (low, med, high).
        fn add_account(&mut self, key_bytes: [u8; 32], seq_num: i64) {
            let account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(key_bytes)));
            let account = AccountEntry {
                account_id: account_id.clone(),
                balance: 10_000_000,
                seq_num: stellar_xdr::SequenceNumber(seq_num),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: stellar_xdr::String32::default(),
                thresholds: stellar_xdr::Thresholds([1, 1, 1, 1]), // master=1, low=1, med=1, high=1
                signers: stellar_xdr::VecM::default(),
                ext: stellar_xdr::AccountEntryExt::V0,
            };
            self.accounts.insert(account_id, account);
        }
    }

    impl AccountProvider for MockAccountProvider {
        fn load_account(
            &self,
            account_id: &AccountId,
        ) -> henyey_ledger::Result<Option<AccountEntry>> {
            Ok(self.accounts.get(account_id).cloned())
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
            min_seq_age: stellar_xdr::Duration(0),
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

        let invalid = get_invalid_tx_list(&txs, &ctx, &bounds, None, None);
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

        let invalid = get_invalid_tx_list(&txs, &ctx, &bounds, None, None);
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

        let invalid = get_invalid_tx_list(&txs, &ctx, &bounds, None, None);
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

        let invalid = get_invalid_tx_list(&[], &ctx, &bounds, None, None);
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

        let invalid = get_invalid_tx_list(&txs, &ctx, &bounds, None, None);
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

        let (valid, invalid) = trim_invalid(&txs, &ctx, &bounds, None, None);
        assert_eq!(valid.len(), 3, "all transactions should be valid");
        assert!(invalid.is_empty(), "no transactions should be invalid");
    }

    #[test]
    fn test_trim_invalid_all_invalid() {
        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();

        let txs = vec![make_low_fee_envelope(1), make_low_fee_envelope(2)];

        let (valid, invalid) = trim_invalid(&txs, &ctx, &bounds, None, None);
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

        let (valid, invalid) = trim_invalid(&txs, &ctx, &bounds, None, None);
        assert_eq!(valid.len(), 2, "should have 2 valid transactions");
        assert_eq!(invalid.len(), 2, "should have 2 invalid transactions");

        // Verify the valid transactions are the ones we expect (by hash)
        let valid_hashes: HashSet<Hash256> = valid.iter().map(|tx| Hash256::hash_xdr(tx)).collect();
        let expected_valid1 = Hash256::hash_xdr(&valid1);
        let expected_valid2 = Hash256::hash_xdr(&valid2);
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

        let (valid, invalid) = trim_invalid(&[], &ctx, &bounds, None, None);
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

        let (valid, _) = trim_invalid(&txs, &ctx, &bounds, None, None);
        assert_eq!(valid.len(), 3);

        // Verify order is preserved
        let hash1 = Hash256::hash_xdr(&tx1);
        let hash2 = Hash256::hash_xdr(&tx2);
        let hash3 = Hash256::hash_xdr(&tx3);

        assert_eq!(Hash256::hash_xdr(&valid[0]), hash1);
        assert_eq!(Hash256::hash_xdr(&valid[1]), hash2);
        assert_eq!(Hash256::hash_xdr(&valid[2]), hash3);
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

        let result_hashes: HashSet<Hash256> =
            result.iter().map(|tx| Hash256::hash_xdr(tx)).collect();
        assert!(result_hashes.contains(&Hash256::hash_xdr(&tx1)));
        assert!(!result_hashes.contains(&Hash256::hash_xdr(&tx2)));
        assert!(result_hashes.contains(&Hash256::hash_xdr(&tx3)));
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
        let invalid = get_invalid_tx_list(
            std::slice::from_ref(&envelope),
            &ctx,
            &bounds_exact,
            None,
            None,
        );
        assert!(
            invalid.is_empty(),
            "tx should be valid with exact close time"
        );

        // With upper offset of 10 (close_time + 10 = 1010 > max_time 1005), should be invalid
        let bounds_offset = CloseTimeBounds::with_offsets(0, 10);
        let invalid = get_invalid_tx_list(
            std::slice::from_ref(&envelope),
            &ctx,
            &bounds_offset,
            None,
            None,
        );
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

        let invalid = get_invalid_tx_list(&[tx], &ctx, &bounds, Some(&provider), None);
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

        let invalid = get_invalid_tx_list(&[tx], &ctx, &bounds, Some(&provider), None);
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

        let invalid = get_invalid_tx_list(&[tx1, tx2], &ctx, &bounds, Some(&provider), None);
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

        let invalid = get_invalid_tx_list(&[tx1, tx2], &ctx, &bounds, Some(&provider), None);
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

        let invalid =
            get_invalid_tx_list(&[tx_a1, tx_a2, tx_b], &ctx, &bounds, Some(&provider), None);
        assert_eq!(invalid.len(), 1, "only source B's tx should be invalid");
    }

    #[test]
    fn test_fee_source_affordability_unknown_account() {
        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();

        // Source [10u8; 32] not in provider (returns None -> treated as 0 balance)
        let tx = make_envelope_with_source([10u8; 32], 200, 1);

        let provider = MockFeeBalanceProvider::new(); // empty

        let invalid = get_invalid_tx_list(&[tx], &ctx, &bounds, Some(&provider), None);
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

        let invalid = get_invalid_tx_list(&[tx1, tx2], &ctx, &bounds, Some(&provider), None);
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

        let invalid = get_invalid_tx_list(&[tx], &ctx, &bounds, None, None);
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
            None,
        );
        assert_eq!(valid.len(), 1, "one tx should be valid");
        assert_eq!(invalid.len(), 1, "one tx should be invalid");
    }

    // --- AUDIT-033: check_fee_map tests ---

    fn make_v0_phase_with_fee(
        txs: Vec<TransactionEnvelope>,
        base_fee: Option<i64>,
    ) -> TransactionPhase {
        use stellar_xdr::{TxSetComponent, TxSetComponentTxsMaybeDiscountedFee};
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
        assert_eq!(check_fee_map(&phase, 100), TxSetValidationResult::Valid);
    }

    #[test]
    fn test_check_fee_map_component_base_fee_too_low() {
        let tx = make_valid_envelope(200, 1);
        // base_fee=50 < lcl_base_fee=100
        let phase = make_v0_phase_with_fee(vec![tx], Some(50));
        assert_eq!(
            check_fee_map(&phase, 100),
            TxSetValidationResult::ComponentBaseFeeTooLow
        );
    }

    #[test]
    fn test_check_fee_map_negative_base_fee_rejected() {
        // Defense-in-depth: negative base_fee is also rejected during
        // deserialization in stellar-core. This test verifies check_fee_map
        // catches it as an extra safety layer.
        let tx = make_valid_envelope(200, 1);
        let phase = make_v0_phase_with_fee(vec![tx], Some(-1));
        assert_eq!(
            check_fee_map(&phase, 100),
            TxSetValidationResult::ComponentBaseFeeTooLow
        );
    }

    #[test]
    fn test_check_fee_map_negative_parallel_base_fee_rejected() {
        use stellar_xdr::ParallelTxsComponent;
        // Defense-in-depth: negative base_fee is also rejected during
        // deserialization in stellar-core.
        let phase = TransactionPhase::V1(ParallelTxsComponent {
            base_fee: Some(-1),
            execution_stages: vec![].try_into().unwrap(),
        });
        assert_eq!(
            check_fee_map(&phase, 100),
            TxSetValidationResult::ComponentBaseFeeTooLow
        );
    }

    #[test]
    fn test_check_fee_map_tx_fee_bid_too_low() {
        // TX with fee=50, 1 op. base_fee=100.
        // inclusion_fee = min(50, 1*100) = 50
        // min_inclusion_fee = max(100, 100) * 1 = 100
        // 50 < 100 -> invalid
        let tx = make_valid_envelope(50, 1);
        let phase = make_v0_phase_with_fee(vec![tx], Some(100));
        assert_eq!(
            check_fee_map(&phase, 100),
            TxSetValidationResult::TxFeeBidTooLow
        );
    }

    #[test]
    fn test_check_fee_map_no_base_fee_valid() {
        // No component base_fee (None). stellar-core skips all fee-map
        // validation for None-baseFee components (TxSetFrame.cpp:726-728).
        // This tx happens to have a high enough fee, but the point is that
        // fee checks are skipped entirely when base_fee is None.
        let tx = make_valid_envelope(200, 1);
        let phase = make_v0_phase_with_fee(vec![tx], None);
        assert_eq!(check_fee_map(&phase, 100), TxSetValidationResult::Valid);
    }

    // --- AUDIT-268: None-baseFee parity regression tests ---

    #[test]
    fn test_check_fee_map_none_base_fee_low_tx_fee_skipped() {
        // AUDIT-268: When component base_fee is None, stellar-core skips
        // all fee validation (TxSetFrame.cpp:726-728). Previously henyey
        // fell back to lcl_base_fee and rejected this tx.
        // TX fee=50, 1 op, lcl_base_fee=100 → would fail if checked
        // (50 < 100*1), but must pass because fee checks are skipped.
        let tx = make_valid_envelope(50, 1);
        let phase = make_v0_phase_with_fee(vec![tx], None);
        assert_eq!(check_fee_map(&phase, 100), TxSetValidationResult::Valid);
    }

    #[test]
    fn test_check_fee_map_v1_none_base_fee_low_tx_fee_skipped() {
        use stellar_xdr::ParallelTxsComponent;
        // AUDIT-268: V1/parallel path — same as above for V1 arm.
        // base_fee=None, tx fee=50 < lcl_base_fee=100 → must pass.
        let tx = make_valid_envelope(50, 1);
        let phase = TransactionPhase::V1(ParallelTxsComponent {
            base_fee: None,
            execution_stages: vec![vec![vec![tx].try_into().unwrap()].try_into().unwrap()]
                .try_into()
                .unwrap(),
        });
        assert_eq!(check_fee_map(&phase, 100), TxSetValidationResult::Valid);
    }

    #[test]
    fn test_check_fee_map_component_base_fee_higher_than_lcl() {
        // When component base_fee (200) > lcl_base_fee (100), the effective
        // base fee is max(100, 200) = 200. TX fee=150 with 1 op has
        // inclusion_fee = min(150, 1*200) = 150 < 200 → invalid.
        let tx = make_valid_envelope(150, 1);
        let phase = make_v0_phase_with_fee(vec![tx], Some(200));
        assert_eq!(
            check_fee_map(&phase, 100),
            TxSetValidationResult::TxFeeBidTooLow
        );
    }

    #[test]
    fn test_check_fee_map_v1_some_base_fee_tx_too_low() {
        use stellar_xdr::ParallelTxsComponent;
        // V1/parallel with base_fee=Some(100), lcl_base_fee=100.
        // TX fee=50, 1 op → inclusion_fee = 50 < 100 → invalid.
        let tx = make_valid_envelope(50, 1);
        let phase = TransactionPhase::V1(ParallelTxsComponent {
            base_fee: Some(100),
            execution_stages: vec![vec![vec![tx].try_into().unwrap()].try_into().unwrap()]
                .try_into()
                .unwrap(),
        });
        assert_eq!(
            check_fee_map(&phase, 100),
            TxSetValidationResult::TxFeeBidTooLow
        );
    }

    #[test]
    fn test_check_fee_map_component_base_fee_too_low_takes_precedence() {
        // When both base_fee < lcl_base_fee AND tx fee bid would be too low,
        // ComponentBaseFeeTooLow is returned first (matches stellar-core traversal order).
        let tx = make_valid_envelope(50, 1); // fee=50, would fail tx bid check too
        let phase = make_v0_phase_with_fee(vec![tx], Some(50)); // base_fee=50 < lcl=100
        assert_eq!(
            check_fee_map(&phase, 100),
            TxSetValidationResult::ComponentBaseFeeTooLow
        );
    }

    #[test]
    fn test_check_fee_map_display_impl() {
        assert_eq!(TxSetValidationResult::Valid.to_string(), "VALID");
        assert_eq!(
            TxSetValidationResult::ComponentBaseFeeTooLow.to_string(),
            "COMPONENT_BASE_FEE_TOO_LOW"
        );
        assert_eq!(
            TxSetValidationResult::TxFeeBidTooLow.to_string(),
            "TX_FEE_BID_TOO_LOW"
        );
    }

    // --- TxSetValidationResult and TxSetValidationError tests ---

    #[test]
    fn test_tx_set_validation_result_display_all_variants() {
        // Verify SCREAMING_SNAKE_CASE Display for all variants
        assert_eq!(TxSetValidationResult::Valid.to_string(), "VALID");
        assert_eq!(
            TxSetValidationResult::GeneralizedTxsetMismatch.to_string(),
            "GENERALIZED_TXSET_MISMATCH"
        );
        assert_eq!(
            TxSetValidationResult::WrongPhaseCount.to_string(),
            "WRONG_PHASE_COUNT"
        );
        assert_eq!(
            TxSetValidationResult::SorobanParallelSupportMismatch.to_string(),
            "SOROBAN_PARALLEL_SUPPORT_MISMATCH"
        );
        assert_eq!(
            TxSetValidationResult::SorobanResourcesOverflow.to_string(),
            "SOROBAN_RESOURCES_OVERFLOW"
        );
        assert_eq!(
            TxSetValidationResult::SorobanResourcesExceedLimit.to_string(),
            "SOROBAN_RESOURCES_EXCEED_LIMIT"
        );
        assert_eq!(
            TxSetValidationResult::TooManySorobanClusters.to_string(),
            "TOO_MANY_SOROBAN_CLUSTERS"
        );
        assert_eq!(
            TxSetValidationResult::SorobanInstructionsOverflow.to_string(),
            "SOROBAN_INSTRUCTIONS_OVERFLOW"
        );
        assert_eq!(
            TxSetValidationResult::SorobanInstructionsExceedLimit.to_string(),
            "SOROBAN_INSTRUCTIONS_EXCEED_LIMIT"
        );
        assert_eq!(
            TxSetValidationResult::SorobanSequentialInstructionsOverflow.to_string(),
            "SOROBAN_SEQUENTIAL_INSTRUCTIONS_OVERFLOW"
        );
        assert_eq!(
            TxSetValidationResult::SorobanConfigUnavailable.to_string(),
            "SOROBAN_CONFIG_UNAVAILABLE"
        );
        assert_eq!(
            TxSetValidationResult::TxOrderingInvalid.to_string(),
            "TX_ORDERING_INVALID"
        );
        assert_eq!(
            TxSetValidationResult::TxValidationFailed.to_string(),
            "TX_VALIDATION_FAILED"
        );
    }

    #[test]
    fn test_tx_set_validation_error_display_without_phase() {
        let err = TxSetValidationError::new(TxSetValidationResult::GeneralizedTxsetMismatch);
        assert_eq!(err.to_string(), "GENERALIZED_TXSET_MISMATCH");
        assert_eq!(err.phase_idx, None);
        assert_eq!(err.invalid_tx_count, None);
    }

    #[test]
    fn test_tx_set_validation_error_display_with_phase() {
        let err =
            TxSetValidationError::new(TxSetValidationResult::ComponentBaseFeeTooLow).with_phase(0);
        assert_eq!(err.to_string(), "phase 0: COMPONENT_BASE_FEE_TOO_LOW");
        assert_eq!(err.phase_idx, Some(0));
    }

    #[test]
    fn test_tx_set_validation_error_display_with_invalid_tx_count() {
        let err = TxSetValidationError::new(TxSetValidationResult::TxValidationFailed)
            .with_phase(1)
            .with_invalid_tx_count(3);
        assert_eq!(
            err.to_string(),
            "phase 1: TX_VALIDATION_FAILED (3 invalid transactions)"
        );
        assert_eq!(err.phase_idx, Some(1));
        assert_eq!(err.invalid_tx_count, Some(3));
    }

    #[test]
    fn test_check_tx_set_valid_fee_error_populates_phase_idx() {
        let tx = make_valid_envelope(200, 1);
        let phase_with_low_base_fee = make_v0_phase_with_fee(vec![tx], Some(50));
        let soroban_phase = make_v0_phase_with_fee(vec![], Some(100));

        use stellar_xdr::{Hash, TransactionSetV1};
        let gen_tx_set = GeneralizedTransactionSet::V1(TransactionSetV1 {
            previous_ledger_hash: Hash([0u8; 32]),
            phases: vec![phase_with_low_base_fee, soroban_phase]
                .try_into()
                .unwrap(),
        });
        let header = make_soroban_lcl_header(25);
        let network_id = NetworkId::testnet();

        let err = check_tx_set_valid(
            &gen_tx_set,
            &header,
            &Hash256::ZERO,
            0,
            network_id,
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(err.result, TxSetValidationResult::ComponentBaseFeeTooLow);
        assert_eq!(err.phase_idx, Some(0));
        assert_eq!(err.invalid_tx_count, None);
    }

    #[test]
    fn test_check_tx_set_valid_per_tx_error_populates_invalid_tx_count() {
        let bad_seq_tx = make_valid_envelope(200, 999_999_999);
        let gen_tx_set = make_gen_tx_set(vec![bad_seq_tx], vec![]);
        let header = make_soroban_lcl_header(25);
        let network_id = NetworkId::testnet();

        let mut fee_provider = MockFeeBalanceProvider::new();
        fee_provider.set_balance([0u8; 32], 1_000_000);
        let mut account_provider = MockAccountProvider::new();
        account_provider.add_account([0u8; 32], 0);

        let err = check_tx_set_valid(
            &gen_tx_set,
            &header,
            &Hash256::ZERO,
            0,
            network_id,
            None,
            Some(&fee_provider),
            Some(&account_provider),
            None,
        )
        .unwrap_err();
        assert_eq!(err.result, TxSetValidationResult::TxValidationFailed);
        assert_eq!(err.phase_idx, Some(0));
        assert!(
            err.invalid_tx_count.is_some() && err.invalid_tx_count.unwrap() > 0,
            "invalid_tx_count should be populated for per-TX failures"
        );
    }

    #[test]
    fn test_check_tx_set_valid_soroban_config_unavailable_error() {
        let soroban_tx = make_soroban_envelope(100, 100, 100, vec![], vec![]);
        let gen_tx_set = make_gen_tx_set(vec![], vec![soroban_tx]);
        let header = make_soroban_lcl_header(25);
        let network_id = NetworkId::testnet();

        let err = check_tx_set_valid(
            &gen_tx_set,
            &header,
            &Hash256::ZERO,
            0,
            network_id,
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(err.result, TxSetValidationResult::SorobanConfigUnavailable);
        assert_eq!(err.phase_idx, Some(1));
    }

    #[test]
    fn test_check_tx_set_valid_wrong_phase_count() {
        use stellar_xdr::{Hash, TransactionSetV1};
        // Create a tx set with only 1 phase (should be 2)
        let phase = TransactionPhase::V0(vec![].try_into().unwrap());
        let gen_tx_set = GeneralizedTransactionSet::V1(TransactionSetV1 {
            previous_ledger_hash: Hash([0u8; 32]),
            phases: vec![phase].try_into().unwrap(),
        });
        let header = make_soroban_lcl_header(25);
        let network_id = NetworkId::testnet();

        let err = check_tx_set_valid(
            &gen_tx_set,
            &header,
            &Hash256::ZERO,
            0,
            network_id,
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(err.result, TxSetValidationResult::WrongPhaseCount);
        assert_eq!(err.phase_idx, None);
    }

    #[test]
    fn test_check_tx_set_valid_generalized_txset_mismatch() {
        use stellar_xdr::{Hash, TransactionSetV1};
        let gen_tx_set = GeneralizedTransactionSet::V1(TransactionSetV1 {
            previous_ledger_hash: Hash([0u8; 32]),
            phases: vec![
                TransactionPhase::V0(vec![].try_into().unwrap()),
                TransactionPhase::V0(vec![].try_into().unwrap()),
            ]
            .try_into()
            .unwrap(),
        });
        // Protocol 19 — pre-V20, generalized not expected
        let mut header = make_soroban_lcl_header(19);
        header.ledger_version = 19;
        let network_id = NetworkId::testnet();

        let err = check_tx_set_valid(
            &gen_tx_set,
            &header,
            &Hash256::ZERO,
            0,
            network_id,
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(err.result, TxSetValidationResult::GeneralizedTxsetMismatch);
        assert_eq!(err.phase_idx, None);
    }

    /// Parity: stellar-core TxSetFrame.cpp:2115-2121.
    /// check_tx_set_valid must reject tx sets whose previousLedgerHash
    /// does not match the LCL hash.
    #[test]
    fn test_check_tx_set_valid_previous_ledger_hash_mismatch() {
        use stellar_xdr::{Hash, TransactionSetV1};

        let gen_tx_set = GeneralizedTransactionSet::V1(TransactionSetV1 {
            previous_ledger_hash: Hash([0u8; 32]),
            phases: vec![
                TransactionPhase::V0(vec![].try_into().unwrap()),
                TransactionPhase::V0(vec![].try_into().unwrap()),
            ]
            .try_into()
            .unwrap(),
        });
        let header = make_soroban_lcl_header(25);
        let network_id = NetworkId::testnet();
        let wrong_lcl = Hash256::from_bytes([1u8; 32]);

        let err = check_tx_set_valid(
            &gen_tx_set,
            &header,
            &wrong_lcl,
            0,
            network_id,
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(
            err.result,
            TxSetValidationResult::PreviousLedgerHashMismatch
        );
    }

    /// Parity: stellar-core TxSetFrame.cpp:2149-2165.
    /// check_tx_set_valid must reject tx sets that have duplicate source
    /// accounts across phases.
    #[test]
    fn test_check_tx_set_valid_duplicate_source_account() {
        // Two txs with the same source account ([0u8; 32]) — one in each phase.
        let classic_tx = make_valid_envelope(200, 1);
        let soroban_tx = make_valid_envelope(200, 2); // same source
        let gen_tx_set = make_gen_tx_set(vec![classic_tx], vec![soroban_tx]);
        let header = make_soroban_lcl_header(25);
        let network_id = NetworkId::testnet();

        let err = check_tx_set_valid(
            &gen_tx_set,
            &header,
            &Hash256::ZERO,
            0,
            network_id,
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(
            err.result,
            TxSetValidationResult::MultipleTxsPerSourceAccount
        );
    }

    /// Control: distinct source accounts across phases must pass.
    #[test]
    fn test_check_tx_set_valid_distinct_sources_passes() {
        // classic_tx source = [0u8; 32], a different source for soroban
        let classic_tx = make_valid_envelope(200, 1);

        // Build soroban tx with different source [5u8; 32]
        let different_source = MuxedAccount::Ed25519(Uint256([5u8; 32]));
        let op = Operation {
            source_account: None,
            body: OperationBody::Payment(PaymentOp {
                destination: MuxedAccount::Ed25519(Uint256([6u8; 32])),
                asset: Asset::Native,
                amount: 1000,
            }),
        };
        let tx = Transaction {
            source_account: different_source,
            fee: 200,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![op].try_into().unwrap(),
            ext: TransactionExt::V0,
        };
        let different_source_tx = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        });

        let gen_tx_set = make_gen_tx_set(vec![classic_tx], vec![different_source_tx]);
        let header = make_soroban_lcl_header(25);
        let network_id = NetworkId::testnet();
        let soroban_info = make_soroban_network_info();

        // Should not fail on duplicate source — may fail on other checks (sequence/sig)
        // but should NOT be MultipleTxsPerSourceAccount
        let result = check_tx_set_valid(
            &gen_tx_set,
            &header,
            &Hash256::ZERO,
            0,
            network_id,
            Some(&soroban_info),
            None,
            None,
            None,
        );
        // If it errors, it must not be MultipleTxsPerSourceAccount
        if let Err(ref e) = result {
            assert_ne!(
                e.result,
                TxSetValidationResult::MultipleTxsPerSourceAccount,
                "distinct sources should not trigger MultipleTxsPerSourceAccount"
            );
        }
    }

    // --- AUDIT-033: check_valid_classic tests ---

    #[test]
    fn test_classic_validation_result_display() {
        assert_eq!(TxSetValidationResult::Valid.to_string(), "VALID");
        assert_eq!(
            TxSetValidationResult::ClassicPhaseParallelNotAllowed.to_string(),
            "CLASSIC_PHASE_PARALLEL_NOT_ALLOWED"
        );
        assert_eq!(
            TxSetValidationResult::TooManyClassicTxs.to_string(),
            "TOO_MANY_CLASSIC_TXS"
        );
        assert_eq!(
            TxSetValidationResult::InvalidPhaseTxType.to_string(),
            "INVALID_PHASE_TX_TYPE"
        );
    }

    #[test]
    fn test_check_valid_classic_within_limit() {
        // 2 TXs with 1 op each, limit = 5
        let tx1 = make_valid_envelope(100, 1);
        let tx2 = make_valid_envelope(200, 2);
        let phase = make_v0_phase_with_fee(vec![tx1, tx2], Some(100));
        assert_eq!(check_valid_classic(&phase, 5), TxSetValidationResult::Valid);
    }

    #[test]
    fn test_check_valid_classic_over_limit() {
        // 3 TXs with 1 op each, limit = 2
        let tx1 = make_valid_envelope(100, 1);
        let tx2 = make_valid_envelope(200, 2);
        let tx3 = make_valid_envelope(300, 3);
        let phase = make_v0_phase_with_fee(vec![tx1, tx2, tx3], Some(100));
        assert_eq!(
            check_valid_classic(&phase, 2),
            TxSetValidationResult::TooManyClassicTxs
        );
    }

    #[test]
    fn test_check_valid_classic_rejects_parallel_phase() {
        use stellar_xdr::ParallelTxsComponent;
        // Intentionally invalid: parallel phase where classic is expected
        let phase = TransactionPhase::V1(ParallelTxsComponent {
            base_fee: Some(100),
            execution_stages: vec![].try_into().unwrap(),
        });
        assert_eq!(
            check_valid_classic(&phase, 100),
            TxSetValidationResult::ClassicPhaseParallelNotAllowed
        );
    }

    // --- AUDIT-033: check_valid_soroban tests ---

    fn make_soroban_envelope(
        instructions: u32,
        read_bytes: u32,
        write_bytes: u32,
        read_only_keys: Vec<stellar_xdr::LedgerKey>,
        read_write_keys: Vec<stellar_xdr::LedgerKey>,
    ) -> TransactionEnvelope {
        use stellar_xdr::{
            InvokeHostFunctionOp, LedgerFootprint, SorobanResources, SorobanTransactionData,
            SorobanTransactionDataExt,
        };

        let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
        let op = Operation {
            source_account: None,
            body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                host_function: stellar_xdr::HostFunction::InvokeContract(
                    stellar_xdr::InvokeContractArgs {
                        contract_address: stellar_xdr::ScAddress::Contract(
                            stellar_xdr::ContractId(stellar_xdr::Hash([0u8; 32])),
                        ),
                        function_name: stellar_xdr::ScSymbol("test".try_into().unwrap()),
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
            ext: stellar_xdr::TransactionExt::V1(soroban_data),
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
            ledger_max_tx_size_bytes: 1_000_000, // 1 MB
            ledger_max_tx_count: 100,
            ..Default::default()
        }
    }

    fn make_soroban_lcl_header(protocol: u32) -> LedgerHeader {
        use stellar_xdr::{Hash, LedgerHeaderExt, StellarValue, StellarValueExt, TimePoint};
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
        assert_eq!(
            check_valid_soroban(&phase, &header, &info),
            TxSetValidationResult::Valid
        );
    }

    #[test]
    fn test_check_valid_soroban_instructions_exceed_limit() {
        let info = make_soroban_network_info();
        let header = make_soroban_lcl_header(22);
        // Instructions exceed ledger max (1,000,000)
        let tx = make_soroban_envelope(2_000_000, 1000, 500, vec![], vec![]);
        let phase = make_v0_phase_with_fee(vec![tx], Some(100));
        assert_eq!(
            check_valid_soroban(&phase, &header, &info),
            TxSetValidationResult::SorobanResourcesExceedLimit
        );
    }

    #[test]
    fn test_check_valid_soroban_parallel_mismatch_protocol_22() {
        use stellar_xdr::ParallelTxsComponent;
        let info = make_soroban_network_info();
        let header = make_soroban_lcl_header(22);
        // Intentionally invalid: parallel phase on protocol 22 (pre-parallel)
        let phase = TransactionPhase::V1(ParallelTxsComponent {
            base_fee: Some(100),
            execution_stages: vec![].try_into().unwrap(),
        });
        assert_eq!(
            check_valid_soroban(&phase, &header, &info),
            TxSetValidationResult::SorobanParallelSupportMismatch
        );
    }

    #[test]
    fn test_check_valid_soroban_sequential_mismatch_protocol_23() {
        let info = make_soroban_network_info();
        let header = make_soroban_lcl_header(23);
        // Protocol 23 requires parallel V1 phase, but we provide V0
        let tx = make_soroban_envelope(100_000, 1000, 500, vec![], vec![]);
        let phase = make_v0_phase_with_fee(vec![tx], Some(100));
        assert_eq!(
            check_valid_soroban(&phase, &header, &info),
            TxSetValidationResult::SorobanParallelSupportMismatch
        );
    }

    #[test]
    fn test_check_valid_soroban_parallel_too_many_clusters() {
        use stellar_xdr::{DependentTxCluster, ParallelTxExecutionStage};
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

        let phase = henyey_tx::tx_set_xdr::soroban_phase_with_stages(
            Some(100),
            vec![stage].try_into().unwrap(),
        );
        assert_eq!(
            check_valid_soroban(&phase, &header, &info),
            TxSetValidationResult::TooManySorobanClusters
        );
    }

    #[test]
    fn test_check_valid_soroban_parallel_sequential_instruction_limit() {
        use stellar_xdr::{DependentTxCluster, ParallelTxExecutionStage};
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

        let phase = henyey_tx::tx_set_xdr::soroban_phase_with_stages(
            Some(100),
            vec![stage1, stage2].try_into().unwrap(),
        );
        assert_eq!(
            check_valid_soroban(&phase, &header, &info),
            TxSetValidationResult::SorobanInstructionsExceedLimit
        );
    }

    #[test]
    fn test_check_valid_soroban_parallel_rw_conflict() {
        use stellar_xdr::{
            DependentTxCluster, LedgerKey, LedgerKeyAccount, ParallelTxExecutionStage,
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

        let phase = henyey_tx::tx_set_xdr::soroban_phase_with_stages(
            Some(100),
            vec![stage].try_into().unwrap(),
        );
        assert_eq!(
            check_valid_soroban(&phase, &header, &info),
            TxSetValidationResult::TxOrderingInvalid
        );
    }

    #[test]
    fn test_check_valid_soroban_parallel_no_conflict_different_keys() {
        use stellar_xdr::{
            DependentTxCluster, LedgerKey, LedgerKeyAccount, ParallelTxExecutionStage,
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

        let phase = henyey_tx::tx_set_xdr::soroban_phase_with_stages(
            Some(100),
            vec![stage].try_into().unwrap(),
        );
        assert_eq!(
            check_valid_soroban(&phase, &header, &info),
            TxSetValidationResult::Valid
        );
    }

    #[test]
    fn test_check_valid_soroban_rejects_classic_tx_in_soroban_phase() {
        let info = make_soroban_network_info();
        let header = make_soroban_lcl_header(22);
        // Classic TX in Soroban phase
        let tx = make_valid_envelope(100, 1);
        let phase = make_v0_phase_with_fee(vec![tx], Some(100));
        assert_eq!(
            check_valid_soroban(&phase, &header, &info),
            TxSetValidationResult::InvalidPhaseTxType
        );
    }

    #[test]
    fn test_check_valid_classic_rejects_soroban_tx() {
        let tx = make_soroban_envelope(100, 100, 100, vec![], vec![]);
        let phase = make_v0_phase_with_fee(vec![tx], Some(100));
        assert_eq!(
            check_valid_classic(&phase, 100),
            TxSetValidationResult::InvalidPhaseTxType
        );
    }

    // --- AUDIT-153: TX_SIZE_BYTES and OPERATIONS resource limit tests ---

    #[test]
    fn test_check_valid_soroban_tx_size_bytes_exceed_limit() {
        let tx = make_soroban_envelope(100, 100, 100, vec![], vec![]);
        let tx_size = henyey_common::xdr_stream::xdr_encoded_len(&tx) as u32;
        // Set limit to less than one envelope's size
        let mut info = make_soroban_network_info();
        info.ledger_max_tx_size_bytes = tx_size - 1;
        let header = make_soroban_lcl_header(22);
        let phase = make_v0_phase_with_fee(vec![tx], Some(100));
        assert_eq!(
            check_valid_soroban(&phase, &header, &info),
            TxSetValidationResult::SorobanResourcesExceedLimit
        );
    }

    #[test]
    fn test_check_valid_soroban_tx_size_at_boundary() {
        let tx = make_soroban_envelope(100, 100, 100, vec![], vec![]);
        let tx_size = henyey_common::xdr_stream::xdr_encoded_len(&tx) as u32;
        let header = make_soroban_lcl_header(22);

        // Exactly at limit: should pass
        let mut info = make_soroban_network_info();
        info.ledger_max_tx_size_bytes = tx_size;
        let phase = make_v0_phase_with_fee(vec![tx.clone()], Some(100));
        assert_eq!(
            check_valid_soroban(&phase, &header, &info),
            TxSetValidationResult::Valid
        );

        // Two TXs exceed the single-TX limit
        let phase = make_v0_phase_with_fee(vec![tx.clone(), tx], Some(100));
        assert_eq!(
            check_valid_soroban(&phase, &header, &info),
            TxSetValidationResult::SorobanResourcesExceedLimit
        );
    }

    #[test]
    fn test_check_valid_soroban_ops_exceed_limit() {
        let mut info = make_soroban_network_info();
        info.ledger_max_tx_count = 2;
        let header = make_soroban_lcl_header(22);
        // 3 TXs, each with 1 op → total_ops = 3 > limit of 2
        let txs: Vec<_> = (0..3)
            .map(|_| make_soroban_envelope(100, 100, 100, vec![], vec![]))
            .collect();
        let phase = make_v0_phase_with_fee(txs, Some(100));
        assert_eq!(
            check_valid_soroban(&phase, &header, &info),
            TxSetValidationResult::SorobanResourcesExceedLimit
        );
    }

    #[test]
    fn test_check_valid_soroban_ops_at_boundary() {
        let mut info = make_soroban_network_info();
        info.ledger_max_tx_count = 2;
        let header = make_soroban_lcl_header(22);
        // 2 TXs exactly at limit → should pass
        let txs: Vec<_> = (0..2)
            .map(|_| make_soroban_envelope(100, 100, 100, vec![], vec![]))
            .collect();
        let phase = make_v0_phase_with_fee(txs, Some(100));
        assert_eq!(
            check_valid_soroban(&phase, &header, &info),
            TxSetValidationResult::Valid
        );
    }

    #[test]
    fn test_check_valid_soroban_fee_bump_tx_size() {
        // Fee-bump Soroban should use inner envelope size, not outer
        let inner = make_soroban_envelope(100, 100, 100, vec![], vec![]);
        let inner_size = henyey_common::xdr_stream::xdr_encoded_len(&inner) as u32;
        let fee_bump = make_fee_bump_envelope(inner, 50000);
        let outer_size = henyey_common::xdr_stream::xdr_encoded_len(&fee_bump) as u32;
        // Verify the inner and outer sizes are different
        assert!(outer_size > inner_size);

        let header = make_soroban_lcl_header(22);

        // Set limit between inner and outer size: should pass (uses inner size)
        let mut info = make_soroban_network_info();
        info.ledger_max_tx_size_bytes = inner_size;
        let phase = make_v0_phase_with_fee(vec![fee_bump.clone()], Some(100));
        assert_eq!(
            check_valid_soroban(&phase, &header, &info),
            TxSetValidationResult::Valid
        );

        // Set limit below inner size: should fail
        info.ledger_max_tx_size_bytes = inner_size - 1;
        let phase = make_v0_phase_with_fee(vec![fee_bump], Some(100));
        assert_eq!(
            check_valid_soroban(&phase, &header, &info),
            TxSetValidationResult::SorobanResourcesExceedLimit
        );
    }

    #[test]
    fn test_check_valid_soroban_fee_bump_ops_count() {
        // Fee-bump Soroban: ops = inner_ops + 1 = 1 + 1 = 2
        let inner = make_soroban_envelope(100, 100, 100, vec![], vec![]);
        let fee_bump = make_fee_bump_envelope(inner, 50000);
        let header = make_soroban_lcl_header(22);

        // Limit of 2: 1 fee-bump tx with 2 ops should pass
        let mut info = make_soroban_network_info();
        info.ledger_max_tx_count = 2;
        let phase = make_v0_phase_with_fee(vec![fee_bump.clone()], Some(100));
        assert_eq!(
            check_valid_soroban(&phase, &header, &info),
            TxSetValidationResult::Valid
        );

        // Limit of 1: 1 fee-bump tx with 2 ops should fail
        info.ledger_max_tx_count = 1;
        let phase = make_v0_phase_with_fee(vec![fee_bump], Some(100));
        assert_eq!(
            check_valid_soroban(&phase, &header, &info),
            TxSetValidationResult::SorobanResourcesExceedLimit
        );
    }

    #[test]
    fn test_soroban_validation_result_display() {
        assert_eq!(TxSetValidationResult::Valid.to_string(), "VALID");
        assert_eq!(
            TxSetValidationResult::SorobanParallelSupportMismatch.to_string(),
            "SOROBAN_PARALLEL_SUPPORT_MISMATCH"
        );
        assert_eq!(
            TxSetValidationResult::InvalidPhaseTxType.to_string(),
            "INVALID_PHASE_TX_TYPE"
        );
        assert_eq!(
            TxSetValidationResult::SorobanResourcesOverflow.to_string(),
            "SOROBAN_RESOURCES_OVERFLOW"
        );
        assert_eq!(
            TxSetValidationResult::SorobanResourcesExceedLimit.to_string(),
            "SOROBAN_RESOURCES_EXCEED_LIMIT"
        );
        assert_eq!(
            TxSetValidationResult::TooManySorobanClusters.to_string(),
            "TOO_MANY_SOROBAN_CLUSTERS"
        );
        assert_eq!(
            TxSetValidationResult::SorobanSequentialInstructionsOverflow.to_string(),
            "SOROBAN_SEQUENTIAL_INSTRUCTIONS_OVERFLOW"
        );
        assert_eq!(
            TxSetValidationResult::SorobanInstructionsOverflow.to_string(),
            "SOROBAN_INSTRUCTIONS_OVERFLOW"
        );
        assert_eq!(
            TxSetValidationResult::SorobanInstructionsExceedLimit.to_string(),
            "SOROBAN_INSTRUCTIONS_EXCEED_LIMIT"
        );
        assert_eq!(
            TxSetValidationResult::TxOrderingInvalid.to_string(),
            "TX_ORDERING_INVALID"
        );
    }

    // Note: End-to-end overflow triggering through check_valid_soroban remains
    // infeasible via standard XDR inputs (individual resource fields are u32,
    // so overflowing i64 sums requires ~2^31 transactions). The overflow
    // detection logic is tested at the helper level below.

    // --- Overflow helper tests: accumulate_resources ---

    #[test]
    fn test_accumulate_resources_normal() {
        use henyey_common::resource::{Resource, ResourceType};
        let mut a = Resource::make_empty_soroban();
        a.set_val(ResourceType::Instructions, 100);
        a.set_val(ResourceType::DiskReadBytes, 200);

        let mut b = Resource::make_empty_soroban();
        b.set_val(ResourceType::Instructions, 50);
        b.set_val(ResourceType::DiskReadBytes, 75);

        let result = accumulate_resources(a, &b).unwrap();
        assert_eq!(result.get_val(ResourceType::Instructions), 150);
        assert_eq!(result.get_val(ResourceType::DiskReadBytes), 275);
    }

    #[test]
    fn test_accumulate_resources_overflow() {
        use henyey_common::resource::{Resource, ResourceType};
        let mut a = Resource::make_empty_soroban();
        a.set_val(ResourceType::Instructions, i64::MAX);

        let mut b = Resource::make_empty_soroban();
        b.set_val(ResourceType::Instructions, 1);

        let result = accumulate_resources(a, &b);
        assert_eq!(
            result.unwrap_err(),
            TxSetValidationResult::SorobanResourcesOverflow
        );
    }

    #[test]
    fn test_accumulate_resources_at_boundary() {
        use henyey_common::resource::{Resource, ResourceType};
        let mut a = Resource::make_empty_soroban();
        a.set_val(ResourceType::Instructions, i64::MAX - 1);

        let mut b = Resource::make_empty_soroban();
        b.set_val(ResourceType::Instructions, 1);

        let result = accumulate_resources(a, &b).unwrap();
        assert_eq!(result.get_val(ResourceType::Instructions), i64::MAX);
    }

    // --- Overflow helper tests: checked_add_cluster_instructions ---

    #[test]
    fn test_checked_add_cluster_instructions_normal() {
        let result = checked_add_cluster_instructions(5000, 1000).unwrap();
        assert_eq!(result, 6000);
    }

    #[test]
    fn test_checked_add_cluster_instructions_overflow() {
        let result = checked_add_cluster_instructions(i64::MAX - 100, 101);
        assert_eq!(
            result.unwrap_err(),
            TxSetValidationResult::SorobanSequentialInstructionsOverflow
        );
    }

    #[test]
    fn test_checked_add_cluster_instructions_at_boundary() {
        let result =
            checked_add_cluster_instructions(i64::MAX - u32::MAX as i64, u32::MAX).unwrap();
        assert_eq!(result, i64::MAX);
    }

    // --- Overflow helper tests: checked_add_sequential_instructions ---

    #[test]
    fn test_checked_add_sequential_instructions_normal() {
        let result = checked_add_sequential_instructions(10_000, 5_000).unwrap();
        assert_eq!(result, 15_000);
    }

    #[test]
    fn test_checked_add_sequential_instructions_overflow() {
        let result = checked_add_sequential_instructions(i64::MAX - 100, 101);
        assert_eq!(
            result.unwrap_err(),
            TxSetValidationResult::SorobanInstructionsOverflow
        );
    }

    #[test]
    fn test_checked_add_sequential_instructions_at_boundary() {
        let result = checked_add_sequential_instructions(i64::MAX - 1000, 1000).unwrap();
        assert_eq!(result, i64::MAX);
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

        let invalid = get_invalid_tx_list(&[envelope], &ctx, &bounds, None, None);
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
        use stellar_xdr::{
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
        assert_eq!(envelope_fee(&fee_bump).as_i64(), 500);
    }

    #[test]
    fn test_envelope_inclusion_fee_fee_bump_classic() {
        // Classic fee-bump: no resource_fee, so inclusion_fee = full fee
        let inner = make_multi_op_envelope(2, 200);
        let fee_bump = make_fee_bump_envelope(inner, 500);
        assert_eq!(envelope_inclusion_fee(&fee_bump).as_i64(), 500);
    }

    // ========================================================================
    // Validation parity tests (issue #1510)
    // These document missing checks; each is #[ignore]d until the fix lands.
    // ========================================================================

    /// Build a minimal 2-phase GeneralizedTransactionSet for check_tx_set_valid.
    fn make_gen_tx_set(
        classic_txs: Vec<TransactionEnvelope>,
        soroban_txs: Vec<TransactionEnvelope>,
    ) -> GeneralizedTransactionSet {
        use stellar_xdr::{Hash, TransactionSetV1};

        let classic_phase = make_v0_phase_with_fee(classic_txs, Some(100));
        let soroban_phase = make_v0_phase_with_fee(soroban_txs, Some(100));

        GeneralizedTransactionSet::V1(TransactionSetV1 {
            previous_ledger_hash: Hash([0u8; 32]),
            phases: vec![classic_phase, soroban_phase].try_into().unwrap(),
        })
    }

    /// Regression test for #1482 — Soroban validation must not be silently bypassed.
    #[test]
    fn test_check_tx_set_valid_rejects_soroban_when_config_none() {
        let soroban_tx = make_soroban_envelope(100, 100, 100, vec![], vec![]);
        let gen_tx_set = make_gen_tx_set(vec![], vec![soroban_tx]);
        let header = make_soroban_lcl_header(25);
        let network_id = NetworkId::testnet();

        // soroban_info=None should cause rejection, not silent bypass
        let result = check_tx_set_valid(
            &gen_tx_set,
            &header,
            &Hash256::ZERO,
            0,
            network_id,
            None, // no soroban config
            None,
            None,
            None,
        );
        assert!(
            result.is_err(),
            "check_tx_set_valid should reject Soroban tx-set when config unavailable"
        );
    }

    /// #1504 — SCP tx-set validation skips sequence/account checks.
    /// Now fixed: validate_tx_for_tx_set checks sequence numbers against account state.
    #[test]
    fn test_check_tx_set_valid_rejects_bad_sequence() {
        // Build a classic tx with an obviously wrong sequence number (999_999_999).
        // The source account has seq_num=0, so a valid tx would need seq_num=1.
        let bad_seq_tx = make_valid_envelope(200, 999_999_999);
        let gen_tx_set = make_gen_tx_set(vec![bad_seq_tx], vec![]);
        let header = make_soroban_lcl_header(25);
        let network_id = NetworkId::testnet();

        let mut fee_provider = MockFeeBalanceProvider::new();
        fee_provider.set_balance([0u8; 32], 1_000_000);

        // Account with seq_num=0 — the tx's seq 999_999_999 is bad
        let mut account_provider = MockAccountProvider::new();
        account_provider.add_account([0u8; 32], 0);

        let result = check_tx_set_valid(
            &gen_tx_set,
            &header,
            &Hash256::ZERO,
            0,
            network_id,
            None,
            Some(&fee_provider),
            Some(&account_provider),
            None,
        );
        assert!(
            result.is_err(),
            "check_tx_set_valid should reject tx-set with bad sequence number"
        );
    }

    /// #1503 — SCP tx-set validation skips auth checks.
    /// Now fixed: validate_tx_for_tx_set checks transaction signatures.
    #[test]
    fn test_check_tx_set_valid_rejects_bad_auth() {
        // Build a classic tx with no signatures (empty signature list)
        let source = MuxedAccount::Ed25519(Uint256([50u8; 32]));
        let dest = MuxedAccount::Ed25519(Uint256([51u8; 32]));
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
            fee: 200,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![op].try_into().unwrap(),
            ext: TransactionExt::V0,
        };
        let no_sig_tx = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![].try_into().unwrap(), // no signatures
        });

        let gen_tx_set = make_gen_tx_set(vec![no_sig_tx], vec![]);
        let header = make_soroban_lcl_header(25);
        let network_id = NetworkId::testnet();

        let mut fee_provider = MockFeeBalanceProvider::new();
        fee_provider.set_balance([50u8; 32], 1_000_000);

        // Account exists with correct seq so we pass seq check and reach auth check
        let mut account_provider = MockAccountProvider::new();
        account_provider.add_account([50u8; 32], 0);

        let result = check_tx_set_valid(
            &gen_tx_set,
            &header,
            &Hash256::ZERO,
            0,
            network_id,
            None,
            Some(&fee_provider),
            Some(&account_provider),
            None,
        );
        // stellar-core would reject unsigned transactions in a tx-set.
        assert!(
            result.is_err(),
            "check_tx_set_valid should reject tx-set with unsigned transactions"
        );
    }

    /// Regression test: check_tx_set_valid reports specific fee-map failure reason.
    #[test]
    fn test_check_tx_set_valid_fee_map_error_reports_reason() {
        // Build a GeneralizedTransactionSet with component base_fee=50 < lcl base_fee=100.
        let tx = make_valid_envelope(200, 1);
        let phase_with_low_base_fee = make_v0_phase_with_fee(vec![tx.clone()], Some(50));
        let soroban_phase = make_v0_phase_with_fee(vec![], Some(100));

        use stellar_xdr::{Hash, TransactionSetV1};
        let gen_tx_set = GeneralizedTransactionSet::V1(TransactionSetV1 {
            previous_ledger_hash: Hash([0u8; 32]),
            phases: vec![phase_with_low_base_fee, soroban_phase]
                .try_into()
                .unwrap(),
        });
        let header = make_soroban_lcl_header(25);
        let network_id = NetworkId::testnet();

        let result = check_tx_set_valid(
            &gen_tx_set,
            &header,
            &Hash256::ZERO,
            0,
            network_id,
            None,
            None,
            None,
            None,
        );
        let err = result.unwrap_err();
        assert_eq!(
            err.result,
            TxSetValidationResult::ComponentBaseFeeTooLow,
            "should report specific fee-map failure reason, got: {}",
            err
        );
        assert_eq!(
            err.phase_idx,
            Some(0),
            "should indicate which phase failed, got: {}",
            err
        );
    }

    #[test]
    fn test_check_tx_set_valid_soroban_error_reports_reason() {
        // Build a GeneralizedTransactionSet with Soroban instructions exceeding the limit.
        let classic_phase = make_v0_phase_with_fee(vec![], Some(100));
        let tx = make_soroban_envelope(2_000_000, 100, 100, vec![], vec![]);
        let soroban_phase = make_v0_phase_with_fee(vec![tx], Some(100));

        use stellar_xdr::{Hash, TransactionSetV1};
        let gen_tx_set = GeneralizedTransactionSet::V1(TransactionSetV1 {
            previous_ledger_hash: Hash([0u8; 32]),
            phases: vec![classic_phase, soroban_phase].try_into().unwrap(),
        });
        let header = make_soroban_lcl_header(22);
        let info = make_soroban_network_info();
        let network_id = NetworkId::testnet();

        let result = check_tx_set_valid(
            &gen_tx_set,
            &header,
            &Hash256::ZERO,
            0,
            network_id,
            Some(&info),
            None,
            None,
            None,
        );
        let err = result.unwrap_err();
        assert_eq!(
            err.result,
            TxSetValidationResult::SorobanResourcesExceedLimit,
            "should report specific TxSetValidationResult, got: {}",
            err
        );
        assert_eq!(
            err.phase_idx,
            Some(1),
            "should indicate which phase failed, got: {}",
            err
        );
    }

    // --- validate_tx_for_tx_set unit tests ---

    /// Test that get_invalid_tx_list rejects tx when source account doesn't exist.
    #[test]
    fn test_validate_rejects_missing_source_account() {
        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();
        let tx = make_valid_envelope(100, 1);

        // Empty account provider — no accounts
        let account_provider = MockAccountProvider::new();

        let invalid = get_invalid_tx_list(&[tx], &ctx, &bounds, None, Some(&account_provider));
        assert_eq!(
            invalid.len(),
            1,
            "tx with missing source should be rejected"
        );
    }

    /// Test that get_invalid_tx_list accepts tx when stateful checks pass.
    #[test]
    fn test_validate_accepts_valid_sequence() {
        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();
        // tx seq = 1, account seq = 0 → valid (0 + 1 = 1)
        let tx = make_valid_envelope(100, 1);

        let mut account_provider = MockAccountProvider::new();
        account_provider.add_account([0u8; 32], 0);

        let invalid = get_invalid_tx_list(&[tx], &ctx, &bounds, None, Some(&account_provider));
        // TX-level auth will fail (no real signature), but it should get past seq check
        // The important thing is that the validation pipeline is invoked
        assert_eq!(invalid.len(), 1, "tx with no real sig should fail auth");
    }

    /// Test that validate_sequence rejects when account seq doesn't match.
    #[test]
    fn test_validate_sequence_mismatch() {
        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();
        // tx seq = 5, account seq = 0 → bad (0 + 1 ≠ 5)
        let tx = make_valid_envelope(100, 5);

        let mut account_provider = MockAccountProvider::new();
        account_provider.add_account([0u8; 32], 0);

        let invalid = get_invalid_tx_list(&[tx], &ctx, &bounds, None, Some(&account_provider));
        assert_eq!(invalid.len(), 1, "tx with wrong seq should be rejected");
    }

    /// Test that validation is skipped when no account_provider is set.
    #[test]
    fn test_validate_skipped_without_provider() {
        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();
        let tx = make_valid_envelope(100, 999_999);

        // No account provider → stateful checks skipped
        let invalid = get_invalid_tx_list(&[tx], &ctx, &bounds, None, None);
        assert!(
            invalid.is_empty(),
            "without account_provider, stateful checks should be skipped"
        );
    }

    /// Test that trim_invalid_two_phase threads account_provider correctly.
    #[test]
    fn test_trim_invalid_two_phase_with_account_provider() {
        let ctx =
            TxSetValidationContext::new(100, 1000, 100, 5_000_000, 25, NetworkId::testnet(), 0);
        let bounds = CloseTimeBounds::exact();

        let tx_good_seq = make_valid_envelope(100, 1);
        let tx_bad_seq = make_valid_envelope(100, 999);

        let mut account_provider = MockAccountProvider::new();
        account_provider.add_account([0u8; 32], 0);

        let (valid_classic, _valid_soroban) = trim_invalid_two_phase(
            &[tx_good_seq, tx_bad_seq],
            &[],
            &ctx,
            &bounds,
            None,
            Some(&account_provider),
        );
        // tx_good_seq: seq=1, account seq=0 → passes seq check, fails auth (no real sig)
        // tx_bad_seq: seq=999, account seq=0 → fails seq check (0+1 ≠ 999)
        // Both should be invalid
        assert_eq!(
            valid_classic.len(),
            0,
            "both txs should be trimmed (bad-seq + auth failure)"
        );
    }

    // ─── Signed transaction test helpers ───────────────────────────────

    use henyey_crypto::{sign_hash, SecretKey as CryptoSecretKey};

    /// Create a properly signed transaction envelope.
    ///
    /// Signs the tx hash with the given secret key, producing a
    /// cryptographically valid `DecoratedSignature`.
    fn make_signed_envelope(secret: &CryptoSecretKey, fee: u32, seq: i64) -> TransactionEnvelope {
        let pk_bytes = *secret.public_key().as_bytes();
        let source = MuxedAccount::Ed25519(Uint256(pk_bytes));
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

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: VecM::default(), // placeholder, will be replaced
        });

        // Compute the tx hash and sign it
        let network_id = NetworkId::testnet();
        let tx_hash = TransactionFrame::hash_envelope(&envelope, &network_id).unwrap();
        let sig = sign_hash(secret, &tx_hash);
        let hint = SignatureHint([pk_bytes[28], pk_bytes[29], pk_bytes[30], pk_bytes[31]]);
        let decorated = DecoratedSignature {
            hint,
            signature: XdrSignature(sig.0.to_vec().try_into().unwrap()),
        };

        // Replace signatures
        match envelope {
            TransactionEnvelope::Tx(mut env) => {
                env.signatures = vec![decorated].try_into().unwrap();
                TransactionEnvelope::Tx(env)
            }
            _ => unreachable!(),
        }
    }

    /// Create a signed fee-bump envelope wrapping a signed inner tx.
    fn make_signed_fee_bump_envelope(
        inner_secret: &CryptoSecretKey,
        fee_secret: &CryptoSecretKey,
        inner_fee: u32,
        inner_seq: i64,
        bumped_fee: i64,
    ) -> TransactionEnvelope {
        use stellar_xdr::{
            FeeBumpTransaction, FeeBumpTransactionEnvelope, FeeBumpTransactionExt,
            FeeBumpTransactionInnerTx,
        };

        // Build the signed inner envelope
        let inner_env = match make_signed_envelope(inner_secret, inner_fee, inner_seq) {
            TransactionEnvelope::Tx(e) => e,
            _ => unreachable!(),
        };

        let fee_pk_bytes = *fee_secret.public_key().as_bytes();
        let fee_source = MuxedAccount::Ed25519(Uint256(fee_pk_bytes));

        let fee_bump_envelope = TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
            tx: FeeBumpTransaction {
                fee_source,
                fee: bumped_fee,
                inner_tx: FeeBumpTransactionInnerTx::Tx(inner_env),
                ext: FeeBumpTransactionExt::V0,
            },
            signatures: VecM::default(), // placeholder
        });

        // Sign the outer envelope
        let network_id = NetworkId::testnet();
        let outer_hash = TransactionFrame::hash_envelope(&fee_bump_envelope, &network_id).unwrap();
        let sig = sign_hash(fee_secret, &outer_hash);
        let hint = SignatureHint([
            fee_pk_bytes[28],
            fee_pk_bytes[29],
            fee_pk_bytes[30],
            fee_pk_bytes[31],
        ]);
        let decorated = DecoratedSignature {
            hint,
            signature: XdrSignature(sig.0.to_vec().try_into().unwrap()),
        };

        match fee_bump_envelope {
            TransactionEnvelope::TxFeeBump(mut env) => {
                env.signatures = vec![decorated].try_into().unwrap();
                TransactionEnvelope::TxFeeBump(env)
            }
            _ => unreachable!(),
        }
    }

    /// Create a signed envelope with V2 preconditions (extra signers).
    fn make_signed_envelope_with_extra_signers(
        secret: &CryptoSecretKey,
        fee: u32,
        seq: i64,
        extra_signer_keys: Vec<SignerKey>,
    ) -> TransactionEnvelope {
        let pk_bytes = *secret.public_key().as_bytes();
        let source = MuxedAccount::Ed25519(Uint256(pk_bytes));
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
            cond: Preconditions::V2(stellar_xdr::PreconditionsV2 {
                time_bounds: None,
                ledger_bounds: None,
                min_seq_num: None,
                min_seq_age: stellar_xdr::Duration(0),
                min_seq_ledger_gap: 0,
                extra_signers: extra_signer_keys.try_into().unwrap(),
            }),
            memo: Memo::None,
            operations: vec![op].try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: VecM::default(),
        });

        let network_id = NetworkId::testnet();
        let tx_hash = TransactionFrame::hash_envelope(&envelope, &network_id).unwrap();
        let sig = sign_hash(secret, &tx_hash);
        let hint = SignatureHint([pk_bytes[28], pk_bytes[29], pk_bytes[30], pk_bytes[31]]);
        let decorated = DecoratedSignature {
            hint,
            signature: XdrSignature(sig.0.to_vec().try_into().unwrap()),
        };

        match envelope {
            TransactionEnvelope::Tx(mut env) => {
                env.signatures = vec![decorated].try_into().unwrap();
                TransactionEnvelope::Tx(env)
            }
            _ => unreachable!(),
        }
    }

    // ─── Positive-path signed transaction tests ────────────────────────

    /// Test that a properly signed transaction passes all validation checks.
    #[test]
    fn test_validate_signed_tx_passes() {
        let secret = CryptoSecretKey::from_seed(&[42u8; 32]);
        let pk_bytes = *secret.public_key().as_bytes();
        let tx = make_signed_envelope(&secret, 100, 1);

        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();

        let mut account_provider = MockAccountProvider::new();
        account_provider.add_account(pk_bytes, 0); // seq=0, so tx seq=1 is valid

        let invalid = get_invalid_tx_list(&[tx], &ctx, &bounds, None, Some(&account_provider));
        assert!(
            invalid.is_empty(),
            "properly signed tx with correct seq should pass validation"
        );
    }

    /// Test that a signed tx with wrong sequence is rejected.
    #[test]
    fn test_validate_signed_tx_bad_sequence() {
        let secret = CryptoSecretKey::from_seed(&[42u8; 32]);
        let pk_bytes = *secret.public_key().as_bytes();
        let tx = make_signed_envelope(&secret, 100, 5); // seq=5

        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();

        let mut account_provider = MockAccountProvider::new();
        account_provider.add_account(pk_bytes, 0); // seq=0, expects tx seq=1

        let invalid = get_invalid_tx_list(&[tx], &ctx, &bounds, None, Some(&account_provider));
        assert_eq!(invalid.len(), 1, "tx with bad sequence should be rejected");
    }

    /// Test that a signed tx with an extra unused signature is rejected.
    #[test]
    fn test_validate_signed_tx_extra_signature_rejected() {
        let secret = CryptoSecretKey::from_seed(&[42u8; 32]);
        let other_secret = CryptoSecretKey::from_seed(&[99u8; 32]);
        let pk_bytes = *secret.public_key().as_bytes();

        // Build a valid signed envelope, then add an extra signature
        let tx = make_signed_envelope(&secret, 100, 1);
        let network_id = NetworkId::testnet();
        let tx_hash = TransactionFrame::hash_envelope(&tx, &network_id).unwrap();

        // Sign with the extra key
        let extra_sig = sign_hash(&other_secret, &tx_hash);
        let other_pk = other_secret.public_key();
        let other_bytes = other_pk.as_bytes();
        let extra_decorated = DecoratedSignature {
            hint: SignatureHint([
                other_bytes[28],
                other_bytes[29],
                other_bytes[30],
                other_bytes[31],
            ]),
            signature: XdrSignature(extra_sig.0.to_vec().try_into().unwrap()),
        };

        let tx = match tx {
            TransactionEnvelope::Tx(mut env) => {
                let mut sigs: Vec<_> = env.signatures.to_vec();
                sigs.push(extra_decorated);
                env.signatures = sigs.try_into().unwrap();
                TransactionEnvelope::Tx(env)
            }
            _ => unreachable!(),
        };

        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();

        let mut account_provider = MockAccountProvider::new();
        account_provider.add_account(pk_bytes, 0);

        let invalid = get_invalid_tx_list(&[tx], &ctx, &bounds, None, Some(&account_provider));
        assert_eq!(
            invalid.len(),
            1,
            "tx with extra unused signature should be rejected (txBAD_AUTH_EXTRA)"
        );
    }

    /// Test that a properly signed fee-bump tx passes validation.
    #[test]
    fn test_validate_signed_fee_bump_passes() {
        let inner_secret = CryptoSecretKey::from_seed(&[42u8; 32]);
        let fee_secret = CryptoSecretKey::from_seed(&[43u8; 32]);
        let inner_pk = *inner_secret.public_key().as_bytes();
        let fee_pk = *fee_secret.public_key().as_bytes();

        let tx = make_signed_fee_bump_envelope(&inner_secret, &fee_secret, 100, 1, 200);

        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();

        let mut account_provider = MockAccountProvider::new();
        account_provider.add_account(inner_pk, 0);
        account_provider.add_account(fee_pk, 0);

        let invalid = get_invalid_tx_list(&[tx], &ctx, &bounds, None, Some(&account_provider));
        assert!(
            invalid.is_empty(),
            "properly signed fee-bump tx should pass validation"
        );
    }

    /// Test that a fee-bump tx with missing fee source account is rejected.
    #[test]
    fn test_validate_fee_bump_missing_fee_source() {
        let inner_secret = CryptoSecretKey::from_seed(&[42u8; 32]);
        let fee_secret = CryptoSecretKey::from_seed(&[43u8; 32]);
        let inner_pk = *inner_secret.public_key().as_bytes();

        let tx = make_signed_fee_bump_envelope(&inner_secret, &fee_secret, 100, 1, 200);

        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();

        let mut account_provider = MockAccountProvider::new();
        account_provider.add_account(inner_pk, 0);
        // fee source NOT added → should be rejected

        let invalid = get_invalid_tx_list(&[tx], &ctx, &bounds, None, Some(&account_provider));
        assert_eq!(
            invalid.len(),
            1,
            "fee-bump with missing fee source should be rejected"
        );
    }

    /// Test that a fee-bump tx signed by a key NOT in the fee source's signer
    /// set is rejected by tx-set validation. This exercises the outer auth
    /// failure path at `validate_fee_bump_for_tx_set` (line ~430).
    ///
    /// Regression coverage for #2270 (AUDIT-245): since apply-time skips outer
    /// re-validation, tx-set validation MUST catch invalid outer signatures.
    #[test]
    fn test_validate_fee_bump_invalid_outer_signature_rejected() {
        use stellar_xdr::{
            FeeBumpTransaction, FeeBumpTransactionEnvelope, FeeBumpTransactionExt,
            FeeBumpTransactionInnerTx,
        };

        let inner_secret = CryptoSecretKey::from_seed(&[42u8; 32]);
        let fee_secret = CryptoSecretKey::from_seed(&[43u8; 32]);
        let wrong_signer = CryptoSecretKey::from_seed(&[99u8; 32]);

        let inner_pk = *inner_secret.public_key().as_bytes();
        let fee_pk = *fee_secret.public_key().as_bytes();
        let wrong_pk = *wrong_signer.public_key().as_bytes();

        // Build a valid signed inner envelope
        let inner_env = match make_signed_envelope(&inner_secret, 100, 1) {
            TransactionEnvelope::Tx(e) => e,
            _ => unreachable!(),
        };

        // Build fee-bump with fee_source = fee_secret's public key
        let fee_source = MuxedAccount::Ed25519(Uint256(fee_pk));
        let fee_bump_envelope = TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
            tx: FeeBumpTransaction {
                fee_source,
                fee: 200,
                inner_tx: FeeBumpTransactionInnerTx::Tx(inner_env),
                ext: FeeBumpTransactionExt::V0,
            },
            signatures: VecM::default(),
        });

        // Sign the outer hash with `wrong_signer` (NOT the fee source)
        let network_id = NetworkId::testnet();
        let outer_hash = TransactionFrame::hash_envelope(&fee_bump_envelope, &network_id).unwrap();
        let sig = sign_hash(&wrong_signer, &outer_hash);
        let hint = SignatureHint([wrong_pk[28], wrong_pk[29], wrong_pk[30], wrong_pk[31]]);
        let decorated = DecoratedSignature {
            hint,
            signature: XdrSignature(sig.0.to_vec().try_into().unwrap()),
        };

        let tx = match fee_bump_envelope {
            TransactionEnvelope::TxFeeBump(mut env) => {
                env.signatures = vec![decorated].try_into().unwrap();
                TransactionEnvelope::TxFeeBump(env)
            }
            _ => unreachable!(),
        };

        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();

        // Both accounts exist so we pass earlier checks and reach outer auth
        let mut account_provider = MockAccountProvider::new();
        account_provider.add_account(inner_pk, 0);
        account_provider.add_account(fee_pk, 0);

        let invalid = get_invalid_tx_list(&[tx], &ctx, &bounds, None, Some(&account_provider));
        assert_eq!(
            invalid.len(),
            1,
            "fee-bump signed by key not in fee source's signer set must be rejected"
        );
    }

    /// Test that a fee-bump with a valid outer signature plus an extra unused
    /// outer signature is rejected (txBAD_AUTH_EXTRA).
    ///
    /// Branch isolation: both accounts exist, outer auth passes via the valid
    /// fee-source signature, inner tx is fully valid. The only defect is the
    /// extra unused outer signature, so rejection can only come from
    /// `check_all_signatures_used()` on the outer envelope.
    #[test]
    fn test_validate_fee_bump_extra_outer_signature_rejected() {
        let inner_secret = CryptoSecretKey::from_seed(&[42u8; 32]);
        let fee_secret = CryptoSecretKey::from_seed(&[43u8; 32]);
        let extra_secret = CryptoSecretKey::from_seed(&[99u8; 32]);

        let inner_pk = *inner_secret.public_key().as_bytes();
        let fee_pk = *fee_secret.public_key().as_bytes();
        let extra_pk = *extra_secret.public_key().as_bytes();

        // Build a valid signed fee-bump envelope
        let tx = make_signed_fee_bump_envelope(&inner_secret, &fee_secret, 100, 1, 200);

        // Sign the outer hash with the extra key and append it
        let network_id = NetworkId::testnet();
        let outer_hash = TransactionFrame::hash_envelope(&tx, &network_id).unwrap();
        let extra_sig = sign_hash(&extra_secret, &outer_hash);
        let extra_decorated = DecoratedSignature {
            hint: SignatureHint([extra_pk[28], extra_pk[29], extra_pk[30], extra_pk[31]]),
            signature: XdrSignature(extra_sig.0.to_vec().try_into().unwrap()),
        };

        let tx = match tx {
            TransactionEnvelope::TxFeeBump(mut env) => {
                let mut sigs: Vec<_> = env.signatures.to_vec();
                sigs.push(extra_decorated);
                env.signatures = sigs.try_into().unwrap();
                TransactionEnvelope::TxFeeBump(env)
            }
            _ => unreachable!(),
        };

        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();

        let mut account_provider = MockAccountProvider::new();
        account_provider.add_account(inner_pk, 0);
        account_provider.add_account(fee_pk, 0);

        let invalid = get_invalid_tx_list(&[tx], &ctx, &bounds, None, Some(&account_provider));
        assert_eq!(
            invalid.len(),
            1,
            "fee-bump with extra unused outer signature should be rejected (txBAD_AUTH_EXTRA)"
        );
    }

    /// Test that a fee-bump whose inner transaction carries an extra unused
    /// signature is rejected (txBAD_AUTH_EXTRA).
    ///
    /// Branch isolation: both accounts exist, outer auth passes (valid
    /// fee-source signature, no extra outer sigs), inner auth passes via the
    /// valid inner signature. The only defect is the extra unused inner
    /// signature, so rejection can only come from `check_all_signatures_used()`
    /// on the inner transaction.
    #[test]
    fn test_validate_fee_bump_extra_inner_signature_rejected() {
        use stellar_xdr::{
            FeeBumpTransaction, FeeBumpTransactionEnvelope, FeeBumpTransactionExt,
            FeeBumpTransactionInnerTx,
        };

        let inner_secret = CryptoSecretKey::from_seed(&[42u8; 32]);
        let fee_secret = CryptoSecretKey::from_seed(&[43u8; 32]);
        let extra_secret = CryptoSecretKey::from_seed(&[99u8; 32]);

        let inner_pk = *inner_secret.public_key().as_bytes();
        let fee_pk = *fee_secret.public_key().as_bytes();
        let extra_pk = *extra_secret.public_key().as_bytes();

        // Build a valid signed inner envelope, then append an extra signature
        let inner_envelope = make_signed_envelope(&inner_secret, 100, 1);
        let network_id = NetworkId::testnet();
        let inner_hash = TransactionFrame::hash_envelope(&inner_envelope, &network_id).unwrap();

        let extra_sig = sign_hash(&extra_secret, &inner_hash);
        let extra_decorated = DecoratedSignature {
            hint: SignatureHint([extra_pk[28], extra_pk[29], extra_pk[30], extra_pk[31]]),
            signature: XdrSignature(extra_sig.0.to_vec().try_into().unwrap()),
        };

        let inner_env = match inner_envelope {
            TransactionEnvelope::Tx(mut env) => {
                let mut sigs: Vec<_> = env.signatures.to_vec();
                sigs.push(extra_decorated);
                env.signatures = sigs.try_into().unwrap();
                env
            }
            _ => unreachable!(),
        };

        // Build fee-bump wrapping the modified inner envelope
        let fee_source = MuxedAccount::Ed25519(Uint256(fee_pk));
        let fee_bump_envelope = TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
            tx: FeeBumpTransaction {
                fee_source,
                fee: 200,
                inner_tx: FeeBumpTransactionInnerTx::Tx(inner_env),
                ext: FeeBumpTransactionExt::V0,
            },
            signatures: VecM::default(),
        });

        // Sign the outer envelope with the fee source key
        let outer_hash = TransactionFrame::hash_envelope(&fee_bump_envelope, &network_id).unwrap();
        let fee_sig = sign_hash(&fee_secret, &outer_hash);
        let fee_decorated = DecoratedSignature {
            hint: SignatureHint([fee_pk[28], fee_pk[29], fee_pk[30], fee_pk[31]]),
            signature: XdrSignature(fee_sig.0.to_vec().try_into().unwrap()),
        };

        let tx = match fee_bump_envelope {
            TransactionEnvelope::TxFeeBump(mut env) => {
                env.signatures = vec![fee_decorated].try_into().unwrap();
                TransactionEnvelope::TxFeeBump(env)
            }
            _ => unreachable!(),
        };

        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();

        let mut account_provider = MockAccountProvider::new();
        account_provider.add_account(inner_pk, 0);
        account_provider.add_account(fee_pk, 0);

        let invalid = get_invalid_tx_list(&[tx], &ctx, &bounds, None, Some(&account_provider));
        assert_eq!(
            invalid.len(),
            1,
            "fee-bump with extra unused inner signature should be rejected (txBAD_AUTH_EXTRA)"
        );
    }

    /// Test that a tx with an extra signer in V2 preconditions passes when
    /// the extra signer's signature is present.
    #[test]
    fn test_validate_extra_signer_present_passes() {
        let tx_secret = CryptoSecretKey::from_seed(&[42u8; 32]);
        let extra_secret = CryptoSecretKey::from_seed(&[99u8; 32]);
        let tx_pk = *tx_secret.public_key().as_bytes();
        let extra_pk = *extra_secret.public_key().as_bytes();

        let extra_signer_key = SignerKey::Ed25519(Uint256(extra_pk));
        let mut tx =
            make_signed_envelope_with_extra_signers(&tx_secret, 100, 1, vec![extra_signer_key]);

        // Add the extra signer's signature to the envelope
        let network_id = NetworkId::testnet();
        let tx_hash = TransactionFrame::hash_envelope(&tx, &network_id).unwrap();
        let extra_sig = sign_hash(&extra_secret, &tx_hash);
        let extra_decorated = DecoratedSignature {
            hint: SignatureHint([extra_pk[28], extra_pk[29], extra_pk[30], extra_pk[31]]),
            signature: XdrSignature(extra_sig.0.to_vec().try_into().unwrap()),
        };

        tx = match tx {
            TransactionEnvelope::Tx(mut env) => {
                let mut sigs: Vec<_> = env.signatures.to_vec();
                sigs.push(extra_decorated);
                env.signatures = sigs.try_into().unwrap();
                TransactionEnvelope::Tx(env)
            }
            _ => unreachable!(),
        };

        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();

        let mut account_provider = MockAccountProvider::new();
        account_provider.add_account(tx_pk, 0);

        let invalid = get_invalid_tx_list(&[tx], &ctx, &bounds, None, Some(&account_provider));
        assert!(
            invalid.is_empty(),
            "tx with satisfied extra signer should pass validation"
        );
    }

    /// Test that a tx with extra signers is rejected when the extra signer's
    /// signature is missing.
    #[test]
    fn test_validate_extra_signer_missing_rejected() {
        let tx_secret = CryptoSecretKey::from_seed(&[42u8; 32]);
        let extra_secret = CryptoSecretKey::from_seed(&[99u8; 32]);
        let tx_pk = *tx_secret.public_key().as_bytes();
        let extra_pk = *extra_secret.public_key().as_bytes();

        let extra_signer_key = SignerKey::Ed25519(Uint256(extra_pk));
        let tx =
            make_signed_envelope_with_extra_signers(&tx_secret, 100, 1, vec![extra_signer_key]);
        // NOTE: extra signer's signature NOT added — only tx source sig is present

        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();

        let mut account_provider = MockAccountProvider::new();
        account_provider.add_account(tx_pk, 0);

        let invalid = get_invalid_tx_list(&[tx], &ctx, &bounds, None, Some(&account_provider));
        assert_eq!(
            invalid.len(),
            1,
            "tx with unsatisfied extra signer should be rejected"
        );
    }

    // ========================================================================
    // Regression tests for #1476: SCP fee-source affordability
    // ========================================================================

    /// Build a fee-bump envelope with a custom fee source.
    fn make_fee_bump_with_fee_source(
        inner_source: [u8; 32],
        fee_source: [u8; 32],
        inner_fee: u32,
        bumped_fee: i64,
        seq: i64,
    ) -> TransactionEnvelope {
        use stellar_xdr::{
            FeeBumpTransaction, FeeBumpTransactionEnvelope, FeeBumpTransactionExt,
            FeeBumpTransactionInnerTx,
        };

        let inner = make_envelope_with_source(inner_source, inner_fee, seq);
        let inner_v1 = match inner {
            TransactionEnvelope::Tx(e) => e,
            _ => panic!("expected V1 envelope"),
        };

        TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
            tx: FeeBumpTransaction {
                fee_source: MuxedAccount::Ed25519(Uint256(fee_source)),
                fee: bumped_fee,
                inner_tx: FeeBumpTransactionInnerTx::Tx(inner_v1),
                ext: FeeBumpTransactionExt::V0,
            },
            signatures: VecM::default(),
        })
    }

    /// Regression test for #1476: two fee-bump transactions with different inner
    /// sources but the same outer fee source. With a provider, cumulative fee
    /// exceeds balance and both are rejected. Without a provider, both pass.
    #[test]
    fn test_fee_bump_shared_fee_source_affordability() {
        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();

        let fee_source = [50u8; 32];
        // Two fee-bumps: different inner sources, same outer fee source.
        // Each has bumped_fee=200, total=400.
        let tx1 = make_fee_bump_with_fee_source([10u8; 32], fee_source, 100, 200, 1);
        let tx2 = make_fee_bump_with_fee_source([20u8; 32], fee_source, 100, 200, 1);

        // With provider: balance=300 < total_fees=400 → both rejected
        let mut provider = MockFeeBalanceProvider::new();
        provider.set_balance(fee_source, 300);

        let invalid = get_invalid_tx_list(
            &[tx1.clone(), tx2.clone()],
            &ctx,
            &bounds,
            Some(&provider),
            None,
        );
        assert_eq!(
            invalid.len(),
            2,
            "both fee-bump txs should be rejected: shared fee source can't cover aggregate fees"
        );

        // Without provider: fee check skipped → both pass (this was the bug)
        let invalid = get_invalid_tx_list(&[tx1, tx2], &ctx, &bounds, None, None);
        assert!(
            invalid.is_empty(),
            "without provider, fee affordability is skipped (documents pre-fix gap)"
        );
    }

    /// Regression test for #1476: account provider catches bad sequence numbers
    /// that would be missed when provider is None.
    #[test]
    fn test_account_provider_catches_bad_sequence() {
        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();

        // Create a tx with seq_num=5, but the account has seq_num=10
        // (tx seq must be account_seq + 1 = 11, so 5 is stale)
        let tx = make_envelope_with_source([10u8; 32], 200, 5);

        let mut account_provider = MockAccountProvider::new();
        account_provider.add_account([10u8; 32], 10);

        // With account provider: bad sequence → rejected
        let invalid = get_invalid_tx_list(
            std::slice::from_ref(&tx),
            &ctx,
            &bounds,
            None,
            Some(&account_provider),
        );
        assert_eq!(
            invalid.len(),
            1,
            "tx with stale sequence should be rejected when account provider is present"
        );

        // Without account provider: sequence check skipped → passes
        let invalid = get_invalid_tx_list(&[tx], &ctx, &bounds, None, None);
        assert!(
            invalid.is_empty(),
            "without account provider, sequence check is skipped (documents pre-fix gap)"
        );
    }

    // ── Phase 6: HashedTx + hashed invalidation tests ──────────────────

    #[test]
    fn test_hashed_invalidation_parity_with_envelope_api() {
        // Verify that get_invalid_hashed_tx_list produces identical results
        // to get_invalid_tx_list for the same inputs.
        let valid_env = make_valid_envelope(100, 1);
        let low_fee_env = make_low_fee_envelope(1);
        let expired_env = make_expired_time_envelope(1);
        let txs = vec![valid_env.clone(), low_fee_env.clone(), expired_env.clone()];

        let ctx = TxSetValidationContext {
            next_ledger_seq: 100,
            close_time: 1000,
            base_fee: 100,
            base_reserve: 5_000_000,
            protocol_version: 21,
            network_id: NetworkId::testnet(),
            ledger_flags: 0,
            soroban_resource_limits: None,
            frozen_key_config: henyey_tx::frozen_keys::FrozenKeyConfig::empty(),
        };
        let bounds = CloseTimeBounds::with_offsets(0, 0);

        // Get results from both APIs.
        let invalid_envelopes = get_invalid_tx_list(&txs, &ctx, &bounds, None, None);
        let hashed_txs: Vec<HashedTx> = txs.iter().map(|tx| HashedTx::new(tx.clone())).collect();
        let invalid_hashed = get_invalid_hashed_tx_list(&hashed_txs, &ctx, &bounds, None, None);

        // Same count.
        assert_eq!(invalid_envelopes.len(), invalid_hashed.len());

        // Same hashes.
        let env_hashes: HashSet<Hash256> = invalid_envelopes
            .iter()
            .map(|e| Hash256::hash_xdr(e))
            .collect();
        let hashed_hashes: HashSet<Hash256> = invalid_hashed.iter().map(|h| h.hash).collect();
        assert_eq!(env_hashes, hashed_hashes);
    }

    #[test]
    fn test_hashed_tx_hash_matches_hash_xdr() {
        // Verify that HashedTx::new() computes the correct hash.
        let env = make_valid_envelope(200, 42);
        let expected_hash = Hash256::hash_xdr(&env);
        let htx = HashedTx::new(env.clone());
        assert_eq!(htx.hash(), expected_hash);
        assert_eq!(Hash256::hash_xdr(htx.envelope()), expected_hash);
    }

    #[test]
    fn test_hashed_invalidation_fee_source_affordability() {
        // Submit multiple txs from the same source whose combined fees
        // exceed the available balance. All should pass pass-1 validation
        // but the fee-source affordability check in pass-2 should catch them.
        let key_bytes = [0u8; 32];
        let env1 = make_valid_envelope(6_000_000, 1); // fee = 6M
        let env2 = make_valid_envelope(6_000_000, 2); // fee = 6M, total = 12M

        let mut fee_provider = MockFeeBalanceProvider::new();
        fee_provider.set_balance(key_bytes, 10_000_000); // only 10M available

        let ctx = TxSetValidationContext {
            next_ledger_seq: 100,
            close_time: 1000,
            base_fee: 100,
            base_reserve: 5_000_000,
            protocol_version: 21,
            network_id: NetworkId::testnet(),
            ledger_flags: 0,
            soroban_resource_limits: None,
            frozen_key_config: henyey_tx::frozen_keys::FrozenKeyConfig::empty(),
        };
        let bounds = CloseTimeBounds::with_offsets(0, 0);

        // Test with hashed API.
        let hashed_txs: Vec<HashedTx> = [env1.clone(), env2.clone()]
            .into_iter()
            .map(HashedTx::new)
            .collect();
        let invalid = get_invalid_hashed_tx_list(
            &hashed_txs,
            &ctx,
            &bounds,
            Some(&fee_provider as &dyn FeeBalanceProvider),
            None,
        );
        // At least one tx should be marked invalid due to fee affordability.
        assert!(
            !invalid.is_empty(),
            "expected fee-source affordability to reject at least one tx"
        );
    }

    #[test]
    fn test_hashed_invalidation_cross_phase_fee_sharing() {
        // Verify get_invalid_hashed_tx_list_with_fee_map correctly
        // accumulates fees across phases via the shared fee map.
        let key_bytes = [0u8; 32];
        let env1 = make_valid_envelope(4_000_000, 1);
        let env2 = make_valid_envelope(4_000_000, 2);

        let mut fee_provider = MockFeeBalanceProvider::new();
        fee_provider.set_balance(key_bytes, 6_000_000); // 6M available

        let ctx = TxSetValidationContext {
            next_ledger_seq: 100,
            close_time: 1000,
            base_fee: 100,
            base_reserve: 5_000_000,
            protocol_version: 21,
            network_id: NetworkId::testnet(),
            ledger_flags: 0,
            soroban_resource_limits: None,
            frozen_key_config: henyey_tx::frozen_keys::FrozenKeyConfig::empty(),
        };
        let bounds = CloseTimeBounds::with_offsets(0, 0);

        // Phase 1: first tx consumes 4M.
        let mut shared_fee_map: HashMap<AccountId, i64> = HashMap::new();
        let hashed1 = vec![HashedTx::new(env1.clone())];
        let invalid1 = get_invalid_hashed_tx_list_with_fee_map(
            &hashed1,
            &ctx,
            &bounds,
            Some(&fee_provider as &dyn FeeBalanceProvider),
            None,
            &mut shared_fee_map,
        );
        assert!(invalid1.is_empty(), "first phase: 4M fee within 6M balance");

        // Phase 2: second tx adds another 4M → total 8M > 6M available.
        let hashed2 = vec![HashedTx::new(env2.clone())];
        let invalid2 = get_invalid_hashed_tx_list_with_fee_map(
            &hashed2,
            &ctx,
            &bounds,
            Some(&fee_provider as &dyn FeeBalanceProvider),
            None,
            &mut shared_fee_map,
        );
        assert!(
            !invalid2.is_empty(),
            "second phase: 8M cumulative fee exceeds 6M balance"
        );
    }

    #[test]
    fn test_hashed_invalidation_duplicate_hash_handling() {
        // Verify correct behavior when duplicate envelopes appear in the input.
        let env = make_valid_envelope(100, 1);
        let htx = HashedTx::new(env.clone());
        let hashed_txs = vec![htx.clone(), htx];

        let ctx = TxSetValidationContext {
            next_ledger_seq: 100,
            close_time: 1000,
            base_fee: 100,
            base_reserve: 5_000_000,
            protocol_version: 21,
            network_id: NetworkId::testnet(),
            ledger_flags: 0,
            soroban_resource_limits: None,
            frozen_key_config: henyey_tx::frozen_keys::FrozenKeyConfig::empty(),
        };
        let bounds = CloseTimeBounds::with_offsets(0, 0);

        // Without fee provider, both duplicates should pass (no dedup in this function).
        let invalid = get_invalid_hashed_tx_list(&hashed_txs, &ctx, &bounds, None, None);
        assert!(
            invalid.is_empty(),
            "valid duplicates should both pass when no fee provider"
        );
    }

    // --- CAP-77 frozen key tests ---

    /// Build a FrozenKeyConfig that freezes the account with the given ed25519 key.
    fn frozen_config_for_account(key_bytes: [u8; 32]) -> henyey_tx::frozen_keys::FrozenKeyConfig {
        use stellar_xdr::{AccountId, LedgerKey, LedgerKeyAccount, PublicKey, Uint256, WriteXdr};
        let account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(key_bytes)));
        let ledger_key = LedgerKey::Account(LedgerKeyAccount { account_id });
        let key_xdr = ledger_key
            .to_xdr(stellar_xdr::Limits::none())
            .expect("encode ledger key");
        henyey_tx::frozen_keys::FrozenKeyConfig::new(vec![key_xdr], vec![])
    }

    #[test]
    fn test_frozen_source_regular_tx_rejected() {
        let mut ctx = test_context();
        // Freeze the source account used by make_valid_envelope ([0u8; 32])
        ctx.frozen_key_config = frozen_config_for_account([0u8; 32]);

        let mut account_provider = MockAccountProvider::new();
        account_provider.add_account([0u8; 32], 0);

        let bounds = CloseTimeBounds::exact();
        let txs = vec![make_valid_envelope(100, 1)];
        let invalid = get_invalid_tx_list(&txs, &ctx, &bounds, None, Some(&account_provider));
        assert_eq!(
            invalid.len(),
            1,
            "tx with frozen source account should be rejected"
        );
    }

    #[test]
    fn test_frozen_source_bypass_hash_passes() {
        use stellar_xdr::{
            AccountId, Hash, LedgerKey, LedgerKeyAccount, PublicKey, Uint256, WriteXdr,
        };

        let account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32])));
        let ledger_key = LedgerKey::Account(LedgerKeyAccount {
            account_id: account_id.clone(),
        });
        let key_xdr = ledger_key
            .to_xdr(stellar_xdr::Limits::none())
            .expect("encode ledger key");

        // Compute the tx hash used for bypass checking
        let envelope = make_valid_envelope(100, 1);
        let tx_hash = HashedTx::new(envelope).hash();
        let bypass_hash = Hash(tx_hash.0);

        let config = henyey_tx::frozen_keys::FrozenKeyConfig::new(vec![key_xdr], vec![bypass_hash]);

        // Source is frozen, but tx hash is in bypass set → should NOT detect frozen access
        assert!(config.has_frozen_keys());
        assert!(config.is_key_frozen(&henyey_tx::frozen_keys::account_key(&account_id)));
        assert!(
            config.is_freeze_bypass_tx(&tx_hash),
            "bypass hash should match"
        );
    }

    #[test]
    fn test_empty_frozen_config_all_pass() {
        // Default/empty frozen config should not reject any transactions
        let ctx = test_context();
        assert!(!ctx.frozen_key_config.has_frozen_keys());

        let bounds = CloseTimeBounds::exact();
        let txs = vec![make_valid_envelope(100, 1), make_valid_envelope(200, 2)];
        let invalid = get_invalid_tx_list(&txs, &ctx, &bounds, None, None);
        assert!(
            invalid.is_empty(),
            "empty frozen config should not reject any transactions"
        );
    }

    #[test]
    fn test_frozen_non_source_account_passes() {
        use stellar_xdr::{AccountId, PublicKey, Uint256};

        let mut ctx = test_context();
        // Freeze a different account (not the source in make_valid_envelope)
        ctx.frozen_key_config = frozen_config_for_account([99u8; 32]);

        // Verify the source account ([0u8; 32]) is not frozen
        let source = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32])));
        assert!(
            !ctx.frozen_key_config
                .is_key_frozen(&henyey_tx::frozen_keys::account_key(&source)),
            "source account should not be frozen"
        );

        // The frozen config affects a different account; the source account's
        // key is not in the frozen set, so the frozen check would pass.
        // (Full pipeline test not possible without real signatures.)
    }

    #[test]
    fn test_hashed_tx_from_queued_transaction() {
        let envelope = make_valid_envelope(100, 1);
        let qt = QueuedTransaction::new(envelope.clone()).unwrap();
        let hashed: HashedTx = HashedTx::from(&qt);
        assert_eq!(hashed.hash(), Hash256::hash_xdr(&envelope));
        assert_eq!(hashed.envelope(), &envelope);
    }

    #[test]
    #[should_panic(expected = "hash does not match envelope")]
    fn test_hashed_tx_from_prehashed_panics_on_mismatch() {
        let envelope = make_valid_envelope(100, 1);
        let wrong_hash = Hash256::hash_xdr(&make_valid_envelope(200, 2));
        HashedTx::from_prehashed(wrong_hash, Arc::new(envelope));
    }

    // --- Parallel pass-1 determinism / parity (issue #3673) ---

    /// Build a mixed tx-set large enough to exercise multiple parallel chunks:
    /// a deterministic interleaving of valid, structurally-invalid (low fee),
    /// time-expired, and bad-ledger-bounds txs, plus a fee-source group whose
    /// cumulative fee straddles the available balance. Returns the hashed txs,
    /// an account provider, and a fee-balance provider.
    fn make_parity_fixture(
        n: usize,
    ) -> (Vec<HashedTx>, MockAccountProvider, MockFeeBalanceProvider) {
        let mut accounts = MockAccountProvider::new();
        let mut fees = MockFeeBalanceProvider::new();
        let mut envelopes: Vec<TransactionEnvelope> = Vec::new();

        // A shared fee source group: several valid txs from distinct source
        // accounts but... in this fixture each tx has a distinct source, so the
        // fee map keys are per-source. We seed balances so some are affordable
        // and some are not, exercising pass-2 across chunk boundaries.
        for i in 0..n {
            let env = match i % 4 {
                0 => {
                    // Valid tx from a distinct, funded source.
                    let mut key = [0u8; 32];
                    key[0] = 0xA0;
                    key[1] = (i & 0xff) as u8;
                    key[2] = ((i >> 8) & 0xff) as u8;
                    accounts.add_account(key, 0); // seq 0 -> tx seq 1 valid
                    fees.set_balance(key, 1_000_000);
                    make_envelope_with_source(key, 200, 1)
                }
                1 => make_low_fee_envelope(i as i64 + 1), // structurally invalid
                2 => make_expired_time_envelope(i as i64 + 1), // time invalid
                _ => {
                    // Valid-structure tx whose source can't pay the fee.
                    let mut key = [0u8; 32];
                    key[0] = 0xB0;
                    key[1] = (i & 0xff) as u8;
                    key[2] = ((i >> 8) & 0xff) as u8;
                    accounts.add_account(key, 0);
                    fees.set_balance(key, 50); // < fee 200 -> can't pay
                    make_envelope_with_source(key, 200, 1)
                }
            };
            envelopes.push(env);
        }

        let hashed: Vec<HashedTx> = envelopes.into_iter().map(HashedTx::new).collect();
        (hashed, accounts, fees)
    }

    /// The parallel pass-1 path must produce a byte-identical invalid-hash list
    /// (in the same order) AND identical fee map to the serial oracle, for every
    /// input. Running it many times would expose any reliance on thread
    /// completion order (a non-deterministic merge would intermittently reorder
    /// the invalid list or mis-accumulate the fee map).
    #[test]
    fn test_parallel_pass1_matches_serial_oracle() {
        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();

        for n in [0usize, 1, 2, 5, 17, 64, 257] {
            let (hashed, accounts, fees) = make_parity_fixture(n);

            // Serial oracle.
            let mut serial_fee_map: HashMap<AccountId, i64> = HashMap::new();
            let serial = get_invalid_hashed_core_serial(
                &hashed,
                &ctx,
                &bounds,
                Some(&fees),
                Some(&accounts),
                &mut serial_fee_map,
            );
            let serial_hashes: Vec<Hash256> = serial.iter().map(|h| h.hash()).collect();

            // Run the parallel path repeatedly to catch completion-order reliance.
            for run in 0..16 {
                let mut par_fee_map: HashMap<AccountId, i64> = HashMap::new();
                let par = get_invalid_hashed_core_parallel(
                    &hashed,
                    &ctx,
                    &bounds,
                    Some(&fees),
                    Some(&accounts),
                    &mut par_fee_map,
                );
                let par_hashes: Vec<Hash256> = par.iter().map(|h| h.hash()).collect();

                assert_eq!(
                    par_hashes, serial_hashes,
                    "n={n} run={run}: parallel invalid-hash list (and order) must equal serial oracle"
                );
                assert_eq!(
                    par_fee_map, serial_fee_map,
                    "n={n} run={run}: parallel fee map must equal serial oracle"
                );
            }
        }
    }

    /// The public dispatcher (which chooses serial vs parallel by size) must
    /// always equal the serial oracle, regardless of the chunking threshold.
    #[test]
    fn test_public_core_matches_serial_oracle() {
        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();

        for n in [0usize, 3, 64, 300] {
            let (hashed, accounts, fees) = make_parity_fixture(n);

            let mut oracle_map: HashMap<AccountId, i64> = HashMap::new();
            let oracle = get_invalid_hashed_core_serial(
                &hashed,
                &ctx,
                &bounds,
                Some(&fees),
                Some(&accounts),
                &mut oracle_map,
            );
            let oracle_hashes: Vec<Hash256> = oracle.iter().map(|h| h.hash()).collect();

            let mut pub_map: HashMap<AccountId, i64> = HashMap::new();
            let actual = get_invalid_hashed_core(
                &hashed,
                &ctx,
                &bounds,
                Some(&fees),
                Some(&accounts),
                &mut pub_map,
            );
            let actual_hashes: Vec<Hash256> = actual.iter().map(|h| h.hash()).collect();

            assert_eq!(
                actual_hashes, oracle_hashes,
                "n={n}: public core must match serial oracle"
            );
            assert_eq!(
                pub_map, oracle_map,
                "n={n}: public core fee map must match serial oracle"
            );
        }
    }

    /// Cross-phase fee map carry-over (V26+) must be preserved by the parallel
    /// path: a pre-populated fee map entry must be added to, not overwritten.
    #[test]
    fn test_parallel_preserves_prepopulated_fee_map() {
        let ctx = test_context();
        let bounds = CloseTimeBounds::exact();
        let (hashed, accounts, fees) = make_parity_fixture(64);

        let prior_key = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0xC0; 32])));

        let mut serial_map: HashMap<AccountId, i64> = HashMap::new();
        serial_map.insert(prior_key.clone(), 777);
        let _ = get_invalid_hashed_core_serial(
            &hashed,
            &ctx,
            &bounds,
            Some(&fees),
            Some(&accounts),
            &mut serial_map,
        );

        let mut par_map: HashMap<AccountId, i64> = HashMap::new();
        par_map.insert(prior_key.clone(), 777);
        let _ = get_invalid_hashed_core_parallel(
            &hashed,
            &ctx,
            &bounds,
            Some(&fees),
            Some(&accounts),
            &mut par_map,
        );

        assert_eq!(
            par_map, serial_map,
            "parallel path must preserve and extend a pre-populated fee map identically to serial"
        );
        assert_eq!(par_map.get(&prior_key).copied(), Some(777));
    }
}
