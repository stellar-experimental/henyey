use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde_json::json;
use stellar_xdr::curr::{Limits, WriteXdr};

use crate::context::RpcContext;
use crate::error::JsonRpcError;
use crate::util;

/// Handle the `getLatestLedger` JSON-RPC method.
///
/// # Consistency guarantees
///
/// All in-memory response fields (`id`, `protocolVersion`, `sequence`,
/// `closeTime`, `headerXdr`) are derived from a single atomic
/// [`HeaderSnapshot`](henyey_ledger::HeaderSnapshot) and are guaranteed
/// to describe the same ledger close.
///
/// `metadataXdr` is loaded from the database and is **best-effort**: after a
/// ledger close the async persist job may not have written the
/// `LedgerCloseMeta` yet, so the field may be an empty string. When present,
/// the blob is validated against the expected sequence number via
/// [`parse_ledger_close_meta_checked`](util::parse_ledger_close_meta_checked).
pub(crate) async fn handle(ctx: &Arc<RpcContext>) -> Result<serde_json::Value, JsonRpcError> {
    // Read all in-memory fields from a single atomic snapshot so they cannot
    // straddle a ledger close boundary.
    let snap = ctx.app.ledger_snapshot();
    let ledger_num = snap.header.ledger_seq;
    let hash = snap.hash.to_hex();
    let close_time = snap.header.scp_value.close_time.0;

    // Encode the LedgerHeader as base64 XDR.
    let header_xdr = snap
        .header
        .to_xdr(Limits::none())
        .map(|b| BASE64.encode(&b))
        .map_err(|e| JsonRpcError::internal_logged("XDR data integrity error", &e))?;

    // Load full LedgerCloseMeta from the database for metadataXdr.
    // Best-effort: may return None if the async persist job hasn't run yet.
    let metadata_xdr = util::blocking_db(ctx, move |db| {
        db.with_connection(|conn| {
            use henyey_db::LedgerCloseMetaQueries;
            conn.load_ledger_close_meta(ledger_num)
        })
    })
    .await
    .map_err(|e| {
        tracing::warn!(error = ?e, "getLatestLedger DB error");
        JsonRpcError::internal("database error")
    })?
    .map(|meta_bytes| {
        // Validate the stored bytes (parse + sequence check) before returning.
        util::parse_ledger_close_meta_checked(ledger_num, &meta_bytes)?;
        Ok::<_, JsonRpcError>(BASE64.encode(&meta_bytes))
    })
    .transpose()?
    .unwrap_or_default();

    Ok(json!({
        "id": hash,
        "protocolVersion": snap.header.ledger_version,
        "sequence": ledger_num,
        "closeTime": close_time.to_string(),
        "headerXdr": header_xdr,
        "metadataXdr": metadata_xdr
    }))
}
