//! stellar-core compatible `/tx` handler.
//!
//! Accepts `GET /tx?blob=<base64 TransactionEnvelope XDR>` and returns
//! stellar-core's exact response format with status strings.
//!
//! The `error` field contains a base64-encoded XDR `TransactionResult`
//! (matching stellar-core's wire format), not a human-readable string.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use henyey_tx::TransactionResultCodeExt;
use stellar_xdr::curr::{
    Limits, ReadXdr, TransactionEnvelope, TransactionResult, TransactionResultExt,
    TransactionResultResult, WriteXdr,
};

use crate::compat_http::CompatServerState;

/// Query parameters for `GET /tx?blob=...`.
#[derive(Deserialize)]
pub(crate) struct TxQueryParams {
    blob: Option<String>,
}

/// GET /tx?blob=<base64>
///
/// Submits a transaction in stellar-core's exact wire format.
/// Returns `{"status": "PENDING"}` or `{"status": "ERROR", "error": "<base64 XDR TransactionResult>"}`.
pub(crate) async fn compat_tx_handler(
    State(state): State<Arc<CompatServerState>>,
    Query(params): Query<TxQueryParams>,
) -> impl IntoResponse {
    let blob = match params.blob {
        Some(b) if !b.is_empty() => b,
        _ => {
            return Json(serde_json::json!({
                "exception": "Must specify a tx blob: tx?blob=<tx in xdr format>"
            }))
            .into_response();
        }
    };

    // Decode base64 to TransactionEnvelope XDR.
    let tx_bytes = match BASE64.decode(&blob) {
        Ok(bytes) => bytes,
        Err(e) => {
            return Json(serde_json::json!({
                "exception": format!("Failed to decode tx blob: {}", e)
            }))
            .into_response();
        }
    };

    let tx_env = match TransactionEnvelope::from_xdr(&tx_bytes, Limits::none()) {
        Ok(tx) => tx,
        Err(e) => {
            return Json(serde_json::json!({
                "exception": format!("Failed to parse tx envelope: {}", e)
            }))
            .into_response();
        }
    };

    // Submit to the herder.
    tracing::info!("compat /tx: Received transaction submission");
    let result = state.app.submit_transaction(tx_env).await;
    tracing::info!(?result, "compat /tx: Transaction submission result");

    // Map to stellar-core status strings.
    use henyey_herder::TxQueueResult;
    let response: CompatTxResponse = match result {
        TxQueueResult::Added => CompatTxResponse {
            status: "PENDING".to_string(),
            error: None,
            diagnostic_events: None,
        },
        TxQueueResult::Duplicate => CompatTxResponse {
            status: "DUPLICATE".to_string(),
            error: None,
            diagnostic_events: None,
        },
        TxQueueResult::TryAgainLater | TxQueueResult::QueueFull => CompatTxResponse {
            status: "TRY_AGAIN_LATER".to_string(),
            error: None,
            diagnostic_events: None,
        },
        TxQueueResult::Filtered => CompatTxResponse {
            status: "FILTERED".to_string(),
            error: None,
            diagnostic_events: None,
        },
        TxQueueResult::Invalid(code) => {
            // stellar-core puts the base64-encoded TransactionResult XDR in
            // the "error" field. We synthesize a minimal TransactionResult from
            // the result code.
            let xdr_result_result = code
                .map(|c| c.to_xdr_result())
                .unwrap_or(TransactionResultResult::TxInternalError);
            let error_b64 = encode_tx_result(xdr_result_result, 0);
            CompatTxResponse {
                status: "ERROR".to_string(),
                error: Some(error_b64),
                diagnostic_events: None,
            }
        }
        TxQueueResult::Banned => {
            let error_b64 = encode_tx_result(TransactionResultResult::TxBadAuth, 0);
            CompatTxResponse {
                status: "ERROR".to_string(),
                error: Some(error_b64),
                diagnostic_events: None,
            }
        }
        TxQueueResult::FeeTooLow => {
            let error_b64 = encode_tx_result(TransactionResultResult::TxInsufficientFee, 0);
            CompatTxResponse {
                status: "ERROR".to_string(),
                error: Some(error_b64),
                diagnostic_events: None,
            }
        }
    };

    Json(response).into_response()
}

/// Encode a `TransactionResultResult` into a base64-encoded `TransactionResult` XDR.
fn encode_tx_result(result: TransactionResultResult, fee_charged: i64) -> String {
    let tx_result = TransactionResult {
        fee_charged,
        result,
        ext: TransactionResultExt::V0,
    };
    let bytes = tx_result.to_xdr(Limits::none()).unwrap_or_default();
    BASE64.encode(&bytes)
}

/// stellar-core compatible tx submission response.
///
/// Matches the Go SDK's `proto.TXResponse` exactly:
/// - `status`: one of "PENDING", "DUPLICATE", "ERROR", "TRY_AGAIN_LATER", "FILTERED"
/// - `error`: base64-encoded XDR `TransactionResult` (only on ERROR)
/// - `diagnostic_events`: base64-encoded XDR `Vec<DiagnosticEvent>` (only on ERROR for Soroban txs)
#[derive(Serialize)]
struct CompatTxResponse {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnostic_events: Option<String>,
}
