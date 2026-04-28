//! Types for Soroban-related endpoints.

use serde::{Deserialize, Serialize};

/// Query parameters for /sorobaninfo endpoint.
#[derive(Deserialize)]
pub struct SorobanInfoParams {
    pub format: Option<String>,
}

/// Response for the /sorobaninfo endpoint (basic format).
#[derive(Serialize)]
pub struct SorobanInfoResponse {
    pub max_contract_size: u32,
    pub max_contract_data_key_size: u32,
    pub max_contract_data_entry_size: u32,
    pub tx: SorobanTxLimits,
    pub ledger: SorobanLedgerLimits,
    pub fee_rate_per_instructions_increment: i64,
    pub fee_read_ledger_entry: i64,
    pub fee_write_ledger_entry: i64,
    pub fee_read_1kb: i64,
    pub fee_write_1kb: i64,
    pub fee_historical_1kb: i64,
    pub fee_contract_events_size_1kb: i64,
    pub fee_transaction_size_1kb: i64,
    pub state_archival: SorobanStateArchival,
    /// Protocol 23+: maximum dependent TX clusters per parallel stage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_dependent_tx_clusters: Option<u32>,
    /// Protocol 23+: SCP timing settings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scp: Option<SorobanScpSettings>,
}

/// SCP timing settings (protocol 23+), matching stellar-core's JSON field names.
#[derive(Serialize)]
pub struct SorobanScpSettings {
    pub ledger_close_time_ms: u32,
    pub nomination_timeout_ms: u32,
    pub nomination_timeout_inc_ms: u32,
    pub ballot_timeout_ms: u32,
    pub ballot_timeout_inc_ms: u32,
}

/// Soroban per-transaction resource limits.
#[derive(Serialize)]
pub struct SorobanTxLimits {
    pub max_instructions: i64,
    pub memory_limit: u32,
    pub max_read_ledger_entries: u32,
    pub max_read_bytes: u32,
    pub max_write_ledger_entries: u32,
    pub max_write_bytes: u32,
    pub max_contract_events_size_bytes: u32,
    pub max_size_bytes: u32,
    /// Protocol 23+: maximum footprint entries per transaction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_footprint_size: Option<u32>,
}

/// Soroban per-ledger resource limits.
#[derive(Serialize)]
pub struct SorobanLedgerLimits {
    pub max_instructions: i64,
    pub max_read_ledger_entries: u32,
    pub max_read_bytes: u32,
    pub max_write_ledger_entries: u32,
    pub max_write_bytes: u32,
    pub max_tx_size_bytes: u32,
    pub max_tx_count: u32,
}

/// Soroban state archival settings.
#[derive(Serialize)]
pub struct SorobanStateArchival {
    pub max_entry_ttl: u32,
    pub min_temporary_ttl: u32,
    pub min_persistent_ttl: u32,
    pub persistent_rent_rate_denominator: i64,
    pub temp_rent_rate_denominator: i64,
    pub max_entries_to_archive: u32,
    pub bucketlist_size_window_sample_size: u32,
    pub eviction_scan_size: i64,
    pub starting_eviction_scan_level: u32,
    /// Alias for `bucketlist_size_window_sample_size`.
    /// stellar-core emits both keys with the same value
    /// (from `stateArchivalSettings.liveSorobanStateSizeWindowSampleSize`).
    pub bucket_list_size_snapshot_period: u32,
    /// Computed average bucket list size (non-configurable).
    pub average_bucket_list_size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_response(
        scp: Option<SorobanScpSettings>,
        clusters: Option<u32>,
        max_footprint_size: Option<u32>,
    ) -> SorobanInfoResponse {
        SorobanInfoResponse {
            max_contract_size: 0,
            max_contract_data_key_size: 0,
            max_contract_data_entry_size: 0,
            tx: SorobanTxLimits {
                max_instructions: 0,
                memory_limit: 0,
                max_read_ledger_entries: 0,
                max_read_bytes: 0,
                max_write_ledger_entries: 0,
                max_write_bytes: 0,
                max_contract_events_size_bytes: 0,
                max_size_bytes: 0,
                max_footprint_size,
            },
            ledger: SorobanLedgerLimits {
                max_instructions: 0,
                max_read_ledger_entries: 0,
                max_read_bytes: 0,
                max_write_ledger_entries: 0,
                max_write_bytes: 0,
                max_tx_size_bytes: 0,
                max_tx_count: 0,
            },
            fee_rate_per_instructions_increment: 0,
            fee_read_ledger_entry: 0,
            fee_write_ledger_entry: 0,
            fee_read_1kb: 0,
            fee_write_1kb: 0,
            fee_historical_1kb: 0,
            fee_contract_events_size_1kb: 0,
            fee_transaction_size_1kb: 0,
            state_archival: SorobanStateArchival {
                max_entry_ttl: 0,
                min_temporary_ttl: 0,
                min_persistent_ttl: 0,
                persistent_rent_rate_denominator: 0,
                temp_rent_rate_denominator: 0,
                max_entries_to_archive: 0,
                bucketlist_size_window_sample_size: 30,
                eviction_scan_size: 0,
                starting_eviction_scan_level: 0,
                bucket_list_size_snapshot_period: 30,
                average_bucket_list_size: 100_000_000,
            },
            max_dependent_tx_clusters: clusters,
            scp,
        }
    }

