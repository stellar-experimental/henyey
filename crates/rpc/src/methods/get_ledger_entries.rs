//! Handler for the `getLedgerEntries` JSON-RPC method.

use std::collections::HashSet;
use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde_json::json;
use stellar_xdr::curr::{LedgerEntry, LedgerKey, Limits, ReadXdr, WriteXdr};

use crate::context::RpcContext;
use crate::error::JsonRpcError;
use crate::util::{self, ttl_key_for_ledger_key, XdrFormat};
use henyey_bucket::SearchableBucketListSnapshot;

/// Maximum number of ledger entry keys allowed per request.
const MAX_KEYS: usize = 200;

// SECURITY: request body bounded by HTTP framework body size limit; serde rejects invalid types
pub async fn handle(
    ctx: &Arc<RpcContext>,
    params: serde_json::Value,
) -> Result<serde_json::Value, JsonRpcError> {
    ctx.require_snapshot_ready()?;

    let format = util::parse_format(&params)?;

    let keys_array = params
        .get("keys")
        .and_then(|v| v.as_array())
        .ok_or_else(|| JsonRpcError::invalid_params("missing or invalid 'keys' parameter"))?;

    if keys_array.is_empty() {
        return Err(JsonRpcError::invalid_params("'keys' must not be empty"));
    }

    if keys_array.len() > MAX_KEYS {
        return Err(JsonRpcError::invalid_params(format!(
            "too many keys: max {} allowed",
            MAX_KEYS
        )));
    }

    // Decode base64 XDR keys, deduplicating by decoded LedgerKey.
    // SECURITY: Dedup prevents duplicate-key amplification attacks where an
    // attacker submits 200 copies of the same key to force repeated I/O and
    // serialization work. First-occurrence order is preserved.
    let mut seen_keys: HashSet<LedgerKey> = HashSet::with_capacity(keys_array.len());
    let mut ledger_keys = Vec::with_capacity(keys_array.len());
    for (i, key_val) in keys_array.iter().enumerate() {
        let key_str = key_val
            .as_str()
            .ok_or_else(|| JsonRpcError::invalid_params(format!("keys[{}] must be a string", i)))?;
        let key_bytes = BASE64.decode(key_str).map_err(|e| {
            JsonRpcError::invalid_params(format!("keys[{}]: invalid base64: {}", i, e))
        })?;
        let key = LedgerKey::from_xdr(&key_bytes, Limits::none()).map_err(|e| {
            JsonRpcError::invalid_params(format!("keys[{}]: invalid XDR: {}", i, e))
        })?;
        if seen_keys.insert(key.clone()) {
            ledger_keys.push((key_str.to_string(), key));
        }
    }

    // Get bucket list snapshot
    let snapshot = ctx
        .app
        .bucket_snapshot_manager()
        .copy_searchable_live_snapshot()
        .ok_or_else(|| JsonRpcError::internal("bucket list snapshot not available"))?;

    let ledger_seq = snapshot.ledger_seq();

    // Look up entries in a blocking task (bucket reads can hit disk).
    // Uses bounded_blocking with bucket_io_semaphore for cancellation-safe
    // concurrency bounding — the permit survives async timeout cancellation.
    let snapshot_results = util::bounded_blocking(&ctx.bucket_io_semaphore, move || {
        #[allow(clippy::type_complexity)]
        let mut results: Vec<(String, LedgerKey, Option<(LedgerEntry, Option<u32>)>)> =
            Vec::with_capacity(ledger_keys.len());
        for (key_b64, key) in ledger_keys {
            let entry = snapshot.load_result(&key)?;
            match entry {
                Some(entry) => {
                    let live_until = lookup_ttl(&snapshot, &entry)?;
                    results.push((key_b64, key, Some((entry, live_until))));
                }
                None => {
                    results.push((key_b64, key, None));
                }
            }
        }
        Ok::<_, henyey_bucket::BucketError>(results)
    })
    .await
    .map_err(|e| match e {
        util::BlockingError::Inner(e) => JsonRpcError::internal_logged("internal error", &e),
        util::BlockingError::JoinError(e) => JsonRpcError::internal_logged("internal error", &e),
        util::BlockingError::SemaphoreClosed => {
            JsonRpcError::internal("bucket I/O semaphore closed")
        }
        util::BlockingError::SemaphoreFull => {
            // bounded_blocking waits, so this shouldn't happen
            JsonRpcError::internal("bucket I/O semaphore full")
        }
    })?;

    // Build JSON response from snapshot results
    let mut result_entries = Vec::new();
    for (key_b64, decoded_key, entry_with_ttl) in &snapshot_results {
        let Some((entry, live_until)) = entry_with_ttl else {
            continue;
        };

        let mut obj = serde_json::Map::new();

        // Key — upstream uses "key" for base64, "keyJson" for JSON
        match format {
            XdrFormat::Base64 => {
                obj.insert("key".into(), json!(key_b64));
            }
            XdrFormat::Json => {
                util::insert_xdr_field(&mut obj, "key", decoded_key, format)?;
            }
        }

        // Data XDR — upstream uses "xdr" for base64, "dataJson" for JSON
        match format {
            XdrFormat::Base64 => {
                let bytes = entry
                    .data
                    .to_xdr(Limits::none())
                    .map_err(|e| JsonRpcError::internal_logged("serialization error", &e))?;
                obj.insert("xdr".into(), json!(BASE64.encode(&bytes)));
            }
            XdrFormat::Json => {
                util::insert_xdr_field(&mut obj, "data", &entry.data, XdrFormat::Json)?;
            }
        }

        obj.insert(
            "lastModifiedLedgerSeq".into(),
            json!(entry.last_modified_ledger_seq),
        );

        // Ext field — upstream uses "extXdr" / "extJson"
        util::insert_xdr_field(&mut obj, "ext", &entry.ext, format)?;

        // For TTL-bearing entries
        if let Some(ttl) = live_until {
            obj.insert("liveUntilLedgerSeq".to_string(), json!(ttl));
        }

        result_entries.push(serde_json::Value::Object(obj));
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

/// Look up the TTL (live_until_ledger_seq) for an entry, if it has one.
fn lookup_ttl(
    snapshot: &SearchableBucketListSnapshot,
    entry: &LedgerEntry,
) -> Result<Option<u32>, henyey_bucket::BucketError> {
    let Some(ttl_key) = ttl_key_for_entry(entry) else {
        return Ok(None);
    };
    let Some(ttl_entry) = snapshot.load_result(&ttl_key)? else {
        return Ok(None);
    };
    if let stellar_xdr::curr::LedgerEntryData::Ttl(ttl_data) = &ttl_entry.data {
        Ok(Some(ttl_data.live_until_ledger_seq))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{
        AccountId, LedgerKeyAccount, LedgerKeyContractCode, PublicKey, Uint256, WriteXdr,
    };

    /// Encode a LedgerKey to base64 for use in test request params.
    fn key_to_b64(key: &LedgerKey) -> String {
        BASE64.encode(key.to_xdr(Limits::none()).unwrap())
    }

    fn account_key(byte: u8) -> LedgerKey {
        LedgerKey::Account(LedgerKeyAccount {
            account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([byte; 32]))),
        })
    }

    fn contract_code_key(byte: u8) -> LedgerKey {
        LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: stellar_xdr::curr::Hash([byte; 32]),
        })
    }

    /// Simulates the key-decoding + dedup logic from `handle()` and returns
    /// the deduplicated keys vec. This mirrors the exact dedup code path.
    fn decode_and_dedup(keys_b64: &[&str]) -> Vec<(String, LedgerKey)> {
        let mut seen_keys: HashSet<LedgerKey> = HashSet::with_capacity(keys_b64.len());
        let mut ledger_keys = Vec::with_capacity(keys_b64.len());
        for key_str in keys_b64 {
            let key_bytes = BASE64.decode(key_str).unwrap();
            let key = LedgerKey::from_xdr(&key_bytes, Limits::none()).unwrap();
            if seen_keys.insert(key.clone()) {
                ledger_keys.push((key_str.to_string(), key));
            }
        }
        ledger_keys
    }

    #[test]
    fn test_dedup_all_identical_keys() {
        let key = account_key(1);
        let b64 = key_to_b64(&key);
        let keys: Vec<&str> = vec![&b64; 200];

        let result = decode_and_dedup(&keys);
        assert_eq!(result.len(), 1, "200 identical keys must collapse to 1");
        assert_eq!(result[0].1, key);
    }

    #[test]
    fn test_dedup_all_unique_keys() {
        let keys: Vec<LedgerKey> = (0..5).map(|i| account_key(i)).collect();
        let b64s: Vec<String> = keys.iter().map(|k| key_to_b64(k)).collect();
        let refs: Vec<&str> = b64s.iter().map(|s| s.as_str()).collect();

        let result = decode_and_dedup(&refs);
        assert_eq!(result.len(), 5, "all unique keys must be preserved");
        for (i, (_, k)) in result.iter().enumerate() {
            assert_eq!(*k, keys[i]);
        }
    }

    #[test]
    fn test_dedup_mixed_preserves_first_occurrence_order() {
        let k1 = account_key(1);
        let k2 = account_key(2);
        let k3 = contract_code_key(3);

        let b1 = key_to_b64(&k1);
        let b2 = key_to_b64(&k2);
        let b3 = key_to_b64(&k3);

        // Order: k1, k2, k1, k3, k2, k3
        let keys = vec![
            b1.as_str(),
            b2.as_str(),
            b1.as_str(),
            b3.as_str(),
            b2.as_str(),
            b3.as_str(),
        ];

        let result = decode_and_dedup(&keys);
        assert_eq!(result.len(), 3, "3 unique keys from 6 inputs");
        assert_eq!(result[0].1, k1, "first occurrence order preserved");
        assert_eq!(result[1].1, k2);
        assert_eq!(result[2].1, k3);
    }

    #[test]
    fn test_dedup_preserves_first_base64_string() {
        let key = account_key(42);
        let b64 = key_to_b64(&key);

        // Even if same key is submitted multiple times, first base64 string wins
        let keys = vec![b64.as_str(), b64.as_str(), b64.as_str()];
        let result = decode_and_dedup(&keys);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, b64, "first base64 string must be preserved");
    }
}
