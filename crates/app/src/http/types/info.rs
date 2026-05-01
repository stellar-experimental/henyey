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
    /// Build version string (e.g. "henyey-v25.0.0-alpha.1").
    pub build: String,
    /// Git commit hash the binary was built from (40-char lowercase hex).
    /// Absent when the build system could not determine the commit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_hash: Option<String>,
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
    /// Quorum info (absent when no quorum data is available).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quorum: Option<henyey_herder::json_api::InfoQuorumSnapshot>,
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
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
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

/// Query parameters for /dumpproposedsettings endpoint.
#[derive(Deserialize)]
pub struct DumpProposedSettingsParams {
    pub blob: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_info_response(commit_hash: Option<String>) -> InfoResponse {
        InfoResponse {
            build: "henyey-v25.0.0-alpha.1".to_string(),
            commit_hash,
            protocol_version: 25,
            state: "Synced!".to_string(),
            started_on: "2024-01-01T00:00:00Z".to_string(),
            uptime_secs: 3600,
            node_name: "test-node".to_string(),
            public_key: "GAAA".to_string(),
            network_passphrase: "Test SDF Network ; September 2015".to_string(),
            is_validator: false,
            ledger: InfoLedgerSummary {
                num: 100,
                hash: "abcd".to_string(),
                close_time: 1700000000,
                version: 25,
                base_fee: 100,
                base_reserve: 5000000,
                max_tx_set_size: 1000,
                flags: 0,
                age: 5,
            },
            peers: InfoPeerSummary {
                pending_count: 0,
                authenticated_count: 10,
            },
            quorum: None,
        }
    }

    #[test]
    fn test_info_response_commit_hash_present() {
        let hash = "a".repeat(40);
        let resp = make_info_response(Some(hash.clone()));
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["commit_hash"], hash);
    }

    #[test]
    fn test_info_response_commit_hash_absent() {
        let resp = make_info_response(None);
        let json = serde_json::to_value(&resp).unwrap();
        assert!(json.get("commit_hash").is_none());
    }
}
