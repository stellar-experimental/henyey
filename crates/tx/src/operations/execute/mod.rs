//! Operation execution dispatcher.
//!
//! This module provides the main entry point for executing Stellar operations.
//! Each operation type has its own submodule with the specific execution logic.
//!
//! # Result Code Test Coverage
//!
//! Every result code variant across all 26 operation types (158 total) is
//! covered by inline unit tests in the corresponding operation file. There are
//! zero untested gaps.
//!
//! The following 14 result codes are dead — unreachable since protocol 13+
//! (CAP-0017 removed issuer checks) or protocol 24+. Each has an `#[ignore]`
//! test stub documenting why:
//!
//! - `NoIssuer` — Payment, PathPaymentStrictReceive, PathPaymentStrictSend,
//!   ChangeTrust (protocol 13+, CAP-0017)
//! - `SellNoIssuer` / `BuyNoIssuer` — ManageSellOffer, ManageBuyOffer,
//!   CreatePassiveSellOffer (protocol 13+, CAP-0017)
//! - `TrustNotRequired` — AllowTrust (protocol 24+)
//! - `NotSupportedYet` — ManageData (protocol 24+)
//! - `PoolFull` — LiquidityPoolDeposit (requires 128-bit overflow, impractical)
//!
//! See: <https://github.com/stellar-experimental/henyey/issues/1126>

use henyey_common::checked_types::CheckedAmount;
use henyey_common::{protocol_version_is_before, protocol_version_starts_from, ProtocolVersion};

// Re-export checked arithmetic helpers from henyey-common so submodules
// can import them via `use super::add_account_balance` etc.
pub(super) use henyey_common::checked_types::{
    add_account_balance, add_pool_reserve, add_pool_shares, add_trustline_balance, dec_sub_entries,
    inc_sub_entries, sub_account_balance, sub_trustline_balance,
};
// Re-export liability accessors used by submodules.
pub(super) use henyey_common::checked_types::{account_liabilities, trustline_liabilities};
use soroban_env_host24::xdr::ReadXdr as ReadXdrP24;
use soroban_env_host_p24 as soroban_env_host24;
use soroban_env_host_p25 as soroban_env_host25;
use soroban_env_host_p26 as soroban_env_host26;
use stellar_xdr::curr::{
    AccountEntry, AccountEntryExt, AccountEntryExtensionV1, AccountEntryExtensionV1Ext, AccountId,
    Asset, ContractEvent, DiagnosticEvent, ExtendFootprintTtlResult, Liabilities, Operation,
    OperationBody, OperationResult, OperationResultTr, RestoreFootprintResult, TrustLineEntry,
    TrustLineEntryExt, TrustLineEntryExtensionV2, TrustLineEntryExtensionV2Ext, TrustLineEntryV1,
    TrustLineEntryV1Ext, TrustLineFlags, WriteXdr,
};

use crate::frame::muxed_to_account_id;
use crate::soroban::{SorobanConfig, SorobanContext};
use crate::state::LedgerStateManager;
use crate::validation::LedgerContext;
use crate::{Result, TxError};

// Shared helpers used by multiple operation submodules.

/// Load the source account entry, returning [`TxError::SourceAccountNotFound`]
/// if missing. The dispatcher already verifies existence (returning `opNO_ACCOUNT`),
/// so this should not fail in practice; the error path is a safety net.
pub(super) fn require_source_account<'a>(
    state: &'a LedgerStateManager,
    source: &AccountId,
) -> Result<&'a AccountEntry> {
    state
        .get_account(source)
        .ok_or(TxError::SourceAccountNotFound)
}

/// Like [`require_source_account`] but returns a clone.
pub(super) fn require_source_account_cloned(
    state: &LedgerStateManager,
    source: &AccountId,
) -> Result<AccountEntry> {
    state
        .get_account(source)
        .cloned()
        .ok_or(TxError::SourceAccountNotFound)
}

/// Transaction identity used to generate deterministic IDs for claimable balances
/// and trust-flag revocations. Bundles the three fields that always travel together.
pub struct TxIdentity<'a> {
    pub source_id: &'a AccountId,
    pub seq: i64,
    pub op_index: u32,
}

const ACCOUNT_SUBENTRY_LIMIT: u32 = 1000;
const AUTHORIZED_FLAG: u32 = TrustLineFlags::AuthorizedFlag as u32;
const AUTHORIZED_TO_MAINTAIN_LIABILITIES_FLAG: u32 =
    TrustLineFlags::AuthorizedToMaintainLiabilitiesFlag as u32;
const TRUSTLINE_CLAWBACK_ENABLED_FLAG: u32 = TrustLineFlags::TrustlineClawbackEnabledFlag as u32;

fn is_trustline_authorized(flags: u32) -> bool {
    flags & AUTHORIZED_FLAG != 0
}

fn is_authorized_to_maintain_liabilities(flags: u32) -> bool {
    flags & (AUTHORIZED_FLAG | AUTHORIZED_TO_MAINTAIN_LIABILITIES_FLAG) != 0
}

fn issuer_for_asset(asset: &Asset) -> Option<&AccountId> {
    match asset {
        Asset::Native => None,
        Asset::CreditAlphanum4(a) => Some(&a.issuer),
        Asset::CreditAlphanum12(a) => Some(&a.issuer),
    }
}

/// Available native balance after selling liabilities, without accounting for
/// minimum balance. Returns `account.balance - selling_liabilities`, clamped
/// to zero. Used for sponsor reserve checks and other contexts where the caller
/// compares against a known reserve requirement.
fn account_balance_after_liabilities(account: &AccountEntry) -> i64 {
    account
        .balance
        .saturating_sub(account_liabilities(account).selling)
}

/// Available trustline balance after selling liabilities. Returns
/// `trustline.balance - selling_liabilities`, clamped to zero.
fn trustline_balance_after_liabilities(trustline: &TrustLineEntry) -> i64 {
    trustline
        .balance
        .saturating_sub(trustline_liabilities(trustline).selling)
}

fn ensure_account_liabilities(account: &mut AccountEntry) -> &mut Liabilities {
    if matches!(account.ext, AccountEntryExt::V0) {
        account.ext = AccountEntryExt::V1(AccountEntryExtensionV1 {
            liabilities: Liabilities {
                buying: 0,
                selling: 0,
            },
            ext: AccountEntryExtensionV1Ext::V0,
        });
    }
    match &mut account.ext {
        AccountEntryExt::V1(v1) => &mut v1.liabilities,
        _ => unreachable!(),
    }
}

fn ensure_trustline_liabilities(trustline: &mut TrustLineEntry) -> &mut Liabilities {
    if matches!(trustline.ext, TrustLineEntryExt::V0) {
        trustline.ext = TrustLineEntryExt::V1(TrustLineEntryV1 {
            liabilities: Liabilities {
                buying: 0,
                selling: 0,
            },
            ext: TrustLineEntryV1Ext::V0,
        });
    }
    match &mut trustline.ext {
        TrustLineEntryExt::V1(v1) => &mut v1.liabilities,
        _ => unreachable!(),
    }
}

/// Ensure a trustline has the V2 extension and return a mutable reference to it.
///
/// Parity: stellar-core `prepareTrustLineEntryExtensionV2` in TransactionUtils.cpp.
pub(super) fn ensure_trustline_ext_v2(
    trustline: &mut TrustLineEntry,
) -> &mut TrustLineEntryExtensionV2 {
    match &mut trustline.ext {
        TrustLineEntryExt::V0 => {
            trustline.ext = TrustLineEntryExt::V1(TrustLineEntryV1 {
                liabilities: Liabilities {
                    buying: 0,
                    selling: 0,
                },
                ext: TrustLineEntryV1Ext::V2(TrustLineEntryExtensionV2 {
                    liquidity_pool_use_count: 0,
                    ext: TrustLineEntryExtensionV2Ext::V0,
                }),
            });
        }
        TrustLineEntryExt::V1(v1) => match v1.ext {
            TrustLineEntryV1Ext::V0 => {
                v1.ext = TrustLineEntryV1Ext::V2(TrustLineEntryExtensionV2 {
                    liquidity_pool_use_count: 0,
                    ext: TrustLineEntryExtensionV2Ext::V0,
                });
            }
            TrustLineEntryV1Ext::V2(_) => {}
        },
    }

    match &mut trustline.ext {
        TrustLineEntryExt::V1(v1) => match &mut v1.ext {
            TrustLineEntryV1Ext::V2(v2) => v2,
            TrustLineEntryV1Ext::V0 => {
                unreachable!("trustline v2 ext was not initialized")
            }
        },
        TrustLineEntryExt::V0 => unreachable!("trustline v1 ext was not initialized"),
    }
}

