//! Handler for the `getTransaction` JSON-RPC method.

use std::sync::Arc;

use serde_json::json;

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

            let mut obj = super::transaction_response::build_transaction_object(
                &record,
                json!(created_at),
                format,
                false,
            )?;
            obj.insert("latestLedger".into(), json!(ledger.num));
            obj.insert(
                "latestLedgerCloseTime".into(),
                json!(ledger.close_time.to_string()),
            );
            obj.insert("oldestLedger".into(), json!(oldest));
            obj.insert("oldestLedgerCloseTime".into(), json!(oldest_close_time));

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
