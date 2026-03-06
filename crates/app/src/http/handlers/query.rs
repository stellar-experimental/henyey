//! Query server handler implementations.
//!
//! Implements `/getledgerentryraw` and `/getledgerentry` endpoints that query
//! the bucket list snapshots for ledger entry data. These match stellar-core's
//! QueryServer endpoints.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;

use henyey_bucket::{
    get_ttl_key, get_ttl_live_until, is_persistent_key, is_soroban_key, BucketSnapshotManager,
    SearchableBucketListSnapshot, SearchableHotArchiveBucketListSnapshot,
};
use stellar_xdr::curr::{
    HotArchiveBucketEntry, LedgerEntry, LedgerKey, Limits, ReadXdr, WriteXdr,
};

use crate::http::types::query::{
    GetLedgerEntryRawResponse, GetLedgerEntryResponse, LedgerEntryResult, LedgerEntryState,
    RawEntryResult,
};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};

/// Shared state for the query server.
pub(crate) struct QueryState {
    pub snapshot_manager: Arc<BucketSnapshotManager>,
}

/// Parsed form-encoded query request (supports repeated `key=` params).
#[derive(Debug)]
struct FormQueryParams {
    keys: Vec<String>,
    ledger_seq: Option<u32>,
}

/// Parse `key=<b64>&key=<b64>&ledgerSeq=N` from a form body.
///
/// This mirrors stellar-core's query parser behavior:
/// - repeated `key=` fields are accepted
/// - `ledgerSeq` must appear at most once and parse as `u32`
/// - no URL percent-decoding is performed
fn parse_form_query_params(body: &[u8]) -> Result<FormQueryParams, String> {
    let body_str = std::str::from_utf8(body).map_err(|e| format!("Invalid UTF-8 body: {}", e))?;
    let mut keys = Vec::new();
    let mut ledger_seq_values = Vec::new();

    for pair in body_str.split('&') {
        if pair.is_empty() {
            continue;
        }
        if let Some((k, v)) = pair.split_once('=') {
            if v.is_empty() {
                continue;
            }

            match k {
                "key" => keys.push(v.to_string()),
                "ledgerSeq" => ledger_seq_values.push(v),
                _ => {} // ignore unknown params
            }
        }
    }

    let ledger_seq = match ledger_seq_values.as_slice() {
        [] => None,
        [value] => Some(
            value
                .parse::<u32>()
                .map_err(|_| "Failed to parse 'ledgerSeq' argument".to_string())?,
        ),
        _ => return Err("Expected exactly one 'ledgerSeq' argument".to_string()),
    };

    Ok(FormQueryParams { keys, ledger_seq })
}

/// POST /getledgerentryraw
///
/// Returns raw ledger entries from the live bucket list without TTL/state
/// classification. Matches stellar-core's `getledgerentryraw` endpoint.
///
/// Accepts `application/x-www-form-urlencoded` body with repeated `key=`
/// params and optional `ledgerSeq=`.
pub(crate) async fn getledgerentryraw_handler(
    State(state): State<Arc<QueryState>>,
    body: Bytes,
) -> impl IntoResponse {
    let params = match parse_form_query_params(&body) {
        Ok(p) => p,
        Err(msg) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": msg})),
            )
                .into_response();
        }
    };

    // Decode all keys from base64 XDR.
    let keys = match decode_keys(&params.keys) {
        Ok(keys) => keys,
        Err(msg) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": msg})),
            )
                .into_response();
        }
    };

    if keys.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "No keys provided"})),
        )
            .into_response();
    }

    // Get the live bucket list snapshot.
    let live_bl = match state.snapshot_manager.copy_searchable_live_snapshot() {
        Some(bl) => bl,
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "Snapshot not available"})),
            )
                .into_response();
        }
    };

    // Load entries.
    let (ledger_seq, entries) = match params.ledger_seq {
        Some(seq) => match live_bl.load_keys_from_ledger(&keys, seq) {
            Some(entries) => (seq, entries),
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error": format!("Ledger {} not available", seq)})),
                )
                    .into_response();
            }
        },
        None => {
            let seq = live_bl.ledger_seq();
            let entries = live_bl.load_keys(&keys);
            (seq, entries)
        }
    };

    // Encode entries to base64 XDR.
    let entry_results: Vec<RawEntryResult> = entries
        .iter()
        .filter_map(|e| {
            e.to_xdr(Limits::none())
                .ok()
                .map(|bytes| RawEntryResult {
                    entry: BASE64.encode(&bytes),
                })
        })
        .collect();

    Json(GetLedgerEntryRawResponse {
        ledger_seq,
        entries: entry_results,
    })
    .into_response()
}

