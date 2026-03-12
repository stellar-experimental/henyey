mod snapshot;

pub use snapshot::BucketListSnapshotSource;

use std::rc::Rc;
use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde_json::json;
use soroban_env_host_p25 as soroban_host;
use stellar_xdr::curr::{
    HostFunction, Limits, OperationBody, ReadXdr, SorobanTransactionData,
    TransactionEnvelope, WriteXdr,
};

use crate::context::RpcContext;
use crate::error::JsonRpcError;

pub async fn handle(
    ctx: &Arc<RpcContext>,
    params: serde_json::Value,
) -> Result<serde_json::Value, JsonRpcError> {
    let tx_b64 = params
        .get("transaction")
        .and_then(|v| v.as_str())
        .ok_or_else(|| JsonRpcError::invalid_params("missing 'transaction' parameter"))?;

    let tx_bytes = BASE64
        .decode(tx_b64)
        .map_err(|e| JsonRpcError::invalid_params(format!("invalid base64: {}", e)))?;

    let tx_env = TransactionEnvelope::from_xdr(&tx_bytes, Limits::none())
        .map_err(|e| JsonRpcError::invalid_params(format!("invalid XDR: {}", e)))?;

    // Extract the InvokeHostFunction operation
    let (source_account, host_fn) = extract_host_function(&tx_env)?;

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

    let network_id = henyey_common::NetworkId::from_passphrase(
        &ctx.app.info().network_passphrase,
    );

    // Build LedgerInfo for the host
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

    // Build the snapshot source from our bucket list snapshot
    let snapshot_source = BucketListSnapshotSource::new(bl_snapshot, ledger.num);

    // Clone what we need before moving into the blocking task
    let host_fn_clone = host_fn.clone();
    let source_account_clone = source_account.clone();

    // Run simulation in a blocking task (soroban Host uses Rc, not Send)
    let result = tokio::task::spawn_blocking(move || {
        run_simulation(
            host_fn_clone,
            source_account_clone,
            ledger_info,
            snapshot_source,
        )
    })
    .await
    .map_err(|e| JsonRpcError::internal(format!("simulation task failed: {}", e)))?;

    match result {
        Ok(sim_result) => build_success_response(sim_result, &soroban_info, ledger.num),
        Err(e) => build_error_response(e, ledger.num),
    }
}

fn extract_host_function(
    tx_env: &TransactionEnvelope,
) -> Result<(stellar_xdr::curr::AccountId, HostFunction), JsonRpcError> {
    let (source, ops) = match tx_env {
        TransactionEnvelope::Tx(tx) => (&tx.tx.source_account, &tx.tx.operations),
        TransactionEnvelope::TxV0(_) => {
            return Err(JsonRpcError::invalid_params("v0 transactions not supported"));
        }
        TransactionEnvelope::TxFeeBump(fb) => match &fb.tx.inner_tx {
            stellar_xdr::curr::FeeBumpTransactionInnerTx::Tx(inner) => {
                (&inner.tx.source_account, &inner.tx.operations)
            }
        },
    };

    let source_account = match source {
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
    };

    if ops.len() != 1 {
        return Err(JsonRpcError::invalid_params(
            "simulateTransaction requires exactly one operation",
        ));
    }

    match &ops[0].body {
        OperationBody::InvokeHostFunction(op) => Ok((source_account, op.host_function.clone())),
        OperationBody::ExtendFootprintTtl(_) | OperationBody::RestoreFootprint(_) => {
            // For extend/restore, we need the SorobanTransactionData from the tx
            // These are simpler operations that don't need the full host invocation
            match &ops[0].body {
                OperationBody::InvokeHostFunction(op) => {
                    Ok((source_account, op.host_function.clone()))
                }
                _ => Err(JsonRpcError::invalid_params(
                    "ExtendFootprintTtl and RestoreFootprint simulation not yet supported",
                )),
            }
        }
        _ => Err(JsonRpcError::invalid_params(
            "operation must be InvokeHostFunction, ExtendFootprintTtl, or RestoreFootprint",
        )),
    }
}

