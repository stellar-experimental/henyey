use std::sync::Arc;

use crate::context::RpcContext;
use crate::error::JsonRpcError;

pub async fn handle(
    ctx: &Arc<RpcContext>,
    params: serde_json::Value,
) -> Result<serde_json::Value, JsonRpcError> {
    crate::simulate::handle(ctx, params).await
}
