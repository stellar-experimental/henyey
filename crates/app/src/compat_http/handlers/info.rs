//! stellar-core compatible `/info` handler.
//!
//! Wraps the response in `{"info": {...}}` with camelCase field names
//! matching stellar-core's `ApplicationImpl::getJsonInfo()`.

use std::sync::Arc;

use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;
use serde::Serialize;

use crate::app::AppState;
use crate::compat_http::CompatServerState;

/// GET /info
///
/// Returns node info in stellar-core's exact JSON format.
pub(crate) async fn compat_info_handler(
    State(state): State<Arc<CompatServerState>>,
) -> impl IntoResponse {
    let app = &state.app;
    let app_state = app.state().await;

    let ledger = app.ledger_summary();
    let (pending_count, authenticated_count) = app.peer_counts().await;

    // Map henyey AppState to stellar-core state string.
    let state_str = match app_state {
        AppState::Initializing => "Booting",
        AppState::CatchingUp => "Catching up",
        AppState::Synced => "Synced!",
        AppState::Validating => "Synced!",
        AppState::ShuttingDown => "Stopping",
    };

    let info = CompatInfoResponse {
        build: henyey_common::version::build_version_string(env!("CARGO_PKG_VERSION")),
        protocol_version: app.config().network.max_protocol_version,
        state: state_str.to_string(),
        started_on: state.started_on.clone(),
        ledger: CompatLedgerInfo {
            num: ledger.num,
            hash: ledger.hash.to_hex(),
            close_time: ledger.close_time,
            version: ledger.version,
            base_fee: ledger.base_fee,
            base_reserve: ledger.base_reserve,
            max_tx_set_size: ledger.max_tx_set_size,
            // Present only when a Soroban network config exists (protocol ≥ 20),
            // mirroring stellar-core's `hasLastClosedSorobanNetworkConfig()` gate;
            // value is the ledger's max OPERATIONS resource == ledger_max_tx_count.
            max_soroban_tx_set_size: app.soroban_network_info().map(|i| i.ledger_max_tx_count),
            flags: if ledger.flags != 0 {
                Some(ledger.flags)
            } else {
                None
            },
            age: ledger.age,
        },
        peers: CompatPeerInfo {
            pending_count,
            authenticated_count,
        },
        network: app.config().network.passphrase.clone(),
        status: Vec::new(),
        quorum: app
            .quorum_info_for_info()
            .map(|q| serde_json::to_value(q).unwrap_or_default())
            .unwrap_or_else(|| serde_json::json!({})),
        invariant_failures: app
            .ledger_manager()
            .invariant_manager()
            .map(|mgr| serde_json::to_value(mgr.get_json_info()).unwrap_or_default()),
    };

    Json(CompatInfoWrapper { info })
}

/// Top-level wrapper: `{"info": {...}}`
#[derive(Serialize)]
struct CompatInfoWrapper {
    info: CompatInfoResponse,
}

/// stellar-core compatible info response.
///
/// Field names match stellar-core's `getJsonInfo()` output exactly.
#[derive(Serialize)]
struct CompatInfoResponse {
    build: String,
    protocol_version: u32,
    state: String,
    #[serde(rename = "startedOn")]
    started_on: String,
    ledger: CompatLedgerInfo,
    peers: CompatPeerInfo,
    network: String,
    status: Vec<String>,
    /// Quorum info — always present in stellar-core's output.
    /// Empty object `{}` when no quorum data is available.
    quorum: serde_json::Value,
    /// Invariant failure info — only present when InvariantManager is enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    invariant_failures: Option<serde_json::Value>,
}

/// Ledger info with stellar-core's camelCase field names.
#[derive(Serialize)]
struct CompatLedgerInfo {
    num: u32,
    hash: String,
    #[serde(rename = "closeTime")]
    close_time: u64,
    version: u32,
    #[serde(rename = "baseFee")]
    base_fee: u32,
    #[serde(rename = "baseReserve")]
    base_reserve: u32,
    #[serde(rename = "maxTxSetSize")]
    max_tx_set_size: u32,
    /// Max Soroban tx-set size (ledger's max OPERATIONS resource). Present only
    /// when the last-closed ledger has a Soroban network config (protocol ≥ 20),
    /// matching stellar-core `ApplicationImpl::getJsonInfo()` (lines 478-484).
    #[serde(
        rename = "maxSorobanTxSetSize",
        skip_serializing_if = "Option::is_none"
    )]
    max_soroban_tx_set_size: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    flags: Option<u32>,
    age: u64,
}

/// Peer count info (stellar-core uses snake_case here, inconsistently).
#[derive(Serialize)]
struct CompatPeerInfo {
    pending_count: usize,
    authenticated_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::Body;
    use http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::app::App;
    use crate::compat_http::{build_compat_router, CompatServerState};

