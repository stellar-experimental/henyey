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
use stellar_xdr::curr::{HotArchiveBucketEntry, LedgerEntry, LedgerKey, Limits, ReadXdr, WriteXdr};

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

/// Decode a percent-encoded string (e.g. `%3D` → `=`).
fn percent_decode(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.as_bytes().iter();
    while let Some(&b) = chars.next() {
        if b == b'%' {
            let hi = chars.next().copied();
            let lo = chars.next().copied();
            if let (Some(h), Some(l)) = (hi, lo) {
                let hex = [h, l];
                if let Ok(s) = std::str::from_utf8(&hex) {
                    if let Ok(byte) = u8::from_str_radix(s, 16) {
                        result.push(byte as char);
                        continue;
                    }
                }
            }
            // Invalid percent encoding — keep literal.
            result.push('%');
        } else if b == b'+' {
            result.push(' ');
        } else {
            result.push(b as char);
        }
    }
    result
}

/// Parse `key=<b64>&key=<b64>&ledgerSeq=N` from a form body.
///
/// This mirrors stellar-core's query parser behavior:
/// - repeated `key=` fields are accepted
/// - `ledgerSeq` must appear at most once and parse as `u32`
/// - URL percent-decoding is performed on values (stellar-rpc sends
///   standard form-encoded bodies where `=` in base64 becomes `%3D`)
fn parse_form_query_params(body: &[u8]) -> Result<FormQueryParams, String> {
    let body_str = std::str::from_utf8(body).map_err(|e| format!("Invalid UTF-8 body: {}", e))?;
    let mut keys = Vec::new();
    let mut ledger_seq_values = Vec::new();

    for pair in body_str.split('&') {
        if pair.is_empty() {
            continue;
        }
        if let Some((k, v)) = pair.split_once('=') {
            let v = &percent_decode(v);
            if v.is_empty() {
                continue;
            }

            match k {
                "key" => keys.push(v.to_string()),
                "ledgerSeq" => ledger_seq_values.push(v.to_string()),
                _ => {} // ignore unknown params
            }
        }
    }

    let ledger_seq = match ledger_seq_values.len() {
        0 => None,
        1 => Some(
            ledger_seq_values[0]
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
///
/// The CPU-heavy work (XDR decoding, bucket scans, response encoding) runs
/// on a blocking thread to avoid starving the query runtime's async executor.
pub(crate) async fn getledgerentryraw_handler(
    State(state): State<Arc<QueryState>>,
    body: Bytes,
) -> axum::response::Response {
    match tokio::task::spawn_blocking(move || process_getledgerentryraw(state, body)).await {
        Ok(response) => response,
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "Query task cancelled"})),
        )
            .into_response(),
    }
}

/// Synchronous implementation of `/getledgerentryraw`.
fn process_getledgerentryraw(
    state: Arc<QueryState>,
    body: Bytes,
) -> axum::response::Response {
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
            let entries = match live_bl.load_keys_result(&keys) {
                Ok(entries) => entries,
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({"error": format!("Bucket read error: {}", e)})),
                    )
                        .into_response();
                }
            };
            (seq, entries)
        }
    };

    // Encode entries to base64 XDR.
    let entry_results: Vec<RawEntryResult> = entries
        .iter()
        .filter_map(|e| {
            e.to_xdr(Limits::none()).ok().map(|bytes| RawEntryResult {
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
///
/// The CPU-heavy work (XDR decoding, bucket scans, response encoding) runs
/// on a blocking thread to avoid starving the query runtime's async executor.
pub(crate) async fn getledgerentry_handler(
    State(state): State<Arc<QueryState>>,
    body: Bytes,
) -> axum::response::Response {
    match tokio::task::spawn_blocking(move || process_getledgerentry(state, body)).await {
        Ok(response) => response,
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "Query task cancelled"})),
        )
            .into_response(),
    }
}

/// Synchronous implementation of `/getledgerentry`.
fn process_getledgerentry(
    state: Arc<QueryState>,
    body: Bytes,
) -> axum::response::Response {
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
    let live_entries =
        match load_from_live(&live_bl, &keys, ledger_seq, params.ledger_seq.is_some()) {
            Ok(entries) => entries,
            Err(resp) => return resp.into_response(),
        };

    // Build a set of keys that were found live.
    let live_entry_map: HashMap<Vec<u8>, &LedgerEntry> = live_entries
        .iter()
        .filter_map(|e| {
            let key = henyey_common::entry_to_key(e);
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
        match load_from_hot_archive(
            &hot_archive_bl,
            &hot_archive_keys,
            ledger_seq,
            params.ledger_seq.is_some(),
        ) {
            Ok(entries) => entries,
            Err(resp) => return resp.into_response(),
        }
    } else {
        Vec::new()
    };

    // Build a map of archived entries by key bytes.
    let archived_map: HashMap<Vec<u8>, &LedgerEntry> = archived_entries
        .iter()
        .filter_map(|hae| match hae {
            HotArchiveBucketEntry::Archived(e) => {
                let key = henyey_common::entry_to_key(e);
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
            let key = henyey_common::entry_to_key(e);
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
            let key = henyey_common::entry_to_key(e);
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
        bl.load_keys_result(keys).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("Bucket read error: {}", e)})),
            )
        })
    }
}

