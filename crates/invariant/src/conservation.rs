//! ConservationOfLumens invariant.
//!
//! Verifies that native XLM is conserved across each operation apply: the sum
//! of per-entry native-balance deltas (`deltaBalances`), the change in the
//! ledger header's `totalCoins` (`deltaTotalCoins`), and the change in `feePool`
//! (`deltaFeePool`) must satisfy the conservation equation.
//!
//! In the no-inflation case (the only live case for protocol 24+, where
//! inflation is removed and always returns `NotTime`/empty payouts) all three
//! deltas must be zero. The inflation branch is retained for parity with
//! stellar-core but is dead in henyey.
//!
//! # Native-balance-bearing entry types
//!
//! Per stellar-core's `getAssetBalance(le, ASSET_TYPE_NATIVE, ...)`
//! (`TransactionUtils.cpp:2032`) / `canHoldAsset(type, NATIVE)`:
//!
//! - `Account.balance` — always native.
//! - `ClaimableBalance.amount` — only when its `asset` is native.
//! - `LiquidityPool` constant-product `reserveA`/`reserveB` — for whichever leg
//!   is the native asset.
//!
//! `ContractData` (Stellar Asset Contract native balances) is NOT counted in
//! this implementation. An Account↔Contract native SAC transfer would show a
//! non-zero Account-side delta with the offsetting `ContractData` credit
//! uncounted, producing a spurious violation (a **false alarm**, not a missed
//! imbalance). Because this invariant is non-strict, that is log noise rather
//! than a crash. Counting SAC `ContractData` native balances is tracked as a
//! follow-up in #2987.
//!
//! # Parity
//!
//! stellar-core reference: `src/invariant/ConservationOfLumens.cpp`
//! (per-operation `checkOnOperationApply`; the bucket-list `checkSnapshot`
//! scan is out of scope — henyey's `Invariant` trait only exposes the per-op
//! hook). Strictness: non-strict (`Invariant(false)`).

use stellar_xdr::curr::{
    Asset, ContractEvent, LedgerEntry, LedgerEntryData, LiquidityPoolEntryBody, Operation,
    OperationResult, OperationResultTr,
};

use crate::{Invariant, OperationDelta};

/// The `ConservationOfLumens` invariant.
pub struct ConservationOfLumens;

impl ConservationOfLumens {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ConservationOfLumens {
    fn default() -> Self {
        Self::new()
    }
}

/// Returns the native-XLM balance held by a single ledger entry, or `None` if
/// the entry holds no native balance (the asset doesn't match / not a
/// balance-bearing type).
///
/// Mirrors `getAssetBalance(le, Asset(ASSET_TYPE_NATIVE), lumenContractInfo)`
/// for the native asset, excluding the SAC `ContractData` case (see module
/// docs / #2987).
fn native_balance(entry: &LedgerEntry) -> Option<i64> {
    match &entry.data {
        LedgerEntryData::Account(acc) => Some(acc.balance),
        LedgerEntryData::ClaimableBalance(cb) => {
            matches!(cb.asset, Asset::Native).then_some(cb.amount)
        }
        LedgerEntryData::LiquidityPool(lp) => {
            let LiquidityPoolEntryBody::LiquidityPoolConstantProduct(cp) = &lp.body;
            if matches!(cp.params.asset_a, Asset::Native) {
                Some(cp.reserve_a)
            } else if matches!(cp.params.asset_b, Asset::Native) {
                Some(cp.reserve_b)
            } else {
                None
            }
        }
        // Trustlines never hold the native asset; offers/data/contract code/
        // config/TTL hold no asset balance. ContractData (SAC) deferred to #2987.
        _ => None,
    }
}

/// Computes the native-balance delta for a single changed entry:
/// `native_balance(current) - native_balance(previous)`, treating a missing
/// entry as contributing 0.
///
/// Mirrors stellar-core's `calculateDeltaBalance`. Both inputs cannot be
/// `None` (every changed entry has at least one side); the caller guarantees
/// this.
fn calculate_delta_balance(current: Option<&LedgerEntry>, previous: Option<&LedgerEntry>) -> i64 {
    let cur = current.and_then(native_balance).unwrap_or(0);
    let prev = previous.and_then(native_balance).unwrap_or(0);
    cur - prev
}

impl Invariant for ConservationOfLumens {
    fn name(&self) -> &str {
        "ConservationOfLumens"
    }

    fn is_strict(&self) -> bool {
        false
    }

