//! Shared test utilities for transaction tests.
//!
//! This module provides common test helpers matching stellar-core SponsorshipTestUtils
//! and TestUtils patterns. It consolidates test utilities that were previously
//! duplicated across individual operation test modules.

use stellar_xdr::curr::{
    AccountEntry, AccountEntryExt, AccountEntryExtensionV1, AccountEntryExtensionV1Ext,
    AccountEntryExtensionV2, AccountEntryExtensionV2Ext, AccountFlags, AccountId, AlphaNum4, Asset,
    AssetCode4, Liabilities, PublicKey, SequenceNumber, String32, Thresholds, TrustLineAsset,
    TrustLineEntry, TrustLineEntryExt, TrustLineEntryExtensionV2, TrustLineEntryExtensionV2Ext,
    TrustLineEntryV1, TrustLineEntryV1Ext, TrustLineFlags, Uint256,
};

use crate::validation::LedgerContext;

/// Maximum number of sub-entries per account (trustlines, offers, data entries, signers).
/// Matches stellar-core ACCOUNT_SUBENTRY_LIMIT.
pub const ACCOUNT_SUBENTRY_LIMIT: u32 = 1000;

/// Maximum number of signers per account.
pub const MAX_SIGNERS: u32 = 20;

/// Maximum INT64 value for overflow testing.
pub const MAX_INT64: i64 = i64::MAX;

/// Value near MAX_INT64 for boundary testing.
pub const NEAR_MAX_INT64: i64 = i64::MAX - 1_000_000;

// ============================================================================
// Account creation helpers
// ============================================================================

/// Create a test account ID from a seed byte.
/// Different seeds produce different account IDs.
pub fn create_test_account_id(seed: u8) -> AccountId {
    AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([seed; 32])))
}

/// Create a basic test account with specified balance.
pub fn create_test_account(account_id: AccountId, balance: i64) -> AccountEntry {
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

/// Create a test account with custom flags (e.g., AUTH_REQUIRED, AUTH_IMMUTABLE).
pub fn create_test_account_with_flags(
    account_id: AccountId,
    balance: i64,
    flags: u32,
) -> AccountEntry {
    let mut account = create_test_account(account_id, balance);
    account.flags = flags;
    account
}

/// Create a test account with specified subentries count.
pub fn create_test_account_with_subentries(
    account_id: AccountId,
    balance: i64,
    num_sub_entries: u32,
) -> AccountEntry {
    let mut account = create_test_account(account_id, balance);
    account.num_sub_entries = num_sub_entries;
    account
}

/// Create a test account at the subentry limit (1000).
pub fn create_account_at_subentry_limit(account_id: AccountId, balance: i64) -> AccountEntry {
    create_test_account_with_subentries(account_id, balance, ACCOUNT_SUBENTRY_LIMIT)
}

/// Create a test account one below the subentry limit (999).
pub fn create_account_near_subentry_limit(account_id: AccountId, balance: i64) -> AccountEntry {
    create_test_account_with_subentries(account_id, balance, ACCOUNT_SUBENTRY_LIMIT - 1)
}

/// Create a test account with liabilities set.
pub fn create_test_account_with_liabilities(
    account_id: AccountId,
    balance: i64,
    buying_liabilities: i64,
    selling_liabilities: i64,
) -> AccountEntry {
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
        ext: AccountEntryExt::V1(AccountEntryExtensionV1 {
            liabilities: Liabilities {
                buying: buying_liabilities,
                selling: selling_liabilities,
            },
            ext: AccountEntryExtensionV1Ext::V0,
        }),
    }
}

