//! HTTP server module for henyey node status and control.
//!
//! This module provides the HTTP API for monitoring and interacting with the node.
//! It is organized into:
//!
//! - [`types`]: Request and response structs for all endpoints
//! - [`handlers`]: Async handler functions for each route
//! - [`helpers`]: Shared utility functions (peer ID parsing, etc.)

pub mod handlers;
pub mod helpers;
pub mod types;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::{
    routing::{get, post},
    Router,
};

use crate::app::App;

use handlers::{
    admin::{
        clearmetrics_handler, ll_handler, logrotate_handler, maintenance_handler,
        manualclose_handler, shutdown_handler,
    },
    info::{
        dumpproposedsettings_handler, health_handler, info_handler, ledger_handler, quorum_handler,
        root_handler, self_check_handler, status_handler, upgrades_handler,
    },
    metrics::metrics_handler,
    peers::{bans_handler, connect_handler, droppeer_handler, peers_handler, unban_handler},
    scp::scp_handler,
    soroban::sorobaninfo_handler,
    survey::{
        start_survey_collecting_handler, stop_survey_collecting_handler,
        stop_survey_reporting_handler, survey_handler, survey_topology_handler,
    },
    tx::submit_tx_handler,
};

/// Shared state for the HTTP server.
pub(crate) struct ServerState {
    pub app: Arc<App>,
    pub start_time: Instant,
    /// ISO 8601 UTC timestamp of when the server started.
    pub started_on: String,
    /// Unix epoch timestamp of when the server started (seconds).
    pub started_on_epoch: f64,
    pub log_handle: Option<crate::logging::LogLevelHandle>,
    /// Prometheus metrics handle for rendering `/metrics` output.
    pub prometheus_handle: Option<metrics_exporter_prometheus::PrometheusHandle>,
    /// Load generation state (only present when `loadgen` feature is enabled).
    #[cfg(feature = "loadgen")]
    pub loadgen_state: Option<Arc<handlers::generateload::GenerateLoadState>>,
}

/// Build the axum router with all endpoints.
pub(crate) fn build_router(state: Arc<ServerState>) -> Router {
    let router = Router::new()
        .route("/", get(root_handler))
        .route("/info", get(info_handler))
        .route("/status", get(status_handler))
        .route("/metrics", get(metrics_handler))
        .route("/peers", get(peers_handler))
        .route("/connect", post(connect_handler))
        .route("/droppeer", post(droppeer_handler))
        .route("/bans", get(bans_handler))
        .route("/unban", post(unban_handler))
        .route("/ledger", get(ledger_handler))
        .route("/upgrades", get(upgrades_handler))
        .route("/self-check", post(self_check_handler))
        .route("/quorum", get(quorum_handler))
        .route("/survey", get(survey_handler))
        .route("/scp", get(scp_handler))
        .route("/survey/start", post(start_survey_collecting_handler))
        .route("/survey/stop", post(stop_survey_collecting_handler))
        .route("/survey/topology", post(survey_topology_handler))
        .route(
            "/survey/reporting/stop",
            post(stop_survey_reporting_handler),
        )
        .route("/tx", post(submit_tx_handler))
        .route("/shutdown", post(shutdown_handler))
        .route("/health", get(health_handler))
        .route("/ll", get(ll_handler).post(ll_handler))
        .route("/manualclose", post(manualclose_handler))
        .route("/sorobaninfo", get(sorobaninfo_handler))
        .route("/clearmetrics", post(clearmetrics_handler))
        .route("/logrotate", post(logrotate_handler))
        .route("/maintenance", post(maintenance_handler))
        .route("/dumpproposedsettings", get(dumpproposedsettings_handler));
    #[cfg(feature = "loadgen")]
    let router = router.route(
        "/generateload",
        get(handlers::generateload::generateload_handler),
    );
    router.with_state(state)
}