fn run_simulation(
    host_fn: HostFunction,
    source_account: stellar_xdr::curr::AccountId,
    ledger_info: soroban_host::LedgerInfo,
    snapshot_source: BucketListSnapshotSource,
) -> Result<soroban_host::e2e_invoke::InvokeHostFunctionRecordingModeResult, String> {
    use soroban_host::e2e_invoke::{
        invoke_host_function_in_recording_mode, RecordingInvocationAuthMode,
    };
    use soroban_host::budget::Budget;

    let budget = Budget::default();
    let mut diagnostic_events = Vec::new();
    let seed: [u8; 32] = rand::random();

    let result = invoke_host_function_in_recording_mode(
        &budget,
        true, // enable_diagnostics
        &host_fn,
        &source_account,
        RecordingInvocationAuthMode::Recording(false),
        ledger_info,
        Rc::new(snapshot_source),
        seed,
        &mut diagnostic_events,
    );

    match result {
        Ok(recording_result) => {
            // Check if the invocation itself succeeded
            match &recording_result.invoke_result {
                Ok(_) => Ok(recording_result),
                Err(e) => Err(format!("host function invocation failed: {:?}", e)),
            }
        }
        Err(e) => Err(format!("simulation failed: {:?}", e)),
    }
}

fn build_success_response(
    sim_result: soroban_host::e2e_invoke::InvokeHostFunctionRecordingModeResult,
    soroban_info: &henyey_ledger::SorobanNetworkInfo,
    latest_ledger: u32,
) -> Result<serde_json::Value, JsonRpcError> {
    let resources = &sim_result.resources;

    // Encode auth entries
    let auth_entries: Vec<String> = sim_result
        .auth
        .iter()
        .filter_map(|a| a.to_xdr(Limits::none()).ok().map(|b| BASE64.encode(&b)))
        .collect();

    // Encode the return value
    let return_value = match &sim_result.invoke_result {
        Ok(val) => val.to_xdr(Limits::none()).ok().map(|b| BASE64.encode(&b)),
        Err(_) => None,
    };

    // Build SorobanTransactionData
    let soroban_data = SorobanTransactionData {
        ext: stellar_xdr::curr::SorobanTransactionDataExt::V0,
        resources: resources.clone(),
        resource_fee: compute_resource_fee(resources, soroban_info),
    };

    let soroban_data_xdr = soroban_data
        .to_xdr(Limits::none())
        .map_err(|e| JsonRpcError::internal(format!("XDR encode error: {}", e)))?;

    let min_resource_fee = soroban_data.resource_fee;

    let mut result = json!({
        "transactionData": BASE64.encode(&soroban_data_xdr),
        "minResourceFee": min_resource_fee.to_string(),
        "cost": {
            "cpuInsns": resources.instructions.to_string(),
            "memBytes": "0"
        },
        "latestLedger": latest_ledger
    });

    if !auth_entries.is_empty() {
        result
            .as_object_mut()
            .unwrap()
            .insert("results".to_string(), json!([{
                "auth": auth_entries,
                "xdr": return_value.unwrap_or_default()
            }]));
    } else if let Some(rv) = return_value {
        result
            .as_object_mut()
            .unwrap()
            .insert("results".to_string(), json!([{
                "auth": [],
                "xdr": rv
            }]));
    }

    Ok(result)
}

fn build_error_response(error: String, latest_ledger: u32) -> Result<serde_json::Value, JsonRpcError> {
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

fn compute_resource_fee(
    resources: &stellar_xdr::curr::SorobanResources,
    soroban_info: &henyey_ledger::SorobanNetworkInfo,
) -> i64 {
    use soroban_host::fees::{compute_transaction_resource_fee, FeeConfiguration, TransactionResources};

    let tx_resources = TransactionResources {
        instructions: resources.instructions,
        disk_read_entries: resources.footprint.read_only.len() as u32
            + resources.footprint.read_write.len() as u32,
        write_entries: resources.footprint.read_write.len() as u32,
        disk_read_bytes: resources.disk_read_bytes,
        write_bytes: resources.write_bytes,
        contract_events_size_bytes: 0,
        transaction_size_bytes: 300,
    };

    let fee_config = FeeConfiguration {
        fee_per_instruction_increment: soroban_info.fee_rate_per_instructions_increment,
        fee_per_disk_read_entry: soroban_info.fee_read_ledger_entry,
        fee_per_write_entry: soroban_info.fee_write_ledger_entry,
        fee_per_disk_read_1kb: soroban_info.fee_read_1kb,
        fee_per_write_1kb: soroban_info.fee_write_1kb,
        fee_per_historical_1kb: soroban_info.fee_historical_1kb,
        fee_per_contract_event_1kb: soroban_info.fee_contract_events_size_1kb,
        fee_per_transaction_size_1kb: soroban_info.fee_transaction_size_1kb,
    };

    let (non_refundable, refundable) =
        compute_transaction_resource_fee(&tx_resources, &fee_config);

    // Add adjustment factor (20% overhead like soroban-simulation)
    let total = non_refundable + refundable;
    total + (total / 5) // +20%
}
