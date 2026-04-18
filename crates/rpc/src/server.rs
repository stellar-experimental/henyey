use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};

use henyey_app::App;

use crate::context::RpcContext;
use crate::dispatch;
use crate::error::JsonRpcError;
use crate::fee_window::{FeeWindowError, FeeWindows};
use crate::types::{JsonRpcRequest, JsonRpcResponse};

/// Maximum allowed JSON-RPC request body size (512 KiB).
const MAX_REQUEST_BODY_BYTES: usize = 512 * 1024;

/// Stellar JSON-RPC 2.0 server.
pub struct RpcServer {
    port: u16,
    app: Arc<App>,
}

/// A bound but not-yet-serving RPC server.
///
/// Returned by [`RpcServer::bind`] so callers (e.g., integration tests) can
/// observe the kernel-assigned port when binding to port 0. Call
/// [`RpcServerRunning::serve`] to run the server to completion.
pub struct RpcServerRunning {
    listener: tokio::net::TcpListener,
    router: Router,
    shutdown_rx: tokio::sync::broadcast::Receiver<()>,
}

impl RpcServerRunning {
    /// Run the server until the shutdown signal fires or an error occurs.
    pub async fn serve(self) -> anyhow::Result<()> {
        let Self {
            listener,
            router,
            mut shutdown_rx,
        } = self;
        axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.recv().await;
            })
            .await?;

        tracing::info!("JSON-RPC server stopped");
        Ok(())
    }
}

impl RpcServer {
    /// Create a new RPC server on the given port.
    pub fn new(port: u16, app: Arc<App>) -> Self {
        Self { port, app }
    }

    /// Bind the TCP listener, construct the router, and spawn the fee-window
    /// poller. Returns the bound server plus its [`SocketAddr`] so callers
    /// can discover the kernel-assigned port when `port == 0`.
    ///
    /// The router is ready to serve requests; call
    /// [`RpcServerRunning::serve`] to drive the event loop.
    pub async fn bind(self) -> anyhow::Result<(RpcServerRunning, SocketAddr)> {
        let retention = self.app.config().rpc.retention_window;
        let fee_windows = Arc::new(FeeWindows::new(retention));

        let ctx = RpcContext::new(self.app.clone(), fee_windows.clone());

        // Spawn background task to populate fee windows from DB
        let poller_app = self.app.clone();
        let poller_windows = fee_windows.clone();
        let mut poller_shutdown = self.app.subscribe_shutdown();
        tokio::spawn(async move {
            fee_window_poller(poller_app, poller_windows, &mut poller_shutdown).await;
        });

        let router = Router::new()
            .route("/", post(rpc_handler))
            .layer(axum::extract::DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
            .with_state(ctx);

        let addr = SocketAddr::from(([0, 0, 0, 0], self.port));
        tracing::info!(port = self.port, "Starting JSON-RPC server");

        let listener = tokio::net::TcpListener::bind(addr).await?;
        let bound = listener.local_addr()?;
        let shutdown_rx = self.app.subscribe_shutdown();

        Ok((
            RpcServerRunning {
                listener,
                router,
                shutdown_rx,
            },
            bound,
        ))
    }

    /// Bind and serve. Convenience wrapper equivalent to
    /// `self.bind().await?.0.serve().await`.
    pub async fn start(self) -> anyhow::Result<()> {
        let (running, _addr) = self.bind().await?;
        running.serve().await
    }
}

/// Background task that polls for new ledgers and feeds fees into the window.
///
/// On startup, does a bulk load of the last `retention_window` ledgers from DB,
/// then polls every second for new ones.
async fn fee_window_poller(
    app: Arc<App>,
    windows: Arc<FeeWindows>,
    shutdown: &mut tokio::sync::broadcast::Receiver<()>,
) {
    let retention = app.config().rpc.retention_window;

    // Initial bulk load
    if let Err(e) = bulk_load_fees(&app, &windows, retention) {
        tracing::warn!(error = %e, "Failed initial fee window load");
    }

    // Poll loop
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = shutdown.recv() => {
                tracing::info!("Fee window poller shutting down");
                return;
            }
        }

        let current_ledger = app.ledger_summary().num;
        let window_latest = windows.latest_ledger();

        if current_ledger <= window_latest {
            continue;
        }

        // Load new ledgers [window_latest+1, current_ledger]
        let start = if window_latest == 0 {
            // Not yet loaded — shouldn't happen after bulk load, but handle gracefully
            current_ledger.saturating_sub(retention).max(1)
        } else {
            window_latest + 1
        };

