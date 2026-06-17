//! Handler for the `getFeeStats` JSON-RPC method.
//!
//! Returns fee distribution statistics for both classic and Soroban transactions,
//! computed over the sliding fee window (default last 2880 ledgers).

use std::sync::Arc;

use serde_json::json;

use crate::context::RpcContext;
use crate::error::JsonRpcError;
use crate::fee_window::FeeDistribution;

pub(crate) async fn handle(ctx: &Arc<RpcContext>) -> Result<serde_json::Value, JsonRpcError> {
    let ledger = ctx.app.ledger_summary();

    let classic = ctx.fee_windows.get_classic_distribution();
    let soroban = ctx.fee_windows.get_soroban_distribution();

    Ok(json!({
        "sorobanInclusionFee": distribution_to_json(&soroban),
        "inclusionFee": distribution_to_json(&classic),
        "latestLedger": ledger.num
    }))
}

/// Convert a `FeeDistribution` into the JSON format expected by the RPC API.
///
/// All numeric values are serialized as strings to match upstream behavior.
fn distribution_to_json(d: &FeeDistribution) -> serde_json::Value {
    json!({
        "max": d.max.to_string(),
        "min": d.min.to_string(),
        "mode": d.mode.to_string(),
        "p10": d.p10.to_string(),
        "p20": d.p20.to_string(),
        "p30": d.p30.to_string(),
        "p40": d.p40.to_string(),
        "p50": d.p50.to_string(),
        "p60": d.p60.to_string(),
        "p70": d.p70.to_string(),
        "p80": d.p80.to_string(),
        "p90": d.p90.to_string(),
        "p95": d.p95.to_string(),
        "p99": d.p99.to_string(),
        "transactionCount": d.fee_count.to_string(),
        "ledgerCount": d.ledger_count
    })
}
