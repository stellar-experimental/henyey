//! Shared utility functions for the RPC crate.

use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde::Serialize;
use stellar_xdr::curr::{
    DiagnosticEvent, LedgerCloseMeta, LedgerHeaderHistoryEntry, LedgerKey, Limits, ReadXdr,
    TransactionMeta, TransactionResultPair, WriteXdr,
};
use tokio::sync::Semaphore;

use crate::context::RpcContext;
use crate::error::JsonRpcError;

// ---------------------------------------------------------------------------
// XDR / Base64 parse error type for RPC helpers
// ---------------------------------------------------------------------------

/// Typed error for XDR parse, XDR serialize, and base64 decode failures
/// in RPC read-path helpers. Distinct from `simulate::ConversionError`
/// (cross-version p25↔workspace conversion).
#[derive(Debug)]
pub(crate) enum RpcXdrError {
    /// XDR deserialization failed.
    XdrParse {
        type_name: &'static str,
        cause: String,
    },
    /// XDR serialization failed.
    XdrSerialize {
        type_name: &'static str,
        cause: String,
    },
    /// Base64 decoding failed.
    Base64Decode { cause: String },
    /// DB row key disagrees with the ledger sequence inside the XDR blob.
    SequenceMismatch { db_seq: u32, parsed_seq: u32 },
}

impl std::fmt::Display for RpcXdrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcXdrError::XdrParse { type_name, cause } => {
                write!(f, "XDR parse of {type_name} failed: {cause}")
            }
            RpcXdrError::XdrSerialize { type_name, cause } => {
                write!(f, "XDR serialize of {type_name} failed: {cause}")
            }
            RpcXdrError::Base64Decode { cause } => {
                write!(f, "base64 decode failed: {cause}")
            }
            RpcXdrError::SequenceMismatch { db_seq, parsed_seq } => {
                write!(
                    f,
                    "DB/XDR sequence mismatch: db_seq={db_seq}, parsed_seq={parsed_seq}"
                )
            }
        }
    }
}

impl From<RpcXdrError> for JsonRpcError {
    fn from(e: RpcXdrError) -> Self {
        JsonRpcError::internal_logged("XDR data integrity error", &e)
    }
}

// ---------------------------------------------------------------------------
// Bounded blocking helpers
// ---------------------------------------------------------------------------

/// Generic error for semaphore-bounded blocking execution.
#[derive(Debug)]
pub(crate) enum BlockingError<E> {
    /// The closure returned an error.
    Inner(E),
    /// The `spawn_blocking` task panicked or was cancelled.
    JoinError(tokio::task::JoinError),
    /// The semaphore was closed (dropped).
    SemaphoreClosed,
    /// The semaphore had no available permits (try-acquire only).
    SemaphoreFull,
}

impl<E: std::fmt::Display> std::fmt::Display for BlockingError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BlockingError::Inner(e) => write!(f, "{e}"),
            BlockingError::JoinError(e) => write!(f, "task join error: {e}"),
            BlockingError::SemaphoreClosed => write!(f, "semaphore closed"),
            BlockingError::SemaphoreFull => write!(f, "semaphore full"),
        }
    }
}

/// Run a closure on a blocking thread with semaphore-bounded concurrency.
///
/// Acquires an `OwnedSemaphorePermit` before spawning, then moves it into
/// the blocking closure. The permit is held until the blocking work completes,
/// even if the caller's async future is cancelled (e.g. by a timeout).
pub(crate) async fn bounded_blocking<T, E, F>(
    semaphore: &Arc<Semaphore>,
    f: F,
) -> Result<T, BlockingError<E>>
where
    T: Send + 'static,
    E: Send + 'static,
    F: FnOnce() -> Result<T, E> + Send + 'static,
{
    let permit = semaphore
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| BlockingError::SemaphoreClosed)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit; // held until closure returns
        f()
    })
    .await
    .map_err(BlockingError::JoinError)?
    .map_err(BlockingError::Inner)
}

/// Like [`bounded_blocking`], but uses `try_acquire_owned` for immediate
/// rejection when the semaphore is full (no backpressure).
pub(crate) async fn try_bounded_blocking<T, E, F>(
    semaphore: &Arc<Semaphore>,
    f: F,
) -> Result<T, BlockingError<E>>
where
    T: Send + 'static,
    E: Send + 'static,
    F: FnOnce() -> Result<T, E> + Send + 'static,
{
    let permit = semaphore.clone().try_acquire_owned().map_err(|e| match e {
        tokio::sync::TryAcquireError::Closed => BlockingError::SemaphoreClosed,
        tokio::sync::TryAcquireError::NoPermits => BlockingError::SemaphoreFull,
    })?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit; // held until closure returns
        f()
    })
    .await
    .map_err(BlockingError::JoinError)?
    .map_err(BlockingError::Inner)
}