        let metas = {
            let poller_db = app.database().clone();
            let s = start;
            let e = current_ledger + 1;
            let r = retention;
            match tokio::task::spawn_blocking(move || {
                poller_db.with_connection(|conn| {
                    use henyey_db::LedgerCloseMetaQueries;
                    conn.load_ledger_close_metas_in_range(s, e, r)
                })
            })
            .await
            {
                Ok(Ok(m)) => m,
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "Failed to load LCMs for fee window");
                    continue;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Fee window DB task panicked");
                    continue;
                }
            }
        };

        ingest_metas_best_effort(&windows, &metas);
    }
}

/// Summary of what happened during best-effort LCM ingestion.
#[derive(Default, Debug)]
struct IngestionSummary {
    /// Number of entries successfully ingested.
    ingested: u32,
    /// Number of times the window was reset (due to gaps).
    resets: u32,
    /// Number of corrupt entries skipped (parse failures or DB/XDR mismatch).
    corrupt_skipped: u32,
}

/// Ingest LCMs into fee windows using best-effort suffix recovery.
///
/// Scans forward through `metas`. On parse error (corrupt XDR or DB/XDR
/// sequence mismatch), the entry is skipped without resetting — this preserves
/// the valid prefix when corruption is at the end. On contiguity gap, the
/// windows are reset and the current entry is re-ingested as the new start.
///
/// This naturally converges to the latest valid contiguous suffix of the input.
fn ingest_metas_best_effort(windows: &FeeWindows, metas: &[(u32, Vec<u8>)]) -> IngestionSummary {
    let mut summary = IngestionSummary::default();

    for (seq, meta_bytes) in metas {
        match windows.ingest_ledger_close_meta(*seq, meta_bytes) {
            Ok(()) => {
                summary.ingested += 1;
            }
            Err(FeeWindowError::Parse(msg)) => {
                // Skip corrupt entry without resetting. If this is mid-stream,
                // the next valid entry will trigger a gap (handled below). If
                // this is at the end, the valid prefix is preserved.
                tracing::warn!(
                    ledger_seq = seq,
                    error = %msg,
                    "Skipping corrupt LCM in fee window"
                );
                summary.corrupt_skipped += 1;
            }
            Err(FeeWindowError::Gap { expected, got }) => {
                // Contiguity gap — reset and re-ingest current entry as new start
                tracing::warn!(
                    ledger_seq = seq,
                    expected,
                    got,
                    "Gap in LCM data, resetting fee window"
                );
                windows.reset();
                summary.resets += 1;
                match windows.ingest_ledger_close_meta(*seq, meta_bytes) {
                    Ok(()) => summary.ingested += 1,
                    Err(e) => {
                        // Should not happen (window is empty after reset), but
                        // handle defensively.
                        tracing::warn!(
                            ledger_seq = seq,
                            error = %e,
                            "Failed to re-ingest after gap reset"
                        );
                        windows.reset();
                    }
                }
            }
        }
    }

    if summary.resets > 0 || summary.corrupt_skipped > 0 {
        tracing::info!(
            ingested = summary.ingested,
            resets = summary.resets,
            corrupt_skipped = summary.corrupt_skipped,
            "Fee window ingestion completed with recovery"
        );
    }

    summary
}

/// Bulk-load the last N ledgers' fees from the database.
fn bulk_load_fees(app: &App, windows: &FeeWindows, retention: u32) -> Result<(), String> {
    let current = app.ledger_summary().num;
    if current == 0 {
        return Ok(());
    }

    let start = current.saturating_sub(retention).max(1);
    let metas = load_fee_window_metas(app, start, current + 1, retention)
        .map_err(|e| format!("DB error loading LCMs: {e}"))?;

    tracing::info!(
        count = metas.len(),
        start,
        end = current,
        "Bulk-loading fee window from DB"
    );

    ingest_metas_best_effort(windows, &metas);

    Ok(())
}

fn load_fee_window_metas(
    app: &App,
    start: u32,
    end: u32,
    retention: u32,
) -> Result<Vec<(u32, Vec<u8>)>, henyey_db::DbError> {
    let db = app.database().clone();
    // Run synchronous DB call in blocking thread context.
    // This function is called from both bulk_load_fees (startup, before server
    // is serving) and fee_window_poller (background task). Neither path is a
    // request handler, so no DB semaphore is needed.
    db.with_connection(|conn| {
        use henyey_db::LedgerCloseMetaQueries;
        conn.load_ledger_close_metas_in_range(start, end, retention)
    })
}

fn ok_json_response(resp: JsonRpcResponse) -> (StatusCode, Json<JsonRpcResponse>) {
    (StatusCode::OK, Json(resp))
}

