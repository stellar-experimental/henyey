use serde::Serialize;

/// JSON-RPC 2.0 error codes.
pub(crate) const PARSE_ERROR: i32 = -32700;
pub(crate) const INVALID_REQUEST: i32 = -32600;
pub(crate) const METHOD_NOT_FOUND: i32 = -32601;
pub(crate) const INVALID_PARAMS: i32 = -32602;
pub(crate) const INTERNAL_ERROR: i32 = -32603;
/// Server is overloaded / too many concurrent requests.
pub(crate) const SERVER_BUSY: i32 = -32000;

/// JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl JsonRpcError {
    fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub(crate) fn parse_error(msg: impl Into<String>) -> Self {
        Self::new(PARSE_ERROR, msg)
    }

    pub(crate) fn invalid_request(msg: impl Into<String>) -> Self {
        Self::new(INVALID_REQUEST, msg)
    }

    pub(crate) fn method_not_found(method: &str) -> Self {
        Self::new(METHOD_NOT_FOUND, format!("method not found: {}", method))
    }

    pub(crate) fn invalid_params(msg: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, msg)
    }

    pub(crate) fn internal(msg: impl Into<String>) -> Self {
        Self::new(INTERNAL_ERROR, msg)
    }

    pub(crate) fn server_busy(msg: impl Into<String>) -> Self {
        Self::new(SERVER_BUSY, msg)
    }

    /// Internal error that logs full details server-side and returns only a
    /// generic category string to the client.
    pub(crate) fn internal_logged(category: &str, err: &dyn std::fmt::Debug) -> Self {
        tracing::warn!(error = ?err, category, "RPC internal error");
        Self::internal(category.to_string())
    }
}