// ---------------------------------------------------------------------------
// Async DB access
// ---------------------------------------------------------------------------

/// Internal error type for RPC database access — not exposed to clients.
#[derive(Debug)]
pub(crate) enum DbAccessError {
    Db(henyey_db::DbError),
    JoinError(tokio::task::JoinError),
    SemaphoreClosed,
}

impl std::fmt::Display for DbAccessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DbAccessError::Db(e) => write!(f, "database error: {e}"),
            DbAccessError::JoinError(e) => write!(f, "task join error: {e}"),
            DbAccessError::SemaphoreClosed => write!(f, "semaphore closed"),
        }
    }
}

impl From<BlockingError<henyey_db::DbError>> for DbAccessError {
    fn from(e: BlockingError<henyey_db::DbError>) -> Self {
        match e {
            BlockingError::Inner(e) => DbAccessError::Db(e),
            BlockingError::JoinError(e) => DbAccessError::JoinError(e),
            BlockingError::SemaphoreClosed | BlockingError::SemaphoreFull => {
                DbAccessError::SemaphoreClosed
            }
        }
    }
}

/// Run a synchronous database closure on a blocking thread, with
/// semaphore-bounded concurrency.
///
/// Delegates to [`bounded_blocking`] with the DB semaphore. The permit is
/// held inside the blocking closure until the DB work completes, even if
/// the caller's future is cancelled by a timeout.
pub(crate) async fn blocking_db<T, F>(ctx: &RpcContext, f: F) -> Result<T, DbAccessError>
where
    T: Send + 'static,
    F: FnOnce(&henyey_db::Database) -> Result<T, henyey_db::DbError> + Send + 'static,
{
    let db = ctx.app.database().clone();
    bounded_blocking(&ctx.db_semaphore, move || f(&db))
        .await
        .map_err(DbAccessError::from)
}

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
                .map_err(|e| JsonRpcError::internal_logged("serialization error", &e))?;
            obj.insert(key, serde_json::Value::String(BASE64.encode(&bytes)));
        }
        XdrFormat::Json => {
            let json_val = serde_json::to_value(val)
                .map_err(|e| JsonRpcError::internal_logged("serialization error", &e))?;
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
    // Always validate stored bytes, regardless of output format.
    let parsed = T::from_xdr(bytes, Limits::none()).map_err(|e| RpcXdrError::XdrParse {
        type_name: std::any::type_name::<T>(),
        cause: e.to_string(),
    })?;
    match format {
        XdrFormat::Base64 => {
            obj.insert(
                format!("{base_name}Xdr"),
                serde_json::Value::String(BASE64.encode(bytes)),
            );
        }
        XdrFormat::Json => {
            let json_val = serde_json::to_value(&parsed)
                .map_err(|e| JsonRpcError::internal_logged("serialization error", &e))?;
            obj.insert(format!("{base_name}Json"), json_val);
        }
    }
    Ok(())
}