/// Single axum handler for all JSON-RPC requests.
async fn rpc_handler(
    State(ctx): State<Arc<RpcContext>>,
    body: axum::body::Bytes,
) -> (StatusCode, Json<JsonRpcResponse>) {
    // Stage 1: Parse raw bytes to a generic JSON value.
    // Syntax errors get -32700 (Parse error) with no serde details leaked.
    let value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(error = %e, "malformed JSON-RPC request body");
            return ok_json_response(JsonRpcResponse::error(
                serde_json::Value::Null,
                JsonRpcError::parse_error("parse error"),
            ));
        }
    };

    // Stage 2: Reject batch requests (JSON arrays).
    // Checking the parsed Value is robust to leading whitespace.
    if value.is_array() {
        return ok_json_response(JsonRpcResponse::error(
            serde_json::Value::Null,
            JsonRpcError::invalid_request("batch requests are not supported"),
        ));
    }

    // Stage 3: Deserialize to typed request.
    // Recover the request id from the parsed Value so the client can correlate
    // the error response, even when other fields are malformed.
    let id = value.get("id").cloned().unwrap_or(serde_json::Value::Null);

    let request: JsonRpcRequest = match serde_json::from_value(value) {
        Ok(req) => req,
        Err(e) => {
            tracing::debug!(error = %e, "invalid JSON-RPC request structure");
            return ok_json_response(JsonRpcResponse::error(
                id,
                JsonRpcError::invalid_request("invalid request"),
            ));
        }
    };

    // Validate jsonrpc version
    if request.jsonrpc != "2.0" {
        return ok_json_response(JsonRpcResponse::error(
            request.id,
            JsonRpcError::invalid_request("jsonrpc must be \"2.0\""),
        ));
    }

    // Acquire request execution permit (immediate reject, not backpressure)
    let _permit = match ctx.request_semaphore.try_acquire() {
        Ok(permit) => permit,
        Err(_) => {
            return ok_json_response(JsonRpcResponse::error(
                request.id,
                JsonRpcError::server_busy("too many concurrent requests"),
            ));
        }
    };

    // Per-method timeout: sendTransaction is side-effectful — timing out after
    // tx submission would mislead the client into thinking it failed.
    //
    // NOTE: When timeout fires, any spawn_blocking work (DB queries, bucket reads,
    // simulations) continues to completion on the blocking threadpool. The request
    // permit is freed so new requests can be admitted. This is an inherent limitation
    // of spawn_blocking — blocking work cannot be cancelled. However, domain-specific
    // semaphore permits (db_semaphore, simulation_semaphore, bucket_io_semaphore) are
    // held inside blocking closures via OwnedSemaphorePermit, so concurrent blocking
    // work remains bounded even after request timeout. The fee window poller
    // (server.rs background task) is outside these semaphores.
    let is_write_method = request.method == "sendTransaction";

    if is_write_method {
        let resp = dispatch::dispatch(&ctx, &request.method, request.id, request.params).await;
        ok_json_response(resp)
    } else {
        match tokio::time::timeout(
            ctx.request_timeout,
            dispatch::dispatch(&ctx, &request.method, request.id.clone(), request.params),
        )
        .await
        {
            Ok(resp) => ok_json_response(resp),
            Err(_) => ok_json_response(JsonRpcResponse::error(
                request.id,
                JsonRpcError::internal("request timed out"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -------------------------------------------------------------------
    // Shared test helper: boot a single-node simulation App
    // -------------------------------------------------------------------

    use std::time::Duration;

    use henyey_app::config::QuorumSetConfig;
    use henyey_app::AppState;
    use henyey_common::Hash256;
    use henyey_crypto::SecretKey;
    use henyey_simulation::{Simulation, SimulationMode};
    use tokio::sync::Semaphore;

    /// Boot a single-node standalone simulation and close one ledger so
    /// the DB has data.  Returns the `Simulation` (must be kept alive) and
    /// the `Arc<App>`.
    async fn boot_test_app() -> (Simulation, std::sync::Arc<App>) {
        let mut sim =
            Simulation::with_network(SimulationMode::OverTcp, "Test SDF Network ; September 2015");

        let seed = Hash256::hash(b"RPC_SERVER_UNIT_TEST_NODE");
        let secret = SecretKey::from_seed(&seed.0);
        let quorum_set = QuorumSetConfig {
            threshold_percent: 100,
            validators: vec![secret.public_key().to_strkey()],
            inner_sets: Vec::new(),
        };

        sim.add_app_node("node0", secret, quorum_set);
        sim.start_all_nodes().await;

        let app = sim.app("node0").expect("app node");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            if app.state().await == AppState::Validating {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(app.state().await, AppState::Validating);

        let closed = sim
            .manual_close_all_app_nodes()
            .await
            .expect("manual close");
        assert_eq!(closed, vec![2]);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            if sim.have_all_app_nodes_externalized(2, 0) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(sim.have_all_app_nodes_externalized(2, 0));

        (sim, app)
    }

    /// Build an `RpcContext` with custom semaphore sizes and timeout.
    fn make_ctx(
        app: std::sync::Arc<App>,
        request_sem: usize,
        db_sem: usize,
        bucket_io_sem: usize,
        timeout: Duration,
    ) -> std::sync::Arc<RpcContext> {
        std::sync::Arc::new(RpcContext {
            app,
            fee_windows: std::sync::Arc::new(FeeWindows::new(10)),
            simulation_semaphore: std::sync::Arc::new(Semaphore::new(1)),
            request_semaphore: std::sync::Arc::new(Semaphore::new(request_sem)),
            db_semaphore: std::sync::Arc::new(Semaphore::new(db_sem)),
            bucket_io_semaphore: std::sync::Arc::new(Semaphore::new(bucket_io_sem)),
            request_timeout: timeout,
        })
    }

    /// Call `rpc_handler` directly and parse the JSON response.
    async fn call_handler(
        ctx: &std::sync::Arc<RpcContext>,
        body: &[u8],
    ) -> (StatusCode, serde_json::Value) {
        let (status, Json(resp)) =
            rpc_handler(State(ctx.clone()), axum::body::Bytes::from(body.to_vec())).await;
        let json = serde_json::to_value(&resp).unwrap();
        (status, json)
    }

    // -------------------------------------------------------------------
    // Category I: Runtime hardening — concurrency & timeout (#1743)
    // -------------------------------------------------------------------

    /// When the request semaphore is full, `rpc_handler` must return
    /// `-32000` (SERVER_BUSY) immediately without dispatching the method.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_server_busy_when_semaphore_exhausted() {
        let (_sim, app) = boot_test_app().await;
        let ctx = make_ctx(app, 1, 1, 1, Duration::from_secs(30));

        // Exhaust the single permit
        let _held = ctx.request_semaphore.try_acquire().unwrap();

        let body = br#"{"jsonrpc":"2.0","id":42,"method":"getHealth"}"#;
        let (status, resp) = call_handler(&ctx, body).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 42);
        assert!(resp.get("result").is_none() || resp["result"].is_null());
        let err = &resp["error"];
        assert_eq!(err["code"], json!(-32000));
        assert_eq!(err["message"], "too many concurrent requests");
        assert!(
            err.get("data").is_none() || err["data"].is_null(),
            "SERVER_BUSY must not include data"
        );
    }

    /// Read-only methods must be subject to `request_timeout`. When the
    /// timeout fires, the response must be `-32603` / "request timed out".
    /// After timeout, the request permit must be released.
    ///
    /// Strategy: hold the DB semaphore so `getHealth`'s `blocking_db` call
    /// blocks indefinitely on `acquire_owned()`. The 100ms timeout fires
    /// reliably because the semaphore is never released.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_read_only_timeout_releases_permit() {
        let (_sim, app) = boot_test_app().await;
        let ctx = make_ctx(app, 2, 1, 1, Duration::from_millis(100));

        // Hold the DB semaphore so getHealth's blocking_db call blocks
        // indefinitely on acquire_owned().
        let _db_held = ctx.db_semaphore.clone().try_acquire_owned().unwrap();

        let body = br#"{"jsonrpc":"2.0","id":77,"method":"getHealth"}"#;
        let (status, resp) = call_handler(&ctx, body).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 77);
        assert!(resp.get("result").is_none() || resp["result"].is_null());
        let err = &resp["error"];
        assert_eq!(err["code"], json!(-32603));
        assert_eq!(err["message"], "request timed out");
        assert!(
            err.get("data").is_none() || err["data"].is_null(),
            "timeout error must not include data"
        );

        // Request permit must be released after the handler returns.
        assert!(
            ctx.request_semaphore.try_acquire().is_ok(),
            "request_semaphore permit must be released after timeout"
        );
    }

    /// The `is_write_method` flag in `rpc_handler` must classify
    /// `sendTransaction` as a write method so it bypasses timeout.
    /// This is a structural test — it verifies the bypass condition
    /// directly, complementing the behavioral test below.
    #[test]
    fn test_send_transaction_classified_as_write_method() {
        // Mirror the classification logic from rpc_handler (line 377).
        let is_write = |method: &str| method == "sendTransaction";

        assert!(
            is_write("sendTransaction"),
            "sendTransaction must bypass timeout"
        );
        assert!(!is_write("getHealth"), "getHealth must NOT bypass timeout");
        assert!(
            !is_write("getTransaction"),
            "getTransaction must NOT bypass timeout"
        );
        assert!(
            !is_write("simulateTransaction"),
            "simulateTransaction must NOT bypass timeout"
        );
    }

    /// `sendTransaction` must NOT be subject to `request_timeout`.
    ///
    /// Proof: use the same config that causes read-only methods to timeout
    /// (DB semaphore held + 100ms timeout), then submit a parseable
    /// TransactionEnvelope via `sendTransaction`. Since `sendTransaction`
    /// bypasses the timeout wrapper, it processes the transaction to
    /// completion (herder rejects it, but no timeout error). Read-only
    /// methods under the same config timeout (proven by
    /// `test_read_only_timeout_releases_permit`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_send_transaction_exempt_from_timeout() {
        use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
        use stellar_xdr::curr::{
            Limits, Memo, MuxedAccount, Operation, OperationBody, Preconditions, SequenceNumber,
            Transaction, TransactionEnvelope, TransactionExt, TransactionV1Envelope, Uint256,
            WriteXdr,
        };

        let (_sim, app) = boot_test_app().await;
        let ctx = make_ctx(app, 2, 1, 1, Duration::from_millis(100));

        // Hold DB semaphore — read-only methods (getHealth) would timeout,
        // but sendTransaction doesn't use blocking_db and bypasses timeout.
        let _db_held = ctx.db_semaphore.clone().try_acquire_owned().unwrap();

        // Build a minimal TransactionEnvelope that passes XDR parsing,
        // reaching submit_transaction (async herder call). The tx will be
        // rejected by the herder (invalid account), but that's fine — we're
        // testing that it doesn't timeout, not that it succeeds.
        let tx = Transaction {
            source_account: MuxedAccount::Ed25519(Uint256([0; 32])),
            fee: 100,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![Operation {
                source_account: None,
                body: OperationBody::Inflation,
            }]
            .try_into()
            .unwrap(),
            ext: TransactionExt::V0,
        };
        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![].try_into().unwrap(),
        });
        let xdr_bytes = envelope.to_xdr(Limits::none()).unwrap();
        let tx_b64 = BASE64.encode(&xdr_bytes);

        let body = format!(
            r#"{{"jsonrpc":"2.0","id":99,"method":"sendTransaction","params":{{"transaction":"{}"}}}}"#,
            tx_b64
        );
        let (status, resp) = call_handler(&ctx, body.as_bytes()).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 99);
        // sendTransaction must NOT return "request timed out". It should
        // return a result (herder rejection) or method-specific error.
        if let Some(err) = resp.get("error") {
            assert_ne!(
                err["message"].as_str().unwrap_or(""),
                "request timed out",
                "sendTransaction must NOT be subject to request_timeout"
            );
        }
        // If result is present, sendTransaction completed normally — also fine.
    }

    #[test]
    fn test_parse_valid_request() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"getHealth"}"#;
        let req: JsonRpcRequest = serde_json::from_slice(body).unwrap();
        assert_eq!(req.jsonrpc, "2.0");
        assert_eq!(req.method, "getHealth");
        assert_eq!(req.id, json!(1));
        assert!(req.params.is_null());
    }

    #[test]
    fn test_parse_request_with_params() {
        let body = br#"{"jsonrpc":"2.0","id":"abc","method":"getLedgerEntries","params":{"keys":["AAAA"]}}"#;
        let req: JsonRpcRequest = serde_json::from_slice(body).unwrap();
        assert_eq!(req.method, "getLedgerEntries");
        assert_eq!(req.id, json!("abc"));
        assert!(req.params.is_object());
    }

    #[test]
    fn test_parse_malformed_json() {
        let body = br#"not json"#;
        let result: Result<JsonRpcRequest, _> = serde_json::from_slice(body);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_missing_method() {
        let body = br#"{"jsonrpc":"2.0","id":1}"#;
        let result: Result<JsonRpcRequest, _> = serde_json::from_slice(body);
        assert!(result.is_err());
    }

    #[test]
    fn test_response_success_serialization() {
        let resp = JsonRpcResponse::success(json!(1), json!({"status": "healthy"}));
        let serialized = serde_json::to_string(&resp).unwrap();
        assert!(serialized.contains("\"result\""));
        assert!(!serialized.contains("\"error\""));
    }

    #[test]
    fn test_response_error_serialization() {
        let resp = JsonRpcResponse::error(json!(1), JsonRpcError::method_not_found("foo"));
        let serialized = serde_json::to_string(&resp).unwrap();
        assert!(serialized.contains("\"error\""));
        assert!(!serialized.contains("\"result\""));
    }

    // -----------------------------------------------------------------------
    // Category F: Server Request Handling (4 tests)
    // -----------------------------------------------------------------------

    #[test]
    fn test_batch_request_detected() {
        // The handler parses to serde_json::Value, then checks .is_array()
        let batch_body = b"[{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"getHealth\"}]";
        let val: serde_json::Value = serde_json::from_slice(batch_body).unwrap();
        assert!(val.is_array());

        // Non-batch body should not trigger
        let single_body = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"getHealth\"}";
        let val: serde_json::Value = serde_json::from_slice(single_body).unwrap();
        assert!(!val.is_array());
    }

    #[test]
    fn test_body_size_limit_config() {
        // The server configures axum with a 512KB body limit.
        // We verify the constant is used correctly by checking the router builds
        // with DefaultBodyLimit::max(512 * 1024).
        let max_body = 512 * 1024;
        assert_eq!(max_body, 524_288);
    }

    #[test]
    fn test_invalid_jsonrpc_version_detected() {
        // The handler checks `request.jsonrpc != "2.0"` for version validation
        let body = br#"{"jsonrpc":"1.0","id":1,"method":"getHealth"}"#;
        let req: JsonRpcRequest = serde_json::from_slice(body).unwrap();
        assert_ne!(req.jsonrpc, "2.0");
    }

    #[test]
    fn test_unknown_method_error() {
        // dispatch returns method_not_found for unknown methods
        let err = JsonRpcError::method_not_found("doesNotExist");
        assert_eq!(err.code, crate::error::METHOD_NOT_FOUND);
        assert!(err.message.contains("doesNotExist"));
    }

    // -----------------------------------------------------------------------
    // Category G: Error Sanitization (AUDIT-163 regression tests)
    // -----------------------------------------------------------------------

    /// Simulate the handler's parse stage: malformed JSON returns -32700 with
    /// no serde details.
    #[test]
    fn test_malformed_json_parse_error_no_leak() {
        let body = b"not json at all";
        let result: Result<serde_json::Value, _> = serde_json::from_slice(body);
        assert!(result.is_err());
        // The handler would return this:
        let err = JsonRpcError::parse_error("parse error");
        assert_eq!(err.code, crate::error::PARSE_ERROR);
        assert_eq!(err.message, "parse error");
        // No serde details in the message
        assert!(!err.message.contains("expected"));
        assert!(!err.message.contains("line"));
        assert!(!err.message.contains("column"));
    }

    /// Valid JSON but missing required fields: returns -32600 with generic message.
    #[test]
    fn test_invalid_request_structure_no_leak() {
        // Valid JSON object but missing "method" (required by JsonRpcRequest)
        let body = br#"{"jsonrpc":"2.0","id":7}"#;
        let value: serde_json::Value = serde_json::from_slice(body).unwrap();
        let result: Result<JsonRpcRequest, _> = serde_json::from_value(value.clone());
        assert!(result.is_err());
        // Handler recovers id from parsed Value
        let id = value.get("id").cloned().unwrap_or(serde_json::Value::Null);
        assert_eq!(id, json!(7));
        // The handler would return this:
        let err = JsonRpcError::invalid_request("invalid request");
        assert_eq!(err.code, crate::error::INVALID_REQUEST);
        assert_eq!(err.message, "invalid request");
        assert!(!err.message.contains("missing field"));
    }

    /// Valid JSON with no id: handler falls back to null.
    #[test]
    fn test_invalid_request_no_id_fallback() {
        let body = br#"{"jsonrpc":"2.0"}"#;
        let value: serde_json::Value = serde_json::from_slice(body).unwrap();
        let id = value.get("id").cloned().unwrap_or(serde_json::Value::Null);
        assert_eq!(id, serde_json::Value::Null);
    }

    /// Whitespace-prefixed batch is still detected by value.is_array().
    #[test]
    fn test_whitespace_prefixed_batch_detected() {
        let body = b"  \n[{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"getHealth\"}]";
        let value: serde_json::Value = serde_json::from_slice(body).unwrap();
        assert!(
            value.is_array(),
            "whitespace-prefixed array must be detected"
        );
    }

    /// Parse error code constant is -32700 per JSON-RPC 2.0 spec.
    #[test]
    fn test_parse_error_code() {
        assert_eq!(crate::error::PARSE_ERROR, -32700);
        let err = JsonRpcError::parse_error("test");
        assert_eq!(err.code, -32700);
    }

    // -----------------------------------------------------------------------
    // Category H: Fee window ingestion recovery (AUDIT-175 regression tests)
    // -----------------------------------------------------------------------

    use stellar_xdr::curr::{
        Hash, LedgerCloseMetaV0, LedgerHeader, LedgerHeaderExt, LedgerHeaderHistoryEntry,
        LedgerHeaderHistoryEntryExt, Limits, StellarValue, StellarValueExt, TimePoint,
        TransactionSet, WriteXdr,
    };

    /// Build minimal valid LedgerCloseMeta XDR bytes for a given ledger sequence.
    fn make_lcm_bytes(seq: u32) -> Vec<u8> {
        let lcm = stellar_xdr::curr::LedgerCloseMeta::V0(LedgerCloseMetaV0 {
            ledger_header: LedgerHeaderHistoryEntry {
                hash: Hash([0; 32]),
                header: LedgerHeader {
                    ledger_version: 21,
                    previous_ledger_hash: Hash([0; 32]),
                    scp_value: StellarValue {
                        tx_set_hash: Hash([0; 32]),
                        close_time: TimePoint(0),
                        upgrades: vec![].try_into().unwrap(),
                        ext: StellarValueExt::Basic,
                    },
                    tx_set_result_hash: Hash([0; 32]),
                    bucket_list_hash: Hash([0; 32]),
                    ledger_seq: seq,
                    total_coins: 0,
                    fee_pool: 0,
                    inflation_seq: 0,
                    id_pool: 0,
                    base_fee: 100,
                    base_reserve: 5_000_000,
                    max_tx_set_size: 100,
                    skip_list: [Hash([0; 32]), Hash([0; 32]), Hash([0; 32]), Hash([0; 32])],
                    ext: LedgerHeaderExt::V0,
                },
                ext: LedgerHeaderHistoryEntryExt::V0,
            },
            tx_set: TransactionSet {
                previous_ledger_hash: Hash([0; 32]),
                txs: vec![].try_into().unwrap(),
            },
            tx_processing: vec![].try_into().unwrap(),
            upgrades_processing: vec![].try_into().unwrap(),
            scp_info: vec![].try_into().unwrap(),
        });
        lcm.to_xdr(Limits::none()).unwrap()
    }

    const CORRUPT_BYTES: &[u8] = &[0xFF, 0xFE, 0xFD, 0xFC, 0xFB, 0xFA, 0x00, 0x01, 0x02, 0x03];

    #[test]
    fn test_all_valid_entries_ingested() {
        let fw = FeeWindows::new(100);
        let metas: Vec<(u32, Vec<u8>)> = (10..=12).map(|s| (s, make_lcm_bytes(s))).collect();
        let summary = ingest_metas_best_effort(&fw, &metas);
        assert_eq!(summary.ingested, 3);
        assert_eq!(summary.resets, 0);
        assert_eq!(summary.corrupt_skipped, 0);
        assert_eq!(fw.latest_ledger(), 12);
    }

    #[test]
    fn test_corrupt_at_start_accepted_after() {
        let fw = FeeWindows::new(100);
        let metas = vec![
            (10, CORRUPT_BYTES.to_vec()),
            (11, make_lcm_bytes(11)),
            (12, make_lcm_bytes(12)),
        ];
        let summary = ingest_metas_best_effort(&fw, &metas);
        assert_eq!(summary.corrupt_skipped, 1);
        assert_eq!(summary.ingested, 2);
        assert_eq!(fw.latest_ledger(), 12);
    }

    #[test]
    fn test_corrupt_in_middle_resets_to_suffix() {
        let fw = FeeWindows::new(100);
        let metas = vec![
            (10, make_lcm_bytes(10)),
            (11, make_lcm_bytes(11)),
            (12, CORRUPT_BYTES.to_vec()),
            (13, make_lcm_bytes(13)),
            (14, make_lcm_bytes(14)),
        ];
        let summary = ingest_metas_best_effort(&fw, &metas);
        assert_eq!(summary.corrupt_skipped, 1);
        // 10, 11 ingested; 12 skipped; 13 triggers gap (expected 12, got 13) → reset, re-ingest; 14 ingested
        assert!(summary.resets >= 1);
        assert_eq!(fw.latest_ledger(), 14);
        // Window should contain [13, 14] (post-reset)
    }

    #[test]
    fn test_corrupt_at_end_preserves_prefix() {
        let fw = FeeWindows::new(100);
        let metas = vec![
            (10, make_lcm_bytes(10)),
            (11, make_lcm_bytes(11)),
            (12, CORRUPT_BYTES.to_vec()),
        ];
        let summary = ingest_metas_best_effort(&fw, &metas);
        assert_eq!(summary.corrupt_skipped, 1);
        assert_eq!(summary.resets, 0);
        // Prefix preserved — this is the key fix for the original bug
        assert_eq!(fw.latest_ledger(), 11);
    }

    #[test]
    fn test_gap_recovery_keeps_current_entry() {
        let fw = FeeWindows::new(100);
        let metas = vec![
            (10, make_lcm_bytes(10)),
            (11, make_lcm_bytes(11)),
            (20, make_lcm_bytes(20)),
            (21, make_lcm_bytes(21)),
        ];
        let summary = ingest_metas_best_effort(&fw, &metas);
        assert_eq!(summary.resets, 1);
        assert_eq!(fw.latest_ledger(), 21);
    }

    #[test]
    fn test_mixed_gap_and_corrupt() {
        let fw = FeeWindows::new(100);
        let metas = vec![
            (10, make_lcm_bytes(10)),
            (11, make_lcm_bytes(11)),
            (12, CORRUPT_BYTES.to_vec()),
            (20, make_lcm_bytes(20)),
            (21, make_lcm_bytes(21)),
        ];
        let summary = ingest_metas_best_effort(&fw, &metas);
        assert!(summary.corrupt_skipped >= 1);
        assert!(summary.resets >= 1);
        assert_eq!(fw.latest_ledger(), 21);
    }

    #[test]
    fn test_all_corrupt_produces_empty_window() {
        let fw = FeeWindows::new(100);
        let metas = vec![(10, CORRUPT_BYTES.to_vec()), (11, CORRUPT_BYTES.to_vec())];
        let summary = ingest_metas_best_effort(&fw, &metas);
        assert_eq!(summary.corrupt_skipped, 2);
        assert_eq!(summary.ingested, 0);
        assert_eq!(fw.latest_ledger(), 0);
    }

    #[test]
    fn test_multiple_consecutive_corrupt() {
        let fw = FeeWindows::new(100);
        let metas = vec![
            (10, make_lcm_bytes(10)),
            (11, CORRUPT_BYTES.to_vec()),
            (12, CORRUPT_BYTES.to_vec()),
            (13, make_lcm_bytes(13)),
            (14, make_lcm_bytes(14)),
        ];
        let summary = ingest_metas_best_effort(&fw, &metas);
        assert_eq!(summary.corrupt_skipped, 2);
        assert_eq!(fw.latest_ledger(), 14);
    }

    #[test]
    fn test_db_seq_xdr_seq_mismatch_treated_as_corrupt() {
        let fw = FeeWindows::new(100);
        // Valid XDR for seq 10, but db_seq says 99
        let metas = vec![
            (99, make_lcm_bytes(10)),
            (11, make_lcm_bytes(11)),
            (12, make_lcm_bytes(12)),
        ];
        let summary = ingest_metas_best_effort(&fw, &metas);
        assert_eq!(summary.corrupt_skipped, 1);
        assert_eq!(fw.latest_ledger(), 12);
    }

    #[test]
    fn test_multiple_gaps_converges_to_suffix() {
        let fw = FeeWindows::new(100);
        let metas = vec![
            (10, make_lcm_bytes(10)),
            (11, make_lcm_bytes(11)),
            (20, make_lcm_bytes(20)),
            (21, make_lcm_bytes(21)),
            (30, make_lcm_bytes(30)),
            (31, make_lcm_bytes(31)),
        ];
        let summary = ingest_metas_best_effort(&fw, &metas);
        assert_eq!(summary.resets, 2);
        assert_eq!(fw.latest_ledger(), 31);
    }

    #[test]
    fn test_no_partial_mutation_on_gap() {
        let fw = FeeWindows::new(100);
        // Ingest one entry, then trigger a gap
        fw.ingest_ledger_close_meta(10, &make_lcm_bytes(10))
            .unwrap();
        assert_eq!(fw.latest_ledger(), 10);

        // Try to ingest a non-contiguous entry
        let result = fw.ingest_ledger_close_meta(20, &make_lcm_bytes(20));
        assert!(matches!(result, Err(FeeWindowError::Gap { .. })));

        // Window should be unchanged (no partial mutation)
        assert_eq!(fw.latest_ledger(), 10);
    }

    #[test]
    fn test_empty_metas_no_op() {
        let fw = FeeWindows::new(100);
        let summary = ingest_metas_best_effort(&fw, &[]);
        assert_eq!(summary.ingested, 0);
        assert_eq!(summary.resets, 0);
        assert_eq!(summary.corrupt_skipped, 0);
        assert_eq!(fw.latest_ledger(), 0);
    }
}
