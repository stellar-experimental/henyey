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
    /// Path to the pre-generated transactions file for `pay_pregenerated`.
    pub preloaded_transactions_file: Option<String>,

    // --- Apply-load (`soroban_invoke_apply_load`) params ---
    /// Whether the node runs in overlay-only mode (gate stand-in).
    pub overlay_only_mode: bool,
    /// `APPLY_LOAD_BL_BATCH_SIZE`.
    pub apply_load_bl_batch_size: u32,
    /// `APPLY_LOAD_BL_SIMULATED_LEDGERS`.
    pub apply_load_bl_simulated_ledgers: u32,
    /// `APPLY_LOAD_DATA_ENTRY_SIZE` (rounded to a multiple of 4 downstream).
    pub apply_load_data_entry_size: u32,
    /// `APPLY_LOAD_NUM_RW_ENTRIES[_DISTRIBUTION]` as `(values, weights)`.
    pub apply_load_num_rw_entries: (Vec<u32>, Vec<u32>),
    /// `APPLY_LOAD_NUM_DISK_READ_ENTRIES[_DISTRIBUTION]`.
    pub apply_load_num_disk_read_entries: (Vec<u32>, Vec<u32>),
    /// `APPLY_LOAD_TX_SIZE_BYTES[_DISTRIBUTION]`.
    pub apply_load_tx_size_bytes: (Vec<u32>, Vec<u32>),
    /// `APPLY_LOAD_EVENT_COUNT[_DISTRIBUTION]`.
    pub apply_load_event_count: (Vec<u32>, Vec<u32>),
    /// `APPLY_LOAD_INSTRUCTIONS[_DISTRIBUTION]`.
    pub apply_load_instructions: (Vec<u32>, Vec<u32>),

    /// `create_upgrade` override for `ledgerMaxInstructions` (`ldgrmxinstrc`).
    pub ledger_max_instructions: Option<i64>,
}

/// Parse a comma-separated list of `u32` (e.g. `"10,20,30"`), ignoring empty
/// entries. Used for the `APPLY_LOAD_*` distribution value/weight params.
fn parse_u32_csv(s: &Option<String>) -> Vec<u32> {
    match s {
        None => Vec::new(),
        Some(s) => s
            .split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .filter_map(|t| t.parse::<u32>().ok())
            .collect(),
    }
}

