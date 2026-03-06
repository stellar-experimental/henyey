//! Mapping from execution failures to XDR transaction result codes.
//!
//! Converts internal `ExecutionFailure` variants into their corresponding
//! `TransactionResultResult` XDR codes for inclusion in ledger close metadata.

use super::*;

pub(super) fn map_failure_to_result(failure: &ExecutionFailure) -> TransactionResultResult {
    match failure {
        ExecutionFailure::Malformed => TransactionResultResult::TxMalformed,
        ExecutionFailure::MissingOperation => TransactionResultResult::TxMissingOperation,
        ExecutionFailure::InvalidSignature => TransactionResultResult::TxBadAuth,
        ExecutionFailure::BadAuthExtra => TransactionResultResult::TxBadAuthExtra,
        ExecutionFailure::BadMinSeqAgeOrGap => TransactionResultResult::TxBadMinSeqAgeOrGap,
        ExecutionFailure::TooEarly => TransactionResultResult::TxTooEarly,
        ExecutionFailure::TooLate => TransactionResultResult::TxTooLate,
        ExecutionFailure::BadSequence => TransactionResultResult::TxBadSeq,
        ExecutionFailure::InsufficientFee => TransactionResultResult::TxInsufficientFee,
        ExecutionFailure::InsufficientBalance => TransactionResultResult::TxInsufficientBalance,
        ExecutionFailure::NoAccount => TransactionResultResult::TxNoAccount,
        ExecutionFailure::NotSupported => TransactionResultResult::TxNotSupported,
        ExecutionFailure::InternalError => TransactionResultResult::TxInternalError,
        ExecutionFailure::BadSponsorship => TransactionResultResult::TxBadSponsorship,
        ExecutionFailure::OperationFailed => {
            TransactionResultResult::TxFailed(Vec::new().try_into().unwrap())
        }
    }
}

pub(super) fn insufficient_refundable_fee_result(op: &Operation) -> OperationResult {
    match &op.body {
        OperationBody::InvokeHostFunction(_) => {
            OperationResult::OpInner(OperationResultTr::InvokeHostFunction(
                stellar_xdr::curr::InvokeHostFunctionResult::InsufficientRefundableFee,
            ))
        }
        OperationBody::ExtendFootprintTtl(_) => {
            OperationResult::OpInner(OperationResultTr::ExtendFootprintTtl(
                stellar_xdr::curr::ExtendFootprintTtlResult::InsufficientRefundableFee,
            ))
        }
        OperationBody::RestoreFootprint(_) => {
            OperationResult::OpInner(OperationResultTr::RestoreFootprint(
                stellar_xdr::curr::RestoreFootprintResult::InsufficientRefundableFee,
            ))
        }
        _ => OperationResult::OpNotSupported,
    }
}

pub(super) fn map_failure_to_inner_result(
    failure: &ExecutionFailure,
    op_results: &[OperationResult],
) -> InnerTransactionResultResult {
    match failure {
        ExecutionFailure::Malformed => InnerTransactionResultResult::TxMalformed,
        ExecutionFailure::MissingOperation => InnerTransactionResultResult::TxMissingOperation,
        ExecutionFailure::InvalidSignature => InnerTransactionResultResult::TxBadAuth,
        ExecutionFailure::BadAuthExtra => InnerTransactionResultResult::TxBadAuthExtra,
        ExecutionFailure::BadMinSeqAgeOrGap => InnerTransactionResultResult::TxBadMinSeqAgeOrGap,
        ExecutionFailure::TooEarly => InnerTransactionResultResult::TxTooEarly,
        ExecutionFailure::TooLate => InnerTransactionResultResult::TxTooLate,
        ExecutionFailure::BadSequence => InnerTransactionResultResult::TxBadSeq,
        ExecutionFailure::InsufficientFee => InnerTransactionResultResult::TxInsufficientFee,
        ExecutionFailure::InsufficientBalance => {
            InnerTransactionResultResult::TxInsufficientBalance
        }
        ExecutionFailure::NoAccount => InnerTransactionResultResult::TxNoAccount,
        ExecutionFailure::NotSupported => InnerTransactionResultResult::TxNotSupported,
        ExecutionFailure::InternalError => InnerTransactionResultResult::TxInternalError,
        ExecutionFailure::BadSponsorship => InnerTransactionResultResult::TxBadSponsorship,
        ExecutionFailure::OperationFailed => InnerTransactionResultResult::TxFailed(
            op_results.to_vec().try_into().unwrap_or_default(),
        ),
    }
}

