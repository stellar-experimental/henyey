use std::sync::Arc;
use std::time::Instant;

use henyey_app::App;

/// Shared state for all RPC handlers.
pub struct RpcContext {
    /// The application instance.
    pub app: Arc<App>,
    /// Server start time (for uptime in getHealth).
    pub start_time: Instant,
}
