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
    let hash = util::require_str(&params, "hash")?;

    let format = util::parse_format(&params)?;

    let lctx = util::LedgerContext::from_app(ctx).await?;

    // Look up the transaction and its close time in a single blocking DB call.
    // If the header was pruned (require_close_times fails), check whether the
    // tx is below the retention boundary (NOT_FOUND) or genuinely missing
    // (propagate integrity error).
    let hash_owned = hash.to_string();
    let tx_record_with_time = util::blocking_db(ctx, move |db| {
        db.with_connection(|conn| {
            use henyey_db::{HistoryQueries, LedgerQueries};
            let record = conn.load_transaction(&hash_owned)?;
            match record {
                Some(record) => {
                    match conn.require_close_times(&[record.ledger_seq]) {
                        Ok(close_times) => {
                            let close_time = close_times[&record.ledger_seq];
                            Ok(Some((record, close_time)))
                        }
                        Err(henyey_db::DbError::Integrity(_)) => {
                            // Header pruned — check if below retention boundary
                            let oldest = conn.get_oldest_ledger_seq()?.unwrap_or(0);
                            if record.ledger_seq < oldest {
                                Ok(None) // stale tx, treat as NOT_FOUND
                            } else {
                                Err(henyey_db::DbError::Integrity(format!(
                                    "missing close time for retained ledger {}",
                                    record.ledger_seq
                                )))
                            }
                        }
                        Err(e) => Err(e),
                    }
                }
                None => Ok(None),
            }
        })
    })
    .await
    .map_err(|e| {
        tracing::warn!(error = ?e, "get_transaction DB error");
        JsonRpcError::internal("database error")
    })?;

    match tx_record_with_time {
        Some((record, close_time)) => {
            let created_at = close_time.to_string();

            let mut obj = super::transaction_response::build_transaction_object(
                &record,
                json!(created_at),
                format,
                false,
            )?;
            lctx.insert_json_fields(&mut obj);

            Ok(serde_json::Value::Object(obj))
        }
        None => {
            let mut result = json!({ "status": "NOT_FOUND" });
            lctx.insert_json_fields(result.as_object_mut().unwrap());
            Ok(result)
        }
    }
}
