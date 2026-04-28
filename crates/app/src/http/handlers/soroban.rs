//! Handler for /sorobaninfo endpoint.

use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
};

use super::super::types::{
    SorobanInfoParams, SorobanInfoResponse, SorobanLedgerLimits, SorobanScpSettings,
    SorobanStateArchival, SorobanTxLimits,
};
use super::super::ServerState;

pub(crate) async fn sorobaninfo_handler(
    State(state): State<Arc<ServerState>>,
    Query(params): Query<SorobanInfoParams>,
) -> impl IntoResponse {
    let format = params.format.as_deref().unwrap_or("basic");

    let protocol_version = state.app.ledger_info().protocol_version;

    match format {
        "basic" => {
            let Some(info) = state.app.soroban_network_info() else {
                return (
                    StatusCode::OK,
                    Json(serde_json::json!({
                        "error": "Soroban config not available (ledger not initialized or pre-protocol 20)",
                        "protocol_version": protocol_version
                    })),
                );
            };

            let response = SorobanInfoResponse {
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
                    max_footprint_size: if protocol_version >= 23 {
                        Some(info.tx_max_footprint_entries)
                    } else {
                        None
                    },
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
                    bucket_list_size_snapshot_period: info.bucketlist_size_window_sample_size,
                    average_bucket_list_size: info.average_bucket_list_size,
                },
                max_dependent_tx_clusters: if protocol_version >= 23 {
                    Some(info.ledger_max_dependent_tx_clusters)
                } else {
                    None
                },
                scp: if protocol_version >= 23 {
                    Some(SorobanScpSettings {
                        ledger_close_time_ms: info.ledger_target_close_time_ms,
                        nomination_timeout_ms: info.nomination_timeout_initial_ms,
                        nomination_timeout_inc_ms: info.nomination_timeout_increment_ms,
                        ballot_timeout_ms: info.ballot_timeout_initial_ms,
                        ballot_timeout_inc_ms: info.ballot_timeout_increment_ms,
                    })
                } else {
                    None
                },
            };
            (
                StatusCode::OK,
                Json(serde_json::to_value(response).unwrap()),
            )
        }
        "detailed" | "upgrade_xdr" => (
            StatusCode::OK,
            Json(serde_json::json!({
                "error": format!("Format '{}' not yet implemented", format),
                "available_formats": ["basic"]
            })),
        ),
        _ => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("Unknown format: {}", format),
                "available_formats": ["basic", "detailed", "upgrade_xdr"]
            })),
        ),
    }
}