/// HTTP query server for ledger entry lookups.
///
/// Runs on a separate port from the status server. Provides endpoints for
/// querying the bucket list snapshots:
///
/// - `POST /getledgerentryraw` — Raw ledger entry lookup
/// - `POST /getledgerentry` — Entry lookup with TTL state classification
///
/// Matches stellar-core's `QueryServer` behavior, including the readiness
/// gate (`mIsReady`) that returns 404 "Core is booting" before the first
/// bucket snapshot is populated.
pub struct QueryServer {
    port: u16,
    address: String,
    app: Arc<App>,
}

impl QueryServer {
    /// Create a new query server on the given port and address.
    pub fn new(port: u16, address: String, app: Arc<App>) -> Self {
        Self { port, address, app }
    }

    /// Build the query server router.
    ///
    /// Applies a readiness middleware via `route_layer` (not `layer`) so that
    /// only matched routes are gated — the fallback 404 handler serves the
    /// welcome page regardless of readiness, matching stellar-core's behavior
    /// where `safeRouter` wraps registered handlers but `notFound` is
    /// independent.
    fn build_router(state: Arc<handlers::query::QueryState>) -> Router {
        Router::new()
            .route(
                "/getledgerentryraw",
                post(handlers::query::getledgerentryraw_handler),
            )
            .route(
                "/getledgerentry",
                post(handlers::query::getledgerentry_handler),
            )
            .route_layer(axum::middleware::from_fn_with_state(
                state.clone(),
                handlers::query::readiness_check,
            ))
            .fallback(handlers::query::not_found_fallback)
            .with_state(state)
    }

    /// Start the query server.
    pub async fn start(self) -> anyhow::Result<()> {
        let state = Arc::new(handlers::query::QueryState {
            snapshot_manager: self.app.bucket_snapshot_manager().clone(),
            is_ready: self.app.query_is_ready().clone(),
        });

        let mut shutdown_rx = self.app.subscribe_shutdown();

        let router = Self::build_router(state);

        let addr: SocketAddr = if self.address.contains(':') {
            format!("[{}]:{}", self.address, self.port)
        } else {
            format!("{}:{}", self.address, self.port)
        }
        .parse()?;
        tracing::info!(%addr, "Starting HTTP query server");

        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.recv().await;
            })
            .await?;

        tracing::info!("HTTP query server stopped");
        Ok(())
    }
}

/// HTTP server for node status and control.
pub struct StatusServer {
    port: u16,
    address: String,
    app: Arc<App>,
    start_time: Instant,
    /// Unix epoch seconds captured at construction (≈ application start).
    started_on_epoch: f64,
    log_handle: Option<crate::logging::LogLevelHandle>,
    prometheus_handle: Option<metrics_exporter_prometheus::PrometheusHandle>,
    #[cfg(feature = "loadgen")]
    loadgen_state: Option<Arc<handlers::generateload::GenerateLoadState>>,
}

impl StatusServer {
    /// Create a new status server.
    pub fn new(port: u16, address: String, app: Arc<App>) -> Self {
        let started_on_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        Self {
            port,
            address,
            app,
            start_time: Instant::now(),
            started_on_epoch,
            log_handle: None,
            prometheus_handle: None,
            #[cfg(feature = "loadgen")]
            loadgen_state: None,
        }
    }

    /// Create a new status server with a log level handle for dynamic log changes.
    pub fn with_log_handle(
        port: u16,
        address: String,
        app: Arc<App>,
        log_handle: crate::logging::LogLevelHandle,
    ) -> Self {
        let started_on_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        Self {
            port,
            address,
            app,
            start_time: Instant::now(),
            started_on_epoch,
            log_handle: Some(log_handle),
            prometheus_handle: None,
            #[cfg(feature = "loadgen")]
            loadgen_state: None,
        }
    }

    /// Set the Prometheus metrics handle for the `/metrics` endpoint.
    pub fn set_prometheus_handle(&mut self, handle: metrics_exporter_prometheus::PrometheusHandle) {
        self.prometheus_handle = Some(handle);
    }