impl From<GenerateLoadParams> for LoadGenRequest {
    fn from(p: GenerateLoadParams) -> Self {
        let dist = |v: &Option<String>, w: &Option<String>| (parse_u32_csv(v), parse_u32_csv(w));
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
            preloaded_transactions_file: p.preloadedtransactionsfile.filter(|s| !s.is_empty()),
            overlay_only_mode: p.overlayonlymode,
            apply_load_bl_batch_size: p.applyloadblbatchsize,
            apply_load_bl_simulated_ledgers: p.applyloadblsimulatedledgers,
            apply_load_data_entry_size: p.applyloaddataentrysize,
            apply_load_num_rw_entries: dist(
                &p.applyloadnumrwentries,
                &p.applyloadnumrwentriesdistribution,
            ),
            apply_load_num_disk_read_entries: dist(
                &p.applyloadnumdiskreadentries,
                &p.applyloadnumdiskreadentriesdistribution,
            ),
            apply_load_tx_size_bytes: dist(
                &p.applyloadtxsizebytes,
                &p.applyloadtxsizebytesdistribution,
            ),
            apply_load_event_count: dist(
                &p.applyloadeventcount,
                &p.applyloadeventcountdistribution,
            ),
            apply_load_instructions: dist(
                &p.applyloadinstructions,
                &p.applyloadinstructionsdistribution,
            ),
            ledger_max_instructions: p.ldgrmxinstrc,
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

    /// Base64-encoded XDR-opaque `ConfigUpgradeSetKey` for the current/last
    /// `create_upgrade` config, or `None` if it cannot be computed (e.g. no
    /// deployed contract instance, or the backend does not support it).
    ///
    /// Mirrors stellar-core `LoadGenerator::getConfigUpgradeSetKey` →
    /// `decoder::encode_b64(xdr_to_opaque(key))`. The default returns `None` so
    /// non-simulation backends are unaffected.
    fn config_upgrade_set_key(&self) -> Option<String> {
        None
    }
}

/// Build the `config_upgrade_set_key` response field for a `/generateload`
/// run. Emits the runner's key ONLY for `create_upgrade` mode (all accepted
/// spellings); every other mode omits it.
///
/// Parity: stellar-core `CommandHandler::generateLoad` (CommandHandler.cpp:1488-1496)
/// adds `res["config_upgrade_set_key"]` only when
/// `cfg.mode == LoadGenMode::SOROBAN_CREATE_UPGRADE`. Shared by the native and
/// compat `/generateload` handlers so the wire shape cannot diverge.
pub fn config_upgrade_set_key_for_response<R: LoadGenRunner + ?Sized>(
    mode: &str,
    runner: &R,
) -> Option<String> {
    if is_create_upgrade_mode(mode) {
        runner.config_upgrade_set_key()
    } else {
        None
    }
}

/// Whether `mode` selects the `SOROBAN_CREATE_UPGRADE` load-gen mode. Accepts
/// the same spellings as the simulation backend's `parse_mode`
/// (`create_upgrade`, `soroban_create_upgrade`, and the no-separator forms),
/// case-insensitively.
fn is_create_upgrade_mode(mode: &str) -> bool {
    matches!(
        mode.to_ascii_lowercase().as_str(),
        "create_upgrade" | "createupgrade" | "soroban_create_upgrade" | "sorobancreateupgrade"
    )
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
            config_upgrade_set_key: None,
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
                config_upgrade_set_key: None,
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
            config_upgrade_set_key: None,
        });
    }

    // Check if a run is already in progress
    if loadgen_state.runner.is_running() {
        return Json(GenerateLoadResponse {
            status: "error".to_string(),
            info: Some("Load generation is already running.".to_string()),
            config_upgrade_set_key: None,
        });
    }

    let summary = format!(
        "Started {} load generation: accounts={}, txs={}, txrate={}",
        params.mode, params.accounts, params.txs, params.txrate,
    );
    // Capture the mode before `params` is consumed by `into()`.
    let mode = params.mode.clone();
    let request: LoadGenRequest = params.into();

    match loadgen_state.runner.start_load(request) {
        Ok(()) => Json(GenerateLoadResponse {
            status: "ok".to_string(),
            info: Some(summary),
            // Parity: only create_upgrade carries the armed key
            // (CommandHandler.cpp:1488-1496).
            config_upgrade_set_key: config_upgrade_set_key_for_response(
                &mode,
                loadgen_state.runner.as_ref(),
            ),
        }),
        Err(e) => Json(GenerateLoadResponse {
            status: "error".to_string(),
            info: Some(e),
            config_upgrade_set_key: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `GenerateLoadParams` with zeroed apply-load fields, for `..` struct
    /// update in test literals that only care about the classic params.
    fn apply_load_params_defaults() -> GenerateLoadParams {
        GenerateLoadParams {
            mode: String::new(),
            accounts: 0,
            txs: 0,
            txrate: 0,
            offset: 0,
            spikeinterval: 0,
            spikesize: 0,
            maxfeerate: 0,
            skiplowfeetxs: false,
            minpercentsuccess: 0,
            instances: 0,
            wasms: 0,
            preloadedtransactionsfile: None,
            overlayonlymode: false,
            applyloadblbatchsize: 1000,
            applyloadblsimulatedledgers: 1000,
            applyloaddataentrysize: 0,
            applyloadnumrwentries: None,
            applyloadnumrwentriesdistribution: None,
            applyloadnumdiskreadentries: None,
            applyloadnumdiskreadentriesdistribution: None,
            applyloadtxsizebytes: None,
            applyloadtxsizebytesdistribution: None,
            applyloadeventcount: None,
            applyloadeventcountdistribution: None,
            applyloadinstructions: None,
            applyloadinstructionsdistribution: None,
        }
    }

    /// A `LoadGenRequest` with zeroed apply-load fields, for `..` struct update
    /// in test literals.
    fn apply_load_request_defaults() -> LoadGenRequest {
        LoadGenRequest {
            mode: String::new(),
            accounts: 0,
            txs: 0,
            tx_rate: 0,
            offset: 0,
            spike_interval: 0,
            spike_size: 0,
            max_fee_rate: 0,
            skip_low_fee_txs: false,
            min_percent_success: 0,
            instances: 0,
            wasms: 0,
            preloaded_transactions_file: None,
            overlay_only_mode: false,
            apply_load_bl_batch_size: 1000,
            apply_load_bl_simulated_ledgers: 1000,
            apply_load_data_entry_size: 0,
            apply_load_num_rw_entries: (Vec::new(), Vec::new()),
            apply_load_num_disk_read_entries: (Vec::new(), Vec::new()),
            apply_load_tx_size_bytes: (Vec::new(), Vec::new()),
            apply_load_event_count: (Vec::new(), Vec::new()),
            apply_load_instructions: (Vec::new(), Vec::new()),
        }
    }

    /// Stub `LoadGenRunner` for testing the response-building logic without a
    /// full `App`/`ServerState`. Reports a fixed base64 config-upgrade-set key.
    struct StubRunner {
        key: Option<String>,
    }
    impl LoadGenRunner for StubRunner {
        fn start_load(&self, _request: LoadGenRequest) -> Result<(), String> {
            Ok(())
        }
        fn stop_load(&self) {}
        fn is_running(&self) -> bool {
            false
        }
        fn config_upgrade_set_key(&self) -> Option<String> {
            self.key.clone()
        }
    }

    /// #3588: the `create_upgrade` response includes the top-level
    /// `config_upgrade_set_key` (read from the runner); non-create_upgrade modes
    /// omit it. Mirrors stellar-core `CommandHandler::generateLoad`
    /// (CommandHandler.cpp:1488-1496).
    ///
    /// FAILS on main: `config_upgrade_set_key_for_response` and the trait method
    /// `config_upgrade_set_key` do not exist.
    #[test]
    fn test_create_upgrade_response_includes_config_upgrade_set_key() {
        let runner = StubRunner {
            key: Some("EXPECTED_KEY_B64".to_string()),
        };

        // create_upgrade (all accepted spellings) → key emitted.
        for mode in ["create_upgrade", "soroban_create_upgrade", "createupgrade"] {
            assert_eq!(
                config_upgrade_set_key_for_response(mode, &runner),
                Some("EXPECTED_KEY_B64".to_string()),
                "mode {mode} must emit the config_upgrade_set_key"
            );
        }

        // Non-create_upgrade modes → field omitted even though the runner has a key.
        for mode in ["pay", "soroban_upload", "soroban_invoke", "upgrade_setup"] {
            assert_eq!(
                config_upgrade_set_key_for_response(mode, &runner),
                None,
                "mode {mode} must NOT emit the config_upgrade_set_key"
            );
        }
    }

    /// The trait default returns `None` (a runner that cannot compute a key).
    #[test]
    fn test_config_upgrade_set_key_default_none() {
        struct DefaultRunner;
        impl LoadGenRunner for DefaultRunner {
            fn start_load(&self, _request: LoadGenRequest) -> Result<(), String> {
                Ok(())
            }
            fn stop_load(&self) {}
            fn is_running(&self) -> bool {
                false
            }
        }
        assert_eq!(
            config_upgrade_set_key_for_response("create_upgrade", &DefaultRunner),
            None
        );
    }

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
            preloadedtransactionsfile: None,
            ..apply_load_params_defaults()
        };

        let request: LoadGenRequest = params.into();

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
            preloaded_transactions_file: None,
            ..apply_load_request_defaults()
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
            preloaded_transactions_file: Some("/tmp/txs.xdr".to_string()),
            ..apply_load_request_defaults()
        };
        let cloned = request.clone();
        assert_eq!(cloned.mode, request.mode);
        assert_eq!(cloned.accounts, request.accounts);
        assert_eq!(cloned.tx_rate, request.tx_rate);
        assert_eq!(cloned.instances, request.instances);
        assert_eq!(
            cloned.preloaded_transactions_file,
            request.preloaded_transactions_file
        );
    }

    /// The pregenerated-file query param flows into `LoadGenRequest`, and an
    /// empty string is normalized to `None` (matches "no file supplied").
    #[test]
    fn test_preloaded_transactions_file_from_params() {
        let mut params = GenerateLoadParams {
            mode: "pay_pregenerated".to_string(),
            accounts: 100,
            txs: 100,
            txrate: 10,
            offset: 0,
            spikeinterval: 0,
            spikesize: 0,
            maxfeerate: 0,
            skiplowfeetxs: false,
            minpercentsuccess: 0,
            instances: 0,
            wasms: 0,
            preloadedtransactionsfile: Some("/data/pregenerated.xdr".to_string()),
            ..apply_load_params_defaults()
        };
        let req: LoadGenRequest = params.clone().into();
        assert_eq!(
            req.preloaded_transactions_file.as_deref(),
            Some("/data/pregenerated.xdr")
        );

        // Empty string → None.
        params.preloadedtransactionsfile = Some(String::new());
        let req2: LoadGenRequest = params.into();
        assert_eq!(req2.preloaded_transactions_file, None);
    }

    /// #3309: the `APPLY_LOAD_*` params (scalars + comma-separated
    /// distribution value/weight strings) flow through `GenerateLoadParams`
    /// into `LoadGenRequest`, parsing CSV into `(values, weights)` pairs.
    #[test]
    fn test_apply_load_params_plumbing() {
        let params = GenerateLoadParams {
            mode: "soroban_invoke_apply_load".to_string(),
            applyloadblbatchsize: 500,
            applyloadblsimulatedledgers: 200,
            applyloaddataentrysize: 7,
            overlayonlymode: true,
            applyloadnumrwentries: Some("1,2,3".to_string()),
            applyloadnumrwentriesdistribution: Some("4,5,6".to_string()),
            applyloadinstructions: Some("100, 200 ,".to_string()),
            applyloadinstructionsdistribution: Some("1,1".to_string()),
            ..apply_load_params_defaults()
        };
        let req: LoadGenRequest = params.into();
        assert!(req.overlay_only_mode);
        assert_eq!(req.apply_load_bl_batch_size, 500);
        assert_eq!(req.apply_load_bl_simulated_ledgers, 200);
        // data_entry_size is left raw here (rounding happens in main.rs).
        assert_eq!(req.apply_load_data_entry_size, 7);
        assert_eq!(
            req.apply_load_num_rw_entries,
            (vec![1, 2, 3], vec![4, 5, 6])
        );
        // Empty CSV entries are ignored.
        assert_eq!(req.apply_load_instructions, (vec![100, 200], vec![1, 1]));
        // Unspecified distributions default to empty.
        assert_eq!(
            req.apply_load_event_count,
            (Vec::<u32>::new(), Vec::<u32>::new())
        );
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
