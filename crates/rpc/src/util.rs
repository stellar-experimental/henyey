//! Shared utility functions for the RPC crate.

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde::Serialize;
use stellar_xdr::curr::{
    DiagnosticEvent, LedgerCloseMeta, LedgerHeaderHistoryEntry, LedgerKey, Limits, ReadXdr,
    TransactionMeta, TransactionResultCode, TransactionResultPair, WriteXdr,
};

use crate::error::JsonRpcError;

// ---------------------------------------------------------------------------
// XDR format helpers (xdrFormat / "format" parameter support)
// ---------------------------------------------------------------------------
//
// The upstream stellar-rpc uses separate JSON keys for XDR vs JSON output:
//   - `xdrFormat: "xdr"` (default) → `envelopeXdr`, `headerXdr`, etc. (base64 strings)
//   - `xdrFormat: "json"` → `envelopeJson`, `headerJson`, etc. (JSON objects)
//
// Callers use `insert_xdr_field()` to add the appropriate key/value pair
// to their response objects.

/// Whether the caller wants base64 XDR strings or JSON objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum XdrFormat {
    /// Default: base64-encoded XDR bytes.
    Base64,
    /// JSON: serde-serialized XDR types.
    Json,
}

/// Parse the `"xdrFormat"` parameter from JSON-RPC params.
///
/// Accepted values: `"xdr"` (default), `"json"`.
pub(crate) fn parse_format(params: &serde_json::Value) -> Result<XdrFormat, JsonRpcError> {
    let val = params.get("xdrFormat").and_then(|v| v.as_str());

    match val {
        None | Some("xdr") => Ok(XdrFormat::Base64),
        Some("json") => Ok(XdrFormat::Json),
        Some(other) => Err(JsonRpcError::invalid_params(format!(
            "unsupported xdrFormat: {other:?} (expected \"xdr\" or \"json\")"
        ))),
    }
}

/// Controls how the JSON key is derived from `base_name` in XDR insert helpers.
///
/// Most RPC methods use [`Suffixed`](XdrKeyStyle::Suffixed) (`envelopeXdr` / `envelopeJson`).
/// The `simulateTransaction` endpoint uses [`Unsuffixed`](XdrKeyStyle::Unsuffixed)
/// (`transactionData` in base64 mode, `transactionDataJson` in JSON mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum XdrKeyStyle {
    /// Base64 key = `"{base_name}Xdr"`, JSON key = `"{base_name}Json"`.
    Suffixed,
    /// Base64 key = `"{base_name}"` (unsuffixed), JSON key = `"{base_name}Json"`.
    Unsuffixed,
}

/// Build the JSON key for a given base name, format, and key style.
fn xdr_key(base_name: &str, format: XdrFormat, style: XdrKeyStyle) -> String {
    match format {
        XdrFormat::Base64 => match style {
            XdrKeyStyle::Suffixed => format!("{base_name}Xdr"),
            XdrKeyStyle::Unsuffixed => base_name.to_string(),
        },
        XdrFormat::Json => format!("{base_name}Json"),
    }
}

/// Insert an XDR field into a JSON object with the correct key name.
///
/// When format is `Base64`, inserts `"{base_name}Xdr": "<base64>"`.
/// When format is `Json`, inserts `"{base_name}Json": <json_object>`.
///
/// `base_name` should be the stem without Xdr/Json suffix (e.g. `"envelope"`, `"header"`).
pub(crate) fn insert_xdr_field<T: WriteXdr + Serialize>(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    base_name: &str,
    val: &T,
    format: XdrFormat,
) -> Result<(), JsonRpcError> {
    insert_xdr_field_styled(obj, base_name, val, format, XdrKeyStyle::Suffixed)
}

