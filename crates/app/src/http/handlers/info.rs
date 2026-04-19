//! Handlers for /info, /status, /health, /ledger, /upgrades, /self-check,
//! /quorum, and /dumpproposedsettings.

use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
};

use crate::app::AppState;
use crate::run_cmd::NodeStatus;

use super::super::helpers::{map_upgrade_item, node_id_to_strkey};
use super::super::types::{
    DumpProposedSettingsParams, HealthResponse, InfoLedgerSummary, InfoPeerSummary, InfoResponse,
    LedgerResponse, QuorumResponse, QuorumSetResponse, RootResponse, SelfCheckResponse,
    UpgradeState, UpgradesResponse,
};
use super::super::ServerState;

pub(crate) async fn root_handler() -> Json<RootResponse> {
    Json(RootResponse {
        name: "henyey".to_string(),
        version: henyey_common::version::build_version_string(env!("CARGO_PKG_VERSION")),
        endpoints: vec![
            "/info".to_string(),
            "/status".to_string(),
            "/metrics".to_string(),
            "/peers".to_string(),
            "/connect".to_string(),
            "/droppeer".to_string(),
            "/bans".to_string(),
            "/unban".to_string(),
            "/ledger".to_string(),
            "/upgrades".to_string(),
            "/self-check".to_string(),
            "/quorum".to_string(),
            "/survey".to_string(),
            "/scp".to_string(),
            "/survey/start".to_string(),
            "/survey/stop".to_string(),
            "/survey/topology".to_string(),
            "/survey/reporting/stop".to_string(),
            "/tx".to_string(),
            "/shutdown".to_string(),
            "/health".to_string(),
            "/ll".to_string(),
            "/manualclose".to_string(),
            "/sorobaninfo".to_string(),
            "/clearmetrics".to_string(),
            "/logrotate".to_string(),
            "/maintenance".to_string(),
            "/dumpproposedsettings".to_string(),
        ],
    })
}

pub(crate) async fn info_handler(State(state): State<Arc<ServerState>>) -> Json<InfoResponse> {
    let info = state.app.info();
    let app_state = state.app.state().await;
    let uptime = state.start_time.elapsed().as_secs();
    let ledger = state.app.ledger_summary();
    let (pending_count, authenticated_count) = state.app.peer_counts().await;

    Json(InfoResponse {
        build: henyey_common::version::build_version_string(&info.version),
        protocol_version: ledger.version,
        state: format!("{}", app_state),
        started_on: state.started_on.clone(),
        uptime_secs: uptime,
        node_name: info.node_name,
        public_key: info.public_key,
        network_passphrase: info.network_passphrase,
        is_validator: info.is_validator,
        ledger: InfoLedgerSummary {
            num: ledger.num,
            hash: ledger.hash.to_hex(),
            close_time: ledger.close_time,
            version: ledger.version,
            base_fee: ledger.base_fee,
            base_reserve: ledger.base_reserve,
            max_tx_set_size: ledger.max_tx_set_size,
            flags: ledger.flags,
            age: ledger.age,
        },
        peers: InfoPeerSummary {
            pending_count,
            authenticated_count,
        },
    })
}

pub(crate) async fn status_handler(State(state): State<Arc<ServerState>>) -> Json<NodeStatus> {
    let info = state.app.ledger_info();
    let stats = state.app.herder_stats();
    let peer_count = state.app.peer_snapshots().await.len();
    Json(NodeStatus {
        ledger_seq: info.ledger_seq,
        ledger_hash: Some(info.hash.to_hex()),
        peer_count,
        consensus_state: stats.state.to_string(),
        pending_tx_count: stats.pending_transactions,
        uptime_secs: state.start_time.elapsed().as_secs(),
        state: format!("{}", state.app.state().await),
    })
}

pub(crate) async fn health_handler(
    State(state): State<Arc<ServerState>>,
) -> (StatusCode, Json<HealthResponse>) {
    let app_state = state.app.state().await;
    let ledger_seq = state.app.ledger_info().ledger_seq;
    let peer_count = state.app.peer_snapshots().await.len();

    // Issue #1822: the AppState gate alone cannot detect the post-
    // catchup livelock — the node keeps reporting Validating while
    // ledgers stop advancing. Consult `consensus_stuck_state` for a
    // direct signal.
    let stall_elapsed = state
        .app
        .consensus_stuck_state_read()
        .await
        .map(|s| s.stuck_start.elapsed().as_secs());

    let state_healthy = matches!(app_state, AppState::Synced | AppState::Validating);
    let stalled = stall_elapsed
        .map(|e| e >= crate::app::HEALTH_STALL_SECS)
        .unwrap_or(false);
    let is_healthy = state_healthy && !stalled;

    let reason = if !state_healthy {
        Some("not_synced".to_string())
    } else if stalled {
        Some("post_catchup_stalled".to_string())
    } else {
        None
    };

    let response = HealthResponse {
        status: if is_healthy { "healthy" } else { "unhealthy" }.to_string(),
        reason,
        state: format!("{}", app_state),
        ledger_seq,
        peer_count,
    };

    let status = if is_healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (status, Json(response))
}

