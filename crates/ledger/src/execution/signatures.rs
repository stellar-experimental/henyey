//! Signature verification and threshold checking.
//!
//! Implements the multi-signature verification pipeline that validates
//! transaction signatures against account signers, checks weight thresholds
//! for each operation type, and handles signed payload authentication.

use super::*;

/// Convert AccountId to key bytes.
pub(crate) fn account_id_to_key(account_id: &AccountId) -> [u8; 32] {
    match &account_id.0 {
        stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(key) => key.0,
    }
}

/// Check if an operation result indicates success.
pub(super) fn is_operation_success(result: &OperationResult) -> bool {
    match result {
        OperationResult::OpInner(inner) => {
            use stellar_xdr::curr::OperationResultTr;
            use stellar_xdr::curr::*;
            match inner {
                OperationResultTr::CreateAccount(r) => {
                    matches!(r, CreateAccountResult::Success)
                }
                OperationResultTr::Payment(r) => {
                    matches!(r, PaymentResult::Success)
                }
                OperationResultTr::PathPaymentStrictReceive(r) => {
                    matches!(r, PathPaymentStrictReceiveResult::Success(_))
                }
                OperationResultTr::ManageSellOffer(r) => {
                    matches!(r, ManageSellOfferResult::Success(_))
                }
                OperationResultTr::CreatePassiveSellOffer(r) => {
                    matches!(r, ManageSellOfferResult::Success(_))
                }
                OperationResultTr::SetOptions(r) => {
                    matches!(r, SetOptionsResult::Success)
                }
                OperationResultTr::ChangeTrust(r) => {
                    matches!(r, ChangeTrustResult::Success)
                }
                OperationResultTr::AllowTrust(r) => {
                    matches!(r, AllowTrustResult::Success)
                }
                OperationResultTr::AccountMerge(r) => {
                    matches!(r, AccountMergeResult::Success(_))
                }
                OperationResultTr::Inflation(r) => {
                    matches!(r, InflationResult::Success(_))
                }
                OperationResultTr::ManageData(r) => {
                    matches!(r, ManageDataResult::Success)
                }
                OperationResultTr::BumpSequence(r) => {
                    matches!(r, BumpSequenceResult::Success)
                }
                OperationResultTr::ManageBuyOffer(r) => {
                    matches!(r, ManageBuyOfferResult::Success(_))
                }
                OperationResultTr::PathPaymentStrictSend(r) => {
                    matches!(r, PathPaymentStrictSendResult::Success(_))
                }
                OperationResultTr::CreateClaimableBalance(r) => {
                    matches!(r, CreateClaimableBalanceResult::Success(_))
                }
                OperationResultTr::ClaimClaimableBalance(r) => {
                    matches!(r, ClaimClaimableBalanceResult::Success)
                }
                OperationResultTr::BeginSponsoringFutureReserves(r) => {
                    matches!(r, BeginSponsoringFutureReservesResult::Success)
                }
                OperationResultTr::EndSponsoringFutureReserves(r) => {
                    matches!(r, EndSponsoringFutureReservesResult::Success)
                }
                OperationResultTr::RevokeSponsorship(r) => {
                    matches!(r, RevokeSponsorshipResult::Success)
                }
                OperationResultTr::Clawback(r) => {
                    matches!(r, ClawbackResult::Success)
                }
                OperationResultTr::ClawbackClaimableBalance(r) => {
                    matches!(r, ClawbackClaimableBalanceResult::Success)
                }
                OperationResultTr::SetTrustLineFlags(r) => {
                    matches!(r, SetTrustLineFlagsResult::Success)
                }
                OperationResultTr::LiquidityPoolDeposit(r) => {
                    matches!(r, LiquidityPoolDepositResult::Success)
                }
                OperationResultTr::LiquidityPoolWithdraw(r) => {
                    matches!(r, LiquidityPoolWithdrawResult::Success)
                }
                OperationResultTr::InvokeHostFunction(r) => {
                    matches!(r, InvokeHostFunctionResult::Success(_))
                }
                OperationResultTr::ExtendFootprintTtl(r) => {
                    matches!(r, ExtendFootprintTtlResult::Success)
                }
                OperationResultTr::RestoreFootprint(r) => {
                    matches!(r, RestoreFootprintResult::Success)
                }
            }
        }
        OperationResult::OpNotSupported => false, // Unsupported operations fail
        _ => false,
    }
}

