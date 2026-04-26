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