pub(crate) async fn ledger_handler(State(state): State<Arc<ServerState>>) -> Json<LedgerResponse> {
    let info = state.app.ledger_info();
    Json(LedgerResponse {
        sequence: info.ledger_seq,
        hash: info.hash.to_hex(),
        close_time: info.close_time,
        protocol_version: info.protocol_version,
    })
}

pub(crate) async fn upgrades_handler(
    State(state): State<Arc<ServerState>>,
) -> Json<UpgradesResponse> {
    let (protocol_version, base_fee, base_reserve, max_tx_set_size) =
        state.app.current_upgrade_state();
    let proposed = state
        .app
        .proposed_upgrades()
        .into_iter()
        .filter_map(map_upgrade_item)
        .collect::<Vec<_>>();

    Json(UpgradesResponse {
        current: UpgradeState {
            protocol_version,
            base_fee,
            base_reserve,
            max_tx_set_size,
        },
        proposed,
    })
}

pub(crate) async fn self_check_handler(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    match state.app.self_check(32) {
        Ok(result) => (
            StatusCode::OK,
            Json(SelfCheckResponse {
                ok: result.ok,
                checked_ledgers: result.checked_ledgers,
                last_checked_ledger: result.last_checked_ledger,
                message: None,
            }),
        ),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(SelfCheckResponse {
                ok: false,
                checked_ledgers: 0,
                last_checked_ledger: None,
                message: Some(err.to_string()),
            }),
        ),
    }
}

pub(crate) async fn quorum_handler(State(state): State<Arc<ServerState>>) -> Json<QuorumResponse> {
    let local = state
        .app
        .local_quorum_set()
        .map(|qs| quorum_set_response(&qs));
    Json(QuorumResponse { local })
}

pub(crate) fn quorum_set_response(
    quorum_set: &stellar_xdr::curr::ScpQuorumSet,
) -> QuorumSetResponse {
    use henyey_scp::hash_quorum_set;

    let hash = hash_quorum_set(quorum_set).to_hex();
    let validators = quorum_set
        .validators
        .iter()
        .filter_map(node_id_to_strkey)
        .collect::<Vec<_>>();
    let inner_sets = quorum_set
        .inner_sets
        .iter()
        .map(quorum_set_response)
        .collect::<Vec<_>>();
    QuorumSetResponse {
        hash,
        threshold: quorum_set.threshold,
        validators,
        inner_sets,
    }
}

pub(crate) async fn dumpproposedsettings_handler(
    State(state): State<Arc<ServerState>>,
    Query(params): Query<DumpProposedSettingsParams>,
) -> impl IntoResponse {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use stellar_xdr::curr::{ConfigUpgradeSetKey, Limits, ReadXdr};

    let Some(blob) = params.blob else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "Must specify a ConfigUpgradeSetKey blob: dumpproposedsettings?blob=<ConfigUpgradeSetKey in xdr format>"
            })),
        );
    };

    let bytes = match STANDARD.decode(&blob) {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": format!("Invalid base64: {}", e)
                })),
            );
        }
    };

    let key: ConfigUpgradeSetKey = match ConfigUpgradeSetKey::from_xdr(&bytes, Limits::none()) {
        Ok(k) => k,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": format!("Invalid XDR: {}", e)
                })),
            );
        }
    };

    match state.app.get_config_upgrade_set(&key) {
        Some(settings) => (StatusCode::OK, Json(serde_json::json!(settings))),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "configUpgradeSet is missing or invalid"
            })),
        ),
    }
}

#[cfg(test)]
mod tests {
    //! Issue #1822 coverage for the `/health` handler: the endpoint must
    //! report `unhealthy` + `reason = "post_catchup_stalled"` when
    //! `consensus_stuck_state.stuck_start` is older than
    //! `HEALTH_STALL_SECS`, and must omit the `reason` field from the
    //! serialized JSON when the node is healthy (backward compat).

    use super::*;
    // ConsensusStuckState is re-exported from the app module root for
    // use by cross-module tests; see `crate::app::types`.
    use crate::app::ConsensusStuckState;
    use crate::app::{App, AppState};
    use crate::config::ConfigBuilder;
    use crate::http::types::HealthResponse;
    use std::sync::Arc;
    use std::time::Instant;