    /// Build a `CompatServerState` backed by a real (minimal) `App` with a
    /// tempdir database and no default peers. Returns `(TempDir, state)`; the
    /// `TempDir` guard is returned first so it outlives the state (which holds
    /// open DB handles).
    async fn mk_compat_state() -> (tempfile::TempDir, Arc<CompatServerState>) {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("compat-info.db");
        let mut config = crate::config::ConfigBuilder::new()
            .database_path(db_path)
            .build();
        config.overlay.known_peers.clear();
        config.is_compat_config = true;
        let app = App::new(config).await.unwrap();
        let state = Arc::new(CompatServerState {
            app: Arc::new(app),
            started_on: "2024-01-01T00:00:00Z".to_string(),
            prometheus_handle: None,
            #[cfg(feature = "loadgen")]
            loadgen_state: None,
        });
        (dir, state)
    }

    /// Collect a JSON response body and parse it.
    async fn body_json(response: axum::response::Response) -> serde_json::Value {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// When a Soroban network config exists, the compat `/info` ledger object
    /// carries `maxSorobanTxSetSize` equal to the ledger's max tx count
    /// (stellar-core's OPERATIONS resource). This is what supercluster's
    /// `GetSorobanMaxTxSetSize().Value` reads. Fails on main (field absent).
    #[tokio::test]
    async fn test_info_soroban_tx_set_size_present_when_soroban_config() {
        let (_dir, state) = mk_compat_state().await;
        state
            .app
            .ledger_manager()
            .set_soroban_network_info_for_test(henyey_ledger::SorobanNetworkInfo {
                ledger_max_tx_count: 500,
                ..Default::default()
            });
        let router = build_compat_router(state);

        let response = router
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
        assert_eq!(
            json["info"]["ledger"]["maxSorobanTxSetSize"],
            500,
            "maxSorobanTxSetSize must equal ledger_max_tx_count, got: {:?}",
            json["info"]["ledger"].get("maxSorobanTxSetSize")
        );
    }

    /// Pre-Soroban (no network config): the key must be absent, mirroring
    /// stellar-core's conditional emission gated on
    /// `hasLastClosedSorobanNetworkConfig()`.
    #[tokio::test]
    async fn test_info_soroban_tx_set_size_absent_pre_soroban() {
        let (_dir, state) = mk_compat_state().await;
        let router = build_compat_router(state);

        let response = router
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
            json["info"]["ledger"].get("maxSorobanTxSetSize").is_none(),
            "maxSorobanTxSetSize must be absent pre-soroban, got: {:?}",
            json["info"]["ledger"].get("maxSorobanTxSetSize")
        );
    }

    /// Verify that the `/info` response JSON shape matches stellar-core.
    ///
    /// This test constructs a `CompatInfoWrapper` by hand and asserts that the
    /// serialised JSON has exactly the top-level and nested keys that
    /// stellar-core's `getJsonInfo()` emits (field names, casing, nesting).
    #[test]
    fn test_info_response_shape_synced() {
        let wrapper = CompatInfoWrapper {
            info: CompatInfoResponse {
                build: henyey_common::version::build_version_string(env!("CARGO_PKG_VERSION")),
                protocol_version: 25,
                state: "Synced!".into(),
                started_on: "2026-01-15T12:00:00Z".into(),
                ledger: CompatLedgerInfo {
                    num: 12345,
                    hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".into(),
                    close_time: 1700000000,
                    version: 25,
                    base_fee: 100,
                    base_reserve: 100000000,
                    max_tx_set_size: 1000,
                    max_soroban_tx_set_size: None,
                    flags: None,
                    age: 5,
                },
                peers: CompatPeerInfo {
                    pending_count: 3,
                    authenticated_count: 10,
                },
                network: "Test SDF Network ; September 2015".into(),
                status: vec!["Catching up: Applying buckets 50.0%".into()],
                quorum: serde_json::json!({}),
                invariant_failures: None,
            },
        };

        let value = serde_json::to_value(&wrapper).unwrap();

        // Top-level: {"info": {...}}
        assert!(value.is_object(), "top-level must be an object");
        assert!(value.get("info").is_some(), "must have 'info' wrapper key");
        let info = &value["info"];

        // Required top-level fields inside "info"
        let expected_top_keys = [
            "build",
            "protocol_version",
            "state",
            "startedOn",
            "ledger",
            "peers",
            "network",
            "status",
            "quorum",
        ];
        for key in &expected_top_keys {
            assert!(info.get(key).is_some(), "missing top-level key: {key}");
        }

        // Ledger sub-object: camelCase field names
        let ledger = &info["ledger"];
        let expected_ledger_keys = [
            "num",
            "hash",
            "closeTime",
            "version",
            "baseFee",
            "baseReserve",
            "maxTxSetSize",
            "age",
        ];
        for key in &expected_ledger_keys {
            assert!(ledger.get(key).is_some(), "missing ledger key: {key}");
        }

        // flags should be absent when None (skip_serializing_if)
        assert!(
            ledger.get("flags").is_none(),
            "flags should be absent when None"
        );

        // Peers sub-object: snake_case (stellar-core inconsistency)
        let peers = &info["peers"];
        assert!(peers.get("pending_count").is_some());
        assert!(peers.get("authenticated_count").is_some());

        // Status is an array
        assert!(info["status"].is_array(), "status must be an array");

        // startedOn uses camelCase (not started_on)
        assert!(
            info.get("started_on").is_none(),
            "should use startedOn, not started_on"
        );
    }