pub(super) fn has_sufficient_signer_weight(
    tx_hash: &Hash256,
    signatures: &[stellar_xdr::curr::DecoratedSignature],
    account: &AccountEntry,
    required_weight: u32,
) -> bool {
    let mut total = 0u32;
    let mut counted: HashSet<Hash256> = HashSet::new();

    // Master key signer.
    let master_key_bytes = account_id_to_key(&account.account_id);
    let master_weight = account.thresholds.0[0] as u32;
    tracing::trace!(
        master_weight = master_weight,
        required_weight = required_weight,
        num_signatures = signatures.len(),
        num_signers = account.signers.len(),
        thresholds = ?account.thresholds.0,
        "Checking signature weight"
    );
    if master_weight > 0 {
        let has_sig = has_ed25519_signature_raw(tx_hash, signatures, &master_key_bytes);
        tracing::trace!(has_master_sig = has_sig, "Master key signature check");
        if has_sig {
            let id = signer_key_id(&SignerKey::Ed25519(stellar_xdr::curr::Uint256(
                master_key_bytes,
            )));
            if counted.insert(id) {
                total = total.saturating_add(master_weight);
            }
        }
    }

    for signer in account.signers.iter() {
        if signer.weight == 0 {
            continue;
        }
        let key = &signer.key;
        let id = signer_key_id(key);

        if counted.contains(&id) {
            continue;
        }

        match key {
            SignerKey::Ed25519(key) => {
                if has_ed25519_signature_raw(tx_hash, signatures, &key.0) && counted.insert(id) {
                    total = total.saturating_add(signer.weight);
                }
            }
            SignerKey::PreAuthTx(key) => {
                if key.0 == tx_hash.0 && counted.insert(id) {
                    total = total.saturating_add(signer.weight);
                }
            }
            SignerKey::HashX(key) => {
                if has_hashx_signature(signatures, key) && counted.insert(id) {
                    total = total.saturating_add(signer.weight);
                }
            }
            SignerKey::Ed25519SignedPayload(payload) => {
                if has_signed_payload_signature(tx_hash, signatures, payload) && counted.insert(id)
                {
                    total = total.saturating_add(signer.weight);
                }
            }
        }

        if total >= required_weight && total > 0 {
            return true;
        }
    }

    total >= required_weight && total > 0
}

pub(super) fn has_required_extra_signers(
    tx_hash: &Hash256,
    signatures: &[stellar_xdr::curr::DecoratedSignature],
    extra_signers: &[SignerKey],
) -> bool {
    extra_signers.iter().all(|signer| match signer {
        SignerKey::Ed25519(key) => {
            has_ed25519_signature_raw(tx_hash, signatures, &key.0)
        }
        SignerKey::PreAuthTx(key) => key.0 == tx_hash.0,
        SignerKey::HashX(key) => has_hashx_signature(signatures, key),
        SignerKey::Ed25519SignedPayload(payload) => {
            has_signed_payload_signature(tx_hash, signatures, payload)
        }
    })
}

pub(super) fn fee_bump_inner_hash(
    frame: &TransactionFrame,
    network_id: &NetworkId,
) -> Result<Hash256> {
    match frame.envelope() {
        TransactionEnvelope::TxFeeBump(env) => match &env.tx.inner_tx {
            stellar_xdr::curr::FeeBumpTransactionInnerTx::Tx(inner) => {
                let inner_env = TransactionEnvelope::Tx(inner.clone());
                let inner_frame = TransactionFrame::with_network(inner_env, *network_id);
                inner_frame
                    .hash(network_id)
                    .map_err(|e| LedgerError::Internal(format!("inner tx hash error: {}", e)))
            }
        },
        _ => frame
            .hash(network_id)
            .map_err(|e| LedgerError::Internal(format!("tx hash error: {}", e))),
    }
}

pub(super) fn threshold_low(account: &AccountEntry) -> u32 {
    account.thresholds.0[1] as u32
}

pub(super) fn threshold_medium(account: &AccountEntry) -> u32 {
    account.thresholds.0[2] as u32
}

pub(super) fn threshold_high(account: &AccountEntry) -> u32 {
    account.thresholds.0[3] as u32
}