/// POST /getledgerentry
///
/// Returns ledger entries with TTL state classification. Implements the
/// three-pass algorithm from stellar-core's QueryServer:
///
/// 1. Load all keys from the live bucket list
/// 2. Search the hot archive for missing Soroban keys
/// 3. Load TTL entries for live Soroban entries
///
/// Results preserve request order.
///
/// Accepts `application/x-www-form-urlencoded` body with repeated `key=`
/// params and optional `ledgerSeq=`.
pub(crate) async fn getledgerentry_handler(
    State(state): State<Arc<QueryState>>,
    body: Bytes,
) -> impl IntoResponse {
    let params = match parse_form_query_params(&body) {
        Ok(p) => p,
        Err(msg) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": msg})),
            )
                .into_response();
        }
    };

    // Decode all keys from base64 XDR.
    let keys = match decode_keys(&params.keys) {
        Ok(keys) => keys,
        Err(msg) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": msg})),
            )
                .into_response();
        }
    };

    if keys.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "No keys provided"})),
        )
            .into_response();
    }

    // Validate: reject TTL keys and duplicates.
    let mut seen = HashSet::with_capacity(keys.len());
    for key in &keys {
        if matches!(key, LedgerKey::Ttl(_)) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "TTL keys are not allowed"})),
            )
                .into_response();
        }
        let key_bytes = match key.to_xdr(Limits::none()) {
            Ok(b) => b,
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": "Failed to serialize key"})),
                )
                    .into_response();
            }
        };
        if !seen.insert(key_bytes) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "Duplicate keys"})),
            )
                .into_response();
        }
    }

    // Get both snapshots atomically.
    let (live_bl, hot_archive_bl) =
        match state.snapshot_manager.copy_live_and_hot_archive_snapshots() {
            Some(pair) => pair,
            None => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(serde_json::json!({"error": "Snapshot not available"})),
                )
                    .into_response();
            }
        };

    let ledger_seq = params.ledger_seq.unwrap_or_else(|| live_bl.ledger_seq());

    // ── Pass 1: Load all keys from live bucket list ──────────────────
    let live_entries = match load_from_live(&live_bl, &keys, ledger_seq, params.ledger_seq.is_some()) {
        Ok(entries) => entries,
        Err(resp) => return resp.into_response(),
    };

    // Build a set of keys that were found live.
    let live_entry_map: HashMap<Vec<u8>, &LedgerEntry> = live_entries
        .iter()
        .filter_map(|e| {
            let key = henyey_bucket::ledger_entry_to_key(e)?;
            key.to_xdr(Limits::none()).ok().map(|kb| (kb, e))
        })
        .collect();

    // Determine which keys were NOT found in the live bucket list.
    let mut missing_keys: Vec<&LedgerKey> = Vec::new();
    for key in &keys {
        let kb = match key.to_xdr(Limits::none()) {
            Ok(b) => b,
            Err(_) => continue,
        };
        if !live_entry_map.contains_key(&kb) {
            missing_keys.push(key);
        }
    }

    // ── Pass 2: Search hot archive for missing Soroban keys ──────────
    let hot_archive_keys: Vec<LedgerKey> = missing_keys
        .iter()
        .filter(|k| is_soroban_key(k))
        .cloned()
        .cloned()
        .collect();

    let archived_entries = if !hot_archive_keys.is_empty() {
        load_from_hot_archive(&hot_archive_bl, &hot_archive_keys, ledger_seq, params.ledger_seq.is_some())
    } else {
        Vec::new()
    };

    // Build a map of archived entries by key bytes.
    let archived_map: HashMap<Vec<u8>, &LedgerEntry> = archived_entries
        .iter()
        .filter_map(|hae| match hae {
            HotArchiveBucketEntry::Archived(e) => {
                let key = henyey_bucket::ledger_entry_to_key(e)?;
                key.to_xdr(Limits::none()).ok().map(|kb| (kb, e))
            }
            _ => None,
        })
        .collect();

    // ── Pass 3: Load TTL keys for live Soroban entries ───────────────
    let ttl_keys: Vec<LedgerKey> = live_entries
        .iter()
        .filter(|e| henyey_bucket::is_soroban_entry(e))
        .filter_map(|e| {
            let key = henyey_bucket::ledger_entry_to_key(e)?;
            get_ttl_key(&key)
        })
        .collect();

    let ttl_entries = if !ttl_keys.is_empty() {
        match load_from_live(&live_bl, &ttl_keys, ledger_seq, params.ledger_seq.is_some()) {
            Ok(entries) => entries,
            Err(resp) => return resp.into_response(),
        }
    } else {
        Vec::new()
    };

    // Build a map from the original key's TTL key bytes -> TTL entry.
    let ttl_map: HashMap<Vec<u8>, &LedgerEntry> = ttl_entries
        .iter()
        .filter_map(|e| {
            let key = henyey_bucket::ledger_entry_to_key(e)?;
            key.to_xdr(Limits::none()).ok().map(|kb| (kb, e))
        })
        .collect();

    // ── Build response in input order ────────────────────────────────
    let mut results = Vec::with_capacity(keys.len());

    for key in &keys {
        let kb = match key.to_xdr(Limits::none()) {
            Ok(b) => b,
            Err(_) => {
                results.push(LedgerEntryResult {
                    state: LedgerEntryState::NotFound,
                    entry: None,
                    live_until_ledger_seq: None,
                });
                continue;
            }
        };

        if let Some(entry) = live_entry_map.get(&kb) {
            // Entry found in live bucket list.
            if is_soroban_key(key) {
                // Look up the TTL entry.
                let ttl_key = get_ttl_key(key);
                let ttl_entry = ttl_key.as_ref().and_then(|tk| {
                    tk.to_xdr(Limits::none())
                        .ok()
                        .and_then(|tkb| ttl_map.get(&tkb))
                });

                if let Some(ttl_e) = ttl_entry {
                    let live_until = get_ttl_live_until(ttl_e).unwrap_or(0);
                    let is_live = live_until >= ledger_seq;

                    if is_live {
                        results.push(LedgerEntryResult {
                            state: LedgerEntryState::Live,
                            entry: encode_entry(entry),
                            live_until_ledger_seq: Some(live_until),
                        });
                    } else if is_persistent_key(key) {
                        // Expired persistent entry = archived in place.
                        results.push(LedgerEntryResult {
                            state: LedgerEntryState::Archived,
                            entry: encode_entry(entry),
                            live_until_ledger_seq: Some(0),
                        });
                    } else {
                        // Expired temporary entry = gone.
                        results.push(LedgerEntryResult {
                            state: LedgerEntryState::NotFound,
                            entry: None,
                            live_until_ledger_seq: None,
                        });
                    }
                } else {
                    // Soroban entry without TTL? Shouldn't happen, treat as live.
                    results.push(LedgerEntryResult {
                        state: LedgerEntryState::Live,
                        entry: encode_entry(entry),
                        live_until_ledger_seq: None,
                    });
                }
            } else {
                // Non-Soroban entry — always live.
                results.push(LedgerEntryResult {
                    state: LedgerEntryState::Live,
                    entry: encode_entry(entry),
                    live_until_ledger_seq: None,
                });
            }
        } else if let Some(entry) = archived_map.get(&kb) {
            // Entry found in hot archive.
            results.push(LedgerEntryResult {
                state: LedgerEntryState::Archived,
                entry: encode_entry(entry),
                live_until_ledger_seq: Some(0),
            });
        } else {
            // Not found anywhere.
            results.push(LedgerEntryResult {
                state: LedgerEntryState::NotFound,
                entry: None,
                live_until_ledger_seq: None,
            });
        }
    }

    Json(GetLedgerEntryResponse {
        ledger_seq,
        entries: results,
    })
    .into_response()
}

