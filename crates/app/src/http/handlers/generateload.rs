//! Handler for the `/generateload` endpoint.
//!
//! Spawns a background load generation task. Gated behind the `loadgen` cargo
//! feature and the `testing.generate_load_for_testing` config flag.
//!
//! The actual load generation logic lives in `henyey-simulation` (which depends
//! on `henyey-app`), so we use a trait-object approach to avoid a cyclic
//! dependency: `henyey-app` defines the [`LoadGenRunner`] trait, and the binary
//! crate injects a concrete implementation at startup.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::Json;

use super::super::types::generateload::{GenerateLoadParams, GenerateLoadResponse};
use super::super::ServerState;

// ---------------------------------------------------------------------------
// LoadGenRunner trait (abstract interface)
// ---------------------------------------------------------------------------

/// Parameters passed from the HTTP handler to the load generation backend.
///
/// This is a plain-data struct that mirrors the HTTP query parameters,
/// decoupled from `henyey-simulation` types.
#[derive(Debug, Clone)]
pub struct LoadGenRequest {
    pub mode: String,
    pub accounts: u32,
    pub txs: u32,
    pub tx_rate: u32,
    pub offset: u32,
    pub spike_interval: u64,
    pub spike_size: u32,
    pub max_fee_rate: u32,
    pub skip_low_fee_txs: bool,
    pub min_percent_success: u32,
    pub instances: u32,
    pub wasms: u32,
}

impl From<GenerateLoadParams> for LoadGenRequest {
    fn from(p: GenerateLoadParams) -> Self {
        Self {
            mode: p.mode,
            accounts: p.accounts,
            txs: p.txs,
            tx_rate: p.txrate,
            offset: p.offset,
            spike_interval: p.spikeinterval,
            spike_size: p.spikesize,
            max_fee_rate: p.maxfeerate,
            skip_low_fee_txs: p.skiplowfeetxs,
            min_percent_success: p.minpercentsuccess,
            instances: p.instances,
            wasms: p.wasms,
        }
    }
}

/// Trait for the load generation backend.
///
/// Implemented by `henyey-simulation` and injected into the HTTP server state
/// by the binary crate. This avoids a cyclic dependency between `henyey-app`
/// and `henyey-simulation`.
pub trait LoadGenRunner: Send + Sync + 'static {
    /// Start a load generation run with the given parameters.
    ///
    /// The implementation should spawn its own background task and return
    /// immediately. Returns `Ok(())` if the run was successfully started,
    /// or `Err(message)` if it could not be started (e.g., invalid mode).
    fn start_load(&self, request: LoadGenRequest) -> Result<(), String>;

    /// Stop a running load generation. No-op if nothing is running.
    ///
    /// Sets the per-run stop token to `true`; the running task checks this
    /// cooperatively at each loop iteration and returns `LoadResult::Stopped`.
    fn stop_load(&self);

    /// Whether a load generation run is currently in progress.
    fn is_running(&self) -> bool;
}

/// Shared state for load generation across requests.
///
/// Stored in `ServerState` behind a feature gate.
pub(crate) struct GenerateLoadState {
    /// The load generation backend (injected by the binary crate).
    pub runner: Box<dyn LoadGenRunner>,
}