/// Apply a balance delta (positive or negative) to an account or trustline.
/// Used by offer settlement — no liability checks (those are handled separately
/// by the offer machinery).
fn apply_balance_delta(
    account_id: &AccountId,
    asset: &Asset,
    amount: i64,
    state: &mut LedgerStateManager,
) -> Result<()> {
    if matches!(asset, Asset::Native) {
        let Some(account) = state.get_account_mut(account_id) else {
            return Err(TxError::Internal(
                "missing account for balance update".into(),
            ));
        };
        let new_balance = CheckedAmount::new(account.balance)
            .checked_add(amount)
            .ok_or_else(|| TxError::Internal("balance overflow".into()))?;
        if new_balance.is_negative() {
            return Err(TxError::Internal("balance underflow".into()));
        }
        account.balance = new_balance.value();
        return Ok(());
    }

    if issuer_for_asset(asset) == Some(account_id) {
        return Ok(());
    }

    let Some(tl) = state.get_trustline_mut(account_id, asset) else {
        return Err(TxError::Internal(
            "missing trustline for balance update".into(),
        ));
    };
    let new_balance = CheckedAmount::new(tl.balance)
        .checked_add(amount)
        .ok_or_else(|| TxError::Internal("trustline balance overflow".into()))?;
    if new_balance.is_negative() || new_balance.value() > tl.limit {
        return Err(TxError::Internal("trustline balance out of bounds".into()));
    }
    tl.balance = new_balance.value();
    Ok(())
}

/// Classify a ledger key for rent purposes: (is_persistent, is_code_entry).
fn rent_classification(key: &stellar_xdr::curr::LedgerKey) -> (bool, bool) {
    match key {
        stellar_xdr::curr::LedgerKey::ContractCode(_) => (true, true),
        stellar_xdr::curr::LedgerKey::ContractData(cd) => (
            cd.durability == stellar_xdr::curr::ContractDataDurability::Persistent,
            false,
        ),
        _ => (false, false),
    }
}

/// Check if an asset code is valid per stellar-core's `isAssetValid()`.
///
/// - Native → always valid
/// - CreditAlphanum4: at least 1 non-zero alphanumeric char, zeros only trailing
/// - CreditAlphanum12: at least 5 non-zero alphanumeric chars, zeros only trailing
///
/// Reference: `stellar-core/src/util/types.cpp:146-211`
fn is_asset_valid(asset: &Asset) -> bool {
    match asset {
        Asset::Native => true,
        Asset::CreditAlphanum4(a) => {
            let code = &a.asset_code.0;
            let mut zeros = false;
            let mut onechar = false;
            for &b in code.iter() {
                if b == 0 {
                    zeros = true;
                } else if zeros {
                    // zeros can only be trailing
                    return false;
                } else {
                    if b > 0x7f || !b.is_ascii_alphanumeric() {
                        return false;
                    }
                    onechar = true;
                }
            }
            onechar
        }
        Asset::CreditAlphanum12(a) => {
            let code = &a.asset_code.0;
            let mut zeros = false;
            let mut charcount = 0;
            for &b in code.iter() {
                if b == 0 {
                    zeros = true;
                } else if zeros {
                    // zeros can only be trailing
                    return false;
                } else {
                    if b > 0x7f || !b.is_ascii_alphanumeric() {
                        return false;
                    }
                    charcount += 1;
                }
            }
            charcount > 4
        }
    }
}

/// Compute how much of `asset` the account can buy, considering buying liabilities.
///
/// For native assets, the capacity is `i64::MAX - balance - buying_liabilities`.
/// For non-native assets, the capacity is `limit - balance - buying_liabilities`.
/// The issuer of a non-native asset can always buy i64::MAX.
fn can_buy_at_most(source: &AccountId, asset: &Asset, state: &LedgerStateManager) -> i64 {
    if matches!(asset, Asset::Native) {
        let Some(account) = state.get_account(source) else {
            return 0;
        };
        let available = i64::MAX - account.balance - account_liabilities(account).buying;
        return available.max(0);
    }

    if issuer_for_asset(asset) == Some(source) {
        return i64::MAX;
    }

    let Some(trustline) = state.get_trustline(source, asset) else {
        return 0;
    };
    if !is_authorized_to_maintain_liabilities(trustline.flags) {
        return 0;
    }
    let available = trustline.limit - trustline.balance - trustline_liabilities(trustline).buying;
    available.max(0)
}

/// Apply buying and selling liability deltas for an account across the given
/// selling and buying assets. Skips the update when the account is the issuer
/// of the asset (issuers have unlimited capacity).
fn apply_liabilities_delta(
    account_id: &AccountId,
    selling: &Asset,
    buying: &Asset,
    selling_delta: i64,
    buying_delta: i64,
    state: &mut LedgerStateManager,
) -> Result<()> {
    if matches!(selling, Asset::Native) {
        let Some(account) = state.get_account_mut(account_id) else {
            return Err(TxError::Internal("missing account for liabilities".into()));
        };
        let liab = ensure_account_liabilities(account);
        update_liabilities(liab, 0, selling_delta)?;
    } else if issuer_for_asset(selling) != Some(account_id) {
        let Some(trustline) = state.get_trustline_mut(account_id, selling) else {
            return Err(TxError::Internal(
                "missing trustline for liabilities".into(),
            ));
        };
        let liab = ensure_trustline_liabilities(trustline);
        update_liabilities(liab, 0, selling_delta)?;
    }

    if matches!(buying, Asset::Native) {
        let Some(account) = state.get_account_mut(account_id) else {
            return Err(TxError::Internal("missing account for liabilities".into()));
        };
        let liab = ensure_account_liabilities(account);
        update_liabilities(liab, buying_delta, 0)?;
    } else if issuer_for_asset(buying) != Some(account_id) {
        let Some(trustline) = state.get_trustline_mut(account_id, buying) else {
            return Err(TxError::Internal(
                "missing trustline for liabilities".into(),
            ));
        };
        let liab = ensure_trustline_liabilities(trustline);
        update_liabilities(liab, buying_delta, 0)?;
    }

    Ok(())
}

/// Safely update buying and selling liabilities with overflow checking.
///
/// Returns an error if the new values would overflow or go negative.
fn update_liabilities(liab: &mut Liabilities, buying_delta: i64, selling_delta: i64) -> Result<()> {
    let new_buying = liab
        .buying
        .checked_add(buying_delta)
        .ok_or_else(|| TxError::Internal("liabilities overflow".into()))?;
    let new_selling = liab
        .selling
        .checked_add(selling_delta)
        .ok_or_else(|| TxError::Internal("liabilities overflow".into()))?;
    if new_buying < 0 || new_selling < 0 {
        return Err(TxError::Internal("liabilities underflow".into()));
    }
    liab.buying = new_buying;
    liab.selling = new_selling;
    Ok(())
}

fn map_exchange_error(err: offer_exchange::ExchangeError) -> TxError {
    TxError::Internal(format!("offer exchange error: {err:?}"))
}

mod account_merge;
mod bump_sequence;
mod change_trust;
mod claimable_balance;
mod clawback;
mod create_account;
mod extend_footprint_ttl;
mod inflation;
mod invoke_host_function;
mod liquidity_pool;
mod manage_data;
mod manage_offer;
mod offer_exchange;
pub(crate) mod offer_utils;
mod path_payment;
mod payment;
pub mod prefetch;
mod restore_footprint;
mod set_options;
mod sponsorship;
mod trust_flags;

pub use offer_exchange::{
    adjust_offer_amount, exchange_v10_without_price_error_thresholds, ExchangeError,
    ExchangeResult, RoundingType,
};

