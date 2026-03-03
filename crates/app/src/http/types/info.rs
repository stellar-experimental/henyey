//! Types for info, status, health, and ledger endpoints.

use serde::{Deserialize, Serialize};

/// Response for the root endpoint.
#[derive(Serialize)]
pub struct RootResponse {
    pub name: String,
    pub version: String,
    pub endpoints: Vec<String>,
}

/// Response for the /info endpoint.
#[derive(Serialize)]
pub struct InfoResponse {
    /// Build version string (e.g. "henyey-v0.1.0").
    pub build: String,
    /// Protocol version of the current ledger.
    pub protocol_version: u32,
    /// Current application state.
    pub state: String,
    /// ISO 8601 UTC timestamp of when the node started.
    pub started_on: String,
    /// Node uptime in seconds.
    pub uptime_secs: u64,
    /// Node name from configuration.
    pub node_name: String,
    /// Node public key (strkey).
    pub public_key: String,
    /// Network passphrase.
    pub network_passphrase: String,
    /// Whether this node is a validator.
    pub is_validator: bool,
    /// Current ledger summary.
    pub ledger: InfoLedgerSummary,
    /// Peer counts.
    pub peers: InfoPeerSummary,
}

/// Ledger summary embedded in the /info response.
#[derive(Serialize)]
pub struct InfoLedgerSummary {
    pub num: u32,
    pub hash: String,
    pub close_time: u64,
    pub version: u32,
    pub base_fee: u32,
    pub base_reserve: u32,
    pub max_tx_set_size: u32,
    pub flags: u32,
    /// Seconds since last ledger close.
    pub age: u64,
}

/// Peer counts embedded in the /info response.
#[derive(Serialize)]
pub struct InfoPeerSummary {
    pub pending_count: usize,
    pub authenticated_count: usize,
}

/// Response for the /ledger endpoint.
#[derive(Serialize)]
pub struct LedgerResponse {
    pub sequence: u32,
    pub hash: String,
    pub close_time: u64,
    pub protocol_version: u32,
}

/// Response for the /health endpoint.
#[derive(Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub state: String,
    pub ledger_seq: u32,
    pub peer_count: usize,
}

/// Response for the /upgrades endpoint.
#[derive(Serialize)]
pub struct UpgradesResponse {
    pub current: UpgradeState,
    pub proposed: Vec<UpgradeItem>,
}

#[derive(Serialize)]
pub struct UpgradeState {
    pub protocol_version: u32,
    pub base_fee: u32,
    pub base_reserve: u32,
    pub max_tx_set_size: u32,
}

#[derive(Serialize)]
pub struct UpgradeItem {
    pub r#type: String,
    pub value: u32,
}

/// Response for the /self-check endpoint.
#[derive(Serialize)]
pub struct SelfCheckResponse {
    pub ok: bool,
    pub checked_ledgers: u32,
    pub last_checked_ledger: Option<u32>,
    pub message: Option<String>,
}

/// Response for the /metrics endpoint (Prometheus format).
#[derive(Serialize)]
pub struct MetricsResponse {
    pub ledger_seq: u32,
    pub peer_count: usize,
    pub pending_transactions: u64,
    pub uptime_seconds: u64,
    pub state: String,
    pub is_validator: bool,
}

/// Query parameters for /dumpproposedsettings endpoint.
#[derive(Deserialize)]
pub struct DumpProposedSettingsParams {
    pub blob: Option<String>,
}