/// Determine the threshold level required for an operation type.
/// Matches stellar-core's per-OperationFrame getThresholdLevel() overrides.
pub(super) fn get_threshold_for_op(op: &Operation) -> ThresholdLevel {
    match &op.body {
        // LOW threshold operations
        OperationBody::BumpSequence(_) => ThresholdLevel::Low,
        OperationBody::ClaimClaimableBalance(_) => ThresholdLevel::Low,
        OperationBody::ExtendFootprintTtl(_) => ThresholdLevel::Low,
        OperationBody::Inflation => ThresholdLevel::Low,
        OperationBody::RestoreFootprint(_) => ThresholdLevel::Low,
        // AllowTrust and SetTrustLineFlags inherit from TrustFlagsOpFrameBase
        OperationBody::AllowTrust(_) => ThresholdLevel::Low,
        OperationBody::SetTrustLineFlags(_) => ThresholdLevel::Low,

        // HIGH threshold operations
        OperationBody::AccountMerge(_) => ThresholdLevel::High,
        OperationBody::SetOptions(set_opts) => {
            // HIGH if modifying weights/thresholds/signers, otherwise MEDIUM
            if set_opts.master_weight.is_some()
                || set_opts.low_threshold.is_some()
                || set_opts.med_threshold.is_some()
                || set_opts.high_threshold.is_some()
                || set_opts.signer.is_some()
            {
                ThresholdLevel::High
            } else {
                ThresholdLevel::Medium
            }
        }

        // All other operations default to MEDIUM
        _ => ThresholdLevel::Medium,
    }
}

/// Get the needed threshold weight for an operation based on its threshold level.
pub(super) fn get_needed_threshold(account: &AccountEntry, level: ThresholdLevel) -> u32 {
    match level {
        ThresholdLevel::Low => threshold_low(account),
        ThresholdLevel::Medium => threshold_medium(account),
        ThresholdLevel::High => threshold_high(account),
    }
}

/// Check if a decorated signature matches an Ed25519SignedPayload signer key.
pub(super) fn has_signed_payload_match(
    sig: &stellar_xdr::curr::DecoratedSignature,
    signed_payload: &stellar_xdr::curr::SignerKeyEd25519SignedPayload,
) -> bool {
    let pk = match henyey_crypto::PublicKey::from_bytes(&signed_payload.ed25519.0) {
        Ok(pk) => pk,
        Err(_) => return false,
    };

    let pubkey_hint = [
        signed_payload.ed25519.0[28],
        signed_payload.ed25519.0[29],
        signed_payload.ed25519.0[30],
        signed_payload.ed25519.0[31],
    ];
    let payload_hint = if signed_payload.payload.len() >= 4 {
        let len = signed_payload.payload.len();
        [
            signed_payload.payload[len - 4],
            signed_payload.payload[len - 3],
            signed_payload.payload[len - 2],
            signed_payload.payload[len - 1],
        ]
    } else {
        let mut hint = [0u8; 4];
        for (i, &byte) in signed_payload.payload.iter().enumerate() {
            if i < 4 {
                hint[i] = byte;
            }
        }
        hint
    };
    let expected_hint = [
        pubkey_hint[0] ^ payload_hint[0],
        pubkey_hint[1] ^ payload_hint[1],
        pubkey_hint[2] ^ payload_hint[2],
        pubkey_hint[3] ^ payload_hint[3],
    ];

    if sig.hint.0 != expected_hint {
        return false;
    }

    let ed_sig = match henyey_crypto::Signature::try_from(&sig.signature) {
        Ok(s) => s,
        Err(_) => return false,
    };
    henyey_crypto::verify(&pk, &signed_payload.payload, &ed_sig).is_ok()
}

/// Check extra signers against the signature tracker.
/// Each extra signer must be matched by at least one signature.
/// Mirrors stellar-core's TransactionFrame::checkExtraSigners().
pub(super) fn check_extra_signers_with_tracker(
    tracker: &mut SignatureTracker,
    extra_signers: &[SignerKey],
) -> bool {
    if extra_signers.is_empty() {
        return true;
    }
    // Build a signer list where each extra signer has weight 1,
    // and the needed weight is the total number of extra signers.
    let signers: Vec<(SignerKey, u32)> = extra_signers
        .iter()
        .map(|key| (key.clone(), 1u32))
        .collect();
    let needed_weight = extra_signers.len() as i32;
    tracker.check_signature_from_signers(&signers, needed_weight)
}