    /// Verify that the `flags` field appears when set.
    #[test]
    fn test_info_response_flags_present_when_set() {
        let wrapper = CompatInfoWrapper {
            info: CompatInfoResponse {
                build: henyey_common::version::build_version_string(env!("CARGO_PKG_VERSION")),
                protocol_version: 25,
                state: "Booting".into(),
                started_on: "2026-01-15T12:00:00Z".into(),
                ledger: CompatLedgerInfo {
                    num: 0,
                    hash: "0".repeat(64),
                    close_time: 0,
                    version: 0,
                    base_fee: 100,
                    base_reserve: 100000000,
                    max_tx_set_size: 1000,
                    max_soroban_tx_set_size: None,
                    flags: Some(3),
                    age: 0,
                },
                peers: CompatPeerInfo {
                    pending_count: 0,
                    authenticated_count: 0,
                },
                network: "Test SDF Network ; September 2015".into(),
                status: vec![],
                quorum: serde_json::json!({}),
                invariant_failures: None,
            },
        };

        let value = serde_json::to_value(&wrapper).unwrap();
        let ledger = &value["info"]["ledger"];
        assert_eq!(ledger["flags"], 3, "flags must be present when Some");
    }

    /// Verify booting state has empty status array.
    #[test]
    fn test_info_response_booting_empty_status() {
        let wrapper = CompatInfoWrapper {
            info: CompatInfoResponse {
                build: henyey_common::version::build_version_string(env!("CARGO_PKG_VERSION")),
                protocol_version: 25,
                state: "Booting".into(),
                started_on: "2026-01-15T12:00:00Z".into(),
                ledger: CompatLedgerInfo {
                    num: 0,
                    hash: "0".repeat(64),
                    close_time: 0,
                    version: 0,
                    base_fee: 100,
                    base_reserve: 100000000,
                    max_tx_set_size: 1000,
                    max_soroban_tx_set_size: None,
                    flags: None,
                    age: 0,
                },
                peers: CompatPeerInfo {
                    pending_count: 0,
                    authenticated_count: 0,
                },
                network: "Test SDF Network ; September 2015".into(),
                status: vec![],
                quorum: serde_json::json!({}),
                invariant_failures: None,
            },
        };

        let value = serde_json::to_value(&wrapper).unwrap();
        let status = value["info"]["status"].as_array().unwrap();
        assert!(
            status.is_empty(),
            "booting state should have empty status array"
        );
    }

    /// Cross-check: serialize and deserialize as generic JSON to ensure
    /// roundtrip integrity and that no unexpected keys leak.
    #[test]
    fn test_info_response_no_unexpected_keys() {
        let wrapper = CompatInfoWrapper {
            info: CompatInfoResponse {
                build: henyey_common::version::build_version_string(env!("CARGO_PKG_VERSION")),
                protocol_version: 25,
                state: "Synced!".into(),
                started_on: "2026-01-15T12:00:00Z".into(),
                ledger: CompatLedgerInfo {
                    num: 1,
                    hash: "a".repeat(64),
                    close_time: 100,
                    version: 25,
                    base_fee: 100,
                    base_reserve: 100000000,
                    max_tx_set_size: 1000,
                    max_soroban_tx_set_size: None,
                    flags: None,
                    age: 0,
                },
                peers: CompatPeerInfo {
                    pending_count: 0,
                    authenticated_count: 0,
                },
                network: "Test SDF Network ; September 2015".into(),
                status: vec![],
                quorum: serde_json::json!({}),
                invariant_failures: None,
            },
        };

        let value = serde_json::to_value(&wrapper).unwrap();
        let top = value.as_object().unwrap();

        // Only "info" at the top level
        assert_eq!(top.len(), 1, "top-level should only have 'info'");

        let info = top["info"].as_object().unwrap();
        let allowed_info_keys: std::collections::HashSet<&str> = [
            "build",
            "protocol_version",
            "state",
            "startedOn",
            "ledger",
            "peers",
            "network",
            "status",
            "quorum",
        ]
        .into_iter()
        .collect();
        for key in info.keys() {
            assert!(
                allowed_info_keys.contains(key.as_str()),
                "unexpected info key: {key}"
            );
        }

        let ledger = info["ledger"].as_object().unwrap();
        let allowed_ledger_keys: std::collections::HashSet<&str> = [
            "num",
            "hash",
            "closeTime",
            "version",
            "baseFee",
            "baseReserve",
            "maxTxSetSize",
            "flags",
            "age",
        ]
        .into_iter()
        .collect();
        for key in ledger.keys() {
            assert!(
                allowed_ledger_keys.contains(key.as_str()),
                "unexpected ledger key: {key}"
            );
        }
    }