/// Create a test account with sponsorship extension (V2).
pub fn create_test_account_with_sponsorship(
    account_id: AccountId,
    balance: i64,
    num_sub_entries: u32,
    num_sponsored: u32,
    num_sponsoring: u32,
) -> AccountEntry {
    AccountEntry {
        account_id,
        balance,
        seq_num: SequenceNumber(1),
        num_sub_entries,
        inflation_dest: None,
        flags: 0,
        home_domain: String32::default(),
        thresholds: Thresholds([1, 0, 0, 0]),
        signers: vec![].try_into().unwrap(),
        ext: AccountEntryExt::V1(AccountEntryExtensionV1 {
            liabilities: Liabilities {
                buying: 0,
                selling: 0,
            },
            ext: AccountEntryExtensionV1Ext::V2(AccountEntryExtensionV2 {
                num_sponsored,
                num_sponsoring,
                signer_sponsoring_i_ds: vec![].try_into().unwrap(),
                ext: AccountEntryExtensionV2Ext::V0,
            }),
        }),
    }
}

/// Create a test account with balance near INT64_MAX.
pub fn create_account_near_max_balance(account_id: AccountId) -> AccountEntry {
    create_test_account(account_id, NEAR_MAX_INT64)
}

/// Create a test account with max buying liabilities.
pub fn create_account_with_max_buying_liabilities(
    account_id: AccountId,
    balance: i64,
) -> AccountEntry {
    create_test_account_with_liabilities(account_id, balance, NEAR_MAX_INT64, 0)
}

// ============================================================================
// Trustline creation helpers
// ============================================================================