/// Perform per-operation signature checking.
///
/// This mirrors stellar-core's processSignatures() + checkOperationSignatures().
/// For each operation, it checks the per-operation source account's signatures
/// at the appropriate threshold level. If any operation fails auth, all
/// operations get their results set (passing ops get default success, failing
/// ops get OpBadAuth or OpNoAccount) and the function returns the operation
/// results with `txFAILED`.
///
/// Also checks that all signatures were used (txBAD_AUTH_EXTRA).
///
/// Returns:
/// - `None` if all checks pass (proceed to operation execution)
/// - `Some((results, failure))` if checks fail
pub(super) fn check_operation_signatures(
    frame: &TransactionFrame,
    state: &LedgerStateManager,
    tx_hash: &Hash256,
    signatures: &[stellar_xdr::curr::DecoratedSignature],
    inner_source_id: &AccountId,
) -> Option<(Vec<OperationResult>, ExecutionFailure)> {
    // Create a signature tracker for used-signature tracking
    let mut tracker = SignatureTracker::new(tx_hash, signatures);

    // Step 1: Re-check TX-level source account signatures with tracking.
    // This mirrors stellar-core's commonPreApply -> checkAllTransactionSignatures,
    // which is done with the same SignatureChecker instance that later checks
    // per-op signatures.
    //
    // For fee-bump transactions, inner sig failures are caught here (after the
    // outer fee has been charged) rather than in validate_preconditions. A prior
    // tx in the same ledger may have modified the inner source's signer set so
    // that the inner sigs are no longer valid at apply-time.
    if let Some(source_account) = state.get_account(inner_source_id) {
        let tx_threshold = threshold_low(source_account);
        if !tracker.check_signature(source_account, tx_threshold) {
            // TX-level auth failed. Return InvalidSignature so the caller records
            // txBadAuth / txFeeBumpInnerFailed with the fee already charged.
            return Some((Vec::new(), TransactionResultCode::TxBadAuth));
        }
    } else {
        return None; // Source account gone — shouldn't happen after seq bump
    }

    // Step 2: Check extra signers with tracking
    if let Preconditions::V2(cond) = frame.preconditions() {
        if !cond.extra_signers.is_empty() {
            check_extra_signers_with_tracker(&mut tracker, &cond.extra_signers);
            // Extra signer failures are already caught by the earlier check,
            // but we need to run this for used-signature tracking.
        }
    }

    // Step 3: Per-operation signature checks
    // Mirrors stellar-core's checkOperationSignatures().
    let ops = frame.operations();
    let mut all_ops_valid = true;
    let mut op_results: Vec<Option<OperationResult>> = vec![None; ops.len()];

    for (i, op) in ops.iter().enumerate() {
        // Resolve per-op source: use op.source_account if set, else TX source
        let op_source_id = if let Some(ref source) = op.source_account {
            henyey_tx::muxed_to_account_id(source)
        } else {
            inner_source_id.clone()
        };

        // Load the op source account from current state
        if let Some(op_account) = state.get_account(&op_source_id) {
            // Account exists: check threshold
            let threshold_level = get_threshold_for_op(op);
            let needed = get_needed_threshold(op_account, threshold_level);
            if !tracker.check_signature(op_account, needed) {
                op_results[i] = Some(OperationResult::OpBadAuth);
                all_ops_valid = false;
            }
        } else {
            // Account doesn't exist
            // forApply=false in checkOperationSignatures, so:
            // - If op has no explicit source → opNO_ACCOUNT
            // - If op has explicit source → try checkSignatureNoAccount
            if op.source_account.is_none() {
                op_results[i] = Some(OperationResult::OpNoAccount);
                all_ops_valid = false;
            } else {
                // checkSignatureNoAccount: synthetic signer with just the pubkey, weight=1, needed=0
                if !tracker.check_signature_no_account(&op_source_id) {
                    op_results[i] = Some(OperationResult::OpBadAuth);
                    all_ops_valid = false;
                }
            }
        }
    }

    if !all_ops_valid {
        // Build full operation results: failed ops get their error, passing ops get
        // a default opBAD_AUTH (matching stellar-core where OperationResult is
        // initialized to opINNER with default success, but the overall TX is marked
        // txFAILED). In stellar-core, the results vector is pre-initialized and
        // only failing ops get their code set. The passing ops keep the default
        // initialized state.
        //
        // However, looking at the actual XDR: when processSignatures sets txFAILED,
        // the op results that weren't touched keep their default value. In XDR,
        // OperationResult has a discriminant that defaults to opINNER with the
        // inner result being the operation-specific default. For ops that pass
        // sig check, they stay as the initialized default.
        let final_results: Vec<OperationResult> = ops
            .iter()
            .enumerate()
            .map(|(i, op)| {
                if let Some(ref result) = op_results[i] {
                    result.clone()
                } else {
                    default_success_op_result(op)
                }
            })
            .collect();
        return Some((final_results, TransactionResultCode::TxFailed));
    }

    // Step 4: Check all signatures used (txBAD_AUTH_EXTRA)
    if !tracker.check_all_signatures_used() {
        return Some((Vec::new(), TransactionResultCode::TxBadAuthExtra));
    }

    None
}

