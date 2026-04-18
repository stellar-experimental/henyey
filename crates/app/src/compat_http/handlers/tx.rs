//! stellar-core compatible `/tx` handler.
//!
//! Accepts `GET /tx?blob=<base64 TransactionEnvelope XDR>` and returns
//! stellar-core's exact response format with status strings.
//!
//! The `error` field contains a base64-encoded XDR `TransactionResult`
//! (matching stellar-core's wire format), not a human-readable string.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
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
            let error_b64 = match encode_tx_result_or_exception(xdr_result_result, 0) {
                Ok(b64) => b64,
                Err(resp) => return resp,
            };
            CompatTxResponse {
                status: "ERROR".to_string(),
                error: Some(error_b64),
                diagnostic_events: None,
            }
        }
        TxQueueResult::Banned => {
            let error_b64 =
                match encode_tx_result_or_exception(TransactionResultResult::TxBadAuth, 0) {
                    Ok(b64) => b64,
                    Err(resp) => return resp,
                };
            CompatTxResponse {
                status: "ERROR".to_string(),
                error: Some(error_b64),
                diagnostic_events: None,
            }
        }
        TxQueueResult::FeeTooLow => {
            let error_b64 = match encode_tx_result_or_exception(
                TransactionResultResult::TxInsufficientFee,
                0,
            ) {
                Ok(b64) => b64,
                Err(resp) => return resp,
            };
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
fn encode_tx_result(
    result: TransactionResultResult,
    fee_charged: i64,
) -> Result<String, stellar_xdr::curr::Error> {
    let tx_result = TransactionResult {
        fee_charged,
        result,
        ext: TransactionResultExt::V0,
    };
    let bytes = tx_result.to_xdr(Limits::none())?;
    Ok(BASE64.encode(&bytes))
}

/// Encode a result or return a stellar-core-compatible `{"exception": ...}` response.
///
/// On encoding failure, logs the error and returns an HTTP 200 JSON exception
/// matching the compat layer convention (all errors are 200 with `"exception"` key).
#[allow(clippy::result_large_err)]
fn encode_tx_result_or_exception(
    result: TransactionResultResult,
    fee_charged: i64,
) -> Result<String, Response> {
    encode_tx_result(result, fee_charged).map_err(|e| {
        tracing::error!(%e, "Failed to encode transaction result XDR");
        Json(serde_json::json!({
            "exception": "Internal error: failed to encode transaction result"
        }))
        .into_response()
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify PENDING response shape: `{"status": "PENDING"}` with no error/diagnostic_events.
    #[test]
    fn test_tx_response_pending_shape() {
        let response = CompatTxResponse {
            status: "PENDING".into(),
            error: None,
            diagnostic_events: None,
        };

        let value = serde_json::to_value(&response).unwrap();
        let obj = value.as_object().unwrap();

        assert_eq!(obj.len(), 1, "PENDING response should only have 'status'");
        assert_eq!(value["status"], "PENDING");
        assert!(
            obj.get("error").is_none(),
            "error should be absent for PENDING"
        );
        assert!(
            obj.get("diagnostic_events").is_none(),
            "diagnostic_events should be absent for PENDING"
        );
    }

    /// Verify ERROR response shape: `{"status": "ERROR", "error": "<base64>"}`.
    #[test]
    fn test_tx_response_error_shape() {
        let response = CompatTxResponse {
            status: "ERROR".into(),
            error: Some("AAAAAAAAAGT////7".into()),
            diagnostic_events: None,
        };

        let value = serde_json::to_value(&response).unwrap();
        let obj = value.as_object().unwrap();

        assert_eq!(
            obj.len(),
            2,
            "ERROR response should have 'status' and 'error'"
        );
        assert_eq!(value["status"], "ERROR");
        assert!(value["error"].is_string(), "error must be a string");
        assert!(
            obj.get("diagnostic_events").is_none(),
            "diagnostic_events should be absent when None"
        );
    }

    /// Verify ERROR response with diagnostic_events includes all three fields.
    #[test]
    fn test_tx_response_error_with_diagnostics_shape() {
        let response = CompatTxResponse {
            status: "ERROR".into(),
            error: Some("AAAAAAAAAGT////7".into()),
            diagnostic_events: Some("AAAAAQ==".into()),
        };

        let value = serde_json::to_value(&response).unwrap();
        let obj = value.as_object().unwrap();

        assert_eq!(obj.len(), 3, "should have status, error, diagnostic_events");
        assert_eq!(value["status"], "ERROR");
        assert!(value["error"].is_string());
        assert!(value["diagnostic_events"].is_string());
    }

    /// Verify all valid status strings.
    #[test]
    fn test_tx_response_all_status_strings() {
        for status in [
            "PENDING",
            "DUPLICATE",
            "ERROR",
            "TRY_AGAIN_LATER",
            "FILTERED",
        ] {
            let response = CompatTxResponse {
                status: status.into(),
                error: None,
                diagnostic_events: None,
            };
            let value = serde_json::to_value(&response).unwrap();
            assert_eq!(value["status"], status);
        }
    }

    /// Verify `encode_tx_result` produces valid base64 and round-trips through XDR.
    #[test]
    fn test_encode_tx_result_produces_valid_base64() {
        use base64::{engine::general_purpose::STANDARD as BASE64, Engine};

        let b64 = encode_tx_result(TransactionResultResult::TxInternalError, 0)
            .expect("encoding TxInternalError should succeed");
        assert!(!b64.is_empty(), "encoded result should not be empty");

        // Should be valid base64
        let decoded = BASE64.decode(&b64);
        assert!(decoded.is_ok(), "should be valid base64");

        // Should be valid XDR
        let bytes = decoded.unwrap();
        let result = TransactionResult::from_xdr(&bytes, Limits::none());
        assert!(result.is_ok(), "should be valid TransactionResult XDR");
    }

    /// Round-trip verification for all handler-used result variants.
    #[test]
    fn test_encode_tx_result_round_trip_all_variants() {
        use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
        use stellar_xdr::curr::TransactionResultCode;

        let cases: &[(TransactionResultResult, i64, TransactionResultCode)] = &[
            (
                TransactionResultResult::TxInternalError,
                0,
                TransactionResultCode::TxInternalError,
            ),
            (
                TransactionResultResult::TxBadAuth,
                0,
                TransactionResultCode::TxBadAuth,
            ),
            (
                TransactionResultResult::TxInsufficientFee,
                0,
                TransactionResultCode::TxInsufficientFee,
            ),
            (
                TransactionResultResult::TxFailed(Default::default()),
                100,
                TransactionResultCode::TxFailed,
            ),
        ];

        for (result_variant, fee, expected_code) in cases {
            let b64 = encode_tx_result(result_variant.clone(), *fee)
                .unwrap_or_else(|e| panic!("encoding {:?} should succeed: {}", expected_code, e));

            let bytes = BASE64
                .decode(&b64)
                .unwrap_or_else(|e| panic!("base64 decode for {:?} failed: {}", expected_code, e));

            let parsed = TransactionResult::from_xdr(&bytes, Limits::none())
                .unwrap_or_else(|e| panic!("XDR parse for {:?} failed: {}", expected_code, e));

            assert_eq!(
                parsed.fee_charged, *fee,
                "fee_charged mismatch for {:?}",
                expected_code
            );
            assert_eq!(
                parsed.result.discriminant(),
                *expected_code,
                "result code mismatch"
            );
        }
    }
}
