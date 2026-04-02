use serde_json::json;
use stellar_xdr::curr::{TransactionEnvelope, TransactionMeta, TransactionResult};

use crate::error::JsonRpcError;
use crate::util::{self, XdrFormat};

pub(super) fn build_transaction_object(
    record: &henyey_db::TxRecord,
    created_at: serde_json::Value,
    format: XdrFormat,
    include_tx_hash: bool,
) -> Result<serde_json::Map<String, serde_json::Value>, JsonRpcError> {
    let status = util::determine_tx_status(&record.result);
    let fee_bump = util::is_fee_bump_envelope(&record.body);
    let application_order = record.tx_index + 1;

    let mut obj = serde_json::Map::new();
    obj.insert("status".into(), json!(status));
    obj.insert("applicationOrder".into(), json!(application_order));
    obj.insert("feeBump".into(), json!(fee_bump));
    obj.insert("ledger".into(), json!(record.ledger_seq));
    obj.insert("createdAt".into(), created_at);

    if include_tx_hash {
        obj.insert("txHash".into(), json!(record.tx_id));
    }

    insert_transaction_xdr_fields(&mut obj, record, format)?;
    Ok(obj)
}

fn insert_transaction_xdr_fields(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    record: &henyey_db::TxRecord,
    format: XdrFormat,
) -> Result<(), JsonRpcError> {
    util::insert_raw_xdr_field::<TransactionEnvelope>(obj, "envelope", &record.body, format)?;

    let result_bytes =
        util::extract_result_xdr(&record.result).unwrap_or_else(|| record.result.clone());
    util::insert_raw_xdr_field::<TransactionResult>(obj, "result", &result_bytes, format)?;

    if let Some(ref meta_bytes) = record.meta {
        util::insert_raw_xdr_field::<TransactionMeta>(obj, "resultMeta", meta_bytes, format)?;
        util::insert_diagnostic_events(obj, meta_bytes, format)?;
    }

    Ok(())
}
