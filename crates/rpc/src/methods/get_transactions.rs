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

    let lctx = util::LedgerContext::from_app(ctx).await?;

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

    // Query transactions and batch-load close times in a single blocking DB call.
    // On pruning race (require_close_times fails), fall back to batch_close_times
    // and skip records with missing headers rather than returning a 500.
    let tx_load_budget = ctx.app.config().rpc.max_tx_load_bytes;
    let tx_query_ops = ctx.app.config().rpc.max_tx_query_ops;
    let (records, close_time_cache) = util::blocking_db(ctx, move |db| {
        db.with_connection(|conn| {
            use henyey_db::{HistoryQueries, LedgerQueries};
            let records = conn.load_transactions_in_range(
                query_start_ledger,
                query_start_tx_index,
                end_ledger,
                effective_limit,
                status_filter,
                tx_load_budget,
                tx_query_ops,
            )?;
            let seqs: Vec<u32> = records
                .iter()
                .map(|r| r.ledger_seq)
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            let close_times = match conn.require_close_times(&seqs) {
                Ok(ct) => ct,
                Err(henyey_db::DbError::Integrity(msg)) => {
                    tracing::warn!(
                        error = %msg,
                        "pruning race in getTransactions; falling back to partial close times"
                    );
                    conn.batch_close_times(&seqs)?
                }
                Err(e) => return Err(e),
            };
            Ok((records, close_times))
        })
    })
    .await
    .map_err(|e| {
        use crate::util::DbAccessError;
        if let DbAccessError::Db(henyey_db::DbError::QueryBudgetExceeded) = &e {
            return JsonRpcError::query_budget_exceeded(
                "query exceeded computational budget; narrow your filter criteria \
                 or reduce the ledger range",
            );
        }
        tracing::warn!(error = ?e, "get_transactions DB error");
        JsonRpcError::internal("database error")
    })?;

    // Build response using pre-fetched close times
    let mut transactions = Vec::with_capacity(records.len());

    for record in &records {
        // Ledger close time — returned as a number (not string) per upstream getTransactions.
        // Skip records whose header was pruned during a maintenance race.
        let Some(created_at) = close_time_cache.get(&record.ledger_seq).copied() else {
            tracing::warn!(
                ledger_seq = record.ledger_seq,
                "skipping transaction with pruned header in getTransactions"
            );
            continue;
        };

        let obj = super::transaction_response::build_transaction_object(
            record,
            json!(created_at),
            format,
            true,
        )?;
        transactions.push(serde_json::Value::Object(obj));
    }

    // Cursor advances past ALL queried records (including skipped ones) to
    // guarantee forward progress even when headers are pruned mid-page.
    let last_cursor = records
        .last()
        .map(|r| {
            let app_order = r.tx_index + 1;
            util::toid_encode(r.ledger_seq, app_order, 0).to_string()
        })
        .unwrap_or_default();

    let mut result = serde_json::json!({
        "transactions": transactions,
        "cursor": last_cursor
    });
    lctx.insert_json_fields(result.as_object_mut().unwrap());
    Ok(result)
}
