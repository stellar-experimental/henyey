//! Transaction precondition validation.
//!
//! Validates a transaction's structure, accounts, fees, preconditions, sequence,
//! and signatures before any state changes. Extracted from the main executor module
//! for readability.

use std::sync::Arc;

use henyey_tx::{
    state::{get_account_seq_ledger, get_account_seq_time},
    validation::{self, LedgerContext as ValidationContext},
    TransactionFrame,
};
use stellar_xdr::curr::{Preconditions, TransactionEnvelope, TransactionResultCode};

use crate::snapshot::SnapshotHandle;
use crate::{LedgerError, Result};

use super::signatures::*;
use super::{failed_result, TransactionExecutor, ValidatedTransaction, ValidationFailure};

impl TransactionExecutor {
    /// Validate a transaction's structure, accounts, fees, preconditions, sequence,
    /// and signatures before any state changes. Returns the validated data needed
    /// for execution, or a `ValidationFailure` on validation failure.
    pub(super) fn validate_preconditions(
        &mut self,
        snapshot: &SnapshotHandle,
        tx_envelope: &Arc<TransactionEnvelope>,
        base_fee: u32,
    ) -> Result<std::result::Result<ValidatedTransaction, ValidationFailure>> {
        let val_start = std::time::Instant::now();
        let frame = TransactionFrame::with_network(Arc::clone(tx_envelope), self.network_id);
        let fee_source_id = henyey_tx::muxed_to_account_id(&frame.fee_source_account());
        let inner_source_id = henyey_tx::muxed_to_account_id(&frame.inner_source_account());

        // Helper to create a pre-seq-check failure (no sequence bump needed).
        let pre_seq_fail = |failure, error| ValidationFailure {
            result: failed_result(failure, error),
            past_seq_check: false,
        };
        // Helper to create a post-seq-check failure (sequence bump needed).
        let post_seq_fail = |failure, error| ValidationFailure {
            result: failed_result(failure, error),
            past_seq_check: true,
        };
        // Helper for fee-bump outer-wrapper failures. In stellar-core, these are
        // emitted via setError() (not setInnermostError()), producing a top-level
        // result code without an InnerTransactionResultPair wrapper.
        let is_fee_bump = frame.is_fee_bump();
        let fee_bump_outer_fail = |failure, error| {
            let mut result = failed_result(failure, error);
            result.fee_bump_outer_failure = true;
            ValidationFailure {
                result,
                past_seq_check: false,
            }
        };

        // Phase 1: Shared stateless structural validation
        // Mirrors the stateless subset of stellar-core's commonValidPreSeqNum.
        // Called by both queue admission and preconditions.
        if let Err(e) = henyey_tx::check_valid_pre_seq_num_with_config(
            &frame,
            self.protocol_version,
            self.ledger_flags,
            Some(self.soroban_config.max_contract_size_bytes),
        ) {
            let code = e.to_tx_result_code();
            return Ok(Err(if is_fee_bump {
                fee_bump_outer_fail(code, &e.to_string())
            } else {
                pre_seq_fail(code, &e.to_string())
            }));
        }

        // Phase 2: Account loading
        // Fee source account not found is an outer failure for fee-bump (stellar-core's
        // FeeBumpTransactionFrame::commonValidPreSeqNum → setError(txNO_ACCOUNT)).
        // Inner source account not found is an inner failure (stellar-core's
        // TransactionFrame::commonValid → setInnermostError via checkValidWithOptionallyChargedFee).
        let acct_load_start = std::time::Instant::now();
        if !self.load_account(snapshot, &fee_source_id)? {
            return Ok(Err(if is_fee_bump {
                fee_bump_outer_fail(
                    TransactionResultCode::TxNoAccount,
                    "Fee source account not found",
                )
            } else {
                pre_seq_fail(
                    TransactionResultCode::TxNoAccount,
                    "Source account not found",
                )
            }));
        }
        if !self.load_account(snapshot, &inner_source_id)? {
            return Ok(Err(pre_seq_fail(
                TransactionResultCode::TxNoAccount,
                "Source account not found",
            )));
        }

        let fee_source_account = match self.state.get_account(&fee_source_id) {
            Some(acc) => acc.clone(),
            None => {
                return Ok(Err(if is_fee_bump {
                    fee_bump_outer_fail(
                        TransactionResultCode::TxNoAccount,
                        "Fee source account not found",
                    )
                } else {
                    pre_seq_fail(
                        TransactionResultCode::TxNoAccount,
                        "Source account not found",
                    )
                }))
            }
        };
        let source_account = match self.state.get_account(&inner_source_id) {
            Some(acc) => acc.clone(),
            None => {
                return Ok(Err(pre_seq_fail(
                    TransactionResultCode::TxNoAccount,
                    "Source account not found",
                )))
            }
        };
        let val_account_load_us = acct_load_start.elapsed().as_micros() as u64;

        // Phase 3: Fee validation
        // SECURITY: fee computation overflow prevented by tx validation bounds (max_fee * max_ops fits i64)
        if frame.is_fee_bump() {
            let outer_min_inclusion_fee = frame.min_inclusion_fee(base_fee as i64);
            let outer_inclusion_fee = frame.inclusion_fee();

            if outer_inclusion_fee < outer_min_inclusion_fee {
                return Ok(Err(fee_bump_outer_fail(
                    TransactionResultCode::TxInsufficientFee,
                    "Insufficient fee",
                )));
            }

            let (inner_inclusion_fee, inner_min_inclusion_fee, inner_is_soroban) = match frame
                .envelope()
            {
                TransactionEnvelope::TxFeeBump(env) => match &env.tx.inner_tx {
                    stellar_xdr::curr::FeeBumpTransactionInnerTx::Tx(inner) => {
                        let inner_env = TransactionEnvelope::Tx(inner.clone());
                        let inner_frame =
                            TransactionFrame::from_owned_with_network(inner_env, self.network_id);
                        (
                            inner_frame.inclusion_fee(),
                            inner_frame.min_inclusion_fee(base_fee as i64),
                            inner_frame.is_soroban(),
                        )
                    }
                },
                _ => (0, base_fee as i64, false),
            };

            if inner_inclusion_fee >= 0 {
                let v1 = outer_inclusion_fee as i128 * inner_min_inclusion_fee as i128;
                let v2 = inner_inclusion_fee as i128 * outer_min_inclusion_fee as i128;
                if v1 < v2 {
                    return Ok(Err(fee_bump_outer_fail(
                        TransactionResultCode::TxInsufficientFee,
                        "Insufficient fee",
                    )));
                }
            } else {
                let allow_negative_inner = inner_is_soroban;
                if !allow_negative_inner {
                    return Ok(Err(fee_bump_outer_fail(
                        TransactionResultCode::TxFailed,
                        "Fee bump inner transaction invalid",
                    )));
                }
            }
        } else {
            let required_fee = frame.operation_count() as u32 * base_fee;
            if frame.fee() < required_fee {
                return Ok(Err(pre_seq_fail(
                    TransactionResultCode::TxInsufficientFee,
                    "Insufficient fee",
                )));
            }

            // Validate that Soroban resource fee does not exceed the full transaction fee.
            // Parity: stellar-core TransactionFrame.cpp commonValidPreSeqNum —
            // for p23+ non-fee-bump txs, rejects when sorobanData.resourceFee > getFullFee().
            // Fee-bump inner txs skip this check (handled by the fee-bump branch above).
            if frame.is_soroban() && frame.declared_soroban_resource_fee() > frame.total_fee() {
                return Ok(Err(pre_seq_fail(
                    TransactionResultCode::TxSorobanInvalid,
                    "Soroban resource fee exceeds full transaction fee",
                )));
            }
        }

        // Phase 4: Time/ledger bounds and precondition validation
        let validation_ctx = ValidationContext::new(
            self.ledger_seq,
            self.close_time,
            base_fee,
            self.base_reserve,
            self.protocol_version,
            self.network_id,
        );

        if let Err(e) = validation::validate_time_bounds(&frame, &validation_ctx) {
            return Ok(Err(pre_seq_fail(
                match e {
                    validation::ValidationError::TooEarly { .. } => {
                        TransactionResultCode::TxTooEarly
                    }
                    validation::ValidationError::TooLate { .. } => TransactionResultCode::TxTooLate,
                    _ => TransactionResultCode::TxFailed,
                },
                "Time bounds invalid",
            )));
        }

        if let Err(e) = validation::validate_ledger_bounds(&frame, &validation_ctx) {
            return Ok(Err(pre_seq_fail(
                match e {
                    validation::ValidationError::LedgerBoundsTooEarly { .. } => {
                        TransactionResultCode::TxTooEarly
                    }
                    validation::ValidationError::LedgerBoundsTooLate { .. } => {
                        TransactionResultCode::TxTooLate
                    }
                    _ => TransactionResultCode::TxFailed,
                },
                "Ledger bounds invalid",
            )));
        }

        // Phase 5: Sequence number validation
        // This combines stellar-core's isBadSeq (including min_seq_num) check.
        if self.ledger_seq <= i32::MAX as u32 {
            let starting_seq = (self.ledger_seq as i64) << 32;
            if frame.sequence_number() == starting_seq {
                return Ok(Err(pre_seq_fail(
                    TransactionResultCode::TxBadSeq,
                    "Bad sequence: equals starting sequence",
                )));
            }
        }

        let min_seq_num = match frame.preconditions() {
            Preconditions::V2(cond) => cond.min_seq_num.map(|s| s.0),
            _ => None,
        };

        let account_seq = source_account.seq_num.0;
        let tx_seq = frame.sequence_number();

        tracing::debug!(
            account_seq,
            tx_seq,
            min_seq_num = ?min_seq_num,
            preconditions_type = ?std::mem::discriminant(&frame.preconditions()),
            "Sequence number validation"
        );

        let is_bad_seq = if let Some(min_seq) = min_seq_num {
            account_seq < min_seq || account_seq >= tx_seq
        } else {
            account_seq == i64::MAX || account_seq + 1 != tx_seq
        };

        if is_bad_seq {
            let error_msg = if let Some(min_seq) = min_seq_num {
                format!(
                    "Bad sequence: account seq {} not in valid range [minSeqNum={}, txSeq={})",
                    account_seq, min_seq, tx_seq
                )
            } else {
                format!(
                    "Bad sequence: expected {}, got {}",
                    account_seq.saturating_add(1),
                    tx_seq
                )
            };
            return Ok(Err(pre_seq_fail(
                TransactionResultCode::TxBadSeq,
                &error_msg,
            )));
        }

        // --- Past this point, the sequence check has passed ---
        // In stellar-core's commonValid, res = kInvalidUpdateSeqNum here.
        // Failures after this point should still bump the sequence number.

        // Phase 5b: Min seq age/gap checks (stellar-core's isTooEarlyForAccount)
        if let Preconditions::V2(cond) = frame.preconditions() {
            if cond.min_seq_age.0 > 0 {
                let acc_seq_time = get_account_seq_time(&source_account);
                let min_seq_age = cond.min_seq_age.0;
                if min_seq_age > self.close_time || self.close_time - min_seq_age < acc_seq_time {
                    return Ok(Err(post_seq_fail(
                        TransactionResultCode::TxBadMinSeqAgeOrGap,
                        "Minimum sequence age not met",
                    )));
                }
            }

            if cond.min_seq_ledger_gap > 0 {
                let acc_seq_ledger = get_account_seq_ledger(&source_account);
                let min_seq_ledger_gap = cond.min_seq_ledger_gap;
                if min_seq_ledger_gap > self.ledger_seq
                    || self.ledger_seq - min_seq_ledger_gap < acc_seq_ledger
                {
                    return Ok(Err(post_seq_fail(
                        TransactionResultCode::TxBadMinSeqAgeOrGap,
                        "Minimum sequence ledger gap not met",
                    )));
                }
            }
        }

        // Phase 6: Signature validation
        let sig_start = std::time::Instant::now();
        if validation::validate_signatures(&frame, &validation_ctx).is_err() {
            if is_fee_bump {
                let mut result =
                    failed_result(TransactionResultCode::TxBadAuth, "Invalid signature");
                result.fee_bump_outer_failure = true;
                return Ok(Err(ValidationFailure {
                    result,
                    past_seq_check: true,
                }));
            } else {
                return Ok(Err(post_seq_fail(
                    TransactionResultCode::TxBadAuth,
                    "Invalid signature",
                )));
            }
        }

        let hash_start = std::time::Instant::now();
        let outer_hash = frame
            .hash(&self.network_id)
            .map_err(|e| LedgerError::Internal(format!("tx hash error: {}", e)))?;
        let val_tx_hash_us = hash_start.elapsed().as_micros() as u64;

        let ed25519_start = std::time::Instant::now();
        let outer_threshold = threshold_low(&fee_source_account);
        if !has_sufficient_signer_weight(
            &outer_hash,
            frame.signatures(),
            &fee_source_account,
            outer_threshold,
        ) {
            tracing::debug!("Signature check failed: fee_source outer check");
            if is_fee_bump {
                // Fee-bump outer signature failure: stellar-core's
                // FeeBumpTransactionFrame::commonValid → setError(txBAD_AUTH).
                let mut result =
                    failed_result(TransactionResultCode::TxBadAuth, "Invalid signature");
                result.fee_bump_outer_failure = true;
                return Ok(Err(ValidationFailure {
                    result,
                    past_seq_check: true,
                }));
            } else {
                return Ok(Err(post_seq_fail(
                    TransactionResultCode::TxBadAuth,
                    "Invalid signature",
                )));
            }
        }

        // NOTE: For fee-bump transactions, we deliberately do NOT check the inner
        // transaction's signatures here. In stellar-core, fee is charged by
        // processFeeSeqNum() BEFORE apply() re-validates inner signatures. If a
        // prior transaction in the same ledger modifies the inner source's signer
        // set, the inner sig check must fail at apply-time (after fee charging),
        // not here. The check_operation_signatures call in execute_transaction_with_fee_mode
        // handles inner sig validation after the fee has been deducted.

        // For non-fee-bump TXs, the fee source IS the inner source. When they're
        // the same account, the second weight check is identical to the first (same
        // account, same threshold_low, same signatures, same hash). Skip it to avoid
        // a redundant sig cache lookup (~5µs/TX × 12,500 TXs = ~62ms/cluster).
        let required_weight = threshold_low(&source_account);
        if !frame.is_fee_bump()
            && fee_source_id != inner_source_id
            && !has_sufficient_signer_weight(
                &outer_hash,
                frame.signatures(),
                &source_account,
                required_weight,
            )
        {
            tracing::debug!(
                required_weight = required_weight,
                is_fee_bump = frame.is_fee_bump(),
                master_weight = source_account.thresholds.0[0],
                num_signers = source_account.signers.len(),
                thresholds = ?source_account.thresholds.0,
                "Signature check failed: source outer check"
            );
            return Ok(Err(post_seq_fail(
                TransactionResultCode::TxBadAuth,
                "Invalid signature",
            )));
        }

        if let Preconditions::V2(cond) = frame.preconditions() {
            if !cond.extra_signers.is_empty() {
                let extra_hash = if frame.is_fee_bump() {
                    fee_bump_inner_hash(&frame, &self.network_id)?
                } else {
                    outer_hash
                };
                let extra_signatures = if frame.is_fee_bump() {
                    frame.inner_signatures()
                } else {
                    frame.signatures()
                };
                if !has_required_extra_signers(&extra_hash, extra_signatures, &cond.extra_signers) {
                    return Ok(Err(post_seq_fail(
                        TransactionResultCode::TxBadAuthExtra,
                        "Missing extra signer",
                    )));
                }
            }
        }

        // CAP-77: Frozen ledger key checks.
        // Fee bump: gate on SOROBAN_PROTOCOL_VERSION (V20) since frozen keys
        // are stored in the Soroban network config.
        // Parity: FeeBumpTransactionFrame::checkValid:300 gates on
        //         SOROBAN_PROTOCOL_VERSION, not V26.
        if is_fee_bump
            && henyey_common::protocol::protocol_version_starts_from(
                self.protocol_version,
                henyey_common::protocol::ProtocolVersion::V20,
            )
            && self.frozen_key_config.has_frozen_keys()
            && self
                .frozen_key_config
                .is_key_frozen(&henyey_tx::frozen_keys::account_key(&fee_source_id))
            && !self.frozen_key_config.is_freeze_bypass_tx(&outer_hash)
        {
            return Ok(Err(fee_bump_outer_fail(
                TransactionResultCode::TxFrozenKeyAccessed,
                "Fee bump source account accesses frozen ledger key",
            )));
        }

        // Inner TX: check source account, Soroban footprint, and operations.
        // No protocol version gate — relies on has_frozen_keys() being false
        // for protocols that don't support frozen keys.
        // Parity: TransactionFrame::commonValidPreSeqNum:1554 has no
        //         protocol gate, only checks if cfg is present.
        if self.frozen_key_config.has_frozen_keys() {
            let soroban_footprint = frame.soroban_data().map(|d| &d.resources.footprint);
            if henyey_tx::frozen_keys::accesses_frozen_key(
                &frame.inner_source_account_id(),
                frame.operations(),
                soroban_footprint,
                &self.frozen_key_config,
            ) && !self.frozen_key_config.is_freeze_bypass_tx(&outer_hash)
            {
                return Ok(Err(post_seq_fail(
                    TransactionResultCode::TxFrozenKeyAccessed,
                    "Transaction accesses frozen ledger key",
                )));
            }
        }

        let val_ed25519_us = ed25519_start.elapsed().as_micros() as u64;
        let val_sig_total_us = sig_start.elapsed().as_micros() as u64;
        let val_total_us = val_start.elapsed().as_micros() as u64;
        let val_other_us = val_total_us.saturating_sub(val_account_load_us + val_sig_total_us);

        Ok(Ok(ValidatedTransaction {
            frame,
            fee_source_id,
            inner_source_id,
            outer_hash,
            val_account_load_us,
            val_tx_hash_us,
            val_ed25519_us,
            val_other_us,
        }))
    }
}
