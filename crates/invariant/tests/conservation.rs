//! Integration tests for the `ConservationOfLumens` invariant.
//!
//! Verifies the per-operation native-XLM conservation equation:
//! `deltaBalances == 0` (no-inflation branch, the only live branch in 24+),
//! and the header-delta checks (`deltaTotalCoins == 0`, `deltaFeePool == 0`).
//!
//! Parity: `src/invariant/ConservationOfLumens.cpp` no-inflation branch.

use std::sync::Arc;

use henyey_invariant::{ConservationOfLumens, Invariant, InvariantManager, OperationDelta};
use stellar_xdr::curr::{
    AccountEntry, AccountEntryExt, AccountId, AlphaNum4, Asset, AssetCode4, ClaimPredicate,
    ClaimableBalanceEntry, ClaimableBalanceEntryExt, ClaimableBalanceId, Claimant, ClaimantV0,
    Hash, InflationResult, LedgerEntry, LedgerEntryData, LedgerEntryExt, LedgerHeader,
    LedgerHeaderExt, LiquidityPoolConstantProductParameters, LiquidityPoolEntry,
    LiquidityPoolEntryBody, LiquidityPoolEntryConstantProduct, Operation, OperationBody,
    OperationResult, OperationResultTr, PoolId, PublicKey, SequenceNumber, StellarValue,
    StellarValueExt, String32, Thresholds, TimePoint, Uint256, VecM,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn account_id(seed: u8) -> AccountId {
    AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([seed; 32])))
}

fn account_entry(seed: u8, balance: i64) -> LedgerEntry {
    LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::Account(AccountEntry {
            account_id: account_id(seed),
            balance,
            seq_num: SequenceNumber(0),
            num_sub_entries: 0,
            inflation_dest: None,
            flags: 0,
            home_domain: String32::default(),
            thresholds: Thresholds([1, 0, 0, 0]),
            signers: VecM::default(),
            ext: AccountEntryExt::V0,
        }),
        ext: LedgerEntryExt::V0,
    }
}

fn claimable_balance_entry(seed: u8, asset: Asset, amount: i64) -> LedgerEntry {
    LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::ClaimableBalance(ClaimableBalanceEntry {
            balance_id: ClaimableBalanceId::ClaimableBalanceIdTypeV0(Hash([seed; 32])),
            claimants: vec![Claimant::ClaimantTypeV0(ClaimantV0 {
                destination: account_id(99),
                predicate: ClaimPredicate::Unconditional,
            })]
            .try_into()
            .unwrap(),
            asset,
            amount,
            ext: ClaimableBalanceEntryExt::V0,
        }),
        ext: LedgerEntryExt::V0,
    }
}

fn liquidity_pool_entry(
    seed: u8,
    native_leg_a: bool,
    reserve_a: i64,
    reserve_b: i64,
) -> LedgerEntry {
    let usd = Asset::CreditAlphanum4(AlphaNum4 {
        asset_code: AssetCode4(*b"USD\0"),
        issuer: account_id(1),
    });
    let (asset_a, asset_b) = if native_leg_a {
        (Asset::Native, usd)
    } else {
        (usd, Asset::Native)
    };
    LedgerEntry {
        last_modified_ledger_seq: 1,
        data: LedgerEntryData::LiquidityPool(LiquidityPoolEntry {
            liquidity_pool_id: PoolId(Hash([seed; 32])),
            body: LiquidityPoolEntryBody::LiquidityPoolConstantProduct(
                LiquidityPoolEntryConstantProduct {
                    params: LiquidityPoolConstantProductParameters {
                        asset_a,
                        asset_b,
                        fee: 30,
                    },
                    reserve_a,
                    reserve_b,
                    total_pool_shares: 1000,
                    pool_shares_trust_line_count: 1,
                },
            ),
        }),
        ext: LedgerEntryExt::V0,
    }
}

fn header(total_coins: i64, fee_pool: i64) -> LedgerHeader {
    LedgerHeader {
        ledger_version: 24,
        previous_ledger_hash: Hash([0; 32]),
        scp_value: StellarValue {
            tx_set_hash: Hash([0; 32]),
            close_time: TimePoint(0),
            upgrades: VecM::default(),
            ext: StellarValueExt::Basic,
        },
        tx_set_result_hash: Hash([0; 32]),
        bucket_list_hash: Hash([0; 32]),
        ledger_seq: 100,
        total_coins,
        fee_pool,
        inflation_seq: 0,
        id_pool: 0,
        base_fee: 100,
        base_reserve: 5_000_000,
        max_tx_set_size: 1000,
        skip_list: [Hash([0; 32]), Hash([0; 32]), Hash([0; 32]), Hash([0; 32])],
        ext: LedgerHeaderExt::V0,
    }
}

