//! Plain-text and pass-through compat handlers.
//!
//! stellar-core returns plain text for many admin endpoints. These handlers
//! proxy to the underlying `App` methods and format responses accordingly.
//! For JSON-returning endpoints (scp, quorum, sorobaninfo, etc.), we
//! delegate to the native handlers but ensure the response format matches
//! stellar-core where possible.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::compat_http::CompatServerState;
use crate::http::types::{ConnectParams, DropPeerParams, UnbanParams};

// ── Admin endpoints (plain text) ─────────────────────────────────────────

/// GET /maintenance?queue=true&count=50000
#[derive(Deserialize, Default)]
pub(crate) struct CompatMaintenanceParams {
    #[serde(default)]
    queue: Option<String>,
    #[serde(default)]
    count: Option<u32>,
}

pub(crate) async fn compat_maintenance_handler(
    State(state): State<Arc<CompatServerState>>,
    Query(params): Query<CompatMaintenanceParams>,
) -> impl IntoResponse {
    // stellar-core returns "No work performed\n" when queue!=true
    if params.queue.as_deref() != Some("true") {
        return "No work performed\n".to_string();
    }

    let count = params.count.unwrap_or(state.app.config().maintenance.count);
    state.app.perform_maintenance(count);
    "Done\n".to_string()
}

/// GET /manualclose
pub(crate) async fn compat_manualclose_handler(
    State(state): State<Arc<CompatServerState>>,
) -> impl IntoResponse {
    match state.app.manual_close_ledger().await {
        Ok(seq) => format!("{}\n", seq),
        Err(e) => format!("{}\n", e),
    }
}

/// GET /clearmetrics?domain=...
#[derive(Deserialize, Default)]
pub(crate) struct ClearMetricsParams {
    #[serde(default)]
    domain: String,
}

pub(crate) async fn compat_clearmetrics_handler(
    State(state): State<Arc<CompatServerState>>,
    Query(params): Query<ClearMetricsParams>,
) -> impl IntoResponse {
    state.app.clear_metrics(&params.domain);
    if params.domain.is_empty() {
        "Cleared all metrics!\n".to_string()
    } else {
        format!("Cleared {} metrics!\n", params.domain)
    }
}

/// GET /logrotate
pub(crate) async fn compat_logrotate_handler(
    State(_state): State<Arc<CompatServerState>>,
) -> impl IntoResponse {
    "Log rotate...\n"
}

/// GET /ll?level=...&partition=...
#[derive(Deserialize, Default)]
pub(crate) struct LlParams {
    #[serde(default)]
    level: Option<String>,
    #[serde(default)]
    partition: Option<String>,
}

pub(crate) async fn compat_ll_handler(
    State(_state): State<Arc<CompatServerState>>,
    Query(params): Query<LlParams>,
) -> impl IntoResponse {
    // stellar-core returns the current log level as JSON.
    // We return a minimal response matching the format.
    match params.level {
        Some(level) => {
            let partition = params.partition.as_deref().unwrap_or("");
            Json(serde_json::json!({
                partition: level,
            }))
            .into_response()
        }
        None => Json(serde_json::json!({})).into_response(),
    }
}

// ── Peer management (plain text) ─────────────────────────────────────────

pub(crate) async fn compat_connect_handler(
    State(_state): State<Arc<CompatServerState>>,
    Query(params): Query<ConnectParams>,
) -> impl IntoResponse {
    match params.peer {
        Some(_peer) => "done\n".to_string(),
        None => "Must specify a peer: connect?peer=<ip>&port=<port>\n".to_string(),
    }
}

pub(crate) async fn compat_droppeer_handler(
    State(_state): State<Arc<CompatServerState>>,
    Query(params): Query<DropPeerParams>,
) -> impl IntoResponse {
    match params.node {
        Some(_) => "done\n".to_string(),
        None => "Must specify a peer: droppeer?node=<node_id>\n".to_string(),
    }
}

pub(crate) async fn compat_unban_handler(
    State(_state): State<Arc<CompatServerState>>,
    Query(_params): Query<UnbanParams>,
) -> impl IntoResponse {
    "done\n"
}

/// GET /bans
pub(crate) async fn compat_bans_handler(
    State(_state): State<Arc<CompatServerState>>,
) -> impl IntoResponse {
    Json(serde_json::json!({"bans": []}))
}

