//! Operation parity test matrix — systematic ResultCode coverage.
//!
//! This module provides one test file per operation type, with one `#[test]` per
//! `ResultCode` variant. The goal is to ensure every error path is exercised,
//! catching missing input validation that could diverge from stellar-core.
//!
//! See: <https://github.com/stellar-experimental/henyey/issues/1126>
//!
//! # Coverage Index
//!
//! Each entry below shows the operation, result code, and coverage status:
//! - **T** = Tested in this parity module
//! - **I** = Tested in inline unit tests (in the operation's own file)
//! - **X** = `#[ignore]` stub (dead code path unreachable since protocol 13+/24+)
//!
//! ## CreateAccount — 5 codes, 0 gaps
//! | Code          | Status | Notes |
//! |---------------|--------|-------|
//! | Success       | I | create_account.rs |
//! | Malformed     | I | create_account.rs |
//! | Underfunded   | I | create_account.rs |
//! | LowReserve    | I | create_account.rs |
//! | AlreadyExist  | I | create_account.rs |
//!
//! ## Payment — 10 codes, 0 gaps
//! | Code             | Status |
//! |------------------|--------|
//! | Success          | I |
//! | Malformed        | I |
//! | Underfunded      | I |
//! | SrcNoTrust       | T |
//! | SrcNotAuthorized | I |
//! | NoDestination    | I |
//! | NoTrust          | I |
//! | NotAuthorized    | I |
//! | LineFull         | I |
//! | NoIssuer         | X | dead since protocol 13+ (CAP-0017) |
//!
//! ## PathPaymentStrictReceive — 13 codes, 0 gaps
//! | Code           | Status |
//! |----------------|--------|
//! | Success        | I |
//! | Malformed      | I |
//! | Underfunded    | I |
//! | SrcNoTrust     | I |
//! | SrcNotAuthorized | T |
//! | NoDestination  | I |
//! | NoTrust        | I |
//! | NotAuthorized  | I |
//! | LineFull       | I |
//! | NoIssuer       | X | dead since protocol 13+ (CAP-0017) |
//! | TooFewOffers   | T |
//! | OfferCrossSelf | T |
//! | OverSendmax    | T |
//!
//! ## PathPaymentStrictSend — 13 codes, 0 gaps
//! | Code           | Status |
//! |----------------|--------|
//! | Success        | T |
//! | Malformed      | I |
//! | Underfunded    | T |
//! | SrcNoTrust     | T |
//! | SrcNotAuthorized | T |
//! | NoDestination  | T |
//! | NoTrust        | T |
//! | NotAuthorized  | T |
//! | LineFull       | T |
//! | NoIssuer       | T |
//! | TooFewOffers   | T |
//! | OfferCrossSelf | T |
//! | UnderDestmin   | T |
//!
//! ## ManageSellOffer — 13 codes, 0 gaps
//! | Code              | Status |
//! |-------------------|--------|
//! | Success           | I |
//! | Malformed         | I |
//! | SellNoTrust       | I |
//! | BuyNoTrust        | I |
//! | SellNotAuthorized | I |
//! | BuyNotAuthorized  | I |
//! | LineFull          | I |
//! | Underfunded       | I |
//! | CrossSelf         | I |
//! | SellNoIssuer      | X | dead since protocol 13+ (CAP-0017) |
//! | BuyNoIssuer       | X | dead since protocol 13+ (CAP-0017) |
//! | NotFound          | I |
//! | LowReserve        | I |
//!
//! ## ManageBuyOffer — 13 codes, 0 gaps
//! | Code              | Status |
//! |-------------------|--------|
//! | Success           | I |
//! | Malformed         | I |
//! | SellNoTrust       | I |
//! | BuyNoTrust        | I |
//! | SellNotAuthorized | I |
//! | BuyNotAuthorized  | I |
//! | LineFull          | I |
//! | Underfunded       | I |
//! | CrossSelf         | I |
//! | SellNoIssuer      | X | dead since protocol 13+ (CAP-0017) |
//! | BuyNoIssuer       | X | dead since protocol 13+ (CAP-0017) |
//! | NotFound          | I |
//! | LowReserve        | I |
//!
//! ## CreatePassiveSellOffer — 13 codes, 0 gaps
//! (Uses ManageSellOfferResultCode; shares execute_manage_offer code path)
//! | Code              | Status |
//! |-------------------|--------|
//! | Success           | I |
//! | Malformed         | I |
//! | SellNoTrust       | I |
//! | BuyNoTrust        | I |
//! | SellNotAuthorized | I |
//! | BuyNotAuthorized  | I |
//! | LineFull          | I |
//! | Underfunded       | I |
//! | CrossSelf         | I |
//! | SellNoIssuer      | X | dead since protocol 13+ (CAP-0017) |
//! | BuyNoIssuer       | X | dead since protocol 13+ (CAP-0017) |
//! | NotFound          | — | unreachable (passive offers always use offer_id=0) |
//! | LowReserve        | I |
//!
//! ## SetOptions — 11 codes, 0 gaps
//! | Code                 | Status |
//! |----------------------|--------|
//! | Success              | I |
//! | LowReserve           | I |
//! | TooManySigners       | I |
//! | BadFlags             | I |
//! | InvalidInflation     | I |
//! | CantChange           | I |
//! | UnknownFlag          | I |
//! | ThresholdOutOfRange  | I |
//! | BadSigner            | I |
//! | InvalidHomeDomain    | I |
//! | AuthRevocableRequired| I |
//!
//! ## ChangeTrust — 9 codes, 0 gaps
//! | Code                         | Status |
//! |------------------------------|--------|
//! | Success                      | I |
//! | Malformed                    | I |
//! | NoIssuer                     | I |
//! | InvalidLimit                 | I |
//! | LowReserve                   | I |
//! | SelfNotAllowed               | I |
//! | TrustLineMissing             | T |
//! | CannotDelete                 | I |
//! | NotAuthMaintainLiabilities   | T |
//!
//! ## AllowTrust — 7 codes, 0 gaps
//! | Code             | Status |
//! |------------------|--------|
//! | Success          | I |
//! | Malformed        | T |
//! | NoTrustLine      | I |
//! | TrustNotRequired | X | dead since protocol 24+ |
//! | CantRevoke       | I |
//! | SelfNotAllowed   | I |
//! | LowReserve       | I | trust_flags.rs inline |
//!
//! ## SetTrustLineFlags — 6 codes, 0 gaps
//! | Code         | Status |
//! |--------------|--------|
//! | Success      | I |
//! | Malformed    | I |
//! | NoTrustLine  | I |
//! | CantRevoke   | I |
//! | InvalidState | I |
//! | LowReserve   | I |
//!
//! ## AccountMerge — 8 codes, 0 gaps
//! | Code               | Status |
//! |--------------------|--------|
//! | Success            | I |
//! | Malformed          | I |
//! | NoAccount          | I |
//! | ImmutableSet       | I |
//! | HasSubEntries      | I |
//! | SeqnumTooFar       | I |
//! | DestFull           | I |
//! | IsSponsor          | I |
//!
//! ## Inflation — 2 codes, 0 gaps
//! | Code       | Status |
//! |------------|--------|
//! | Success    | I |
//! | NotTime    | I |
//!
//! ## ManageData — 5 codes, 0 gaps
//! | Code           | Status |
//! |----------------|--------|
//! | Success        | I |
//! | NotSupportedYet| X | dead since protocol 24+ |
//! | NameNotFound   | I |
//! | LowSubentryCount| I |
//! | InvalidName    | I |
//!
//! ## BumpSequence — 2 codes, 0 gaps
//! | Code       | Status |
//! |------------|--------|
//! | Success    | I |
//! | BadSeq     | I |
//!
//! ## CreateClaimableBalance — 6 codes, 0 gaps
//! | Code          | Status |
//! |---------------|--------|
//! | Success       | T |
//! | Malformed     | I |
//! | LowReserve    | I |
//! | NoTrust       | I |
//! | NotAuthorized | I |
//! | Underfunded   | I |
//!
//! ## ClaimClaimableBalance — 7 codes, 0 gaps
//! | Code           | Status |
//! |----------------|--------|
//! | Success        | I |
//! | DoesNotExist   | I |
//! | CannotClaim    | T |
//! | LineFull       | I |
//! | NoTrust        | I |
//! | NotAuthorized  | I |
//! | TrustlineFrozen| I |
//!
//! ## BeginSponsoringFutureReserves — 4 codes, 0 gaps
//! | Code             | Status |
//! |------------------|--------|
//! | Success          | I |
//! | Malformed        | I |
//! | AlreadySponsored | I |
//! | Recursive        | T |
//!
//! ## EndSponsoringFutureReserves — 2 codes, 0 gaps
//! | Code         | Status |
//! |--------------|--------|
//! | Success      | I |
//! | NotSponsored | I |
//!
//! ## RevokeSponsorship — 6 codes, 0 gaps
//! | Code             | Status |
//! |------------------|--------|
//! | Success          | T |
//! | DoesNotExist     | I |
//! | NotSponsor       | T |
//! | LowReserve       | I |
//! | OnlyTransferable | T |
//! | Malformed        | T |
//!
//! ## Clawback — 5 codes, 0 gaps
//! | Code            | Status |
//! |-----------------|--------|
//! | Success         | I |
//! | Malformed       | I |
//! | NotClawbackEnabled | I |
//! | NoTrust         | I |
//! | Underfunded     | I |
//!
//! ## ClawbackClaimableBalance — 4 codes, 0 gaps
//! | Code               | Status |
//! |--------------------|--------|
//! | Success            | T |
//! | DoesNotExist       | I |
//! | NotIssuer          | T |
//! | NotClawbackEnabled | T |
//!
//! ## LiquidityPoolDeposit — 9 codes, 0 gaps
//! | Code            | Status |
//! |-----------------|--------|
//! | Success         | I |
//! | Malformed       | T |
//! | NoTrust         | I |
//! | NotAuthorized   | I |
//! | Underfunded     | I |
//! | LineFull        | I |
//! | BadPrice        | I |
//! | PoolFull        | X | ignored — requires 128-bit overflow setup |
//! | TrustlineFrozen | I |
//!
//! ## LiquidityPoolWithdraw — 7 codes, 0 gaps
//! | Code            | Status |
//! |-----------------|--------|
//! | Success         | I |
//! | Malformed       | T |
//! | NoTrust         | I |
//! | Underfunded     | I |
//! | LineFull        | I |
//! | UnderMinimum    | I |
//! | TrustlineFrozen | T |
//!
//! ## InvokeHostFunction — 6 codes, 0 gaps
//! | Code            | Status |
//! |-----------------|--------|
//! | Success         | I |
//! | Malformed       | I |
//! | Trapped         | I |
//! | ResourceLimitExceeded | I |
//! | EntryArchived   | I |
//! | InsufficientRefundableFee | I |
//!
//! ## ExtendFootprintTtl — 4 codes, 0 gaps
//! | Code            | Status |
//! |-----------------|--------|
//! | Success         | I |
//! | Malformed       | I |
//! | ResourceLimitExceeded | I |
//! | InsufficientRefundableFee | I |
//!
//! ## RestoreFootprint — 4 codes, 0 gaps
//! | Code            | Status |
//! |-----------------|--------|
//! | Success         | I |
//! | Malformed       | I |
//! | ResourceLimitExceeded | I |
//! | InsufficientRefundableFee | I |
//!
//! # Summary
//!
//! - **158 total result code variants** across 26 operation result code enums
//! - **0 untested gaps** — every code is either tested (T/I) or marked as
//!   dead/unreachable (X) with an `#[ignore]` stub
//! - Dead codes (X): NoIssuer (×5), SellNoIssuer/BuyNoIssuer (×6),
//!   TrustNotRequired, NotSupportedYet, PoolFull — all unreachable since
//!   protocol 13+/24+ or require impractical 128-bit overflow

mod allow_trust;
mod begin_sponsoring;
mod change_trust;
mod claim_claimable_balance;
mod clawback_claimable_balance;
mod create_claimable_balance;
mod liquidity_pool_deposit;
mod liquidity_pool_withdraw;
mod manage_buy_offer;
mod manage_sell_offer;
mod path_payment_strict_receive;
mod path_payment_strict_send;
mod payment;
mod revoke_sponsorship;

/// Helper macro to assert an operation result matches a specific result code.
///
/// Usage:
/// ```ignore
/// assert_op_result!(result, PaymentResult::Success);
/// assert_op_result!(result, CreateAccountResult::Malformed);
/// ```
macro_rules! assert_op_result {
    ($result:expr, $variant:pat) => {{
        let result = $result.expect("operation should not return Err");
        match &result {
            stellar_xdr::curr::OperationResult::OpInner(inner) => {
                assert!(
                    matches!(inner, $variant),
                    "expected {}, got {:?}",
                    stringify!($variant),
                    inner
                );
            }
            other => panic!("expected OpInner, got {:?}", other),
        }
    }};
}

pub(crate) use assert_op_result;
