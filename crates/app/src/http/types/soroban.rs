//! Types for Soroban-related endpoints.
//!
//! `SorobanInfoResponse` is the single source of truth for projecting
//! [`henyey_ledger::SorobanNetworkInfo`] into JSON. Both the native handler
//! ([`crate::http::handlers::soroban::sorobaninfo_handler`]) and the compat
//! `/sorobaninfo` handler serialize it directly — the same stellar-core
//! top-level nested shape, no wrapper.
//!
//! Protocol-23 gating lives in exactly one place —
//! [`SorobanInfoResponse::from_network_info`] — and
//! `serde(skip_serializing_if = "Option::is_none")` on the optional fields
//! makes the omission a property of the type rather than a runtime
//! conditional that can drift between handlers.

use henyey_ledger::SorobanNetworkInfo;
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
///
/// Shared between the native and compat `/sorobaninfo` responses — both emit
/// the same five fields with the same names. `Clone` is derived so the compat
/// reshape can move an owned copy out of [`SorobanInfoResponse`].
#[derive(Serialize, Clone)]
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

impl SorobanInfoResponse {
    /// Project a [`SorobanNetworkInfo`] into the typed `/sorobaninfo` basic
    /// response.
    ///
    /// This is the **single read of `SorobanNetworkInfo`**. Both the native
    /// handler and the compat handler funnel through this constructor and
    /// serialize the result directly. Centralizing the projection here — and
    /// encoding the protocol-23 gating as `Some`/`None` on the optional
    /// fields — makes cross-handler drift on shared fields and the
    /// protocol-23 gate structurally impossible.
    pub(crate) fn from_network_info(info: &SorobanNetworkInfo, protocol_version: u32) -> Self {
        let p23_or_later = protocol_version >= 23;
        Self {
            max_contract_size: info.max_contract_size,
            max_contract_data_key_size: info.max_contract_data_key_size,
            max_contract_data_entry_size: info.max_contract_data_entry_size,
            tx: SorobanTxLimits {
                max_instructions: info.tx_max_instructions,
                memory_limit: info.tx_memory_limit,
                max_read_ledger_entries: info.tx_max_read_ledger_entries,
                max_read_bytes: info.tx_max_read_bytes,
                max_write_ledger_entries: info.tx_max_write_ledger_entries,
                max_write_bytes: info.tx_max_write_bytes,
                max_contract_events_size_bytes: info.tx_max_contract_events_size_bytes,
                max_size_bytes: info.tx_max_size_bytes,
                max_footprint_size: p23_or_later.then_some(info.tx_max_footprint_entries),
            },
            ledger: SorobanLedgerLimits {
                max_instructions: info.ledger_max_instructions,
                max_read_ledger_entries: info.ledger_max_read_ledger_entries,
                max_read_bytes: info.ledger_max_read_bytes,
                max_write_ledger_entries: info.ledger_max_write_ledger_entries,
                max_write_bytes: info.ledger_max_write_bytes,
                max_tx_size_bytes: info.ledger_max_tx_size_bytes,
                max_tx_count: info.ledger_max_tx_count,
            },
            fee_rate_per_instructions_increment: info.fee_rate_per_instructions_increment,
            fee_read_ledger_entry: info.fee_read_ledger_entry,
            fee_write_ledger_entry: info.fee_write_ledger_entry,
            fee_read_1kb: info.fee_read_1kb,
            fee_write_1kb: info.fee_write_1kb,
            fee_historical_1kb: info.fee_historical_1kb,
            fee_contract_events_size_1kb: info.fee_contract_events_size_1kb,
            fee_transaction_size_1kb: info.fee_transaction_size_1kb,
            state_archival: SorobanStateArchival {
                max_entry_ttl: info.max_entry_ttl,
                min_temporary_ttl: info.min_temporary_ttl,
                min_persistent_ttl: info.min_persistent_ttl,
                persistent_rent_rate_denominator: info.persistent_rent_rate_denominator,
                temp_rent_rate_denominator: info.temp_rent_rate_denominator,
                max_entries_to_archive: info.max_entries_to_archive,
                bucketlist_size_window_sample_size: info.bucketlist_size_window_sample_size,
                eviction_scan_size: info.eviction_scan_size,
                starting_eviction_scan_level: info.starting_eviction_scan_level,
                // stellar-core emits this alias key with the same value as
                // `bucketlist_size_window_sample_size`
                // (CommandHandler.cpp:1019). Keep them in sync here.
                bucket_list_size_snapshot_period: info.bucketlist_size_window_sample_size,
                average_bucket_list_size: info.average_bucket_list_size,
            },
            max_dependent_tx_clusters: p23_or_later
                .then_some(info.ledger_max_dependent_tx_clusters),
            scp: p23_or_later.then(|| SorobanScpSettings {
                ledger_close_time_ms: info.ledger_target_close_time_ms,
                nomination_timeout_ms: info.nomination_timeout_initial_ms,
                nomination_timeout_inc_ms: info.nomination_timeout_increment_ms,
                ballot_timeout_ms: info.ballot_timeout_initial_ms,
                ballot_timeout_inc_ms: info.ballot_timeout_increment_ms,
            }),
        }
    }
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

