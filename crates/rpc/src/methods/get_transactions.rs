use std::sync::Arc;

use serde_json::json;

use crate::context::RpcContext;
use crate::error::JsonRpcError;

pub async fn handle(
    ctx: &Arc<RpcContext>,
    _params: serde_json::Value,
) -> Result<serde_json::Value, JsonRpcError> {
    let ledger = ctx.app.ledger_summary();

    // getTransactions range query is not yet fully implemented.
    Ok(json!({
        "transactions": [],
        "latestLedger": ledger.num,
        "cursor": ""
    }))
}