/// Insert an XDR field with explicit key style control.
///
/// See [`XdrKeyStyle`] for the difference between suffixed and unsuffixed keys.
pub(crate) fn insert_xdr_field_styled<T: WriteXdr + Serialize>(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    base_name: &str,
    val: &T,
    format: XdrFormat,
    style: XdrKeyStyle,
) -> Result<(), JsonRpcError> {
    let key = xdr_key(base_name, format, style);
    match format {
        XdrFormat::Base64 => {
            let bytes = val
                .to_xdr(Limits::none())
                .map_err(|e| JsonRpcError::internal(format!("XDR encode error: {e}")))?;
            obj.insert(key, serde_json::Value::String(BASE64.encode(&bytes)));
        }
        XdrFormat::Json => {
            let json_val = serde_json::to_value(val)
                .map_err(|e| JsonRpcError::internal(format!("JSON serialize error: {e}")))?;
            obj.insert(key, json_val);
        }
    }
    Ok(())
}

/// Insert an XDR field from raw bytes into a JSON object with the correct key name.
///
/// Like `insert_xdr_field` but starts from raw XDR bytes. In JSON mode the bytes
/// are first deserialized as type `T`.
// SECURITY: XDR input pre-bounded by HTTP body size limit; Limits::none() is safe
pub(crate) fn insert_raw_xdr_field<T: ReadXdr + Serialize>(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    base_name: &str,
    bytes: &[u8],
    format: XdrFormat,
) -> Result<(), JsonRpcError> {
    match format {
        XdrFormat::Base64 => {
            obj.insert(
                format!("{base_name}Xdr"),
                serde_json::Value::String(BASE64.encode(bytes)),
            );
        }
        XdrFormat::Json => {
            let val = T::from_xdr(bytes, Limits::none())
                .map_err(|e| JsonRpcError::internal(format!("XDR decode error: {e}")))?;
            let json_val = serde_json::to_value(&val)
                .map_err(|e| JsonRpcError::internal(format!("JSON serialize error: {e}")))?;
            obj.insert(format!("{base_name}Json"), json_val);
        }
    }
    Ok(())
}

/// Insert an array of XDR items into a JSON object with the correct key name.
///
/// `Base64`: inserts `"{base_name}Xdr": ["<b64>", ...]`.
/// `Json`: inserts `"{base_name}Json": [{...}, ...]`.
pub(crate) fn insert_xdr_array_field<T: WriteXdr + Serialize>(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    base_name: &str,
    items: &[T],
    format: XdrFormat,
) -> Result<(), JsonRpcError> {
    insert_xdr_array_field_styled(obj, base_name, items, format, XdrKeyStyle::Suffixed)
}

