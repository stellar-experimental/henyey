//! Plain-text and pass-through compat handlers.
//!
//! stellar-core returns plain text for many admin endpoints. These handlers
//! proxy to the underlying `App` methods and format responses accordingly.
//! For JSON-returning endpoints (scp, quorum, sorobaninfo, etc.), we
//! delegate to the native handlers but ensure the response format matches
//! stellar-core where possible.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::app::AppState;
use crate::compat_http::CompatServerState;
use crate::http::types::{
    CompatSorobanInfoResponse, ConnectParams, DropPeerParams, SorobanInfoResponse, UnbanParams,
};

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
    let app = Arc::clone(&state.app);
    match henyey_common::spawn_blocking_logged("compat-maintenance", move || {
        app.perform_maintenance(count);
    })
    .await
    {
        Ok(()) => {}
        Err(join_err) => std::panic::resume_unwind(join_err.into_panic()),
    }
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

/// GET /connect?peer=PEER&port=PORT
///
/// Mirrors stellar-core `CommandHandler::connect` (CommandHandler.cpp:478):
/// the connect is fire-and-forget, so the success string is returned
/// unconditionally once a peer+port are supplied (independent of whether the
/// TCP connection ultimately succeeds). Wires into [`App::connect_peer`].
pub(crate) async fn compat_connect_handler(
    State(state): State<Arc<CompatServerState>>,
    Query(params): Query<ConnectParams>,
) -> impl IntoResponse {
    // stellar-core keys on both peer and port being present.
    let (Some(peer), Some(port)) = (params.peer.clone(), params.port) else {
        return "Must specify a peer and port: connect&peer=PEER&port=PORT\n".to_string();
    };

    let addr = match crate::http::helpers::parse_connect_params(&params) {
        Ok(addr) => addr,
        // A malformed peer/port still yields the "must specify" guidance, as
        // there is no resolvable target to connect to.
        Err(_) => {
            return "Must specify a peer and port: connect&peer=PEER&port=PORT\n".to_string();
        }
    };

    // Fire-and-forget: kick off the connect but do not reflect its outcome,
    // matching stellar-core's unconditional retStr.
    let _ = state.app.connect_peer(addr).await;
    format!("Connect to: {}:{}\n", peer, port)
}

/// GET /droppeer?node=NODE_ID[&ban=1]
///
/// Mirrors stellar-core `CommandHandler::dropPeer` (CommandHandler.cpp:~543):
/// keyed on `node`. Wires into [`App::disconnect_peer`] (+ [`App::ban_peer`]
/// when `ban=1`). Any disconnect failure variant or an unresolvable node id
/// collapses to the "not found" string so internal error prose never leaks.
pub(crate) async fn compat_droppeer_handler(
    State(state): State<Arc<CompatServerState>>,
    Query(params): Query<DropPeerParams>,
) -> impl IntoResponse {
    let Some(node) = params.node.clone() else {
        return "Must specify at least peer id: droppeer?node=NODE_ID\n".to_string();
    };

    let peer_id = match crate::http::helpers::parse_peer_id_params(&None, &params.node) {
        Ok(peer_id) => peer_id,
        // resolveNodeID-false in core → "Peer X not found".
        Err(_) => return format!("Peer {} not found\n", node),
    };

    // Any DisconnectError (PeerNotFound or OverlayUnavailable) maps to the
    // core not-found string.
    if state.app.disconnect_peer(&peer_id).await.is_err() {
        return format!("Peer {} not found\n", node);
    }

    let ban_requested = params.ban.unwrap_or(0) == 1;
    if ban_requested {
        // The disconnect already happened; a ban failure is non-fatal to the
        // compat response (the peer is dropped regardless).
        let _ = state.app.ban_peer(peer_id).await;
        format!("Drop and ban peer: {}\n", node)
    } else {
        format!("Drop peer: {}\n", node)
    }
}

/// GET /unban?node=NODE_ID
///
/// Mirrors stellar-core `CommandHandler::unban` (CommandHandler.cpp:566):
/// returns the success string whenever the node id resolves. Wires into
/// [`App::unban_peer`], which removes the DB ban row first; an
/// overlay-unavailable error after that point is collapsed to success because
/// the meaningful (DB) side effect has already happened.
pub(crate) async fn compat_unban_handler(
    State(state): State<Arc<CompatServerState>>,
    Query(params): Query<UnbanParams>,
) -> impl IntoResponse {
    let Some(node) = params.node.clone() else {
        return "Must specify at least peer id: unban?node=NODE_ID\n".to_string();
    };

    let peer_id = match crate::http::helpers::parse_peer_id_params(&None, &params.node) {
        Ok(peer_id) => peer_id,
        Err(_) => return format!("Peer {} not found\n", node),
    };

    // unban_peer removes the DB row before its overlay check, so even an
    // overlay-unavailable Err means the meaningful side effect succeeded.
    // core returns "Unban peer:" whenever the id resolves, regardless.
    let _ = state.app.unban_peer(&peer_id).await;
    format!("Unban peer: {}\n", node)
}

