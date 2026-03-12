use std::sync::Arc;

use serde_json::json;

use crate::context::RpcContext;
use crate::error::JsonRpcError;

pub async fn handle(ctx: &Arc<RpcContext>) -> Result<serde_json::Value, JsonRpcError> {
    let info = ctx.app.info();
    let ledger = ctx.app.ledger_summary();

    Ok(json!({
        "version": info.version,
        "commitHash": "",
        "buildTimestamp": "",
        "captiveCoreVersion": "",
        "protocolVersion": ledger.version
    }))
}
