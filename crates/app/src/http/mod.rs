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
use std::time::Instant;

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
        dumpproposedsettings_handler, health_handler, info_handler, ledger_handler,
        quorum_handler, root_handler, self_check_handler, status_handler, upgrades_handler,
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
    pub log_handle: Option<crate::logging::LogLevelHandle>,
}

/// Build the axum router with all endpoints.
pub(crate) fn build_router(state: Arc<ServerState>) -> Router {
    Router::new()
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
        .route("/dumpproposedsettings", get(dumpproposedsettings_handler))
        .with_state(state)
}

/// HTTP server for node status and control.
pub struct StatusServer {
    port: u16,
    app: Arc<App>,
    start_time: Instant,
    log_handle: Option<crate::logging::LogLevelHandle>,
}

impl StatusServer {
    /// Create a new status server.
    pub fn new(port: u16, app: Arc<App>) -> Self {
        Self {
            port,
            app,
            start_time: Instant::now(),
            log_handle: None,
        }
    }

    /// Create a new status server with a log level handle for dynamic log changes.
    pub fn with_log_handle(
        port: u16,
        app: Arc<App>,
        log_handle: crate::logging::LogLevelHandle,
    ) -> Self {
        Self {
            port,
            app,
            start_time: Instant::now(),
            log_handle: Some(log_handle),
        }
    }

    /// Start the server.
    pub async fn start(self) -> anyhow::Result<()> {
        let started_on = format_utc_now();
        let state = Arc::new(ServerState {
            app: self.app,
            start_time: self.start_time,
            started_on,
            log_handle: self.log_handle,
        });

        let mut shutdown_rx = state.app.subscribe_shutdown();

        let router = build_router(state);

        let addr = SocketAddr::from(([0, 0, 0, 0], self.port));
        tracing::info!(port = self.port, "Starting HTTP status server");

        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.recv().await;
            })
            .await?;

        tracing::info!("HTTP status server stopped");
        Ok(())
    }
}

/// Format the current UTC time as ISO 8601 (e.g. "2026-01-15T12:34:56Z").
fn format_utc_now() -> String {
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
#[allow(clippy::manual_range_contains)]
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
}