    async fn make_app() -> Arc<App> {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("rs-stellar-test.db");
        let config = ConfigBuilder::new().database_path(db_path).build();
        // Leak the tempdir for the duration of the test (App holds file
        // locks referencing it).
        std::mem::forget(dir);
        Arc::new(App::new(config).await.unwrap())
    }

    fn server_state(app: Arc<App>) -> Arc<ServerState> {
        Arc::new(ServerState {
            app,
            start_time: Instant::now(),
            started_on: "test".to_string(),
            log_handle: None,
            #[cfg(feature = "loadgen")]
            loadgen_state: None,
        })
    }

    /// Call the handler directly and unpack the `(StatusCode, Json<_>)` tuple.
    async fn call_health(state: Arc<ServerState>) -> (u16, HealthResponse) {
        let (status, Json(body)) = health_handler(axum::extract::State(state)).await;
        (status.as_u16(), body)
    }

    #[tokio::test]
    async fn test_health_healthy_when_synced_and_no_stall() {
        let app = make_app().await;
        app.set_state(AppState::Synced).await;
        let state = server_state(app);
        let (code, body) = call_health(state).await;
        assert_eq!(code, 200);
        assert_eq!(body.status, "healthy");
        assert!(body.reason.is_none(), "reason must be omitted when healthy");
    }

    #[tokio::test]
    async fn test_health_unhealthy_not_synced_when_catching_up() {
        let app = make_app().await;
        app.set_state(AppState::CatchingUp).await;
        // Even with a stale stuck state, CatchingUp should report
        // "not_synced" (the AppState gate wins).
        let now = app.clock_for_test().now();
        app.seed_consensus_stuck_state_for_test(Some(ConsensusStuckState {
            current_ledger: 42,
            first_buffered: 44,
            stuck_start: now - std::time::Duration::from_secs(crate::app::HEALTH_STALL_SECS + 10),
            last_recovery_attempt: now,
            recovery_attempts: 5,
            catchup_triggered: false,
        }))
        .await;
        let state = server_state(app);
        let (code, body) = call_health(state).await;
        assert_eq!(code, 503);
        assert_eq!(body.reason.as_deref(), Some("not_synced"));
    }

    #[tokio::test]
    async fn test_health_unhealthy_post_catchup_stalled() {
        let app = make_app().await;
        app.set_state(AppState::Validating).await;
        let now = app.clock_for_test().now();
        app.seed_consensus_stuck_state_for_test(Some(ConsensusStuckState {
            current_ledger: 62187968,
            first_buffered: 62187971,
            stuck_start: now - std::time::Duration::from_secs(crate::app::HEALTH_STALL_SECS + 10),
            last_recovery_attempt: now,
            recovery_attempts: 12,
            catchup_triggered: false,
        }))
        .await;
        let state = server_state(app);
        let (code, body) = call_health(state).await;
        assert_eq!(code, 503);
        assert_eq!(body.reason.as_deref(), Some("post_catchup_stalled"));
    }

    #[tokio::test]
    async fn test_health_healthy_when_stall_below_threshold() {
        let app = make_app().await;
        app.set_state(AppState::Validating).await;
        let now = app.clock_for_test().now();
        // Stall just started — under the threshold.
        app.seed_consensus_stuck_state_for_test(Some(ConsensusStuckState {
            current_ledger: 10,
            first_buffered: 12,
            stuck_start: now,
            last_recovery_attempt: now,
            recovery_attempts: 1,
            catchup_triggered: false,
        }))
        .await;
        let state = server_state(app);
        let (code, body) = call_health(state).await;
        assert_eq!(code, 200);
        assert_eq!(body.status, "healthy");
        assert!(body.reason.is_none());
    }

    #[test]
    fn test_health_response_reason_omitted_when_healthy() {
        // Serialize a `HealthResponse { reason: None, .. }` and verify
        // the JSON does not contain the `reason` key (backward compat
        // with pre-1822 consumers).
        let resp = HealthResponse {
            status: "healthy".to_string(),
            reason: None,
            state: "Synced".to_string(),
            ledger_seq: 100,
            peer_count: 5,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert!(
            !json.as_object().unwrap().contains_key("reason"),
            "reason must be omitted from JSON when healthy"
        );
        // Four keys: status, state, ledger_seq, peer_count.
        assert_eq!(json.as_object().unwrap().len(), 4);
    }

    #[test]
    fn test_health_response_reason_present_when_unhealthy() {
        let resp = HealthResponse {
            status: "unhealthy".to_string(),
            reason: Some("post_catchup_stalled".to_string()),
            state: "Validating".to_string(),
            ledger_seq: 100,
            peer_count: 5,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(
            json.get("reason").and_then(|v| v.as_str()),
            Some("post_catchup_stalled")
        );
    }
}
