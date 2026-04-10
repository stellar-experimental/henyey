//! Payment operation execution.
//!
//! This module implements the execution logic for the Payment operation,
//! which transfers assets between accounts.

use stellar_xdr::curr::{
    AccountId, Asset, OperationResult, OperationResultTr, PathPaymentStrictReceiveOp,
    PathPaymentStrictReceiveResult, PaymentOp, PaymentResult, PaymentResultCode,
};

use super::is_asset_valid;
use super::path_payment::execute_path_payment_strict_receive;
use crate::frame::muxed_to_account_id;
use crate::state::LedgerStateManager;
use crate::validation::LedgerContext;
use crate::Result;

/// Execute a Payment operation.
///
/// This operation transfers assets from the source account to the destination.
/// For native assets, the transfer is direct. For credit assets, both accounts
/// must have trustlines for the asset.
///
/// # Arguments
///
/// * `op` - The Payment operation data
/// * `source` - The source account ID
/// * `state` - The ledger state manager
/// * `context` - The ledger context
///
/// # Returns
///
/// Returns the operation result indicating success or a specific failure reason.
pub(crate) fn execute_payment(
    op: &PaymentOp,
    source: &AccountId,
    state: &mut LedgerStateManager,
    context: &LedgerContext,
) -> Result<OperationResult> {
    let dest = muxed_to_account_id(&op.destination);

    // Amount must be positive
    if op.amount <= 0 {
        return Ok(make_result(PaymentResultCode::Malformed));
    }

    // Asset must be valid (stellar-core doCheckValid)
    if !is_asset_valid(&op.asset) {
        return Ok(make_result(PaymentResultCode::Malformed));
    }

    // stellar-core optimization: if sending native XLM to self, mark as instant success
    // without accessing any ledger entries. This matches behavior from protocol v3+.
    // (Before v3, this applied to all asset types, but we only support v23+)
    if *source == dest && matches!(op.asset, Asset::Native) {
        return Ok(make_result(PaymentResultCode::Success));
    }

    // stellar-core delegates ALL payments to PathPaymentStrictReceive
    // (PaymentOpFrame.cpp:56-126). Build the equivalent operation and translate results.
    let pp_op = PathPaymentStrictReceiveOp {
        send_asset: op.asset.clone(),
        send_max: op.amount,
        destination: op.destination.clone(),
        dest_asset: op.asset.clone(),
        dest_amount: op.amount,
        path: vec![].try_into().unwrap(),
    };

    let pp_result = execute_path_payment_strict_receive(&pp_op, source, state, context)?;

    // Translate PathPaymentStrictReceive result codes to Payment result codes
    match pp_result {
        OperationResult::OpInner(OperationResultTr::PathPaymentStrictReceive(ref pp)) => {
            let payment_code = match pp {
                PathPaymentStrictReceiveResult::Success(_) => PaymentResultCode::Success,
                PathPaymentStrictReceiveResult::Malformed => PaymentResultCode::Malformed,
                PathPaymentStrictReceiveResult::Underfunded => PaymentResultCode::Underfunded,
                PathPaymentStrictReceiveResult::SrcNotAuthorized => {
                    PaymentResultCode::SrcNotAuthorized
                }
                PathPaymentStrictReceiveResult::SrcNoTrust => PaymentResultCode::SrcNoTrust,
                PathPaymentStrictReceiveResult::NoDestination => PaymentResultCode::NoDestination,
                PathPaymentStrictReceiveResult::NoTrust => PaymentResultCode::NoTrust,
                PathPaymentStrictReceiveResult::NotAuthorized => PaymentResultCode::NotAuthorized,
                PathPaymentStrictReceiveResult::LineFull => PaymentResultCode::LineFull,
                PathPaymentStrictReceiveResult::NoIssuer(_) => PaymentResultCode::NoIssuer,
                _ => PaymentResultCode::Malformed,
            };
            Ok(make_result(payment_code))
        }
        _ => Ok(make_result(PaymentResultCode::Malformed)),
    }
}