fn dummy_op() -> Operation {
    Operation {
        source_account: None,
        body: OperationBody::Inflation,
    }
}

/// A non-inflation op result (Payment success) so the no-inflation branch runs.
fn payment_result() -> OperationResult {
    use stellar_xdr::curr::PaymentResult;
    OperationResult::OpInner(OperationResultTr::Payment(PaymentResult::Success))
}

fn inflation_result(payouts: Vec<(u8, i64)>) -> OperationResult {
    use stellar_xdr::curr::InflationPayout;
    let payouts: VecM<InflationPayout> = payouts
        .into_iter()
        .map(|(seed, amount)| InflationPayout {
            destination: account_id(seed),
            amount,
        })
        .collect::<Vec<_>>()
        .try_into()
        .unwrap();
    OperationResult::OpInner(OperationResultTr::Inflation(InflationResult::Success(
        payouts,
    )))
}

struct DeltaBuilder {
    created: Vec<LedgerEntry>,
    updated: Vec<LedgerEntry>,
    update_states: Vec<LedgerEntry>,
    delete_states: Vec<LedgerEntry>,
}

impl DeltaBuilder {
    fn new() -> Self {
        Self {
            created: vec![],
            updated: vec![],
            update_states: vec![],
            delete_states: vec![],
        }
    }
    fn created(mut self, e: LedgerEntry) -> Self {
        self.created.push(e);
        self
    }
    fn updated(mut self, prev: LedgerEntry, cur: LedgerEntry) -> Self {
        self.update_states.push(prev);
        self.updated.push(cur);
        self
    }
    fn deleted(mut self, prev: LedgerEntry) -> Self {
        self.delete_states.push(prev);
        self
    }

    fn check(
        &self,
        inv: &ConservationOfLumens,
        op_result: &OperationResult,
        header_current: Option<&LedgerHeader>,
        header_previous: Option<&LedgerHeader>,
    ) -> Result<(), String> {
        let network_id = [0u8; 32];
        let deleted_keys: Vec<stellar_xdr::curr::LedgerKey> = vec![]; // not read by invariant
        let delta = OperationDelta {
            created: &self.created,
            updated: &self.updated,
            update_states: &self.update_states,
            deleted: &deleted_keys,
            delete_states: &self.delete_states,
            ledger_seq: 100,
            ledger_version: 24,
            header_current,
            header_previous,
            network_id: &network_id,
        };
        inv.check_on_operation_apply(&dummy_op(), op_result, &delta, &[])
    }
}

// ---------------------------------------------------------------------------
// Regression canary
// ---------------------------------------------------------------------------

#[test]
fn test_conservation_fails_on_minted_lumens() {
    let inv = ConservationOfLumens::new();
    let h = header(1_000_000, 0);
    // Single account whose native balance grows by 500 with no offset → minted lumens.
    let res = DeltaBuilder::new()
        .updated(account_entry(2, 1000), account_entry(2, 1500))
        .check(&inv, &payment_result(), Some(&h), Some(&h));
    let err = res.expect_err("minted lumens must violate ConservationOfLumens");
    assert!(
        err.contains("balances changed"),
        "unexpected message: {err}"
    );
}

// ---------------------------------------------------------------------------
// New coverage
// ---------------------------------------------------------------------------

#[test]
fn test_conservation_holds_on_balanced_delete_and_credit() {
    let inv = ConservationOfLumens::new();
    let h = header(1_000_000, 0);
    // A native claimable balance of 700 is deleted (claimed); the destination
    // account is credited 700 → net zero.
    let res = DeltaBuilder::new()
        .deleted(claimable_balance_entry(5, Asset::Native, 700))
        .updated(account_entry(3, 1000), account_entry(3, 1700))
        .check(&inv, &payment_result(), Some(&h), Some(&h));
    assert!(res.is_ok(), "balanced delete+credit should hold: {res:?}");

    // Deleting the native CB without crediting anyone → lumens vanish → fails.
    let bad = DeltaBuilder::new()
        .deleted(claimable_balance_entry(5, Asset::Native, 700))
        .check(&inv, &payment_result(), Some(&h), Some(&h));
    assert!(bad.is_err(), "unmatched CB delete should fail");
}

