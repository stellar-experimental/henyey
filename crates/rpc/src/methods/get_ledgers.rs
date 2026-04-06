//! Handler for the `getLedgers` JSON-RPC method.
//!
//! Returns a paginated list of ledger metadata within a ledger range.
//! Supports cursor-based pagination using ledger sequence numbers.

use std::sync::Arc;

use serde_json::json;
use stellar_xdr::curr::{LedgerCloseMeta, Limits, ReadXdr};

use crate::context::RpcContext;
use crate::error::JsonRpcError;
use crate::util;

/// Default number of ledgers returned per query.
const DEFAULT_LEDGER_LIMIT: u32 = 5;
/// Maximum number of ledgers that can be requested in a single query.
const MAX_LEDGER_LIMIT: u32 = 200;

pub async fn handle(
    ctx: &Arc<RpcContext>,
    params: serde_json::Value,
) -> Result<serde_json::Value, JsonRpcError> {
    let format = util::parse_format(&params)?;

    let lctx = util::LedgerContext::from_app(&ctx.app);

    // Parse parameters
    let start_ledger = params
        .get("startLedger")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);

    let pagination = params.get("pagination");
    let cursor_str = pagination
        .and_then(|p| p.get("cursor"))
        .and_then(|v| v.as_str());
    let limit = pagination
        .and_then(|p| p.get("limit"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);

    // Validate and resolve pagination.
    // getLedgers cursor is a plain ledger sequence (not a TOID).
    let (effective_start, effective_limit) = validate_ledger_pagination(
        start_ledger,
        cursor_str,
        limit,
        lctx.oldest_ledger,
        lctx.latest_ledger,
    )?;

    // End ledger for query: latest + 1 (exclusive upper bound)
    let end_ledger = lctx.latest_ledger + 1;

    // Query ledger close metas from database
    let metas = ctx
        .app
        .database()
        .with_connection(|conn| {
            use henyey_db::LedgerCloseMetaQueries;
            conn.load_ledger_close_metas_in_range(effective_start, end_ledger, effective_limit)
        })
        .map_err(|e| JsonRpcError::internal(format!("database error: {}", e)))?;

    // Build response
    let mut ledgers = Vec::with_capacity(metas.len());
    let mut last_cursor = String::new();

    for (sequence, meta_bytes) in &metas {
        let lcm = LedgerCloseMeta::from_xdr(meta_bytes.as_slice(), Limits::none())
            .map_err(|e| JsonRpcError::internal(format!("corrupt LedgerCloseMeta: {e}")))?;

        let header_entry = util::ledger_header_entry(&lcm);
        let hash = hex::encode(header_entry.hash.0);
        let close_time = header_entry.header.scp_value.close_time.0;

        last_cursor = sequence.to_string();

        let mut obj = serde_json::Map::new();
        obj.insert("hash".into(), json!(hash));
        obj.insert("sequence".into(), json!(sequence));
        obj.insert("ledgerCloseTime".into(), json!(close_time.to_string()));

        // Header XDR — encode the LedgerHeaderHistoryEntry
        util::insert_xdr_field(&mut obj, "header", header_entry, format)?;

        // Metadata XDR — encode the full LedgerCloseMeta
        util::insert_raw_xdr_field::<LedgerCloseMeta>(&mut obj, "metadata", meta_bytes, format)?;

        ledgers.push(serde_json::Value::Object(obj));
    }

    let mut result = json!({
        "ledgers": ledgers,
        "cursor": last_cursor
    });
    lctx.insert_json_fields(result.as_object_mut().unwrap());
    Ok(result)
}

/// Validate getLedgers pagination. Cursor is a plain ledger sequence.
fn validate_ledger_pagination(
    start_ledger: Option<u32>,
    cursor: Option<&str>,
    limit: Option<u32>,
    oldest_ledger: u32,
    latest_ledger: u32,
) -> Result<(u32, u32), JsonRpcError> {
    if cursor.is_some() && start_ledger.is_some() {
        return Err(JsonRpcError::invalid_params(
            "startLedger and cursor are mutually exclusive",
        ));
    }

    let limit = limit
        .unwrap_or(DEFAULT_LEDGER_LIMIT)
        .clamp(1, MAX_LEDGER_LIMIT);

    if let Some(c) = cursor {
        if c.is_empty() {
            return Err(JsonRpcError::invalid_params("cursor must not be empty"));
        }
        let seq: u32 = c
            .parse()
            .map_err(|_| JsonRpcError::invalid_params(format!("invalid cursor: {c}")))?;
        // Start from cursor + 1 (cursor is the last seen ledger)
        return Ok((seq + 1, limit));
    }

    let start = start_ledger
        .ok_or_else(|| JsonRpcError::invalid_params("startLedger or cursor is required"))?;

    if start < oldest_ledger || start > latest_ledger {
        return Err(JsonRpcError::invalid_params(format!(
            "startLedger must be within [{oldest_ledger}, {latest_ledger}]"
        )));
    }

    Ok((start, limit))
}