    /// Set the load generation backend (must be called before `start()`).
    #[cfg(feature = "loadgen")]
    pub fn set_loadgen_runner(&mut self, runner: Box<dyn handlers::generateload::LoadGenRunner>) {
        self.loadgen_state = Some(Arc::new(handlers::generateload::GenerateLoadState {
            runner,
        }));
    }

    /// Start the server.
    pub async fn start(self) -> anyhow::Result<()> {
        let started_on = format_utc_now();
        let state = Arc::new(ServerState {
            app: self.app,
            start_time: self.start_time,
            started_on,
            started_on_epoch: self.started_on_epoch,
            log_handle: self.log_handle,
            prometheus_handle: self.prometheus_handle,
            #[cfg(feature = "loadgen")]
            loadgen_state: self.loadgen_state,
        });

        let mut shutdown_rx = state.app.subscribe_shutdown();

        let router = build_router(state);

        let addr: SocketAddr = if self.address.contains(':') {
            format!("[{}]:{}", self.address, self.port)
        } else {
            format!("{}:{}", self.address, self.port)
        }
        .parse()?;
        tracing::info!(%addr, "Starting HTTP status server");

        let listener = tokio::net::TcpListener::bind(addr).await?;
        // When bound to port 0 (ephemeral), emit the kernel-assigned port
        // on stdout in a deterministic machine-readable form. Integration
        // tests that launch the node as a subprocess parse this line to
        // discover the port. Plain `println!` bypasses any tracing
        // subscriber filtering. Static-port deployments skip the line so
        // production log aggregators see no new output.
        if self.port == 0 {
            let bound_port = listener.local_addr()?.port();
            println!("{ADMIN_PORT_LOG_PREFIX}{bound_port}");
        }

        axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.recv().await;
            })
            .await?;

        tracing::info!("HTTP status server stopped");
        Ok(())
    }
}

/// Boot-log prefix used to surface the kernel-assigned admin HTTP port when
/// `http.port = 0`. Tests that launch `henyey` as a subprocess parse stdout
/// for this prefix to discover which port to send requests to.
///
/// Changing this string is a breaking change for integration tests — keep it
/// stable.
pub const ADMIN_PORT_LOG_PREFIX: &str = "HTTP_ADMIN_PORT=";

/// Format the current UTC time as ISO 8601 (e.g. "2026-01-15T12:34:56Z").
pub(crate) fn format_utc_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format_unix_timestamp(secs)
}

/// Format a UNIX timestamp as ISO 8601 UTC.
fn format_unix_timestamp(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let time_secs = secs % 86400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year,
        month,
        day,
        time_secs / 3600,
        (time_secs % 3600) / 60,
        time_secs % 60,
    )
}

/// Convert days since 1970-01-01 to `(year, month, day)`.
/// Algorithm from Howard Hinnant's `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_unix_timestamp() {
        // 2024-01-01T00:00:00Z = 1704067200
        assert_eq!(format_unix_timestamp(1704067200), "2024-01-01T00:00:00Z");
        // Unix epoch
        assert_eq!(format_unix_timestamp(0), "1970-01-01T00:00:00Z");
        // 2026-03-03T15:30:45Z
        assert_eq!(format_unix_timestamp(1772551845), "2026-03-03T15:30:45Z");
    }

    #[test]
    fn test_civil_from_days() {
        // Day 0 = 1970-01-01
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // Day 19723 = 2024-01-01 (1704067200 / 86400 = 19723)
        assert_eq!(civil_from_days(19723), (2024, 1, 1));
    }

    #[test]
    fn test_started_on_epoch_is_reasonable() {
        // Verify the epoch computation produces a reasonable value.
        let epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        // Should be after 2024-01-01 and before 2030-01-01.
        assert!(epoch > 1_704_067_200.0, "epoch too old: {}", epoch);
        assert!(
            epoch < 1_893_456_000.0,
            "epoch too far in future: {}",
            epoch
        );
    }
}