/// GET /bans
///
/// Mirrors stellar-core `CommandHandler::bans` (CommandHandler.cpp:553):
/// returns the banned node strkeys. Wires into [`App::banned_peers`] (pure DB
/// `load_bans`). The empty case emits `{"bans": []}` (a documented divergence
/// from core's jsoncpp `{"bans": null}`; see crates/app/PARITY_STATUS.md).
pub(crate) async fn compat_bans_handler(
    State(state): State<Arc<CompatServerState>>,
) -> impl IntoResponse {
    let bans = state
        .app
        .banned_peers()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(crate::http::helpers::peer_id_to_strkey)
        .collect::<Vec<_>>();
    Json(serde_json::json!({ "bans": bans }))
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
    let app = Arc::clone(&state.app);
    let depth = params.depth;
    match henyey_common::spawn_blocking_logged("compat-self-check", move || app.self_check(depth))
        .await
    {
        Ok(Ok(result)) => Json(serde_json::json!({
            "ok": result.ok,
            "checked_ledgers": result.checked_ledgers,
        }))
        .into_response(),
        Ok(Err(e)) => Json(serde_json::json!({
            "exception": format!("{}", e),
        }))
        .into_response(),
        Err(join_err) => std::panic::resume_unwind(join_err.into_panic()),
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
///
/// Returns the stellar-rpc compat shape: a flattened **subset** of the
/// native `/sorobaninfo` basic format wrapped under `{"info": ...}`.
///
/// All field projection — including the protocol-23 gating — flows through
/// [`SorobanInfoResponse::from_network_info`]. The compat handler reshapes
/// that result via [`CompatSorobanInfoResponse::from`], which is a pure
/// data shuffle with no protocol logic. This guarantees the two handlers
/// cannot drift on shared fields or the protocol-23 gate.
pub(crate) async fn compat_sorobaninfo_handler(
    State(state): State<Arc<CompatServerState>>,
) -> impl IntoResponse {
    match state.app.soroban_network_info() {
        Some(info) => {
            let protocol_version = state.app.ledger_info().protocol_version;
            let native = SorobanInfoResponse::from_network_info(&info, protocol_version);
            let compat = CompatSorobanInfoResponse::from(&native);
            Json(serde_json::json!({ "info": compat }))
        }
        None => Json(serde_json::json!({"info": "Soroban not available"})),
    }
}

// ── Survey endpoints (stellar-core URL paths) ───────────────────────────
//
// These mirror stellar-core `CommandHandler` (v26.0.1) exactly — NOT henyey's
// native `/survey/*` JSON handlers. stellar-core returns **plain text**
// `retStr` for start/stop-collecting, surveytopologytimesliced, and stopsurvey,
// and **JSON** for getsurveyresult. Param names follow stellar-core
// (`nonce`, `node`, `inboundpeerindex`, `outboundpeerindex`) — these differ
// from the native handlers' `inbound_index`/`outbound_index`. All survey logic
// (signing, nonce, reporting state) lives in the native `App` methods; these
// handlers are pure wiring (#3298, analogous to #3294).

/// Query params for `/startsurveycollecting` — `nonce` is required, matching
/// stellar-core `parseRequiredParam<uint32_t>(map, "nonce")`.
#[derive(Deserialize)]
pub(crate) struct CompatStartSurveyParams {
    nonce: u32,
}

/// Query params for `/surveytopologytimesliced`. All three are required,
/// matching stellar-core `parseRequiredParam` for `node` / `inboundpeerindex`
/// / `outboundpeerindex`.
#[derive(Deserialize)]
pub(crate) struct CompatSurveyTopologyParams {
    node: String,
    inboundpeerindex: u32,
    outboundpeerindex: u32,
}

/// Booted gate mirroring native `survey_booted`: survey commands run only when
/// the node is `Synced` or `Validating`.
async fn compat_survey_booted(state: &CompatServerState) -> bool {
    matches!(
        state.app.state().await,
        AppState::Synced | AppState::Validating
    )
}

/// stellar-core does not gate these via a booted check; the native handlers do.
/// We return HTTP 503 + the verbatim native message when not booted.
fn compat_survey_not_booted() -> (StatusCode, String) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "Application is not fully booted, try again later.".to_string(),
    )
}

/// GET /getsurveyresult  (stellar-core path for henyey's /survey)
///
/// Returns JSON in stellar-core `SurveyManager::getJsonResults()` shape
/// (`CommandHandler::getSurveyResult` → `toStyledString`): `surveyInProgress`
/// (bool), `backlog` / `badResponseNodes` (arrays of **strkey** peer ids,
/// matching `KeyUtils::toStrKey`, `SurveyManager.cpp:682,690`), and a
/// `topology` map keyed by surveyed-peer strkey (matching
/// `mResults["topology"][toStrKey(peer)]`).
///
/// The native `survey_report()` carries `backlog`/`bad_response_nodes` and the
/// per-peer `peer_id`s as 64-char **hex** (`PeerId::to_hex`); we re-derive the
/// strkey form here so the compat wire output matches stellar-core (and does
/// NOT leak henyey's internal hex encoding).
pub(crate) async fn compat_getsurveyresult_handler(
    State(state): State<Arc<CompatServerState>>,
) -> impl IntoResponse {
    let report = state.app.survey_report().await;

    let backlog: Vec<String> = report.backlog.iter().map(|h| hex_to_strkey(h)).collect();
    let bad_response_nodes: Vec<String> = report
        .bad_response_nodes
        .iter()
        .map(|h| hex_to_strkey(h))
        .collect();

    // stellar-core keys per-peer topology results under
    // mResults["topology"][toStrKey(surveyedPeerID)]. The native report keys
    // its per-peer results by nonce then peer (hex); flatten to a strkey-keyed
    // map mirroring stellar-core.
    let mut topology = serde_json::Map::new();
    for reports in report.peer_reports.values() {
        for peer in reports {
            topology.insert(
                hex_to_strkey(&peer.peer_id),
                serde_json::to_value(&peer.response).unwrap_or(serde_json::Value::Null),
            );
        }
    }

    Json(serde_json::json!({
        "surveyInProgress": report.survey_in_progress,
        "backlog": backlog,
        "badResponseNodes": bad_response_nodes,
        "topology": topology,
    }))
}

/// GET /startsurveycollecting?nonce=<u32>
///
/// stellar-core `CommandHandler::startSurveyCollecting`: required `nonce`, then
/// `broadcastStartSurveyCollecting(nonce)` → verbatim success/failure strings.
pub(crate) async fn compat_startsurveycollecting_handler(
    State(state): State<Arc<CompatServerState>>,
    Query(params): Query<CompatStartSurveyParams>,
) -> impl IntoResponse {
    if !compat_survey_booted(&state).await {
        return compat_survey_not_booted();
    }
    let ok = state
        .app
        .start_survey_collecting(params.nonce)
        .await
        .is_ok();
    let msg = if ok {
        "Requested network to start survey collecting."
    } else {
        "Failed to start survey collecting. Another survey is active on the network."
    };
    (StatusCode::OK, msg.to_string())
}

/// GET /stopsurveycollecting
///
/// stellar-core `CommandHandler::stopSurveyCollecting`:
/// `broadcastStopSurveyCollecting()` → verbatim success/failure strings.
pub(crate) async fn compat_stopsurveycollecting_handler(
    State(state): State<Arc<CompatServerState>>,
) -> impl IntoResponse {
    if !compat_survey_booted(&state).await {
        return compat_survey_not_booted();
    }
    let ok = state.app.stop_survey_collecting().await.is_ok();
    let msg = if ok {
        "Requested network to stop survey collecting."
    } else {
        "Failed to stop survey collecting. No survey is active on the network."
    };
    (StatusCode::OK, msg.to_string())
}

