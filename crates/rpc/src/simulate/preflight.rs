//! Preflight simulation logic: core simulation execution, host function
//! invocation, footprint processing, state preparation.

use std::rc::Rc;

use soroban_env_host_p25 as soroban_host;
use stellar_xdr::curr::{
    ExtendFootprintTtlOp, ExtensionPoint, LedgerEntryType, LedgerFootprint, LedgerKey, Limits,
    OperationBody, ReadXdr, RestoreFootprintOp, SorobanResources, SorobanTransactionData,
    SorobanTransactionDataExt, WriteXdr,
};

use super::resources::{compute_resource_fee_with_rent, estimate_tx_size_for_op};
use super::snapshot::BucketListSnapshotSource;
use super::LedgerEntryDiff;

pub(super) struct InvokeSimulationOutput {
    pub recording_result: soroban_host::e2e_invoke::InvokeHostFunctionRecordingModeResult,
    pub diagnostic_events: Vec<soroban_host::xdr::DiagnosticEvent>,
    pub state_changes: Vec<LedgerEntryDiff>,
}

pub(super) fn run_invoke_simulation(
    host_fn: stellar_xdr::curr::HostFunction,
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

    // Convert workspace types to P25 types for the host invocation
    let p25_host_fn: soroban_host::xdr::HostFunction = super::convert::ws_to_p25(&host_fn)
        .ok_or_else(|| "failed to convert HostFunction to P25 XDR".to_string())?;
    let p25_source: soroban_host::xdr::AccountId = super::convert::ws_to_p25(&source_account)
        .ok_or_else(|| "failed to convert AccountId to P25 XDR".to_string())?;

    let result = invoke_host_function_in_recording_mode(
        &budget,
        true, // enable_diagnostics
        &p25_host_fn,
        &p25_source,
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
        let state_after = change
            .encoded_new_value
            .as_ref()
            .and_then(|v| stellar_xdr::curr::LedgerEntry::from_xdr(v, Limits::none()).ok());

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

// ---------------------------------------------------------------------------
// Shared helpers for TTL simulation
// ---------------------------------------------------------------------------

/// Resolved entry info shared by extend-TTL and restore simulations.
struct ResolvedEntry {
    durability: soroban_host::xdr::ContractDataDurability,
    current_live_until: u32,
    is_code_entry: bool,
    entry_xdr_size: u32,
    entry_rent_size: u32,
}

/// Look up a key in the snapshot, convert to P25 XDR, and compute its rent
/// size. Returns `Ok(None)` when the entry doesn't exist in the snapshot.
fn resolve_entry(
    key: &LedgerKey,
    snapshot: &BucketListSnapshotSource,
    budget: &soroban_host::budget::Budget,
) -> Result<Option<ResolvedEntry>, String> {
    use soroban_host::e2e_invoke::entry_size_for_rent;
    use soroban_host::ledger_info::get_key_durability;

    let p25_key: soroban_host::xdr::LedgerKey = super::convert::ws_to_p25(key)
        .ok_or_else(|| "failed to convert LedgerKey to P25 XDR".to_string())?;

    let durability = get_key_durability(&p25_key).ok_or_else(|| {
        format!(
            "cannot operate on key {:?}: only contract data/code entries have TTL",
            key.discriminant()
        )
    })?;

    let Some((entry, live_until)) = snapshot.get_unfiltered(key) else {
        return Ok(None);
    };

    let current_live_until = live_until.ok_or_else(|| {
        format!(
            "missing TTL for key that must have TTL: {:?}",
            key.discriminant()
        )
    })?;

    let entry_xdr_size = entry
        .to_xdr(Limits::none())
        .map(|b| b.len() as u32)
        .unwrap_or(0);

    let p25_entry: soroban_host::xdr::LedgerEntry = super::convert::ws_to_p25(&entry)
        .ok_or_else(|| "failed to convert LedgerEntry to P25 XDR".to_string())?;
    let entry_rent_size = entry_size_for_rent(budget, &p25_entry, entry_xdr_size)
        .map_err(|e| format!("entry_size_for_rent failed: {e:?}"))?;

    Ok(Some(ResolvedEntry {
        durability,
        current_live_until,
        is_code_entry: matches!(key.discriminant(), LedgerEntryType::ContractCode),
        entry_xdr_size,
        entry_rent_size,
    }))
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
pub(super) fn simulate_extend_ttl_op(
    snapshot: &BucketListSnapshotSource,
    ledger_info: &soroban_host::LedgerInfo,
    keys: &[LedgerKey],
    extend_to: u32,
    soroban_info: &henyey_ledger::SorobanNetworkInfo,
) -> Result<SorobanTransactionData, String> {
    use soroban_host::fees::LedgerEntryRentChange;

    let budget = soroban_host::budget::Budget::default();
    let new_live_until = ledger_info.sequence_number.saturating_add(extend_to);

    let mut rent_changes = Vec::with_capacity(keys.len());
    let mut extended_keys = Vec::with_capacity(keys.len());

    for key in keys {
        let Some(resolved) = resolve_entry(key, snapshot, &budget)? else {
            continue; // entry doesn't exist, skip
        };

        // Skip entries that don't need extension
        if new_live_until <= resolved.current_live_until {
            continue;
        }

        // Expired entries cannot be extended (must be restored first)
        if resolved.current_live_until < ledger_info.sequence_number {
            return Err(format!(
                "cannot extend TTL for expired entry (live_until={}, \
                 current_ledger={}). Restore the entry first.",
                resolved.current_live_until, ledger_info.sequence_number
            ));
        }

        extended_keys.push(key.clone());

        rent_changes.push(LedgerEntryRentChange {
            is_persistent: resolved.durability
                == soroban_host::xdr::ContractDataDurability::Persistent,
            is_code_entry: resolved.is_code_entry,
            old_size_bytes: resolved.entry_rent_size,
            new_size_bytes: resolved.entry_rent_size,
            old_live_until_ledger: resolved.current_live_until,
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
pub(super) fn simulate_restore_op(
    snapshot: &BucketListSnapshotSource,
    ledger_info: &soroban_host::LedgerInfo,
    keys: &[LedgerKey],
    soroban_info: &henyey_ledger::SorobanNetworkInfo,
) -> Result<SorobanTransactionData, String> {
    use soroban_host::fees::LedgerEntryRentChange;

    let budget = soroban_host::budget::Budget::default();
    let restored_live_until = ledger_info
        .min_live_until_ledger_checked(soroban_host::xdr::ContractDataDurability::Persistent)
        .ok_or("min persistent live_until ledger overflows")?;

    let mut rent_changes = Vec::with_capacity(keys.len());
    let mut restored_keys = Vec::with_capacity(keys.len());
    let mut restored_bytes = 0u32;

    for key in keys {
        let resolved = resolve_entry(key, snapshot, &budget)?
            .ok_or_else(|| format!("missing entry to restore for key {:?}", key.discriminant()))?;

        if resolved.durability != soroban_host::xdr::ContractDataDurability::Persistent {
            return Err(format!(
                "cannot restore key {:?}: only persistent entries can be restored",
                key.discriminant()
            ));
        }

        // Skip entries that are still live (not expired)
        if resolved.current_live_until >= ledger_info.sequence_number {
            continue;
        }

        restored_keys.push(key.clone());
        restored_bytes = restored_bytes.saturating_add(resolved.entry_xdr_size);

        rent_changes.push(LedgerEntryRentChange {
            is_persistent: true,
            is_code_entry: resolved.is_code_entry,
            old_size_bytes: 0,
            new_size_bytes: resolved.entry_rent_size,
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
