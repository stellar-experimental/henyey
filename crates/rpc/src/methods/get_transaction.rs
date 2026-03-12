use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde_json::json;

use crate::context::RpcContext;
use crate::error::JsonRpcError;

pub async fn handle(
    ctx: &Arc<RpcContext>,
    params: serde_json::Value,
) -> Result<serde_json::Value, JsonRpcError> {
    let hash = params
        .get("hash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| JsonRpcError::invalid_params("missing 'hash' parameter"))?;

    let ledger = ctx.app.ledger_summary();

    // Look up the transaction in the database
    let tx_record = ctx
        .app
        .database()
        .with_connection(|conn| {
            use henyey_db::HistoryQueries;
            conn.load_transaction(hash)
        })
        .map_err(|e| JsonRpcError::internal(format!("database error: {}", e)))?;

    match tx_record {
        Some(record) => {
            let envelope_xdr = BASE64.encode(&record.body);
            // DB stores TransactionResultPair; extract just TransactionResult for the API
            let result_xdr = match extract_result_xdr(&record.result) {
                Some(result_bytes) => BASE64.encode(&result_bytes),
                None => BASE64.encode(&record.result), // fallback
            };
            let result_meta_xdr = record
                .meta
                .as_ref()
                .map(|m| BASE64.encode(m))
                .unwrap_or_default();

            // Look up the ledger close time
            let created_at = ctx
                .app
                .database()
                .with_connection(|conn| {
                    use henyey_db::LedgerQueries;
                    conn.load_ledger_header(record.ledger_seq)
                })
                .ok()
                .flatten()
                .map(|h| h.scp_value.close_time.0.to_string())
                .unwrap_or_else(|| "0".to_string());

            // Determine status from the result XDR
            let status = determine_tx_status(&record.result);

            Ok(json!({
                "status": status,
                "latestLedger": ledger.num,
                "latestLedgerCloseTime": ledger.close_time.to_string(),
                "oldestLedger": 1,
                "oldestLedgerCloseTime": "0",
                "ledger": record.ledger_seq,
                "createdAt": created_at,
                "applicationOrder": record.tx_index + 1,
                "envelopeXdr": envelope_xdr,
                "resultXdr": result_xdr,
                "resultMetaXdr": result_meta_xdr
            }))
        }
        None => Ok(json!({
            "status": "NOT_FOUND",
            "latestLedger": ledger.num,
            "latestLedgerCloseTime": ledger.close_time.to_string(),
            "oldestLedger": 1,
            "oldestLedgerCloseTime": "0"
        })),
    }
}

fn determine_tx_status(result_bytes: &[u8]) -> &'static str {
    use stellar_xdr::curr::{Limits, ReadXdr, TransactionResultPair, TransactionResultCode};

    // The database stores TransactionResultPair (hash + result)
    match TransactionResultPair::from_xdr(result_bytes, Limits::none()) {
        Ok(pair) => {
            let code = pair.result.result.discriminant();
            if code == TransactionResultCode::TxSuccess
                || code == TransactionResultCode::TxFeeBumpInnerSuccess
            {
                "SUCCESS"
            } else {
                "FAILED"
            }
        }
        Err(_) => "FAILED",
    }
}

/// Extract just the TransactionResult XDR from the stored TransactionResultPair bytes.
fn extract_result_xdr(result_pair_bytes: &[u8]) -> Option<Vec<u8>> {
    use stellar_xdr::curr::{Limits, ReadXdr, TransactionResultPair, WriteXdr};

    let pair = TransactionResultPair::from_xdr(result_pair_bytes, Limits::none()).ok()?;
    pair.result.to_xdr(Limits::none()).ok()
}
