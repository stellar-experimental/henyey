use std::sync::Arc;

use serde_json::json;

use crate::context::RpcContext;
use crate::error::JsonRpcError;

pub async fn handle(ctx: &Arc<RpcContext>) -> Result<serde_json::Value, JsonRpcError> {
    let ledger = ctx.app.ledger_summary();
    let hash = ledger.hash.to_hex();

    Ok(json!({
        "id": hash,
        "protocolVersion": ledger.version,
        "sequence": ledger.num
    }))
}
