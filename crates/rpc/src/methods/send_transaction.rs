//! Handler for the `sendTransaction` JSON-RPC method.

use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde_json::json;
use stellar_xdr::curr::{Limits, ReadXdr, TransactionEnvelope, TransactionResultCode};

use crate::context::RpcContext;
use crate::error::JsonRpcError;
use crate::util::{self, XdrFormat};

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
        .map_err(|e| JsonRpcError::invalid_params(format!("invalid base64: {}", e)))?;

    // SECURITY: XDR decode bounded by HTTP body size limit; Limits::none() safe after frame-level check
    let tx_env = TransactionEnvelope::from_xdr(&tx_bytes, Limits::none())
        .map_err(|e| JsonRpcError::invalid_params(format!("invalid XDR: {}", e)))?;

    // Compute the transaction hash from a reference (no clone needed)
    let network_id = henyey_common::NetworkId::from_passphrase(&ctx.app.info().network_passphrase);
    let hash = henyey_tx::TransactionFrame::hash_envelope(&tx_env, &network_id)
        .map(|h| h.to_hex())
        .unwrap_or_default();

    let ledger = ctx.app.ledger_summary();

    // Submit to herder (move tx_env, no clone)
    let result = ctx.app.submit_transaction(tx_env).await;

    // Build base response with fields common to all outcomes
    let close_time = ledger.close_time.to_string();
    let mut obj = serde_json::Map::new();
    obj.insert("hash".into(), json!(hash));
    obj.insert("latestLedger".into(), json!(ledger.num));
    obj.insert("latestLedgerCloseTime".into(), json!(close_time));

    match result {
        henyey_herder::TxQueueResult::Added => {
            obj.insert("status".into(), json!("PENDING"));
        }
        henyey_herder::TxQueueResult::Duplicate => {
            obj.insert("status".into(), json!("DUPLICATE"));
        }
        henyey_herder::TxQueueResult::QueueFull | henyey_herder::TxQueueResult::TryAgainLater => {
            obj.insert("status".into(), json!("TRY_AGAIN_LATER"));
        }
        henyey_herder::TxQueueResult::Invalid(code) => {
            insert_error_response(&mut obj, format, code)?;
        }
        henyey_herder::TxQueueResult::Banned => {
            insert_error_response(&mut obj, format, Some(TransactionResultCode::TxTooLate))?;
        }
        henyey_herder::TxQueueResult::FeeTooLow => {
            insert_error_response(
                &mut obj,
                format,
                Some(TransactionResultCode::TxInsufficientFee),
            )?;
        }
        henyey_herder::TxQueueResult::Filtered => {
            insert_error_response(&mut obj, format, Some(TransactionResultCode::TxFailed))?;
        }
    }

    Ok(serde_json::Value::Object(obj))
}

fn insert_error_response(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    format: XdrFormat,
    code: Option<TransactionResultCode>,
) -> Result<(), JsonRpcError> {
    obj.insert("status".into(), json!("ERROR"));
    let error_result = build_error_result(code);
    util::insert_xdr_field(obj, "errorResult", &error_result, format)?;
    insert_empty_diagnostic_events(obj, format);
    Ok(())
}

/// Insert an empty diagnosticEvents array into the response.
fn insert_empty_diagnostic_events(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    format: XdrFormat,
) {
    match format {
        XdrFormat::Base64 => {
            obj.insert("diagnosticEventsXdr".into(), json!([]));
        }
        XdrFormat::Json => {
            obj.insert("diagnosticEventsJson".into(), json!([]));
        }
    }
}

/// Build an error TransactionResult for the response using the actual error code.
fn build_error_result(code: Option<TransactionResultCode>) -> stellar_xdr::curr::TransactionResult {
    use henyey_tx::TransactionResultCodeExt;
    use stellar_xdr::curr::{TransactionResult, TransactionResultExt, TransactionResultResult};

    let result = code
        .map(|c| c.to_xdr_result())
        .unwrap_or(TransactionResultResult::TxInternalError);

    TransactionResult {
        fee_charged: 0,
        result,
        ext: TransactionResultExt::V0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{TransactionResultResult, WriteXdr};

    #[test]
    fn test_build_error_result_structure() {
        let result = build_error_result(Some(TransactionResultCode::TxFailed));
        assert_eq!(result.fee_charged, 0);
        assert!(matches!(
            result.result,
            TransactionResultResult::TxFailed(_)
        ));
        // Should be serializable to XDR
        assert!(result.to_xdr(Limits::none()).is_ok());
    }

    #[test]
    fn test_insert_empty_diagnostic_events() {
        let mut obj = serde_json::Map::new();
        insert_empty_diagnostic_events(&mut obj, XdrFormat::Base64);
        assert_eq!(obj["diagnosticEventsXdr"], json!([]));

        let mut obj2 = serde_json::Map::new();
        insert_empty_diagnostic_events(&mut obj2, XdrFormat::Json);
        assert_eq!(obj2["diagnosticEventsJson"], json!([]));
    }

    #[test]
    fn test_build_error_result_codes() {
        // TxTooLate
        let result = build_error_result(Some(TransactionResultCode::TxTooLate));
        assert!(matches!(result.result, TransactionResultResult::TxTooLate));

        // TxInsufficientFee
        let result = build_error_result(Some(TransactionResultCode::TxInsufficientFee));
        assert!(matches!(
            result.result,
            TransactionResultResult::TxInsufficientFee
        ));

        // None -> TxInternalError
        let result = build_error_result(None);
        assert!(matches!(
            result.result,
            TransactionResultResult::TxInternalError
        ));
    }
}