/// Create an OperationResult from a PaymentResultCode.
fn make_result(code: PaymentResultCode) -> OperationResult {
    let result = match code {
        PaymentResultCode::Success => PaymentResult::Success,
        PaymentResultCode::Malformed => PaymentResult::Malformed,
        PaymentResultCode::Underfunded => PaymentResult::Underfunded,
        PaymentResultCode::SrcNoTrust => PaymentResult::SrcNoTrust,
        PaymentResultCode::SrcNotAuthorized => PaymentResult::SrcNotAuthorized,
        PaymentResultCode::NoDestination => PaymentResult::NoDestination,
        PaymentResultCode::NoTrust => PaymentResult::NoTrust,
        PaymentResultCode::NotAuthorized => PaymentResult::NotAuthorized,
        PaymentResultCode::LineFull => PaymentResult::LineFull,
        PaymentResultCode::NoIssuer => PaymentResult::NoIssuer,
    };

    OperationResult::OpInner(OperationResultTr::Payment(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{
        create_test_account_id, create_test_asset, create_test_trustline_asset,
    };
    use stellar_xdr::curr::*;

    const AUTHORIZED_FLAG: u32 = 0x1; // TrustLineFlags::AuthorizedFlag

    fn create_test_muxed_account(seed: u8) -> MuxedAccount {
        MuxedAccount::Ed25519(Uint256([seed; 32]))
    }

    fn create_test_account(account_id: AccountId, balance: i64) -> AccountEntry {
        AccountEntry {
            account_id,
            balance,
            seq_num: SequenceNumber(1),
            num_sub_entries: 0,
            inflation_dest: None,
            flags: 0,
            home_domain: String32::default(),
            thresholds: Thresholds([1, 0, 0, 0]),
            signers: vec![].try_into().unwrap(),
            ext: AccountEntryExt::V0,
        }
    }

    fn create_test_account_with_liabilities(
        account_id: AccountId,
        balance: i64,
        buying: i64,
        selling: i64,
    ) -> AccountEntry {
        let mut account = create_test_account(account_id, balance);
        account.ext = AccountEntryExt::V1(AccountEntryExtensionV1 {
            liabilities: Liabilities { buying, selling },
            ext: AccountEntryExtensionV1Ext::V0,
        });
        account
    }

    fn create_test_context() -> LedgerContext {
        LedgerContext::testnet(1, 1000)
    }

    fn create_test_trustline(
        account_id: AccountId,
        asset: TrustLineAsset,
        balance: i64,
        limit: i64,
        flags: u32,
    ) -> TrustLineEntry {
        TrustLineEntry {
            account_id,
            asset,
            balance,
            limit,
            flags,
            ext: TrustLineEntryExt::V0,
        }
    }

    fn create_test_trustline_with_liabilities(
        account_id: AccountId,
        asset: TrustLineAsset,
        balance: i64,
        limit: i64,
        flags: u32,
        buying: i64,
        selling: i64,
    ) -> TrustLineEntry {
        TrustLineEntry {
            account_id,
            asset,
            balance,
            limit,
            flags,
            ext: TrustLineEntryExt::V1(TrustLineEntryV1 {
                liabilities: Liabilities { buying, selling },
                ext: TrustLineEntryV1Ext::V0,
            }),
        }
    }

    #[test]
    fn test_native_payment_success() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let dest_id = create_test_account_id(1);

        // Create both accounts
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(dest_id.clone(), 50_000_000));

        let op = PaymentOp {
            destination: create_test_muxed_account(1),
            asset: Asset::Native,
            amount: 10_000_000,
        };

        let result = execute_payment(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        // Verify balances changed
        assert_eq!(state.get_account(&source_id).unwrap().balance, 90_000_000);
        assert_eq!(state.get_account(&dest_id).unwrap().balance, 60_000_000);
    }

    #[test]
    fn test_payment_no_destination() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let op = PaymentOp {
            destination: create_test_muxed_account(1), // Non-existent destination
            asset: Asset::Native,
            amount: 10_000_000,
        };

        let result = execute_payment(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::Payment(r)) => {
                assert!(matches!(r, PaymentResult::NoDestination));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_payment_underfunded() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let dest_id = create_test_account_id(1);

        // Source has 15M, minimum is 10M, so only 5M available
        state.create_account(create_test_account(source_id.clone(), 15_000_000));
        state.create_account(create_test_account(dest_id.clone(), 50_000_000));

        let op = PaymentOp {
            destination: create_test_muxed_account(1),
            asset: Asset::Native,
            amount: 10_000_000, // More than available
        };

        let result = execute_payment(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::Payment(r)) => {
                assert!(matches!(r, PaymentResult::Underfunded));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_payment_underfunded_with_liabilities() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        let dest_id = create_test_account_id(1);

        let min_balance = state
            .minimum_balance_with_counts(context.protocol_version, 0, 0, 0)
            .unwrap();
        state.create_account(create_test_account_with_liabilities(
            source_id.clone(),
            min_balance + 1_000_000,
            0,
            900_000,
        ));
        state.create_account(create_test_account(dest_id.clone(), 50_000_000));

        let op = PaymentOp {
            destination: create_test_muxed_account(1),
            asset: Asset::Native,
            amount: 200_000,
        };

        let result = execute_payment(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::Payment(r)) => {
                assert!(matches!(r, PaymentResult::Underfunded));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_payment_malformed() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();

        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let op = PaymentOp {
            destination: create_test_muxed_account(1),
            asset: Asset::Native,
            amount: 0, // Invalid amount
        };

        let result = execute_payment(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::Payment(r)) => {
                assert!(matches!(r, PaymentResult::Malformed));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_credit_payment_issuer_not_exist_succeeds_in_protocol_23() {
        // In protocol 23+, the issuer existence check was removed (CAP-0017).
        // Payments succeed as long as trustlines exist, even if the issuer account doesn't.
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();

        let issuer_id = create_test_account_id(9);
        let source_id = create_test_account_id(0);
        let dest_id = create_test_account_id(1);
        // Note: issuer account is NOT created - only source and dest exist
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(dest_id.clone(), 100_000_000));

        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
            issuer: issuer_id.clone(),
        });
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            100,
            1_000_000,
            AUTHORIZED_FLAG,
        ));
        state.create_trustline(create_test_trustline(
            dest_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            0,
            1_000_000,
            AUTHORIZED_FLAG,
        ));

        let op = PaymentOp {
            destination: create_test_muxed_account(1),
            asset,
            amount: 10,
        };

        // Payment succeeds even without issuer account in protocol 23+
        let result = execute_payment(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::Payment(r)) => {
                assert!(matches!(r, PaymentResult::Success));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_credit_payment_src_not_authorized() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();

        let issuer_id = create_test_account_id(9);
        let source_id = create_test_account_id(0);
        let dest_id = create_test_account_id(1);
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(dest_id.clone(), 100_000_000));
        state.get_account_mut(&issuer_id).unwrap().flags = AccountFlags::RequiredFlag as u32;

        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
            issuer: issuer_id.clone(),
        });
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            100,
            1_000_000,
            0,
        ));
        state.create_trustline(create_test_trustline(
            dest_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            0,
            1_000_000,
            AUTHORIZED_FLAG,
        ));

        let op = PaymentOp {
            destination: create_test_muxed_account(1),
            asset,
            amount: 10,
        };

        let result = execute_payment(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::Payment(r)) => {
                assert!(matches!(r, PaymentResult::SrcNotAuthorized));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_credit_payment_not_authorized_dest() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();

        let issuer_id = create_test_account_id(9);
        let source_id = create_test_account_id(0);
        let dest_id = create_test_account_id(1);
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(dest_id.clone(), 100_000_000));
        state.get_account_mut(&issuer_id).unwrap().flags = AccountFlags::RequiredFlag as u32;

        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
            issuer: issuer_id.clone(),
        });
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            100,
            1_000_000,
            AUTHORIZED_FLAG,
        ));
        state.create_trustline(create_test_trustline(
            dest_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            0,
            1_000_000,
            0,
        ));

        let op = PaymentOp {
            destination: create_test_muxed_account(1),
            asset,
            amount: 10,
        };

        let result = execute_payment(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::Payment(r)) => {
                assert!(matches!(r, PaymentResult::NotAuthorized));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_credit_payment_line_full_with_liabilities() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();

        let issuer_id = create_test_account_id(9);
        let source_id = create_test_account_id(0);
        let dest_id = create_test_account_id(1);
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(dest_id.clone(), 100_000_000));

        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
            issuer: issuer_id.clone(),
        });
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            100,
            1_000_000,
            AUTHORIZED_FLAG,
        ));
        state.create_trustline(create_test_trustline_with_liabilities(
            dest_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            90,
            100,
            AUTHORIZED_FLAG,
            10,
            0,
        ));

        let op = PaymentOp {
            destination: create_test_muxed_account(1),
            asset,
            amount: 1,
        };

        let result = execute_payment(&op, &source_id, &mut state, &context);
        assert!(result.is_ok());

        match result.unwrap() {
            OperationResult::OpInner(OperationResultTr::Payment(r)) => {
                assert!(matches!(r, PaymentResult::LineFull));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_credit_payment_success_no_auth_required() {
        // When issuer doesn't have AUTH_REQUIRED_FLAG, trustlines are automatically
        // authorized (per ChangeTrust logic). This test verifies payments work in this case.
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();

        let issuer_id = create_test_account_id(9);
        let source_id = create_test_account_id(0);
        let dest_id = create_test_account_id(1);
        // Issuer has no AUTH_REQUIRED_FLAG (flags=0)
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(dest_id.clone(), 100_000_000));

        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
            issuer: issuer_id.clone(),
        });
        // Trustlines are automatically authorized when issuer has no AUTH_REQUIRED
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            100,
            1_000_000,
            AUTHORIZED_FLAG,
        ));
        state.create_trustline(create_test_trustline(
            dest_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            0,
            1_000_000,
            AUTHORIZED_FLAG,
        ));

        let op = PaymentOp {
            destination: create_test_muxed_account(1),
            asset,
            amount: 10,
        };

        let result = execute_payment(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::Payment(r)) => {
                assert!(matches!(r, PaymentResult::Success));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_credit_payment_from_issuer_without_trustline() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();

        let issuer_id = create_test_account_id(9);
        let dest_id = create_test_account_id(1);
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));
        state.create_account(create_test_account(dest_id.clone(), 100_000_000));

        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
            issuer: issuer_id.clone(),
        });
        // Trustline is auto-authorized when issuer has no AUTH_REQUIRED
        state.create_trustline(create_test_trustline(
            dest_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            0,
            1_000_000,
            AUTHORIZED_FLAG,
        ));

        let op = PaymentOp {
            destination: create_test_muxed_account(1),
            asset,
            amount: 10,
        };

        let result = execute_payment(&op, &issuer_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::Payment(r)) => {
                assert!(matches!(r, PaymentResult::Success));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_credit_payment_to_issuer_without_trustline() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();

        let issuer_id = create_test_account_id(9);
        let source_id = create_test_account_id(0);
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
            issuer: issuer_id.clone(),
        });
        // Trustline is auto-authorized when issuer has no AUTH_REQUIRED
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'C']),
                issuer: issuer_id.clone(),
            }),
            100,
            1_000_000,
            AUTHORIZED_FLAG,
        ));

        let op = PaymentOp {
            destination: create_test_muxed_account(9),
            asset,
            amount: 10,
        };

        let result = execute_payment(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::Payment(r)) => {
                assert!(matches!(r, PaymentResult::Success));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    #[test]
    fn test_credit_self_payment_with_zero_balance_succeeds() {
        // This tests the critical self-payment behavior where source == destination.
        // Even with zero balance, a self-payment should succeed because:
        // 1. The destination trustline is credited first (+amount)
        // 2. Then the source trustline is debited (-amount)
        // Since they're the same trustline, the credit makes the balance available for debit.
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();

        let issuer_id = create_test_account_id(9);
        let account_id = create_test_account_id(0);
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));
        state.create_account(create_test_account(account_id.clone(), 100_000_000));

        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', b'S', b'D', b'Z']),
            issuer: issuer_id.clone(),
        });
        // Create trustline with ZERO balance
        state.create_trustline(create_test_trustline(
            account_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'Z']),
                issuer: issuer_id.clone(),
            }),
            0, // Zero balance!
            1_000_000_000,
            AUTHORIZED_FLAG,
        ));

        // Self-payment: account pays itself 20,000 USDZ
        let op = PaymentOp {
            destination: create_test_muxed_account(0), // Same as source
            asset,
            amount: 200_000_000, // 20,000 USDZ (7 decimals)
        };

        let result = execute_payment(&op, &account_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::Payment(r)) => {
                assert!(matches!(r, PaymentResult::Success));
            }
            _ => panic!("Unexpected result type"),
        }

        // Balance should still be zero after self-payment
        let trustline = state
            .get_trustline(
                &account_id,
                &Asset::CreditAlphanum4(AlphaNum4 {
                    asset_code: AssetCode4([b'U', b'S', b'D', b'Z']),
                    issuer: issuer_id.clone(),
                }),
            )
            .unwrap();
        assert_eq!(trustline.balance, 0);
    }

    #[test]
    fn test_credit_self_payment_line_full() {
        // Self-payment should fail with LineFull if amount exceeds available room
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();

        let issuer_id = create_test_account_id(9);
        let account_id = create_test_account_id(0);
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));
        state.create_account(create_test_account(account_id.clone(), 100_000_000));

        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', b'S', b'D', b'Z']),
            issuer: issuer_id.clone(),
        });
        // Create trustline with balance near limit
        state.create_trustline(create_test_trustline(
            account_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', b'Z']),
                issuer: issuer_id.clone(),
            }),
            90,  // Balance = 90
            100, // Limit = 100, only 10 room
            AUTHORIZED_FLAG,
        ));

        // Self-payment of 20 exceeds available room (10)
        let op = PaymentOp {
            destination: create_test_muxed_account(0), // Same as source
            asset,
            amount: 20,
        };

        let result = execute_payment(&op, &account_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::Payment(r)) => {
                assert!(matches!(r, PaymentResult::LineFull));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test credit payment from issuer to trustline holder.
    /// Issuer has infinite supply so their balance doesn't change.
    ///
    /// C++ Reference: PaymentTests.cpp - "issuer large amounts" section
    #[test]
    fn test_credit_payment_issuer_to_holder() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();

        let issuer_id = create_test_account_id(0);
        let holder_id = create_test_account_id(1);
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));
        state.create_account(create_test_account(holder_id.clone(), 100_000_000));

        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id.clone(),
        });

        // Create trustline for holder to receive the asset
        state.create_trustline(create_test_trustline(
            holder_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"USD\0"),
                issuer: issuer_id.clone(),
            }),
            0, // Start with 0 balance
            1_000_000_000,
            AUTHORIZED_FLAG,
        ));

        // Issuer pays holder 1000 units
        let op = PaymentOp {
            destination: create_test_muxed_account(1),
            asset,
            amount: 1000,
        };

        let result = execute_payment(&op, &issuer_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::Payment(r)) => {
                assert!(matches!(r, PaymentResult::Success));
            }
            _ => panic!("Unexpected result type"),
        }

        // Verify holder received the payment
        let trustline = state
            .get_trustline(
                &holder_id,
                &Asset::CreditAlphanum4(AlphaNum4 {
                    asset_code: AssetCode4(*b"USD\0"),
                    issuer: issuer_id.clone(),
                }),
            )
            .unwrap();
        assert_eq!(trustline.balance, 1000);

        // Issuer's XLM balance unchanged (issuer doesn't need trustline for own asset)
        let issuer_account = state.get_account(&issuer_id).unwrap();
        assert_eq!(issuer_account.balance, 100_000_000);
    }

    /// Test credit payment from trustline holder to issuer.
    /// The issuer absorbs the payment (their balance is conceptually infinite).
    ///
    /// C++ Reference: PaymentTests.cpp - holder to issuer test
    #[test]
    fn test_credit_payment_holder_to_issuer() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();

        let issuer_id = create_test_account_id(0);
        let holder_id = create_test_account_id(1);
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));
        state.create_account(create_test_account(holder_id.clone(), 100_000_000));

        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"EUR\0"),
            issuer: issuer_id.clone(),
        });

        // Create trustline for holder with some balance
        state.create_trustline(create_test_trustline(
            holder_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"EUR\0"),
                issuer: issuer_id.clone(),
            }),
            5000, // Holder has 5000 units
            1_000_000_000,
            AUTHORIZED_FLAG,
        ));

        // Holder pays issuer 3000 units
        let op = PaymentOp {
            destination: create_test_muxed_account(0), // Issuer
            asset,
            amount: 3000,
        };

        let result = execute_payment(&op, &holder_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::Payment(r)) => {
                assert!(matches!(r, PaymentResult::Success));
            }
            _ => panic!("Unexpected result type"),
        }

        // Verify holder's trustline was debited
        let trustline = state
            .get_trustline(
                &holder_id,
                &Asset::CreditAlphanum4(AlphaNum4 {
                    asset_code: AssetCode4(*b"EUR\0"),
                    issuer: issuer_id.clone(),
                }),
            )
            .unwrap();
        assert_eq!(trustline.balance, 2000); // 5000 - 3000
    }

    /// Test credit payment respects destination buying liabilities.
    /// Available room = limit - balance - buyingLiabilities
    ///
    /// C++ Reference: PaymentTests.cpp - "with buying liabilities" section
    #[test]
    fn test_credit_payment_destination_buying_liabilities() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();

        let issuer_id = create_test_account_id(0);
        let source_id = create_test_account_id(1);
        let dest_id = create_test_account_id(2);
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));
        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(dest_id.clone(), 100_000_000));

        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"JPY\0"),
            issuer: issuer_id.clone(),
        });

        // Create source trustline with balance
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"JPY\0"),
                issuer: issuer_id.clone(),
            }),
            5000,
            1_000_000_000,
            AUTHORIZED_FLAG,
        ));

        // Create destination trustline with limit=1000, balance=500, buying_liabilities=400
        // Available room = 1000 - 500 - 400 = 100
        let mut dest_trustline = create_test_trustline(
            dest_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"JPY\0"),
                issuer: issuer_id.clone(),
            }),
            500,
            1000,
            AUTHORIZED_FLAG,
        );
        dest_trustline.ext = TrustLineEntryExt::V1(TrustLineEntryV1 {
            liabilities: Liabilities {
                buying: 400,
                selling: 0,
            },
            ext: TrustLineEntryV1Ext::V0,
        });
        state.create_trustline(dest_trustline);

        // Try to pay 200 - should fail with LineFull (only 100 room available)
        let op = PaymentOp {
            destination: create_test_muxed_account(2),
            asset: asset.clone(),
            amount: 200,
        };

        let result = execute_payment(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::Payment(r)) => {
                assert!(
                    matches!(r, PaymentResult::LineFull),
                    "Expected LineFull due to buying liabilities, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }

        // Pay exactly 100 - should succeed
        let op_exact = PaymentOp {
            destination: create_test_muxed_account(2),
            asset,
            amount: 100,
        };

        let result = execute_payment(&op_exact, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::Payment(r)) => {
                assert!(matches!(r, PaymentResult::Success));
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test native payment to self succeeds (no-op, but valid).
    ///
    /// C++ Reference: PaymentTests.cpp - "pay self" test section
    #[test]
    fn test_native_payment_to_self() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();

        let source_id = create_test_account_id(50);
        state.create_account(create_test_account(source_id.clone(), 100_000_000));

        // Pay self
        let op = PaymentOp {
            destination: create_test_muxed_account(50), // Same as source
            asset: Asset::Native,
            amount: 1000,
        };

        let result = execute_payment(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::Payment(r)) => {
                assert!(
                    matches!(r, PaymentResult::Success),
                    "Self-payment should succeed"
                );
            }
            _ => panic!("Unexpected result type"),
        }

        // Balance should be unchanged (self-payment is a no-op)
        assert_eq!(state.get_account(&source_id).unwrap().balance, 100_000_000);
    }

    /// Test native payment source only has minimum reserve fails with Underfunded.
    ///
    /// C++ Reference: PaymentTests.cpp - "source only has reserve" test section
    #[test]
    fn test_native_payment_source_only_has_reserve() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();

        let source_id = create_test_account_id(51);
        let dest_id = create_test_account_id(52);

        // Calculate exact minimum balance
        let min_balance = state
            .minimum_balance_with_counts(context.protocol_version, 0, 0, 0)
            .unwrap();

        // Source has exactly minimum balance
        state.create_account(create_test_account(source_id.clone(), min_balance));
        state.create_account(create_test_account(dest_id.clone(), 100_000_000));

        // Try to pay any amount - should fail
        let op = PaymentOp {
            destination: create_test_muxed_account(52),
            asset: Asset::Native,
            amount: 1,
        };

        let result = execute_payment(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::Payment(r)) => {
                assert!(
                    matches!(r, PaymentResult::Underfunded),
                    "Expected Underfunded when source only has reserve, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test native payment negative amount returns Malformed.
    ///
    /// C++ Reference: PaymentTests.cpp - "malformed negative amount" test section
    #[test]
    fn test_native_payment_negative_amount() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();

        let source_id = create_test_account_id(53);
        let dest_id = create_test_account_id(54);

        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(dest_id.clone(), 100_000_000));

        let op = PaymentOp {
            destination: create_test_muxed_account(54),
            asset: Asset::Native,
            amount: -1,
        };

        let result = execute_payment(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::Payment(r)) => {
                assert!(
                    matches!(r, PaymentResult::Malformed),
                    "Expected Malformed for negative amount, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test credit payment underfunded - source has zero balance in trustline.
    ///
    /// C++ Reference: PaymentTests.cpp - "underfunded credit" test section
    #[test]
    fn test_credit_payment_underfunded() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();

        let source_id = create_test_account_id(55);
        let dest_id = create_test_account_id(56);
        let issuer_id = create_test_account_id(57);

        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(dest_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id.clone(),
        });

        // Source trustline with zero balance
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"USD\0"),
                issuer: issuer_id.clone(),
            }),
            0, // Zero balance
            1_000_000,
            AUTHORIZED_FLAG,
        ));

        // Destination trustline
        state.create_trustline(create_test_trustline(
            dest_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"USD\0"),
                issuer: issuer_id,
            }),
            0,
            1_000_000,
            AUTHORIZED_FLAG,
        ));

        let op = PaymentOp {
            destination: create_test_muxed_account(56),
            asset,
            amount: 100,
        };

        let result = execute_payment(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::Payment(r)) => {
                assert!(
                    matches!(r, PaymentResult::Underfunded),
                    "Expected Underfunded for zero balance, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test credit payment line full - destination trustline at limit.
    ///
    /// C++ Reference: PaymentTests.cpp - "line full" test section
    #[test]
    fn test_credit_payment_line_full() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();

        let source_id = create_test_account_id(58);
        let dest_id = create_test_account_id(59);
        let issuer_id = create_test_account_id(60);

        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(dest_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id.clone(),
        });

        // Source trustline with balance
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"USD\0"),
                issuer: issuer_id.clone(),
            }),
            1000,
            1_000_000,
            AUTHORIZED_FLAG,
        ));

        // Destination trustline already at limit
        state.create_trustline(create_test_trustline(
            dest_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"USD\0"),
                issuer: issuer_id,
            }),
            1000,
            1000, // Limit equals balance - no room
            AUTHORIZED_FLAG,
        ));

        let op = PaymentOp {
            destination: create_test_muxed_account(59),
            asset,
            amount: 100,
        };

        let result = execute_payment(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::Payment(r)) => {
                assert!(
                    matches!(r, PaymentResult::LineFull),
                    "Expected LineFull when dest at limit, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test credit payment source no trust - paying credit without trustline.
    ///
    /// C++ Reference: PaymentTests.cpp - "src no trust" test section
    #[test]
    fn test_credit_payment_source_no_trust() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();

        let source_id = create_test_account_id(61);
        let dest_id = create_test_account_id(62);
        let issuer_id = create_test_account_id(63);

        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(dest_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id.clone(),
        });

        // No trustline for source - only for dest
        state.create_trustline(create_test_trustline(
            dest_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"USD\0"),
                issuer: issuer_id,
            }),
            0,
            1_000_000,
            AUTHORIZED_FLAG,
        ));

        let op = PaymentOp {
            destination: create_test_muxed_account(62),
            asset,
            amount: 100,
        };

        let result = execute_payment(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::Payment(r)) => {
                assert!(
                    matches!(r, PaymentResult::SrcNoTrust),
                    "Expected SrcNoTrust, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test payment with invalid asset code returns Malformed.
    ///
    /// stellar-core's doCheckValid calls isAssetValid() after checking amount > 0.
    /// An all-zero asset code is not valid.
    ///
    /// C++ Reference: PaymentOpFrame.cpp:136-140
    #[test]
    fn test_payment_invalid_asset_code_returns_malformed() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();

        let source_id = create_test_account_id(70);
        let dest_id = create_test_account_id(71);
        let issuer_id = create_test_account_id(72);

        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(dest_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        // All-zero asset code is invalid
        let op = PaymentOp {
            destination: create_test_muxed_account(71),
            asset: Asset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([0, 0, 0, 0]),
                issuer: issuer_id.clone(),
            }),
            amount: 100,
        };

        let result = execute_payment(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::Payment(r)) => {
                assert!(
                    matches!(r, PaymentResult::Malformed),
                    "Expected Malformed for all-zero asset code, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }

        // Non-alphanumeric character in asset code
        let op2 = PaymentOp {
            destination: create_test_muxed_account(71),
            asset: Asset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'$', 0]),
                issuer: issuer_id,
            }),
            amount: 100,
        };

        let result2 = execute_payment(&op2, &source_id, &mut state, &context).unwrap();
        match result2 {
            OperationResult::OpInner(OperationResultTr::Payment(r)) => {
                assert!(
                    matches!(r, PaymentResult::Malformed),
                    "Expected Malformed for non-alphanumeric asset code, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test native payment returns LineFull before Underfunded.
    ///
    /// stellar-core delegates ALL payments (including native) to PathPaymentStrictReceive,
    /// which credits dest first. When dest would overflow AND source is underfunded,
    /// the result should be LineFull (not Underfunded).
    ///
    /// C++ Reference: PaymentOpFrame.cpp:56-116
    #[test]
    fn test_native_payment_line_full_before_underfunded() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();

        let source_id = create_test_account_id(80);
        let dest_id = create_test_account_id(81);

        // Source has very little available balance (minimum reserve + small amount)
        let min_balance = state
            .minimum_balance_with_counts(context.protocol_version, 0, 0, 0)
            .unwrap();
        state.create_account(create_test_account(source_id.clone(), min_balance + 100));

        // Dest has high balance + buying liabilities near i64::MAX so any credit overflows
        state.create_account(create_test_account_with_liabilities(
            dest_id.clone(),
            i64::MAX - 10, // Very high balance
            20,            // Buying liabilities that make dest LineFull
            0,
        ));

        // Amount of 1000 exceeds both source available (100) and dest capacity
        let op = PaymentOp {
            destination: create_test_muxed_account(81),
            asset: Asset::Native,
            amount: 1000,
        };

        let result = execute_payment(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::Payment(r)) => {
                assert!(
                    matches!(r, PaymentResult::LineFull),
                    "Expected LineFull (dest credited first via PathPayment delegation), got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// Test credit payment destination no trust.
    ///
    /// C++ Reference: PaymentTests.cpp - "no trust dest" test section
    #[test]
    fn test_credit_payment_no_trust_dest() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();

        let source_id = create_test_account_id(64);
        let dest_id = create_test_account_id(65);
        let issuer_id = create_test_account_id(66);

        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(dest_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: issuer_id.clone(),
        });

        // Source has trustline, dest doesn't
        state.create_trustline(create_test_trustline(
            source_id.clone(),
            TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4(*b"USD\0"),
                issuer: issuer_id,
            }),
            1000,
            1_000_000,
            AUTHORIZED_FLAG,
        ));

        let op = PaymentOp {
            destination: create_test_muxed_account(65),
            asset,
            amount: 100,
        };

        let result = execute_payment(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::Payment(r)) => {
                assert!(
                    matches!(r, PaymentResult::NoTrust),
                    "Expected NoTrust for dest, got {:?}",
                    r
                );
            }
            _ => panic!("Unexpected result type"),
        }
    }

    /// NoIssuer is unreachable since protocol 13+ (CAP-0017 removed issuer checks).
    /// This test documents that the code path is dead for protocol 24+.
    #[test]
    #[ignore = "NoIssuer is unreachable since protocol 13+ (CAP-0017)"]
    fn test_payment_no_issuer() {
        // NoIssuer would require check_issuer() to fail, but it's a no-op since protocol 13.
        // PaymentResult::NoIssuer is defined in XDR but never produced.
    }

    /// Payment of a credit asset fails with `SrcNoTrust` when the source account
    /// has no trustline for the asset being sent, even though the destination does.
    #[test]
    fn test_payment_src_no_trust() {
        let mut state = LedgerStateManager::new(5_000_000, 100.into());
        let context = create_test_context();

        let source_id = create_test_account_id(1);
        let dest_id = create_test_account_id(2);
        let issuer_id = create_test_account_id(3);

        state.create_account(create_test_account(source_id.clone(), 100_000_000));
        state.create_account(create_test_account(dest_id.clone(), 100_000_000));
        state.create_account(create_test_account(issuer_id.clone(), 100_000_000));

        let asset = create_test_asset(b"USD\0", issuer_id.clone());
        let tl_asset = create_test_trustline_asset(b"USD\0", issuer_id);

        // Destination has an authorized trustline; source does NOT.
        state.create_trustline(create_test_trustline(
            dest_id,
            tl_asset,
            0,
            1_000_000,
            AUTHORIZED_FLAG,
        ));

        let op = PaymentOp {
            destination: MuxedAccount::Ed25519(Uint256([2; 32])),
            asset,
            amount: 100,
        };

        let result = execute_payment(&op, &source_id, &mut state, &context).unwrap();
        match result {
            OperationResult::OpInner(OperationResultTr::Payment(r)) => {
                assert!(
                    matches!(r, PaymentResult::SrcNoTrust),
                    "Expected SrcNoTrust, got {:?}",
                    r
                );
            }
            other => panic!("unexpected: {:?}", other),
        }
    }
}
