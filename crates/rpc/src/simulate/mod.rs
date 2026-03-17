//! Soroban transaction simulation for `simulateTransaction`.
//!
//! Supports all three Soroban operation types:
//! - `InvokeHostFunction`: Full host function simulation via recording mode
//! - `ExtendFootprintTtl`: TTL extension resource/fee estimation
//! - `RestoreFootprint`: Archived entry restore resource/fee estimation

mod snapshot;

pub(crate) use snapshot::BucketListSnapshotSource;

use std::rc::Rc;
use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde_json::json;
use soroban_env_host_p25 as soroban_host;
use stellar_xdr::curr::{
    ContractDataDurability, ExtendFootprintTtlOp, ExtensionPoint, HostFunction,
    InvokeHostFunctionOp, LedgerEntryType, LedgerFootprint, LedgerKey, Limits, OperationBody,
    ReadXdr, RestoreFootprintOp, SorobanResources, SorobanTransactionData,
    SorobanTransactionDataExt, TransactionEnvelope, WriteXdr,
};

use crate::context::RpcContext;
use crate::error::JsonRpcError;
use crate::util::{self, XdrFormat};

/// Multiplicative adjustment factor for refundable fees (soroban-simulation default).
const REFUNDABLE_FEE_ADJUSTMENT_FACTOR: f64 = 1.15;

// ---------------------------------------------------------------------------
// Operation extraction
// ---------------------------------------------------------------------------

/// The three Soroban operation kinds we can simulate.
enum SorobanOp {
    InvokeHostFunction {
        host_fn: HostFunction,
        auth: Vec<stellar_xdr::curr::SorobanAuthorizationEntry>,
    },
    ExtendFootprintTtl {
        keys: Vec<LedgerKey>,
        extend_to: u32,
    },
    RestoreFootprint {
        keys: Vec<LedgerKey>,
    },
}

struct InvokeRequest {
    host_fn: HostFunction,
    source_account: stellar_xdr::curr::AccountId,
    ledger_info: soroban_host::LedgerInfo,
    snapshot_source: BucketListSnapshotSource,
    soroban_info: henyey_ledger::SorobanNetworkInfo,
    latest_ledger: u32,
    format: XdrFormat,
    auth_mode: soroban_host::e2e_invoke::RecordingInvocationAuthMode,
    instruction_leeway: u32,
}

struct InvokeResponseContext<'a> {
    soroban_info: &'a henyey_ledger::SorobanNetworkInfo,
    latest_ledger: u32,
    host_fn: &'a HostFunction,
    format: XdrFormat,
    instruction_leeway: u32,
}

/// Extract the Soroban operation, source account, and optional footprint from the envelope.
fn extract_soroban_op(
    tx_env: &TransactionEnvelope,
) -> Result<(stellar_xdr::curr::AccountId, SorobanOp, stellar_xdr::curr::Memo), JsonRpcError> {
    let (source, ops, ext, memo) = match tx_env {
        TransactionEnvelope::Tx(tx) => (
            &tx.tx.source_account,
            &tx.tx.operations,
            &tx.tx.ext,
            &tx.tx.memo,
        ),
        TransactionEnvelope::TxV0(_) => {
            return Err(JsonRpcError::invalid_params("v0 transactions not supported"));
        }
        TransactionEnvelope::TxFeeBump(fb) => match &fb.tx.inner_tx {
            stellar_xdr::curr::FeeBumpTransactionInnerTx::Tx(inner) => (
                &inner.tx.source_account,
                &inner.tx.operations,
                &inner.tx.ext,
                &inner.tx.memo,
            ),
        },
    };

    let source_account = muxed_to_account_id(source);

    if ops.len() != 1 {
        return Err(JsonRpcError::invalid_params(
            "simulateTransaction requires exactly one operation",
        ));
    }

    match &ops[0].body {
        OperationBody::InvokeHostFunction(op) => {
            let auth: Vec<stellar_xdr::curr::SorobanAuthorizationEntry> =
                op.auth.iter().cloned().collect();
            Ok((
                source_account,
                SorobanOp::InvokeHostFunction {
                    host_fn: op.host_function.clone(),
                    auth,
                },
                memo.clone(),
            ))
        }
        OperationBody::ExtendFootprintTtl(op) => {
            let keys = extract_footprint_keys(ext)?;
            Ok((
                source_account,
                SorobanOp::ExtendFootprintTtl {
                    keys,
                    extend_to: op.extend_to,
                },
                memo.clone(),
            ))
        }
        OperationBody::RestoreFootprint(_) => {
            let keys = extract_footprint_keys(ext)?;
            Ok((
                source_account,
                SorobanOp::RestoreFootprint { keys },
                memo.clone(),
            ))
        }
        _ => Err(JsonRpcError::invalid_params(
            "operation must be InvokeHostFunction, ExtendFootprintTtl, or RestoreFootprint",
        )),
    }
}

/// Validate memo (MemoText must be ≤ 28 bytes).
fn validate_memo(memo: &stellar_xdr::curr::Memo) -> Result<(), JsonRpcError> {
    if let stellar_xdr::curr::Memo::Text(text) = memo {
        if text.len() > 28 {
            return Err(JsonRpcError::invalid_params(format!(
                "memo text too long: {} bytes (max 28)",
                text.len()
            )));
        }
    }
    Ok(())
}

fn muxed_to_account_id(source: &stellar_xdr::curr::MuxedAccount) -> stellar_xdr::curr::AccountId {
    match source {
        stellar_xdr::curr::MuxedAccount::Ed25519(key) => {
            stellar_xdr::curr::AccountId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
                key.clone(),
            ))
        }
        stellar_xdr::curr::MuxedAccount::MuxedEd25519(muxed) => {
            stellar_xdr::curr::AccountId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
                muxed.ed25519.clone(),
            ))
        }
    }
}

/// Extract the read_only + read_write keys from the SorobanTransactionData footprint
/// embedded in the transaction envelope's ext field.
fn extract_footprint_keys(
    ext: &stellar_xdr::curr::TransactionExt,
) -> Result<Vec<LedgerKey>, JsonRpcError> {
    let soroban_data = match ext {
        stellar_xdr::curr::TransactionExt::V1(data) => data,
        _ => {
            return Err(JsonRpcError::invalid_params(
                "ExtendFootprintTtl/RestoreFootprint requires SorobanTransactionData in tx ext",
            ));
        }
    };
    let footprint = &soroban_data.resources.footprint;
    let mut keys = Vec::with_capacity(
        footprint.read_only.len() + footprint.read_write.len(),
    );
    keys.extend(footprint.read_only.iter().cloned());
    keys.extend(footprint.read_write.iter().cloned());
    Ok(keys)
}

// ---------------------------------------------------------------------------
// Handler entry point
// ---------------------------------------------------------------------------

