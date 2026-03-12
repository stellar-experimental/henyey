use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};

use henyey_app::App;

use crate::context::RpcContext;
use crate::dispatch;
use crate::error::JsonRpcError;
use crate::types::{JsonRpcRequest, JsonRpcResponse};

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
        let ctx = Arc::new(RpcContext {
            app: self.app.clone(),
            start_time: Instant::now(),
        });

        let mut shutdown_rx = self.app.subscribe_shutdown();

        let router = Router::new()
            .route("/", post(rpc_handler))
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

/// Single axum handler for all JSON-RPC requests.
async fn rpc_handler(
    State(ctx): State<Arc<RpcContext>>,
    body: axum::body::Bytes,
) -> (StatusCode, Json<JsonRpcResponse>) {
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
}
