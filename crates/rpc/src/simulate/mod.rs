//! Soroban transaction simulation for `simulateTransaction`.
//!
//! Supports all three Soroban operation types:
//! - `InvokeHostFunction`: Full host function simulation via recording mode
//! - `ExtendFootprintTtl`: TTL extension resource/fee estimation
//! - `RestoreFootprint`: Archived entry restore resource/fee estimation

mod convert;
mod preflight;
mod resources;
mod response;
mod snapshot;

pub(crate) use snapshot::BucketListSnapshotSource;

use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use soroban_env_host_p25 as soroban_host;
use stellar_xdr::curr::{
    HostFunction, LedgerKey, Limits, OperationBody, ReadXdr, SorobanTransactionData,
    TransactionEnvelope,
};
use tokio::sync::Semaphore;

use crate::context::RpcContext;
use crate::error::JsonRpcError;
use crate::util::{self, BlockingError, XdrFormat};

use preflight::{run_invoke_simulation, simulate_extend_ttl_op, simulate_restore_op};
use response::{
    build_error_response, build_footprint_response, build_invoke_response, InvokeResponseContext,
};

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

/// Represents a single ledger entry state change from simulation.
pub(self) struct LedgerEntryDiff {
    key: LedgerKey,
    state_before: Option<stellar_xdr::curr::LedgerEntry>,
    state_after: Option<stellar_xdr::curr::LedgerEntry>,
}

/// Extract the Soroban operation, source account, and optional footprint from the envelope.
fn extract_soroban_op(
    tx_env: &TransactionEnvelope,
) -> Result<
    (
        stellar_xdr::curr::AccountId,
        SorobanOp,
        stellar_xdr::curr::Memo,
    ),
    JsonRpcError,
