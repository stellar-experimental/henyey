//! Request and response types for the query server endpoints.
//!
//! The query server provides two endpoints:
//! - `/getledgerentryraw` — Returns raw ledger entries from the bucket list
//! - `/getledgerentry` — Returns entries with TTL state classification

use henyey_common::LedgerSeq;
use serde::Serialize;

/// Response body for `/getledgerentryraw`.
#[derive(Debug, Serialize)]
pub struct GetLedgerEntryRawResponse {
    /// The ledger sequence of the snapshot used.
    #[serde(rename = "ledgerSeq")]
    pub ledger_seq: LedgerSeq,
    /// Found entries, each as a base64-encoded XDR `LedgerEntry`.
    pub entries: Vec<RawEntryResult>,
}

/// A single entry result from `/getledgerentryraw`.
#[derive(Debug, Serialize)]
pub struct RawEntryResult {
    /// Base64-encoded XDR `LedgerEntry`.
    pub entry: String,
}

/// Response body for `/getledgerentry`.
#[derive(Debug, Serialize)]
pub struct GetLedgerEntryResponse {
    /// The ledger sequence of the snapshot used.
    #[serde(rename = "ledgerSeq")]
    pub ledger_seq: LedgerSeq,
    /// Entry results in the same order as the request keys.
    pub entries: Vec<LedgerEntryResult>,
}

/// State classification for a ledger entry.
///
/// Matches stellar-core's state strings in the `/getledgerentry` response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LedgerEntryState {
    /// Entry is live in the bucket list with a valid TTL.
    Live,
    /// Entry is archived (expired persistent entry or in hot archive).
    Archived,
    /// Entry was not found (never existed, or expired temporary).
    NotFound,
}

/// A single entry result from `/getledgerentry`.
#[derive(Debug, Serialize)]
pub struct LedgerEntryResult {
    /// State of the entry.
    pub state: LedgerEntryState,

    /// Base64-encoded XDR `LedgerEntry` (present for `live` and `archived`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,

    /// For Soroban entries: the `live_until_ledger_seq` from the TTL entry.
    /// Present when state is `live` or `archived` (0 for archived).
    #[serde(skip_serializing_if = "Option::is_none", rename = "liveUntilLedgerSeq")]
    pub live_until_ledger_seq: Option<u32>,
}
