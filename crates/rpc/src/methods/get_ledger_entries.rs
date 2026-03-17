//! Handler for the `getLedgerEntries` JSON-RPC method.

use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde_json::json;
use stellar_xdr::curr::{LedgerEntry, LedgerKey, Limits, ReadXdr, WriteXdr};

use crate::context::RpcContext;
use crate::error::JsonRpcError;
use crate::util::{self, XdrFormat, ttl_key_for_ledger_key};

/// Maximum number of ledger entry keys allowed per request.
const MAX_KEYS: usize = 200;

pub async fn handle(
    ctx: &Arc<RpcContext>,
    params: serde_json::Value,
) -> Result<serde_json::Value, JsonRpcError> {
    let format = util::parse_format(&params)?;

    let keys_array = params
        .get("keys")
        .and_then(|v| v.as_array())
        .ok_or_else(|| JsonRpcError::invalid_params("missing or invalid 'keys' parameter"))?;

    if keys_array.is_empty() {
        return Err(JsonRpcError::invalid_params("'keys' must not be empty"));
    }

    if keys_array.len() > MAX_KEYS {
        return Err(JsonRpcError::invalid_params(
            format!("too many keys: max {} allowed", MAX_KEYS),
        ));
    }

    // Decode base64 XDR keys, keeping the original base64 strings
    let mut ledger_keys = Vec::with_capacity(keys_array.len());
    for (i, key_val) in keys_array.iter().enumerate() {
        let key_str = key_val
            .as_str()
            .ok_or_else(|| JsonRpcError::invalid_params(format!("keys[{}] must be a string", i)))?;
        let key_bytes = BASE64
            .decode(key_str)
            .map_err(|e| JsonRpcError::invalid_params(format!("keys[{}]: invalid base64: {}", i, e)))?;
        let key = LedgerKey::from_xdr(&key_bytes, Limits::none())
            .map_err(|e| JsonRpcError::invalid_params(format!("keys[{}]: invalid XDR: {}", i, e)))?;
        ledger_keys.push((key_str.to_string(), key));
    }

    // Get bucket list snapshot
    let snapshot = ctx
        .app
        .bucket_snapshot_manager()
        .copy_searchable_live_snapshot()
        .ok_or_else(|| JsonRpcError::internal("bucket list snapshot not available"))?;

    let ledger_seq = snapshot.ledger_seq();

    // Look up each key individually to preserve key-entry mapping
    let mut result_entries = Vec::new();
    for (key_b64, key) in &ledger_keys {
        if let Some(entry) = snapshot.load(key) {
            let mut obj = serde_json::Map::new();

            // Key — upstream uses "key" for base64, "keyJson" for JSON
            match format {
                XdrFormat::Base64 => {
                    obj.insert("key".into(), json!(key_b64));
                }
                XdrFormat::Json => {
                    util::insert_xdr_field(&mut obj, "key", key, format)?;
                }
            }

            // Data XDR — upstream uses "xdr" for base64, "dataJson" for JSON
            match format {
                XdrFormat::Base64 => {
                    let bytes = entry.data.to_xdr(Limits::none())
                        .map_err(|e| JsonRpcError::internal(format!("XDR encode error: {e}")))?;
                    obj.insert("xdr".into(), json!(BASE64.encode(&bytes)));
                }
                XdrFormat::Json => {
                    util::insert_xdr_field(&mut obj, "data", &entry.data, XdrFormat::Json)?;
                }
            }

            obj.insert("lastModifiedLedgerSeq".into(), json!(entry.last_modified_ledger_seq));

            // Ext field — upstream uses "extXdr" / "extJson"
            util::insert_xdr_field(&mut obj, "ext", &entry.ext, format)?;

            // For TTL-bearing entries, look up the TTL
            if let Some(ttl_key) = ttl_key_for_entry(&entry) {
                if let Some(ttl_entry) = snapshot.load(&ttl_key) {
                    if let stellar_xdr::curr::LedgerEntryData::Ttl(ttl_data) = &ttl_entry.data {
                        obj.insert(
                            "liveUntilLedgerSeq".to_string(),
                            json!(ttl_data.live_until_ledger_seq),
                        );
                    }
                }
            }

            result_entries.push(serde_json::Value::Object(obj));
        }
    }

    Ok(json!({
        "entries": result_entries,
        "latestLedger": ledger_seq
    }))
}

/// For contract data and contract code entries, build the corresponding TTL key.
fn ttl_key_for_entry(entry: &LedgerEntry) -> Option<LedgerKey> {
    let entry_key = match &entry.data {
        stellar_xdr::curr::LedgerEntryData::ContractData(data) => {
            LedgerKey::ContractData(stellar_xdr::curr::LedgerKeyContractData {
                contract: data.contract.clone(),
                key: data.key.clone(),
                durability: data.durability,
            })
        }
        stellar_xdr::curr::LedgerEntryData::ContractCode(code) => {
            LedgerKey::ContractCode(stellar_xdr::curr::LedgerKeyContractCode {
                hash: code.hash.clone(),
            })
        }
        _ => return None,
    };
    ttl_key_for_ledger_key(&entry_key)
}