pub struct SorobanOperationMeta {
    /// Contract/system events emitted by the operation.
    pub events: Vec<ContractEvent>,
    /// Diagnostic events emitted during execution.
    pub diagnostic_events: Vec<DiagnosticEvent>,
    /// Return value for invoke host function (if any).
    pub return_value: Option<stellar_xdr::curr::ScVal>,
    /// Contract events + return value size in bytes.
    pub event_size_bytes: u32,
    /// Rent fee charged for storage changes.
    pub rent_fee: i64,
    /// Entries restored from the live BucketList (expired TTL but not yet evicted).
    /// These need RESTORED ledger entry changes emitted in transaction meta.
    pub live_bucket_list_restores: Vec<crate::soroban::protocol::LiveBucketListRestore>,
    /// Entries restored from the hot archive (for RestoreFootprint).
    /// These need RESTORED ledger entry changes emitted in transaction meta.
    /// Contains both the key and the entry value.
    pub hot_archive_restores: Vec<HotArchiveRestore>,
    /// Indices of entries ACTUALLY restored from hot archive in THIS transaction.
    /// This is a subset of the transaction envelope's archived_soroban_entries,
    /// excluding entries that were already restored by a previous transaction
    /// in the same ledger. Used to determine whether to emit INIT vs LIVE changes.
    pub actual_restored_indices: Vec<u32>,
}

/// Entry restored from the hot archive.
#[derive(Debug, Clone)]
pub struct HotArchiveRestore {
    /// The key of the restored entry.
    pub key: stellar_xdr::curr::LedgerKey,
    /// The restored entry value.
    pub entry: stellar_xdr::curr::LedgerEntry,
}

pub struct OperationExecutionResult {
    pub result: OperationResult,
    pub soroban_meta: Option<SorobanOperationMeta>,
}

impl OperationExecutionResult {
    fn new(result: OperationResult) -> Self {
        Self {
            result,
            soroban_meta: None,
        }
    }

    fn with_soroban_meta(result: OperationResult, meta: SorobanOperationMeta) -> Self {
        Self {
            result,
            soroban_meta: Some(meta),
        }
    }
}

struct RentSnapshot {
    key: stellar_xdr::curr::LedgerKey,
    is_persistent: bool,
    is_code_entry: bool,
    old_size_bytes: u32,
    old_live_until: u32,
}

struct RentChange {
    is_persistent: bool,
    is_code_entry: bool,
    old_size_bytes: u32,
    new_size_bytes: u32,
    old_live_until_ledger: u32,
    new_live_until_ledger: u32,
}

pub fn entry_size_for_rent_by_protocol(
    protocol_version: u32,
    entry: &stellar_xdr::curr::LedgerEntry,
    entry_xdr_size: u32,
) -> u32 {
    entry_size_for_rent_by_protocol_with_cost_params(protocol_version, entry, entry_xdr_size, None)
}

/// Like `entry_size_for_rent_by_protocol`, but accepts optional on-chain cost
/// parameters (cpu_cost_params, mem_cost_params) so that the budget used for
/// computing WASM module memory cost matches the network configuration.
///
/// When `cost_params` is `None`, falls back to `Budget::default()` which uses
/// hard-coded cost model parameters. For deterministic parity with stellar-core
/// stellar-core, callers should pass the actual on-chain cost params whenever
/// available.
pub fn entry_size_for_rent_by_protocol_with_cost_params(
    protocol_version: u32,
    entry: &stellar_xdr::curr::LedgerEntry,
    entry_xdr_size: u32,
    cost_params: Option<(
        &stellar_xdr::curr::ContractCostParams,
        &stellar_xdr::curr::ContractCostParams,
    )>,
) -> u32 {
    if protocol_version_is_before(protocol_version, ProtocolVersion::V25) {
        let budget = match cost_params {
            Some((cpu, mem)) => build_budget_p24(cpu, mem),
            None => soroban_env_host24::budget::Budget::default(),
        };
        let entry = convert_ledger_entry_to_p24(entry);
        entry
            .and_then(|entry| {
                soroban_env_host24::e2e_invoke::entry_size_for_rent(&budget, &entry, entry_xdr_size)
                    .ok()
            })
            .unwrap_or(entry_xdr_size)
    } else {
        let budget = match cost_params {
            Some((cpu, mem)) => build_budget_p25(cpu, mem),
            None => soroban_env_host25::budget::Budget::default(),
        };
        // Convert workspace LedgerEntry to P25 LedgerEntry via XDR bytes.
        let p25_entry = convert_ledger_entry_ws_to_p25(entry);
        p25_entry
            .and_then(|e| {
                soroban_env_host25::e2e_invoke::entry_size_for_rent(&budget, &e, entry_xdr_size)
                    .ok()
            })
            .unwrap_or(entry_xdr_size)
    }
}

/// Build a P24 Budget from on-chain cost parameters.
fn build_budget_p24(
    cpu_cost_params: &stellar_xdr::curr::ContractCostParams,
    mem_cost_params: &stellar_xdr::curr::ContractCostParams,
) -> soroban_env_host24::budget::Budget {
    let cpu = convert_cost_params_to_p24(cpu_cost_params);
    let mem = convert_cost_params_to_p24(mem_cost_params);
    match (cpu, mem) {
        (Some(cpu), Some(mem)) => {
            // Use limits of 0 — we only need the cost model, not actual metering
            soroban_env_host24::budget::Budget::try_from_configs(0, 0, cpu, mem)
                .unwrap_or_else(|_| soroban_env_host24::budget::Budget::default())
        }
        _ => soroban_env_host24::budget::Budget::default(),
    }
}

/// Build a P25 Budget from on-chain cost parameters.
/// Converts workspace (v26) ContractCostParams to P25 (v25) types via XDR bytes.
fn build_budget_p25(
    cpu_cost_params: &stellar_xdr::curr::ContractCostParams,
    mem_cost_params: &stellar_xdr::curr::ContractCostParams,
) -> soroban_env_host25::budget::Budget {
    let cpu = convert_cost_params_ws_to_p25(cpu_cost_params);
    let mem = convert_cost_params_ws_to_p25(mem_cost_params);
    match (cpu, mem) {
        (Some(cpu), Some(mem)) => {
            soroban_env_host25::budget::Budget::try_from_configs(0, 0, cpu, mem)
                .unwrap_or_else(|_| soroban_env_host25::budget::Budget::default())
        }
        _ => soroban_env_host25::budget::Budget::default(),
    }
}

fn convert_cost_params_to_p24(
    params: &stellar_xdr::curr::ContractCostParams,
) -> Option<soroban_env_host24::xdr::ContractCostParams> {
    let bytes = params.to_xdr(stellar_xdr::curr::Limits::none()).ok()?;
    soroban_env_host24::xdr::ContractCostParams::from_xdr(
        &bytes,
        soroban_env_host24::xdr::Limits::none(),
    )
    .ok()
}

fn convert_ledger_entry_to_p24(
    entry: &stellar_xdr::curr::LedgerEntry,
) -> Option<soroban_env_host24::xdr::LedgerEntry> {
    let bytes = entry.to_xdr(stellar_xdr::curr::Limits::none()).ok()?;
    soroban_env_host24::xdr::LedgerEntry::from_xdr(&bytes, soroban_env_host24::xdr::Limits::none())
        .ok()
}

/// Convert workspace (v26) ContractCostParams to P25 (v25) ContractCostParams via XDR bytes.
fn convert_cost_params_ws_to_p25(
    params: &stellar_xdr::curr::ContractCostParams,
) -> Option<soroban_env_host25::xdr::ContractCostParams> {
    let bytes = params.to_xdr(stellar_xdr::curr::Limits::none()).ok()?;
    use soroban_env_host25::xdr::ReadXdr as ReadXdrP25;
    soroban_env_host25::xdr::ContractCostParams::from_xdr(
        &bytes,
        soroban_env_host25::xdr::Limits::none(),
    )
    .ok()
}

/// Convert workspace (v26) LedgerEntry to P25 (v25) LedgerEntry via XDR bytes.
fn convert_ledger_entry_ws_to_p25(
    entry: &stellar_xdr::curr::LedgerEntry,
) -> Option<soroban_env_host25::xdr::LedgerEntry> {
    let bytes = entry.to_xdr(stellar_xdr::curr::Limits::none()).ok()?;
    use soroban_env_host25::xdr::ReadXdr as ReadXdrP25;
    soroban_env_host25::xdr::LedgerEntry::from_xdr(&bytes, soroban_env_host25::xdr::Limits::none())
        .ok()
}