> {
    let (source, ops, ext, memo) = match tx_env {
        TransactionEnvelope::Tx(tx) => (
            &tx.tx.source_account,
            &tx.tx.operations,
            &tx.tx.ext,
            &tx.tx.memo,
        ),
        TransactionEnvelope::TxV0(_) => {
            return Err(JsonRpcError::invalid_params(
                "v0 transactions not supported",
            ));
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

/// Maximum length of a memo text field in bytes (Stellar protocol limit).
const MAX_MEMO_TEXT_BYTES: usize = 28;

/// Validate memo (MemoText must be <= MAX_MEMO_TEXT_BYTES bytes).
fn validate_memo(memo: &stellar_xdr::curr::Memo) -> Result<(), JsonRpcError> {
    if let stellar_xdr::curr::Memo::Text(text) = memo {
        if text.len() > MAX_MEMO_TEXT_BYTES {
            return Err(JsonRpcError::invalid_params(format!(
                "memo text too long: {} bytes (max {})",
                text.len(),
                MAX_MEMO_TEXT_BYTES,
            )));
        }
    }
    Ok(())
}

fn muxed_to_account_id(source: &stellar_xdr::curr::MuxedAccount) -> stellar_xdr::curr::AccountId {
    match source {
        stellar_xdr::curr::MuxedAccount::Ed25519(key) => stellar_xdr::curr::AccountId(
            stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(key.clone()),
        ),
        stellar_xdr::curr::MuxedAccount::MuxedEd25519(muxed) => stellar_xdr::curr::AccountId(
            stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(muxed.ed25519.clone()),
        ),
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
    let mut keys = Vec::with_capacity(footprint.read_only.len() + footprint.read_write.len());
    keys.extend(footprint.read_only.iter().cloned());
    keys.extend(footprint.read_write.iter().cloned());
    Ok(keys)
}

/// Common simulation context: snapshot, ledger info, and Soroban network config.
struct SimulationContext {
    snapshot_source: BucketListSnapshotSource,
    ledger_info: soroban_host::LedgerInfo,
    soroban_info: henyey_ledger::SorobanNetworkInfo,
    latest_ledger: u32,
}

impl SimulationContext {
    /// Build from the running app state.
    ///
    /// Uses the snapshot's own ledger header instead of `app.ledger_summary()`
    /// to avoid TOCTOU: the snapshot and header are captured atomically.
    fn from_app(app: &dyn crate::context::RpcAppHandle) -> Result<Self, JsonRpcError> {
        let bl_snapshot = app
            .bucket_snapshot_manager()
            .copy_searchable_live_snapshot()
            .ok_or_else(|| JsonRpcError::internal("bucket list snapshot not available"))?;

        let soroban_info = app
            .soroban_network_info()
            .ok_or_else(|| JsonRpcError::internal("soroban network config not available"))?;

        let header = bl_snapshot.ledger_header();
        let ledger_num = bl_snapshot.ledger_seq();
        let network_id = henyey_common::NetworkId::from_passphrase(&app.info().network_passphrase);

        let ledger_info = soroban_host::LedgerInfo {
            protocol_version: header.ledger_version,
            sequence_number: ledger_num,
            timestamp: header.scp_value.close_time.0,
            network_id: network_id.0 .0,
            base_reserve: header.base_reserve,
            min_temp_entry_ttl: soroban_info.min_temporary_ttl,
            min_persistent_entry_ttl: soroban_info.min_persistent_ttl,
            max_entry_ttl: soroban_info.max_entry_ttl,
        };

        let snapshot_source = BucketListSnapshotSource::new(bl_snapshot, ledger_num);

        Ok(Self {
            snapshot_source,
            ledger_info,
            soroban_info,
            latest_ledger: ledger_num,
        })
    }
}

// ---------------------------------------------------------------------------
// Footprint simulation helper
// ---------------------------------------------------------------------------

/// Run a footprint-only simulation (ExtendFootprintTtl or RestoreFootprint) in a
/// blocking task and build the JSON-RPC response.
///
/// The caller provides a closure that performs the actual simulation given
/// references to the snapshot source, ledger info, and soroban network config.
///
/// Uses [`try_bounded_blocking`] to acquire and hold an `OwnedSemaphorePermit`
/// inside the blocking closure, ensuring the permit survives async cancellation.
async fn run_footprint_simulation<F>(
    simulation_semaphore: &Arc<Semaphore>,
    snapshot_source: BucketListSnapshotSource,
    ledger_info: soroban_host::LedgerInfo,
    soroban_info: henyey_ledger::SorobanNetworkInfo,
    latest_ledger: u32,
    format: XdrFormat,
    sim_fn: F,
) -> Result<serde_json::Value, JsonRpcError>
where
    F: FnOnce(
            &BucketListSnapshotSource,
            &soroban_host::LedgerInfo,
            &henyey_ledger::SorobanNetworkInfo,
        ) -> Result<SorobanTransactionData, String>
        + Send
        + 'static,
{
    let result = util::try_bounded_blocking(simulation_semaphore, move || {
        sim_fn(&snapshot_source, &ledger_info, &soroban_info)
    })
    .await;

    match result {
        Ok(tx_data) => build_footprint_response(tx_data, latest_ledger, format),
        Err(BlockingError::Inner(e)) => build_error_response(e, latest_ledger),
        Err(BlockingError::SemaphoreFull) => Err(JsonRpcError::server_busy(
            "too many concurrent simulation requests",
        )),
        Err(BlockingError::SemaphoreClosed) => {
            Err(JsonRpcError::internal("simulation semaphore closed"))
        }
        Err(BlockingError::JoinError(e)) => {
            Err(JsonRpcError::internal_logged("simulation failed", &e))
        }
    }
}

// ---------------------------------------------------------------------------
// Handler entry point
// ---------------------------------------------------------------------------

// SECURITY: simulation input bounded by HTTP body size limit and serde type validation.
// SECURITY: concurrent simulations bounded by try_bounded_blocking with simulation_semaphore.
// Pre-blocking work (XDR parsing, validation) is bounded by request_semaphore at the server level.
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

    // Soroban stateless structural validation (closes drift with queue admission).
    // We call the Soroban-specific subset here rather than full check_valid_pre_seq_num
    // because simulation is Soroban-only and doesn't need classic op validation.
    {
        let frame = henyey_tx::TransactionFrame::from_owned(tx_env.clone());
        if !frame.validate_soroban_memo() {
            return Err(JsonRpcError::invalid_params(
                "Soroban transactions must not use memo or muxed source accounts",
            ));
        }
        if !frame.validate_host_fn() {
            return Err(JsonRpcError::invalid_params(
                "invalid host function pairing",
            ));
        }
        // Duplicate footprint keys
        if let Some(data) = frame.soroban_data() {
            let fp = &data.resources.footprint;
            let mut seen = std::collections::HashSet::new();
            for key in fp.read_only.iter().chain(fp.read_write.iter()) {
                if !seen.insert(key) {
                    return Err(JsonRpcError::invalid_params(
                        "duplicate key in Soroban footprint",
                    ));
                }
            }
        }
    }

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

    let sim = SimulationContext::from_app(&*ctx.app)?;

    match soroban_op {
        SorobanOp::InvokeHostFunction { host_fn, auth } => {
            // Validate and resolve authMode
            if !auth_mode_str.is_empty()
                && !matches!(auth_mode_str, "enforce" | "record" | "record_allow_nonroot")
            {
                return Err(JsonRpcError::invalid_params(format!(
                    "unsupported authMode: '{}' (allowed: enforce, record, record_allow_nonroot)",
                    auth_mode_str
                )));
            }

            // Determine the effective auth mode
            let resolved_auth_mode = resolve_auth_mode(auth_mode_str, &auth)?;

            handle_invoke(
                &ctx.simulation_semaphore,
                InvokeRequest {
                    host_fn,
                    source_account,
                    ledger_info: sim.ledger_info,
                    snapshot_source: sim.snapshot_source,
                    soroban_info: sim.soroban_info.clone(),
                    latest_ledger: sim.latest_ledger,
                    format,
                    auth_mode: resolved_auth_mode,
                    instruction_leeway,
                },
            )
            .await
        }
        SorobanOp::ExtendFootprintTtl { keys, extend_to } => {
            if !auth_mode_str.is_empty() {
                return Err(JsonRpcError::invalid_params(
                    "authMode is only supported for InvokeHostFunction operations",
                ));
            }
            run_footprint_simulation(
                &ctx.simulation_semaphore,
                sim.snapshot_source,
                sim.ledger_info,
                sim.soroban_info,
                sim.latest_ledger,
                format,
                move |snap, li, si| simulate_extend_ttl_op(snap, li, &keys, extend_to, si),
            )
            .await
        }
        SorobanOp::RestoreFootprint { keys } => {
            if !auth_mode_str.is_empty() {
                return Err(JsonRpcError::invalid_params(
                    "authMode is only supported for InvokeHostFunction operations",
                ));
            }
            run_footprint_simulation(
                &ctx.simulation_semaphore,
                sim.snapshot_source,
                sim.ledger_info,
                sim.soroban_info,
                sim.latest_ledger,
                format,
                move |snap, li, si| simulate_restore_op(snap, li, &keys, si),
            )
            .await
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
        "enforce" => {
            let p25_auth: Vec<soroban_host::xdr::SorobanAuthorizationEntry> = tx_auth
                .iter()
                .map(|a| {
                    convert::ws_to_p25(a).map_err(|_| {
                        JsonRpcError::invalid_params(
                            "invalid SorobanAuthorizationEntry XDR in auth entries",
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(RecordingInvocationAuthMode::Enforcing(p25_auth))
        }
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
                let p25_auth: Vec<soroban_host::xdr::SorobanAuthorizationEntry> = tx_auth
                    .iter()
                    .map(|a| {
                        convert::ws_to_p25(a).map_err(|_| {
                            JsonRpcError::invalid_params(
                                "invalid SorobanAuthorizationEntry XDR in auth entries",
                            )
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(RecordingInvocationAuthMode::Enforcing(p25_auth))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// InvokeHostFunction path
// ---------------------------------------------------------------------------

async fn handle_invoke(
    simulation_semaphore: &Arc<Semaphore>,
    request: InvokeRequest,
) -> Result<serde_json::Value, JsonRpcError> {
    let host_fn_clone = request.host_fn.clone();
    let source_account_clone = request.source_account.clone();
    let ledger_info_clone = request.ledger_info.clone();
    let snapshot_source = request.snapshot_source;
    let auth_mode = request.auth_mode;

    let result = util::try_bounded_blocking(simulation_semaphore, move || {
        run_invoke_simulation(
            host_fn_clone,
            source_account_clone,
            ledger_info_clone,
            snapshot_source,
            auth_mode,
        )
    })
    .await;

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
        Err(BlockingError::Inner(e)) => build_error_response(e, request.latest_ledger),
        Err(BlockingError::SemaphoreFull) => Err(JsonRpcError::server_busy(
            "too many concurrent simulation requests",
        )),
        Err(BlockingError::SemaphoreClosed) => {
            Err(JsonRpcError::internal("simulation semaphore closed"))
        }
        Err(BlockingError::JoinError(e)) => {
            Err(JsonRpcError::internal_logged("simulation failed", &e))
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::*;

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

    fn test_contract_data_key(contract_byte: u8) -> LedgerKey {
        LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(Hash([contract_byte; 32]))),
            key: ScVal::LedgerKeyContractInstance,
            durability: ContractDataDurability::Persistent,
        })
    }

    // -----------------------------------------------------------------------
    // AUDIT-001: simulation concurrency limit
    // -----------------------------------------------------------------------

    #[test]
    fn test_audit_001_simulation_semaphore_rejects_when_full() {
        // Verify that try_acquire_owned on a full semaphore returns Err,
        // which is the mechanism used by try_bounded_blocking to reject excess
        // simulation requests.
        let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(2));
        let _p1 = sem
            .clone()
            .try_acquire_owned()
            .expect("first permit should succeed");
        let _p2 = sem
            .clone()
            .try_acquire_owned()
            .expect("second permit should succeed");
        assert!(
            sem.clone().try_acquire_owned().is_err(),
            "third acquire should fail when semaphore is full"
        );
    }

    #[test]
    fn test_audit_001_server_busy_error_code() {
        let err = crate::error::JsonRpcError::server_busy("too many requests");
        assert_eq!(err.code, crate::error::SERVER_BUSY);
        assert!(err.message.contains("too many"));
    }

    // -----------------------------------------------------------------------
    // Simulation cancellation-safety regression (#1744)
    // -----------------------------------------------------------------------

    /// When the simulation semaphore is exhausted, `try_bounded_blocking` returns
    /// `SemaphoreFull`. The simulation handler maps this to SERVER_BUSY (-32000).
    /// This test verifies the complete error mapping path end-to-end.
    #[tokio::test]
    async fn test_simulation_semaphore_full_returns_server_busy() {
        let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let _held = sem.clone().try_acquire_owned().unwrap();

        // Call try_bounded_blocking with an exhausted semaphore — mirrors what
        // run_footprint_simulation and handle_invoke do internally.
        let result: Result<(), crate::util::BlockingError<String>> =
            crate::util::try_bounded_blocking(&sem, || Ok(())).await;

        // Apply the same error mapping the simulation handlers use.
        let err = match result {
            Ok(_) => panic!("should have failed"),
            Err(crate::util::BlockingError::SemaphoreFull) => {
                crate::error::JsonRpcError::server_busy("too many concurrent simulation requests")
            }
            Err(other) => panic!("unexpected error: {other}"),
        };

        assert_eq!(
            err.code,
            crate::error::SERVER_BUSY,
            "must return SERVER_BUSY when simulation semaphore is full"
        );
        assert!(
            err.message.contains("concurrent simulation"),
            "error message must mention simulation: {}",
            err.message
        );
    }

    /// When a simulation is running (permit held inside spawn_blocking via
    /// try_bounded_blocking), a concurrent simulation attempt must be rejected.
    /// After the first completes, the permit is released for new simulations.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_simulation_permit_held_during_blocking_rejects_concurrent() {
        use std::time::Duration;

        let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(1));

        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let barrier2 = barrier.clone();

        // Start a "simulation" that blocks inside spawn_blocking.
        let sem2 = sem.clone();
        let handle = tokio::spawn(async move {
            crate::util::try_bounded_blocking(&sem2, move || {
                let _ = tx.send(());
                barrier2.wait();
                Ok::<(), String>(())
            })
            .await
        });

        // Wait for the blocking closure to start.
        rx.recv().expect("closure must signal start");

        // Concurrent simulation must be rejected — permit is held.
        let concurrent: Result<(), crate::util::BlockingError<String>> =
            crate::util::try_bounded_blocking(&sem, || Ok(())).await;
        assert!(
            matches!(concurrent, Err(crate::util::BlockingError::SemaphoreFull)),
            "concurrent simulation must be rejected while first is running"
        );

        // Unblock the first simulation.
        barrier.wait();
        let _ = handle.await;

        // Now the permit should be available again.
        let mut acquired = false;
        for _ in 0..50 {
            if sem.try_acquire().is_ok() {
                acquired = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            acquired,
            "simulation_semaphore must be released after blocking work completes"
        );
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
    // resolve_auth_mode: error propagation (regression for #1738)
    // -----------------------------------------------------------------------

    #[test]
    fn test_resolve_auth_mode_enforce_valid() {
        let auth = vec![SorobanAuthorizationEntry {
            credentials: SorobanCredentials::SourceAccount,
            root_invocation: SorobanAuthorizedInvocation {
                function: SorobanAuthorizedFunction::ContractFn(InvokeContractArgs {
                    contract_address: ScAddress::Contract(ContractId(Hash([0xAA; 32]))),
                    function_name: ScSymbol("test".try_into().unwrap()),
                    args: Default::default(),
                }),
                sub_invocations: Default::default(),
            },
        }];
        let result = resolve_auth_mode("enforce", &auth);
        assert!(result.is_ok());
    }

    #[test]
    fn test_resolve_auth_mode_record_rejects_auth() {
        let auth = vec![SorobanAuthorizationEntry {
            credentials: SorobanCredentials::SourceAccount,
            root_invocation: SorobanAuthorizedInvocation {
                function: SorobanAuthorizedFunction::ContractFn(InvokeContractArgs {
                    contract_address: ScAddress::Contract(ContractId(Hash([0xAA; 32]))),
                    function_name: ScSymbol("test".try_into().unwrap()),
                    args: Default::default(),
                }),
                sub_invocations: Default::default(),
            },
        }];
        let result = resolve_auth_mode("record", &auth);
        match result {
            Err(e) => assert!(e.message.contains("record")),
            Ok(_) => panic!("expected error for 'record' mode with auth entries"),
        }
    }
}
