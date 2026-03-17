//! Handler for the `getTransaction` JSON-RPC method.

use std::sync::Arc;

use serde_json::json;
use stellar_xdr::curr::{TransactionEnvelope, TransactionMeta, TransactionResult};

use crate::context::RpcContext;
use crate::error::JsonRpcError;
use crate::util;

pub async fn handle(
    ctx: &Arc<RpcContext>,
    params: serde_json::Value,
) -> Result<serde_json::Value, JsonRpcError> {
    let hash = params
        .get("hash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| JsonRpcError::invalid_params("missing 'hash' parameter"))?;

    let format = util::parse_format(&params)?;

    let ledger = ctx.app.ledger_summary();
    let oldest = util::oldest_ledger(&ctx.app);

    // Look up the oldest ledger close time
    let oldest_close_time = util::ledger_close_time(&ctx.app, oldest).to_string();

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
            // Look up the ledger close time
            let created_at = util::ledger_close_time(&ctx.app, record.ledger_seq).to_string();

            // Determine status from the result XDR
            let status = util::determine_tx_status(&record.result);

            // Detect fee bump from the envelope
            let fee_bump = util::is_fee_bump_envelope(&record.body);

            let mut obj = serde_json::Map::new();
            obj.insert("status".into(), json!(status));
            obj.insert("latestLedger".into(), json!(ledger.num));
            obj.insert("latestLedgerCloseTime".into(), json!(ledger.close_time.to_string()));
            obj.insert("oldestLedger".into(), json!(oldest));
            obj.insert("oldestLedgerCloseTime".into(), json!(oldest_close_time));
            obj.insert("ledger".into(), json!(record.ledger_seq));
            obj.insert("createdAt".into(), json!(created_at));
            obj.insert("applicationOrder".into(), json!(record.tx_index + 1));
            obj.insert("feeBump".into(), json!(fee_bump));

            // Envelope XDR
            util::insert_raw_xdr_field::<TransactionEnvelope>(
                &mut obj, "envelope", &record.body, format,
            )?;

            // Result XDR — extract TransactionResult from the stored TransactionResultPair
            let result_bytes = util::extract_result_xdr(&record.result)
                .unwrap_or_else(|| record.result.clone());
            util::insert_raw_xdr_field::<TransactionResult>(
                &mut obj, "result", &result_bytes, format,
            )?;

            // Result meta XDR
            if let Some(ref meta_bytes) = record.meta {
                util::insert_raw_xdr_field::<TransactionMeta>(
                    &mut obj, "resultMeta", meta_bytes, format,
                )?;
            }

            // Diagnostic events
            if let Some(ref meta_bytes) = record.meta {
                util::insert_diagnostic_events(&mut obj, meta_bytes, format)?;
            }

            Ok(serde_json::Value::Object(obj))
        }
        None => Ok(json!({
            "status": "NOT_FOUND",
            "latestLedger": ledger.num,
            "latestLedgerCloseTime": ledger.close_time.to_string(),
            "oldestLedger": oldest,
            "oldestLedgerCloseTime": oldest_close_time
        })),
    }
}
