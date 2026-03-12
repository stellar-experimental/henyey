use std::sync::Arc;

use serde_json::json;

use crate::context::RpcContext;
use crate::error::JsonRpcError;

pub async fn handle(ctx: &Arc<RpcContext>) -> Result<serde_json::Value, JsonRpcError> {
    let ledger = ctx.app.ledger_summary();
    let base_fee = ledger.base_fee.to_string();

    // Return fee stats with the base fee as all percentiles.
    // A more complete implementation would track rolling stats from recent ledgers.
    let fee_distribution = json!({
        "max": &base_fee,
        "min": &base_fee,
        "mode": &base_fee,
        "p10": &base_fee,
        "p20": &base_fee,
        "p30": &base_fee,
        "p40": &base_fee,
        "p50": &base_fee,
        "p60": &base_fee,
        "p70": &base_fee,
        "p80": &base_fee,
        "p90": &base_fee,
        "p95": &base_fee,
        "p99": &base_fee,
        "transactionCount": "0",
        "ledgerCount": 1
    });

    Ok(json!({
        "sorobanInclusionFee": fee_distribution,
        "inclusionFee": fee_distribution,
        "latestLedger": ledger.num
    }))
}
