//! Types for the `/generateload` endpoint.

use serde::{Deserialize, Serialize};

/// Query parameters for `/generateload`.
///
/// Matches stellar-core's `generateload` command parameters.
/// All parameters are optional with sensible defaults.
#[derive(Debug, Clone, Deserialize)]
pub struct GenerateLoadParams {
    /// Load generation mode: "pay", "soroban_upload",
    /// "soroban_invoke_setup", "soroban_invoke", "mixed_classic_soroban".
    /// The "create" mode is deprecated and returns an error.
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

    /// Path to the pre-generated transactions file (for pay_pregenerated).
    #[serde(default)]
    pub preloadedtransactionsfile: Option<String>,

    /// `create_upgrade` override for `ledgerMaxInstructions` in the
    /// `CONFIG_SETTING_CONTRACT_COMPUTE_V0` upgrade entry (supercluster sends
    /// this as `ldgrmxinstrc`). Without it the create_upgrade set is a no-op
    /// and the Soroban config upgrade is never proposed. Mirrors stellar-core
    /// `LoadGenerator`'s `ldgrmxinstrc` parameter.
    #[serde(default)]
    pub ldgrmxinstrc: Option<i64>,

    /// `create_upgrade` ledger-limit overrides for the SSC
    /// `UpgradeSorobanLedgerLimits` mission (wire names match stellar-core).
    #[serde(default)]
    pub ldgrmxrdbyt: Option<u32>,
    #[serde(default)]
    pub ldgrmxwrbyt: Option<u32>,
    #[serde(default)]
    pub ldgrmxrdntry: Option<u32>,
    #[serde(default)]
    pub ldgrmxwrntry: Option<u32>,
    #[serde(default)]
    pub ldgrmxtxcnt: Option<u32>,
    #[serde(default)]
    pub ldgrmxtxsz: Option<u32>,

    // --- Apply-load (`soroban_invoke_apply_load`) params ---
    /// Whether the node is running in overlay-only mode (gate stand-in for
    /// stellar-core's `getRunInOverlayOnlyMode`).
    #[serde(default)]
    pub overlayonlymode: bool,

    /// `APPLY_LOAD_BL_BATCH_SIZE` (default 1000, matching Config.h).
    #[serde(default = "default_apply_load_bl_batch_size")]
    pub applyloadblbatchsize: u32,

    /// `APPLY_LOAD_BL_SIMULATED_LEDGERS` (default 1000, matching Config.h).
    #[serde(default = "default_apply_load_bl_simulated_ledgers")]
    pub applyloadblsimulatedledgers: u32,

    /// `APPLY_LOAD_DATA_ENTRY_SIZE` (default 0; rounded to a multiple of 4).
    #[serde(default)]
    pub applyloaddataentrysize: u32,

    /// `APPLY_LOAD_NUM_RW_ENTRIES` values (comma-separated `u32`s).
    #[serde(default)]
    pub applyloadnumrwentries: Option<String>,
    /// `APPLY_LOAD_NUM_RW_ENTRIES_DISTRIBUTION` weights (comma-separated).
    #[serde(default)]
    pub applyloadnumrwentriesdistribution: Option<String>,

    /// `APPLY_LOAD_NUM_DISK_READ_ENTRIES` values.
    #[serde(default)]
    pub applyloadnumdiskreadentries: Option<String>,
    /// `APPLY_LOAD_NUM_DISK_READ_ENTRIES_DISTRIBUTION` weights.
    #[serde(default)]
    pub applyloadnumdiskreadentriesdistribution: Option<String>,

    /// `APPLY_LOAD_TX_SIZE_BYTES` values.
    #[serde(default)]
    pub applyloadtxsizebytes: Option<String>,
    /// `APPLY_LOAD_TX_SIZE_BYTES_DISTRIBUTION` weights.
    #[serde(default)]
    pub applyloadtxsizebytesdistribution: Option<String>,

    /// `APPLY_LOAD_EVENT_COUNT` values.
    #[serde(default)]
    pub applyloadeventcount: Option<String>,
    /// `APPLY_LOAD_EVENT_COUNT_DISTRIBUTION` weights.
    #[serde(default)]
    pub applyloadeventcountdistribution: Option<String>,

    /// `APPLY_LOAD_INSTRUCTIONS` values.
    #[serde(default)]
    pub applyloadinstructions: Option<String>,
    /// `APPLY_LOAD_INSTRUCTIONS_DISTRIBUTION` weights.
    #[serde(default)]
    pub applyloadinstructionsdistribution: Option<String>,
}

fn default_mode() -> String {
    "pay".to_string()
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

fn default_apply_load_bl_batch_size() -> u32 {
    1000
}

fn default_apply_load_bl_simulated_ledgers() -> u32 {
    1000
}

/// Response for the `/generateload` endpoint.
#[derive(Serialize)]
pub struct GenerateLoadResponse {
    /// Status message.
    pub status: String,
    /// Additional info (e.g., error details).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub info: Option<String>,
    /// Base64-encoded XDR-opaque `ConfigUpgradeSetKey`, emitted ONLY for a
    /// successful `create_upgrade` run so supercluster can arm
    /// `/upgrades?configupgradesetkey=…`. Mirrors stellar-core
    /// `CommandHandler::generateLoad` (CommandHandler.cpp:1488-1496), which sets
    /// `res["config_upgrade_set_key"] = decoder::encode_b64(xdr_to_opaque(key))`
    /// only for `SOROBAN_CREATE_UPGRADE`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_upgrade_set_key: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_defaults_when_empty_json() {
        // Deserialize from an empty JSON object to test serde defaults.
        let params: GenerateLoadParams = serde_json::from_str("{}").unwrap();
        assert_eq!(params.mode, "pay");
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
            config_upgrade_set_key: None,
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
            config_upgrade_set_key: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["status"], "ok");
        assert!(json.get("info").is_none());
        assert!(json.get("config_upgrade_set_key").is_none());
    }

    /// #3588: only the `create_upgrade` response carries a top-level
    /// `config_upgrade_set_key`; when present it must serialize under that
    /// exact snake_case field name (parity: stellar-core
    /// `CommandHandler::generateLoad`, CommandHandler.cpp:1488-1496).
    #[test]
    fn test_response_serialization_with_config_upgrade_set_key() {
        let resp = GenerateLoadResponse {
            status: "ok".to_string(),
            info: Some("Started create_upgrade".to_string()),
            config_upgrade_set_key: Some("AAAA-base64-key".to_string()),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["status"], "ok");
        assert_eq!(json["config_upgrade_set_key"], "AAAA-base64-key");
    }
}