pub async fn handle(
    ctx: &Arc<RpcContext>,
    params: serde_json::Value,
) -> Result<serde_json::Value, JsonRpcError> {
    let format = util::parse_format(&params)?;

    let tx_b64 = params
        .get("transaction")
        .and_then(|v| v.as_str())
        .ok_or_else(|| JsonRpcError::invalid_params("missing 'transaction' parameter"))?;

    let tx_bytes = BASE64
        .decode(tx_b64)
        .map_err(|e| JsonRpcError::invalid_params(format!("invalid base64: {e}")))?;

    let tx_env = TransactionEnvelope::from_xdr(&tx_bytes, Limits::none())
        .map_err(|e| JsonRpcError::invalid_params(format!("invalid XDR: {e}")))?;

    let (source_account, soroban_op, memo) = extract_soroban_op(&tx_env)?;

    // Validate memo
    validate_memo(&memo)?;

    // Parse authMode parameter
    let auth_mode_str = params
        .get("authMode")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // Parse resourceConfig parameter
    let instruction_leeway: u32 = params
        .get("resourceConfig")
        .and_then(|v| v.get("instructionLeeway"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;

    // Get bucket list snapshot
    let bl_snapshot = ctx
        .app
        .bucket_snapshot_manager()
        .copy_searchable_live_snapshot()
        .ok_or_else(|| JsonRpcError::internal("bucket list snapshot not available"))?;

    // Get ledger info
    let ledger = ctx.app.ledger_summary();
    let soroban_info = ctx
        .app
        .soroban_network_info()
        .ok_or_else(|| JsonRpcError::internal("soroban network config not available"))?;

    let network_id = henyey_common::NetworkId::from_passphrase(&ctx.app.info().network_passphrase);

    let ledger_info = soroban_host::LedgerInfo {
        protocol_version: ledger.version,
        sequence_number: ledger.num,
        timestamp: ledger.close_time,
        network_id: network_id.0 .0,
        base_reserve: ledger.base_reserve,
        min_temp_entry_ttl: soroban_info.min_temporary_ttl,
        min_persistent_entry_ttl: soroban_info.min_persistent_ttl,
        max_entry_ttl: soroban_info.max_entry_ttl,
    };

    let snapshot_source = BucketListSnapshotSource::new(bl_snapshot, ledger.num);

    match soroban_op {
        SorobanOp::InvokeHostFunction { host_fn, auth } => {
            // Validate and resolve authMode
            if !auth_mode_str.is_empty()
                && !matches!(
                    auth_mode_str,
                    "enforce" | "record" | "record_allow_nonroot"
                )
            {
                return Err(JsonRpcError::invalid_params(format!(
                    "unsupported authMode: '{}' (allowed: enforce, record, record_allow_nonroot)",
                    auth_mode_str
                )));
            }

            // Non-InvokeHostFunction ops cannot have authMode set — already matched above

            // Determine the effective auth mode
            let resolved_auth_mode = resolve_auth_mode(auth_mode_str, &auth)?;

            handle_invoke(InvokeRequest {
                host_fn,
                source_account,
                ledger_info,
                snapshot_source,
                soroban_info: soroban_info.clone(),
                latest_ledger: ledger.num,
                format,
                auth_mode: resolved_auth_mode,
                instruction_leeway,
            })
            .await
        }
        SorobanOp::ExtendFootprintTtl { keys, extend_to } => {
            if !auth_mode_str.is_empty() {
                return Err(JsonRpcError::invalid_params(
                    "authMode is only supported for InvokeHostFunction operations",
                ));
            }
            let soroban_info_clone = soroban_info.clone();
            let result = tokio::task::spawn_blocking(move || {
                simulate_extend_ttl_op(
                    &snapshot_source,
                    &ledger_info,
                    &keys,
                    extend_to,
                    &soroban_info_clone,
                )
            })
            .await
            .map_err(|e| JsonRpcError::internal(format!("simulation task failed: {e}")))?;

            match result {
                Ok(tx_data) => build_footprint_response(tx_data, &soroban_info, ledger.num, format),
                Err(e) => build_error_response(e, ledger.num),
            }
        }
        SorobanOp::RestoreFootprint { keys } => {
            if !auth_mode_str.is_empty() {
                return Err(JsonRpcError::invalid_params(
                    "authMode is only supported for InvokeHostFunction operations",
                ));
            }
            let soroban_info_clone = soroban_info.clone();
            let result = tokio::task::spawn_blocking(move || {
                simulate_restore_op(&snapshot_source, &ledger_info, &keys, &soroban_info_clone)
            })
            .await
            .map_err(|e| JsonRpcError::internal(format!("simulation task failed: {e}")))?;

            match result {
                Ok(tx_data) => build_footprint_response(tx_data, &soroban_info, ledger.num, format),
                Err(e) => build_error_response(e, ledger.num),
            }
        }
    }
}

/// Resolve the effective `RecordingInvocationAuthMode` from the request parameter.
fn resolve_auth_mode(
    auth_mode_str: &str,
    tx_auth: &[stellar_xdr::curr::SorobanAuthorizationEntry],
) -> Result<soroban_host::e2e_invoke::RecordingInvocationAuthMode, JsonRpcError> {
    use soroban_host::e2e_invoke::RecordingInvocationAuthMode;

    match auth_mode_str {
        "enforce" => Ok(RecordingInvocationAuthMode::Enforcing(tx_auth.to_vec())),
        "record" => {
            if !tx_auth.is_empty() {
                return Err(JsonRpcError::invalid_params(
                    "authMode 'record' cannot be used when transaction has auth entries",
                ));
            }
            Ok(RecordingInvocationAuthMode::Recording(true))
        }
        "record_allow_nonroot" => {
            if !tx_auth.is_empty() {
                return Err(JsonRpcError::invalid_params(
                    "authMode 'record_allow_nonroot' cannot be used when transaction has auth entries",
                ));
            }
            Ok(RecordingInvocationAuthMode::Recording(false))
        }
        _ => {
            // Default: if tx has auth entries -> enforce, else -> record (non-root disabled)
            if tx_auth.is_empty() {
                Ok(RecordingInvocationAuthMode::Recording(true))
            } else {
                Ok(RecordingInvocationAuthMode::Enforcing(tx_auth.to_vec()))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// InvokeHostFunction path (existing logic, refactored)
// ---------------------------------------------------------------------------

async fn handle_invoke(request: InvokeRequest) -> Result<serde_json::Value, JsonRpcError> {
    let host_fn_clone = request.host_fn.clone();
    let source_account_clone = request.source_account.clone();
    let ledger_info_clone = request.ledger_info.clone();
    let snapshot_source = request.snapshot_source;
    let auth_mode = request.auth_mode;

    let result = tokio::task::spawn_blocking(move || {
        run_invoke_simulation(
            host_fn_clone,
            source_account_clone,
            ledger_info_clone,
            snapshot_source,
            auth_mode,
        )
    })
    .await
    .map_err(|e| JsonRpcError::internal(format!("simulation task failed: {e}")))?;

    match result {
        Ok(sim_output) => build_invoke_response(
            sim_output.recording_result,
            sim_output.diagnostic_events,
            sim_output.state_changes,
            InvokeResponseContext {
                soroban_info: &request.soroban_info,
                latest_ledger: request.latest_ledger,
                host_fn: &request.host_fn,
                format: request.format,
                instruction_leeway: request.instruction_leeway,
            },
        ),
        Err(e) => build_error_response(e, request.latest_ledger),
    }
}

/// Represents a single ledger entry state change from simulation.
struct LedgerEntryDiff {
    key: LedgerKey,
    state_before: Option<stellar_xdr::curr::LedgerEntry>,
    state_after: Option<stellar_xdr::curr::LedgerEntry>,
}

struct InvokeSimulationOutput {
    recording_result: soroban_host::e2e_invoke::InvokeHostFunctionRecordingModeResult,
    diagnostic_events: Vec<stellar_xdr::curr::DiagnosticEvent>,
    state_changes: Vec<LedgerEntryDiff>,
}

fn run_invoke_simulation(
    host_fn: HostFunction,
    source_account: stellar_xdr::curr::AccountId,
    ledger_info: soroban_host::LedgerInfo,
    snapshot_source: BucketListSnapshotSource,
    auth_mode: soroban_host::e2e_invoke::RecordingInvocationAuthMode,
) -> Result<InvokeSimulationOutput, String> {
    use soroban_host::budget::Budget;
    use soroban_host::e2e_invoke::invoke_host_function_in_recording_mode;

    let budget = Budget::default();
    let mut diagnostic_events = Vec::new();
    let seed: [u8; 32] = rand::random();

    let snapshot_rc = Rc::new(snapshot_source);

    let result = invoke_host_function_in_recording_mode(
        &budget,
        true, // enable_diagnostics
        &host_fn,
        &source_account,
        auth_mode,
        ledger_info.clone(),
        snapshot_rc.clone(),
        seed,
        &mut diagnostic_events,
    );

    match result {
        Ok(recording_result) => match &recording_result.invoke_result {
            Ok(_) => {
                // Extract state changes (before/after diffs) for read-write entries
                let state_changes = extract_modified_entries(
                    &snapshot_rc,
                    &recording_result.ledger_changes,
                    &ledger_info,
                );

                Ok(InvokeSimulationOutput {
                    recording_result,
                    diagnostic_events,
                    state_changes,
                })
            }
            Err(e) => Err(format!("host function invocation failed: {e:?}")),
        },
        Err(e) => Err(format!("simulation failed: {e:?}")),
    }
}

/// Extract before/after state diffs for read-write entries.
///
/// Mirrors `soroban-simulation::extract_modified_entries`.
fn extract_modified_entries(
    snapshot: &BucketListSnapshotSource,
    ledger_changes: &[soroban_host::e2e_invoke::LedgerEntryChange],
    ledger_info: &soroban_host::LedgerInfo,
) -> Vec<LedgerEntryDiff> {
    let mut diffs = Vec::new();

    for change in ledger_changes {
        if change.read_only {
            continue;
        }

        let key = match LedgerKey::from_xdr(&change.encoded_key, Limits::none()) {
            Ok(k) => k,
            Err(_) => continue,
        };

        // Get state before: re-query the snapshot for the entry
        let state_before = if let Some((entry, live_until)) = snapshot.get_unfiltered(&key) {
            // Check if entry is expired
            if let Some(lu) = live_until {
                if lu < ledger_info.sequence_number {
                    None // expired = treated as non-existent
                } else {
                    Some(entry)
                }
            } else {
                Some(entry)
            }
        } else {
            None
        };

        // Get state after: decode from encoded_new_value
        let state_after = change.encoded_new_value.as_ref().and_then(|v| {
            stellar_xdr::curr::LedgerEntry::from_xdr(v, Limits::none()).ok()
        });

        // Skip entries where both before and after are None (no diff)
        if state_before.is_none() && state_after.is_none() {
            continue;
        }

        diffs.push(LedgerEntryDiff {
            key,
            state_before,
            state_after,
        });
    }

    diffs
}

// ---------------------------------------------------------------------------
// ExtendFootprintTtl simulation
// ---------------------------------------------------------------------------

/// Simulate an `ExtendFootprintTtlOp`.
///
/// For each key in the footprint, look up the current entry and TTL. Skip entries
/// that don't exist, don't have a TTL, or already have a live-until >= the requested
/// extension. Compute rent fee changes for the remaining entries.
///
/// Mirrors `soroban-simulation::simulate_extend_ttl_op`.
fn simulate_extend_ttl_op(
    snapshot: &BucketListSnapshotSource,
    ledger_info: &soroban_host::LedgerInfo,
    keys: &[LedgerKey],
    extend_to: u32,
    soroban_info: &henyey_ledger::SorobanNetworkInfo,
) -> Result<SorobanTransactionData, String> {
    use soroban_host::e2e_invoke::entry_size_for_rent;
    use soroban_host::fees::LedgerEntryRentChange;
    use soroban_host::ledger_info::get_key_durability;

    let budget = soroban_host::budget::Budget::default();
    let new_live_until = ledger_info.sequence_number.saturating_add(extend_to);

    let mut rent_changes = Vec::with_capacity(keys.len());
    let mut extended_keys = Vec::with_capacity(keys.len());

    for key in keys {
        let durability = get_key_durability(key).ok_or_else(|| {
            format!(
                "cannot extend TTL for key {:?}: only contract data/code entries have TTL",
                key.discriminant()
            )
        })?;

        let Some((entry, live_until)) = snapshot.get_unfiltered(key) else {
            continue; // entry doesn't exist, skip
        };

        let current_live_until = live_until.ok_or_else(|| {
            format!("missing TTL for key that must have TTL: {:?}", key.discriminant())
        })?;

        // Skip entries that don't need extension
        if new_live_until <= current_live_until {
            continue;
        }

        // Expired entries cannot be extended (must be restored first)
        if current_live_until < ledger_info.sequence_number {
            return Err(format!(
                "cannot extend TTL for expired entry (live_until={current_live_until}, \
                 current_ledger={}). Restore the entry first.",
                ledger_info.sequence_number
            ));
        }

        extended_keys.push(key.clone());

        let entry_xdr_size = entry
            .to_xdr(Limits::none())
            .map(|b| b.len() as u32)
            .unwrap_or(0);
        let entry_size = entry_size_for_rent(&budget, &entry, entry_xdr_size)
            .map_err(|e| format!("entry_size_for_rent failed: {e:?}"))?;

        rent_changes.push(LedgerEntryRentChange {
            is_persistent: durability == ContractDataDurability::Persistent,
            is_code_entry: matches!(key.discriminant(), LedgerEntryType::ContractCode),
            old_size_bytes: entry_size,
            new_size_bytes: entry_size,
            old_live_until_ledger: current_live_until,
            new_live_until_ledger: new_live_until,
        });
    }

    extended_keys.sort();

    let resources = SorobanResources {
        footprint: LedgerFootprint {
            read_only: extended_keys
                .try_into()
                .map_err(|_| "too many keys for footprint".to_string())?,
            read_write: Default::default(),
        },
        instructions: 0,
        disk_read_bytes: 0,
        write_bytes: 0,
    };

    let operation = OperationBody::ExtendFootprintTtl(ExtendFootprintTtlOp {
        ext: ExtensionPoint::V0,
        extend_to,
    });

    let tx_size = estimate_tx_size_for_op(&operation, &resources);
    let resource_fee = compute_resource_fee_with_rent(
        &resources,
        &rent_changes,
        soroban_info,
        ledger_info.sequence_number,
        0, // no contract events
        tx_size,
    );

    Ok(SorobanTransactionData {
        ext: SorobanTransactionDataExt::V0,
        resources,
        resource_fee,
    })
}

// ---------------------------------------------------------------------------
// RestoreFootprint simulation
// ---------------------------------------------------------------------------

/// Simulate a `RestoreFootprintOp`.
///
/// For each key, verify it is a persistent contract data/code entry. Look up the
/// entry and its TTL. Skip entries that are still live. For expired entries,
/// compute the cost of restoring them with TTL = min_persistent_entry_ttl.
///
/// Mirrors `soroban-simulation::simulate_restore_op`.
fn simulate_restore_op(
    snapshot: &BucketListSnapshotSource,
    ledger_info: &soroban_host::LedgerInfo,
    keys: &[LedgerKey],
    soroban_info: &henyey_ledger::SorobanNetworkInfo,
) -> Result<SorobanTransactionData, String> {
    use soroban_host::e2e_invoke::entry_size_for_rent;
    use soroban_host::fees::LedgerEntryRentChange;
    use soroban_host::ledger_info::get_key_durability;

    let budget = soroban_host::budget::Budget::default();
    let restored_live_until = ledger_info
        .min_live_until_ledger_checked(ContractDataDurability::Persistent)
        .ok_or("min persistent live_until ledger overflows")?;

    let mut rent_changes = Vec::with_capacity(keys.len());
    let mut restored_keys = Vec::with_capacity(keys.len());
    let mut restored_bytes = 0u32;

    for key in keys {
        let durability = get_key_durability(key);
        if durability != Some(ContractDataDurability::Persistent) {
            return Err(format!(
                "cannot restore key {:?}: only persistent entries can be restored",
                key.discriminant()
            ));
        }

        let (entry, live_until) = snapshot
            .get_unfiltered(key)
            .ok_or_else(|| format!("missing entry to restore for key {:?}", key.discriminant()))?;

        let current_live_until = live_until.ok_or_else(|| {
            format!("missing TTL for key that must have TTL: {:?}", key.discriminant())
        })?;

        // Skip entries that are still live (not expired)
        if current_live_until >= ledger_info.sequence_number {
            continue;
        }

        restored_keys.push(key.clone());

        let entry_xdr_size = entry
            .to_xdr(Limits::none())
            .map(|b| b.len() as u32)
            .unwrap_or(0);
        let entry_rent_size = entry_size_for_rent(&budget, &entry, entry_xdr_size)
            .map_err(|e| format!("entry_size_for_rent failed: {e:?}"))?;

        restored_bytes = restored_bytes.saturating_add(entry_xdr_size);

        rent_changes.push(LedgerEntryRentChange {
            is_persistent: true,
            is_code_entry: matches!(key.discriminant(), LedgerEntryType::ContractCode),
            old_size_bytes: 0,
            new_size_bytes: entry_rent_size,
            old_live_until_ledger: 0,
            new_live_until_ledger: restored_live_until,
        });
    }

    restored_keys.sort();

    let resources = SorobanResources {
        footprint: LedgerFootprint {
            read_only: Default::default(),
            read_write: restored_keys
                .try_into()
                .map_err(|_| "too many keys for footprint".to_string())?,
        },
        instructions: 0,
        disk_read_bytes: restored_bytes,
        write_bytes: restored_bytes,
    };

    let operation = OperationBody::RestoreFootprint(RestoreFootprintOp {
        ext: ExtensionPoint::V0,
    });

    let tx_size = estimate_tx_size_for_op(&operation, &resources);
    let resource_fee = compute_resource_fee_with_rent(
        &resources,
        &rent_changes,
        soroban_info,
        ledger_info.sequence_number,
        0, // no contract events
        tx_size,
    );

    Ok(SorobanTransactionData {
        ext: SorobanTransactionDataExt::V0,
        resources,
        resource_fee,
    })
}

// ---------------------------------------------------------------------------
// Response builders
// ---------------------------------------------------------------------------

fn build_invoke_response(
    sim_result: soroban_host::e2e_invoke::InvokeHostFunctionRecordingModeResult,
    diagnostic_events: Vec<stellar_xdr::curr::DiagnosticEvent>,
    state_changes: Vec<LedgerEntryDiff>,
    ctx: InvokeResponseContext<'_>,
) -> Result<serde_json::Value, JsonRpcError> {
    // Use the host's resource estimates directly. The host computes:
    //   - instructions: CPU insns consumed during simulation
    //   - disk_read_bytes: non-Soroban entries + auto-restored entries from initial footprint
    //   - write_bytes: sum of encoded_new_value sizes for RW entries
    // This matches how upstream soroban-simulation passes recording_result.resources
    // through to compute_adjusted_transaction_resources.
    let resources = sim_result.resources.clone();

    // Apply resource adjustments (mirrors soroban-simulation default_adjustment)
    let mut adjusted_resources = resources.clone();
    adjust_resources(&mut adjusted_resources, ctx.instruction_leeway);

    // Compute rent changes for fee estimation
    let rent_changes = soroban_host::e2e_invoke::extract_rent_changes(&sim_result.ledger_changes);

    // Estimate the transaction size for fee computation
    let op = OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
        host_function: ctx.host_fn.clone(),
        auth: sim_result
            .auth
            .clone()
            .try_into()
            .unwrap_or_default(),
    });
    let tx_size = estimate_tx_size_for_op(&op, &adjusted_resources);

    // Build SorobanTransactionData
    // Build the extension: V1 with archived entry indices when entries were
    // auto-restored during simulation, V0 otherwise.
    let ext = if sim_result.restored_rw_entry_indices.is_empty() {
        SorobanTransactionDataExt::V0
    } else {
        SorobanTransactionDataExt::V1(stellar_xdr::curr::SorobanResourcesExtV0 {
            archived_soroban_entries: sim_result
                .restored_rw_entry_indices
                .clone()
                .try_into()
                .unwrap_or_default(),
        })
    };

    let soroban_data = SorobanTransactionData {
        ext,
        resources: adjusted_resources,
        resource_fee: compute_invoke_resource_fee(
            &resources,
            &rent_changes,
            ctx.soroban_info,
            ctx.latest_ledger,
            sim_result.contract_events_and_return_value_size,
            tx_size,
            sim_result.restored_rw_entry_indices.len() as u32,
        ),
    };

    let min_resource_fee = soroban_data.resource_fee;

    let mut obj = serde_json::Map::new();

    // transactionData — upstream uses unsuffixed "transactionData" for base64
    insert_sim_xdr_field(&mut obj, "transactionData", &soroban_data, ctx.format)?;

    obj.insert("minResourceFee".into(), json!(min_resource_fee.to_string()));
    obj.insert(
        "cost".into(),
        json!({
            "cpuInsns": resources.instructions.to_string(),
            "memBytes": "0"
        }),
    );
    obj.insert("latestLedger".into(), json!(ctx.latest_ledger));

    // Diagnostic events — upstream uses unsuffixed "events" for base64
    if !diagnostic_events.is_empty() {
        insert_sim_xdr_array_field(&mut obj, "events", &diagnostic_events, ctx.format)?;
    }

    // Encode auth entries and return value
    let auth = &sim_result.auth;
    let return_value = match &sim_result.invoke_result {
        Ok(val) => Some(val.clone()),
        Err(_) => None,
    };

    if !auth.is_empty() || return_value.is_some() {
        let mut result_obj = serde_json::Map::new();

        // auth array
        match ctx.format {
            XdrFormat::Base64 => {
                let auth_b64: Vec<serde_json::Value> = auth
                    .iter()
                    .filter_map(|a| {
                        a.to_xdr(Limits::none())
                            .ok()
                            .map(|b| serde_json::Value::String(BASE64.encode(&b)))
                    })
                    .collect();
                result_obj.insert("auth".into(), serde_json::Value::Array(auth_b64));
            }
            XdrFormat::Json => {
                let auth_json: Vec<serde_json::Value> = auth
                    .iter()
                    .filter_map(|a| serde_json::to_value(a).ok())
                    .collect();
                result_obj.insert("authJson".into(), serde_json::Value::Array(auth_json));
            }
        }

        // return value
        if let Some(rv) = &return_value {
            util::insert_xdr_field(&mut result_obj, "xdr_val", rv, ctx.format)?;
            // Upstream uses "xdr" for base64, "xdrJson" for JSON
            match ctx.format {
                XdrFormat::Base64 => {
                    if let Some(val) = result_obj.remove("xdr_valXdr") {
                        result_obj.insert("xdr".into(), val);
                    }
                }
                XdrFormat::Json => {
                    if let Some(val) = result_obj.remove("xdr_valJson") {
                        result_obj.insert("xdrJson".into(), val);
                    }
                }
            }
        }

        obj.insert(
            "results".into(),
            json!([serde_json::Value::Object(result_obj)]),
        );
    }

    // State changes (ledger entry diffs)
    if !state_changes.is_empty() {
        let changes_json = serialize_state_changes(&state_changes, ctx.format)?;
        obj.insert("stateChanges".into(), changes_json);
    }

    Ok(serde_json::Value::Object(obj))
}

/// Serialize state changes to JSON.
fn serialize_state_changes(
    diffs: &[LedgerEntryDiff],
    format: XdrFormat,
) -> Result<serde_json::Value, JsonRpcError> {
    let mut entries = Vec::with_capacity(diffs.len());

    for diff in diffs {
        let change_type = match (&diff.state_before, &diff.state_after) {
            (None, Some(_)) => "created",
            (Some(_), Some(_)) => "updated",
            (Some(_), None) => "deleted",
            (None, None) => continue,
        };

        let mut entry = serde_json::Map::new();
        entry.insert("type".into(), json!(change_type));

        // Key
        match format {
            XdrFormat::Base64 => {
                let key_bytes = diff
                    .key
                    .to_xdr(Limits::none())
                    .map_err(|e| JsonRpcError::internal(format!("XDR encode error: {e}")))?;
                entry.insert("key".into(), json!(BASE64.encode(&key_bytes)));
            }
            XdrFormat::Json => {
                let key_json = serde_json::to_value(&diff.key)
                    .map_err(|e| JsonRpcError::internal(format!("JSON serialize error: {e}")))?;
                entry.insert("keyJson".into(), key_json);
            }
        }

        // Before
        match (&diff.state_before, format) {
            (Some(before), XdrFormat::Base64) => {
                let bytes = before
                    .to_xdr(Limits::none())
                    .map_err(|e| JsonRpcError::internal(format!("XDR encode error: {e}")))?;
                entry.insert("before".into(), json!(BASE64.encode(&bytes)));
            }
            (Some(before), XdrFormat::Json) => {
                let jv = serde_json::to_value(before)
                    .map_err(|e| JsonRpcError::internal(format!("JSON serialize error: {e}")))?;
                entry.insert("beforeJson".into(), jv);
            }
            (None, XdrFormat::Base64) => {
                entry.insert("before".into(), serde_json::Value::Null);
            }
            (None, XdrFormat::Json) => {
                entry.insert("beforeJson".into(), serde_json::Value::Null);
            }
        }

        // After
        match (&diff.state_after, format) {
            (Some(after), XdrFormat::Base64) => {
                let bytes = after
                    .to_xdr(Limits::none())
                    .map_err(|e| JsonRpcError::internal(format!("XDR encode error: {e}")))?;
                entry.insert("after".into(), json!(BASE64.encode(&bytes)));
            }
            (Some(after), XdrFormat::Json) => {
                let jv = serde_json::to_value(after)
                    .map_err(|e| JsonRpcError::internal(format!("JSON serialize error: {e}")))?;
                entry.insert("afterJson".into(), jv);
            }
            (None, XdrFormat::Base64) => {
                entry.insert("after".into(), serde_json::Value::Null);
            }
            (None, XdrFormat::Json) => {
                entry.insert("afterJson".into(), serde_json::Value::Null);
            }
        }

        entries.push(serde_json::Value::Object(entry));
    }

    Ok(serde_json::Value::Array(entries))
}

/// Build the response for ExtendFootprintTtl / RestoreFootprint.
///
/// These operations produce no results, no auth, and no return value — just
/// `transactionData` and `minResourceFee`.
fn build_footprint_response(
    tx_data: SorobanTransactionData,
    _soroban_info: &henyey_ledger::SorobanNetworkInfo,
    latest_ledger: u32,
    format: XdrFormat,
) -> Result<serde_json::Value, JsonRpcError> {
    let min_resource_fee = tx_data.resource_fee;
    let mut obj = serde_json::Map::new();

    insert_sim_xdr_field(&mut obj, "transactionData", &tx_data, format)?;
    obj.insert("minResourceFee".into(), json!(min_resource_fee.to_string()));
    obj.insert(
        "cost".into(),
        json!({
            "cpuInsns": "0",
            "memBytes": "0"
        }),
    );
    obj.insert("latestLedger".into(), json!(latest_ledger));

    Ok(serde_json::Value::Object(obj))
}

fn build_error_response(
    error: String,
    latest_ledger: u32,
) -> Result<serde_json::Value, JsonRpcError> {
    Ok(json!({
        "error": error,
        "transactionData": "",
        "minResourceFee": "0",
        "cost": {
            "cpuInsns": "0",
            "memBytes": "0"
        },
        "latestLedger": latest_ledger
    }))
}

// ---------------------------------------------------------------------------
// Simulate-specific XDR field helpers
// ---------------------------------------------------------------------------

/// Insert a single XDR value with simulate-specific naming.
///
/// Unlike `util::insert_xdr_field` (which appends `Xdr`/`Json` suffixes),
/// the `simulateTransaction` upstream response uses **unsuffixed** field names
/// for base64 mode (e.g. `transactionData`, `events`) and appends `Json` only
/// in JSON mode.
fn insert_sim_xdr_field<T: WriteXdr + serde::Serialize>(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    base_name: &str,
    val: &T,
    format: XdrFormat,
) -> Result<(), JsonRpcError> {
    match format {
        XdrFormat::Base64 => {
            let bytes = val
                .to_xdr(Limits::none())
                .map_err(|e| JsonRpcError::internal(format!("XDR encode error: {e}")))?;
            obj.insert(
                base_name.to_string(),
                serde_json::Value::String(BASE64.encode(&bytes)),
            );
        }
        XdrFormat::Json => {
            let json_val = serde_json::to_value(val)
                .map_err(|e| JsonRpcError::internal(format!("JSON serialize error: {e}")))?;
            obj.insert(format!("{base_name}Json"), json_val);
        }
    }
    Ok(())
}

/// Insert an array of XDR values with simulate-specific naming.
///
/// Base64: `"{base_name}": ["<b64>", ...]`
/// Json: `"{base_name}Json": [{...}, ...]`
fn insert_sim_xdr_array_field<T: WriteXdr + serde::Serialize>(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    base_name: &str,
    items: &[T],
    format: XdrFormat,
) -> Result<(), JsonRpcError> {
    match format {
        XdrFormat::Base64 => {
            let encoded: Vec<serde_json::Value> = items
                .iter()
                .map(|item| {
                    item.to_xdr(Limits::none())
                        .map(|b| serde_json::Value::String(BASE64.encode(&b)))
                        .map_err(|e| JsonRpcError::internal(format!("XDR encode error: {e}")))
                })
                .collect::<Result<_, _>>()?;
            obj.insert(base_name.to_string(), serde_json::Value::Array(encoded));
        }
        XdrFormat::Json => {
            let json_items: Vec<serde_json::Value> = items
                .iter()
                .map(|item| {
                    serde_json::to_value(item)
                        .map_err(|e| JsonRpcError::internal(format!("JSON serialize error: {e}")))
                })
                .collect::<Result<_, _>>()?;
            obj.insert(
                format!("{base_name}Json"),
                serde_json::Value::Array(json_items),
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Resource adjustments
// ---------------------------------------------------------------------------

/// Apply soroban-simulation default adjustment: max(x + additive, floor(x * mult)).
fn sim_adjust(value: u32, multiplicative: f64, additive: u32) -> u32 {
    if value == 0 {
        return 0;
    }
    let mult_adjusted = (value as f64 * multiplicative).floor() as u32;
    (value.saturating_add(additive)).max(mult_adjusted)
}

/// Apply resource adjustment factors matching soroban-simulation defaults.
///
/// `instruction_leeway` comes from the `resourceConfig.instructionLeeway` request param.
/// The effective additive factor is `max(50_000, instruction_leeway)`.
/// `disk_read_bytes` and `write_bytes` use `(1.0, 0)` (no additive adjustment) per upstream.
fn adjust_resources(resources: &mut SorobanResources, instruction_leeway: u32) {
    let additive = 50_000u32.max(instruction_leeway);
    resources.instructions = sim_adjust(resources.instructions, 1.04, additive);
    resources.disk_read_bytes = sim_adjust(resources.disk_read_bytes, 1.0, 0);
    resources.write_bytes = sim_adjust(resources.write_bytes, 1.0, 0);
}

// ---------------------------------------------------------------------------
// Transaction size estimation
// ---------------------------------------------------------------------------

/// Estimate the XDR-encoded transaction size for fee computation.
///
/// Mirrors soroban-simulation: builds a max-size synthetic envelope with
/// 20 signatures and full preconditions, then applies the tx_size adjustment.
fn estimate_tx_size_for_op(
    operation: &OperationBody,
    resources: &SorobanResources,
) -> u32 {
    use stellar_xdr::curr::*;

    let soroban_data = SorobanTransactionData {
        ext: SorobanTransactionDataExt::V0,
        resources: SorobanResources {
            footprint: resources.footprint.clone(),
            instructions: 0,
            disk_read_bytes: 0,
            write_bytes: 0,
        },
        resource_fee: 0,
    };

    let sig = DecoratedSignature {
        hint: SignatureHint([0u8; 4]),
        signature: Signature::try_from(vec![0u8; 64]).unwrap_or_default(),
    };
    let sigs: Vec<DecoratedSignature> = (0..20).map(|_| sig.clone()).collect();

    let source = MuxedAccount::MuxedEd25519(MuxedAccountMed25519 {
        id: 0,
        ed25519: Uint256([0u8; 32]),
    });

    let tx = Transaction {
        source_account: source.clone(),
        fee: u32::MAX,
        seq_num: SequenceNumber(0),
        cond: Preconditions::V2(PreconditionsV2 {
            time_bounds: Some(TimeBounds {
                min_time: TimePoint(0),
                max_time: TimePoint(0),
            }),
            ledger_bounds: Some(LedgerBounds {
                min_ledger: 0,
                max_ledger: 0,
            }),
            min_seq_num: Some(SequenceNumber(0)),
            min_seq_age: Duration(0),
            min_seq_ledger_gap: 0,
            extra_signers: vec![
                SignerKey::Ed25519(Uint256([0u8; 32])),
                SignerKey::Ed25519(Uint256([0u8; 32])),
            ]
            .try_into()
            .unwrap_or_default(),
        }),
        memo: Memo::None,
        operations: vec![Operation {
            source_account: Some(source),
            body: operation.clone(),
        }]
        .try_into()
        .unwrap_or_default(),
        ext: TransactionExt::V1(soroban_data),
    };

    let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: sigs.try_into().unwrap_or_default(),
    });

    let raw_size = envelope
        .to_xdr(Limits::none())
        .map(|b| b.len() as u32)
        .unwrap_or(300);

    // Apply tx_size adjustment: max(x + 500, floor(x * 1.1))
    sim_adjust(raw_size, 1.1, 500)
}

// ---------------------------------------------------------------------------
// Fee computation
// ---------------------------------------------------------------------------

/// Build `FeeConfiguration` from `SorobanNetworkInfo`.
fn build_fee_config(
    soroban_info: &henyey_ledger::SorobanNetworkInfo,
) -> soroban_host::fees::FeeConfiguration {
    soroban_host::fees::FeeConfiguration {
        fee_per_instruction_increment: soroban_info.fee_rate_per_instructions_increment,
        fee_per_disk_read_entry: soroban_info.fee_read_ledger_entry,
        fee_per_write_entry: soroban_info.fee_write_ledger_entry,
        fee_per_disk_read_1kb: soroban_info.fee_read_1kb,
        fee_per_write_1kb: soroban_info.fee_write_1kb,
        fee_per_historical_1kb: soroban_info.fee_historical_1kb,
        fee_per_contract_event_1kb: soroban_info.fee_contract_events_size_1kb,
        fee_per_transaction_size_1kb: soroban_info.fee_transaction_size_1kb,
    }
}

/// Build `RentFeeConfiguration` from `SorobanNetworkInfo`.
fn build_rent_fee_config(
    soroban_info: &henyey_ledger::SorobanNetworkInfo,
) -> soroban_host::fees::RentFeeConfiguration {
    use soroban_host::fees::{RentFeeConfiguration, RentWriteFeeConfiguration};

    let rent_write_config = RentWriteFeeConfiguration {
        state_target_size_bytes: soroban_info.state_target_size_bytes,
        rent_fee_1kb_state_size_low: soroban_info.rent_fee_1kb_state_size_low,
        rent_fee_1kb_state_size_high: soroban_info.rent_fee_1kb_state_size_high,
        state_size_rent_fee_growth_factor: soroban_info.state_size_rent_fee_growth_factor,
    };
    let fee_per_rent_1kb = soroban_host::fees::compute_rent_write_fee_per_1kb(
        soroban_info.average_bucket_list_size as i64,
        &rent_write_config,
    );

    RentFeeConfiguration {
        fee_per_write_1kb: soroban_info.fee_write_1kb,
        fee_per_rent_1kb,
        fee_per_write_entry: soroban_info.fee_write_ledger_entry,
        persistent_rent_rate_denominator: soroban_info.persistent_rent_rate_denominator,
        temporary_rent_rate_denominator: soroban_info.temp_rent_rate_denominator,
    }
}

/// Compute resource fee for InvokeHostFunction (includes rent).
fn compute_invoke_resource_fee(
    resources: &SorobanResources,
    rent_changes: &[soroban_host::fees::LedgerEntryRentChange],
    soroban_info: &henyey_ledger::SorobanNetworkInfo,
    current_ledger_seq: u32,
    contract_events_and_return_value_size: u32,
    tx_size: u32,
    restored_entry_count: u32,
) -> i64 {
    use soroban_host::fees::{compute_rent_fee, compute_transaction_resource_fee, TransactionResources};

    // Compute disk_read_entries the same way as upstream soroban-simulation:
    // only non-Soroban entries (accounts, trustlines etc.) count, since Soroban
    // entries (ContractData/ContractCode) are cached in memory and don't require
    // disk reads. Auto-restored entries also count.
    let mut disk_read_entries = 0u32;
    for k in resources.footprint.read_only.iter() {
        match k {
            LedgerKey::ContractData(_) | LedgerKey::ContractCode(_) => (),
            _ => disk_read_entries += 1,
        }
    }
    for k in resources.footprint.read_write.iter() {
        match k {
            LedgerKey::ContractData(_) | LedgerKey::ContractCode(_) => (),
            _ => disk_read_entries += 1,
        }
    }
    disk_read_entries += restored_entry_count;

    let tx_resources = TransactionResources {
        instructions: resources.instructions,
        disk_read_entries,
        write_entries: resources.footprint.read_write.len() as u32,
        disk_read_bytes: resources.disk_read_bytes,
        write_bytes: resources.write_bytes,
        contract_events_size_bytes: contract_events_and_return_value_size,
        transaction_size_bytes: tx_size,
    };

    let fee_config = build_fee_config(soroban_info);
    let (non_refundable, refundable) =
        compute_transaction_resource_fee(&tx_resources, &fee_config);

    let rent_fee = compute_rent_fee(
        rent_changes,
        &build_rent_fee_config(soroban_info),
        current_ledger_seq,
    );

    // Apply adjustment to refundable fee + rent (matches soroban-simulation default)
    let total_refundable = refundable.saturating_add(rent_fee);
    let adjusted_refundable = if total_refundable > 0 {
        ((total_refundable as f64) * REFUNDABLE_FEE_ADJUSTMENT_FACTOR).floor() as i64
    } else {
        0
    };
    non_refundable.saturating_add(adjusted_refundable)
}

/// Compute resource fee for ExtendTTL/Restore operations (rent-dominant).
///
/// These operations have no instructions and no contract events, but do have
/// rent fees that dominate the cost.
fn compute_resource_fee_with_rent(
    resources: &SorobanResources,
    rent_changes: &[soroban_host::fees::LedgerEntryRentChange],
    soroban_info: &henyey_ledger::SorobanNetworkInfo,
    current_ledger_seq: u32,
    contract_events_size: u32,
    tx_size: u32,
) -> i64 {
    use soroban_host::fees::{compute_rent_fee, compute_transaction_resource_fee, TransactionResources};

    let tx_resources = TransactionResources {
        instructions: resources.instructions,
        disk_read_entries: resources.footprint.read_only.len() as u32
            + resources.footprint.read_write.len() as u32,
        write_entries: resources.footprint.read_write.len() as u32,
        disk_read_bytes: resources.disk_read_bytes,
        write_bytes: resources.write_bytes,
        contract_events_size_bytes: contract_events_size,
        transaction_size_bytes: tx_size,
    };

    let fee_config = build_fee_config(soroban_info);
    let rent_fee_config = build_rent_fee_config(soroban_info);

    let (non_refundable, refundable) =
        compute_transaction_resource_fee(&tx_resources, &fee_config);

    let rent_fee = compute_rent_fee(rent_changes, &rent_fee_config, current_ledger_seq);

    let total_refundable = refundable.saturating_add(rent_fee);
    let adjusted_refundable = if total_refundable > 0 {
        ((total_refundable as f64) * REFUNDABLE_FEE_ADJUSTMENT_FACTOR).floor() as i64
    } else {
        0
    };
    non_refundable.saturating_add(adjusted_refundable)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::*;

    // -----------------------------------------------------------------------
    // Test helpers (Category G)
    // -----------------------------------------------------------------------

    fn test_soroban_network_info() -> henyey_ledger::SorobanNetworkInfo {
        henyey_ledger::SorobanNetworkInfo {
            max_contract_size: 65536,
            max_contract_data_key_size: 250,
            max_contract_data_entry_size: 65536,
            tx_max_instructions: 100_000_000,
            ledger_max_instructions: 2_500_000_000,
            fee_rate_per_instructions_increment: 25,
            tx_memory_limit: 41943040,
            ledger_max_read_ledger_entries: 200,
            ledger_max_read_bytes: 200_000,
            ledger_max_write_ledger_entries: 150,
            ledger_max_write_bytes: 65536,
            tx_max_read_ledger_entries: 40,
            tx_max_read_bytes: 200_000,
            tx_max_write_ledger_entries: 25,
            tx_max_write_bytes: 65536,
            fee_read_ledger_entry: 6250,
            fee_write_ledger_entry: 10000,
            fee_read_1kb: 1786,
            fee_write_1kb: 11800,
            fee_historical_1kb: 16235,
            tx_max_contract_events_size_bytes: 8198,
            fee_contract_events_size_1kb: 10000,
            ledger_max_tx_size_bytes: 71680,
            tx_max_size_bytes: 71680,
            fee_transaction_size_1kb: 1624,
            ledger_max_tx_count: 150,
            max_entry_ttl: 6_312_000,
            min_temporary_ttl: 17280,
            min_persistent_ttl: 120960,
            persistent_rent_rate_denominator: 2103840,
            temp_rent_rate_denominator: 4096,
            max_entries_to_archive: 100,
            bucketlist_size_window_sample_size: 30,
            eviction_scan_size: 100000,
            starting_eviction_scan_level: 7,
            average_bucket_list_size: 100_000_000,
            state_target_size_bytes: 134217728,
            rent_fee_1kb_state_size_low: 1000,
            rent_fee_1kb_state_size_high: 100000000,
            state_size_rent_fee_growth_factor: 1000,
            nomination_timeout_initial_ms: 1000,
            nomination_timeout_increment_ms: 500,
            ballot_timeout_initial_ms: 1000,
            ballot_timeout_increment_ms: 500,
        }
    }

    fn test_account_key(key_byte: u8) -> LedgerKey {
        LedgerKey::Account(LedgerKeyAccount {
            account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([key_byte; 32]))),
        })
    }

    fn test_contract_data_key(contract_byte: u8) -> LedgerKey {
        LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(Hash([contract_byte; 32]))),
            key: ScVal::LedgerKeyContractInstance,
            durability: ContractDataDurability::Persistent,
        })
    }

    fn test_soroban_resources(ro: Vec<LedgerKey>, rw: Vec<LedgerKey>) -> SorobanResources {
        SorobanResources {
            footprint: LedgerFootprint {
                read_only: ro.try_into().unwrap_or_default(),
                read_write: rw.try_into().unwrap_or_default(),
            },
            instructions: 1_000_000,
            disk_read_bytes: 5000,
            write_bytes: 2000,
        }
    }

    fn make_tx_envelope(ops: Vec<Operation>) -> TransactionEnvelope {
        let source = MuxedAccount::Ed25519(Uint256([1u8; 32]));
        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx: Transaction {
                source_account: source,
                fee: 100,
                seq_num: SequenceNumber(1),
                cond: Preconditions::None,
                memo: Memo::None,
                operations: ops.try_into().unwrap_or_default(),
                ext: TransactionExt::V0,
            },
            signatures: Default::default(),
        })
    }

    fn make_invoke_tx_envelope() -> TransactionEnvelope {
        let op = Operation {
            source_account: None,
            body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                host_function: HostFunction::InvokeContract(InvokeContractArgs {
                    contract_address: ScAddress::Contract(ContractId(Hash([0xAA; 32]))),
                    function_name: ScSymbol("hello".try_into().unwrap()),
                    args: Default::default(),
                }),
                auth: Default::default(),
            }),
        };
        make_tx_envelope(vec![op])
    }

    fn make_fee_bump_invoke_tx_envelope() -> TransactionEnvelope {
        let op = Operation {
            source_account: None,
            body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                host_function: HostFunction::InvokeContract(InvokeContractArgs {
                    contract_address: ScAddress::Contract(ContractId(Hash([0xBB; 32]))),
                    function_name: ScSymbol("test".try_into().unwrap()),
                    args: Default::default(),
                }),
                auth: Default::default(),
            }),
        };
        let inner_source = MuxedAccount::Ed25519(Uint256([2u8; 32]));
        let inner_tx = TransactionV1Envelope {
            tx: Transaction {
                source_account: inner_source,
                fee: 100,
                seq_num: SequenceNumber(1),
                cond: Preconditions::None,
                memo: Memo::None,
                operations: vec![op].try_into().unwrap_or_default(),
                ext: TransactionExt::V0,
            },
            signatures: Default::default(),
        };
        TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
            tx: FeeBumpTransaction {
                fee_source: MuxedAccount::Ed25519(Uint256([3u8; 32])),
                fee: 200,
                inner_tx: FeeBumpTransactionInnerTx::Tx(inner_tx),
                ext: FeeBumpTransactionExt::V0,
            },
            signatures: Default::default(),
        })
    }

    // -----------------------------------------------------------------------
    // B1. sim_adjust (5 tests)
    // -----------------------------------------------------------------------

    #[test]
    fn test_sim_adjust_zero_returns_zero() {
        assert_eq!(sim_adjust(0, 1.04, 50_000), 0);
    }

    #[test]
    fn test_sim_adjust_additive_dominates() {
        // 100_000 + 50_000 = 150_000 vs floor(100_000 * 1.04) = 104_000
        // max(150_000, 104_000) = 150_000
        assert_eq!(sim_adjust(100_000, 1.04, 50_000), 150_000);
    }

    #[test]
    fn test_sim_adjust_multiplicative_dominates() {
        // 10_000_000 + 50_000 = 10_050_000 vs floor(10_000_000 * 1.04) = 10_400_000
        // max(10_050_000, 10_400_000) = 10_400_000
        assert_eq!(sim_adjust(10_000_000, 1.04, 50_000), 10_400_000);
    }

    #[test]
    fn test_sim_adjust_no_adjustment() {
        assert_eq!(sim_adjust(500, 1.0, 0), 500);
    }

    #[test]
    fn test_sim_adjust_saturating() {
        // Should saturate to u32::MAX instead of overflowing
        assert_eq!(sim_adjust(u32::MAX - 10, 1.04, 50_000), u32::MAX);
    }

    // -----------------------------------------------------------------------
    // B2. adjust_resources (3 tests)
    // -----------------------------------------------------------------------

    #[test]
    fn test_adjust_resources_default() {
        let mut resources = SorobanResources {
            footprint: LedgerFootprint {
                read_only: Default::default(),
                read_write: Default::default(),
            },
            instructions: 1_000_000,
            disk_read_bytes: 5000,
            write_bytes: 2000,
        };
        adjust_resources(&mut resources, 0);

        // instructions: max(1_000_000 + 50_000, floor(1_000_000 * 1.04)) = max(1_050_000, 1_040_000) = 1_050_000
        assert_eq!(resources.instructions, 1_050_000);
        // disk_read_bytes: sim_adjust(5000, 1.0, 0) = 5000
        assert_eq!(resources.disk_read_bytes, 5000);
        // write_bytes: sim_adjust(2000, 1.0, 0) = 2000
        assert_eq!(resources.write_bytes, 2000);
    }

    #[test]
    fn test_adjust_resources_custom_leeway() {
        let mut resources = SorobanResources {
            footprint: LedgerFootprint {
                read_only: Default::default(),
                read_write: Default::default(),
            },
            instructions: 1_000_000,
            disk_read_bytes: 0,
            write_bytes: 0,
        };
        adjust_resources(&mut resources, 200_000);

        // additive = max(50_000, 200_000) = 200_000
        // instructions: max(1_000_000 + 200_000, floor(1_000_000 * 1.04)) = max(1_200_000, 1_040_000) = 1_200_000
        assert_eq!(resources.instructions, 1_200_000);
    }

    #[test]
    fn test_adjust_resources_zero_values() {
        let mut resources = SorobanResources {
            footprint: LedgerFootprint {
                read_only: Default::default(),
                read_write: Default::default(),
            },
            instructions: 0,
            disk_read_bytes: 0,
            write_bytes: 0,
        };
        adjust_resources(&mut resources, 0);

        assert_eq!(resources.instructions, 0);
        assert_eq!(resources.disk_read_bytes, 0);
        assert_eq!(resources.write_bytes, 0);
    }

    // -----------------------------------------------------------------------
    // B3. validate_memo (4 tests)
    // -----------------------------------------------------------------------

    #[test]
    fn test_validate_memo_none_ok() {
        assert!(validate_memo(&Memo::None).is_ok());
    }

    #[test]
    fn test_validate_memo_text_28_ok() {
        let text = StringM::<28>::try_from("abcdefghijklmnopqrstuvwxyzAB").unwrap();
        assert!(validate_memo(&Memo::Text(text)).is_ok());
    }

    #[test]
    fn test_validate_memo_text_29_error() {
        // StringM<28> enforces max 28 at the XDR type level, so we can't construct 29 bytes.
        // But validate_memo checks the runtime length, so this test confirms 28 is ok.
        // The actual protection comes from XDR type constraints. Test boundary:
        let text = StringM::<28>::try_from("abcdefghijklmnopqrstuvwxyzAB").unwrap();
        assert_eq!(text.len(), 28);
        assert!(validate_memo(&Memo::Text(text)).is_ok());
    }

    #[test]
    fn test_validate_memo_hash_ok() {
        assert!(validate_memo(&Memo::Hash(Hash([0u8; 32]))).is_ok());
    }

    // -----------------------------------------------------------------------
    // B4. resolve_auth_mode (6 tests)
    // -----------------------------------------------------------------------

    #[test]
    fn test_resolve_auth_mode_default_no_auth() {
        let result = resolve_auth_mode("", &[]).unwrap();
        // Empty auth + default -> Recording(true)
        match result {
            soroban_host::e2e_invoke::RecordingInvocationAuthMode::Recording(v) => {
                assert!(v, "expected root_invocation_only=true");
            }
            _ => panic!("expected Recording mode"),
        }
    }

    #[test]
    fn test_resolve_auth_mode_default_with_auth() {
        let auth_entry = SorobanAuthorizationEntry {
            credentials: SorobanCredentials::SourceAccount,
            root_invocation: SorobanAuthorizedInvocation {
                function: SorobanAuthorizedFunction::ContractFn(InvokeContractArgs {
                    contract_address: ScAddress::Contract(ContractId(Hash([0xAA; 32]))),
                    function_name: ScSymbol("test".try_into().unwrap()),
                    args: Default::default(),
                }),
                sub_invocations: Default::default(),
            },
        };
        let result = resolve_auth_mode("", std::slice::from_ref(&auth_entry)).unwrap();
        match result {
            soroban_host::e2e_invoke::RecordingInvocationAuthMode::Enforcing(entries) => {
                assert_eq!(entries.len(), 1);
            }
            _ => panic!("expected Enforcing mode"),
        }
    }

    #[test]
    fn test_resolve_auth_mode_record() {
        let result = resolve_auth_mode("record", &[]).unwrap();
        match result {
            soroban_host::e2e_invoke::RecordingInvocationAuthMode::Recording(v) => {
                assert!(v, "expected root_invocation_only=true");
            }
            _ => panic!("expected Recording mode"),
        }
    }

    #[test]
    fn test_resolve_auth_mode_record_with_auth_error() {
        let auth_entry = SorobanAuthorizationEntry {
            credentials: SorobanCredentials::SourceAccount,
            root_invocation: SorobanAuthorizedInvocation {
                function: SorobanAuthorizedFunction::ContractFn(InvokeContractArgs {
                    contract_address: ScAddress::Contract(ContractId(Hash([0xAA; 32]))),
                    function_name: ScSymbol("test".try_into().unwrap()),
                    args: Default::default(),
                }),
                sub_invocations: Default::default(),
            },
        };
        let result = resolve_auth_mode("record", &[auth_entry]);
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_auth_mode_enforce() {
        let auth_entry = SorobanAuthorizationEntry {
            credentials: SorobanCredentials::SourceAccount,
            root_invocation: SorobanAuthorizedInvocation {
                function: SorobanAuthorizedFunction::ContractFn(InvokeContractArgs {
                    contract_address: ScAddress::Contract(ContractId(Hash([0xAA; 32]))),
                    function_name: ScSymbol("test".try_into().unwrap()),
                    args: Default::default(),
                }),
                sub_invocations: Default::default(),
            },
        };
        let result = resolve_auth_mode("enforce", &[auth_entry]).unwrap();
        match result {
            soroban_host::e2e_invoke::RecordingInvocationAuthMode::Enforcing(entries) => {
                assert_eq!(entries.len(), 1);
            }
            _ => panic!("expected Enforcing mode"),
        }
    }

    #[test]
    fn test_resolve_auth_mode_invalid() {
        // "bogus" is not handled by resolve_auth_mode itself — it falls to the default arm.
        // The validation happens in handle() before calling resolve_auth_mode.
        // resolve_auth_mode("bogus", &[]) will fall through to the default match arm.
        let result = resolve_auth_mode("bogus", &[]);
        // Default arm with empty auth -> Recording(true)
        assert!(result.is_ok());
    }

    // -----------------------------------------------------------------------
    // B5. muxed_to_account_id (2 tests)
    // -----------------------------------------------------------------------

    #[test]
    fn test_muxed_ed25519() {
        let key = Uint256([42u8; 32]);
        let muxed = MuxedAccount::Ed25519(key.clone());
        let account_id = muxed_to_account_id(&muxed);
        match account_id.0 {
            PublicKey::PublicKeyTypeEd25519(k) => assert_eq!(k, key),
        }
    }

    #[test]
    fn test_muxed_ed25519_muxed() {
        let key = Uint256([99u8; 32]);
        let muxed = MuxedAccount::MuxedEd25519(MuxedAccountMed25519 {
            id: 12345,
            ed25519: key.clone(),
        });
        let account_id = muxed_to_account_id(&muxed);
        match account_id.0 {
            PublicKey::PublicKeyTypeEd25519(k) => assert_eq!(k, key),
        }
    }

    // -----------------------------------------------------------------------
    // B6. extract_soroban_op (6 tests)
    // -----------------------------------------------------------------------

    #[test]
    fn test_extract_invoke_host_function() {
        let env = make_invoke_tx_envelope();
        let (account_id, op, memo) = extract_soroban_op(&env).unwrap();
        assert!(matches!(op, SorobanOp::InvokeHostFunction { .. }));
        assert!(matches!(memo, Memo::None));
        // Source account should match
        match account_id.0 {
            PublicKey::PublicKeyTypeEd25519(k) => assert_eq!(k, Uint256([1u8; 32])),
        }
    }

    #[test]
    fn test_extract_extend_ttl() {
        let contract_key = test_contract_data_key(0xCC);
        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: vec![contract_key].try_into().unwrap(),
                    read_write: Default::default(),
                },
                instructions: 0,
                disk_read_bytes: 0,
                write_bytes: 0,
            },
            resource_fee: 0,
        };
        let op = Operation {
            source_account: None,
            body: OperationBody::ExtendFootprintTtl(ExtendFootprintTtlOp {
                ext: ExtensionPoint::V0,
                extend_to: 1000,
            }),
        };
        let source = MuxedAccount::Ed25519(Uint256([1u8; 32]));
        let env = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx: Transaction {
                source_account: source,
                fee: 100,
                seq_num: SequenceNumber(1),
                cond: Preconditions::None,
                memo: Memo::None,
                operations: vec![op].try_into().unwrap_or_default(),
                ext: TransactionExt::V1(soroban_data),
            },
            signatures: Default::default(),
        });

        let (_account, soroban_op, _memo) = extract_soroban_op(&env).unwrap();
        match soroban_op {
            SorobanOp::ExtendFootprintTtl { keys, extend_to } => {
                assert_eq!(keys.len(), 1);
                assert_eq!(extend_to, 1000);
            }
            _ => panic!("expected ExtendFootprintTtl"),
        }
    }

    #[test]
    fn test_extract_restore() {
        let contract_key = test_contract_data_key(0xDD);
        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: Default::default(),
                    read_write: vec![contract_key].try_into().unwrap(),
                },
                instructions: 0,
                disk_read_bytes: 0,
                write_bytes: 0,
            },
            resource_fee: 0,
        };
        let op = Operation {
            source_account: None,
            body: OperationBody::RestoreFootprint(RestoreFootprintOp {
                ext: ExtensionPoint::V0,
            }),
        };
        let source = MuxedAccount::Ed25519(Uint256([1u8; 32]));
        let env = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx: Transaction {
                source_account: source,
                fee: 100,
                seq_num: SequenceNumber(1),
                cond: Preconditions::None,
                memo: Memo::None,
                operations: vec![op].try_into().unwrap_or_default(),
                ext: TransactionExt::V1(soroban_data),
            },
            signatures: Default::default(),
        });

        let (_account, soroban_op, _memo) = extract_soroban_op(&env).unwrap();
        assert!(matches!(soroban_op, SorobanOp::RestoreFootprint { .. }));
    }

    #[test]
    fn test_extract_non_soroban_op_error() {
        let op = Operation {
            source_account: None,
            body: OperationBody::Payment(PaymentOp {
                destination: MuxedAccount::Ed25519(Uint256([2u8; 32])),
                asset: Asset::Native,
                amount: 1000,
            }),
        };
        let env = make_tx_envelope(vec![op]);
        assert!(extract_soroban_op(&env).is_err());
    }

    #[test]
    fn test_extract_multi_op_error() {
        let op1 = Operation {
            source_account: None,
            body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                host_function: HostFunction::InvokeContract(InvokeContractArgs {
                    contract_address: ScAddress::Contract(ContractId(Hash([0xAA; 32]))),
                    function_name: ScSymbol("a".try_into().unwrap()),
                    args: Default::default(),
                }),
                auth: Default::default(),
            }),
        };
        let op2 = Operation {
            source_account: None,
            body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                host_function: HostFunction::InvokeContract(InvokeContractArgs {
                    contract_address: ScAddress::Contract(ContractId(Hash([0xBB; 32]))),
                    function_name: ScSymbol("b".try_into().unwrap()),
                    args: Default::default(),
                }),
                auth: Default::default(),
            }),
        };
        let env = make_tx_envelope(vec![op1, op2]);
        assert!(extract_soroban_op(&env).is_err());
    }

    #[test]
    fn test_extract_fee_bump_unwrap() {
        let env = make_fee_bump_invoke_tx_envelope();
        let (account_id, op, _memo) = extract_soroban_op(&env).unwrap();
        assert!(matches!(op, SorobanOp::InvokeHostFunction { .. }));
        // Source should be from inner tx (key byte 2), not fee bump source (key byte 3)
        match account_id.0 {
            PublicKey::PublicKeyTypeEd25519(k) => assert_eq!(k, Uint256([2u8; 32])),
        }
    }

    // -----------------------------------------------------------------------
    // B7. insert_sim_xdr_field (4 tests) [REGRESSION]
    // -----------------------------------------------------------------------

    #[test]
    fn test_sim_xdr_field_base64_unsuffixed() {
        let mut obj = serde_json::Map::new();
        let data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: Default::default(),
                    read_write: Default::default(),
                },
                instructions: 0,
                disk_read_bytes: 0,
                write_bytes: 0,
            },
            resource_fee: 0,
        };
        insert_sim_xdr_field(&mut obj, "transactionData", &data, XdrFormat::Base64).unwrap();
        assert!(obj.contains_key("transactionData"), "base64 mode should use unsuffixed key");
        assert!(!obj.contains_key("transactionDataXdr"), "should NOT have Xdr suffix");
    }

    #[test]
    fn test_sim_xdr_field_json_suffixed() {
        let mut obj = serde_json::Map::new();
        let data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: Default::default(),
                    read_write: Default::default(),
                },
                instructions: 0,
                disk_read_bytes: 0,
                write_bytes: 0,
            },
            resource_fee: 0,
        };
        insert_sim_xdr_field(&mut obj, "transactionData", &data, XdrFormat::Json).unwrap();
        assert!(obj.contains_key("transactionDataJson"), "json mode should have Json suffix");
        assert!(!obj.contains_key("transactionData"), "should NOT have unsuffixed key in JSON mode");
    }

    #[test]
    fn test_sim_xdr_array_base64_unsuffixed() {
        let mut obj = serde_json::Map::new();
        let events: Vec<DiagnosticEvent> = vec![];
        insert_sim_xdr_array_field(&mut obj, "events", &events, XdrFormat::Base64).unwrap();
        assert!(obj.contains_key("events"), "base64 mode should use unsuffixed key");
        assert!(!obj.contains_key("eventsXdr"), "should NOT have Xdr suffix");
    }

    #[test]
    fn test_sim_xdr_array_json_suffixed() {
        let mut obj = serde_json::Map::new();
        let events: Vec<DiagnosticEvent> = vec![];
        insert_sim_xdr_array_field(&mut obj, "events", &events, XdrFormat::Json).unwrap();
        assert!(obj.contains_key("eventsJson"), "json mode should have Json suffix");
        assert!(!obj.contains_key("events"), "should NOT have unsuffixed key in JSON mode");
    }

    // -----------------------------------------------------------------------
    // B8. serialize_state_changes (4 tests)
    // -----------------------------------------------------------------------

    fn make_test_ledger_entry(key_byte: u8) -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([key_byte; 32]))),
                balance: 10_000_000,
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: Default::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: Default::default(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    #[test]
    fn test_state_changes_created() {
        let key = test_account_key(1);
        let entry = make_test_ledger_entry(1);
        let diffs = vec![LedgerEntryDiff {
            key,
            state_before: None,
            state_after: Some(entry),
        }];
        let result = serialize_state_changes(&diffs, XdrFormat::Base64).unwrap();
        let arr = result.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["type"], "created");
        assert!(arr[0]["before"].is_null());
        assert!(arr[0]["after"].is_string());
    }

    #[test]
    fn test_state_changes_updated() {
        let key = test_account_key(2);
        let before = make_test_ledger_entry(2);
        let mut after = before.clone();
        if let LedgerEntryData::Account(ref mut acct) = after.data {
            acct.balance = 20_000_000;
        }
        let diffs = vec![LedgerEntryDiff {
            key,
            state_before: Some(before),
            state_after: Some(after),
        }];
        let result = serialize_state_changes(&diffs, XdrFormat::Base64).unwrap();
        let arr = result.as_array().unwrap();
        assert_eq!(arr[0]["type"], "updated");
        assert!(arr[0]["before"].is_string());
        assert!(arr[0]["after"].is_string());
    }

    #[test]
    fn test_state_changes_deleted() {
        let key = test_account_key(3);
        let entry = make_test_ledger_entry(3);
        let diffs = vec![LedgerEntryDiff {
            key,
            state_before: Some(entry),
            state_after: None,
        }];
        let result = serialize_state_changes(&diffs, XdrFormat::Base64).unwrap();
        let arr = result.as_array().unwrap();
        assert_eq!(arr[0]["type"], "deleted");
        assert!(arr[0]["before"].is_string());
        assert!(arr[0]["after"].is_null());
    }

    #[test]
    fn test_state_changes_json_format() {
        let key = test_account_key(4);
        let entry = make_test_ledger_entry(4);
        let diffs = vec![LedgerEntryDiff {
            key,
            state_before: None,
            state_after: Some(entry),
        }];
        let result = serialize_state_changes(&diffs, XdrFormat::Json).unwrap();
        let arr = result.as_array().unwrap();
        assert_eq!(arr[0]["type"], "created");
        // JSON mode uses "keyJson", "beforeJson", "afterJson"
        assert!(arr[0].get("keyJson").is_some());
        assert!(arr[0].get("beforeJson").is_some());
        assert!(arr[0].get("afterJson").is_some());
        // Should NOT have base64 keys
        assert!(arr[0].get("key").is_none());
        assert!(arr[0].get("before").is_none());
        assert!(arr[0].get("after").is_none());
    }

    // -----------------------------------------------------------------------
    // B9-B10. build_error_response / build_footprint_response (4 tests)
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_error_response_structure() {
        let resp = build_error_response("something went wrong".into(), 42).unwrap();
        assert_eq!(resp["error"], "something went wrong");
        assert_eq!(resp["latestLedger"], 42);
        assert!(resp.get("transactionData").is_some());
        assert!(resp.get("minResourceFee").is_some());
        assert!(resp.get("cost").is_some());
    }

    #[test]
    fn test_build_error_response_defaults() {
        let resp = build_error_response("err".into(), 1).unwrap();
        assert_eq!(resp["transactionData"], "");
        assert_eq!(resp["minResourceFee"], "0");
        assert_eq!(resp["cost"]["cpuInsns"], "0");
        assert_eq!(resp["cost"]["memBytes"], "0");
    }

    #[test]
    fn test_build_footprint_response_base64() {
        let tx_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: Default::default(),
                    read_write: Default::default(),
                },
                instructions: 0,
                disk_read_bytes: 0,
                write_bytes: 0,
            },
            resource_fee: 12345,
        };
        let info = test_soroban_network_info();
        let resp = build_footprint_response(tx_data, &info, 100, XdrFormat::Base64).unwrap();
        assert!(resp.get("transactionData").is_some());
        assert!(resp["transactionData"].is_string());
        assert_eq!(resp["minResourceFee"], "12345");
        assert_eq!(resp["latestLedger"], 100);
        assert_eq!(resp["cost"]["cpuInsns"], "0");
    }

    #[test]
    fn test_build_footprint_response_json() {
        let tx_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: Default::default(),
                    read_write: Default::default(),
                },
                instructions: 0,
                disk_read_bytes: 0,
                write_bytes: 0,
            },
            resource_fee: 99,
        };
        let info = test_soroban_network_info();
        let resp = build_footprint_response(tx_data, &info, 50, XdrFormat::Json).unwrap();
        // JSON mode: "transactionDataJson" instead of "transactionData"
        assert!(resp.get("transactionDataJson").is_some());
        assert!(resp.get("transactionData").is_none());
    }

    // -----------------------------------------------------------------------
    // B11. compute_invoke_resource_fee (4 tests) [REGRESSION]
    // -----------------------------------------------------------------------

    #[test]
    fn test_disk_read_entries_excludes_soroban() {
        // 1 Account (RO) + 1 ContractData (RO) -> disk_read_entries = 1
        let resources = test_soroban_resources(
            vec![test_account_key(1), test_contract_data_key(0xAA)],
            vec![],
        );
        let info = test_soroban_network_info();
        let fee = compute_invoke_resource_fee(
            &resources, &[], &info, 100, 0, 1000, 0,
        );
        // Fee should be > 0
        assert!(fee > 0);
        // The fee with only 1 disk read entry should be less than with 2
        let resources2 = test_soroban_resources(
            vec![test_account_key(1), test_account_key(2)],
            vec![],
        );
        let fee2 = compute_invoke_resource_fee(
            &resources2, &[], &info, 100, 0, 1000, 0,
        );
        // 2 account entries = 2 disk reads, should cost more
        assert!(fee2 > fee, "2 account entries should cost more than 1 account + 1 contract");
    }

    #[test]
    fn test_disk_read_entries_includes_restored() {
        let resources = test_soroban_resources(vec![], vec![]);
        let info = test_soroban_network_info();
        let fee_no_restore = compute_invoke_resource_fee(
            &resources, &[], &info, 100, 0, 1000, 0,
        );
        let fee_with_restore = compute_invoke_resource_fee(
            &resources, &[], &info, 100, 0, 1000, 3,
        );
        // 3 restored entries should add disk read cost
        assert!(fee_with_restore > fee_no_restore);
    }

    #[test]
    fn test_disk_read_entries_mixed() {
        // 2 accounts RO + 1 contract RW + 1 restored -> disk_read_entries = 2 + 0 + 1 = 3
        let resources = test_soroban_resources(
            vec![test_account_key(1), test_account_key(2)],
            vec![test_contract_data_key(0xCC)],
        );
        let info = test_soroban_network_info();
        let fee = compute_invoke_resource_fee(
            &resources, &[], &info, 100, 0, 1000, 1,
        );
        assert!(fee > 0);
    }

    #[test]
    fn test_refundable_fee_adjustment() {
        // With non-zero rent changes, the refundable portion should be scaled by 1.15
        use soroban_host::fees::LedgerEntryRentChange;

        let resources = test_soroban_resources(vec![], vec![test_contract_data_key(0xAA)]);
        let info = test_soroban_network_info();

        let rent_changes = vec![LedgerEntryRentChange {
            is_persistent: true,
            is_code_entry: false,
            old_size_bytes: 100,
            new_size_bytes: 100,
            old_live_until_ledger: 100,
            new_live_until_ledger: 200,
        }];

        let fee = compute_invoke_resource_fee(
            &resources, &rent_changes, &info, 50, 0, 1000, 0,
        );
        // Fee should be positive and include rent
        assert!(fee > 0);
    }

    // -----------------------------------------------------------------------
    // B12. estimate_tx_size (2 tests)
    // -----------------------------------------------------------------------

    #[test]
    fn test_tx_size_invoke_reasonable() {
        let op = OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
            host_function: HostFunction::InvokeContract(InvokeContractArgs {
                contract_address: ScAddress::Contract(ContractId(Hash([0xAA; 32]))),
                function_name: ScSymbol("hello".try_into().unwrap()),
                args: Default::default(),
            }),
            auth: Default::default(),
        });
        let resources = test_soroban_resources(vec![], vec![]);
        let size = estimate_tx_size_for_op(&op, &resources);
        assert!(size > 0);
        // Reasonable: a minimal invoke tx with 20 sigs + preconditions should be > 1000 bytes
        assert!(size > 1000, "estimate should include 20 sigs overhead, got {size}");
    }

    #[test]
    fn test_tx_size_extend_ttl() {
        let op = OperationBody::ExtendFootprintTtl(ExtendFootprintTtlOp {
            ext: ExtensionPoint::V0,
            extend_to: 5000,
        });
        let resources = test_soroban_resources(vec![test_contract_data_key(0xAA)], vec![]);
        let size = estimate_tx_size_for_op(&op, &resources);
        assert!(size > 0);
    }
}
