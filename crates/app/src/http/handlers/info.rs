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
    let quorum = state.app.quorum_info_for_info();

    Json(InfoResponse {
        build: henyey_common::version::build_version_string(&info.version),
        commit_hash: info.commit_hash,
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
        quorum,
    })
}

pub(crate) async fn status_handler(State(state): State<Arc<ServerState>>) -> Json<NodeStatus> {
    let info = state.app.ledger_info();
    let stats = state.app.herder_stats();
    let peer_count = state.app.peer_count().await;
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

pub(crate) async fn health_handler(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    let app_state = state.app.state().await;
    let ledger_seq = state.app.ledger_info().ledger_seq;
    let peer_count = state.app.peer_count().await;

    // Inline stall check: read consensus_stuck_state under lock.
    let stall_elapsed = {
        let guard = state.app.consensus_stuck_state.read().await;
        guard.as_ref().map(|s| s.stuck_start.elapsed().as_secs())
    };

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
    let app = Arc::clone(&state.app);
    match henyey_common::spawn_blocking_logged("self-check", move || app.self_check(32)).await {
        Ok(Ok(result)) => (
            StatusCode::OK,
            Json(SelfCheckResponse {
                ok: result.ok,
                checked_ledgers: result.checked_ledgers,
                last_checked_ledger: result.last_checked_ledger,
                message: None,
            }),
        ),
        Ok(Err(err)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(SelfCheckResponse {
                ok: false,
                checked_ledgers: 0,
                last_checked_ledger: None,
                message: Some(err.to_string()),
            }),
        ),
        Err(join_err) => std::panic::resume_unwind(join_err.into_panic()),
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
    use std::sync::Arc;
    use std::time::Instant;

    use axum::body::Body;
    use http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::app::App;
    use crate::config::{BuildMetadata, ConfigBuilder};
    use crate::http::{build_router, ServerState};

    /// Build a `ServerState` backed by a real (minimal) `App` with the given
    /// `commit_hash`. Returns the state and the `TempDir` whose lifetime keeps
    /// the on-disk database valid.
    async fn test_server_state(commit_hash: &str) -> (Arc<ServerState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("test.db");
        let mut config = ConfigBuilder::new().database_path(db_path).build();
        config.build = if commit_hash.is_empty() {
            BuildMetadata::default()
        } else {
            BuildMetadata::new(commit_hash, "")
        };

        let app = App::new(config).await.unwrap();
        let state = Arc::new(ServerState {
            app: Arc::new(app),
            start_time: Instant::now(),
            started_on: "2024-01-01T00:00:00Z".to_string(),
            log_handle: None,
            prometheus_handle: None,
            #[cfg(feature = "loadgen")]
            loadgen_state: None,
        });
        (state, dir)
    }

    /// Collect an axum response body and parse it as JSON.
    async fn body_json(response: axum::response::Response) -> serde_json::Value {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn test_info_handler_commit_hash_present() {
        let commit = "abc123def456";
        let (state, _dir) = test_server_state(commit).await;
        let app = build_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/info")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), http::StatusCode::OK);
        let json = body_json(response).await;
        assert_eq!(json["commit_hash"], commit);
    }

    #[tokio::test]
    async fn test_info_handler_commit_hash_absent() {
        let (state, _dir) = test_server_state("").await;
        let app = build_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/info")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), http::StatusCode::OK);
        let json = body_json(response).await;
        assert!(
            json.get("commit_hash").is_none(),
            "commit_hash field should be absent when empty, got: {:?}",
            json.get("commit_hash")
        );
    }
}