#[test]
fn test_conservation_holds_on_balanced_transfer() {
    let inv = ConservationOfLumens::new();
    let h = header(1_000_000, 0);
    // Account 2 loses 500, account 3 gains 500 → net zero.
    let res = DeltaBuilder::new()
        .updated(account_entry(2, 1000), account_entry(2, 500))
        .updated(account_entry(3, 2000), account_entry(3, 2500))
        .check(&inv, &payment_result(), Some(&h), Some(&h));
    assert!(res.is_ok(), "balanced transfer should hold: {res:?}");
}

#[test]
fn test_conservation_claimable_balance_native() {
    let inv = ConservationOfLumens::new();
    let h = header(1_000_000, 0);
    // Balanced: account debited 700, native claimable balance created with 700.
    let ok = DeltaBuilder::new()
        .updated(account_entry(2, 1000), account_entry(2, 300))
        .created(claimable_balance_entry(5, Asset::Native, 700))
        .check(&inv, &payment_result(), Some(&h), Some(&h));
    assert!(ok.is_ok(), "balanced CB create should hold: {ok:?}");

    // Unbalanced: CB created with 700 but account only debited 100.
    let bad = DeltaBuilder::new()
        .updated(account_entry(2, 1000), account_entry(2, 900))
        .created(claimable_balance_entry(5, Asset::Native, 700))
        .check(&inv, &payment_result(), Some(&h), Some(&h));
    assert!(bad.is_err(), "unbalanced CB create should fail");

    // Non-native claimable balance must be ignored (only the account debit counts → fails).
    let usd = Asset::CreditAlphanum4(AlphaNum4 {
        asset_code: AssetCode4(*b"USD\0"),
        issuer: account_id(1),
    });
    let non_native = DeltaBuilder::new()
        .created(claimable_balance_entry(5, usd, 700))
        .check(&inv, &payment_result(), Some(&h), Some(&h));
    assert!(
        non_native.is_ok(),
        "non-native CB carries no native delta: {non_native:?}"
    );
}

#[test]
fn test_conservation_liquidity_pool_native_leg() {
    let inv = ConservationOfLumens::new();
    let h = header(1_000_000, 0);
    // LP native reserve (leg A) grows by 400, matched by an account debit of 400.
    let ok = DeltaBuilder::new()
        .updated(
            liquidity_pool_entry(7, true, 1000, 2000),
            liquidity_pool_entry(7, true, 1400, 2000),
        )
        .updated(account_entry(2, 1000), account_entry(2, 600))
        .check(&inv, &payment_result(), Some(&h), Some(&h));
    assert!(
        ok.is_ok(),
        "balanced LP native-leg change should hold: {ok:?}"
    );

    // Native leg B variant: change in reserve_b counts when asset_b is native.
    let ok_b = DeltaBuilder::new()
        .updated(
            liquidity_pool_entry(8, false, 2000, 1000),
            liquidity_pool_entry(8, false, 2000, 1400),
        )
        .updated(account_entry(2, 1000), account_entry(2, 600))
        .check(&inv, &payment_result(), Some(&h), Some(&h));
    assert!(
        ok_b.is_ok(),
        "balanced LP native-leg-B change should hold: {ok_b:?}"
    );

    // Unbalanced LP change → fails.
    let bad = DeltaBuilder::new()
        .updated(
            liquidity_pool_entry(7, true, 1000, 2000),
            liquidity_pool_entry(7, true, 1400, 2000),
        )
        .check(&inv, &payment_result(), Some(&h), Some(&h));
    assert!(bad.is_err(), "unmatched LP native-leg change should fail");
}

#[test]
fn test_conservation_overflow_detected() {
    let inv = ConservationOfLumens::new();
    let h = header(1_000_000, 0);
    // Two created native balances each near i64::MAX → overflow accumulating positive deltas.
    let res = DeltaBuilder::new()
        .created(account_entry(2, i64::MAX))
        .created(account_entry(3, i64::MAX))
        .check(&inv, &payment_result(), Some(&h), Some(&h));
    let err = res.expect_err("overflowing deltaBalances must fail");
    assert!(err.contains("Overflow"), "unexpected message: {err}");
}

