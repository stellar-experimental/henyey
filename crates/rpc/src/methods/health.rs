use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;

use crate::context::RpcContext;
use crate::error::JsonRpcError;
use crate::util;

pub(crate) async fn handle(ctx: &Arc<RpcContext>) -> Result<serde_json::Value, JsonRpcError> {
    let ledger = ctx.app.ledger_summary();
    let rpc_config = &ctx.app.config().rpc;

    let oldest_ledger = util::blocking_db(ctx, |db| {
        db.with_connection(|conn| {
            use henyey_db::LedgerQueries;
            Ok(conn.get_oldest_ledger_seq()?.unwrap_or(1))
        })
    })
    .await
    .unwrap_or_else(|e| {
        tracing::warn!(error = ?e, "health check DB error");
        1
    });

    // Determine health status based on ledger age
    let max_latency = rpc_config.max_healthy_ledger_latency_secs;
    let status = if max_latency > 0 && ledger.close_time > 0 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let age = now.saturating_sub(ledger.close_time);
        if age > max_latency {
            "unhealthy"
        } else {
            "healthy"
        }
    } else {
        // Latency check disabled or no ledger close time available
        "healthy"
    };

    Ok(json!({
        "status": status,
        "latestLedger": ledger.num,
        "oldestLedger": oldest_ledger,
        "ledgerRetentionWindow": rpc_config.retention_window
    }))
}
