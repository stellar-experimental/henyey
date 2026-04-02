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

pub async fn handle(
    ctx: &Arc<RpcContext>,
    params: serde_json::Value,
) -> Result<serde_json::Value, JsonRpcError> {
    let format = util::parse_format(&params)?;

    let ledger = ctx.app.ledger_summary();
    let oldest = util::oldest_ledger(&ctx.app);

    // Look up oldest ledger close time
    let oldest_close_time = util::ledger_close_time(&ctx.app, oldest).to_string();

    // Parse optional status filter
    let status_filter = match params.get("status").and_then(|v| v.as_str()) {
        Some("SUCCESS") => Some(henyey_db::TX_STATUS_SUCCESS),
        Some("FAILED") => Some(henyey_db::TX_STATUS_FAILED),
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
        oldest,
        ledger.num,
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
    let end_ledger = ledger.num + 1;

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

    // Build response
    let mut transactions = Vec::with_capacity(records.len());
    let mut last_cursor = String::new();

    for record in &records {
        // Application order is 1-based (txindex is 0-based in DB)
        let application_order = record.tx_index + 1;

        // Ledger close time — returned as a number (not string) per upstream getTransactions
        let created_at = util::ledger_close_time(&ctx.app, record.ledger_seq);

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

    Ok(json!({
        "transactions": transactions,
        "latestLedger": ledger.num,
        "latestLedgerCloseTime": ledger.close_time.to_string(),
        "oldestLedger": oldest,
        "oldestLedgerCloseTime": oldest_close_time,
        "cursor": last_cursor
    }))
}