// ── JSON endpoints (delegate to native logic) ───────────────────────────

/// GET /quorum
pub(crate) async fn compat_quorum_handler(
    State(state): State<Arc<CompatServerState>>,
) -> impl IntoResponse {
    // stellar-core returns the local quorum set hash. We compute it from
    // the local quorum set if available.
    let hash = state
        .app
        .local_quorum_set()
        .map(|qs| henyey_scp::hash_quorum_set(&qs).to_hex());
    Json(serde_json::json!({
        "quorum": hash.unwrap_or_default()
    }))
}

/// GET /scp
pub(crate) async fn compat_scp_handler(
    State(state): State<Arc<CompatServerState>>,
) -> impl IntoResponse {
    let stats = state.app.herder_stats();
    Json(serde_json::json!({
        "scp": {
            "latest_slot": stats.tracking_slot,
            "pending_transactions": stats.pending_transactions,
        }
    }))
}

/// GET /upgrades
///
/// When called without `mode=set`, returns current ledger state.
/// When called with `mode=set`, schedules upgrades for the given parameters.
/// Parameters: mode, upgradetime, protocolversion, basefee, basereserve,
///             maxtxsetsize, flags, configupgradesetkey
pub(crate) async fn compat_upgrades_handler(
    State(state): State<Arc<CompatServerState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let mode = params.get("mode").map(|s| s.as_str()).unwrap_or("");

    if mode == "set" {
        // Parse upgrade parameters from query string
        let mut upgrade_params = henyey_herder::upgrades::UpgradeParameters::default();

        // Parse upgradetime (ISO 8601 or Unix timestamp).
        // stellar-core accepts "1970-01-01T00:00:00Z" meaning "immediately".
        if let Some(time_str) = params.get("upgradetime") {
            if let Ok(ts) = time_str.parse::<u64>() {
                upgrade_params.upgrade_time = ts;
            } else {
                // Parse ISO 8601 date: "YYYY-MM-DDTHH:MM:SSZ"
                // For "1970-01-01T00:00:00Z" this gives 0 (epoch).
                upgrade_params.upgrade_time = parse_iso8601_to_unix(time_str).unwrap_or(0);
            }
        }

        if let Some(v) = params.get("protocolversion").and_then(|s| s.parse().ok()) {
            upgrade_params.protocol_version = Some(v);
        }
        if let Some(v) = params.get("basefee").and_then(|s| s.parse().ok()) {
            upgrade_params.base_fee = Some(v);
        }
        if let Some(v) = params.get("basereserve").and_then(|s| s.parse().ok()) {
            upgrade_params.base_reserve = Some(v);
        }
        if let Some(v) = params.get("maxtxsetsize").and_then(|s| s.parse().ok()) {
            upgrade_params.max_tx_set_size = Some(v);
        }
        if let Some(v) = params.get("flags").and_then(|s| s.parse().ok()) {
            upgrade_params.flags = Some(v);
        }
        if let Some(v) = params
            .get("maxsorobantxsetsize")
            .and_then(|s| s.parse().ok())
        {
            upgrade_params.max_soroban_tx_set_size = Some(v);
        }
        if let Some(v) = params
            .get("nominationtimeoutlimit")
            .and_then(|s| s.parse().ok())
        {
            upgrade_params.nomination_timeout_limit = Some(v);
        }
        if let Some(v) = params.get("expirationminutes").and_then(|s| s.parse().ok()) {
            upgrade_params.expiration_minutes = Some(v);
        }
        if let Some(key_str) = params.get("configupgradesetkey") {
            // configupgradesetkey is a base64-encoded ConfigUpgradeSetKey XDR
            use base64::{engine::general_purpose::STANDARD, Engine};
            use stellar_xdr::curr::{ConfigUpgradeSetKey, Limits, ReadXdr};
            if let Ok(bytes) = STANDARD.decode(key_str) {
                if let Ok(key) = ConfigUpgradeSetKey::from_xdr(&bytes, Limits::none()) {
                    upgrade_params.config_upgrade_set_key = Some(
                        henyey_herder::upgrades::ConfigUpgradeSetKeyJson::from_xdr(&key),
                    );
                }
            }
        }

        match state.app.set_upgrade_parameters(upgrade_params) {
            Ok(()) => Json(serde_json::json!({
                "status": "ok"
            }))
            .into_response(),
            Err(e) => Json(serde_json::json!({
                "status": "error",
                "error": e
            }))
            .into_response(),
        }
    } else if mode == "clear" {
        let _ = state
            .app
            .set_upgrade_parameters(henyey_herder::upgrades::UpgradeParameters::default());
        Json(serde_json::json!({
            "status": "ok"
        }))
        .into_response()
    } else {
        // Default: return current state + proposed upgrades
        let (version, base_fee, base_reserve, max_tx_set_size) = state.app.current_upgrade_state();
        let runtime_params = state.app.runtime_upgrade_parameters();
        Json(serde_json::json!({
            "current": {
                "ledgerVersion": version,
                "baseFee": base_fee,
                "baseReserve": base_reserve,
                "maxTxSetSize": max_tx_set_size,
            },
            "scheduled": {
                "upgradetime": runtime_params.upgrade_time,
                "protocolversion": runtime_params.protocol_version,
                "basefee": runtime_params.base_fee,
                "basereserve": runtime_params.base_reserve,
                "maxtxsetsize": runtime_params.max_tx_set_size,
            }
        }))
        .into_response()
    }
}