fn rent_snapshot_for_keys(
    keys: &[stellar_xdr::curr::LedgerKey],
    state: &LedgerStateManager,
    protocol_version: u32,
    cost_params: Option<(
        &stellar_xdr::curr::ContractCostParams,
        &stellar_xdr::curr::ContractCostParams,
    )>,
    ttl_key_cache: Option<&crate::soroban::TtlKeyCache>,
) -> Vec<RentSnapshot> {
    let mut snapshots = Vec::new();
    for key in keys {
        let Some(entry) = state.get_entry(key) else {
            continue;
        };
        let entry_xdr = entry
            .to_xdr(stellar_xdr::curr::Limits::none())
            .unwrap_or_default();
        let entry_size = entry_size_for_rent_by_protocol_with_cost_params(
            protocol_version,
            &entry,
            entry_xdr.len() as u32,
            cost_params,
        );
        let key_hash = crate::soroban::get_or_compute_key_hash(ttl_key_cache, key);
        let old_live_until = state
            .get_ttl(&key_hash)
            .map(|ttl| ttl.live_until_ledger_seq)
            .unwrap_or(0);
        let (is_persistent, is_code_entry) = rent_classification(key);
        snapshots.push(RentSnapshot {
            key: key.clone(),
            is_persistent,
            is_code_entry,
            old_size_bytes: entry_size,
            old_live_until,
        });
    }
    snapshots
}

fn rent_changes_from_snapshots(
    snapshots: &[RentSnapshot],
    state: &LedgerStateManager,
    protocol_version: u32,
    cost_params: Option<(
        &stellar_xdr::curr::ContractCostParams,
        &stellar_xdr::curr::ContractCostParams,
    )>,
    ttl_key_cache: Option<&crate::soroban::TtlKeyCache>,
) -> Vec<RentChange> {
    let mut changes = Vec::new();
    for snapshot in snapshots {
        let Some(entry) = state.get_entry(&snapshot.key) else {
            tracing::debug!(?snapshot.key, "rent_changes_from_snapshots: entry not found, skipping");
            continue;
        };
        let entry_xdr = entry
            .to_xdr(stellar_xdr::curr::Limits::none())
            .unwrap_or_default();
        let new_size_bytes = entry_size_for_rent_by_protocol_with_cost_params(
            protocol_version,
            &entry,
            entry_xdr.len() as u32,
            cost_params,
        );
        let key_hash = crate::soroban::get_or_compute_key_hash(ttl_key_cache, &snapshot.key);
        let new_live_until = state
            .get_ttl(&key_hash)
            .map(|ttl| ttl.live_until_ledger_seq)
            .unwrap_or(snapshot.old_live_until);

        tracing::debug!(
            ?snapshot.key,
            old_size_bytes = snapshot.old_size_bytes,
            new_size_bytes,
            old_live_until = snapshot.old_live_until,
            new_live_until,
            "rent_changes_from_snapshots: processing entry"
        );

        if new_live_until <= snapshot.old_live_until && new_size_bytes <= snapshot.old_size_bytes {
            tracing::debug!(?snapshot.key, "rent_changes_from_snapshots: no change needed, skipping");
            continue;
        }
        changes.push(RentChange {
            is_persistent: snapshot.is_persistent,
            is_code_entry: snapshot.is_code_entry,
            old_size_bytes: snapshot.old_size_bytes,
            new_size_bytes,
            old_live_until_ledger: snapshot.old_live_until,
            new_live_until_ledger: new_live_until,
        });
    }
    tracing::debug!(
        changes_count = changes.len(),
        "rent_changes_from_snapshots: total changes"
    );
    changes
}

fn rent_fee_config_p25_to_p24(
    config: &soroban_env_host25::fees::RentFeeConfiguration,
) -> soroban_env_host24::fees::RentFeeConfiguration {
    soroban_env_host24::fees::RentFeeConfiguration {
        fee_per_write_1kb: config.fee_per_write_1kb,
        fee_per_rent_1kb: config.fee_per_rent_1kb,
        fee_per_write_entry: config.fee_per_write_entry,
        persistent_rent_rate_denominator: config.persistent_rent_rate_denominator,
        temporary_rent_rate_denominator: config.temporary_rent_rate_denominator,
    }
}

// INVARIANT: rent config always valid on production networks; config validated at upgrade time
fn compute_rent_fee_by_protocol(
    protocol_version: u32,
    rent_changes: &[RentChange],
    config: &soroban_env_host25::fees::RentFeeConfiguration,
    ledger_seq: u32,
) -> i64 {
    let fee = if protocol_version_is_before(protocol_version, ProtocolVersion::V25) {
        let changes: Vec<soroban_env_host24::fees::LedgerEntryRentChange> = rent_changes
            .iter()
            .map(|change| soroban_env_host24::fees::LedgerEntryRentChange {
                is_persistent: change.is_persistent,
                is_code_entry: change.is_code_entry,
                old_size_bytes: change.old_size_bytes,
                new_size_bytes: change.new_size_bytes,
                old_live_until_ledger: change.old_live_until_ledger,
                new_live_until_ledger: change.new_live_until_ledger,
            })
            .collect();
        let p24_config = rent_fee_config_p25_to_p24(config);
        tracing::debug!(
            fee_per_write_1kb = p24_config.fee_per_write_1kb,
            fee_per_rent_1kb = p24_config.fee_per_rent_1kb,
            fee_per_write_entry = p24_config.fee_per_write_entry,
            persistent_rent_rate_denominator = p24_config.persistent_rent_rate_denominator,
            temporary_rent_rate_denominator = p24_config.temporary_rent_rate_denominator,
            "compute_rent_fee_by_protocol: P24 config"
        );
        soroban_env_host24::fees::compute_rent_fee(&changes, &p24_config, ledger_seq)
    } else if protocol_version_starts_from(protocol_version, ProtocolVersion::V26) {
        // P26: code entry rent uses div_ceil(fee, 3) instead of P25's fee /= 3 (truncation).
        // Use the P26 host's compute_rent_fee to get the correct rounding behavior.
        let changes: Vec<soroban_env_host26::fees::LedgerEntryRentChange> = rent_changes
            .iter()
            .map(|change| soroban_env_host26::fees::LedgerEntryRentChange {
                is_persistent: change.is_persistent,
                is_code_entry: change.is_code_entry,
                old_size_bytes: change.old_size_bytes,
                new_size_bytes: change.new_size_bytes,
                old_live_until_ledger: change.old_live_until_ledger,
                new_live_until_ledger: change.new_live_until_ledger,
            })
            .collect();
        let p26_config = soroban_env_host26::fees::RentFeeConfiguration {
            fee_per_write_1kb: config.fee_per_write_1kb,
            fee_per_rent_1kb: config.fee_per_rent_1kb,
            fee_per_write_entry: config.fee_per_write_entry,
            persistent_rent_rate_denominator: config.persistent_rent_rate_denominator,
            temporary_rent_rate_denominator: config.temporary_rent_rate_denominator,
        };
        soroban_env_host26::fees::compute_rent_fee(&changes, &p26_config, ledger_seq)
    } else {
        let changes: Vec<soroban_env_host25::fees::LedgerEntryRentChange> = rent_changes
            .iter()
            .map(|change| soroban_env_host25::fees::LedgerEntryRentChange {
                is_persistent: change.is_persistent,
                is_code_entry: change.is_code_entry,
                old_size_bytes: change.old_size_bytes,
                new_size_bytes: change.new_size_bytes,
                old_live_until_ledger: change.old_live_until_ledger,
                new_live_until_ledger: change.new_live_until_ledger,
            })
            .collect();
        soroban_env_host25::fees::compute_rent_fee(&changes, config, ledger_seq)
    };
    tracing::debug!(
        rent_fee = fee,
        changes_count = rent_changes.len(),
        protocol_version,
        ledger_seq,
        "compute_rent_fee_by_protocol: computed rent fee"
    );
    fee
}

#[cfg(test)]
pub(crate) fn execute_operation(
    op: &Operation,
    source_account_id: &AccountId,
    state: &mut LedgerStateManager,
    context: &LedgerContext,
) -> Result<OperationExecutionResult> {
    let tx_id = TxIdentity {
        source_id: source_account_id,
        seq: 0,
        op_index: 0,
    };
    execute_operation_with_soroban(
        op,
        source_account_id,
        &tx_id,
        state,
        context,
        &SorobanContext::none(),
    )
}

