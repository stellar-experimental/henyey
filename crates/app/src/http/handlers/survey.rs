//! Handlers for survey-related endpoints.

use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
};

use crate::app::{AppState, SurveyReport};

use super::super::types::{StartSurveyParams, SurveyCommandResponse, SurveyTopologyParams};
use super::super::ServerState;

pub(crate) async fn survey_handler(State(state): State<Arc<ServerState>>) -> Json<SurveyReport> {
    let report = state.app.survey_report().await;
    Json(report)
}

pub(crate) async fn start_survey_collecting_handler(
    State(state): State<Arc<ServerState>>,
    Query(params): Query<StartSurveyParams>,
) -> impl IntoResponse {
    if !survey_booted(&state).await {
        return survey_unavailable();
    }
    let ok = state.app.start_survey_collecting(params.nonce).await;
    survey_response(
        ok,
        "Requested network to start survey collecting.",
        "Failed to start survey collecting.",
    )
}

pub(crate) async fn stop_survey_collecting_handler(
    State(state): State<Arc<ServerState>>,
) -> impl IntoResponse {
    if !survey_booted(&state).await {
        return survey_unavailable();
    }
    let ok = state.app.stop_survey_collecting().await;
    survey_response(
        ok,
        "Requested network to stop survey collecting.",
        "Failed to stop survey collecting.",
    )
}

pub(crate) async fn survey_topology_handler(
    State(state): State<Arc<ServerState>>,
    Query(params): Query<SurveyTopologyParams>,
) -> impl IntoResponse {
    if !survey_booted(&state).await {
        return survey_unavailable();
    }
    let pubkey = match henyey_crypto::PublicKey::from_strkey(&params.node) {
        Ok(key) => key,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(SurveyCommandResponse {
                    success: false,
                    message: "Invalid node public key".to_string(),
                }),
            );
        }
    };
    let peer_id = henyey_overlay::PeerId::from_bytes(*pubkey.as_bytes());
    let (Some(inbound), Some(outbound)) = (params.inbound_index, params.outbound_index) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(SurveyCommandResponse {
                success: false,
                message: "Missing inbound_index or outbound_index".to_string(),
            }),
        );
    };

    let ok = state
        .app
        .survey_topology_timesliced(peer_id, inbound, outbound)
        .await;
    survey_response(ok, "Survey request queued.", "Survey request rejected.")
}

pub(crate) async fn stop_survey_reporting_handler(
    State(state): State<Arc<ServerState>>,
) -> impl IntoResponse {
    state.app.stop_survey_reporting().await;
    (
        StatusCode::OK,
        Json(SurveyCommandResponse {
            success: true,
            message: "Survey reporting stopped.".to_string(),
        }),
    )
}

async fn survey_booted(state: &ServerState) -> bool {
    matches!(
        state.app.state().await,
        AppState::Synced | AppState::Validating
    )
}

fn survey_unavailable() -> (StatusCode, Json<SurveyCommandResponse>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(SurveyCommandResponse {
            success: false,
            message: "Application is not fully booted, try again later.".to_string(),
        }),
    )
}

fn survey_response(
    success: bool,
    success_message: &'static str,
    failure_message: &'static str,
) -> (StatusCode, Json<SurveyCommandResponse>) {
    let message = if success {
        success_message
    } else {
        failure_message
    };
    (
        StatusCode::OK,
        Json(SurveyCommandResponse {
            success,
            message: message.to_string(),
        }),
    )
}
