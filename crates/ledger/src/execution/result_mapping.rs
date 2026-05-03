//! Mapping from transaction result codes to XDR transaction result types.
//!
//! Converts `TransactionResultCode` values into their corresponding
//! `TransactionResultResult` XDR types for inclusion in ledger close metadata.

use super::*;

pub(super) fn failure_code_to_result(code: &TransactionResultCode) -> TransactionResultResult {
    match code {
        TransactionResultCode::TxMalformed => TransactionResultResult::TxMalformed,
        TransactionResultCode::TxMissingOperation => TransactionResultResult::TxMissingOperation,
        TransactionResultCode::TxBadAuth => TransactionResultResult::TxBadAuth,
        TransactionResultCode::TxBadAuthExtra => TransactionResultResult::TxBadAuthExtra,
        TransactionResultCode::TxBadMinSeqAgeOrGap => TransactionResultResult::TxBadMinSeqAgeOrGap,
        TransactionResultCode::TxTooEarly => TransactionResultResult::TxTooEarly,
        TransactionResultCode::TxTooLate => TransactionResultResult::TxTooLate,
        TransactionResultCode::TxBadSeq => TransactionResultResult::TxBadSeq,
        TransactionResultCode::TxInsufficientFee => TransactionResultResult::TxInsufficientFee,
        TransactionResultCode::TxInsufficientBalance => {
            TransactionResultResult::TxInsufficientBalance
        }
        TransactionResultCode::TxNoAccount => TransactionResultResult::TxNoAccount,
        TransactionResultCode::TxNotSupported => TransactionResultResult::TxNotSupported,
        TransactionResultCode::TxInternalError => TransactionResultResult::TxInternalError,
        TransactionResultCode::TxBadSponsorship => TransactionResultResult::TxBadSponsorship,
        TransactionResultCode::TxSorobanInvalid => TransactionResultResult::TxSorobanInvalid,
        TransactionResultCode::TxFrozenKeyAccessed => TransactionResultResult::TxFrozenKeyAccessed,
        // TxFailed, TxSuccess, TxFeeBumpInnerSuccess, TxFeeBumpInnerFailed carry payloads
        // and are handled specially by build_tx_result_pair.
        TransactionResultCode::TxFailed
        | TransactionResultCode::TxSuccess
        | TransactionResultCode::TxFeeBumpInnerSuccess
        | TransactionResultCode::TxFeeBumpInnerFailed => {
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

pub(super) fn failure_code_to_inner_result(
    code: &TransactionResultCode,
    op_results: &[OperationResult],
) -> InnerTransactionResultResult {
    match code {
        TransactionResultCode::TxMalformed => InnerTransactionResultResult::TxMalformed,
        TransactionResultCode::TxMissingOperation => {
            InnerTransactionResultResult::TxMissingOperation
        }
        TransactionResultCode::TxBadAuth => InnerTransactionResultResult::TxBadAuth,
        TransactionResultCode::TxBadAuthExtra => InnerTransactionResultResult::TxBadAuthExtra,
        TransactionResultCode::TxBadMinSeqAgeOrGap => {
            InnerTransactionResultResult::TxBadMinSeqAgeOrGap
        }
        TransactionResultCode::TxTooEarly => InnerTransactionResultResult::TxTooEarly,
        TransactionResultCode::TxTooLate => InnerTransactionResultResult::TxTooLate,
        TransactionResultCode::TxBadSeq => InnerTransactionResultResult::TxBadSeq,
        TransactionResultCode::TxInsufficientFee => InnerTransactionResultResult::TxInsufficientFee,
        TransactionResultCode::TxInsufficientBalance => {
            InnerTransactionResultResult::TxInsufficientBalance
        }
        TransactionResultCode::TxNoAccount => InnerTransactionResultResult::TxNoAccount,
        TransactionResultCode::TxNotSupported => InnerTransactionResultResult::TxNotSupported,
        TransactionResultCode::TxInternalError => InnerTransactionResultResult::TxInternalError,
        TransactionResultCode::TxBadSponsorship => InnerTransactionResultResult::TxBadSponsorship,
        TransactionResultCode::TxSorobanInvalid => InnerTransactionResultResult::TxSorobanInvalid,
        TransactionResultCode::TxFrozenKeyAccessed => {
            InnerTransactionResultResult::TxFrozenKeyAccessed
        }
        // TxFailed and success/fee-bump codes carry payloads.
        TransactionResultCode::TxFailed
        | TransactionResultCode::TxSuccess
        | TransactionResultCode::TxFeeBumpInnerSuccess
        | TransactionResultCode::TxFeeBumpInnerFailed => InnerTransactionResultResult::TxFailed(
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
    // Reuse cached hash from execution when available, avoiding redundant XDR+SHA-256
    let tx_hash = if let Some(h) = exec.tx_hash {
        h
    } else {
        frame
            .hash(network_id)
            .map_err(|e| LedgerError::Internal(format!("tx hash error: {}", e)))?
    };
    let op_results: Vec<OperationResult> = exec.operation_results.clone();

    let result = if frame.is_fee_bump() && exec.fee_bump_outer_failure {
        // Fee-bump outer-wrapper failure: emit a top-level result code without
        // an InnerTransactionResultPair. Matches stellar-core's setError()
        // behavior in FeeBumpTransactionFrame::commonValid/commonValidPreSeqNum.
        let result = if let Some(failure) = &exec.failure {
            failure_code_to_result(failure)
        } else {
            TransactionResultResult::TxFailed(Vec::new().try_into().unwrap())
        };
        TransactionResult {
            fee_charged: exec.fee_charged,
            result,
            ext: TransactionResultExt::V0,
        }
    } else if frame.is_fee_bump() {
        // Fee-bump inner failure or success: wrap in InnerTransactionResultPair.
        let inner_hash = fee_bump_inner_hash(frame, network_id)?;
        let inner_result = if exec.success {
            InnerTransactionResultResult::TxSuccess(
                op_results.clone().try_into().unwrap_or_default(),
            )
        } else if let Some(failure) = &exec.failure {
            failure_code_to_inner_result(failure, &op_results)
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
        let inner_fee_charged = if !fee_bump_refund_applies_to_inner(protocol_version) {
            0
        } else {
            let num_inner_ops = frame.operation_count() as i64;
            let adjusted_fee = base_fee * std::cmp::max(1, num_inner_ops);
            if frame.is_soroban() {
                // For Soroban transactions, include the declared resource fee
                let resource_fee = frame.declared_soroban_resource_fee().as_i64();
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
            TransactionResultCode::TxFailed => {
                TransactionResultResult::TxFailed(op_results.try_into().unwrap_or_default())
            }
            _ => failure_code_to_result(failure),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_audit_573_soroban_invalid_maps_correctly() {
        // TxSorobanInvalid must map to TxSorobanInvalid, not TxNotSupported.
        // Using TxNotSupported would produce a different tx_set_result_hash
        // and cause consensus divergence.
        let result = failure_code_to_result(&TransactionResultCode::TxSorobanInvalid);
        assert!(
            matches!(result, TransactionResultResult::TxSorobanInvalid),
            "TxSorobanInvalid should map to TxSorobanInvalid, got {:?}",
            result
        );
    }

    #[test]
    fn test_audit_573_soroban_invalid_inner_maps_correctly() {
        let result = failure_code_to_inner_result(&TransactionResultCode::TxSorobanInvalid, &[]);
        assert!(
            matches!(result, InnerTransactionResultResult::TxSorobanInvalid),
            "TxSorobanInvalid inner should map to TxSorobanInvalid, got {:?}",
            result
        );
    }

    #[test]
    fn test_all_failure_codes_map_to_distinct_variants() {
        // Ensure no two distinct failure codes map to the same result variant.
        // This catches copy-paste errors where a new code is mapped to an existing variant.
        let codes = [
            TransactionResultCode::TxMalformed,
            TransactionResultCode::TxMissingOperation,
            TransactionResultCode::TxBadAuth,
            TransactionResultCode::TxBadAuthExtra,
            TransactionResultCode::TxBadMinSeqAgeOrGap,
            TransactionResultCode::TxTooEarly,
            TransactionResultCode::TxTooLate,
            TransactionResultCode::TxBadSeq,
            TransactionResultCode::TxInsufficientFee,
            TransactionResultCode::TxInsufficientBalance,
            TransactionResultCode::TxNoAccount,
            TransactionResultCode::TxNotSupported,
            TransactionResultCode::TxInternalError,
            TransactionResultCode::TxBadSponsorship,
            TransactionResultCode::TxSorobanInvalid,
            TransactionResultCode::TxFrozenKeyAccessed,
        ];

        for (i, code_a) in codes.iter().enumerate() {
            for code_b in codes.iter().skip(i + 1) {
                let result_a = failure_code_to_result(code_a);
                let result_b = failure_code_to_result(code_b);
                let disc_a = std::mem::discriminant(&result_a);
                let disc_b = std::mem::discriminant(&result_b);
                assert_ne!(
                    disc_a, disc_b,
                    "Distinct failure codes {:?} and {:?} map to the same result variant",
                    code_a, code_b
                );
            }
        }
    }

    #[test]
    fn test_all_inner_failure_codes_map_to_distinct_variants() {
        // Same structural check as above but for failure_code_to_inner_result.
        // Catches copy-paste errors in the inner result mapping.
        let codes = [
            TransactionResultCode::TxMalformed,
            TransactionResultCode::TxMissingOperation,
            TransactionResultCode::TxBadAuth,
            TransactionResultCode::TxBadAuthExtra,
            TransactionResultCode::TxBadMinSeqAgeOrGap,
            TransactionResultCode::TxTooEarly,
            TransactionResultCode::TxTooLate,
            TransactionResultCode::TxBadSeq,
            TransactionResultCode::TxInsufficientFee,
            TransactionResultCode::TxInsufficientBalance,
            TransactionResultCode::TxNoAccount,
            TransactionResultCode::TxNotSupported,
            TransactionResultCode::TxInternalError,
            TransactionResultCode::TxBadSponsorship,
            TransactionResultCode::TxSorobanInvalid,
            TransactionResultCode::TxFrozenKeyAccessed,
        ];

        for (i, code_a) in codes.iter().enumerate() {
            for code_b in codes.iter().skip(i + 1) {
                let result_a = failure_code_to_inner_result(code_a, &[]);
                let result_b = failure_code_to_inner_result(code_b, &[]);
                let disc_a = std::mem::discriminant(&result_a);
                let disc_b = std::mem::discriminant(&result_b);
                assert_ne!(
                    disc_a, disc_b,
                    "Distinct failure codes {:?} and {:?} map to the same inner result variant",
                    code_a, code_b
                );
            }
        }
    }
}