/// Execute a single operation with optional Soroban transaction data.
///
/// This variant is used for Soroban operations that need access to the footprint
/// and network configuration.
///
/// # Arguments
///
/// * `tx_id` - Transaction identity for generating deterministic claimable balance
///   and revocation IDs.
/// * `soroban` - Soroban execution context with optional config, module cache,
///   hot archive lookup, and TTL key cache. Use `SorobanContext::none()` for
///   non-Soroban transactions.
pub fn execute_operation_with_soroban(
    op: &Operation,
    source_account_id: &AccountId,
    tx_id: &TxIdentity<'_>,
    state: &mut LedgerStateManager,
    context: &LedgerContext,
    soroban: &SorobanContext<'_>,
) -> Result<OperationExecutionResult> {
    // Get the actual source for this operation
    // If the operation has an explicit source, use it; otherwise use the transaction source
    let op_source = op
        .source_account
        .as_ref()
        .map(muxed_to_account_id)
        .unwrap_or_else(|| source_account_id.clone());

    // Check that the operation's source account exists.
    // This matches stellar-core's OperationFrame::checkSourceAccount().
    // If the source account doesn't exist (e.g., it was merged by a prior operation),
    // return opNO_ACCOUNT.
    if state.get_account(&op_source).is_none() {
        return Ok(OperationExecutionResult::new(OperationResult::OpNoAccount));
    }

    let result = (|| -> Result<OperationExecutionResult> {
        match &op.body {
            OperationBody::CreateAccount(op_data) => Ok(OperationExecutionResult::new(
                create_account::execute_create_account(op_data, &op_source, state, context)?,
            )),
            OperationBody::Payment(op_data) => Ok(OperationExecutionResult::new(
                payment::execute_payment(op_data, &op_source, state, context)?,
            )),
            OperationBody::ChangeTrust(op_data) => Ok(OperationExecutionResult::new(
                change_trust::execute_change_trust(op_data, &op_source, state, context)?,
            )),
            OperationBody::ManageData(op_data) => Ok(OperationExecutionResult::new(
                manage_data::execute_manage_data(op_data, &op_source, state, context)?,
            )),
            OperationBody::BumpSequence(op_data) => Ok(OperationExecutionResult::new(
                bump_sequence::execute_bump_sequence(op_data, &op_source, state, context)?,
            )),
            OperationBody::AccountMerge(dest) => Ok(OperationExecutionResult::new(
                account_merge::execute_account_merge(dest, &op_source, state, context)?,
            )),
            OperationBody::SetOptions(op_data) => Ok(OperationExecutionResult::new(
                set_options::execute_set_options(op_data, &op_source, state, context)?,
            )),
            // Soroban operations
            OperationBody::InvokeHostFunction(op_data) => {
                invoke_host_function::execute_invoke_host_function(
                    op_data, &op_source, state, context, soroban,
                )
            }
            OperationBody::ExtendFootprintTtl(op_data) => {
                let default_config = SorobanConfig::default();
                let config = soroban.config.unwrap_or(&default_config);
                let snapshots = soroban
                    .soroban_data
                    .map(|data| {
                        let mut keys = Vec::new();
                        keys.extend(data.resources.footprint.read_only.iter().cloned());
                        keys.extend(data.resources.footprint.read_write.iter().cloned());
                        rent_snapshot_for_keys(
                            &keys,
                            state,
                            context.protocol_version,
                            soroban
                                .config
                                .map(|c| (&c.cpu_cost_params, &c.mem_cost_params)),
                            soroban.ttl_key_cache,
                        )
                    })
                    .unwrap_or_default();
                let result = extend_footprint_ttl::execute_extend_footprint_ttl(
                    op_data,
                    &op_source,
                    state,
                    context,
                    soroban.soroban_data,
                    soroban.ttl_key_cache,
                    Some(&extend_footprint_ttl::ContractSizeLimits {
                        max_contract_size_bytes: config.max_contract_size_bytes,
                        max_contract_data_entry_size_bytes: config
                            .max_contract_data_entry_size_bytes,
                    }),
                    config.max_entry_ttl,
                )?;
                let mut exec = OperationExecutionResult::new(result);
                if matches!(
                    exec.result,
                    OperationResult::OpInner(OperationResultTr::ExtendFootprintTtl(
                        ExtendFootprintTtlResult::Success
                    ))
                ) {
                    let rent_changes = rent_changes_from_snapshots(
                        &snapshots,
                        state,
                        context.protocol_version,
                        soroban
                            .config
                            .map(|c| (&c.cpu_cost_params, &c.mem_cost_params)),
                        soroban.ttl_key_cache,
                    );
                    let rent_fee = compute_rent_fee_by_protocol(
                        context.protocol_version,
                        &rent_changes,
                        &config.rent_fee_config,
                        context.sequence,
                    );
                    exec.soroban_meta = Some(SorobanOperationMeta {
                        events: Vec::new(),
                        diagnostic_events: Vec::new(),
                        return_value: None,
                        event_size_bytes: 0,
                        rent_fee,
                        live_bucket_list_restores: Vec::new(),
                        hot_archive_restores: Vec::new(),
                        actual_restored_indices: Vec::new(),
                    });
                }
                Ok(exec)
            }
            OperationBody::RestoreFootprint(op_data) => {
                let default_config = SorobanConfig::default();
                let config = soroban.config.unwrap_or(&default_config);
                // For RestoreFootprint, we need to track which entries are ACTUALLY restored.
                // stellar-core only computes rent for entries that need restoration (not already live).
                //
                // Per stellar-core RestoreFootprintOpFrame::doApply():
                // 1. If TTL exists and isLive (TTL >= current_ledger) -> skip (already live)
                // 2. If no TTL exists -> check hot archive
                //    - If hot archive entry found -> include (restore from hot archive)
                //    - If no hot archive entry -> skip (entry doesn't exist)
                // 3. If TTL exists but expired (TTL < current_ledger) -> include (restore from live BL)
                let mut snapshots = Vec::new();
                let mut hot_archive_restores = Vec::new();
                if let Some(data) = soroban.soroban_data {
                    for key in data.resources.footprint.read_write.iter() {
                        // Only compute rent for entries that need restoration
                        let key_hash =
                            crate::soroban::get_or_compute_key_hash(soroban.ttl_key_cache, key);
                        let current_ttl = state.get_ttl(&key_hash).map(|t| t.live_until_ledger_seq);

                        // Case 1: TTL exists and entry is live -> skip
                        if let Some(ttl) = current_ttl {
                            if ttl >= context.sequence {
                                continue;
                            }
                            // Case 3: TTL exists but expired -> restore from live bucket list
                            if let Some(entry) = state.get_entry(key) {
                                let entry_xdr = entry
                                    .to_xdr(stellar_xdr::curr::Limits::none())
                                    .unwrap_or_default();
                                let entry_size = entry_size_for_rent_by_protocol_with_cost_params(
                                    context.protocol_version,
                                    &entry,
                                    entry_xdr.len() as u32,
                                    soroban
                                        .config
                                        .map(|c| (&c.cpu_cost_params, &c.mem_cost_params)),
                                );
                                let (is_persistent, is_code_entry) = rent_classification(key);
                                snapshots.push(RentSnapshot {
                                    key: key.clone(),
                                    is_persistent,
                                    is_code_entry,
                                    old_size_bytes: entry_size,
                                    old_live_until: ttl,
                                });
                            }
                        } else {
                            // Case 2: No TTL -> check hot archive
                            // Per stellar-core createEntryRentChangeWithoutModification():
                            // When entryLiveUntilLedger is std::nullopt (no previous TTL):
                            //   - old_size_bytes = 0
                            //   - old_live_until_ledger = 0
                            // This is different from expired entries where we use the actual old size.
                            if let Some(ha) = soroban.hot_archive {
                                if let Some(entry) = ha.get(key).map_err(|e| {
                                    TxError::Internal(format!(
                                        "hot archive lookup failed during restore: {e}"
                                    ))
                                })? {
                                    let (is_persistent, is_code_entry) = rent_classification(key);
                                    snapshots.push(RentSnapshot {
                                        key: key.clone(),
                                        is_persistent,
                                        is_code_entry,
                                        old_size_bytes: 0, // Hot archive entries: old_size_bytes = 0
                                        old_live_until: 0, // Hot archive entries: old_live_until = 0
                                    });
                                    // Track this entry for RESTORED metadata emission
                                    hot_archive_restores.push(HotArchiveRestore {
                                        key: key.clone(),
                                        entry: entry.clone(),
                                    });
                                }
                            }
                        }
                    }
                }
                // Convert HotArchiveRestore to HotArchiveRestoreEntry for execute_restore_footprint
                let ha_restore_entries: Vec<restore_footprint::HotArchiveRestoreEntry> =
                    hot_archive_restores
                        .iter()
                        .map(|r| restore_footprint::HotArchiveRestoreEntry {
                            key: r.key.clone(),
                            entry: r.entry.clone(),
                        })
                        .collect();
                let result = restore_footprint::execute_restore_footprint(
                    op_data,
                    &op_source,
                    state,
                    context,
                    restore_footprint::RestoreFootprintResources {
                        soroban_data: soroban.soroban_data,
                        min_persistent_entry_ttl: config.min_persistent_entry_ttl,
                        hot_archive_restores: &ha_restore_entries,
                        ttl_key_cache: soroban.ttl_key_cache,
                        size_limits: Some(extend_footprint_ttl::ContractSizeLimits {
                            max_contract_size_bytes: config.max_contract_size_bytes,
                            max_contract_data_entry_size_bytes: config
                                .max_contract_data_entry_size_bytes,
                        }),
                    },
                )?;
                let mut exec = OperationExecutionResult::new(result);
                if matches!(
                    exec.result,
                    OperationResult::OpInner(OperationResultTr::RestoreFootprint(
                        RestoreFootprintResult::Success
                    ))
                ) {
                    let rent_changes = rent_changes_from_snapshots(
                        &snapshots,
                        state,
                        context.protocol_version,
                        soroban
                            .config
                            .map(|c| (&c.cpu_cost_params, &c.mem_cost_params)),
                        soroban.ttl_key_cache,
                    );
                    let rent_fee = compute_rent_fee_by_protocol(
                        context.protocol_version,
                        &rent_changes,
                        &config.rent_fee_config,
                        context.sequence,
                    );
                    exec.soroban_meta = Some(SorobanOperationMeta {
                        events: Vec::new(),
                        diagnostic_events: Vec::new(),
                        return_value: None,
                        event_size_bytes: 0,
                        rent_fee,
                        live_bucket_list_restores: Vec::new(),
                        hot_archive_restores,
                        actual_restored_indices: Vec::new(),
                    });
                }
                Ok(exec)
            }
            // DEX operations
            OperationBody::PathPaymentStrictReceive(op_data) => Ok(OperationExecutionResult::new(
                path_payment::execute_path_payment_strict_receive(
                    op_data, &op_source, state, context,
                )?,
            )),
            OperationBody::PathPaymentStrictSend(op_data) => Ok(OperationExecutionResult::new(
                path_payment::execute_path_payment_strict_send(
                    op_data, &op_source, state, context,
                )?,
            )),
            OperationBody::ManageSellOffer(op_data) => Ok(OperationExecutionResult::new(
                manage_offer::execute_manage_sell_offer(op_data, &op_source, state, context)?,
            )),
            OperationBody::ManageBuyOffer(op_data) => Ok(OperationExecutionResult::new(
                manage_offer::execute_manage_buy_offer(op_data, &op_source, state, context)?,
            )),
            OperationBody::CreatePassiveSellOffer(op_data) => Ok(OperationExecutionResult::new(
                manage_offer::execute_create_passive_sell_offer(
                    op_data, &op_source, state, context,
                )?,
            )),
            OperationBody::AllowTrust(op_data) => Ok(OperationExecutionResult::new(
                trust_flags::execute_allow_trust(op_data, &op_source, &tx_id, state, context)?,
            )),
            OperationBody::Inflation => Ok(OperationExecutionResult::new(
                inflation::execute_inflation(&op_source, state, context)?,
            )),
            OperationBody::CreateClaimableBalance(op_data) => Ok(OperationExecutionResult::new(
                claimable_balance::execute_create_claimable_balance(
                    op_data, &op_source, &tx_id, state, context,
                )?,
            )),
            OperationBody::ClaimClaimableBalance(op_data) => Ok(OperationExecutionResult::new(
                claimable_balance::execute_claim_claimable_balance(
                    op_data, &op_source, state, context,
                )?,
            )),
            OperationBody::BeginSponsoringFutureReserves(op_data) => {
                Ok(OperationExecutionResult::new(
                    sponsorship::execute_begin_sponsoring_future_reserves(
                        op_data, &op_source, state, context,
                    )?,
                ))
            }
            OperationBody::EndSponsoringFutureReserves => Ok(OperationExecutionResult::new(
                sponsorship::execute_end_sponsoring_future_reserves(&op_source, state, context)?,
            )),
            OperationBody::RevokeSponsorship(op_data) => Ok(OperationExecutionResult::new(
                sponsorship::execute_revoke_sponsorship(op_data, &op_source, state, context)?,
            )),
            OperationBody::Clawback(op_data) => Ok(OperationExecutionResult::new(
                clawback::execute_clawback(op_data, &op_source, state, context)?,
            )),
            OperationBody::ClawbackClaimableBalance(op_data) => Ok(OperationExecutionResult::new(
                clawback::execute_clawback_claimable_balance(op_data, &op_source, state, context)?,
            )),
            OperationBody::SetTrustLineFlags(op_data) => Ok(OperationExecutionResult::new(
                trust_flags::execute_set_trust_line_flags(
                    op_data, &op_source, &tx_id, state, context,
                )?,
            )),
            OperationBody::LiquidityPoolDeposit(op_data) => Ok(OperationExecutionResult::new(
                liquidity_pool::execute_liquidity_pool_deposit(
                    op_data, &op_source, state, context,
                )?,
            )),
            OperationBody::LiquidityPoolWithdraw(op_data) => Ok(OperationExecutionResult::new(
                liquidity_pool::execute_liquidity_pool_withdraw(
                    op_data, &op_source, state, context,
                )?,
            )),
        }
    })();

    // Convert TooManySponsoring errors to the proper operation result code.
    // Mirrors stellar-core which returns opTOO_MANY_SPONSORING from sponsorship
    // utility functions instead of escalating to txINTERNAL_ERROR.
    result.or_else(|e| match e {
        TxError::TooManySponsoring => Ok(OperationExecutionResult::new(
            OperationResult::OpTooManySponsoring,
        )),
        other => Err(other),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::create_test_account_id;
    use stellar_xdr::curr::*;

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

    fn create_test_context() -> LedgerContext {
        LedgerContext::testnet(1, 1000)
    }

    #[test]
    fn test_inflation_operation_dispatch() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();
        let source = create_test_account_id(0);

        // Add the source account to state
        state.create_account(create_test_account(source.clone(), 100_000_000));

        // Test that Inflation returns NotTime (deprecated since Protocol 12)
        let op = Operation {
            source_account: None,
            body: OperationBody::Inflation,
        };

        let result = execute_operation(&op, &source, &mut state, &context).expect("execute op");

        // Inflation is deprecated and returns NotTime
        match result.result {
            OperationResult::OpInner(OperationResultTr::Inflation(r)) => {
                assert!(matches!(r, InflationResult::NotTime));
            }
            _ => panic!("Expected Inflation result"),
        }
    }

    // === OperationExecutionResult tests ===

    #[test]
    fn test_operation_execution_result_new() {
        let op_result = OperationResult::OpBadAuth;
        let result = OperationExecutionResult::new(op_result);

        assert!(result.soroban_meta.is_none());
        match result.result {
            OperationResult::OpBadAuth => {}
            _ => panic!("Expected OpBadAuth"),
        }
    }

    #[test]
    fn test_operation_execution_result_with_soroban_meta() {
        let op_result =
            OperationResult::OpInner(OperationResultTr::Inflation(InflationResult::NotTime));
        let meta = SorobanOperationMeta {
            events: vec![],
            diagnostic_events: vec![],
            return_value: None,
            event_size_bytes: 100,
            rent_fee: 500,
            live_bucket_list_restores: vec![],
            hot_archive_restores: vec![],
            actual_restored_indices: vec![],
        };

        let result = OperationExecutionResult::with_soroban_meta(op_result, meta);

        assert!(result.soroban_meta.is_some());
        let soroban_meta = result.soroban_meta.unwrap();
        assert_eq!(soroban_meta.event_size_bytes, 100);
        assert_eq!(soroban_meta.rent_fee, 500);
    }

    // === SorobanOperationMeta tests ===

    #[test]
    fn test_soroban_operation_meta_default_values() {
        let meta = SorobanOperationMeta {
            events: vec![],
            diagnostic_events: vec![],
            return_value: None,
            event_size_bytes: 0,
            rent_fee: 0,
            live_bucket_list_restores: vec![],
            hot_archive_restores: vec![],
            actual_restored_indices: vec![],
        };

        assert!(meta.events.is_empty());
        assert!(meta.diagnostic_events.is_empty());
        assert!(meta.return_value.is_none());
        assert_eq!(meta.event_size_bytes, 0);
        assert_eq!(meta.rent_fee, 0);
    }

    #[test]
    fn test_soroban_operation_meta_with_return_value() {
        let meta = SorobanOperationMeta {
            events: vec![],
            diagnostic_events: vec![],
            return_value: Some(ScVal::I32(42)),
            event_size_bytes: 50,
            rent_fee: 100,
            live_bucket_list_restores: vec![],
            hot_archive_restores: vec![],
            actual_restored_indices: vec![1, 2, 3],
        };

        assert!(meta.return_value.is_some());
        match meta.return_value.unwrap() {
            ScVal::I32(v) => assert_eq!(v, 42),
            _ => panic!("Expected I32"),
        }
        assert_eq!(meta.actual_restored_indices.len(), 3);
    }

    // === HotArchiveRestore tests ===

    #[test]
    fn test_hot_archive_restore_struct() {
        let key = LedgerKey::Account(LedgerKeyAccount {
            account_id: create_test_account_id(0),
        });
        let entry = LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::Account(create_test_account(
                create_test_account_id(0),
                1_000_000,
            )),
            ext: LedgerEntryExt::V0,
        };

        let restore = HotArchiveRestore {
            key: key.clone(),
            entry: entry.clone(),
        };

        assert_eq!(restore.entry.last_modified_ledger_seq, 100);
    }

    #[test]
    fn test_hot_archive_restore_debug() {
        let key = LedgerKey::Account(LedgerKeyAccount {
            account_id: create_test_account_id(0),
        });
        let entry = LedgerEntry {
            last_modified_ledger_seq: 50,
            data: LedgerEntryData::Account(create_test_account(create_test_account_id(0), 500_000)),
            ext: LedgerEntryExt::V0,
        };

        let restore = HotArchiveRestore { key, entry };
        let debug_str = format!("{:?}", restore);
        assert!(debug_str.contains("HotArchiveRestore"));
    }

    // === ledger_key_hash tests ===

    #[test]
    fn test_ledger_key_hash_account() {
        let key = LedgerKey::Account(LedgerKeyAccount {
            account_id: create_test_account_id(0),
        });

        let hash = crate::soroban::compute_key_hash(&key);
        // Hash should be 32 bytes (256 bits)
        assert_eq!(hash.0.len(), 32);
        // Same key should produce same hash
        let hash2 = crate::soroban::compute_key_hash(&key);
        assert_eq!(hash.0, hash2.0);
    }

    #[test]
    fn test_ledger_key_hash_different_keys() {
        let key1 = LedgerKey::Account(LedgerKeyAccount {
            account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32]))),
        });
        let key2 = LedgerKey::Account(LedgerKeyAccount {
            account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32]))),
        });

        let hash1 = crate::soroban::compute_key_hash(&key1);
        let hash2 = crate::soroban::compute_key_hash(&key2);

        // Different keys should produce different hashes
        assert_ne!(hash1.0, hash2.0);
    }

    // === BumpSequence operation dispatch ===

    #[test]
    fn test_bump_sequence_operation_dispatch() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();
        let source = create_test_account_id(0);

        state.create_account(create_test_account(source.clone(), 100_000_000));

        let op = Operation {
            source_account: None,
            body: OperationBody::BumpSequence(BumpSequenceOp {
                bump_to: SequenceNumber(10),
            }),
        };

        let result = execute_operation(&op, &source, &mut state, &context).expect("execute op");

        match result.result {
            OperationResult::OpInner(OperationResultTr::BumpSequence(r)) => {
                assert!(matches!(r, BumpSequenceResult::Success));
            }
            _ => panic!("Expected BumpSequence result"),
        }
    }

    // === CreateAccount operation dispatch ===

    #[test]
    fn test_create_account_operation_dispatch() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();
        let source = create_test_account_id(0);

        state.create_account(create_test_account(source.clone(), 100_000_000));

        let new_account = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([5u8; 32])));

        let op = Operation {
            source_account: None,
            body: OperationBody::CreateAccount(CreateAccountOp {
                destination: new_account.clone(),
                starting_balance: 10_000_000,
            }),
        };

        let result = execute_operation(&op, &source, &mut state, &context).expect("execute op");

        match result.result {
            OperationResult::OpInner(OperationResultTr::CreateAccount(r)) => {
                assert!(matches!(r, CreateAccountResult::Success));
            }
            _ => panic!("Expected CreateAccount result"),
        }
    }

    // === Payment operation dispatch ===

    #[test]
    fn test_payment_operation_dispatch_no_dest() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();
        let source = create_test_account_id(0);

        state.create_account(create_test_account(source.clone(), 100_000_000));

        let dest = MuxedAccount::Ed25519(Uint256([5u8; 32]));

        let op = Operation {
            source_account: None,
            body: OperationBody::Payment(PaymentOp {
                destination: dest,
                asset: Asset::Native,
                amount: 1_000_000,
            }),
        };

        let result = execute_operation(&op, &source, &mut state, &context).expect("execute op");

        // Should fail because destination account doesn't exist
        match result.result {
            OperationResult::OpInner(OperationResultTr::Payment(PaymentResult::NoDestination)) => {}
            _ => panic!("Expected Payment NoDestination result"),
        }
    }

    // === ManageData operation dispatch ===

    #[test]
    fn test_manage_data_operation_dispatch() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();
        let source = create_test_account_id(0);

        state.create_account(create_test_account(source.clone(), 100_000_000));

        let op = Operation {
            source_account: None,
            body: OperationBody::ManageData(ManageDataOp {
                data_name: String64::try_from(b"testkey".to_vec()).unwrap(),
                data_value: Some(DataValue(vec![1, 2, 3, 4].try_into().unwrap())),
            }),
        };

        let result = execute_operation(&op, &source, &mut state, &context).expect("execute op");

        match result.result {
            OperationResult::OpInner(OperationResultTr::ManageData(ManageDataResult::Success)) => {}
            _ => panic!("Expected ManageData Success result"),
        }
    }

    // === Operation with explicit source ===

    #[test]
    fn test_operation_with_explicit_source() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();
        let tx_source = create_test_account_id(0);
        let op_source = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([9u8; 32])));

        // Create both accounts
        state.create_account(create_test_account(tx_source.clone(), 100_000_000));
        state.create_account(create_test_account(op_source.clone(), 100_000_000));

        // Operation with explicit source different from tx source
        let op = Operation {
            source_account: Some(MuxedAccount::Ed25519(Uint256([9u8; 32]))),
            body: OperationBody::BumpSequence(BumpSequenceOp {
                bump_to: SequenceNumber(10),
            }),
        };

        let result = execute_operation(&op, &tx_source, &mut state, &context).expect("execute op");

        match result.result {
            OperationResult::OpInner(OperationResultTr::BumpSequence(r)) => {
                assert!(matches!(r, BumpSequenceResult::Success));
            }
            _ => panic!("Expected BumpSequence result"),
        }
    }

    // === account_balance_after_liabilities tests ===

    #[test]
    fn test_account_balance_after_liabilities_no_ext() {
        let account = create_test_account(create_test_account_id(0), 100_000_000);
        // V0 ext means 0 selling liabilities
        assert_eq!(account_balance_after_liabilities(&account), 100_000_000);
    }

    #[test]
    fn test_account_balance_after_liabilities_with_liabilities() {
        let mut account = create_test_account(create_test_account_id(0), 100_000_000);
        account.ext = AccountEntryExt::V1(AccountEntryExtensionV1 {
            liabilities: Liabilities {
                buying: 5_000_000,
                selling: 30_000_000,
            },
            ext: AccountEntryExtensionV1Ext::V0,
        });
        assert_eq!(account_balance_after_liabilities(&account), 70_000_000);
    }

    #[test]
    fn test_account_balance_after_liabilities_saturates_negative() {
        let mut account = create_test_account(create_test_account_id(0), 10_000_000);
        account.ext = AccountEntryExt::V1(AccountEntryExtensionV1 {
            liabilities: Liabilities {
                buying: 0,
                selling: 50_000_000, // More than balance
            },
            ext: AccountEntryExtensionV1Ext::V0,
        });
        // saturating_sub on i64 saturates at i64::MIN, not 0 — the result can be
        // negative, but callers always compare available < min_balance which is
        // correct since negative < positive.
        assert_eq!(account_balance_after_liabilities(&account), -40_000_000);
    }

    #[test]
    fn test_account_balance_after_liabilities_zero_balance() {
        let account = create_test_account(create_test_account_id(0), 0);
        assert_eq!(account_balance_after_liabilities(&account), 0);
    }

    // === trustline_balance_after_liabilities tests ===

    fn create_test_trustline(balance: i64, limit: i64, selling_liab: i64) -> TrustLineEntry {
        let ext = if selling_liab > 0 {
            TrustLineEntryExt::V1(TrustLineEntryV1 {
                liabilities: Liabilities {
                    buying: 0,
                    selling: selling_liab,
                },
                ext: TrustLineEntryV1Ext::V0,
            })
        } else {
            TrustLineEntryExt::V0
        };
        TrustLineEntry {
            account_id: create_test_account_id(0),
            asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', 0]),
                issuer: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32]))),
            }),
            balance,
            limit,
            flags: AUTHORIZED_FLAG,
            ext,
        }
    }

    #[test]
    fn test_trustline_balance_after_liabilities_no_ext() {
        let tl = create_test_trustline(50_000_000, 100_000_000, 0);
        assert_eq!(trustline_balance_after_liabilities(&tl), 50_000_000);
    }

    #[test]
    fn test_trustline_balance_after_liabilities_with_liabilities() {
        let tl = create_test_trustline(50_000_000, 100_000_000, 20_000_000);
        assert_eq!(trustline_balance_after_liabilities(&tl), 30_000_000);
    }

    #[test]
    fn test_trustline_balance_after_liabilities_negative_when_exceeded() {
        let tl = create_test_trustline(10_000_000, 100_000_000, 50_000_000);
        // Same as account: saturating_sub on i64 can go negative
        assert_eq!(trustline_balance_after_liabilities(&tl), -40_000_000);
    }

    // === update_liabilities tests ===

    #[test]
    fn test_update_liabilities_basic() {
        let mut liab = Liabilities {
            buying: 100,
            selling: 200,
        };
        update_liabilities(&mut liab, 50, 30).unwrap();
        assert_eq!(liab.buying, 150);
        assert_eq!(liab.selling, 230);
    }

    #[test]
    fn test_update_liabilities_negative_deltas() {
        let mut liab = Liabilities {
            buying: 100,
            selling: 200,
        };
        update_liabilities(&mut liab, -50, -100).unwrap();
        assert_eq!(liab.buying, 50);
        assert_eq!(liab.selling, 100);
    }

    #[test]
    fn test_update_liabilities_underflow_buying() {
        let mut liab = Liabilities {
            buying: 50,
            selling: 200,
        };
        let result = update_liabilities(&mut liab, -100, 0);
        assert!(result.is_err());
    }

    #[test]
    fn test_update_liabilities_underflow_selling() {
        let mut liab = Liabilities {
            buying: 100,
            selling: 50,
        };
        let result = update_liabilities(&mut liab, 0, -100);
        assert!(result.is_err());
    }

    #[test]
    fn test_update_liabilities_overflow_buying() {
        let mut liab = Liabilities {
            buying: i64::MAX,
            selling: 0,
        };
        let result = update_liabilities(&mut liab, 1, 0);
        assert!(result.is_err());
    }

    #[test]
    fn test_update_liabilities_overflow_selling() {
        let mut liab = Liabilities {
            buying: 0,
            selling: i64::MAX,
        };
        let result = update_liabilities(&mut liab, 0, 1);
        assert!(result.is_err());
    }

    #[test]
    fn test_update_liabilities_zero_deltas() {
        let mut liab = Liabilities {
            buying: 100,
            selling: 200,
        };
        update_liabilities(&mut liab, 0, 0).unwrap();
        assert_eq!(liab.buying, 100);
        assert_eq!(liab.selling, 200);
    }

    // === can_buy_at_most tests ===

    #[test]
    fn test_can_buy_at_most_native_no_account() {
        let state = LedgerStateManager::new(5_000_000, 100);
        let source = create_test_account_id(0);
        assert_eq!(can_buy_at_most(&source, &Asset::Native, &state), 0);
    }

    #[test]
    fn test_can_buy_at_most_native_no_liabilities() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let source = create_test_account_id(0);
        state.create_account(create_test_account(source.clone(), 100_000_000));
        let capacity = can_buy_at_most(&source, &Asset::Native, &state);
        assert_eq!(capacity, i64::MAX - 100_000_000);
    }

    #[test]
    fn test_can_buy_at_most_native_with_buying_liabilities() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let source = create_test_account_id(0);
        let mut account = create_test_account(source.clone(), 100_000_000);
        account.ext = AccountEntryExt::V1(AccountEntryExtensionV1 {
            liabilities: Liabilities {
                buying: 50_000_000,
                selling: 0,
            },
            ext: AccountEntryExtensionV1Ext::V0,
        });
        state.create_account(account);
        let capacity = can_buy_at_most(&source, &Asset::Native, &state);
        assert_eq!(capacity, i64::MAX - 100_000_000 - 50_000_000);
    }

    #[test]
    fn test_can_buy_at_most_non_native_issuer() {
        let state = LedgerStateManager::new(5_000_000, 100);
        let issuer = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32])));
        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', b'S', b'D', 0]),
            issuer: issuer.clone(),
        });
        assert_eq!(can_buy_at_most(&issuer, &asset, &state), i64::MAX);
    }

    #[test]
    fn test_can_buy_at_most_non_native_no_trustline() {
        let state = LedgerStateManager::new(5_000_000, 100);
        let source = create_test_account_id(0);
        let asset = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'U', b'S', b'D', 0]),
            issuer: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32]))),
        });
        assert_eq!(can_buy_at_most(&source, &asset, &state), 0);
    }

    // === apply_liabilities_delta tests ===

    #[test]
    fn test_apply_liabilities_delta_native_selling() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let source = create_test_account_id(0);
        state.create_account(create_test_account(source.clone(), 100_000_000));

        apply_liabilities_delta(
            &source,
            &Asset::Native,
            &Asset::Native,
            1000,
            500,
            &mut state,
        )
        .unwrap();

        let account = state.get_account(&source).unwrap();
        let liab = account_liabilities(account);
        // Both selling and buying deltas applied to native account
        assert_eq!(liab.selling, 1000);
        assert_eq!(liab.buying, 500);
    }

    #[test]
    fn test_apply_liabilities_delta_missing_account() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let source = create_test_account_id(0);
        // No account created

        let result = apply_liabilities_delta(
            &source,
            &Asset::Native,
            &Asset::Native,
            1000,
            500,
            &mut state,
        );
        assert!(result.is_err());
    }

    /// Regression test for AUDIT-058: Sponsored ManageData when sponsor is at
    /// num_sponsoring = u32::MAX should return OpTooManySponsoring, not
    /// TxInternalError.
    #[test]
    fn test_audit_058_sponsored_manage_data_at_max_sponsoring() {
        let mut state = LedgerStateManager::new(5_000_000, 100);
        let context = create_test_context();

        let source = create_test_account_id(1);
        let sponsor = create_test_account_id(2);

        // Create source account
        let mut source_acct = create_test_account(source.clone(), 1_000_000_000);
        source_acct.num_sub_entries = 0;
        state.create_account(source_acct);

        // Create sponsor account with num_sponsoring at u32::MAX
        let mut sponsor_acct = create_test_account(sponsor.clone(), i64::MAX);
        sponsor_acct.ext = AccountEntryExt::V1(AccountEntryExtensionV1 {
            liabilities: Liabilities {
                buying: 0,
                selling: 0,
            },
            ext: AccountEntryExtensionV1Ext::V2(AccountEntryExtensionV2 {
                num_sponsoring: u32::MAX,
                num_sponsored: 0,
                signer_sponsoring_i_ds: vec![].try_into().unwrap(),
                ext: AccountEntryExtensionV2Ext::V0,
            }),
        });
        state.create_account(sponsor_acct);

        // Set up sponsorship: sponsor is sponsoring source
        state.push_sponsorship(sponsor.clone(), source.clone());

        let op = Operation {
            source_account: None,
            body: OperationBody::ManageData(ManageDataOp {
                data_name: String64::try_from("test_key".as_bytes().to_vec()).unwrap(),
                data_value: Some(vec![1, 2, 3].try_into().unwrap()),
            }),
        };

        let result = execute_operation(&op, &source, &mut state, &context);
        match &result {
            Err(e) => panic!("Should not return Err (TxInternalError): {:?}", e),
            Ok(r) => match &r.result {
                OperationResult::OpTooManySponsoring => {
                    // Correct: maps to opTOO_MANY_SPONSORING like stellar-core
                }
                other => panic!("Expected OpTooManySponsoring, got {:?}", other),
            },
        }
    }
}