    // ── from_network_info tests ─────────────────────────────────────────────

    /// Build a `SorobanNetworkInfo` with sentinel values that make
    /// field-swap regressions easy to spot. Each field gets a distinct
    /// non-zero value drawn from the prime sequence.
    fn sentinel_network_info() -> SorobanNetworkInfo {
        SorobanNetworkInfo {
            max_contract_size: 2,
            max_contract_data_key_size: 3,
            max_contract_data_entry_size: 5,
            tx_max_instructions: 7,
            ledger_max_instructions: 11,
            fee_rate_per_instructions_increment: 13,
            tx_memory_limit: 17,
            ledger_max_read_ledger_entries: 19,
            ledger_max_read_bytes: 23,
            ledger_max_write_ledger_entries: 29,
            ledger_max_write_bytes: 31,
            tx_max_read_ledger_entries: 37,
            tx_max_read_bytes: 41,
            tx_max_write_ledger_entries: 43,
            tx_max_write_bytes: 47,
            fee_read_ledger_entry: 53,
            fee_write_ledger_entry: 59,
            fee_read_1kb: 61,
            fee_write_1kb: 67,
            fee_historical_1kb: 71,
            tx_max_contract_events_size_bytes: 73,
            fee_contract_events_size_1kb: 79,
            ledger_max_tx_size_bytes: 83,
            tx_max_size_bytes: 89,
            fee_transaction_size_1kb: 97,
            ledger_max_tx_count: 101,
            max_entry_ttl: 103,
            min_temporary_ttl: 107,
            min_persistent_ttl: 109,
            persistent_rent_rate_denominator: 113,
            temp_rent_rate_denominator: 127,
            max_entries_to_archive: 131,
            bucketlist_size_window_sample_size: 137,
            eviction_scan_size: 139,
            starting_eviction_scan_level: 149,
            average_bucket_list_size: 151,
            state_target_size_bytes: 157,
            rent_fee_1kb_state_size_low: 163,
            rent_fee_1kb_state_size_high: 167,
            state_size_rent_fee_growth_factor: 173,
            nomination_timeout_initial_ms: 179,
            nomination_timeout_increment_ms: 181,
            ballot_timeout_initial_ms: 191,
            ballot_timeout_increment_ms: 193,
            ledger_target_close_time_ms: 197,
            ledger_max_dependent_tx_clusters: 199,
            tx_max_footprint_entries: 211,
        }
    }

