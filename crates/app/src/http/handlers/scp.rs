//! Handler for /scp endpoint.

use std::sync::Arc;

use axum::{
    extract::{Query, State},
    response::Json,
};

use super::super::types::{ScpInfoResponse, ScpParams, ScpSlotInfo};
use super::super::ServerState;

pub(crate) async fn scp_handler(
    State(state): State<Arc<ServerState>>,
    Query(params): Query<ScpParams>,
) -> Json<ScpInfoResponse> {
    let limit = params.limit.unwrap_or(2).min(20);
    let slots = state
        .app
        .scp_slot_snapshots(limit)
        .into_iter()
        .map(ScpSlotInfo::from)
        .collect();
    Json(ScpInfoResponse {
        node: state.app.info().public_key,
        slots,
    })
}

impl From<crate::app::ScpSlotSnapshot> for ScpSlotInfo {
    fn from(snapshot: crate::app::ScpSlotSnapshot) -> Self {
        Self {
            slot_index: snapshot.slot_index,
            is_externalized: snapshot.is_externalized,
            is_nominating: snapshot.is_nominating,
            fully_validated: snapshot.fully_validated,
            ballot_phase: snapshot.ballot_phase,
            nomination_round: snapshot.nomination_round,
            ballot_round: snapshot.ballot_round,
            envelope_count: snapshot.envelope_count,
        }
    }
}