#[test]
fn test_conservation_none_header_skips_header_checks() {
    let inv = ConservationOfLumens::new();
    // header_current = None: header-delta checks skipped, only deltaBalances evaluated.
    // Balanced balances → Ok despite missing headers.
    let res = DeltaBuilder::new()
        .updated(account_entry(2, 1000), account_entry(2, 500))
        .updated(account_entry(3, 2000), account_entry(3, 2500))
        .check(&inv, &payment_result(), None, None);
    assert!(
        res.is_ok(),
        "None headers + balanced balances should hold: {res:?}"
    );

    // But an imbalance still fires even with None headers.
    let bad = DeltaBuilder::new()
        .updated(account_entry(2, 1000), account_entry(2, 1500))
        .check(&inv, &payment_result(), None, None);
    assert!(
        bad.is_err(),
        "None headers must not suppress balance imbalance"
    );
}

#[test]
fn test_conservation_inflation_branch_balanced() {
    // Dead-but-retained inflation branch: totalCoins grows by exactly the payout sum,
    // feePool unchanged, and account balances grow by the same payout sum.
    let inv = ConservationOfLumens::new();
    let prev = header(1_000_000, 0);
    let curr = header(1_000_300, 0); // +300 total coins
    let res = DeltaBuilder::new()
        .updated(account_entry(2, 1000), account_entry(2, 1300)) // +300 balance
        .check(
            &inv,
            &inflation_result(vec![(2, 300)]),
            Some(&curr),
            Some(&prev),
        );
    assert!(res.is_ok(), "balanced inflation should hold: {res:?}");
}

#[test]
fn test_conservation_mismatched_update_lengths_fail_fast() {
    // #2997: `updated` and `update_states` must be parallel. If their lengths
    // diverge, the invariant must error early rather than silently truncating
    // the longer slice via `zip` (which would skip the trailing entries and
    // could mask a real imbalance). Build a divergent delta directly, bypassing
    // the lockstep `DeltaBuilder` helper.
    let inv = ConservationOfLumens::new();
    let h = header(1_000_000, 0);
    let network_id = [0u8; 32];
    let deleted_keys: Vec<stellar_xdr::curr::LedgerKey> = vec![];

    // Two post-states but only one pre-state → length divergence.
    let updated = vec![account_entry(2, 500), account_entry(3, 2500)];
    let update_states = vec![account_entry(2, 1000)];

    let delta = OperationDelta {
        created: &[],
        updated: &updated,
        update_states: &update_states,
        deleted: &deleted_keys,
        delete_states: &[],
        ledger_seq: 100,
        ledger_version: 24,
        header_current: Some(&h),
        header_previous: Some(&h),
        network_id: &network_id,
    };
    let res = inv.check_on_operation_apply(&dummy_op(), &payment_result(), &delta, &[]);
    let err = res.expect_err("mismatched updated/update_states lengths must fail fast");
    assert!(
        err.contains("updated") && err.contains("update_states"),
        "unexpected message: {err}"
    );
}

#[test]
fn test_conservation_inflation_payout_sum_overflow_detected() {
    // #2998: the inflation-payout sum must use checked accumulation. Two payouts
    // each near i64::MAX overflow the sum; this must surface a detectable error
    // rather than silently wrapping to a misleading value.
    let inv = ConservationOfLumens::new();
    let prev = header(1_000_000, 0);
    let curr = header(1_000_000, 0);
    let res = DeltaBuilder::new().check(
        &inv,
        &inflation_result(vec![(2, i64::MAX), (3, i64::MAX)]),
        Some(&curr),
        Some(&prev),
    );
    let err = res.expect_err("overflowing inflation payout sum must fail");
    assert!(
        err.contains("Overflow") && err.contains("inflation payouts"),
        "unexpected message: {err}"
    );
}

#[test]
fn test_conservation_manager_registers_and_enables() {
    let mut mgr = InvariantManager::new();
    mgr.register(Arc::new(ConservationOfLumens::new()));
    mgr.enable("ConservationOfLumens").unwrap();
    assert_eq!(mgr.get_enabled_invariants(), vec!["ConservationOfLumens"]);

    let mut mgr2 = InvariantManager::new();
    mgr2.register(Arc::new(ConservationOfLumens::new()));
    mgr2.enable(".*").unwrap();
    assert_eq!(mgr2.get_enabled_invariants(), vec!["ConservationOfLumens"]);
}