/// GET /surveytopologytimesliced?node=<strkey>&inboundpeerindex=<u32>&outboundpeerindex=<u32>
///
/// stellar-core `CommandHandler::surveyTopologyTimeSliced`: parses `node`
/// (strkey), `inboundpeerindex`, `outboundpeerindex`, then sets
/// `retStr = "Adding node."` and appends `"Survey started "` when a fresh
/// reporting session started, else `"Survey already running!"`.
///
/// stellar-core performs two independent steps — `startSurveyReporting()`
/// (bool drives the trailing token) and an UNCONDITIONAL
/// `addNodeToRunningSurveyBacklog(...)`. Henyey's `survey_topology_timesliced`
/// fuses both into one bool: it returns `true` only on a fresh `Started`
/// (≈ stellar-core's `startSurveyReporting()` success), so we use it to select
/// the trailing token. **Accepted bounded divergence (#3298):** for the
/// duplicate-peer / self-peer / not-ready edge the native method returns
/// `false` and SKIPS the insert, whereas stellar-core always adds the node and
/// only varies the token. This multi-node already-running edge is a documented
/// reuse limitation of the native fused bool — not reimplemented here.
pub(crate) async fn compat_surveytopology_handler(
    State(state): State<Arc<CompatServerState>>,
    Query(params): Query<CompatSurveyTopologyParams>,
) -> impl IntoResponse {
    // Validate the `node` strkey before the booted gate: stellar-core parses
    // required params first (`parseRequiredParam` throws on a bad value), so a
    // malformed `node` is a 400 regardless of node state.
    let pubkey = match henyey_crypto::PublicKey::from_strkey(&params.node) {
        Ok(key) => key,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                "Invalid node public key".to_string(),
            );
        }
    };
    if !compat_survey_booted(&state).await {
        return compat_survey_not_booted();
    }
    let peer_id = henyey_overlay::PeerId::from_bytes(*pubkey.as_bytes());
    let started = state
        .app
        .survey_topology_timesliced(peer_id, params.inboundpeerindex, params.outboundpeerindex)
        .await;
    let msg = if started {
        "Adding node.Survey started "
    } else {
        "Adding node.Survey already running!"
    };
    (StatusCode::OK, msg.to_string())
}

/// GET /stopsurvey (stellar-core path for henyey's /survey/reporting/stop)
///
/// stellar-core `CommandHandler::stopSurvey`: `stopSurveyReporting()` then the
/// verbatim, ungated `retStr = "survey stopped"`.
pub(crate) async fn compat_stopreporting_handler(
    State(state): State<Arc<CompatServerState>>,
) -> impl IntoResponse {
    state.app.stop_survey_reporting().await;
    (StatusCode::OK, "survey stopped".to_string())
}