/// Create a default success operation result for an operation.
/// When per-op signature checking succeeds for an op but another op fails,
/// the passing op keeps its default-initialized result. In stellar-core's XDR,
/// OperationResult defaults to opINNER with a default inner result for the
/// operation type (typically the "Success" variant).
pub(super) fn default_success_op_result(op: &Operation) -> OperationResult {
    match &op.body {
        OperationBody::CreateAccount(_) => OperationResult::OpInner(
            OperationResultTr::CreateAccount(stellar_xdr::curr::CreateAccountResult::Success),
        ),
        OperationBody::Payment(_) => OperationResult::OpInner(OperationResultTr::Payment(
            stellar_xdr::curr::PaymentResult::Success,
        )),
        OperationBody::PathPaymentStrictReceive(_) => OperationResult::OpInner(
            OperationResultTr::PathPaymentStrictReceive(PathPaymentStrictReceiveResult::Success(
                stellar_xdr::curr::PathPaymentStrictReceiveResultSuccess {
                    offers: Vec::new().try_into().unwrap_or_default(),
                    last: stellar_xdr::curr::SimplePaymentResult {
                        destination: AccountId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
                            stellar_xdr::curr::Uint256([0; 32]),
                        )),
                        asset: Asset::Native,
                        amount: 0,
                    },
                },
            )),
        ),
        OperationBody::ManageSellOffer(_) => {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(
                ManageSellOfferResult::Success(stellar_xdr::curr::ManageOfferSuccessResult {
                    offers_claimed: Vec::new().try_into().unwrap_or_default(),
                    offer: stellar_xdr::curr::ManageOfferSuccessResultOffer::Deleted,
                }),
            ))
        }
        OperationBody::CreatePassiveSellOffer(_) => {
            OperationResult::OpInner(OperationResultTr::CreatePassiveSellOffer(
                ManageSellOfferResult::Success(stellar_xdr::curr::ManageOfferSuccessResult {
                    offers_claimed: Vec::new().try_into().unwrap_or_default(),
                    offer: stellar_xdr::curr::ManageOfferSuccessResultOffer::Deleted,
                }),
            ))
        }
        OperationBody::SetOptions(_) => OperationResult::OpInner(OperationResultTr::SetOptions(
            stellar_xdr::curr::SetOptionsResult::Success,
        )),
        OperationBody::ChangeTrust(_) => OperationResult::OpInner(OperationResultTr::ChangeTrust(
            stellar_xdr::curr::ChangeTrustResult::Success,
        )),
        OperationBody::AllowTrust(_) => OperationResult::OpInner(OperationResultTr::AllowTrust(
            stellar_xdr::curr::AllowTrustResult::Success,
        )),
        OperationBody::AccountMerge(_) => OperationResult::OpInner(
            OperationResultTr::AccountMerge(AccountMergeResult::Success(0)),
        ),
        OperationBody::Inflation => OperationResult::OpInner(OperationResultTr::Inflation(
            InflationResult::Success(Vec::new().try_into().unwrap_or_default()),
        )),
        OperationBody::ManageData(_) => OperationResult::OpInner(OperationResultTr::ManageData(
            stellar_xdr::curr::ManageDataResult::Success,
        )),
        OperationBody::BumpSequence(_) => OperationResult::OpInner(
            OperationResultTr::BumpSequence(stellar_xdr::curr::BumpSequenceResult::Success),
        ),
        OperationBody::ManageBuyOffer(_) => {
            OperationResult::OpInner(OperationResultTr::ManageBuyOffer(
                ManageBuyOfferResult::Success(stellar_xdr::curr::ManageOfferSuccessResult {
                    offers_claimed: Vec::new().try_into().unwrap_or_default(),
                    offer: stellar_xdr::curr::ManageOfferSuccessResultOffer::Deleted,
                }),
            ))
        }
        OperationBody::PathPaymentStrictSend(_) => OperationResult::OpInner(
            OperationResultTr::PathPaymentStrictSend(PathPaymentStrictSendResult::Success(
                stellar_xdr::curr::PathPaymentStrictSendResultSuccess {
                    offers: Vec::new().try_into().unwrap_or_default(),
                    last: stellar_xdr::curr::SimplePaymentResult {
                        destination: AccountId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
                            stellar_xdr::curr::Uint256([0; 32]),
                        )),
                        asset: Asset::Native,
                        amount: 0,
                    },
                },
            )),
        ),
        OperationBody::CreateClaimableBalance(_) => OperationResult::OpInner(
            OperationResultTr::CreateClaimableBalance(CreateClaimableBalanceResult::Success(
                ClaimableBalanceId::ClaimableBalanceIdTypeV0(stellar_xdr::curr::Hash([0; 32])),
            )),
        ),
        OperationBody::ClaimClaimableBalance(_) => {
            OperationResult::OpInner(OperationResultTr::ClaimClaimableBalance(
                stellar_xdr::curr::ClaimClaimableBalanceResult::Success,
            ))
        }
        OperationBody::BeginSponsoringFutureReserves(_) => {
            OperationResult::OpInner(OperationResultTr::BeginSponsoringFutureReserves(
                stellar_xdr::curr::BeginSponsoringFutureReservesResult::Success,
            ))
        }
        OperationBody::EndSponsoringFutureReserves => {
            OperationResult::OpInner(OperationResultTr::EndSponsoringFutureReserves(
                stellar_xdr::curr::EndSponsoringFutureReservesResult::Success,
            ))
        }
        OperationBody::RevokeSponsorship(_) => {
            OperationResult::OpInner(OperationResultTr::RevokeSponsorship(
                stellar_xdr::curr::RevokeSponsorshipResult::Success,
            ))
        }
        OperationBody::Clawback(_) => OperationResult::OpInner(OperationResultTr::Clawback(
            stellar_xdr::curr::ClawbackResult::Success,
        )),
        OperationBody::ClawbackClaimableBalance(_) => {
            OperationResult::OpInner(OperationResultTr::ClawbackClaimableBalance(
                stellar_xdr::curr::ClawbackClaimableBalanceResult::Success,
            ))
        }
        OperationBody::SetTrustLineFlags(_) => {
            OperationResult::OpInner(OperationResultTr::SetTrustLineFlags(
                stellar_xdr::curr::SetTrustLineFlagsResult::Success,
            ))
        }
        OperationBody::LiquidityPoolDeposit(_) => {
            OperationResult::OpInner(OperationResultTr::LiquidityPoolDeposit(
                stellar_xdr::curr::LiquidityPoolDepositResult::Success,
            ))
        }
        OperationBody::LiquidityPoolWithdraw(_) => {
            OperationResult::OpInner(OperationResultTr::LiquidityPoolWithdraw(
                stellar_xdr::curr::LiquidityPoolWithdrawResult::Success,
            ))
        }
        OperationBody::InvokeHostFunction(_) => {
            OperationResult::OpInner(OperationResultTr::InvokeHostFunction(
                stellar_xdr::curr::InvokeHostFunctionResult::Success(stellar_xdr::curr::Hash(
                    [0; 32],
                )),
            ))
        }
        OperationBody::ExtendFootprintTtl(_) => {
            OperationResult::OpInner(OperationResultTr::ExtendFootprintTtl(
                stellar_xdr::curr::ExtendFootprintTtlResult::Success,
            ))
        }
        OperationBody::RestoreFootprint(_) => OperationResult::OpInner(
            OperationResultTr::RestoreFootprint(stellar_xdr::curr::RestoreFootprintResult::Success),
        ),
    }
}

