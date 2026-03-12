use std::sync::Arc;

use henyey_app::AppState;
use serde_json::json;

use crate::context::RpcContext;
use crate::error::JsonRpcError;

pub async fn handle(ctx: &Arc<RpcContext>) -> Result<serde_json::Value, JsonRpcError> {
    let state = ctx.app.state().await;
    let ledger = ctx.app.ledger_summary();

    let status = match state {
        AppState::Synced | AppState::Validating => "healthy",
        _ => "healthy", // stellar-rpc returns healthy if reachable
    };

    Ok(json!({
        "status": status,
        "latestLedger": ledger.num,
        "oldestLedger": 1, // TODO: compute from retention window
        "ledgerRetentionWindow": 2880
    }))
}