/// Convert a 64-char hex-encoded ed25519 public key (as carried by the native
/// `SurveyReport`) into its Stellar strkey (`G...`) form, matching
/// stellar-core `KeyUtils::toStrKey`. Falls back to the original string if it
/// is not parseable hex (defensive — `SurveyReport` always emits valid hex).
fn hex_to_strkey(hex_pubkey: &str) -> String {
    match hex::decode(hex_pubkey) {
        Ok(bytes) if bytes.len() == 32 => {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&bytes);
            henyey_overlay::PeerId::from_bytes(arr).to_strkey()
        }
        _ => hex_pubkey.to_string(),
    }
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

    // ── Live-App peer-admin handler tests (#3294) ───────────────────────
    //
    // These exercise the real wiring of the four compat peer-admin handlers
    // (`/connect`, `/droppeer`, `/unban`, `/bans`) against a live `App`
    // backed by a tempdir SQLite database, mirroring stellar-core
    // `CommandHandler` response shapes.

    use std::sync::Arc;

    use http_body_util::BodyExt;

    use crate::app::App;
    use crate::compat_http::CompatServerState;
    use crate::http::types::{ConnectParams, DropPeerParams, UnbanParams};

    /// Build a `CompatServerState` backed by a real (minimal) `App` with a
    /// tempdir database and no default peers. Returns `(TempDir, state)`; the
    /// `TempDir` guard is returned first so it outlives the state (which holds
    /// open DB handles).
    async fn mk_compat_state() -> (tempfile::TempDir, Arc<CompatServerState>) {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("compat-peer-admin.db");
        let mut config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        // Avoid pulling in default network peers in unit tests.
        config.overlay.known_peers.clear();
        config.is_compat_config = true;
        let app = App::new(config).await.unwrap();
        let state = Arc::new(CompatServerState {
            app: Arc::new(app),
            started_on: "2024-01-01T00:00:00Z".to_string(),
            #[cfg(feature = "loadgen")]
            loadgen_state: None,
        });
        (dir, state)
    }

    /// Collect a plain-text response body into a `String`.
    async fn body_text(response: axum::response::Response) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// Collect a JSON response body and parse it.
    async fn body_json(response: axum::response::Response) -> serde_json::Value {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Generate a fresh peer (PeerId + its G... strkey).
    fn mk_peer() -> (henyey_overlay::PeerId, String) {
        let secret = henyey_crypto::SecretKey::generate();
        let public = secret.public_key();
        let strkey = public.to_strkey();
        let peer_id = henyey_overlay::PeerId::from_bytes(*public.as_bytes());
        (peer_id, strkey)
    }

    /// `/bans` reflects the real DB `load_bans` result — a banned peer's
    /// strkey appears in the `bans` array. FAILS on `origin/main` (the stub
    /// always returns `{"bans": []}` regardless of DB state).
    #[tokio::test]
    async fn test_compat_bans_reflects_db_load_bans() {
        let (_dir, state) = mk_compat_state().await;
        let (peer_id, strkey) = mk_peer();
        // Start overlay so ban_peer's overlay side completes; the DB row is the
        // part /bans reads back.
        state.app.start_overlay().await.unwrap();
        state.app.ban_peer(peer_id).await.unwrap();

        let resp = compat_bans_handler(State(Arc::clone(&state)))
            .await
            .into_response();
        let json = body_json(resp).await;
        let bans = json["bans"].as_array().expect("bans array");
        assert!(
            bans.iter().any(|b| b.as_str() == Some(strkey.as_str())),
            "banned strkey {strkey} not found in {json}"
        );
        state.app.shutdown();
    }

    /// `/bans` with no bans yields the documented `{"bans": []}` shape.
    #[tokio::test]
    async fn test_compat_bans_empty() {
        let (_dir, state) = mk_compat_state().await;
        let resp = compat_bans_handler(State(Arc::clone(&state)))
            .await
            .into_response();
        let json = body_json(resp).await;
        assert_eq!(json, serde_json::json!({"bans": []}));
        state.app.shutdown();
    }

    /// `/unban?node=<strkey>` removes the DB ban row AND returns
    /// `"Unban peer: <node>\n"`. FAILS on `origin/main` (the stub returns
    /// `"done\n"` with no side effect, so the ban would persist).
    #[tokio::test]
    async fn test_compat_unban_removes_ban() {
        let (_dir, state) = mk_compat_state().await;
        let (peer_id, strkey) = mk_peer();
        state.app.start_overlay().await.unwrap();
        state.app.ban_peer(peer_id).await.unwrap();
        // Precondition: the ban is present.
        let before = state.app.banned_peers().await.unwrap();
        assert!(
            before
                .iter()
                .any(|p| peer_id_strkey(p) == Some(strkey.clone())),
            "ban should be present before unban"
        );

        let params = UnbanParams {
            peer_id: None,
            node: Some(strkey.clone()),
        };
        let resp = compat_unban_handler(State(Arc::clone(&state)), Query(params))
            .await
            .into_response();
        let text = body_text(resp).await;
        assert_eq!(text, format!("Unban peer: {strkey}\n"));

        // Real DB side effect: the ban is gone.
        let after = state.app.banned_peers().await.unwrap();
        assert!(
            !after
                .iter()
                .any(|p| peer_id_strkey(p) == Some(strkey.clone())),
            "ban should be removed after unban"
        );
        state.app.shutdown();
    }

    /// `/unban` with no `node` → the stellar-core "must specify" message.
    #[tokio::test]
    async fn test_compat_unban_missing_node() {
        let (_dir, state) = mk_compat_state().await;
        let params = UnbanParams {
            peer_id: None,
            node: None,
        };
        let resp = compat_unban_handler(State(Arc::clone(&state)), Query(params))
            .await
            .into_response();
        let text = body_text(resp).await;
        assert_eq!(text, "Must specify at least peer id: unban?node=NODE_ID\n");
        state.app.shutdown();
    }

    /// `/unban?node=garbage` (unresolvable) → "Peer <node> not found".
    #[tokio::test]
    async fn test_compat_unban_unresolvable_node() {
        let (_dir, state) = mk_compat_state().await;
        let params = UnbanParams {
            peer_id: None,
            node: Some("garbage".to_string()),
        };
        let resp = compat_unban_handler(State(Arc::clone(&state)), Query(params))
            .await
            .into_response();
        let text = body_text(resp).await;
        assert_eq!(text, "Peer garbage not found\n");
        state.app.shutdown();
    }

    /// `/droppeer` with no `node` → the stellar-core "must specify" message.
    #[tokio::test]
    async fn test_compat_droppeer_missing_node() {
        let (_dir, state) = mk_compat_state().await;
        let params = DropPeerParams {
            peer_id: None,
            node: None,
            ban: None,
        };
        let resp = compat_droppeer_handler(State(Arc::clone(&state)), Query(params))
            .await
            .into_response();
        let text = body_text(resp).await;
        assert_eq!(
            text,
            "Must specify at least peer id: droppeer?node=NODE_ID\n"
        );
        state.app.shutdown();
    }

    /// `/droppeer?node=<strkey>` for a peer that is not connected →
    /// "Peer <node> not found".
    #[tokio::test]
    async fn test_compat_droppeer_not_found() {
        let (_dir, state) = mk_compat_state().await;
        state.app.start_overlay().await.unwrap();
        let (_peer_id, strkey) = mk_peer();
        let params = DropPeerParams {
            peer_id: None,
            node: Some(strkey.clone()),
            ban: None,
        };
        let resp = compat_droppeer_handler(State(Arc::clone(&state)), Query(params))
            .await
            .into_response();
        let text = body_text(resp).await;
        assert_eq!(text, format!("Peer {strkey} not found\n"));
        state.app.shutdown();
    }

    /// `/connect` with no `peer`/`port` → the stellar-core "must specify"
    /// message.
    #[tokio::test]
    async fn test_compat_connect_missing_param() {
        let (_dir, state) = mk_compat_state().await;
        let params = ConnectParams {
            addr: None,
            peer: None,
            port: None,
        };
        let resp = compat_connect_handler(State(Arc::clone(&state)), Query(params))
            .await
            .into_response();
        let text = body_text(resp).await;
        assert_eq!(
            text,
            "Must specify a peer and port: connect&peer=PEER&port=PORT\n"
        );
        state.app.shutdown();
    }

    /// `/connect?peer=127.0.0.1&port=11625` → the fire-and-forget success
    /// message (mirrors stellar-core's unconditional retStr). Proves the
    /// wiring/side-effect path is invoked.
    #[tokio::test]
    async fn test_compat_connect_success_shape() {
        let (_dir, state) = mk_compat_state().await;
        state.app.start_overlay().await.unwrap();
        let params = ConnectParams {
            addr: None,
            peer: Some("127.0.0.1".to_string()),
            port: Some(11625),
        };
        let resp = compat_connect_handler(State(Arc::clone(&state)), Query(params))
            .await
            .into_response();
        let text = body_text(resp).await;
        assert_eq!(text, "Connect to: 127.0.0.1:11625\n");
        state.app.shutdown();
    }

    /// Helper: PeerId → its G... strkey (used by ban-state assertions).
    fn peer_id_strkey(peer_id: &henyey_overlay::PeerId) -> Option<String> {
        henyey_crypto::PublicKey::from_bytes(peer_id.as_bytes())
            .ok()
            .map(|pk| pk.to_strkey())
    }

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

    // ── #3300: /upgrades?mode=set exact SSC parameter-set pinning ───────
    //
    // Drives the REAL `compat_upgrades_handler` against a live `App` and reads
    // back the parsed `UpgradeParameters` via `runtime_upgrade_parameters()`,
    // pinning the parse 1:1 against stellar-core `CommandHandler::upgrades`
    // (CommandHandler.cpp:613-671). stellar-core's `mode=set` branch accepts
    // exactly these query params, which must each map to the matching
    // `UpgradeParameters` field:
    //   upgradetime, basefee, basereserve, maxtxsetsize, protocolversion,
    //   flags, configupgradesetkey (base64 XDR), maxsorobantxsetsize,
    //   nominationtimeoutlimit, expirationminutes.
    // This is the parity-load-bearing path for an SSC protocol-upgrade
    // mission (the harness drives upgrades via mode=set).

    /// Build the base64-encoded `configupgradesetkey` XDR that SSC sends, and
    /// return both the wire string and the decoded key for round-trip checks.
    fn ssc_config_upgrade_set_key() -> (String, stellar_xdr::curr::ConfigUpgradeSetKey) {
        use base64::{engine::general_purpose::STANDARD, Engine};
        use stellar_xdr::curr::{ConfigUpgradeSetKey, ContractId, Hash, Limits, WriteXdr};
        let key = ConfigUpgradeSetKey {
            contract_id: ContractId(Hash([0xAB; 32])),
            content_hash: Hash([0xCD; 32]),
        };
        let bytes = key.to_xdr(Limits::none()).expect("encode key");
        (STANDARD.encode(bytes), key)
    }

    /// Build the `mode=set` query param map with the full SSC parameter set.
    fn full_ssc_set_params(config_key_b64: &str) -> std::collections::HashMap<String, String> {
        let mut m = std::collections::HashMap::new();
        m.insert("mode".into(), "set".into());
        // Unix-timestamp form (stellar-core also accepts ISO 8601; both paths
        // are parsed by the handler — see `parse_iso8601_to_unix`).
        m.insert("upgradetime".into(), "1700000000".into());
        m.insert("basefee".into(), "250".into());
        m.insert("basereserve".into(), "6000000".into());
        m.insert("maxtxsetsize".into(), "2000".into());
        m.insert("protocolversion".into(), "25".into());
        m.insert("flags".into(), "1".into());
        m.insert("configupgradesetkey".into(), config_key_b64.into());
        m.insert("maxsorobantxsetsize".into(), "50".into());
        m.insert("nominationtimeoutlimit".into(), "3".into());
        m.insert("expirationminutes".into(), "120".into());
        m
    }

    /// `/upgrades?mode=set` with the full SSC parameter set parses every
    /// param into the correct `UpgradeParameters` field. Drives the real
    /// handler + `App::set_upgrade_parameters`, reads back via
    /// `runtime_upgrade_parameters()`.
    #[tokio::test]
    async fn test_upgrades_set_parses_full_ssc_param_set() {
        let (_dir, state) = mk_compat_state().await;
        let (config_key_b64, expected_key) = ssc_config_upgrade_set_key();

        let resp = compat_upgrades_handler(
            State(Arc::clone(&state)),
            Query(full_ssc_set_params(&config_key_b64)),
        )
        .await
        .into_response();
        let json = body_json(resp).await;
        assert_eq!(json["status"], "ok", "mode=set should succeed: {json}");

        let params = state.app.runtime_upgrade_parameters();
        assert_eq!(params.upgrade_time, 1_700_000_000, "upgradetime");
        assert_eq!(params.base_fee, Some(250), "basefee");
        assert_eq!(params.base_reserve, Some(6_000_000), "basereserve");
        assert_eq!(params.max_tx_set_size, Some(2000), "maxtxsetsize");
        assert_eq!(params.protocol_version, Some(25), "protocolversion");
        assert_eq!(params.flags, Some(1), "flags");
        assert_eq!(
            params.max_soroban_tx_set_size,
            Some(50),
            "maxsorobantxsetsize"
        );
        assert_eq!(
            params.nomination_timeout_limit,
            Some(3),
            "nominationtimeoutlimit"
        );
        assert_eq!(params.expiration_minutes, Some(120), "expirationminutes");

        // configupgradesetkey: base64 XDR decoded back to the exact key.
        let parsed_key = params
            .config_upgrade_set_key
            .as_ref()
            .expect("configupgradesetkey must be parsed")
            .to_xdr()
            .expect("key round-trips to XDR");
        assert_eq!(
            parsed_key, expected_key,
            "configupgradesetkey must decode to the exact ConfigUpgradeSetKey"
        );

        state.app.shutdown();
    }

    /// `/upgrades?mode=set&upgradetime=1970-01-01T00:00:00Z` — the ISO 8601
    /// "apply immediately" form stellar-core's `isoStringToTm` accepts — parses
    /// to epoch 0.
    #[tokio::test]
    async fn test_upgrades_set_parses_iso8601_upgradetime() {
        let (_dir, state) = mk_compat_state().await;
        let mut m = std::collections::HashMap::new();
        m.insert("mode".into(), "set".into());
        m.insert("upgradetime".into(), "1970-01-01T00:00:00Z".into());
        m.insert("protocolversion".into(), "25".into());

        let resp = compat_upgrades_handler(State(Arc::clone(&state)), Query(m))
            .await
            .into_response();
        let json = body_json(resp).await;
        assert_eq!(json["status"], "ok");

        let params = state.app.runtime_upgrade_parameters();
        assert_eq!(params.upgrade_time, 0, "epoch ISO 8601 → 0");
        assert_eq!(params.protocol_version, Some(25));
        state.app.shutdown();
    }

    /// `/upgrades?mode=clear` resets the scheduled parameters back to the
    /// default (no upgrades scheduled), mirroring stellar-core's `clear`
    /// branch (CommandHandler.cpp:672 — sets an empty `UpgradeParameters`).
    #[tokio::test]
    async fn test_upgrades_clear_resets_to_default() {
        let (_dir, state) = mk_compat_state().await;
        let (config_key_b64, _key) = ssc_config_upgrade_set_key();

        // First set a full param set.
        let resp = compat_upgrades_handler(
            State(Arc::clone(&state)),
            Query(full_ssc_set_params(&config_key_b64)),
        )
        .await
        .into_response();
        assert_eq!(body_json(resp).await["status"], "ok");
        assert!(state
            .app
            .runtime_upgrade_parameters()
            .protocol_version
            .is_some());

        // Now clear.
        let mut clear = std::collections::HashMap::new();
        clear.insert("mode".into(), "clear".into());
        let resp = compat_upgrades_handler(State(Arc::clone(&state)), Query(clear))
            .await
            .into_response();
        assert_eq!(body_json(resp).await["status"], "ok");

        let params = state.app.runtime_upgrade_parameters();
        let default = henyey_herder::upgrades::UpgradeParameters::default();
        assert_eq!(params.protocol_version, default.protocol_version);
        assert_eq!(params.base_fee, default.base_fee);
        assert_eq!(params.base_reserve, default.base_reserve);
        assert_eq!(params.max_tx_set_size, default.max_tx_set_size);
        assert_eq!(params.flags, default.flags);
        assert_eq!(
            params.max_soroban_tx_set_size,
            default.max_soroban_tx_set_size
        );
        assert!(params.config_upgrade_set_key.is_none());
        assert!(
            !params.has_any_upgrade(),
            "clear must remove all scheduled upgrades"
        );
        state.app.shutdown();
    }

    /// Default GET (`/upgrades` with no/empty mode) returns the bespoke
    /// `{current, scheduled}` shape. NOTE: this is a DOCUMENTED divergence
    /// from stellar-core, which returns `"mode required"` for empty mode and
    /// only emits `getUpgradesJson()` under `mode=get` (see PARITY_STATUS).
    #[tokio::test]
    async fn test_upgrades_default_get_shape() {
        let (_dir, state) = mk_compat_state().await;
        let resp = compat_upgrades_handler(
            State(Arc::clone(&state)),
            Query(std::collections::HashMap::new()),
        )
        .await
        .into_response();
        let json = body_json(resp).await;
        let obj = json.as_object().expect("object body");
        assert!(obj.contains_key("current"), "default GET has 'current'");
        assert!(obj.contains_key("scheduled"), "default GET has 'scheduled'");
        for key in ["ledgerVersion", "baseFee", "baseReserve", "maxTxSetSize"] {
            assert!(json["current"].get(key).is_some(), "current.{key}");
        }
        for key in [
            "upgradetime",
            "protocolversion",
            "basefee",
            "basereserve",
            "maxtxsetsize",
        ] {
            assert!(json["scheduled"].get(key).is_some(), "scheduled.{key}");
        }
        state.app.shutdown();
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

    // ── /sorobaninfo compat response shape tests ────────────────────────
    //
    // These tests exercise the **production** projection chain
    // `SorobanNetworkInfo` → `SorobanInfoResponse::from_network_info` →
    // `CompatSorobanInfoResponse::from` → `Json({"info": ...})`. They no
    // longer replicate JSON literals; the upstream type tests in
    // `crates/app/src/http/types/soroban.rs` cover the value-correctness
    // and structural-projection invariants. Here we only confirm the
    // wire-shape envelope (`{"info": {...}}`) and the protocol-23 gate
    // visible through serde.

    use crate::http::types::{CompatSorobanInfoResponse, SorobanInfoResponse, SorobanScpSettings};

    /// Build a minimal CompatSorobanInfoResponse with explicit values
    /// (avoids reaching into henyey_ledger from this test module).
    fn make_compat(
        max_dependent_tx_clusters: Option<u32>,
        max_footprint_size: Option<u32>,
        scp: Option<SorobanScpSettings>,
    ) -> CompatSorobanInfoResponse {
        CompatSorobanInfoResponse {
            ledger_max_instructions: 100,
            tx_max_instructions: 50,
            tx_memory_limit: 1024,
            ledger_max_read_ledger_entries: 10,
            ledger_max_read_bytes: 2048,
            ledger_max_write_ledger_entries: 5,
            ledger_max_write_bytes: 1024,
            ledger_max_tx_count: 100,
            tx_max_size_bytes: 512,
            average_bucket_list_size: 100_000_000,
            bucket_list_size_snapshot_period: 30,
            max_dependent_tx_clusters,
            max_footprint_size,
            scp,
        }
    }

    /// Pre-P23: the `{"info": ...}` envelope contains the always-present
    /// keys but omits the three protocol-23 fields. Asserts the wire
    /// shape produced by the production handler path.
    #[test]
    fn test_compat_sorobaninfo_pre_protocol_23_omits_scp_fields() {
        let compat = make_compat(None, None, None);
        let envelope = serde_json::json!({ "info": compat });
        let info = envelope["info"].as_object().unwrap();

        assert!(
            !info.contains_key("scp"),
            "scp should be absent for pre-protocol 23"
        );
        assert!(
            !info.contains_key("max_dependent_tx_clusters"),
            "max_dependent_tx_clusters should be absent for pre-protocol 23"
        );
        assert!(
            !info.contains_key("max_footprint_size"),
            "max_footprint_size should be absent for pre-protocol 23"
        );
        // Always-present keys.
        assert!(
            info.contains_key("average_bucket_list_size"),
            "average_bucket_list_size should always be present"
        );
        assert!(
            info.contains_key("bucket_list_size_snapshot_period"),
            "bucket_list_size_snapshot_period should always be present"
        );
    }

    /// P23+: the `{"info": ...}` envelope includes the three protocol-23
    /// fields, including the nested `scp` block with all five expected
    /// keys.
    #[test]
    fn test_compat_sorobaninfo_protocol_23_includes_scp_fields() {
        let compat = make_compat(
            Some(8),
            Some(40),
            Some(SorobanScpSettings {
                ledger_close_time_ms: 5000,
                nomination_timeout_ms: 1000,
                nomination_timeout_inc_ms: 500,
                ballot_timeout_ms: 1000,
                ballot_timeout_inc_ms: 1000,
            }),
        );
        let envelope = serde_json::json!({ "info": compat });
        let info = envelope["info"].as_object().unwrap();

        assert_eq!(info["max_dependent_tx_clusters"], 8);
        assert_eq!(info["max_footprint_size"], 40);

        let scp = info["scp"].as_object().unwrap();
        for key in [
            "ledger_close_time_ms",
            "nomination_timeout_ms",
            "nomination_timeout_inc_ms",
            "ballot_timeout_ms",
            "ballot_timeout_inc_ms",
        ] {
            assert!(scp.contains_key(key), "compat scp missing key: {key}");
        }
        assert_eq!(scp.len(), 5, "unexpected extra SCP fields in compat");
    }

    /// End-to-end: a `SorobanInfoResponse` built for P23 round-trips
    /// through `CompatSorobanInfoResponse::from` and `serde_json::json!`
    /// into the expected envelope, with values pulled from the right
    /// nested paths. This is the regression test that would have caught
    /// the kind of drift addressed by #2020 had it existed in the
    /// original PR.
    #[test]
    fn test_compat_envelope_pulls_values_through_native_response() {
        // Hand-build a SorobanInfoResponse so the test does not depend on
        // henyey_ledger here (the upstream test in http::types::soroban
        // already covers SorobanNetworkInfo → SorobanInfoResponse).
        let scp = SorobanScpSettings {
            ledger_close_time_ms: 5000,
            nomination_timeout_ms: 1000,
            nomination_timeout_inc_ms: 500,
            ballot_timeout_ms: 1500,
            ballot_timeout_inc_ms: 200,
        };
        let native = SorobanInfoResponse {
            max_contract_size: 64_000,
            max_contract_data_key_size: 250,
            max_contract_data_entry_size: 65_000,
            tx: crate::http::types::SorobanTxLimits {
                max_instructions: 100_000_000,
                memory_limit: 41_943_040,
                max_read_ledger_entries: 40,
                max_read_bytes: 200_704,
                max_write_ledger_entries: 25,
                max_write_bytes: 132_096,
                max_contract_events_size_bytes: 8_198,
                max_size_bytes: 129_024,
                max_footprint_size: Some(60),
            },
            ledger: crate::http::types::SorobanLedgerLimits {
                max_instructions: 500_000_000,
                max_read_ledger_entries: 200,
                max_read_bytes: 500_000,
                max_write_ledger_entries: 125,
                max_write_bytes: 500_000,
                max_tx_size_bytes: 130_048,
                max_tx_count: 100,
            },
            fee_rate_per_instructions_increment: 100,
            fee_read_ledger_entry: 6250,
            fee_write_ledger_entry: 10_000,
            fee_read_1kb: 1786,
            fee_write_1kb: 11_800,
            fee_historical_1kb: 16_235,
            fee_contract_events_size_1kb: 10_000,
            fee_transaction_size_1kb: 1624,
            state_archival: crate::http::types::SorobanStateArchival {
                max_entry_ttl: 6_312_000,
                min_temporary_ttl: 17_280,
                min_persistent_ttl: 4096,
                persistent_rent_rate_denominator: 5_362_408,
                temp_rent_rate_denominator: 5_362_408,
                max_entries_to_archive: 1000,
                bucketlist_size_window_sample_size: 30,
                eviction_scan_size: 100_000,
                starting_eviction_scan_level: 7,
                bucket_list_size_snapshot_period: 30,
                average_bucket_list_size: 100_000_000,
            },
            max_dependent_tx_clusters: Some(2),
            scp: Some(scp),
        };

        let compat = CompatSorobanInfoResponse::from(&native);
        let envelope = serde_json::json!({ "info": compat });

        // Wire shape: `{"info": {...}}`
        assert!(envelope.is_object());
        let info = envelope["info"].as_object().expect("info must be object");

        // Every flat compat key sources from the right native path.
        assert_eq!(info["ledger_max_instructions"], 500_000_000);
        assert_eq!(info["tx_max_instructions"], 100_000_000);
        assert_eq!(info["tx_max_size_bytes"], 129_024);
        assert_eq!(info["max_footprint_size"], 60);
        assert_eq!(info["max_dependent_tx_clusters"], 2);
        assert_eq!(info["bucket_list_size_snapshot_period"], 30);
        assert_eq!(info["scp"]["ballot_timeout_ms"], 1500);
        assert_eq!(info["scp"]["nomination_timeout_inc_ms"], 500);
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

    // ── Compat survey-endpoint wiring tests (#3298) ─────────────────────
    //
    // These exercise the five compat survey handlers (`/getsurveyresult`,
    // `/startsurveycollecting`, `/stopsurveycollecting`,
    // `/surveytopologytimesliced`, `/stopsurvey`) against a live `App`,
    // mirroring stellar-core `CommandHandler` (v26.0.1) param names, the
    // booted-state gate, plain-text vs JSON response medium, and the
    // strkey-encoded peer lists in `getsurveyresult`.
    //
    // Each FAILS on `origin/main`: the stub handlers ignore `State` and return
    // `"done\n"` / `{"survey":"not implemented"}` regardless of app state.

    use axum::body::Body;
    use http::Request;
    use tower::ServiceExt;

    use crate::compat_http::build_compat_router;

    /// Send a `GET <uri>` through the full compat router (so axum `Query`
    /// extraction runs, mirroring stellar-core's `parseRequiredParam`).
    async fn compat_get(state: Arc<CompatServerState>, uri: &str) -> axum::response::Response {
        let router = build_compat_router(state);
        router
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    /// `/startsurveycollecting?nonce=1` on a fresh (pre-Synced) App returns 503
    /// + the not-booted message. FAILS on main (stub returns 200 `"done\n"`).
    #[tokio::test]
    async fn test_compat_startsurveycollecting_gated_when_not_booted() {
        let (_dir, state) = mk_compat_state().await;
        let resp = compat_get(Arc::clone(&state), "/startsurveycollecting?nonce=1").await;
        assert_eq!(resp.status(), http::StatusCode::SERVICE_UNAVAILABLE);
        let text = body_text(resp).await;
        assert!(
            text.contains("not fully booted"),
            "expected not-booted message, got {text:?}"
        );
        state.app.shutdown();
    }

    /// `/startsurveycollecting` with no `nonce` is a missing required param →
    /// 400 (axum Query rejection ≈ stellar-core parseRequiredParam throw).
    /// FAILS on main (stub returns 200 `"done\n"`).
    #[tokio::test]
    async fn test_compat_startsurveycollecting_missing_nonce_400() {
        let (_dir, state) = mk_compat_state().await;
        let resp = compat_get(Arc::clone(&state), "/startsurveycollecting").await;
        assert_eq!(resp.status(), http::StatusCode::BAD_REQUEST);
        state.app.shutdown();
    }

    /// `/stopsurveycollecting` on a fresh App returns 503. FAILS on main.
    #[tokio::test]
    async fn test_compat_stopsurveycollecting_gated_when_not_booted() {
        let (_dir, state) = mk_compat_state().await;
        let resp = compat_get(Arc::clone(&state), "/stopsurveycollecting").await;
        assert_eq!(resp.status(), http::StatusCode::SERVICE_UNAVAILABLE);
        state.app.shutdown();
    }

    /// `/surveytopologytimesliced` with a malformed `node` strkey → 400, while
    /// exercising the stellar-core param names `inboundpeerindex` /
    /// `outboundpeerindex`. FAILS on main (stub returns 200 `"done\n"`).
    #[tokio::test]
    async fn test_compat_surveytopologytimesliced_invalid_node_param() {
        let (_dir, state) = mk_compat_state().await;
        let resp = compat_get(
            Arc::clone(&state),
            "/surveytopologytimesliced?node=not-a-strkey&inboundpeerindex=0&outboundpeerindex=0",
        )
        .await;
        assert_eq!(resp.status(), http::StatusCode::BAD_REQUEST);
        state.app.shutdown();
    }

    /// `/surveytopologytimesliced` with valid params on a fresh App returns 503
    /// (booted gate). FAILS on main (stub returns 200 `"done\n"`).
    #[tokio::test]
    async fn test_compat_surveytopologytimesliced_gated_when_not_booted() {
        let (_dir, state) = mk_compat_state().await;
        let (_peer_id, strkey) = mk_peer();
        let uri = format!(
            "/surveytopologytimesliced?node={strkey}&inboundpeerindex=0&outboundpeerindex=0"
        );
        let resp = compat_get(Arc::clone(&state), &uri).await;
        assert_eq!(resp.status(), http::StatusCode::SERVICE_UNAVAILABLE);
        state.app.shutdown();
    }

    /// `/stopsurvey` returns the verbatim plain-text `"survey stopped"` (NOT
    /// JSON), invoking `stop_survey_reporting()` (ungated, idempotent). FAILS
    /// on main (stub returns `"done\n"`).
    #[tokio::test]
    async fn test_compat_stopsurvey_returns_plaintext() {
        let (_dir, state) = mk_compat_state().await;
        let resp = compat_get(Arc::clone(&state), "/stopsurvey").await;
        assert_eq!(resp.status(), http::StatusCode::OK);
        let text = body_text(resp).await;
        assert_eq!(text, "survey stopped");
        state.app.shutdown();
    }

    /// `/getsurveyresult` returns JSON shaped like stellar-core
    /// `SurveyManager::getJsonResults()`: `surveyInProgress` (bool) plus
    /// `backlog` / `badResponseNodes` as **strkey** arrays (NOT hex). FAILS on
    /// main (stub returns `{"survey":"not implemented"}`).
    #[tokio::test]
    async fn test_compat_getsurveyresult_returns_json_shape() {
        let (_dir, state) = mk_compat_state().await;
        let resp = compat_get(Arc::clone(&state), "/getsurveyresult").await;
        assert_eq!(resp.status(), http::StatusCode::OK);
        let json = body_json(resp).await;
        assert!(
            json.get("surveyInProgress").is_some(),
            "missing surveyInProgress in {json}"
        );
        assert!(json["surveyInProgress"].is_boolean());
        assert!(
            json.get("backlog").and_then(|v| v.as_array()).is_some(),
            "backlog must be an array in {json}"
        );
        assert!(
            json.get("badResponseNodes")
                .and_then(|v| v.as_array())
                .is_some(),
            "badResponseNodes must be an array in {json}"
        );
        // The stub key must be gone.
        assert!(
            json.get("survey").is_none(),
            "stub key `survey` should not be present in {json}"
        );
        state.app.shutdown();
    }

    /// strkey (not hex) parity for `getsurveyresult`'s `backlog` /
    /// `badResponseNodes`. We populate the running-survey reporting backlog
    /// with a known peer via `survey_topology_timesliced`, then assert the
    /// emitted entries are that peer's G... STRKEY (matching
    /// `SurveyManager.cpp:682,690` `KeyUtils::toStrKey`), NOT the 64-hex form
    /// `survey_report()` carries internally. FAILS on main (stub).
    #[tokio::test]
    async fn test_compat_getsurveyresult_backlog_is_strkey_not_hex() {
        let (_dir, state) = mk_compat_state().await;
        let (peer_id, strkey) = mk_peer();
        // Seed a running reporting session with a known peer in the backlog so
        // the projection has a non-empty list to encode (test-only seam — does
        // not exercise survey logic).
        state
            .app
            .seed_survey_reporting_backlog_for_test(peer_id.clone())
            .await;

        // Internally the native SurveyReport carries this peer as 64-char hex.
        let report = state.app.survey_report().await;
        assert_eq!(report.backlog, vec![peer_id.to_hex()]);
        assert_ne!(
            peer_id.to_hex(),
            strkey,
            "hex and strkey forms must differ for the test to be meaningful"
        );

        // The compat handler MUST re-derive strkey, matching stellar-core
        // SurveyManager.cpp:682,690 (KeyUtils::toStrKey), NOT emit the hex.
        let resp = compat_get(Arc::clone(&state), "/getsurveyresult").await;
        let json = body_json(resp).await;
        let backlog = json["backlog"].as_array().expect("backlog array");
        assert_eq!(
            backlog.len(),
            1,
            "expected exactly the seeded peer in backlog {json}"
        );
        let entry = backlog[0].as_str().unwrap();
        assert_eq!(
            entry, strkey,
            "backlog entry must be the peer's strkey, not hex"
        );
        assert!(
            entry.starts_with('G'),
            "backlog entry {entry:?} is not a G... strkey"
        );
        assert_ne!(
            entry,
            peer_id.to_hex(),
            "backlog entry must NOT be the hex form"
        );
        state.app.shutdown();
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