    /// Verify that `quorum` is always present in compat response (empty object when no data).
    #[test]
    fn test_info_response_quorum_always_present() {
        let wrapper = CompatInfoWrapper {
            info: CompatInfoResponse {
                build: henyey_common::version::build_version_string(env!("CARGO_PKG_VERSION")),
                protocol_version: 25,
                state: "Booting".into(),
                started_on: "2026-01-15T12:00:00Z".into(),
                ledger: CompatLedgerInfo {
                    num: 0,
                    hash: "0".repeat(64),
                    close_time: 0,
                    version: 0,
                    base_fee: 100,
                    base_reserve: 100000000,
                    max_tx_set_size: 1000,
                    max_soroban_tx_set_size: None,
                    flags: None,
                    age: 0,
                },
                peers: CompatPeerInfo {
                    pending_count: 0,
                    authenticated_count: 0,
                },
                network: "Test SDF Network ; September 2015".into(),
                status: vec![],
                quorum: serde_json::json!({}),
                invariant_failures: None,
            },
        };

        let value = serde_json::to_value(&wrapper).unwrap();
        let quorum = &value["info"]["quorum"];
        assert!(quorum.is_object(), "quorum must always be an object");
        assert!(
            quorum.as_object().unwrap().is_empty(),
            "quorum should be empty object when no data"
        );
    }

    /// Verify that populated quorum data serializes correctly with `validated`
    /// nested inside `qset`.
    #[test]
    fn test_info_response_quorum_populated() {
        use henyey_herder::json_api::{InfoQuorumSetSnapshot, InfoQuorumSnapshot};

        let snapshot = InfoQuorumSnapshot {
            node: "GABCD".to_string(),
            qset: InfoQuorumSetSnapshot {
                phase: "PREPARE".to_string(),
                hash: Some("abcdef".to_string()),
                fail_at: Some(2),
                validated: Some(true),
                agree: 3,
                disagree: 0,
                missing: 1,
                delayed: 0,
                ledger: 42,
                lag_ms: None,
            },
            transitive: None,
        };

        let wrapper = CompatInfoWrapper {
            info: CompatInfoResponse {
                build: henyey_common::version::build_version_string(env!("CARGO_PKG_VERSION")),
                protocol_version: 25,
                state: "Synced!".into(),
                started_on: "2026-01-15T12:00:00Z".into(),
                ledger: CompatLedgerInfo {
                    num: 42,
                    hash: "a".repeat(64),
                    close_time: 100,
                    version: 25,
                    base_fee: 100,
                    base_reserve: 100000000,
                    max_tx_set_size: 1000,
                    max_soroban_tx_set_size: None,
                    flags: None,
                    age: 0,
                },
                peers: CompatPeerInfo {
                    pending_count: 0,
                    authenticated_count: 5,
                },
                network: "Test SDF Network ; September 2015".into(),
                status: vec![],
                quorum: serde_json::to_value(&snapshot).unwrap(),
                invariant_failures: None,
            },
        };

        let value = serde_json::to_value(&wrapper).unwrap();
        let quorum = &value["info"]["quorum"];
        assert_eq!(quorum["node"], "GABCD");
        assert_eq!(quorum["qset"]["phase"], "PREPARE");
        assert_eq!(quorum["qset"]["hash"], "abcdef");
        assert_eq!(quorum["qset"]["fail_at"], 2);
        assert_eq!(quorum["qset"]["agree"], 3);
        assert_eq!(quorum["qset"]["ledger"], 42);
        // validated must be inside qset
        assert!(
            quorum.get("validated").is_none(),
            "validated must not be at quorum top level"
        );
        assert_eq!(quorum["qset"]["validated"], true);
    }
}