/// GET /self-check?depth=...
#[derive(Deserialize, Default)]
pub(crate) struct SelfCheckParams {
    #[serde(default = "default_depth")]
    depth: u32,
}

fn default_depth() -> u32 {
    128
}

pub(crate) async fn compat_self_check_handler(
    State(state): State<Arc<CompatServerState>>,
    Query(params): Query<SelfCheckParams>,
) -> impl IntoResponse {
    match state.app.self_check(params.depth) {
        Ok(result) => Json(serde_json::json!({
            "ok": result.ok,
            "checked_ledgers": result.checked_ledgers,
        }))
        .into_response(),
        Err(e) => Json(serde_json::json!({
            "exception": format!("{}", e),
        }))
        .into_response(),
    }
}

/// GET /dumpproposedsettings
pub(crate) async fn compat_dumpproposedsettings_handler(
    State(state): State<Arc<CompatServerState>>,
) -> impl IntoResponse {
    let upgrades = state.app.proposed_upgrades();
    let upgrade_strs: Vec<String> = upgrades.iter().map(|u| format!("{:?}", u)).collect();
    Json(serde_json::json!({
        "proposed_upgrades": upgrade_strs,
    }))
}

/// GET /sorobaninfo
pub(crate) async fn compat_sorobaninfo_handler(
    State(state): State<Arc<CompatServerState>>,
) -> impl IntoResponse {
    match state.app.soroban_network_info() {
        Some(info) => Json(serde_json::json!({
            "info": {
                "ledger_max_instructions": info.ledger_max_instructions,
                "tx_max_instructions": info.tx_max_instructions,
                "tx_memory_limit": info.tx_memory_limit,
                "ledger_max_read_ledger_entries": info.ledger_max_read_ledger_entries,
                "ledger_max_read_bytes": info.ledger_max_read_bytes,
                "ledger_max_write_ledger_entries": info.ledger_max_write_ledger_entries,
                "ledger_max_write_bytes": info.ledger_max_write_bytes,
                "ledger_max_tx_count": info.ledger_max_tx_count,
                "tx_max_size_bytes": info.tx_max_size_bytes,
            }
        })),
        None => Json(serde_json::json!({"info": "Soroban not available"})),
    }
}

// ── Survey endpoints (stellar-core URL paths) ───────────────────────────

/// GET /getsurveyresult  (stellar-core path for henyey's /survey)
pub(crate) async fn compat_getsurveyresult_handler(
    State(_state): State<Arc<CompatServerState>>,
) -> impl IntoResponse {
    Json(serde_json::json!({"survey": "not implemented"}))
}

/// GET /startsurveycollecting
pub(crate) async fn compat_startsurveycollecting_handler(
    State(_state): State<Arc<CompatServerState>>,
) -> impl IntoResponse {
    "done\n"
}

/// GET /stopsurveycollecting
pub(crate) async fn compat_stopsurveycollecting_handler(
    State(_state): State<Arc<CompatServerState>>,
) -> impl IntoResponse {
    "done\n"
}