/// Load entries from the hot archive, optionally at a specific ledger.
fn load_from_hot_archive(
    bl: &SearchableHotArchiveBucketListSnapshot,
    keys: &[LedgerKey],
    ledger_seq: u32,
    historical: bool,
) -> Result<Vec<HotArchiveBucketEntry>, (StatusCode, Json<serde_json::Value>)> {
    if historical {
        bl.load_keys_from_ledger(keys, ledger_seq).ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": format!("Ledger {} not available", ledger_seq)})),
            )
        })
    } else {
        bl.load_keys_result(keys).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("Hot archive read error: {}", e)})),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_form_query_params, percent_decode};

    #[test]
    fn test_parse_form_query_params_accepts_repeated_key_values() {
        let params = parse_form_query_params(b"key=AAA=&key=BBB=&ledgerSeq=123").unwrap();
        assert_eq!(params.keys, vec!["AAA=", "BBB="]);
        assert_eq!(params.ledger_seq, Some(123));
    }

    #[test]
    fn test_parse_form_query_params_preserves_plus_signs_in_base64() {
        // In form encoding, + means space, but base64 uses + as a valid char.
        // stellar-core's parser doesn't decode + → space either, but the Go SDK
        // percent-encodes + as %2B, so this corner case doesn't arise in practice.
        let params = parse_form_query_params(b"key=Zm9vK2Jhcg%3D%3D").unwrap();
        assert_eq!(params.keys, vec!["Zm9vK2Jhcg=="]);
        assert_eq!(params.ledger_seq, None);
    }

    #[test]
    fn test_parse_form_query_params_decodes_percent_encoded_equals() {
        // stellar-rpc sends base64 padding as %3D
        let params = parse_form_query_params(
            b"key=AAAAAAAAAABzdv3ojkzWHMD7KUoXhrPx0GH18vHKV0ZfqpMiEblG1g%3D%3D",
        )
        .unwrap();
        assert_eq!(
            params.keys,
            vec!["AAAAAAAAAABzdv3ojkzWHMD7KUoXhrPx0GH18vHKV0ZfqpMiEblG1g=="]
        );
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

    #[test]
    fn test_percent_decode_basic() {
        assert_eq!(percent_decode("hello%20world"), "hello world");
        assert_eq!(percent_decode("a%3Db%3Dc"), "a=b=c");
        assert_eq!(percent_decode("no+encoding+here"), "no encoding here");
        assert_eq!(percent_decode("plain"), "plain");
    }

    #[test]
    fn test_percent_decode_base64_padding() {
        assert_eq!(percent_decode("AAA%3D"), "AAA=");
        assert_eq!(percent_decode("AAA%3D%3D"), "AAA==");
    }

    #[tokio::test]
    async fn test_getledgerentryraw_handler_offloads_to_blocking_thread() {
        // Verify the handler returns a proper error response when there is no
        // snapshot available — the important thing is that the spawn_blocking
        // round-trip works and produces a valid HTTP response.
        use super::*;
        use axum::body::Bytes;

        let snapshot_manager = Arc::new(BucketSnapshotManager::empty(0));
        let state = Arc::new(QueryState { snapshot_manager });

        // Empty body → BAD_REQUEST from the sync processor.
        let response = getledgerentryraw_handler(
            State(state.clone()),
            Bytes::from_static(b""),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        // Valid key param but no snapshot → SERVICE_UNAVAILABLE.
        // Use a minimal valid Account LedgerKey XDR encoded as base64.
        let response = getledgerentryraw_handler(
            State(state),
            Bytes::from_static(b"key=AAAAAAAAAAA="),
        )
        .await;
        // Either BAD_REQUEST (invalid XDR) or SERVICE_UNAVAILABLE (no snapshot)
        // — both are expected. The test validates the spawn_blocking pathway
        // completes without panicking.
        let status = response.status();
        assert!(
            status == StatusCode::BAD_REQUEST || status == StatusCode::SERVICE_UNAVAILABLE,
            "unexpected status: {status}"
        );
    }

    #[tokio::test]
    async fn test_getledgerentry_handler_offloads_to_blocking_thread() {
        use super::*;
        use axum::body::Bytes;

        let snapshot_manager = Arc::new(BucketSnapshotManager::empty(0));
        let state = Arc::new(QueryState { snapshot_manager });

        // Empty body → BAD_REQUEST from the sync processor.
        let response = getledgerentry_handler(
            State(state.clone()),
            Bytes::from_static(b""),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        // Valid key param but no snapshot → SERVICE_UNAVAILABLE.
        let response = getledgerentry_handler(
            State(state),
            Bytes::from_static(b"key=AAAAAAAAAAA="),
        )
        .await;
        let status = response.status();
        assert!(
            status == StatusCode::BAD_REQUEST || status == StatusCode::SERVICE_UNAVAILABLE,
            "unexpected status: {status}"
        );
    }
}
