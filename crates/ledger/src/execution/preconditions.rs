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
        // The fee that processFeeSeqNum will charge (or has charged) against the
        // fee source, used by the final affordability guard to evaluate
        // stellar-core's post-fee, applying=true predicate. The guard caps this
        // at the fee source's balance (mirroring `fee = min(balance, fee)`)
        // before subtracting, so the value passed here is the UNCAPPED fee:
        //   - Production tx-set path (`FeeMode::Skip`): `process_fee_only`
        //     already deducted the capped fee from `self.state`, so the
        //     fee-source balance read by the guard is already net — pass 0.
        //   - Single-phase `FeeMode::Deduct`: validation runs before the inline
        //     deduction, so the balance is pre-fee — pass `fee_to_charge`
        //     (the guard caps it against the balance it reads).
        fee_to_charge: i64,
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
            self.soroban_resource_limits.as_ref(),
        ) {
            let code = e.to_tx_result_code();
            return Ok(Err(if is_fee_bump {
                fee_bump_outer_fail(code, &e.to_string())
            } else {
                pre_seq_fail(code, &e.to_string())
            }));
        }

        // Phase 1b: Soroban resource-fee bound #3 (declared >= computed).
        //
        // Mirrors stellar-core's TransactionFrame::commonValidPreSeqNum
        // (TransactionFrame.cpp:1434-1460): the resource fee is computed at
        // validation time via computePreApplySorobanResourceFee (eventsSize = 0,
        // declared resources), then the overflow guard and bound #3
        // (`sorobanData.resourceFee < non_refundable + refundable` ⇒
        // txSOROBAN_INVALID) are enforced — unconditionally for any Soroban tx
        // (NOT gated by chargeFee/validateResourceFee, unlike bound #2). This
        // runs inside commonValidPreSeqNum, before the sequence check and the
        // Phase 2 time/fee/account checks, so failures must NOT bump the
        // sequence number → pre_seq_fail. Because stellar-core surfaces this via
        // setInnermostError (the inner result), the fee-bump case also uses
        // pre_seq_fail rather than fee_bump_outer_fail.
        //
        // Bounds #1 (<= MAX_RESOURCE_FEE) and #2 (<= totalFee) are enforced
        // statelessly in Phase 1 (check_valid_pre_seq_num_with_config); only
        // bound #3 needs the SorobanConfig, so it lives here where the frame and
        // self.soroban_config coexist. compute_soroban_resource_fee with
        // event_size_bytes = 0 exactly mirrors the apply-path call (mod.rs:1810)
        // and computePreApplySorobanResourceFee. This is read-only validation: it
        // computes a fee for comparison only and deducts nothing.
        if frame.is_soroban() {
            let (non_refundable, refundable) = super::compute_soroban_resource_fee(
                &frame,
                self.protocol_version,
                &self.soroban_config,
                0,
            )
            .unwrap_or((0, 0));

            // Overflow guard: refundable + non_refundable must not exceed i64::MAX
            // (TransactionFrame.cpp:1435-1443, also txSOROBAN_INVALID).
            if refundable > i64::MAX - non_refundable {
                return Ok(Err(pre_seq_fail(
                    TransactionResultCode::TxSorobanInvalid,
                    "Soroban resource fee overflows i64",
                )));
            }

            // Bound #3: declared resourceFee must cover the computed resource fee.
            if frame.declared_soroban_resource_fee().as_i64() < non_refundable + refundable {
                return Ok(Err(pre_seq_fail(
                    TransactionResultCode::TxSorobanInvalid,
                    "Declared Soroban resource fee is below the computed resource fee",
                )));
            }
        }

        // Phase 2: Fee, time/ledger bounds, and account loading.
        //
        // The ordering of these checks differs between fee-bump and non-fee-bump
        // transactions, matching stellar-core's separate validation paths:
        //
        // Non-fee-bump (TransactionFrame::commonValidPreSeqNum):
        //   time/ledger bounds → inclusion fee → account load
        //
        // Fee-bump (FeeBumpTransactionFrame::commonValidPreSeqNum + inner commonValidPreSeqNum):
        //   outer fee → fee source load → inner time/ledger bounds → inner source load
        //
        // This ordering determines which TransactionResultCode is returned when
        // multiple checks fail simultaneously (first-hit wins).
        let validation_ctx = ValidationContext::new(
            self.ledger_seq,
            self.close_time,
            base_fee,
            self.base_reserve,
            self.protocol_version,
            self.network_id,
        );

        let acct_load_start = std::time::Instant::now();

        let (fee_source_account, source_account, precomputed_outer_hash) = if is_fee_bump {
            // Fee-bump ordering: outer fee → fee source load → time/ledger bounds → inner source load
            // Parity: FeeBumpTransactionFrame::commonValidPreSeqNum (lines 337-398)
            //         then inner TransactionFrame::commonValidPreSeqNum (lines 1471-1502)

            // 2a. Outer fee check
            // SECURITY: fee computation overflow prevented by tx validation bounds
            let outer_min_inclusion_fee = frame.min_inclusion_fee(base_fee as i64);
            let outer_inclusion_fee = frame.inclusion_fee();

            if !frame.has_sufficient_inclusion_fee(base_fee as i64) {
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
                            inner_frame.inclusion_fee().as_i64(),
                            inner_frame.min_inclusion_fee(base_fee as i64).as_i64(),
                            inner_frame.is_soroban(),
                        )
                    }
                },
                _ => (0, base_fee as i64, false),
            };

            if inner_inclusion_fee >= 0 {
                let v1 = outer_inclusion_fee.as_i64() as i128 * inner_min_inclusion_fee as i128;
                let v2 = inner_inclusion_fee as i128 * outer_min_inclusion_fee.as_i64() as i128;
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

            // 2b. Fee source account load
            // Parity: FeeBumpTransactionFrame::commonValidPreSeqNum line 391-396
            if !self.load_account(snapshot, &fee_source_id)? {
                return Ok(Err(fee_bump_outer_fail(
                    TransactionResultCode::TxNoAccount,
                    "Fee source account not found",
                )));
            }
            let fee_source_account = match self.state.get_account(&fee_source_id) {
                Some(acc) => acc.clone(),
                None => {
                    return Ok(Err(fee_bump_outer_fail(
                        TransactionResultCode::TxNoAccount,
                        "Fee source account not found",
                    )))
                }
            };

            // 2b'. Outer auth check (fee source signature, LOW threshold)
            // Parity: FeeBumpTransactionFrame::commonValid lines 416-421
            //         (`checkAllTransactionSignatures` before inner validation).
            //
            // Read the fee source from the IMMUTABLE SNAPSHOT, not self.state.
            // Precondition: the snapshot is the LCL — the same state that tx-set
            // validation validated against. Reading from self.state would see
            // prior-TX mutations (e.g., signer removal) and create a parity
            // divergence: stellar-core does NOT re-check outer auth at apply time,
            // so using the snapshot ensures we match what tx-set validation saw.
            let outer_hash = frame
                .hash(&self.network_id)
                .map_err(|e| LedgerError::Internal(format!("tx hash error: {}", e)))?;
            if let Some(snapshot_fee_source) = snapshot.get_account(&fee_source_id)? {
                if !fee_bump_outer_auth_check(&outer_hash, frame.signatures(), &snapshot_fee_source)
                {
                    return Ok(Err(fee_bump_outer_fail(
                        TransactionResultCode::TxBadAuth,
                        "Fee-bump outer signature check failed",
                    )));
                }
            }
            // If the snapshot doesn't have the account, the state-based load above
            // found it (created by a prior TX in this ledger). In that case the
            // outer auth was validated by tx-set construction and we proceed.

            // 2c. Time/ledger bounds (inner tx properties)
            // Parity: isTooEarly then isTooLate (TransactionFrame.cpp:1471-1476)
            if let Err(_e) = validation::is_too_early(&frame, &validation_ctx) {
                return Ok(Err(pre_seq_fail(
                    TransactionResultCode::TxTooEarly,
                    "Too early",
                )));
            }

            if let Err(_e) = validation::is_too_late(&frame, &validation_ctx) {
                return Ok(Err(pre_seq_fail(
                    TransactionResultCode::TxTooLate,
                    "Too late",
                )));
            }

            // 2d. Inner source account load
            // Parity: inner TransactionFrame::commonValidPreSeqNum line 1495-1502
            if !self.load_account(snapshot, &inner_source_id)? {
                return Ok(Err(pre_seq_fail(
                    TransactionResultCode::TxNoAccount,
                    "Source account not found",
                )));
            }
            let source_account = match self.state.get_account(&inner_source_id) {
                Some(acc) => acc.clone(),
                None => {
                    return Ok(Err(pre_seq_fail(
                        TransactionResultCode::TxNoAccount,
                        "Source account not found",
                    )))
                }
            };

            (fee_source_account, source_account, Some(outer_hash))
        } else {
            // 2a. Time/ledger bounds
            // Parity: isTooEarly then isTooLate (TransactionFrame.cpp:1471-1476)
            if let Err(_e) = validation::is_too_early(&frame, &validation_ctx) {
                return Ok(Err(pre_seq_fail(
                    TransactionResultCode::TxTooEarly,
                    "Too early",
                )));
            }

            if let Err(_e) = validation::is_too_late(&frame, &validation_ctx) {
                return Ok(Err(pre_seq_fail(
                    TransactionResultCode::TxTooLate,
                    "Too late",
                )));
            }

            // 2b. Inclusion fee check
            // Parity: TransactionFrame::commonValidPreSeqNum lines 1482-1493
            if !frame.has_sufficient_inclusion_fee(base_fee as i64) {
                return Ok(Err(pre_seq_fail(
                    TransactionResultCode::TxInsufficientFee,
                    "Insufficient fee",
                )));
            }

            // 2c. Account load
            // Parity: TransactionFrame::commonValidPreSeqNum lines 1495-1502
            if !self.load_account(snapshot, &fee_source_id)? {
                return Ok(Err(pre_seq_fail(
                    TransactionResultCode::TxNoAccount,
                    "Source account not found",
                )));
            }
            // For non-fee-bump, fee source and inner source are typically the same
            // account. Load inner source separately only if they differ.
            if fee_source_id != inner_source_id && !self.load_account(snapshot, &inner_source_id)? {
                return Ok(Err(pre_seq_fail(
                    TransactionResultCode::TxNoAccount,
                    "Source account not found",
                )));
            }

            let fee_source_account = match self.state.get_account(&fee_source_id) {
                Some(acc) => acc.clone(),
                None => {
                    return Ok(Err(pre_seq_fail(
                        TransactionResultCode::TxNoAccount,
                        "Source account not found",
                    )))
                }
            };
            let source_account = if fee_source_id == inner_source_id {
                fee_source_account.clone()
            } else {
                match self.state.get_account(&inner_source_id) {
                    Some(acc) => acc.clone(),
                    None => {
                        return Ok(Err(pre_seq_fail(
                            TransactionResultCode::TxNoAccount,
                            "Source account not found",
                        )))
                    }
                }
            };

            (fee_source_account, source_account, None)
        };

        let val_account_load_us = acct_load_start.elapsed().as_micros() as u64;

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
            // validate_signatures only fails on hash computation errors.
            // For fee-bump, hash failure is now caught in Phase 2b' above.
            // For non-fee-bump, report as TxBadAuth.
            return Ok(Err(post_seq_fail(
                TransactionResultCode::TxBadAuth,
                "Invalid signature",
            )));
        }

        let hash_start = std::time::Instant::now();
        // For fee-bump, outer_hash was computed in Phase 2b'. For non-fee-bump,
        // compute it here.
        let outer_hash = if let Some(h) = precomputed_outer_hash {
            h
        } else {
            frame
                .hash(&self.network_id)
                .map_err(|e| LedgerError::Internal(format!("tx hash error: {}", e)))?
        };
        let val_tx_hash_us = hash_start.elapsed().as_micros() as u64;

        let ed25519_start = std::time::Instant::now();
        // Fee-bump outer auth (fee source signature) is validated in Phase 2b'
        // against the immutable snapshot, matching stellar-core's
        // FeeBumpTransactionFrame::commonValid ordering. The check below handles
        // non-fee-bump source account signature validation only.
        //
        // NOTE: For fee-bump transactions, we deliberately do NOT re-check the
        // outer signature here against self.state. The Phase 2b' check uses the
        // snapshot (LCL state) which is what tx-set validation saw. A prior TX
        // in the same ledger may have modified the fee source's signer set;
        // stellar-core's apply path also does not re-check outer auth.
        if !is_fee_bump {
            let outer_threshold = threshold_low(&fee_source_account);
            if !has_sufficient_signer_weight(
                &outer_hash,
                frame.signatures(),
                &fee_source_account,
                outer_threshold,
            ) {
                tracing::debug!("Signature check failed: fee_source outer check");
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

        // Fee affordability guard — final step of stellar-core's commonValid
        // (applying=true). The fee source's *available* balance must cover the
        // (capped) fee, where available = balance − minBalance − sellingLiab,
        // computed WITHOUT saturation (TransactionUtils.cpp:752-778,
        // getAvailableBalance). With applying=true, feeToPay is 0 because the
        // fee was (or will be) charged by processFeeSeqNum, so the predicate
        // reduces to `(balance − chargedFee) − minBalance − sellingLiab < 0`.
        //
        // We deliberately compute the three-term subtraction directly in i64
        // rather than reuse `reserves::available_to_send`, which saturates the
        // intermediate result to 0 and would make the `< 0` test unreachable.
        //
        // The guard checks the FEE SOURCE account for both fee-bump and
        // non-fee-bump transactions, mirroring:
        //   - TransactionFrame::commonValid (TransactionFrame.cpp:1742-1755),
        //     setInnermostError(txINSUFFICIENT_BALANCE) ⇒ post_seq_fail.
        //   - FeeBumpTransactionFrame::commonValid's independent fee-source
        //     guard (FeeBumpTransactionFrame.cpp:466-475),
        //     setError(txINSUFFICIENT_BALANCE) ⇒ fee_bump_outer_fail.
        //
        // Read the fee-source balance from `self.state` and cap the fee against
        // it at the same point the deduction caps (`fee = min(balance, fee)`),
        // so the charged amount matches the actual deduction.
        if let Some(fee_source) = self.state.get_account(&fee_source_id) {
            let charged_fee = std::cmp::min(fee_source.balance, fee_to_charge);
            let min_balance = crate::reserves::minimum_balance(fee_source, self.base_reserve);
            let selling_liabilities = crate::reserves::selling_liabilities(fee_source);
            let available_balance = fee_source
                .balance
                .saturating_sub(charged_fee)
                .saturating_sub(min_balance)
                .saturating_sub(selling_liabilities);
            if available_balance < 0 {
                tracing::debug!(
                    balance = fee_source.balance,
                    charged_fee = charged_fee,
                    min_balance = min_balance,
                    selling_liabilities = selling_liabilities,
                    is_fee_bump = is_fee_bump,
                    "Fee source available balance below zero after fee"
                );
                return Ok(Err(if is_fee_bump {
                    fee_bump_outer_fail(
                        TransactionResultCode::TxInsufficientBalance,
                        "Fee source available balance insufficient for fee",
                    )
                } else {
                    post_seq_fail(
                        TransactionResultCode::TxInsufficientBalance,
                        "Fee source available balance insufficient for fee",
                    )
                }));
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
