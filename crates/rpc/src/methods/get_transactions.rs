//! Handler for the `getTransactions` JSON-RPC method.
//!
//! Returns a paginated list of transactions within a ledger range.
//! Supports cursor-based pagination using TOID (Total Order ID) values.

use std::sync::Arc;

use serde_json::json;

use crate::context::RpcContext;
use crate::error::JsonRpcError;
use crate::util;

/// Default number of transactions returned per query.
const DEFAULT_TX_LIMIT: u32 = 10;
/// Maximum number of transactions that can be requested in a single query.
const MAX_TX_LIMIT: u32 = 200;

// SECURITY: request body bounded by HTTP framework body size limit; serde rejects invalid types
pub async fn handle(
    ctx: &Arc<RpcContext>,
    params: serde_json::Value,
) -> Result<serde_json::Value, JsonRpcError> {
    let format = util::parse_format(&params)?;

    let lctx = util::LedgerContext::from_app(&ctx.app);

    // Parse optional status filter
    let status_filter = match params.get("status").and_then(|v| v.as_str()) {
        Some("SUCCESS") => Some(henyey_db::TxStatus::Success),
        Some("FAILED") => Some(henyey_db::TxStatus::Failed),
        Some(other) => {
            return Err(JsonRpcError::invalid_params(format!(
                "invalid status filter: '{}' (allowed: SUCCESS, FAILED)",
                other
            )));
        }
        None => None,
    };

    // Parse parameters
    let start_ledger = params
        .get("startLedger")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);

    let pagination = params.get("pagination");
    let cursor = pagination
        .and_then(|p| p.get("cursor"))
        .and_then(|v| v.as_str());
    let limit = pagination
        .and_then(|p| p.get("limit"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);

    // Validate pagination
    let (effective_start, effective_cursor, effective_limit) = util::validate_pagination(
        start_ledger,
        cursor,
        limit,
        DEFAULT_TX_LIMIT,
        MAX_TX_LIMIT,
        &lctx,
    )?;

    // Convert cursor to (start_ledger, start_tx_index) for the DB query
    let (query_start_ledger, query_start_tx_index) = match effective_cursor {
        Some(toid) => {
            let (l, tx_order, _) = util::toid_decode(toid);
            // tx_order in TOID is 1-based application order; DB txindex is 0-based
            (l, Some(tx_order.saturating_sub(1)))
        }
        None => (effective_start, None),
    };

    // End ledger for query: latest + 1 (exclusive upper bound)
    let end_ledger = lctx.latest_ledger + 1;

    // Query transactions from database
    let records = ctx
        .app
        .database()
        .with_connection(|conn| {
            use henyey_db::HistoryQueries;
            conn.load_transactions_in_range(
                query_start_ledger,
                query_start_tx_index,
                end_ledger,
                effective_limit,
                status_filter,
            )
        })
        .map_err(|e| JsonRpcError::internal(format!("database error: {}", e)))?;

    // Build response, caching ledger close times per ledger_seq to avoid N+1 queries
    let mut transactions = Vec::with_capacity(records.len());
    let mut last_cursor = String::new();
    let mut close_time_cache: std::collections::HashMap<u32, u64> =
        std::collections::HashMap::new();

    for record in &records {
        // Application order is 1-based (txindex is 0-based in DB)
        let application_order = record.tx_index + 1;

        // Ledger close time — returned as a number (not string) per upstream getTransactions
        let created_at = *close_time_cache
            .entry(record.ledger_seq)
            .or_insert_with(|| util::ledger_close_time(&ctx.app, record.ledger_seq));

        // Build TOID cursor for this transaction
        let toid = util::toid_encode(record.ledger_seq, application_order, 0);
        last_cursor = toid.to_string();

        let obj = super::transaction_response::build_transaction_object(
            record,
            json!(created_at),
            format,
            true,
        )?;
        transactions.push(serde_json::Value::Object(obj));
    }

    let mut result = serde_json::json!({
        "transactions": transactions,
        "cursor": last_cursor
    });
    lctx.insert_json_fields(result.as_object_mut().unwrap());
    Ok(result)
}