pub fn build_tx_result_pair(
    frame: &TransactionFrame,
    network_id: &NetworkId,
    exec: &TransactionExecutionResult,
    base_fee: i64,
    protocol_version: u32,
) -> Result<TransactionResultPair> {
    let tx_hash = frame
        .hash(network_id)
        .map_err(|e| LedgerError::Internal(format!("tx hash error: {}", e)))?;
    let op_results: Vec<OperationResult> = exec.operation_results.clone();

    let result = if frame.is_fee_bump() {
        let inner_hash = fee_bump_inner_hash(frame, network_id)?;
        let inner_result = if exec.success {
            InnerTransactionResultResult::TxSuccess(
                op_results.clone().try_into().unwrap_or_default(),
            )
        } else if let Some(failure) = &exec.failure {
            map_failure_to_inner_result(failure, &op_results)
        } else {
            InnerTransactionResultResult::TxFailed(
                op_results.clone().try_into().unwrap_or_default(),
            )
        };

        // Calculate inner fee_charged using stellar-core formula:
        // Protocol >= 25: 0 (outer pays everything)
        // Protocol < 25 and protocol >= 11:
        //   - For Soroban: resourceFee + min(inclusionFee, baseFee * numOps) - refund
        //     (stellar-core had a bug where refund was applied to inner fee; this was fixed in p25)
        //   - For classic: min(inner_fee, baseFee * numOps)
        let inner_fee_charged = if protocol_version >= 25 {
            0
        } else {
            let num_inner_ops = frame.operation_count() as i64;
            let adjusted_fee = base_fee * std::cmp::max(1, num_inner_ops);
            if frame.is_soroban() {
                // For Soroban transactions, include the declared resource fee
                let resource_fee = frame.declared_soroban_resource_fee();
                let inner_fee = frame.inner_fee() as i64;
                let inclusion_fee = inner_fee - resource_fee;
                let computed_fee = resource_fee + std::cmp::min(inclusion_fee, adjusted_fee);
                // Prior to protocol 25, stellar-core incorrectly applied the refund to the inner
                // feeCharged field for fee bump transactions. We replicate this behavior
                // for compatibility.
                computed_fee.saturating_sub(exec.fee_refund)
            } else {
                // For classic transactions
                std::cmp::min(frame.inner_fee() as i64, adjusted_fee)
            }
        };

        let inner_pair = InnerTransactionResultPair {
            transaction_hash: stellar_xdr::curr::Hash(inner_hash.0),
            result: InnerTransactionResult {
                fee_charged: inner_fee_charged,
                result: inner_result,
                ext: InnerTransactionResultExt::V0,
            },
        };

        let result = if exec.success {
            TransactionResultResult::TxFeeBumpInnerSuccess(inner_pair)
        } else {
            TransactionResultResult::TxFeeBumpInnerFailed(inner_pair)
        };

        TransactionResult {
            fee_charged: exec.fee_charged,
            result,
            ext: TransactionResultExt::V0,
        }
    } else if exec.success {
        TransactionResult {
            fee_charged: exec.fee_charged,
            result: TransactionResultResult::TxSuccess(op_results.try_into().unwrap_or_default()),
            ext: TransactionResultExt::V0,
        }
    } else if let Some(failure) = &exec.failure {
        let result = match failure {
            ExecutionFailure::OperationFailed => {
                TransactionResultResult::TxFailed(op_results.try_into().unwrap_or_default())
            }
            _ => map_failure_to_result(failure),
        };
        TransactionResult {
            fee_charged: exec.fee_charged,
            result,
            ext: TransactionResultExt::V0,
        }
    } else {
        TransactionResult {
            fee_charged: exec.fee_charged,
            result: TransactionResultResult::TxFailed(op_results.try_into().unwrap_or_default()),
            ext: TransactionResultExt::V0,
        }
    };

    Ok(TransactionResultPair {
        transaction_hash: stellar_xdr::curr::Hash(tx_hash.0),
        result,
    })
}