/// Insert an XDR field using a pre-parsed value and its raw bytes.
///
/// Like [`insert_raw_xdr_field`] but avoids re-parsing: the caller has already
/// deserialized the XDR bytes into `parsed`. In Base64 mode the raw `bytes` are
/// encoded directly; in JSON mode the pre-parsed value is serialized.
pub(crate) fn insert_pre_parsed_xdr_field<T: Serialize>(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    base_name: &str,
    bytes: &[u8],
    parsed: &T,
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
            let json_val = serde_json::to_value(parsed)
                .map_err(|e| JsonRpcError::internal_logged("serialization error", &e))?;
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
                        .map_err(|e| JsonRpcError::internal_logged("serialization error", &e))
                })
                .collect::<Result<_, _>>()?;
            obj.insert(key, serde_json::Value::Array(encoded));
        }
        XdrFormat::Json => {
            let json_items: Vec<serde_json::Value> = items
                .iter()
                .map(|item| {
                    serde_json::to_value(item)
                        .map_err(|e| JsonRpcError::internal_logged("serialization error", &e))
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

/// Parse a [`LedgerCloseMeta`] from raw bytes and validate that the embedded
/// ledger sequence matches the expected `db_seq` from the database row key.
///
/// Returns [`RpcXdrError::XdrParse`] if XDR deserialization fails, or
/// [`RpcXdrError::SequenceMismatch`] if the parsed sequence disagrees with
/// `db_seq` (indicating data corruption).
pub(crate) fn parse_ledger_close_meta_checked(
    db_seq: u32,
    meta_bytes: &[u8],
) -> Result<LedgerCloseMeta, RpcXdrError> {
    let lcm = LedgerCloseMeta::from_xdr(meta_bytes, Limits::none()).map_err(|e| {
        RpcXdrError::XdrParse {
            type_name: "LedgerCloseMeta",
            cause: e.to_string(),
        }
    })?;

    let parsed_seq = ledger_header_entry(&lcm).header.ledger_seq;
    if parsed_seq != db_seq {
        return Err(RpcXdrError::SequenceMismatch { db_seq, parsed_seq });
    }

    Ok(lcm)
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
    lctx: &LedgerContext,
) -> Result<(u32, Option<i64>, u32), crate::error::JsonRpcError> {
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

    if start < lctx.oldest_ledger || start > lctx.latest_ledger {
        return Err(JsonRpcError::invalid_params(format!(
            "startLedger must be within [{}, {}]",
            lctx.oldest_ledger, lctx.latest_ledger
        )));
    }

    Ok((start, None, limit))
}

// ---------------------------------------------------------------------------
// Transaction helpers (shared by getTransaction and getTransactions)
// ---------------------------------------------------------------------------

/// Derive the transaction status string from `TxStatus`.
pub(crate) fn tx_status_str(status: henyey_db::TxStatus) -> &'static str {
    match status {
        henyey_db::TxStatus::Success => "SUCCESS",
        henyey_db::TxStatus::Failed => "FAILED",
    }
}

/// Extract just the `TransactionResult` XDR bytes from a stored `TransactionResultPair` blob.
///
/// Returns an error if the stored bytes are corrupt (cannot parse as
/// `TransactionResultPair` or cannot re-serialize the inner `TransactionResult`).
pub(crate) fn extract_result_xdr(result_pair_bytes: &[u8]) -> Result<Vec<u8>, RpcXdrError> {
    let pair = TransactionResultPair::from_xdr(result_pair_bytes, Limits::none()).map_err(|e| {
        RpcXdrError::XdrParse {
            type_name: "TransactionResultPair",
            cause: e.to_string(),
        }
    })?;
    pair.result
        .to_xdr(Limits::none())
        .map_err(|e| RpcXdrError::XdrSerialize {
            type_name: "TransactionResult",
            cause: e.to_string(),
        })
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
    /// Build from the running app state, batching DB lookups into one blocking call.
    pub async fn from_app(ctx: &RpcContext) -> Result<Self, JsonRpcError> {
        let summary = ctx.app.ledger_summary();
        let (oldest, oldest_close) = blocking_db(ctx, move |db| {
            db.with_connection(|conn| {
                use henyey_db::LedgerQueries;
                match conn.get_oldest_ledger_info()? {
                    Some((seq, close_time)) => Ok((seq, close_time)),
                    // Empty DB at startup — no headers yet
                    None => Ok((1, 0)),
                }
            })
        })
        .await
        .map_err(|e| {
            tracing::warn!(error = ?e, "LedgerContext DB error");
            JsonRpcError::internal("database error")
        })?;
        Ok(Self {
            latest_ledger: summary.num,
            latest_close_time: summary.close_time,
            oldest_ledger: oldest,
            oldest_close_time: oldest_close,
        })
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
pub(crate) fn is_fee_bump_envelope(envelope_bytes: &[u8]) -> Result<bool, RpcXdrError> {
    use stellar_xdr::curr::TransactionEnvelope;
    let env = TransactionEnvelope::from_xdr(envelope_bytes, Limits::none()).map_err(|e| {
        RpcXdrError::XdrParse {
            type_name: "TransactionEnvelope",
            cause: e.to_string(),
        }
    })?;
    Ok(matches!(env, TransactionEnvelope::TxFeeBump(_)))
}

/// Extract diagnostic events from `TransactionMeta` bytes.
///
/// Returns:
/// - `Ok(None)` if no diagnostic events are present (non-Soroban meta V0/V1/V2,
///   V3 with no `soroban_meta`, or V3/V4 with empty diagnostic events).
/// - `Ok(Some(...))` if V3/V4 meta contains non-empty diagnostic events.
/// - `Err` if the meta bytes cannot be parsed as `TransactionMeta`.
pub(crate) fn extract_diagnostic_events(
    meta_bytes: &[u8],
) -> Result<Option<Vec<DiagnosticEvent>>, RpcXdrError> {
    let meta = TransactionMeta::from_xdr(meta_bytes, Limits::none()).map_err(|e| {
        RpcXdrError::XdrParse {
            type_name: "TransactionMeta",
            cause: e.to_string(),
        }
    })?;

    let events: &[DiagnosticEvent] = match &meta {
        TransactionMeta::V3(v3) => v3
            .soroban_meta
            .as_ref()
            .map(|sm| sm.diagnostic_events.as_slice())
            .unwrap_or(&[]),
        TransactionMeta::V4(v4) => v4.diagnostic_events.as_slice(),
        _ => return Ok(None),
    };

    if events.is_empty() {
        Ok(None)
    } else {
        Ok(Some(events.to_vec()))
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
    if let Some(events) = extract_diagnostic_events(meta_bytes)? {
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
        let lctx = LedgerContext {
            latest_ledger: 100,
            latest_close_time: 0,
            oldest_ledger: 1,
            oldest_close_time: 0,
        };
        let (start, cursor, limit) =
            validate_pagination(Some(10), None, None, 5, 200, &lctx).unwrap();
        assert_eq!(start, 10);
        assert!(cursor.is_none());
        assert_eq!(limit, 5); // default
    }

    #[test]
    fn test_validate_pagination_cursor() {
        let lctx = LedgerContext {
            latest_ledger: 100,
            latest_close_time: 0,
            oldest_ledger: 1,
            oldest_close_time: 0,
        };
        let toid = toid_encode(50, 3, 0);
        let cursor_str = toid.to_string();
        let (start, cursor, limit) =
            validate_pagination(None, Some(&cursor_str), Some(20), 5, 200, &lctx).unwrap();
        assert_eq!(start, 50);
        assert_eq!(cursor, Some(toid));
        assert_eq!(limit, 20);
    }

    #[test]
    fn test_validate_pagination_mutual_exclusion() {
        let lctx = LedgerContext {
            latest_ledger: 100,
            latest_close_time: 0,
            oldest_ledger: 1,
            oldest_close_time: 0,
        };
        let toid = toid_encode(50, 3, 0);
        let cursor_str = toid.to_string();
        let result = validate_pagination(Some(10), Some(&cursor_str), None, 5, 200, &lctx);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_pagination_start_ledger_out_of_range() {
        let lctx = LedgerContext {
            latest_ledger: 100,
            latest_close_time: 0,
            oldest_ledger: 1,
            oldest_close_time: 0,
        };
        let result = validate_pagination(Some(200), None, None, 5, 200, &lctx);
        assert!(result.is_err());

        let result = validate_pagination(Some(0), None, None, 5, 200, &lctx);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_pagination_limit_clamping() {
        let lctx = LedgerContext {
            latest_ledger: 100,
            latest_close_time: 0,
            oldest_ledger: 1,
            oldest_close_time: 0,
        };
        // Over max
        let (_, _, limit) = validate_pagination(Some(10), None, Some(500), 5, 200, &lctx).unwrap();
        assert_eq!(limit, 200);
        // Under min
        let (_, _, limit) = validate_pagination(Some(10), None, Some(0), 5, 200, &lctx).unwrap();
        assert_eq!(limit, 1);
    }

    #[test]
    fn test_validate_pagination_missing_both() {
        let lctx = LedgerContext {
            latest_ledger: 100,
            latest_close_time: 0,
            oldest_ledger: 1,
            oldest_close_time: 0,
        };
        let result = validate_pagination(None, None, None, 5, 200, &lctx);
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_result_xdr_invalid() {
        assert!(extract_result_xdr(&[0, 1, 2]).is_err());
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

    // C4: tx_status_str tests

    #[test]
    fn test_tx_status_str() {
        assert_eq!(tx_status_str(henyey_db::TxStatus::Success), "SUCCESS");
        assert_eq!(tx_status_str(henyey_db::TxStatus::Failed), "FAILED");
    }

    // C5-C8: extract_result_xdr tests

    #[test]
    fn test_extract_result_xdr_success() {
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
            result: result.clone(),
        };
        let bytes = pair.to_xdr(Limits::none()).unwrap();
        let extracted = extract_result_xdr(&bytes).unwrap();
        // Round-trip: the extracted bytes should parse as TransactionResult
        let parsed =
            stellar_xdr::curr::TransactionResult::from_xdr(&extracted, Limits::none()).unwrap();
        assert_eq!(parsed.fee_charged, 100);
    }

    #[test]
    fn test_extract_result_xdr_corrupt_bytes() {
        let result = extract_result_xdr(&[0xff, 0xfe, 0xfd]);
        assert!(result.is_err());
    }

    // C9-C11: is_fee_bump_envelope tests

    #[test]
    fn test_is_fee_bump_envelope_non_fee_bump() {
        use stellar_xdr::curr::{
            Transaction, TransactionEnvelope, TransactionV1Envelope, WriteXdr,
        };
        let tx = Transaction {
            source_account: stellar_xdr::curr::MuxedAccount::Ed25519(stellar_xdr::curr::Uint256(
                [0u8; 32],
            )),
            fee: 100,
            seq_num: stellar_xdr::curr::SequenceNumber(1),
            cond: stellar_xdr::curr::Preconditions::None,
            memo: stellar_xdr::curr::Memo::None,
            operations: vec![].try_into().unwrap(),
            ext: stellar_xdr::curr::TransactionExt::V0,
        };
        let env = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![].try_into().unwrap(),
        });
        let bytes = env.to_xdr(Limits::none()).unwrap();
        assert_eq!(is_fee_bump_envelope(&bytes).unwrap(), false);
    }

    #[test]
    fn test_is_fee_bump_envelope_corrupt_bytes() {
        assert!(is_fee_bump_envelope(&[0xff, 0xfe]).is_err());
    }

    // C12-C16: extract_diagnostic_events tests

    #[test]
    fn test_extract_diagnostic_events_corrupt_bytes() {
        assert!(extract_diagnostic_events(&[0xff, 0xfe, 0xfd]).is_err());
    }

    #[test]
    fn test_extract_diagnostic_events_v0_meta() {
        use stellar_xdr::curr::{TransactionMeta, WriteXdr};
        let meta = TransactionMeta::V0(Default::default());
        let bytes = meta.to_xdr(Limits::none()).unwrap();
        assert!(extract_diagnostic_events(&bytes).unwrap().is_none());
    }

    #[test]
    fn test_extract_diagnostic_events_v3_no_soroban() {
        use stellar_xdr::curr::{TransactionMeta, TransactionMetaV3, WriteXdr};
        let meta = TransactionMeta::V3(TransactionMetaV3 {
            ext: stellar_xdr::curr::ExtensionPoint::V0,
            tx_changes_before: Default::default(),
            operations: Default::default(),
            tx_changes_after: Default::default(),
            soroban_meta: None,
        });
        let bytes = meta.to_xdr(Limits::none()).unwrap();
        assert!(extract_diagnostic_events(&bytes).unwrap().is_none());
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

    // -------------------------------------------------------------------
    // blocking_db cancellation-safety (#1743)
    // -------------------------------------------------------------------

    /// `blocking_db` moves an `OwnedSemaphorePermit` into the
    /// `spawn_blocking` closure.  If the caller's future is cancelled
    /// (e.g. by a timeout), the permit must remain held until the blocking
    /// work finishes — otherwise the DB concurrency bound is violated.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_blocking_db_permit_survives_cancellation() {
        use std::sync::Arc as StdArc;
        use std::time::Duration;

        use henyey_app::config::QuorumSetConfig;
        use henyey_app::AppState;
        use henyey_common::Hash256;
        use henyey_crypto::SecretKey;
        use henyey_simulation::{Simulation, SimulationMode};

        // --- Boot a minimal App ---
        let mut sim =
            Simulation::with_network(SimulationMode::OverTcp, "Test SDF Network ; September 2015");
        let seed = Hash256::hash(b"RPC_UTIL_CANCEL_TEST_NODE");
        let secret = SecretKey::from_seed(&seed.0);
        let quorum_set = QuorumSetConfig {
            threshold_percent: 100,
            validators: vec![secret.public_key().to_strkey()],
            inner_sets: Vec::new(),
        };
        sim.add_app_node("node0", secret, quorum_set);
        sim.start_all_nodes().await;
        let app = sim.app("node0").expect("app");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            if app.state().await == AppState::Validating {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // --- Build RpcContext with db_semaphore capacity = 1 ---
        let ctx = StdArc::new(RpcContext {
            app: app.clone(),
            fee_windows: StdArc::new(crate::fee_window::FeeWindows::new(10)),
            simulation_semaphore: StdArc::new(tokio::sync::Semaphore::new(1)),
            request_semaphore: StdArc::new(tokio::sync::Semaphore::new(1)),
            db_semaphore: StdArc::new(tokio::sync::Semaphore::new(1)),
            bucket_io_semaphore: StdArc::new(tokio::sync::Semaphore::new(1)),
            request_timeout: Duration::from_secs(30),
        });

        // Channel: closure signals "I'm running and hold the permit"
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        // Barrier: test tells closure "you may finish now"
        let barrier = StdArc::new(std::sync::Barrier::new(2));
        let barrier2 = barrier.clone();

        let ctx2 = ctx.clone();
        let handle = tokio::spawn(async move {
            blocking_db(&ctx2, move |_db| {
                // Signal: we are inside spawn_blocking, permit is held.
                let _ = tx.send(());
                // Wait for the test to tell us to finish.
                barrier2.wait();
                Ok(())
            })
            .await
        });

        // Wait for the closure to start (permit is now held inside spawn_blocking).
        rx.recv().expect("closure must signal start");

        // Cancel the outer future.
        handle.abort();
        let join_result = handle.await;
        assert!(
            join_result.unwrap_err().is_cancelled(),
            "task must be cancelled"
        );

        // The permit must still be held by the blocking thread.
        assert!(
            ctx.db_semaphore.try_acquire().is_err(),
            "permit must still be held after caller cancellation"
        );

        // Let the closure finish — releases the permit.
        barrier.wait();

        // Poll until the permit is returned (bounded, no fixed sleep).
        let mut acquired = false;
        for _ in 0..100 {
            if ctx.db_semaphore.try_acquire().is_ok() {
                acquired = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            acquired,
            "db_semaphore permit must be returned after blocking work completes"
        );

        drop(sim);
    }

    // -------------------------------------------------------------------
    // bounded_blocking cancellation-safety
    // -------------------------------------------------------------------

    /// `bounded_blocking` moves an `OwnedSemaphorePermit` into the
    /// `spawn_blocking` closure.  If the caller's future is cancelled
    /// (e.g. by a timeout), the permit must remain held until the blocking
    /// work finishes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bounded_blocking_permit_survives_cancellation() {
        use std::sync::Arc as StdArc;
        use std::time::Duration;

        let sem = StdArc::new(tokio::sync::Semaphore::new(1));

        // Channel: closure signals "I'm running and hold the permit"
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        // Barrier: test tells closure "you may finish now"
        let barrier = StdArc::new(std::sync::Barrier::new(2));
        let barrier2 = barrier.clone();

        let sem2 = sem.clone();
        let handle = tokio::spawn(async move {
            bounded_blocking(&sem2, move || {
                let _ = tx.send(());
                barrier2.wait();
                Ok::<(), String>(())
            })
            .await
        });

        // Wait for the closure to start.
        rx.recv().expect("closure must signal start");

        // Cancel the outer future.
        handle.abort();
        let join_result = handle.await;
        assert!(
            join_result.unwrap_err().is_cancelled(),
            "task must be cancelled"
        );

        // The permit must still be held by the blocking thread.
        assert!(
            sem.try_acquire().is_err(),
            "permit must still be held after caller cancellation"
        );

        // Let the closure finish — releases the permit.
        barrier.wait();

        // Poll until the permit is returned.
        let mut acquired = false;
        for _ in 0..100 {
            if sem.try_acquire().is_ok() {
                acquired = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            acquired,
            "permit must be returned after blocking work completes"
        );
    }

    // -------------------------------------------------------------------
    // try_bounded_blocking rejection and closed
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_try_bounded_blocking_rejects_when_full() {
        use std::sync::Arc as StdArc;

        let sem = StdArc::new(tokio::sync::Semaphore::new(1));
        // Exhaust the single permit.
        let _held = sem.clone().try_acquire_owned().unwrap();

        let result = try_bounded_blocking(&sem, || Ok::<(), String>(())).await;
        assert!(
            matches!(result, Err(BlockingError::SemaphoreFull)),
            "must return SemaphoreFull when permits exhausted"
        );
    }

    #[tokio::test]
    async fn test_try_bounded_blocking_detects_closed_semaphore() {
        use std::sync::Arc as StdArc;

        let sem = StdArc::new(tokio::sync::Semaphore::new(1));
        sem.close();

        let result = try_bounded_blocking(&sem, || Ok::<(), String>(())).await;
        assert!(
            matches!(result, Err(BlockingError::SemaphoreClosed)),
            "must return SemaphoreClosed when semaphore is closed"
        );
    }

    #[tokio::test]
    async fn test_bounded_blocking_detects_closed_semaphore() {
        use std::sync::Arc as StdArc;

        let sem = StdArc::new(tokio::sync::Semaphore::new(1));
        sem.close();

        let result = bounded_blocking(&sem, || Ok::<(), String>(())).await;
        assert!(
            matches!(result, Err(BlockingError::SemaphoreClosed)),
            "must return SemaphoreClosed when semaphore is closed"
        );
    }

    // -------------------------------------------------------------------
    // Simulation-style cancellation regression
    // -------------------------------------------------------------------

    /// Simulates the pattern used by try_bounded_blocking in simulation:
    /// acquire permit, run blocking work, cancel outer future. The permit
    /// must survive and concurrent try_acquire must fail.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_try_bounded_blocking_permit_survives_cancellation() {
        use std::sync::Arc as StdArc;
        use std::time::Duration;

        let sem = StdArc::new(tokio::sync::Semaphore::new(1));

        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let barrier = StdArc::new(std::sync::Barrier::new(2));
        let barrier2 = barrier.clone();

        let sem2 = sem.clone();
        let handle = tokio::spawn(async move {
            try_bounded_blocking(&sem2, move || {
                let _ = tx.send(());
                barrier2.wait();
                Ok::<(), String>(())
            })
            .await
        });

        rx.recv().expect("closure must signal start");

        // Cancel the outer future.
        handle.abort();
        let _ = handle.await;

        // Permit is still held — concurrent simulation attempt must fail.
        assert!(
            sem.clone().try_acquire_owned().is_err(),
            "permit must still be held after cancellation"
        );

        // A second try_bounded_blocking must return SemaphoreFull.
        let result = try_bounded_blocking(&sem, || Ok::<(), String>(())).await;
        assert!(
            matches!(result, Err(BlockingError::SemaphoreFull)),
            "concurrent simulation must be rejected"
        );

        // Let the closure finish.
        barrier.wait();

        // Poll until the permit is returned.
        let mut acquired = false;
        for _ in 0..100 {
            if sem.try_acquire().is_ok() {
                acquired = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            acquired,
            "permit must be returned after blocking work completes"
        );
    }

    // -------------------------------------------------------------------
    // Bucket I/O (get_ledger_entries) cancellation-safety regression (#1744)
    // -------------------------------------------------------------------

    /// `bounded_blocking` with the bucket_io_semaphore must hold the permit
    /// inside the blocking closure even after the caller is cancelled.
    /// This simulates the get_ledger_entries handler path: timeout fires,
    /// but the bucket read's semaphore permit stays held until the read finishes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bucket_io_permit_held_after_timeout_cancellation() {
        use std::sync::Arc as StdArc;
        use std::time::Duration;

        let sem = StdArc::new(tokio::sync::Semaphore::new(1));

        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let barrier = StdArc::new(std::sync::Barrier::new(2));
        let barrier2 = barrier.clone();

        let sem2 = sem.clone();
        // Simulate a bucket read that blocks (e.g. disk I/O).
        let handle = tokio::spawn(async move {
            bounded_blocking(&sem2, move || {
                let _ = tx.send(());
                barrier2.wait();
                Ok::<Vec<u8>, String>(vec![1, 2, 3])
            })
            .await
        });

        // Wait for the "bucket read" to start.
        rx.recv().expect("closure must signal start");

        // Simulate timeout cancellation of the outer request.
        handle.abort();
        let join_result = handle.await;
        assert!(
            join_result.unwrap_err().is_cancelled(),
            "task must be cancelled"
        );

        // The bucket_io permit must still be held by the blocking thread.
        assert!(
            sem.try_acquire().is_err(),
            "bucket_io_semaphore permit must be held after timeout cancellation"
        );

        // Let the "bucket read" finish.
        barrier.wait();

        // Poll until the permit is returned.
        let mut acquired = false;
        for _ in 0..100 {
            if sem.try_acquire().is_ok() {
                acquired = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            acquired,
            "bucket_io_semaphore permit must be returned after bucket read completes"
        );
    }

    /// When bucket_io_semaphore is exhausted, `bounded_blocking` must wait
    /// (backpressure) rather than reject immediately. Once a permit is
    /// released, the blocked caller proceeds.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bucket_io_bounded_blocking_waits_for_permit() {
        use std::sync::Arc as StdArc;
        use std::time::Duration;

        let sem = StdArc::new(tokio::sync::Semaphore::new(1));
        // Hold the single permit.
        let held = sem.clone().try_acquire_owned().unwrap();

        let sem2 = sem.clone();
        // Start a bounded_blocking that will wait for the permit.
        let handle =
            tokio::spawn(async move { bounded_blocking(&sem2, || Ok::<i32, String>(42)).await });

        // Give it a moment to start waiting.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Release the held permit.
        drop(held);

        // The blocked call should now complete successfully.
        let result = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("should complete within 5s")
            .expect("join should succeed");
        assert_eq!(
            result.unwrap(),
            42,
            "bounded_blocking should return the closure result"
        );
    }

    // -----------------------------------------------------------------------
    // parse_ledger_close_meta_checked tests
    // -----------------------------------------------------------------------

    /// Build a minimal `LedgerCloseMeta` with the given ledger sequence and
    /// serialize it to XDR bytes.
    fn make_lcm_bytes(seq: u32) -> Vec<u8> {
        use stellar_xdr::curr::{
            Hash, LedgerCloseMetaV0, LedgerHeader, LedgerHeaderExt, LedgerHeaderHistoryEntryExt,
            StellarValue, StellarValueExt, TimePoint, TransactionSet, WriteXdr,
        };

        let lcm = LedgerCloseMeta::V0(LedgerCloseMetaV0 {
            ledger_header: LedgerHeaderHistoryEntry {
                hash: Hash([0; 32]),
                header: LedgerHeader {
                    ledger_version: 21,
                    previous_ledger_hash: Hash([0; 32]),
                    scp_value: StellarValue {
                        tx_set_hash: Hash([0; 32]),
                        close_time: TimePoint(0),
                        upgrades: vec![].try_into().unwrap(),
                        ext: StellarValueExt::Basic,
                    },
                    tx_set_result_hash: Hash([0; 32]),
                    bucket_list_hash: Hash([0; 32]),
                    ledger_seq: seq,
                    total_coins: 0,
                    fee_pool: 0,
                    inflation_seq: 0,
                    id_pool: 0,
                    base_fee: 100,
                    base_reserve: 5_000_000,
                    max_tx_set_size: 100,
                    skip_list: [Hash([0; 32]), Hash([0; 32]), Hash([0; 32]), Hash([0; 32])],
                    ext: LedgerHeaderExt::V0,
                },
                ext: LedgerHeaderHistoryEntryExt::V0,
            },
            tx_set: TransactionSet {
                previous_ledger_hash: Hash([0; 32]),
                txs: vec![].try_into().unwrap(),
            },
            tx_processing: vec![].try_into().unwrap(),
            upgrades_processing: vec![].try_into().unwrap(),
            scp_info: vec![].try_into().unwrap(),
        });
        lcm.to_xdr(Limits::none()).unwrap()
    }

    #[test]
    fn test_parse_lcm_checked_happy_path() {
        let bytes = make_lcm_bytes(42);
        let lcm = parse_ledger_close_meta_checked(42, &bytes).unwrap();
        assert_eq!(ledger_header_entry(&lcm).header.ledger_seq, 42);
    }

    #[test]
    fn test_parse_lcm_checked_sequence_mismatch() {
        let bytes = make_lcm_bytes(42);
        let err = parse_ledger_close_meta_checked(99, &bytes).unwrap_err();
        match &err {
            RpcXdrError::SequenceMismatch { db_seq, parsed_seq } => {
                assert_eq!(*db_seq, 99);
                assert_eq!(*parsed_seq, 42);
            }
            other => panic!("expected SequenceMismatch, got: {other:?}"),
        }
        assert!(
            err.to_string().contains("mismatch"),
            "Display should mention mismatch: {}",
            err
        );
    }

    #[test]
    fn test_parse_lcm_checked_invalid_xdr() {
        let garbage = &[0xFF, 0xFE, 0xFD];
        let err = parse_ledger_close_meta_checked(1, garbage).unwrap_err();
        match &err {
            RpcXdrError::XdrParse { type_name, .. } => {
                assert_eq!(*type_name, "LedgerCloseMeta");
            }
            other => panic!("expected XdrParse, got: {other:?}"),
        }
    }
}
