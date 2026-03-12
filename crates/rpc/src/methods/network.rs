use std::sync::Arc;

use serde_json::json;

use crate::context::RpcContext;
use crate::error::JsonRpcError;

pub async fn handle(ctx: &Arc<RpcContext>) -> Result<serde_json::Value, JsonRpcError> {
    let info = ctx.app.info();

    // Provide the friendbot URL for testnet
    let friendbot_url = if info.network_passphrase == "Test SDF Network ; September 2015" {
        "https://friendbot.stellar.org/"
    } else {
        ""
    };

    Ok(json!({
        "friendbotUrl": friendbot_url,
        "passphrase": info.network_passphrase,
        "protocolVersion": ctx.app.ledger_summary().version
    }))
}