// ── Helpers ──────────────────────────────────────────────────────────────

/// Decode base64 XDR keys into `LedgerKey` values.
fn decode_keys(encoded: &[String]) -> Result<Vec<LedgerKey>, String> {
    encoded
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let bytes = BASE64
                .decode(s)
                .map_err(|e| format!("Invalid base64 at index {}: {}", i, e))?;
            LedgerKey::from_xdr(&bytes, Limits::none())
                .map_err(|e| format!("Invalid XDR LedgerKey at index {}: {}", i, e))
        })
        .collect()
}

/// Encode a `LedgerEntry` to base64 XDR.
fn encode_entry(entry: &LedgerEntry) -> Option<String> {
    entry
        .to_xdr(Limits::none())
        .ok()
        .map(|bytes| BASE64.encode(&bytes))
}

/// Load entries from the live bucket list, optionally at a specific ledger.
fn load_from_live(
    bl: &SearchableBucketListSnapshot,
    keys: &[LedgerKey],
    ledger_seq: u32,
    historical: bool,
) -> Result<Vec<LedgerEntry>, (StatusCode, Json<serde_json::Value>)> {
    if historical {
        bl.load_keys_from_ledger(keys, ledger_seq).ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": format!("Ledger {} not available", ledger_seq)})),
            )
        })
    } else {
        Ok(bl.load_keys(keys))
    }
}

