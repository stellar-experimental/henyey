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
//! - `ContractData` (Stellar Asset Contract native balances) — only the native
//!   SAC's `Balance` entries, mirroring core's `getAssetBalance` CONTRACT_DATA
//!   branch (see [`native_balance`]). The native SAC contract id is derived
//!   per-op from `delta.network_id` (a pure function of the network passphrase,
//!   identical to core's precomputed `mLumenContractInfo`).
//!
//! # Parity
//!
//! stellar-core reference: `src/invariant/ConservationOfLumens.cpp`
//! (per-operation `checkOnOperationApply`; the bucket-list `checkSnapshot`
//! scan is out of scope — henyey's `Invariant` trait only exposes the per-op
//! hook). Strictness: non-strict (`Invariant(false)`).

use stellar_xdr::curr::{
    Asset, ContractEvent, ContractId, ContractIdPreimage, Hash, HashIdPreimage,
    HashIdPreimageContractId, LedgerEntry, LedgerEntryData, LiquidityPoolEntryBody, Operation,
    OperationResult, OperationResultTr, ScAddress, ScVal,
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

/// Outcome of inspecting one ledger entry for a native-XLM balance.
///
/// Mirrors the relevant part of stellar-core's `AssetBalanceResult`
/// (`{overflowed, assetMatched, balance}`): an entry either holds no native
/// balance (`None` → contributes 0), holds a representable native balance
/// (`Balance(i64)`), or — only in the SAC `ContractData` case — holds an i128
/// amount outside the `i64` range (`Overflow`), which fails the invariant.
enum NativeBalance {
    /// No native balance (asset mismatch, non-balance-bearing type, or a
    /// matched-but-malformed SAC entry). Contributes 0, never an error.
    None,
    /// A representable native balance.
    Balance(i64),
    /// SAC i128 amount out of `i64` range (`hi > 0 || lo > i64::MAX`).
    Overflow,
}

/// Derives the native-XLM Stellar-Asset-Contract id for `network_id`, mirroring
/// stellar-core's `getAssetContractID(networkID, Asset(ASSET_TYPE_NATIVE))`.
///
/// This is a pure function of the network passphrase, so deriving it per-op is
/// behaviorally identical to core's precomputed `mLumenContractInfo`.
fn native_sac_contract_id(network_id: &[u8; 32]) -> ContractId {
    let preimage = HashIdPreimage::ContractId(HashIdPreimageContractId {
        network_id: Hash(*network_id),
        contract_id_preimage: ContractIdPreimage::Asset(Asset::Native),
    });
    let hash = henyey_common::Hash256::hash_xdr(&preimage);
    ContractId(Hash(hash.0))
}

/// Returns the native-XLM balance held by a single ledger entry.
///
/// Mirrors `getAssetBalance(le, Asset(ASSET_TYPE_NATIVE), lumenContractInfo)`
/// for the native asset, including the SAC `ContractData` branch
/// (`TransactionUtils.cpp` `getAssetBalance`): an entry is the native SAC
/// balance iff its `contract` is `Contract(native_sac_id)`, its `key` is a
/// non-empty `Vec` whose first element is `Symbol("Balance")`, and its `val` is
/// a non-empty `Map` whose first entry is keyed `Symbol("amount")` with an
/// `I128` value. A matched-contract-but-malformed entry contributes 0 (not an
/// error); only an in-range-failing i128 (`hi > 0 || lo > i64::MAX`) is an
/// [`NativeBalance::Overflow`].
fn native_balance(entry: &LedgerEntry, native_sac_id: &ContractId) -> NativeBalance {
    match &entry.data {
        LedgerEntryData::Account(acc) => NativeBalance::Balance(acc.balance),
        LedgerEntryData::ClaimableBalance(cb) => match cb.asset {
            Asset::Native => NativeBalance::Balance(cb.amount),
            _ => NativeBalance::None,
        },
        LedgerEntryData::LiquidityPool(lp) => {
            let LiquidityPoolEntryBody::LiquidityPoolConstantProduct(cp) = &lp.body;
            if matches!(cp.params.asset_a, Asset::Native) {
                NativeBalance::Balance(cp.reserve_a)
            } else if matches!(cp.params.asset_b, Asset::Native) {
                NativeBalance::Balance(cp.reserve_b)
            } else {
                NativeBalance::None
            }
        }
        LedgerEntryData::ContractData(cd) => sac_native_balance(cd, native_sac_id),
        // Trustlines never hold the native asset; offers/data/contract code/
        // config/TTL hold no asset balance.
        _ => NativeBalance::None,
    }
}

/// Extracts the native-XLM balance from a `ContractData` entry, mirroring the
/// CONTRACT_DATA branch of stellar-core's `getAssetBalance`.
fn sac_native_balance(
    cd: &stellar_xdr::curr::ContractDataEntry,
    native_sac_id: &ContractId,
) -> NativeBalance {
    // The entry must be stored under the native SAC contract address.
    let ScAddress::Contract(contract_id) = &cd.contract else {
        return NativeBalance::None;
    };
    if contract_id != native_sac_id {
        return NativeBalance::None;
    }
    // key == Vec(Some(v)), non-empty, v[0] == Symbol("Balance").
    let ScVal::Vec(Some(key_vec)) = &cd.key else {
        return NativeBalance::None;
    };
    match key_vec.first() {
        Some(ScVal::Symbol(sym)) if sym.0.as_slice() == b"Balance" => {}
        _ => return NativeBalance::None,
    }
    // val == Map(Some(m)), non-empty, m[0].key == Symbol("amount"),
    // m[0].val == I128(parts).
    let ScVal::Map(Some(val_map)) = &cd.val else {
        return NativeBalance::None;
    };
    let Some(amount_entry) = val_map.first() else {
        return NativeBalance::None;
    };
    match &amount_entry.key {
        ScVal::Symbol(sym) if sym.0.as_slice() == b"amount" => {}
        _ => return NativeBalance::None,
    }
    let ScVal::I128(parts) = &amount_entry.val else {
        return NativeBalance::None;
    };
    // Out of i64 range (`hi > 0 || lo > i64::MAX`) → overflow; mirror core's
    // `hi > 0` (not `hi != 0`) exactly. `hi` is `i64` in this XDR version, so
    // `hi > 0` is byte-for-byte equivalent to core's unsigned check for the
    // representable cases (any high word means out of range).
    if parts.hi > 0 || parts.lo > i64::MAX as u64 {
        return NativeBalance::Overflow;
    }
    NativeBalance::Balance(parts.lo as i64)
}

/// Computes the native-balance delta for a single changed entry:
/// `native_balance(current) - native_balance(previous)`, treating a missing
/// entry (or a non-balance-bearing one) as contributing 0.
///
/// Mirrors stellar-core's `calculateDeltaBalance`: if either side reports an
/// i128 overflow, the delta cannot be computed and the invariant fails with
/// core's verbatim error string. Both inputs cannot be `None` (every changed
/// entry has at least one side); the caller guarantees this.
fn calculate_delta_balance(
    current: Option<&LedgerEntry>,
    previous: Option<&LedgerEntry>,
    native_sac_id: &ContractId,
) -> Result<i64, String> {
    let cur = current.map_or(NativeBalance::Balance(0), |e| {
        native_balance(e, native_sac_id)
    });
    let prev = previous.map_or(NativeBalance::Balance(0), |e| {
        native_balance(e, native_sac_id)
    });
    if matches!(cur, NativeBalance::Overflow) || matches!(prev, NativeBalance::Overflow) {
        return Err("Could not calculate lumen balance delta for an entry".to_string());
    }
    let cur = match cur {
        NativeBalance::Balance(b) => b,
        NativeBalance::None | NativeBalance::Overflow => 0,
    };
    let prev = match prev {
        NativeBalance::Balance(b) => b,
        NativeBalance::None | NativeBalance::Overflow => 0,
    };
    Ok(cur - prev)
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

        // The native SAC contract id is a pure function of the network id;
        // derive it once per op (core precomputes it as `mLumenContractInfo`).
        let native_sac_id = native_sac_contract_id(delta.network_id);

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
            accumulate(calculate_delta_balance(Some(entry), None, &native_sac_id)?)?;
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
            accumulate(calculate_delta_balance(
                Some(current),
                Some(previous),
                &native_sac_id,
            )?)?;
        }
        // Deleted: (current = None, previous = Some).
        for previous in delta.delete_states {
            accumulate(calculate_delta_balance(
                None,
                Some(previous),
                &native_sac_id,
            )?)?;
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
                            // `checked_add` returns `None` on both overflow
                            // (positive addend) and underflow (negative addend);
                            // distinguish so the message reports the actual
                            // condition rather than always saying "Overflow"
                            // (#3007). Inflation payouts are non-negative in
                            // practice, but a corrupt/unexpected meta could carry
                            // a negative amount.
                            if p.amount >= 0 {
                                "Overflow detected when summing inflation payouts".to_string()
                            } else {
                                "Underflow detected when summing inflation payouts".to_string()
                            }
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
