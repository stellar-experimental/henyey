//! Handlers for administrative endpoints: /shutdown, /ll, /manualclose,
//! /clearmetrics, /logrotate, /maintenance.

use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
};

use super::super::types::{
    ClearMetricsParams, ClearMetricsResponse, LlParams, LlResponse, LogRotateResponse,
    MaintenanceParams, MaintenanceResponse, ManualCloseParams, ManualCloseResponse,
    SurveyCommandResponse,
};
use super::super::ServerState;

pub(crate) async fn shutdown_handler(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    state.app.shutdown();
    (
        StatusCode::OK,
        Json(SurveyCommandResponse {
            success: true,
            message: "Shutdown requested.".to_string(),
        }),
    )
}

/// Handler for /ll endpoint - get or set log levels.
///
/// GET /ll - returns current log levels for all partitions
/// POST /ll?level=INFO - set global log level
/// POST /ll?level=DEBUG&partition=SCP - set log level for specific partition
pub(crate) async fn ll_handler(
    State(state): State<Arc<ServerState>>,
    Query(params): Query<LlParams>,
) -> impl IntoResponse {
    use std::collections::HashMap;

    // If we don't have a log handle, return the stub response
    let Some(log_handle) = &state.log_handle else {
        let mut levels = HashMap::new();
        for (partition, _) in crate::logging::LOG_PARTITIONS {
            levels.insert(partition.to_string(), "INFO".to_string());
        }
        levels.insert("Global".to_string(), "INFO".to_string());

        if params.level.is_some() {
            levels.insert(
                "warning".to_string(),
                "Log level handle not available. Logging initialized without dynamic support."
                    .to_string(),
            );
        }

        return (StatusCode::OK, Json(LlResponse { levels }));
    };

    if let Some(level_str) = &params.level {
        if let Some(partition) = &params.partition {
            match log_handle.set_partition_level(partition, level_str) {
                Ok(()) => {
                    tracing::info!(
                        partition = %partition,
                        level = %level_str,
                        "Log level updated for partition"
                    );
                    let levels = log_handle.get_levels();
                    return (StatusCode::OK, Json(LlResponse { levels }));
                }
                Err(e) => {
                    let mut levels = HashMap::new();
                    levels.insert("error".to_string(), e.to_string());
                    return (StatusCode::BAD_REQUEST, Json(LlResponse { levels }));
                }
            }
        } else {
            match log_handle.set_level(level_str) {
                Ok(()) => {
                    tracing::info!(level = %level_str, "Global log level updated");
                    let levels = log_handle.get_levels();
                    return (StatusCode::OK, Json(LlResponse { levels }));
                }
                Err(e) => {
                    let mut levels = HashMap::new();
                    levels.insert("error".to_string(), e.to_string());
                    return (StatusCode::BAD_REQUEST, Json(LlResponse { levels }));
                }
            }
        }
    }

    // GET request: return current levels
    let levels = log_handle.get_levels();
    (StatusCode::OK, Json(LlResponse { levels }))
}

/// Handler for /manualclose endpoint - manually close a ledger.
pub(crate) async fn manualclose_handler(
    State(state): State<Arc<ServerState>>,
    Query(params): Query<ManualCloseParams>,
) -> impl IntoResponse {
    if params.ledger_seq.is_some() || params.close_time.is_some() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ManualCloseResponse {
                success: false,
                ledger_seq: None,
                message: Some(
                    "The 'manualclose' command accepts parameters only if the configuration includes RUN_STANDALONE=true.".to_string()
                ),
            }),
        );
    }

    match state.app.manual_close_ledger().await {
        Ok(new_ledger) => (
            StatusCode::OK,
            Json(ManualCloseResponse {
                success: true,
                ledger_seq: Some(new_ledger),
                message: Some(format!("Triggered manual close for ledger {}", new_ledger)),
            }),
        ),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ManualCloseResponse {
                success: false,
                ledger_seq: None,
                message: Some(e.to_string()),
            }),
        ),
    }
}

/// Handler for /clearmetrics endpoint - clear metrics registry.
pub(crate) async fn clearmetrics_handler(
    State(state): State<Arc<ServerState>>,
    Query(params): Query<ClearMetricsParams>,
) -> Json<ClearMetricsResponse> {
    let domain = params.domain.unwrap_or_default();
    state.app.clear_metrics(&domain);

    let message = if domain.is_empty() {
        "Cleared all metrics!".to_string()
    } else {
        format!("Cleared {} metrics!", domain)
    };

    Json(ClearMetricsResponse { message })
}

/// Handler for /logrotate endpoint - trigger log file rotation.
pub(crate) async fn logrotate_handler() -> Json<LogRotateResponse> {
    tracing::info!("Log rotate requested");
    Json(LogRotateResponse {
        message: "Log rotate...".to_string(),
    })
}

/// Handler for /maintenance endpoint - trigger manual database maintenance.
pub(crate) async fn maintenance_handler(
    State(state): State<Arc<ServerState>>,
    Query(params): Query<MaintenanceParams>,
) -> Json<MaintenanceResponse> {
    if params.queue.as_deref() != Some("true") {
        return Json(MaintenanceResponse {
            message: "No work performed".to_string(),
        });
    }

    let count = params.count.unwrap_or(state.app.config().maintenance.count);
    let app = Arc::clone(&state.app);
    match henyey_common::spawn_blocking_logged("maintenance", move || {
        app.perform_maintenance(count);
    })
    .await
    {
        Ok(()) => {}
        Err(join_err) => std::panic::resume_unwind(join_err.into_panic()),
    }

    Json(MaintenanceResponse {
        message: "Done".to_string(),
    })
}