/// Load entries from the hot archive, optionally at a specific ledger.
fn load_from_hot_archive(
    bl: &SearchableHotArchiveBucketListSnapshot,
    keys: &[LedgerKey],
    ledger_seq: u32,
    historical: bool,
) -> Vec<HotArchiveBucketEntry> {
    if historical {
        bl.load_keys_from_ledger(keys, ledger_seq).unwrap_or_default()
    } else {
        bl.load_keys(keys)
    }
}

#[cfg(test)]
mod tests {
    use super::parse_form_query_params;

    #[test]
    fn test_parse_form_query_params_accepts_repeated_key_values() {
        let params = parse_form_query_params(b"key=AAA=&key=BBB=&ledgerSeq=123").unwrap();
        assert_eq!(params.keys, vec!["AAA=", "BBB="]);
        assert_eq!(params.ledger_seq, Some(123));
    }

    #[test]
    fn test_parse_form_query_params_preserves_plus_signs() {
        let params = parse_form_query_params(b"key=Zm9vK2Jhcg==&key=abc+def").unwrap();
        assert_eq!(params.keys, vec!["Zm9vK2Jhcg==", "abc+def"]);
        assert_eq!(params.ledger_seq, None);
    }

    #[test]
    fn test_parse_form_query_params_rejects_invalid_ledger_seq() {
        let err = parse_form_query_params(b"key=AAA=&ledgerSeq=bad").unwrap_err();
        assert_eq!(err, "Failed to parse 'ledgerSeq' argument");
    }

    #[test]
    fn test_parse_form_query_params_rejects_duplicate_ledger_seq() {
        let err = parse_form_query_params(b"key=AAA=&ledgerSeq=10&ledgerSeq=11").unwrap_err();
        assert_eq!(err, "Expected exactly one 'ledgerSeq' argument");
    }
}
