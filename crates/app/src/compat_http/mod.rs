//! stellar-core compatibility HTTP server.
//!
//! This module provides an optional HTTP server that matches stellar-core's
//! exact wire format, enabling henyey as a drop-in replacement for stellar-core
//! when used by stellar-rpc.
//!
//! Key differences from the native henyey HTTP server:
//!
//! - All endpoints use `GET` with query parameters (stellar-core style)
//! - JSON field names use camelCase where stellar-core does
//! - Error responses use `{"exception": "message"}` format
//! - Admin endpoints return plain text instead of JSON
//! - `/info` wraps response in `{"info": {...}}`
//! - `/tx` accepts `GET /tx?blob=<base64>` and returns stellar-core status strings

pub mod handlers;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use tower::ServiceBuilder;
use tower_http::catch_panic::CatchPanicLayer;

use crate::app::App;

/// Shared state for the compatibility HTTP server.
pub(crate) struct CompatServerState {
    pub app: Arc<App>,
    /// Used for uptime calculation (e.g. in `/info` handler).
    #[allow(dead_code)]
    pub start_time: Instant,
    /// ISO 8601 UTC timestamp of when the server started.
    pub started_on: String,
    /// Load generation state (only present when `loadgen` feature is enabled).
    #[cfg(feature = "loadgen")]
    pub loadgen_state: Option<Arc<crate::http::handlers::generateload::GenerateLoadState>>,
}

/// Build the stellar-core compatibility router.
///
/// All routes are GET-based with query parameters, matching stellar-core's
/// `CommandHandler` registration pattern. The `CatchPanicLayer` provides
/// the `safeRouter` equivalent, catching panics and returning error JSON.
pub(crate) fn build_compat_router(state: Arc<CompatServerState>) -> Router {
    let router = Router::new()
        .route("/info", get(handlers::info::compat_info_handler))
        .route("/tx", get(handlers::tx::compat_tx_handler))
        .route("/peers", get(handlers::peers::compat_peers_handler))
        .route("/metrics", get(handlers::metrics::compat_metrics_handler))
        .route(
            "/sorobaninfo",
            get(handlers::plaintext::compat_sorobaninfo_handler),
        )
        .route(
            "/maintenance",
            get(handlers::plaintext::compat_maintenance_handler),
        )
        .route(
            "/manualclose",
            get(handlers::plaintext::compat_manualclose_handler),
        )
        .route(
            "/clearmetrics",
            get(handlers::plaintext::compat_clearmetrics_handler),
        )
        .route(
            "/logrotate",
            get(handlers::plaintext::compat_logrotate_handler),
        )
        .route("/ll", get(handlers::plaintext::compat_ll_handler))
        .route("/connect", get(handlers::plaintext::compat_connect_handler))
        .route(
            "/droppeer",
            get(handlers::plaintext::compat_droppeer_handler),
        )
        .route("/unban", get(handlers::plaintext::compat_unban_handler))
        .route("/bans", get(handlers::plaintext::compat_bans_handler))
        .route("/quorum", get(handlers::plaintext::compat_quorum_handler))
        .route("/scp", get(handlers::plaintext::compat_scp_handler))
        .route("/upgrades", get(handlers::plaintext::compat_upgrades_handler))
        .route(
            "/self-check",
            get(handlers::plaintext::compat_self_check_handler),
        )
        .route(
            "/dumpproposedsettings",
            get(handlers::plaintext::compat_dumpproposedsettings_handler),
        )
        // Survey endpoints use stellar-core URL paths
        .route(
            "/getsurveyresult",
            get(handlers::plaintext::compat_getsurveyresult_handler),
        )
        .route(
            "/startsurveycollecting",
            get(handlers::plaintext::compat_startsurveycollecting_handler),
        )
        .route(
            "/stopsurveycollecting",
            get(handlers::plaintext::compat_stopsurveycollecting_handler),
        )
        .route(
            "/surveytopologytimesliced",
            get(handlers::plaintext::compat_surveytopology_handler),
        )
        .route(
            "/stopsurvey",
            get(handlers::plaintext::compat_stopreporting_handler),
        );
    #[cfg(feature = "loadgen")]
    let router = router.route(
        "/generateload",
        get(handlers::plaintext::compat_generateload_handler),
    );
    router.layer(
            ServiceBuilder::new()
                .layer(CatchPanicLayer::custom(safe_router_panic_handler)),
        )
        .with_state(state)
}

/// Panic handler for the `CatchPanicLayer`.
///
/// Matches stellar-core's `safeRouter` behavior: on exception/panic, return
/// `{"exception": "<message>"}`.
fn safe_router_panic_handler(
    _err: Box<dyn std::any::Any + Send + 'static>,
) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        axum::Json(serde_json::json!({"exception": "generic"})),
    )
        .into_response()
}

/// stellar-core compatibility HTTP server.
///
/// Runs on a configurable port (default 11626) and provides stellar-core's
/// exact wire format for all HTTP endpoints.
pub struct CompatServer {
    port: u16,
    app: Arc<App>,
    #[cfg(feature = "loadgen")]
    loadgen_state: Option<Arc<crate::http::handlers::generateload::GenerateLoadState>>,
}

impl CompatServer {
    /// Create a new compatibility server.
    pub fn new(port: u16, app: Arc<App>) -> Self {
        Self {
            port,
            app,
            #[cfg(feature = "loadgen")]
            loadgen_state: None,
        }
    }

    /// Set the load generation backend (must be called before `start()`).
    #[cfg(feature = "loadgen")]
    pub fn set_loadgen_runner(
        &mut self,
        runner: Box<dyn crate::http::handlers::generateload::LoadGenRunner>,
    ) {
        self.loadgen_state = Some(Arc::new(
            crate::http::handlers::generateload::GenerateLoadState {
                runner,
            },
        ));
    }

    /// Start the compatibility server.
    pub async fn start(self) -> anyhow::Result<()> {
        let started_on = crate::http::format_utc_now();
        let state = Arc::new(CompatServerState {
            app: self.app.clone(),
            start_time: Instant::now(),
            started_on,
            #[cfg(feature = "loadgen")]
            loadgen_state: self.loadgen_state,
        });

        let mut shutdown_rx = self.app.subscribe_shutdown();
        let router = build_compat_router(state);

        let addr = SocketAddr::from(([0, 0, 0, 0], self.port));
        tracing::info!(
            port = self.port,
            "Starting stellar-core compatibility HTTP server"
        );

        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.recv().await;
            })
            .await?;

        tracing::info!("stellar-core compatibility HTTP server stopped");
        Ok(())
    }
}
