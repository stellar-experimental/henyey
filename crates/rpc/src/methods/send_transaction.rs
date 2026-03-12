use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde_json::json;
use stellar_xdr::curr::{Limits, ReadXdr, TransactionEnvelope, WriteXdr};

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

    // Compute the transaction hash
    let network_id =
        henyey_common::NetworkId::from_passphrase(&ctx.app.info().network_passphrase);
    let mut frame = henyey_tx::TransactionFrame::with_network(tx_env.clone(), network_id);
    let hash = frame
        .compute_hash(&network_id)
        .map(|h| h.to_hex())
        .unwrap_or_default();

    let ledger = ctx.app.ledger_summary();

    // Submit to herder
    let result = ctx.app.submit_transaction(tx_env.clone()).await;

    match result {
        henyey_herder::TxQueueResult::Added => Ok(json!({
            "status": "PENDING",
            "hash": hash,
            "latestLedger": ledger.num,
            "latestLedgerCloseTime": ledger.close_time.to_string()
        })),
        henyey_herder::TxQueueResult::Duplicate => Ok(json!({
            "status": "DUPLICATE",
            "hash": hash,
            "latestLedger": ledger.num,
            "latestLedgerCloseTime": ledger.close_time.to_string()
        })),
        henyey_herder::TxQueueResult::QueueFull
        | henyey_herder::TxQueueResult::TryAgainLater => Ok(json!({
            "status": "TRY_AGAIN_LATER",
            "hash": hash,
            "latestLedger": ledger.num,
            "latestLedgerCloseTime": ledger.close_time.to_string()
        })),
        henyey_herder::TxQueueResult::Invalid(code) => {
            let error_result_xdr = build_error_result_xdr(code);

            Ok(json!({
                "status": "ERROR",
                "hash": hash,
                "errorResultXdr": error_result_xdr,
                "latestLedger": ledger.num,
                "latestLedgerCloseTime": ledger.close_time.to_string(),
                "diagnosticEventsXdr": []
            }))
        }
        henyey_herder::TxQueueResult::Banned
        | henyey_herder::TxQueueResult::FeeTooLow
        | henyey_herder::TxQueueResult::Filtered => {
            Ok(json!({
                "status": "ERROR",
                "hash": hash,
                "latestLedger": ledger.num,
                "latestLedgerCloseTime": ledger.close_time.to_string()
            }))
        }
    }
}

fn build_error_result_xdr(
    _code: Option<henyey_tx::TxResultCode>,
) -> String {
    use stellar_xdr::curr::{
        TransactionResult, TransactionResultExt,
        TransactionResultResult,
    };

    // Build a minimal TransactionResult
    let result = TransactionResult {
        fee_charged: 0,
        result: TransactionResultResult::TxFailed(stellar_xdr::curr::VecM::default()),
        ext: TransactionResultExt::V0,
    };

    match result.to_xdr(Limits::none()) {
        Ok(bytes) => BASE64.encode(&bytes),
        Err(_) => String::new(),
    }
}
