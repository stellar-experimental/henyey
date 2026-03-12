use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde_json::json;
use stellar_xdr::curr::{LedgerEntry, LedgerKey, Limits, ReadXdr, WriteXdr};

use crate::context::RpcContext;
use crate::error::JsonRpcError;

pub async fn handle(
    ctx: &Arc<RpcContext>,
    params: serde_json::Value,
) -> Result<serde_json::Value, JsonRpcError> {
    let keys_array = params
        .get("keys")
        .and_then(|v| v.as_array())
        .ok_or_else(|| JsonRpcError::invalid_params("missing or invalid 'keys' parameter"))?;

    if keys_array.is_empty() {
        return Err(JsonRpcError::invalid_params("'keys' must not be empty"));
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
            // SDF RPC returns LedgerEntryData (not full LedgerEntry) in the xdr field
            let xdr_bytes = entry
                .data
                .to_xdr(Limits::none())
                .map_err(|e| JsonRpcError::internal(format!("XDR encode error: {}", e)))?;
            let xdr_b64 = BASE64.encode(&xdr_bytes);

            let last_modified = entry.last_modified_ledger_seq;

            // Encode the entry's ext field (from the outer LedgerEntry, not the data)
            let ext_xdr = entry
                .ext
                .to_xdr(Limits::none())
                .map(|b| BASE64.encode(&b))
                .unwrap_or_default();

            let mut entry_obj = json!({
                "key": key_b64,
                "xdr": xdr_b64,
                "lastModifiedLedgerSeq": last_modified,
                "extXdr": ext_xdr
            });

            // For TTL-bearing entries, look up the TTL
            if let Some(ttl_key) = ttl_key_for_entry(&entry) {
                if let Some(ttl_entry) = snapshot.load(&ttl_key) {
                    if let stellar_xdr::curr::LedgerEntryData::Ttl(ttl_data) = &ttl_entry.data {
                        entry_obj.as_object_mut().unwrap().insert(
                            "liveUntilLedgerSeq".to_string(),
                            json!(ttl_data.live_until_ledger_seq),
                        );
                    }
                }
            }

            result_entries.push(entry_obj);
        }
    }

    Ok(json!({
        "entries": result_entries,
        "latestLedger": ledger_seq
    }))
}

/// For contract data and contract code entries, build the corresponding TTL key.
fn ttl_key_for_entry(entry: &LedgerEntry) -> Option<LedgerKey> {
    match &entry.data {
        stellar_xdr::curr::LedgerEntryData::ContractData(data) => {
            Some(LedgerKey::Ttl(stellar_xdr::curr::LedgerKeyTtl {
                key_hash: hash_ledger_key(&LedgerKey::ContractData(
                    stellar_xdr::curr::LedgerKeyContractData {
                        contract: data.contract.clone(),
                        key: data.key.clone(),
                        durability: data.durability,
                    },
                )),
            }))
        }
        stellar_xdr::curr::LedgerEntryData::ContractCode(_) => {
            let code_key = LedgerKey::ContractCode(
                stellar_xdr::curr::LedgerKeyContractCode {
                    hash: match &entry.data {
                        stellar_xdr::curr::LedgerEntryData::ContractCode(c) => c.hash.clone(),
                        _ => unreachable!(),
                    },
                },
            );
            Some(LedgerKey::Ttl(stellar_xdr::curr::LedgerKeyTtl {
                key_hash: hash_ledger_key(&code_key),
            }))
        }
        _ => None,
    }
}

fn hash_ledger_key(key: &LedgerKey) -> stellar_xdr::curr::Hash {
    use henyey_crypto::sha256;
    let xdr_bytes = key.to_xdr(Limits::none()).expect("XDR encode");
    let hash = sha256(&xdr_bytes);
    stellar_xdr::curr::Hash(*hash.as_bytes())
}