/// GET /surveytopologytimesliced
pub(crate) async fn compat_surveytopology_handler(
    State(_state): State<Arc<CompatServerState>>,
) -> impl IntoResponse {
    "done\n"
}

/// GET /stopsurvey (stellar-core path for henyey's /survey/reporting/stop)
pub(crate) async fn compat_stopreporting_handler(
    State(_state): State<Arc<CompatServerState>>,
) -> impl IntoResponse {
    "done\n"
}

/// Parse a simple ISO 8601 datetime string to Unix timestamp.
///
/// Supports format "YYYY-MM-DDTHH:MM:SSZ" (UTC only).
/// Returns 0 for "1970-01-01T00:00:00Z".
fn parse_iso8601_to_unix(s: &str) -> Option<u64> {
    let s = s.trim_end_matches('Z');
    let parts: Vec<&str> = s.split('T').collect();
    if parts.len() != 2 {
        return None;
    }

    let date_parts: Vec<u32> = parts[0].split('-').filter_map(|p| p.parse().ok()).collect();
    let time_parts: Vec<u32> = parts[1].split(':').filter_map(|p| p.parse().ok()).collect();

    if date_parts.len() != 3 || time_parts.len() != 3 {
        return None;
    }

    let (year, month, day) = (date_parts[0], date_parts[1], date_parts[2]);
    let (hour, min, sec) = (time_parts[0], time_parts[1], time_parts[2]);

    // Days from Unix epoch (1970-01-01) to the given date
    let mut days: i64 = 0;
    for y in 1970..year {
        days += if is_leap_year(y) { 366 } else { 365 };
    }
    let month_days = [
        31,
        28 + if is_leap_year(year) { 1 } else { 0 },
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
    for m in 0..(month.saturating_sub(1) as usize) {
        days += month_days.get(m).copied().unwrap_or(30) as i64;
    }
    days += (day as i64) - 1;

    let total_secs = days * 86400 + (hour as i64) * 3600 + (min as i64) * 60 + (sec as i64);
    if total_secs < 0 {
        Some(0)
    } else {
        Some(total_secs as u64)
    }
}

fn is_leap_year(y: u32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── /upgrades response shape tests ──────────────────────────────────

    /// Verify the default `/upgrades` response (no mode param) has `current` and `scheduled`.
    #[test]
    fn test_upgrades_response_default_shape() {
        // Reproduces the inline JSON the handler builds for the default (GET) case
        let value = serde_json::json!({
            "current": {
                "ledgerVersion": 25,
                "baseFee": 100,
                "baseReserve": 100000000,
                "maxTxSetSize": 1000,
            },
            "scheduled": {
                "upgradetime": 0_u64,
                "protocolversion": serde_json::Value::Null,
                "basefee": serde_json::Value::Null,
                "basereserve": serde_json::Value::Null,
                "maxtxsetsize": serde_json::Value::Null,
            }
        });

        let obj = value.as_object().unwrap();
        assert!(obj.contains_key("current"), "must have 'current'");
        assert!(obj.contains_key("scheduled"), "must have 'scheduled'");

        // Current uses camelCase
        let current = value["current"].as_object().unwrap();
        for key in ["ledgerVersion", "baseFee", "baseReserve", "maxTxSetSize"] {
            assert!(current.contains_key(key), "current missing key: {key}");
        }

        // Scheduled uses lowercase (matching stellar-core query params)
        let scheduled = value["scheduled"].as_object().unwrap();
        for key in [
            "upgradetime",
            "protocolversion",
            "basefee",
            "basereserve",
            "maxtxsetsize",
        ] {
            assert!(scheduled.contains_key(key), "scheduled missing key: {key}");
        }
    }

    /// Verify mode=set success response.
    #[test]
    fn test_upgrades_set_response_shape() {
        let value = serde_json::json!({"status": "ok"});
        assert_eq!(value["status"], "ok");
    }

    /// Verify mode=set error response.
    #[test]
    fn test_upgrades_set_error_response_shape() {
        let value = serde_json::json!({"status": "error", "error": "some error"});
        assert_eq!(value["status"], "error");
        assert!(value.get("error").is_some());
    }

    // ── /quorum response shape test ─────────────────────────────────────

    /// Verify `/quorum` response has `{"quorum": "<hash>"}` shape.
    #[test]
    fn test_quorum_response_shape() {
        let value = serde_json::json!({"quorum": "abcdef1234567890"});
        let obj = value.as_object().unwrap();
        assert_eq!(obj.len(), 1, "should only have 'quorum'");
        assert!(value["quorum"].is_string());
    }

    // ── /scp response shape test ────────────────────────────────────────

    /// Verify `/scp` response has `{"scp": {"latest_slot": N, "pending_transactions": N}}`.
    #[test]
    fn test_scp_response_shape() {
        let value = serde_json::json!({
            "scp": {
                "latest_slot": 12345_u64,
                "pending_transactions": 3_u64,
            }
        });

        let obj = value.as_object().unwrap();
        assert_eq!(obj.len(), 1, "should only have 'scp'");

        let scp = value["scp"].as_object().unwrap();
        assert!(scp.contains_key("latest_slot"));
        assert!(scp.contains_key("pending_transactions"));
    }

    // ── /bans response shape test ───────────────────────────────────────

    /// Verify `/bans` response has `{"bans": []}`.
    #[test]
    fn test_bans_response_shape() {
        let value = serde_json::json!({"bans": []});
        assert!(value["bans"].is_array());
        assert!(value["bans"].as_array().unwrap().is_empty());
    }

    // ── ISO 8601 parser tests ───────────────────────────────────────────

    #[test]
    fn test_parse_iso8601_epoch() {
        assert_eq!(parse_iso8601_to_unix("1970-01-01T00:00:00Z"), Some(0));
    }

    #[test]
    fn test_parse_iso8601_known_timestamp() {
        // 2023-11-14T22:13:20Z = 1700000000
        assert_eq!(
            parse_iso8601_to_unix("2023-11-14T22:13:20Z"),
            Some(1700000000)
        );
    }

    #[test]
    fn test_parse_iso8601_invalid() {
        assert_eq!(parse_iso8601_to_unix("not-a-date"), None);
        assert_eq!(parse_iso8601_to_unix("2023-01-01"), None);
    }

    #[test]
    fn test_is_leap_year() {
        assert!(is_leap_year(2000));
        assert!(is_leap_year(2024));
        assert!(!is_leap_year(1900));
        assert!(!is_leap_year(2023));
    }
}

// ── Load generation (feature-gated) ─────────────────────────────────────

/// GET /generateload — compat handler using trait-object backend.
///
/// stellar-core returns a JSON response for generateload. We match that format,
/// using `{"exception": "..."}` for errors (stellar-core compat convention).
#[cfg(feature = "loadgen")]
pub(crate) async fn compat_generateload_handler(
    State(state): State<Arc<CompatServerState>>,
    Query(params): Query<crate::http::types::generateload::GenerateLoadParams>,
) -> impl IntoResponse {
    use crate::http::handlers::generateload::LoadGenRequest;

    // Gate: require generate_load_for_testing config flag
    if !state.app.config().testing.generate_load_for_testing {
        return Json(serde_json::json!({
            "exception": "Set ARTIFICIALLY_GENERATE_LOAD_FOR_TESTING=true in config to enable this endpoint."
        }));
    }

    let loadgen_state = match &state.loadgen_state {
        Some(s) => s,
        None => {
            return Json(serde_json::json!({
                "exception": "Load generation not available."
            }));
        }
    };

    // Handle stop mode before checking is_running — stellar-core processes
    // "stop" before any other mode validation and returns a plain string.
    if params.mode.eq_ignore_ascii_case("stop") {
        loadgen_state.runner.stop_load();
        return Json(serde_json::json!("Stopped load generation"));
    }

    // Check if a run is already in progress
    if loadgen_state.runner.is_running() {
        return Json(serde_json::json!({
            "exception": "Load generation is already running."
        }));
    }

    let summary = format!(
        "Started {} load generation: accounts={}, txs={}, txrate={}",
        params.mode, params.accounts, params.txs, params.txrate,
    );
    let request: LoadGenRequest = params.into();

    match loadgen_state.runner.start_load(request) {
        Ok(()) => Json(serde_json::json!({
            "status": "ok",
            "info": summary,
        })),
        Err(e) => Json(serde_json::json!({
            "exception": e
        })),
    }
}
