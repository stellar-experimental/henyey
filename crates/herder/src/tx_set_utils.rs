//! Transaction set utility functions.
//!
//! This module provides utility functions for filtering invalid transactions
//! from candidate transaction sets. These are used during nomination (to build
//! valid transaction sets) and post-ledger-close (to ban transactions that
//! became invalid).
//!
//! # Parity
//!
//! Mirrors `TxSetUtils::getInvalidTxList()` and `TxSetUtils::trimInvalid()`
//! from stellar-core `src/herder/TxSetUtils.cpp`.

use std::collections::{HashMap, HashSet};

use henyey_common::{Hash256, NetworkId};
use henyey_tx::{validate_basic, LedgerContext, TransactionFrame};
use stellar_xdr::curr::{AccountId, TransactionEnvelope};

use crate::tx_queue::FeeBalanceProvider;

/// Parameters for close-time bounds validation.
///
/// In upstream stellar-core, `lowerBoundCloseTimeOffset` and
/// `upperBoundCloseTimeOffset` are used to create a range of possible close
/// times during nomination (since the exact close time is not yet known).
/// The lower bound is `closeTime + lowerOffset` and the upper bound is
/// `closeTime + upperOffset`.
///
/// For post-ledger-close validation (where the close time is known exactly),
/// both offsets should be 0.
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
    pub fn new(
        last_closed_ledger_seq: u32,
        close_time: u64,
        base_fee: u32,
        base_reserve: u32,
        protocol_version: u32,
        network_id: NetworkId,
    ) -> Self {
        Self {
            next_ledger_seq: last_closed_ledger_seq.saturating_add(1),
            close_time,
            base_fee,
            base_reserve,
            protocol_version,
            network_id,
        }
    }

    /// Build a `LedgerContext` from this validation context.
    fn to_ledger_context(&self, close_time: u64) -> LedgerContext {
        LedgerContext::new(
            self.next_ledger_seq,
            close_time,
            self.base_fee,
            self.base_reserve,
            self.protocol_version,
            self.network_id,
        )
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

    // --- Pass 1: per-transaction basic validation ---
    // Also accumulate fees per fee source for valid transactions.
    let mut account_fee_map: HashMap<AccountId, i64> = HashMap::new();

    for tx in txs {
        let frame = TransactionFrame::from_owned_with_network(tx.clone(), ctx.network_id);

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
        let ctx = TxSetValidationContext::new(100, 1000, 100, 5_000_000, 21, NetworkId::testnet());
        assert_eq!(ctx.next_ledger_seq, 101, "next ledger should be LCL + 1");
    }

    #[test]
    fn test_validation_context_saturating_add() {
        // Edge case: LCL at u32::MAX should not overflow
        let ctx =
            TxSetValidationContext::new(u32::MAX, 1000, 100, 5_000_000, 21, NetworkId::testnet());
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
        let invalid = get_invalid_tx_list(&[envelope.clone()], &ctx, &bounds_exact, None);
        assert!(
            invalid.is_empty(),
            "tx should be valid with exact close time"
        );

        // With upper offset of 10 (close_time + 10 = 1010 > max_time 1005), should be invalid
        let bounds_offset = CloseTimeBounds::with_offsets(0, 10);
        let invalid = get_invalid_tx_list(&[envelope.clone()], &ctx, &bounds_offset, None);
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
}