    fn check_on_operation_apply(
        &self,
        _operation: &Operation,
        op_result: &OperationResult,
        delta: &OperationDelta<'_>,
        _events: &[ContractEvent],
    ) -> Result<(), String> {
        // Accumulate the native-balance delta across every changed entry, with
        // checked arithmetic mirroring stellar-core's explicit overflow /
        // underflow guards.
        let mut delta_balances: i64 = 0;

        let mut accumulate = |d: i64| -> Result<(), String> {
            // Overflow: positive d pushing past i64::MAX.
            if d > 0 && delta_balances > i64::MAX - d {
                return Err("Overflow detected when adding to deltaBalances".to_string());
            }
            // Underflow: negative d pushing past i64::MIN.
            if d < 0 && delta_balances < i64::MIN - d {
                return Err("Underflow detected when adding to deltaBalances".to_string());
            }
            delta_balances += d;
            Ok(())
        };

        // Created: (current = Some, previous = None).
        for entry in delta.created {
            accumulate(calculate_delta_balance(Some(entry), None))?;
        }
        // Updated: (current, previous) pairs. `updated` and `update_states` are
        // documented as parallel slices; a length divergence is a caller bug.
        // Fail fast instead of letting `zip` silently truncate to the shorter
        // slice (which would drop trailing entries and could mask a real
        // imbalance). See #2997.
        if delta.updated.len() != delta.update_states.len() {
            return Err(format!(
                "OperationDelta updated ({}) and update_states ({}) lengths diverge",
                delta.updated.len(),
                delta.update_states.len()
            ));
        }
        for (current, previous) in delta.updated.iter().zip(delta.update_states.iter()) {
            accumulate(calculate_delta_balance(Some(current), Some(previous)))?;
        }
        // Deleted: (current = None, previous = Some).
        for previous in delta.delete_states {
            accumulate(calculate_delta_balance(None, Some(previous)))?;
        }

        // Header deltas. If either header is missing, skip the header-delta
        // checks and treat both as 0 (see module docs / OperationDelta).
        let (delta_total_coins, delta_fee_pool, headers_present) =
            match (delta.header_current, delta.header_previous) {
                (Some(curr), Some(prev)) => (
                    curr.total_coins - prev.total_coins,
                    curr.fee_pool - prev.fee_pool,
                    true,
                ),
                _ => (0, 0, false),
            };

        let is_inflation = matches!(
            op_result,
            OperationResult::OpInner(OperationResultTr::Inflation(_))
        );

        if is_inflation {
            // Retained for parity; dead in 24+ (inflation always NotTime/empty).
            // Use checked accumulation so a pathological payout set (only
            // reachable via tests / unexpected result variants) surfaces a
            // detectable error rather than wrapping silently and producing a
            // misleading invariant message. See #2998.
            let inflation_payouts: i64 = match op_result {
                OperationResult::OpInner(OperationResultTr::Inflation(
                    stellar_xdr::curr::InflationResult::Success(payouts),
                )) => {
                    let mut sum: i64 = 0;
                    for p in payouts.iter() {
                        sum = sum.checked_add(p.amount).ok_or_else(|| {
                            "Overflow detected when summing inflation payouts".to_string()
                        })?;
                    }
                    sum
                }
                _ => 0,
            };

            if headers_present && delta_total_coins != inflation_payouts + delta_fee_pool {
                return Err(format!(
                    "LedgerHeader totalCoins change ({}) did not match feePool change ({}) \
                     plus inflation payouts ({})",
                    delta_total_coins, delta_fee_pool, inflation_payouts
                ));
            }
            if delta_balances != inflation_payouts {
                // #2999: "LedgerEntry account balances" is upstream-verbatim by
                // design. `delta_balances` also covers native claimable-balance
                // and the native leg of liquidity-pool reserves, but the wording
                // is a byte-for-byte match to stellar-core's
                // ConservationOfLumens.cpp error string (FMT_STRING at
                // "LedgerEntry account balances change ({:d}) did not match
                // inflation payouts ({:d})"). Rewording would deviate from
                // message parity, so we keep the upstream phrasing intentionally.
                return Err(format!(
                    "LedgerEntry account balances change ({}) did not match inflation payouts ({})",
                    delta_balances, inflation_payouts
                ));
            }
        } else {
            if headers_present && delta_total_coins != 0 {
                return Err(format!(
                    "LedgerHeader totalCoins changed by {} without inflation",
                    delta_total_coins
                ));
            }
            if headers_present && delta_fee_pool != 0 {
                return Err(format!(
                    "LedgerHeader feePool changed by {} without inflation",
                    delta_fee_pool
                ));
            }
            if delta_balances != 0 {
                // #2999: "LedgerEntry account balances" is upstream-verbatim by
                // design (covers native CB amounts and native LP reserve legs in
                // addition to account balances). Byte-for-byte match to
                // stellar-core's ConservationOfLumens.cpp FMT_STRING
                // "LedgerEntry account balances changed by {:d} without
                // inflation"; kept unchanged for message parity.
                return Err(format!(
                    "LedgerEntry account balances changed by {} without inflation",
                    delta_balances
                ));
            }
        }

        Ok(())
    }
}
