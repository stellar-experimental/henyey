//! Types for the `/generateload` endpoint.

use serde::{Deserialize, Serialize};

/// Query parameters for `/generateload`.
///
/// Matches stellar-core's `generateload` command parameters.
/// All parameters are optional with sensible defaults.
#[derive(Debug, Deserialize)]
pub struct GenerateLoadParams {
    /// Load generation mode: "create", "pay", "sorobanupload",
    /// "sorobaninvokesetup", "sorobaninvoke", "mixed".
    #[serde(default = "default_mode")]
    pub mode: String,

    /// Number of accounts in the pool.
    #[serde(default = "default_accounts")]
    pub accounts: u32,

    /// Number of transactions to submit.
    #[serde(default = "default_txs")]
    pub txs: u32,

    /// Target transaction rate (transactions per second).
    #[serde(default = "default_txrate")]
    pub txrate: u32,

    /// Account ID offset.
    #[serde(default)]
    pub offset: u32,

    /// Spike interval in seconds (0 = no spikes).
    #[serde(default)]
    pub spikeinterval: u64,

    /// Number of extra transactions per spike burst.
    #[serde(default)]
    pub spikesize: u32,

    /// Maximum fee rate (0 = use base fee).
    #[serde(default)]
    pub maxfeerate: u32,

    /// Whether to skip transactions rejected for low fee.
    #[serde(default)]
    pub skiplowfeetxs: bool,

    /// Minimum Soroban success percentage (0–100).
    #[serde(default)]
    pub minpercentsuccess: u32,

    /// Number of contract instances (for sorobaninvokesetup).
    #[serde(default = "default_instances")]
    pub instances: u32,

    /// Number of Wasm blobs to upload (for sorobaninvokesetup).
    #[serde(default)]
    pub wasms: u32,
}

fn default_mode() -> String {
    "create".to_string()
}

fn default_accounts() -> u32 {
    100
}

fn default_txs() -> u32 {
    100
}

fn default_txrate() -> u32 {
    10
}

fn default_instances() -> u32 {
    0
}

/// Response for the `/generateload` endpoint.
#[derive(Serialize)]
pub struct GenerateLoadResponse {
    /// Status message.
    pub status: String,
    /// Additional info (e.g., error details).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub info: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_defaults_when_empty_json() {
        // Deserialize from an empty JSON object to test serde defaults.
        let params: GenerateLoadParams = serde_json::from_str("{}").unwrap();
        assert_eq!(params.mode, "create");
        assert_eq!(params.accounts, 100);
        assert_eq!(params.txs, 100);
        assert_eq!(params.txrate, 10);
        assert_eq!(params.offset, 0);
        assert_eq!(params.spikeinterval, 0);
        assert_eq!(params.spikesize, 0);
        assert_eq!(params.maxfeerate, 0);
        assert!(!params.skiplowfeetxs);
        assert_eq!(params.minpercentsuccess, 0);
        assert_eq!(params.instances, 0);
        assert_eq!(params.wasms, 0);
    }

    #[test]
    fn test_custom_params() {
        let json = r#"{
            "mode": "pay",
            "accounts": 200,
            "txs": 50,
            "txrate": 20,
            "offset": 5,
            "spikeinterval": 30,
            "spikesize": 10,
            "maxfeerate": 500,
            "skiplowfeetxs": true,
            "minpercentsuccess": 90,
            "instances": 3,
            "wasms": 2
        }"#;
        let params: GenerateLoadParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.mode, "pay");
        assert_eq!(params.accounts, 200);
        assert_eq!(params.txs, 50);
        assert_eq!(params.txrate, 20);
        assert_eq!(params.offset, 5);
        assert_eq!(params.spikeinterval, 30);
        assert_eq!(params.spikesize, 10);
        assert_eq!(params.maxfeerate, 500);
        assert!(params.skiplowfeetxs);
        assert_eq!(params.minpercentsuccess, 90);
        assert_eq!(params.instances, 3);
        assert_eq!(params.wasms, 2);
    }

    #[test]
    fn test_partial_params_use_defaults() {
        let json = r#"{"mode": "sorobaninvoke", "txrate": 50}"#;
        let params: GenerateLoadParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.mode, "sorobaninvoke");
        assert_eq!(params.accounts, 100); // default
        assert_eq!(params.txs, 100); // default
        assert_eq!(params.txrate, 50);
    }

    #[test]
    fn test_response_serialization_with_info() {
        let resp = GenerateLoadResponse {
            status: "ok".to_string(),
            info: Some("Started load".to_string()),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["status"], "ok");
        assert_eq!(json["info"], "Started load");
    }

    #[test]
    fn test_response_serialization_without_info() {
        let resp = GenerateLoadResponse {
            status: "ok".to_string(),
            info: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["status"], "ok");
        assert!(json.get("info").is_none());
    }
}
