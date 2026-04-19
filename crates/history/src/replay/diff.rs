//! Diff and comparison utilities for replay verification.
//!
//! Functions for comparing expected vs actual transaction results and
//! logging detailed mismatch information for debugging.

use henyey_common::Hash256;
use stellar_xdr::curr::{LedgerHeader, TransactionEnvelope, TransactionResultPair};

/// Log detailed information about transaction result mismatches.
///
/// Called when result verification fails to help diagnose which transaction
/// produced different results during re-execution.
pub(super) fn log_tx_result_mismatch(
    header: &LedgerHeader,
    expected: &[TransactionResultPair],
    actual: &[TransactionResultPair],
    transactions: &[(std::sync::Arc<TransactionEnvelope>, Option<u32>)],
) {
    use tracing::warn;

    if expected.len() != actual.len() {
        warn!(
            ledger_seq = header.ledger_seq,
            expected_len = expected.len(),
            actual_len = actual.len(),
            "Transaction result count mismatch"
        );
    }

    let limit = expected.len().min(actual.len());
    for (idx, (expected_item, actual_item)) in
        expected.iter().zip(actual.iter()).take(limit).enumerate()
    {
        let expected_hash = Hash256::hash_xdr(expected_item);
        let actual_hash = Hash256::hash_xdr(actual_item);
        if expected_hash != actual_hash {
            let expected_tx_hash = Hash256::from(expected_item.transaction_hash.0).to_hex();
            let actual_tx_hash = Hash256::from(actual_item.transaction_hash.0).to_hex();
            let expected_code = format!("{:?}", expected_item.result.result);
            let actual_code = format!("{:?}", actual_item.result.result);
            let expected_fee = expected_item.result.fee_charged;
            let actual_fee = actual_item.result.fee_charged;
            let expected_ext = format!("{:?}", expected_item.result.ext);
            let actual_ext = format!("{:?}", actual_item.result.ext);
            let op_summaries = transactions
                .get(idx)
                .map(|(tx, _)| summarize_operations(tx))
                .unwrap_or_default();
            warn!(
                ledger_seq = header.ledger_seq,
                index = idx,
                expected_tx_hash = %expected_tx_hash,
                actual_tx_hash = %actual_tx_hash,
                expected_fee = %expected_fee,
                actual_fee = %actual_fee,
                expected_ext = %expected_ext,
                actual_ext = %actual_ext,
                expected_code = %expected_code,
                actual_code = %actual_code,
                expected_hash = %expected_hash.to_hex(),
                actual_hash = %actual_hash.to_hex(),
                operations = ?op_summaries,
                "Transaction result mismatch"
            );
            break;
        }
    }
}

fn summarize_operations(tx: &TransactionEnvelope) -> Vec<String> {
    let ops = match tx {
        TransactionEnvelope::TxV0(env) => env.tx.operations.as_slice(),
        TransactionEnvelope::Tx(env) => env.tx.operations.as_slice(),
        TransactionEnvelope::TxFeeBump(env) => match &env.tx.inner_tx {
            stellar_xdr::curr::FeeBumpTransactionInnerTx::Tx(inner) => {
                inner.tx.operations.as_slice()
            }
        },
    };

    ops.iter()
        .map(|op| {
            let source = op.source_account.as_ref().map(|a| format!("{:?}", a));
            let body = format!("{:?}", op.body);
            format!("source={:?} body={}", source, body)
        })
        .collect()
}