    #[test]
    fn test_sorobaninfo_pre_protocol_23_omits_scp_fields() {
        let response = default_response(None, None, None);
        let json = serde_json::to_value(&response).unwrap();

        assert!(
            json.get("scp").is_none(),
            "scp should be absent for pre-protocol 23"
        );
        assert!(
            json.get("max_dependent_tx_clusters").is_none(),
            "max_dependent_tx_clusters should be absent for pre-protocol 23"
        );
        assert!(
            json["tx"].get("max_footprint_size").is_none(),
            "max_footprint_size should be absent for pre-protocol 23"
        );
    }

    #[test]
    fn test_sorobaninfo_protocol_23_includes_scp_fields() {
        let response = default_response(
            Some(SorobanScpSettings {
                ledger_close_time_ms: 5000,
                nomination_timeout_ms: 1000,
                nomination_timeout_inc_ms: 500,
                ballot_timeout_ms: 1000,
                ballot_timeout_inc_ms: 1000,
            }),
            Some(8),
            Some(40),
        );
        let json = serde_json::to_value(&response).unwrap();

        assert_eq!(json["max_dependent_tx_clusters"], 8);

        let scp = &json["scp"];
        assert_eq!(scp["ledger_close_time_ms"], 5000);
        assert_eq!(scp["nomination_timeout_ms"], 1000);
        assert_eq!(scp["nomination_timeout_inc_ms"], 500);
        assert_eq!(scp["ballot_timeout_ms"], 1000);
        assert_eq!(scp["ballot_timeout_inc_ms"], 1000);

        // Protocol 23+ includes max_footprint_size in tx
        assert_eq!(json["tx"]["max_footprint_size"], 40);
    }

    #[test]
    fn test_sorobaninfo_scp_field_names_match_stellar_core() {
        let response = default_response(
            Some(SorobanScpSettings {
                ledger_close_time_ms: 1,
                nomination_timeout_ms: 2,
                nomination_timeout_inc_ms: 3,
                ballot_timeout_ms: 4,
                ballot_timeout_inc_ms: 5,
            }),
            Some(6),
            Some(16),
        );
        let json = serde_json::to_value(&response).unwrap();
        let scp = json["scp"].as_object().unwrap();

        // Verify exact field names match stellar-core's CommandHandler.cpp:1034-1044.
        let expected_keys = [
            "ledger_close_time_ms",
            "nomination_timeout_ms",
            "nomination_timeout_inc_ms",
            "ballot_timeout_ms",
            "ballot_timeout_inc_ms",
        ];
        for key in &expected_keys {
            assert!(scp.contains_key(*key), "missing expected SCP field: {key}");
        }
        assert_eq!(
            scp.len(),
            expected_keys.len(),
            "unexpected extra SCP fields"
        );
    }

    #[test]
    fn test_sorobaninfo_state_archival_includes_bucket_list_fields() {
        let response = default_response(None, None, None);
        let json = serde_json::to_value(&response).unwrap();
        let archival = &json["state_archival"];

        assert_eq!(
            archival["average_bucket_list_size"], 100_000_000_u64,
            "average_bucket_list_size should always be present"
        );
        assert_eq!(
            archival["bucket_list_size_snapshot_period"], 30,
            "bucket_list_size_snapshot_period should always be present"
        );
        // Verify alias invariant: bucket_list_size_snapshot_period == bucketlist_size_window_sample_size
        assert_eq!(
            archival["bucket_list_size_snapshot_period"],
            archival["bucketlist_size_window_sample_size"],
            "bucket_list_size_snapshot_period must equal bucketlist_size_window_sample_size (alias)"
        );
    }

    #[test]
    fn test_sorobaninfo_max_footprint_size_protocol_gating() {
        // Pre-protocol 23: max_footprint_size absent
        let pre_p23 = default_response(None, None, None);
        let json = serde_json::to_value(&pre_p23).unwrap();
        assert!(
            json["tx"].get("max_footprint_size").is_none(),
            "max_footprint_size should be absent pre-P23"
        );

        // Protocol 23+: max_footprint_size present
        let p23 = default_response(None, None, Some(40));
        let json = serde_json::to_value(&p23).unwrap();
        assert_eq!(
            json["tx"]["max_footprint_size"], 40,
            "max_footprint_size should be 40 for P23+"
        );
    }
}