/// Create a basic test trustline.
pub fn create_test_trustline(
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

/// Create a trustline with liabilities.
pub fn create_trustline_with_liabilities(
    account_id: AccountId,
    asset: TrustLineAsset,
    balance: i64,
    limit: i64,
    flags: u32,
    buying_liabilities: i64,
    selling_liabilities: i64,
) -> TrustLineEntry {
    TrustLineEntry {
        account_id,
        asset,
        balance,
        limit,
        flags,
        ext: TrustLineEntryExt::V1(TrustLineEntryV1 {
            liabilities: Liabilities {
                buying: buying_liabilities,
                selling: selling_liabilities,
            },
            ext: TrustLineEntryV1Ext::V0,
        }),
    }
}

/// Create a trustline with pool use count (for liquidity pool testing).
pub fn trustline_with_pool_use_count(
    account_id: AccountId,
    asset: TrustLineAsset,
    balance: i64,
    limit: i64,
    flags: u32,
    pool_use_count: i32,
) -> TrustLineEntry {
    TrustLineEntry {
        account_id,
        asset,
        balance,
        limit,
        flags,
        ext: TrustLineEntryExt::V1(TrustLineEntryV1 {
            liabilities: Liabilities {
                buying: 0,
                selling: 0,
            },
            ext: TrustLineEntryV1Ext::V2(TrustLineEntryExtensionV2 {
                liquidity_pool_use_count: pool_use_count,
                ext: TrustLineEntryExtensionV2Ext::V0,
            }),
        }),
    }
}

/// Create a trustline near INT64_MAX balance.
pub fn create_trustline_near_max_balance(
    account_id: AccountId,
    asset: TrustLineAsset,
    limit: i64,
) -> TrustLineEntry {
    create_test_trustline(
        account_id,
        asset,
        NEAR_MAX_INT64,
        limit,
        TrustLineFlags::AuthorizedFlag as u32,
    )
}

// ============================================================================
// Asset helpers
// ============================================================================

/// Create a test credit asset (AlphaNum4).
pub fn create_test_asset(code: &[u8; 4], issuer: AccountId) -> Asset {
    Asset::CreditAlphanum4(AlphaNum4 {
        asset_code: AssetCode4(*code),
        issuer,
    })
}

/// Create a test trustline asset (AlphaNum4).
pub fn create_test_trustline_asset(code: &[u8; 4], issuer: AccountId) -> TrustLineAsset {
    TrustLineAsset::CreditAlphanum4(AlphaNum4 {
        asset_code: AssetCode4(*code),
        issuer,
    })
}

// ============================================================================
// Context helpers
// ============================================================================

/// Create a standard test context (testnet, ledger 1, seq 1000).
pub fn create_test_context() -> LedgerContext {
    LedgerContext::testnet(1, 1000)
}

/// Create a test context with specific protocol version.
pub fn create_test_context_with_protocol(protocol_version: u32) -> LedgerContext {
    let mut context = LedgerContext::testnet(1, 1000);
    context.protocol_version = protocol_version;
    context
}

// ============================================================================
// Result assertion helpers
// ============================================================================

/// Assert that an operation result is OpTooManySubentries.
#[track_caller]
pub fn assert_too_many_subentries(result: &stellar_xdr::curr::OperationResult) {
    match result {
        stellar_xdr::curr::OperationResult::OpTooManySubentries => {}
        other => panic!("expected OpTooManySubentries, got {:?}", other),
    }
}

/// Assert that an operation result is OpTooManySponsoring.
#[track_caller]
pub fn assert_too_many_sponsoring(result: &stellar_xdr::curr::OperationResult) {
    match result {
        stellar_xdr::curr::OperationResult::OpTooManySponsoring => {}
        other => panic!("expected OpTooManySponsoring, got {:?}", other),
    }
}

// ============================================================================
// Account flag constants (matching stellar-core)
// ============================================================================

/// AUTH_REQUIRED_FLAG - trustlines require authorization from issuer.
pub const AUTH_REQUIRED_FLAG: u32 = AccountFlags::RequiredFlag as u32;

/// AUTH_REVOCABLE_FLAG - trustlines can be revoked by issuer.
pub const AUTH_REVOCABLE_FLAG: u32 = AccountFlags::RevocableFlag as u32;

/// AUTH_IMMUTABLE_FLAG - account flags cannot be changed.
pub const AUTH_IMMUTABLE_FLAG: u32 = AccountFlags::ImmutableFlag as u32;

/// AUTH_CLAWBACK_ENABLED_FLAG - issuer can clawback assets.
pub const AUTH_CLAWBACK_FLAG: u32 = AccountFlags::ClawbackEnabledFlag as u32;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_test_account_id_uniqueness() {
        let id1 = create_test_account_id(0);
        let id2 = create_test_account_id(1);
        let id3 = create_test_account_id(0);

        // Different seeds produce different IDs
        assert_ne!(id1, id2, "different seeds should produce different IDs");

        // Same seed produces same ID
        assert_eq!(id1, id3, "same seed should produce same ID");
    }

    #[test]
    fn test_create_account_at_subentry_limit() {
        let account = create_account_at_subentry_limit(create_test_account_id(0), 100_000_000);
        assert_eq!(account.num_sub_entries, ACCOUNT_SUBENTRY_LIMIT);
        assert_eq!(account.num_sub_entries, 1000);
    }

    #[test]
    fn test_create_account_with_liabilities() {
        let account =
            create_test_account_with_liabilities(create_test_account_id(0), 100_000_000, 500, 300);

        match &account.ext {
            AccountEntryExt::V1(v1) => {
                assert_eq!(v1.liabilities.buying, 500);
                assert_eq!(v1.liabilities.selling, 300);
            }
            _ => panic!("expected V1 extension"),
        }
    }

    #[test]
    fn test_create_account_with_sponsorship() {
        let account = create_test_account_with_sponsorship(
            create_test_account_id(0),
            100_000_000,
            5, // num_sub_entries
            2, // num_sponsored
            3, // num_sponsoring
        );

        assert_eq!(account.num_sub_entries, 5);

        match &account.ext {
            AccountEntryExt::V1(v1) => match &v1.ext {
                AccountEntryExtensionV1Ext::V2(v2) => {
                    assert_eq!(v2.num_sponsored, 2);
                    assert_eq!(v2.num_sponsoring, 3);
                }
                _ => panic!("expected V2 extension"),
            },
            _ => panic!("expected V1 extension"),
        }
    }

    #[test]
    fn test_create_trustline_with_liabilities() {
        let tl = create_trustline_with_liabilities(
            create_test_account_id(0),
            create_test_trustline_asset(b"USD\0", create_test_account_id(1)),
            1000,
            10000,
            TrustLineFlags::AuthorizedFlag as u32,
            200,
            100,
        );

        match &tl.ext {
            TrustLineEntryExt::V1(v1) => {
                assert_eq!(v1.liabilities.buying, 200);
                assert_eq!(v1.liabilities.selling, 100);
            }
            _ => panic!("expected V1 extension"),
        }
    }

    #[test]
    fn test_trustline_with_pool_use_count() {
        let tl = trustline_with_pool_use_count(
            create_test_account_id(0),
            create_test_trustline_asset(b"USD\0", create_test_account_id(1)),
            0,
            10000,
            TrustLineFlags::AuthorizedFlag as u32,
            2,
        );

        match &tl.ext {
            TrustLineEntryExt::V1(v1) => match &v1.ext {
                TrustLineEntryV1Ext::V2(v2) => {
                    assert_eq!(v2.liquidity_pool_use_count, 2);
                }
                _ => panic!("expected V2 extension"),
            },
            _ => panic!("expected V1 extension"),
        }
    }

    /// Test create_test_account basic functionality.
    #[test]
    fn test_create_test_account_basic() {
        let id = create_test_account_id(5);
        let account = create_test_account(id.clone(), 1_000_000);

        assert_eq!(account.account_id, id);
        assert_eq!(account.balance, 1_000_000);
        assert_eq!(account.seq_num.0, 1);
        assert_eq!(account.num_sub_entries, 0);
        assert_eq!(account.flags, 0);
    }

    /// Test create_test_account_with_flags.
    #[test]
    fn test_create_test_account_with_flags() {
        let id = create_test_account_id(6);
        let auth_required = AccountFlags::RequiredFlag as u32;
        let account = create_test_account_with_flags(id.clone(), 2_000_000, auth_required);

        assert_eq!(account.account_id, id);
        assert_eq!(account.balance, 2_000_000);
        assert_eq!(account.flags, auth_required);
    }

    /// Test create_test_account_with_subentries.
    #[test]
    fn test_create_test_account_with_subentries() {
        let id = create_test_account_id(7);
        let account = create_test_account_with_subentries(id.clone(), 3_000_000, 50);

        assert_eq!(account.account_id, id);
        assert_eq!(account.balance, 3_000_000);
        assert_eq!(account.num_sub_entries, 50);
    }

    /// Test create_account_near_subentry_limit.
    #[test]
    fn test_create_account_near_subentry_limit() {
        let account = create_account_near_subentry_limit(create_test_account_id(8), 100_000_000);
        assert_eq!(account.num_sub_entries, ACCOUNT_SUBENTRY_LIMIT - 1);
        assert_eq!(account.num_sub_entries, 999);
    }

    /// Test create_test_asset.
    #[test]
    fn test_create_test_asset() {
        let issuer = create_test_account_id(10);
        let asset = create_test_asset(b"USDC", issuer.clone());

        match asset {
            Asset::CreditAlphanum4(alpha) => {
                assert_eq!(alpha.asset_code.0, *b"USDC");
                assert_eq!(alpha.issuer, issuer);
            }
            _ => panic!("expected CreditAlphanum4"),
        }
    }

    /// Test create_test_trustline_asset.
    #[test]
    fn test_create_test_trustline_asset() {
        let issuer = create_test_account_id(11);
        let asset = create_test_trustline_asset(b"EUR\0", issuer.clone());

        match asset {
            TrustLineAsset::CreditAlphanum4(alpha) => {
                assert_eq!(alpha.asset_code.0, *b"EUR\0");
                assert_eq!(alpha.issuer, issuer);
            }
            _ => panic!("expected CreditAlphanum4"),
        }
    }

    /// Test create_test_context.
    #[test]
    fn test_create_test_context() {
        let context = create_test_context();
        assert_eq!(context.sequence, 1);
        assert_eq!(context.close_time, 1000);
    }

    /// Test constants.
    #[test]
    fn test_constants() {
        assert_eq!(ACCOUNT_SUBENTRY_LIMIT, 1000);
        assert_eq!(MAX_SIGNERS, 20);
        assert_eq!(MAX_INT64, i64::MAX);
        assert_eq!(NEAR_MAX_INT64, i64::MAX - 1_000_000);
    }
}
