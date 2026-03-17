use std::sync::Arc;

use crate::context::RpcContext;
use crate::error::JsonRpcError;
use crate::methods;
use crate::types::JsonRpcResponse;

/// Dispatch a JSON-RPC method call to the appropriate handler.
pub async fn dispatch(
    ctx: &Arc<RpcContext>,
    method: &str,
    id: serde_json::Value,
    params: serde_json::Value,
) -> JsonRpcResponse {
    let result = match method {
        "getHealth" => methods::health::handle(ctx).await,
        "getNetwork" => methods::network::handle(ctx).await,
        "getLatestLedger" => methods::latest_ledger::handle(ctx).await,
        "getVersionInfo" => methods::version_info::handle(ctx).await,
        "getFeeStats" => methods::fee_stats::handle(ctx).await,
        "getLedgerEntries" => methods::get_ledger_entries::handle(ctx, params).await,
        "getTransaction" => methods::get_transaction::handle(ctx, params).await,
        "getTransactions" => methods::get_transactions::handle(ctx, params).await,
        "getLedgers" => methods::get_ledgers::handle(ctx, params).await,
        "getEvents" => methods::get_events::handle(ctx, params).await,
        "sendTransaction" => methods::send_transaction::handle(ctx, params).await,
        "simulateTransaction" => crate::simulate::handle(ctx, params).await,
        _ => Err(JsonRpcError::method_not_found(method)),
    };

    match result {
        Ok(value) => JsonRpcResponse::success(id, value),
        Err(err) => JsonRpcResponse::error(id, err),
    }
}