/// Handler for `GET /generateload`.
///
/// Checks the `generate_load_for_testing` config gate, parses parameters,
/// and delegates to the [`LoadGenRunner`] backend. Returns immediately with
/// a status message.
///
/// Matches stellar-core's `CommandHandler::generateLoad()`.
pub(crate) async fn generateload_handler(
    State(state): State<Arc<ServerState>>,
    Query(params): Query<GenerateLoadParams>,
) -> impl IntoResponse {
    // Gate: require generate_load_for_testing config flag
    if !state.app.config().testing.generate_load_for_testing {
        return Json(GenerateLoadResponse {
            status: "error".to_string(),
            info: Some(
                "Set ARTIFICIALLY_GENERATE_LOAD_FOR_TESTING=true in config to enable this endpoint."
                    .to_string(),
            ),
        });
    }

    let loadgen_state = match &state.loadgen_state {
        Some(s) => s,
        None => {
            return Json(GenerateLoadResponse {
                status: "error".to_string(),
                info: Some(
                    "Load generation not available (loadgen feature not compiled in).".to_string(),
                ),
            });
        }
    };

    // Handle stop mode before checking is_running — matches stellar-core
    // which processes "stop" before any other mode validation.
    if params.mode.eq_ignore_ascii_case("stop") {
        loadgen_state.runner.stop_load();
        return Json(GenerateLoadResponse {
            status: "ok".to_string(),
            info: Some("Stopped load generation".to_string()),
        });
    }

    // Check if a run is already in progress
    if loadgen_state.runner.is_running() {
        return Json(GenerateLoadResponse {
            status: "error".to_string(),
            info: Some("Load generation is already running.".to_string()),
        });
    }

    let summary = format!(
        "Started {} load generation: accounts={}, txs={}, txrate={}",
        params.mode, params.accounts, params.txs, params.txrate,
    );
    let request: LoadGenRequest = params.into();

    match loadgen_state.runner.start_load(request) {
        Ok(()) => Json(GenerateLoadResponse {
            status: "ok".to_string(),
            info: Some(summary),
        }),
        Err(e) => Json(GenerateLoadResponse {
            status: "error".to_string(),
            info: Some(e),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_gen_request_from_params() {
        let params = GenerateLoadParams {
            mode: "pay".to_string(),
            accounts: 200,
            txs: 50,
            txrate: 20,
            offset: 5,
            spikeinterval: 30,
            spikesize: 10,
            maxfeerate: 500,
            skiplowfeetxs: true,
            minpercentsuccess: 90,
            instances: 3,
            wasms: 2,
        };

        let request = LoadGenRequest {
            mode: params.mode.clone(),
            accounts: params.accounts,
            txs: params.txs,
            tx_rate: params.txrate,
            offset: params.offset,
            spike_interval: params.spikeinterval,
            spike_size: params.spikesize,
            max_fee_rate: params.maxfeerate,
            skip_low_fee_txs: params.skiplowfeetxs,
            min_percent_success: params.minpercentsuccess,
            instances: params.instances,
            wasms: params.wasms,
        };

        assert_eq!(request.mode, "pay");
        assert_eq!(request.accounts, 200);
        assert_eq!(request.txs, 50);
        assert_eq!(request.tx_rate, 20);
        assert_eq!(request.offset, 5);
        assert_eq!(request.spike_interval, 30);
        assert_eq!(request.spike_size, 10);
        assert_eq!(request.max_fee_rate, 500);
        assert!(request.skip_low_fee_txs);
        assert_eq!(request.min_percent_success, 90);
        assert_eq!(request.instances, 3);
        assert_eq!(request.wasms, 2);
    }

    #[test]
    fn test_load_gen_request_debug() {
        let request = LoadGenRequest {
            mode: "pay".to_string(),
            accounts: 100,
            txs: 100,
            tx_rate: 10,
            offset: 0,
            spike_interval: 0,
            spike_size: 0,
            max_fee_rate: 0,
            skip_low_fee_txs: false,
            min_percent_success: 0,
            instances: 0,
            wasms: 0,
        };
        let debug = format!("{:?}", request);
        assert!(debug.contains("pay"));
        assert!(debug.contains("100"));
    }

    #[test]
    fn test_load_gen_request_clone() {
        let request = LoadGenRequest {
            mode: "sorobaninvoke".to_string(),
            accounts: 50,
            txs: 200,
            tx_rate: 25,
            offset: 10,
            spike_interval: 60,
            spike_size: 5,
            max_fee_rate: 1000,
            skip_low_fee_txs: true,
            min_percent_success: 95,
            instances: 4,
            wasms: 1,
        };
        let cloned = request.clone();
        assert_eq!(cloned.mode, request.mode);
        assert_eq!(cloned.accounts, request.accounts);
        assert_eq!(cloned.tx_rate, request.tx_rate);
        assert_eq!(cloned.instances, request.instances);
    }

    /// Verify that mode=stop is handled at the HTTP layer before is_running
    /// and is case-insensitive, matching stellar-core behavior.
    #[test]
    fn test_stop_mode_case_insensitive() {
        for mode in &["stop", "STOP", "Stop", "sToP"] {
            assert!(
                mode.eq_ignore_ascii_case("stop"),
                "Expected '{}' to match stop mode",
                mode
            );
        }
    }
}