pub(super) fn signer_key_id(key: &SignerKey) -> Hash256 {
    let bytes = key
        .to_xdr(stellar_xdr::curr::Limits::none())
        .unwrap_or_default();
    Hash256::hash(&bytes)
}

pub(super) fn has_ed25519_signature_raw(
    tx_hash: &Hash256,
    signatures: &[stellar_xdr::curr::DecoratedSignature],
    key_bytes: &[u8; 32],
) -> bool {
    signatures
        .iter()
        .any(|sig| validation::verify_signature_with_raw_key(tx_hash, sig, key_bytes))
}

pub(super) fn has_hashx_signature(
    signatures: &[stellar_xdr::curr::DecoratedSignature],
    key: &stellar_xdr::curr::Uint256,
) -> bool {
    signatures.iter().any(|sig| {
        // HashX signatures can be any length - the signature is the preimage
        // whose SHA256 hash should equal the signer key.
        // Check hint first (last 4 bytes of key)
        let expected_hint = [key.0[28], key.0[29], key.0[30], key.0[31]];
        if sig.hint.0 != expected_hint {
            return false;
        }
        // Hash the preimage (signature) and compare to key
        let hash = Hash256::hash(&sig.signature.0);
        hash.0 == key.0
    })
}

pub(super) fn has_signed_payload_signature(
    _tx_hash: &Hash256,
    signatures: &[stellar_xdr::curr::DecoratedSignature],
    signed_payload: &stellar_xdr::curr::SignerKeyEd25519SignedPayload,
) -> bool {
    let pk = match henyey_crypto::PublicKey::from_bytes(&signed_payload.ed25519.0) {
        Ok(pk) => pk,
        Err(_) => return false,
    };

    // The hint for signed payloads is XOR of pubkey hint and payload hint.
    // See SignatureUtils::getSignedPayloadHint in stellar-core.
    let pubkey_hint = [
        signed_payload.ed25519.0[28],
        signed_payload.ed25519.0[29],
        signed_payload.ed25519.0[30],
        signed_payload.ed25519.0[31],
    ];
    let payload_hint = if signed_payload.payload.len() >= 4 {
        let len = signed_payload.payload.len();
        [
            signed_payload.payload[len - 4],
            signed_payload.payload[len - 3],
            signed_payload.payload[len - 2],
            signed_payload.payload[len - 1],
        ]
    } else {
        // For shorter payloads, stellar-core getHint copies from the beginning
        let mut hint = [0u8; 4];
        for (i, &byte) in signed_payload.payload.iter().enumerate() {
            if i < 4 {
                hint[i] = byte;
            }
        }
        hint
    };
    let expected_hint = [
        pubkey_hint[0] ^ payload_hint[0],
        pubkey_hint[1] ^ payload_hint[1],
        pubkey_hint[2] ^ payload_hint[2],
        pubkey_hint[3] ^ payload_hint[3],
    ];

    signatures.iter().any(|sig| {
        // Check hint first (XOR of pubkey hint and payload hint)
        if sig.hint.0 != expected_hint {
            return false;
        }

        // stellar-core verifies the signature against the raw payload bytes,
        // not a hash. This is per CAP-0040 - the signed payload signer
        // requires a valid signature of the payload from the ed25519 public key.
        let ed_sig = match henyey_crypto::Signature::try_from(&sig.signature) {
            Ok(s) => s,
            Err(_) => return false,
        };
        henyey_crypto::verify(&pk, &signed_payload.payload, &ed_sig).is_ok()
    })
}

/// Compute subSha256(baseSeed, index) as used by stellar-core for PRNG seeds.
///
/// This computes SHA256(baseSeed || xdr::xdr_to_opaque(index)) where index is a u64.
/// XDR encodes uint64 as 8 bytes in big-endian (network byte order).
///
/// Note: stellar-core uses `static_cast<uint64_t>(index)` before passing to `xdr::xdr_to_opaque`,
/// so even though the index is originally an int, it's serialized as 8 bytes.
pub(super) fn sub_sha256(base_seed: &[u8; 32], index: u32) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(base_seed);
    // XDR uint64 is 8 bytes big-endian
    hasher.update((index as u64).to_be_bytes());
    hasher.finalize().into()
}