/// Insert an array of XDR items with explicit key style control.
///
/// See [`XdrKeyStyle`] for the difference between suffixed and unsuffixed keys.
pub(crate) fn insert_xdr_array_field_styled<T: WriteXdr + Serialize>(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    base_name: &str,
    items: &[T],
    format: XdrFormat,
    style: XdrKeyStyle,
) -> Result<(), JsonRpcError> {
    let key = xdr_key(base_name, format, style);
    match format {
        XdrFormat::Base64 => {
            let encoded: Vec<serde_json::Value> = items
                .iter()
                .map(|item| {
                    item.to_xdr(Limits::none())
                        .map(|b| serde_json::Value::String(BASE64.encode(&b)))
                        .map_err(|e| JsonRpcError::internal(format!("XDR encode error: {e}")))
                })
                .collect::<Result<_, _>>()?;
            obj.insert(key, serde_json::Value::Array(encoded));
        }
        XdrFormat::Json => {
            let json_items: Vec<serde_json::Value> = items
                .iter()
                .map(|item| {
                    serde_json::to_value(item)
                        .map_err(|e| JsonRpcError::internal(format!("JSON serialize error: {e}")))
                })
                .collect::<Result<_, _>>()?;
            obj.insert(key, serde_json::Value::Array(json_items));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// LedgerCloseMeta helpers
// ---------------------------------------------------------------------------

/// Extract the [`LedgerHeaderHistoryEntry`] reference from any version of
/// [`LedgerCloseMeta`].
pub(crate) fn ledger_header_entry(lcm: &LedgerCloseMeta) -> &LedgerHeaderHistoryEntry {
    match lcm {
        LedgerCloseMeta::V0(v0) => &v0.ledger_header,
        LedgerCloseMeta::V1(v1) => &v1.ledger_header,
        LedgerCloseMeta::V2(v2) => &v2.ledger_header,
    }
}

// ---------------------------------------------------------------------------
// TOID (Total Order ID) encoding
// ---------------------------------------------------------------------------

/// Bit layout: [63..32] = ledger_seq, [31..12] = tx_order (20 bits), [11..0] = op_index (12 bits).
const LEDGER_SHIFT: u32 = 32;
const TX_ORDER_SHIFT: u32 = 12;
const TX_ORDER_MASK: u64 = 0x000F_FFFF; // 20 bits
const OP_INDEX_MASK: u64 = 0x0000_0FFF; // 12 bits

/// Encode a (ledger, tx_order, op_index) triple into a TOID.
pub(crate) fn toid_encode(ledger_seq: u32, tx_order: u32, op_index: u32) -> i64 {
    let v = ((ledger_seq as u64) << LEDGER_SHIFT)
        | (((tx_order as u64) & TX_ORDER_MASK) << TX_ORDER_SHIFT)
        | ((op_index as u64) & OP_INDEX_MASK);
    v as i64
}

/// Decode a TOID back to (ledger_seq, tx_order, op_index).
pub(crate) fn toid_decode(toid: i64) -> (u32, u32, u32) {
    let v = toid as u64;
    let ledger_seq = (v >> LEDGER_SHIFT) as u32;
    let tx_order = ((v >> TX_ORDER_SHIFT) & TX_ORDER_MASK) as u32;
    let op_index = (v & OP_INDEX_MASK) as u32;
    (ledger_seq, tx_order, op_index)
}

/// Parse a decimal-string cursor into a TOID (i64).
pub(crate) fn toid_parse_cursor(cursor: &str) -> Result<i64, JsonRpcError> {
    cursor
        .parse::<i64>()
        .map_err(|_| JsonRpcError::invalid_params(format!("invalid cursor: {cursor}")))
}

// ---------------------------------------------------------------------------
// Pagination validation
// ---------------------------------------------------------------------------

/// Validate and normalise pagination parameters shared by `getTransactions` and `getLedgers`.
///
/// Returns `(effective_start_ledger, effective_start_cursor, effective_limit)`.
///
/// * `start_ledger` and `cursor` are mutually exclusive.
/// * If a cursor is provided it takes priority; `start_ledger` is ignored.
/// * `start_ledger` must be within `[oldest_ledger, latest_ledger]`.
/// * `limit` is clamped to `[1, max_limit]` and defaults to `default_limit`.
pub(crate) fn validate_pagination(
    start_ledger: Option<u32>,
    cursor: Option<&str>,
    limit: Option<u32>,
    default_limit: u32,
    max_limit: u32,
    oldest_ledger: u32,
    latest_ledger: u32,
) -> Result<(u32, Option<i64>, u32), JsonRpcError> {
    // cursor and startLedger are mutually exclusive
    if cursor.is_some() && start_ledger.is_some() {
        return Err(JsonRpcError::invalid_params(
            "startLedger and cursor are mutually exclusive",
        ));
    }

    let limit = limit.unwrap_or(default_limit).min(max_limit).max(1);

    if let Some(c) = cursor {
        if c.is_empty() {
            return Err(JsonRpcError::invalid_params("cursor must not be empty"));
        }
        let toid = toid_parse_cursor(c)?;
        let (ledger, _, _) = toid_decode(toid);
        return Ok((ledger, Some(toid), limit));
    }

    let start = start_ledger
        .ok_or_else(|| JsonRpcError::invalid_params("startLedger or cursor is required"))?;

    if start < oldest_ledger || start > latest_ledger {
        return Err(JsonRpcError::invalid_params(format!(
            "startLedger must be within [{oldest_ledger}, {latest_ledger}]"
        )));
    }

    Ok((start, None, limit))
}

// ---------------------------------------------------------------------------
// Transaction helpers (shared by getTransaction and getTransactions)
// ---------------------------------------------------------------------------

/// Determine the transaction status ("SUCCESS" or "FAILED") from a `TransactionResultPair` blob.
pub(crate) fn determine_tx_status(result_bytes: &[u8]) -> &'static str {
    match TransactionResultPair::from_xdr(result_bytes, Limits::none()) {
        Ok(pair) => {
            let code = pair.result.result.discriminant();
            if code == TransactionResultCode::TxSuccess
                || code == TransactionResultCode::TxFeeBumpInnerSuccess
            {
                "SUCCESS"
            } else {
                "FAILED"
            }
        }
        Err(_) => "FAILED",
    }
}

/// Extract just the `TransactionResult` XDR bytes from a stored `TransactionResultPair` blob.
pub(crate) fn extract_result_xdr(result_pair_bytes: &[u8]) -> Option<Vec<u8>> {
    let pair = TransactionResultPair::from_xdr(result_pair_bytes, Limits::none()).ok()?;
    pair.result.to_xdr(Limits::none()).ok()
}

/// Get the oldest ledger sequence from the database, defaulting to 1 on error.
pub(crate) fn oldest_ledger(app: &henyey_app::App) -> u32 {
    app.database()
        .get_oldest_ledger_seq()
        .unwrap_or(Some(1))
        .unwrap_or(1)
}

/// Get the close time (unix seconds) for a given ledger, returning 0 on error.
pub(crate) fn ledger_close_time(app: &henyey_app::App, ledger_seq: u32) -> u64 {
    app.database()
        .with_connection(|conn| {
            use henyey_db::LedgerQueries;
            conn.load_ledger_header(ledger_seq)
        })
        .ok()
        .flatten()
        .map(|h| h.scp_value.close_time.0)
        .unwrap_or(0)
}

/// Common ledger context fields included in most RPC responses.
///
/// Captures the latest and oldest ledger sequence numbers and their close
/// times in a single struct, avoiding the repeated 3-call boilerplate
/// (`ledger_summary` + `oldest_ledger` + `ledger_close_time`).
pub(crate) struct LedgerContext {
    pub latest_ledger: u32,
    pub latest_close_time: u64,
    pub oldest_ledger: u32,
    pub oldest_close_time: u64,
}

impl LedgerContext {
    /// Build from the running app state.
    pub fn from_app(app: &henyey_app::App) -> Self {
        let summary = app.ledger_summary();
        let oldest = oldest_ledger(app);
        let oldest_close = ledger_close_time(app, oldest);
        Self {
            latest_ledger: summary.num,
            latest_close_time: summary.close_time,
            oldest_ledger: oldest,
            oldest_close_time: oldest_close,
        }
    }

    /// Insert the four standard fields into a `serde_json::Map`.
    pub fn insert_json_fields(&self, map: &mut serde_json::Map<String, serde_json::Value>) {
        map.insert("latestLedger".into(), serde_json::json!(self.latest_ledger));
        map.insert(
            "latestLedgerCloseTime".into(),
            serde_json::json!(self.latest_close_time.to_string()),
        );
        map.insert("oldestLedger".into(), serde_json::json!(self.oldest_ledger));
        map.insert(
            "oldestLedgerCloseTime".into(),
            serde_json::json!(self.oldest_close_time.to_string()),
        );
    }
}

/// Check if XDR-encoded transaction envelope bytes represent a fee bump transaction.
// SECURITY: XDR input pre-bounded by HTTP body size limit; Limits::none() is safe
pub(crate) fn is_fee_bump_envelope(envelope_bytes: &[u8]) -> bool {
    use stellar_xdr::curr::TransactionEnvelope;
    TransactionEnvelope::from_xdr(envelope_bytes, Limits::none())
        .map(|env| matches!(env, TransactionEnvelope::TxFeeBump(_)))
        .unwrap_or(false)
}

/// Extract diagnostic events from `TransactionMeta` bytes.
///
/// Returns `None` if no diagnostic events are present or the meta cannot be parsed.
/// V3 meta has events in `soroban_meta.diagnostic_events`, V4 has them directly.
pub(crate) fn extract_diagnostic_events(meta_bytes: &[u8]) -> Option<Vec<DiagnosticEvent>> {
    let meta = TransactionMeta::from_xdr(meta_bytes, Limits::none()).ok()?;

    let events: &[DiagnosticEvent] = match &meta {
        TransactionMeta::V3(v3) => v3
            .soroban_meta
            .as_ref()
            .map(|sm| sm.diagnostic_events.as_slice())
            .unwrap_or(&[]),
        TransactionMeta::V4(v4) => v4.diagnostic_events.as_slice(),
        _ => return None,
    };

    if events.is_empty() {
        None
    } else {
        Some(events.to_vec())
    }
}

/// Insert diagnostic events from `TransactionMeta` bytes into a JSON object.
///
/// Adds `diagnosticEventsXdr` (base64 array) or `diagnosticEventsJson` (JSON array)
/// depending on the format. Does nothing if no diagnostic events are present.
pub(crate) fn insert_diagnostic_events(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    meta_bytes: &[u8],
    format: XdrFormat,
) -> Result<(), JsonRpcError> {
    if let Some(events) = extract_diagnostic_events(meta_bytes) {
        insert_xdr_array_field(obj, "diagnosticEvents", &events, format)?;
    }
    Ok(())
}

/// Build the TTL lookup key for a contract data or contract code ledger key.
///
/// Returns `None` if the key is not a TTL-bearing type.
pub(crate) fn ttl_key_for_ledger_key(key: &LedgerKey) -> Option<LedgerKey> {
    match key {
        LedgerKey::ContractData(_) | LedgerKey::ContractCode(_) => {
            Some(LedgerKey::Ttl(stellar_xdr::curr::LedgerKeyTtl {
                key_hash: hash_ledger_key(key),
            }))
        }
        _ => None,
    }
}

/// SHA-256 hash of the XDR-encoded ledger key, returned as an XDR `Hash`.
pub(crate) fn hash_ledger_key(key: &LedgerKey) -> stellar_xdr::curr::Hash {
    let xdr_bytes = key.to_xdr(Limits::none()).expect("XDR encode");
    let hash = henyey_crypto::sha256(&xdr_bytes);
    stellar_xdr::curr::Hash(*hash.as_bytes())
}

/// Format a Unix timestamp as an ISO 8601 UTC string (e.g. `2024-01-15T12:30:00Z`).
pub(crate) fn format_unix_timestamp_utc(unix_ts: u64) -> String {
    let secs = unix_ts as i64;
    let days_since_epoch = secs / 86400;
    let time_of_day = secs % 86400;

    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Calculate year/month/day from days since 1970-01-01
    let mut days = days_since_epoch;
    let mut year = 1970i32;

    loop {
        let days_in_year = if is_leap_year(year) { 366 } else { 365 };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        year += 1;
    }

    let leap = is_leap_year(year);
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];

    let mut month = 0usize;
    for (i, &md) in month_days.iter().enumerate() {
        if days < md {
            month = i;
            break;
        }
        days -= md;
    }

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year,
        month + 1,
        days + 1,
        hours,
        minutes,
        seconds
    )
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_toid_roundtrip() {
        let cases = [
            (1u32, 0u32, 0u32),
            (100, 5, 3),
            (u32::MAX >> 1, (1 << 20) - 1, (1 << 12) - 1), // max values
            (0, 0, 0),
        ];
        for (ledger, tx_order, op_index) in cases {
            let encoded = toid_encode(ledger, tx_order, op_index);
            let (l, t, o) = toid_decode(encoded);
            assert_eq!(
                (l, t, o),
                (ledger, tx_order, op_index),
                "roundtrip failed for ({ledger}, {tx_order}, {op_index})"
            );
        }
    }

    #[test]
    fn test_toid_ordering() {
        // Transactions in later ledgers have higher TOID values
        assert!(toid_encode(2, 0, 0) > toid_encode(1, 0, 0));
        // Higher tx_order in same ledger has higher TOID
        assert!(toid_encode(1, 2, 0) > toid_encode(1, 1, 0));
        // Higher op_index has higher TOID
        assert!(toid_encode(1, 1, 2) > toid_encode(1, 1, 1));
    }

    #[test]
    fn test_toid_parse_cursor() {
        let toid = toid_encode(100, 5, 0);
        let s = toid.to_string();
        assert_eq!(toid_parse_cursor(&s).unwrap(), toid);

        assert!(toid_parse_cursor("not_a_number").is_err());
    }

    #[test]
    fn test_validate_pagination_start_ledger() {
        let (start, cursor, limit) =
            validate_pagination(Some(10), None, None, 5, 200, 1, 100).unwrap();
        assert_eq!(start, 10);
        assert!(cursor.is_none());
        assert_eq!(limit, 5); // default
    }

    #[test]
    fn test_validate_pagination_cursor() {
        let toid = toid_encode(50, 3, 0);
        let cursor_str = toid.to_string();
        let (start, cursor, limit) =
            validate_pagination(None, Some(&cursor_str), Some(20), 5, 200, 1, 100).unwrap();
        assert_eq!(start, 50);
        assert_eq!(cursor, Some(toid));
        assert_eq!(limit, 20);
    }

    #[test]
    fn test_validate_pagination_mutual_exclusion() {
        let toid = toid_encode(50, 3, 0);
        let cursor_str = toid.to_string();
        let result = validate_pagination(Some(10), Some(&cursor_str), None, 5, 200, 1, 100);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_pagination_start_ledger_out_of_range() {
        let result = validate_pagination(Some(200), None, None, 5, 200, 1, 100);
        assert!(result.is_err());

        let result = validate_pagination(Some(0), None, None, 5, 200, 1, 100);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_pagination_limit_clamping() {
        // Over max
        let (_, _, limit) = validate_pagination(Some(10), None, Some(500), 5, 200, 1, 100).unwrap();
        assert_eq!(limit, 200);
        // Under min
        let (_, _, limit) = validate_pagination(Some(10), None, Some(0), 5, 200, 1, 100).unwrap();
        assert_eq!(limit, 1);
    }

    #[test]
    fn test_validate_pagination_missing_both() {
        let result = validate_pagination(None, None, None, 5, 200, 1, 100);
        assert!(result.is_err());
    }

    #[test]
    fn test_determine_tx_status_invalid() {
        assert_eq!(determine_tx_status(&[0, 1, 2]), "FAILED");
    }

    // -----------------------------------------------------------------------
    // Category C: util.rs Gaps
    // -----------------------------------------------------------------------

    // C1-C3: parse_format tests

    #[test]
    fn test_parse_format_none() {
        let params = serde_json::json!({});
        assert_eq!(parse_format(&params).unwrap(), XdrFormat::Base64);
    }

    #[test]
    fn test_parse_format_json() {
        let params = serde_json::json!({"xdrFormat": "json"});
        assert_eq!(parse_format(&params).unwrap(), XdrFormat::Json);
    }

    #[test]
    fn test_parse_format_invalid() {
        let params = serde_json::json!({"xdrFormat": "xml"});
        assert!(parse_format(&params).is_err());
    }

    // C4-C7: determine_tx_status tests

    #[test]
    fn test_determine_tx_status_success() {
        use stellar_xdr::curr::{
            TransactionResult, TransactionResultExt, TransactionResultResult, WriteXdr,
        };
        let result = TransactionResult {
            fee_charged: 100,
            result: TransactionResultResult::TxSuccess(Default::default()),
            ext: TransactionResultExt::V0,
        };
        let pair = TransactionResultPair {
            transaction_hash: stellar_xdr::curr::Hash([0u8; 32]),
            result,
        };
        let bytes = pair.to_xdr(Limits::none()).unwrap();
        assert_eq!(determine_tx_status(&bytes), "SUCCESS");
    }

    #[test]
    fn test_determine_tx_status_failed() {
        use stellar_xdr::curr::{
            TransactionResult, TransactionResultExt, TransactionResultResult, WriteXdr,
        };
        let result = TransactionResult {
            fee_charged: 100,
            result: TransactionResultResult::TxFailed(Default::default()),
            ext: TransactionResultExt::V0,
        };
        let pair = TransactionResultPair {
            transaction_hash: stellar_xdr::curr::Hash([0u8; 32]),
            result,
        };
        let bytes = pair.to_xdr(Limits::none()).unwrap();
        assert_eq!(determine_tx_status(&bytes), "FAILED");
    }

    #[test]
    fn test_determine_tx_status_fee_bump_success() {
        use stellar_xdr::curr::{
            InnerTransactionResult, InnerTransactionResultExt, InnerTransactionResultPair,
            InnerTransactionResultResult, TransactionResult, TransactionResultExt,
            TransactionResultResult, WriteXdr,
        };
        let inner_result = InnerTransactionResultPair {
            transaction_hash: stellar_xdr::curr::Hash([0u8; 32]),
            result: InnerTransactionResult {
                fee_charged: 50,
                result: InnerTransactionResultResult::TxSuccess(Default::default()),
                ext: InnerTransactionResultExt::V0,
            },
        };
        let result = TransactionResult {
            fee_charged: 100,
            result: TransactionResultResult::TxFeeBumpInnerSuccess(inner_result),
            ext: TransactionResultExt::V0,
        };
        let pair = TransactionResultPair {
            transaction_hash: stellar_xdr::curr::Hash([0u8; 32]),
            result,
        };
        let bytes = pair.to_xdr(Limits::none()).unwrap();
        assert_eq!(determine_tx_status(&bytes), "SUCCESS");
    }

    #[test]
    fn test_determine_tx_status_fee_bump_failed() {
        use stellar_xdr::curr::{
            InnerTransactionResult, InnerTransactionResultExt, InnerTransactionResultPair,
            InnerTransactionResultResult, TransactionResult, TransactionResultExt,
            TransactionResultResult, WriteXdr,
        };
        let inner_result = InnerTransactionResultPair {
            transaction_hash: stellar_xdr::curr::Hash([0u8; 32]),
            result: InnerTransactionResult {
                fee_charged: 50,
                result: InnerTransactionResultResult::TxFailed(Default::default()),
                ext: InnerTransactionResultExt::V0,
            },
        };
        let result = TransactionResult {
            fee_charged: 100,
            result: TransactionResultResult::TxFeeBumpInnerFailed(inner_result),
            ext: TransactionResultExt::V0,
        };
        let pair = TransactionResultPair {
            transaction_hash: stellar_xdr::curr::Hash([0u8; 32]),
            result,
        };
        let bytes = pair.to_xdr(Limits::none()).unwrap();
        assert_eq!(determine_tx_status(&bytes), "FAILED");
    }

    // C8-C10: format_unix_timestamp_utc tests

    #[test]
    fn test_format_unix_epoch() {
        assert_eq!(format_unix_timestamp_utc(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn test_format_known_date() {
        assert_eq!(
            format_unix_timestamp_utc(1704067200),
            "2024-01-01T00:00:00Z"
        );
    }

    #[test]
    fn test_format_leap_year() {
        assert_eq!(
            format_unix_timestamp_utc(1709164800),
            "2024-02-29T00:00:00Z"
        );
    }

    // C11-C12: ttl_key_for_ledger_key tests

    #[test]
    fn test_ttl_key_for_contract_data() {
        let key = LedgerKey::ContractData(stellar_xdr::curr::LedgerKeyContractData {
            contract: stellar_xdr::curr::ScAddress::Contract(stellar_xdr::curr::ContractId(
                stellar_xdr::curr::Hash([0xAA; 32]),
            )),
            key: stellar_xdr::curr::ScVal::LedgerKeyContractInstance,
            durability: stellar_xdr::curr::ContractDataDurability::Persistent,
        });
        let ttl_key = ttl_key_for_ledger_key(&key);
        assert!(ttl_key.is_some());
        match ttl_key.unwrap() {
            LedgerKey::Ttl(ttl) => {
                // Should be a hash of the original key
                assert_ne!(ttl.key_hash.0, [0u8; 32]);
            }
            _ => panic!("expected Ttl key"),
        }
    }

    #[test]
    fn test_ttl_key_for_account() {
        let key = LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
            account_id: stellar_xdr::curr::AccountId(
                stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(stellar_xdr::curr::Uint256(
                    [1u8; 32],
                )),
            ),
        });
        assert!(ttl_key_for_ledger_key(&key).is_none());
    }
}