    /// `from_network_info` must copy every field into the right slot.
    /// Sentinel primes catch accidental field swaps (e.g., reading
    /// `tx_max_read_bytes` into `tx_max_write_bytes`).
    #[test]
    fn test_from_network_info_basic_shape() {
        let info = sentinel_network_info();
        let r = SorobanInfoResponse::from_network_info(&info, 22);

        // Top-level
        assert_eq!(r.max_contract_size, 2);
        assert_eq!(r.max_contract_data_key_size, 3);
        assert_eq!(r.max_contract_data_entry_size, 5);
        assert_eq!(r.fee_rate_per_instructions_increment, 13);
        assert_eq!(r.fee_read_ledger_entry, 53);
        assert_eq!(r.fee_write_ledger_entry, 59);
        assert_eq!(r.fee_read_1kb, 61);
        assert_eq!(r.fee_write_1kb, 67);
        assert_eq!(r.fee_historical_1kb, 71);
        assert_eq!(r.fee_contract_events_size_1kb, 79);
        assert_eq!(r.fee_transaction_size_1kb, 97);

        // tx
        assert_eq!(r.tx.max_instructions, 7);
        assert_eq!(r.tx.memory_limit, 17);
        assert_eq!(r.tx.max_read_ledger_entries, 37);
        assert_eq!(r.tx.max_read_bytes, 41);
        assert_eq!(r.tx.max_write_ledger_entries, 43);
        assert_eq!(r.tx.max_write_bytes, 47);
        assert_eq!(r.tx.max_contract_events_size_bytes, 73);
        assert_eq!(r.tx.max_size_bytes, 89);

        // ledger
        assert_eq!(r.ledger.max_instructions, 11);
        assert_eq!(r.ledger.max_read_ledger_entries, 19);
        assert_eq!(r.ledger.max_read_bytes, 23);
        assert_eq!(r.ledger.max_write_ledger_entries, 29);
        assert_eq!(r.ledger.max_write_bytes, 31);
        assert_eq!(r.ledger.max_tx_size_bytes, 83);
        assert_eq!(r.ledger.max_tx_count, 101);

        // state_archival
        assert_eq!(r.state_archival.max_entry_ttl, 103);
        assert_eq!(r.state_archival.min_temporary_ttl, 107);
        assert_eq!(r.state_archival.min_persistent_ttl, 109);
        assert_eq!(r.state_archival.persistent_rent_rate_denominator, 113);
        assert_eq!(r.state_archival.temp_rent_rate_denominator, 127);
        assert_eq!(r.state_archival.max_entries_to_archive, 131);
        assert_eq!(r.state_archival.bucketlist_size_window_sample_size, 137);
        assert_eq!(r.state_archival.eviction_scan_size, 139);
        assert_eq!(r.state_archival.starting_eviction_scan_level, 149);
        assert_eq!(r.state_archival.average_bucket_list_size, 151);

        // Alias invariant: bucket_list_size_snapshot_period must mirror
        // bucketlist_size_window_sample_size (stellar-core
        // CommandHandler.cpp:1019).
        assert_eq!(r.state_archival.bucket_list_size_snapshot_period, 137);
    }

    #[test]
    fn test_from_network_info_protocol_22_omits_p23_fields() {
        let info = sentinel_network_info();
        let r = SorobanInfoResponse::from_network_info(&info, 22);

        assert!(r.scp.is_none(), "scp should be None pre-P23");
        assert!(
            r.max_dependent_tx_clusters.is_none(),
            "max_dependent_tx_clusters should be None pre-P23"
        );
        assert!(
            r.tx.max_footprint_size.is_none(),
            "tx.max_footprint_size should be None pre-P23"
        );

        // Confirm serialization respects skip_serializing_if.
        let json = serde_json::to_value(&r).unwrap();
        assert!(json.get("scp").is_none());
        assert!(json.get("max_dependent_tx_clusters").is_none());
        assert!(json["tx"].get("max_footprint_size").is_none());
    }

    #[test]
    fn test_from_network_info_protocol_23_includes_p23_fields() {
        let info = sentinel_network_info();
        let r = SorobanInfoResponse::from_network_info(&info, 23);

        // Values must come from the right SorobanNetworkInfo fields.
        assert_eq!(r.max_dependent_tx_clusters, Some(199));
        assert_eq!(r.tx.max_footprint_size, Some(211));
        let scp = r.scp.as_ref().expect("scp must be Some at P23+");
        assert_eq!(scp.ledger_close_time_ms, 197);
        assert_eq!(scp.nomination_timeout_ms, 179);
        assert_eq!(scp.nomination_timeout_inc_ms, 181);
        assert_eq!(scp.ballot_timeout_ms, 191);
        assert_eq!(scp.ballot_timeout_inc_ms, 193);
    }
}
