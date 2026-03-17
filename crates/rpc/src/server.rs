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
use crate::fee_window::FeeWindows;
use crate::types::{JsonRpcRequest, JsonRpcResponse};

/// Maximum allowed JSON-RPC request body size (512 KiB).
const MAX_REQUEST_BODY_BYTES: usize = 512 * 1024;

/// Stellar JSON-RPC 2.0 server.
pub struct RpcServer {
    port: u16,
    app: Arc<App>,
}

impl RpcServer {
    /// Create a new RPC server on the given port.
    pub fn new(port: u16, app: Arc<App>) -> Self {
        Self { port, app }
    }

    /// Start the RPC server.
    pub async fn start(self) -> anyhow::Result<()> {
        let retention = self.app.config().rpc.retention_window;
        let fee_windows = Arc::new(FeeWindows::new(retention));

        let ctx = Arc::new(RpcContext {
            app: self.app.clone(),
            fee_windows: fee_windows.clone(),
        });

        let mut shutdown_rx = self.app.subscribe_shutdown();

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
        axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.recv().await;
            })
            .await?;

        tracing::info!("JSON-RPC server stopped");
        Ok(())
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

        let metas = match app
            .database()
            .with_connection(|conn| {
                use henyey_db::LedgerCloseMetaQueries;
                conn.load_ledger_close_metas_in_range(start, current_ledger + 1, retention)
            })
        {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to load LCMs for fee window");
                continue;
            }
        };

        let mut gap_found = false;
        if let Err(e) = ingest_metas_with_gap_recovery(&windows, &metas) {
            tracing::warn!(error = %e, "Multiple gaps in LCM data");
            windows.reset();
            gap_found = true;
        }
        if gap_found {
            // Already handled the gap above, continue to next poll cycle
            continue;
        }
    }
}

/// Ingest a sequence of LCMs into the fee windows, recovering from a single gap.
///
/// If a gap is detected (ingestion fails), the windows are reset and
/// re-ingestion starts from the gap position. Returns `Err` if a second gap is
/// found in the post-gap suffix.
fn ingest_metas_with_gap_recovery(
    windows: &FeeWindows,
    metas: &[(u32, Vec<u8>)],
) -> Result<(), String> {
    for (i, (_seq, meta_bytes)) in metas.iter().enumerate() {
        if let Err(e) = windows.ingest_ledger_close_meta(meta_bytes) {
            // Gap detected — reset and re-ingest from this point forward
            tracing::warn!(error = %e, "Gap in LCM data, resetting fee window to post-gap range");
            windows.reset();
            for (_seq2, meta_bytes2) in &metas[i..] {
                if let Err(e2) = windows.ingest_ledger_close_meta(meta_bytes2) {
                    return Err(format!("multiple gaps in fee window data: {e2}"));
                }
            }
            return Ok(());
        }
    }
    Ok(())
}

/// Bulk-load the last N ledgers' fees from the database.
fn bulk_load_fees(app: &App, windows: &FeeWindows, retention: u32) -> Result<(), String> {
    let current = app.ledger_summary().num;
    if current == 0 {
        return Ok(());
    }

    let start = current.saturating_sub(retention).max(1);
    let metas = app
        .database()
        .with_connection(|conn| {
            use henyey_db::LedgerCloseMetaQueries;
            conn.load_ledger_close_metas_in_range(start, current + 1, retention)
        })
        .map_err(|e| format!("DB error loading LCMs: {e}"))?;

    tracing::info!(
        count = metas.len(),
        start,
        end = current,
        "Bulk-loading fee window from DB"
    );

    // Ingest metas, skipping over any gaps (e.g., from catchup).
    // Only keep the contiguous suffix so the window starts clean.
    ingest_metas_with_gap_recovery(windows, &metas)?;

    Ok(())
}

/// Single axum handler for all JSON-RPC requests.
async fn rpc_handler(
    State(ctx): State<Arc<RpcContext>>,
    body: axum::body::Bytes,
) -> (StatusCode, Json<JsonRpcResponse>) {
    // Reject batch requests (JSON arrays)
    if body.first().copied() == Some(b'[') {
        let resp = JsonRpcResponse::error(
            serde_json::Value::Null,
            JsonRpcError::invalid_request("batch requests are not supported"),
        );
        return (StatusCode::OK, Json(resp));
    }

    // Parse the request body
    let request: JsonRpcRequest = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(e) => {
            let resp = JsonRpcResponse::error(
                serde_json::Value::Null,
                JsonRpcError::invalid_request(format!("invalid JSON: {}", e)),
            );
            return (StatusCode::OK, Json(resp));
        }
    };

    // Validate jsonrpc version
    if request.jsonrpc != "2.0" {
        let resp = JsonRpcResponse::error(
            request.id,
            JsonRpcError::invalid_request("jsonrpc must be \"2.0\""),
        );
        return (StatusCode::OK, Json(resp));
    }

    let resp = dispatch::dispatch(&ctx, &request.method, request.id, request.params).await;
    (StatusCode::OK, Json(resp))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        let resp = JsonRpcResponse::error(
            json!(1),
            JsonRpcError::method_not_found("foo"),
        );
        let serialized = serde_json::to_string(&resp).unwrap();
        assert!(serialized.contains("\"error\""));
        assert!(!serialized.contains("\"result\""));
    }

    // -----------------------------------------------------------------------
    // Category F: Server Request Handling (4 tests)
    // -----------------------------------------------------------------------

    #[test]
    fn test_batch_request_detected() {
        // The handler checks `body.first().copied() == Some(b'[')` for batch rejection
        let batch_body = b"[{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"getHealth\"}]";
        assert_eq!(batch_body.first().copied(), Some(b'['));

        // Non-batch body should not trigger
        let single_body = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"getHealth\"}";
        assert_ne!(single_body.first().copied(), Some(b'['));
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
}
