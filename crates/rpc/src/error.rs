use serde::Serialize;

/// JSON-RPC 2.0 error codes.
pub(crate) const PARSE_ERROR: i32 = -32700;
pub(crate) const INVALID_REQUEST: i32 = -32600;
pub(crate) const METHOD_NOT_FOUND: i32 = -32601;
pub(crate) const INVALID_PARAMS: i32 = -32602;
pub(crate) const INTERNAL_ERROR: i32 = -32603;
/// Server is overloaded / too many concurrent requests.
pub(crate) const SERVER_BUSY: i32 = -32000;
/// Server is not yet ready (e.g., bucket snapshot not populated during boot).
pub(crate) const SERVER_NOT_READY: i32 = -32001;
/// Query exceeded its computational budget.
pub(crate) const QUERY_BUDGET_EXCEEDED: i32 = -32002;

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

    pub(crate) fn server_not_ready() -> Self {
        Self::new(SERVER_NOT_READY, "server is not ready, try again later")
    }

    pub(crate) fn query_budget_exceeded(msg: impl Into<String>) -> Self {
        Self::new(QUERY_BUDGET_EXCEEDED, msg)
    }

    /// Internal error that logs full details server-side and returns only a
    /// generic category string to the client.
    pub(crate) fn internal_logged(category: &str, err: &dyn std::fmt::Debug) -> Self {
        tracing::warn!(error = ?err, category, "RPC internal error");
        Self::internal(category.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `internal_logged` must return only the generic category string to the
    /// client.  The detailed error is logged server-side but must never appear
    /// in the response message or data fields.
    #[test]
    fn internal_logged_strips_sensitive_details() {
        let detail = "SELECT * FROM accounts WHERE id = 'GABCD': SQLITE_CORRUPT";
        let err = JsonRpcError::internal_logged("database error", &detail);

        assert_eq!(err.code, INTERNAL_ERROR);
        assert_eq!(err.message, "database error");
        assert!(err.data.is_none());

        // Negative: no sensitive info leaked
        assert!(!err.message.contains("SELECT"));
        assert!(!err.message.contains("GABCD"));
        assert!(!err.message.contains("SQLITE"));
    }

    /// Same contract with a complex `Debug`-implementing type — the formatted
    /// debug output must not leak into the client-visible response.
    #[test]
    fn internal_logged_with_complex_debug_type() {
        #[derive(Debug)]
        #[allow(dead_code)]
        struct InternalDbError {
            query: String,
            backtrace: String,
        }
        let err = InternalDbError {
            query: "DELETE FROM ledgers".into(),
            backtrace: "at src/db.rs:42".into(),
        };
        let rpc_err = JsonRpcError::internal_logged("storage failure", &err);

        assert_eq!(rpc_err.code, INTERNAL_ERROR);
        assert_eq!(rpc_err.message, "storage failure");
        assert!(rpc_err.data.is_none());
        assert!(!rpc_err.message.contains("DELETE"));
        assert!(!rpc_err.message.contains("backtrace"));
        assert!(!rpc_err.message.contains("src/db.rs"));
    }

    /// `server_not_ready()` returns the expected code and message.
    #[test]
    fn server_not_ready_code_and_message() {
        let err = JsonRpcError::server_not_ready();
        assert_eq!(err.code, SERVER_NOT_READY);
        assert_eq!(err.code, -32001);
        assert_eq!(err.message, "server is not ready, try again later");
        assert!(err.data.is_none());
    }
}
