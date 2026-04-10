//! Handlers for /peers, /connect, /droppeer, /bans, /unban.

use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};

use super::super::helpers::{parse_connect_params, parse_peer_id_params, peer_id_to_strkey};
use super::super::types::{
    BansResponse, ConnectParams, DropPeerParams, PeerInfo, PeersByDirection, PeersResponse,
    SurveyCommandResponse, UnbanParams,
};
use super::super::ServerState;

pub(crate) async fn peers_handler(State(state): State<Arc<ServerState>>) -> Json<PeersResponse> {
    let snapshots = state.app.peer_snapshots().await;
    let (pending_count, _authenticated_count) = state.app.peer_counts().await;

    let mut inbound = Vec::new();
    let mut outbound = Vec::new();

    for snapshot in &snapshots {
        let direction_str = match snapshot.info.direction {
            henyey_overlay::ConnectionDirection::Inbound => "inbound",
            henyey_overlay::ConnectionDirection::Outbound => "outbound",
        };
        let peer_info = PeerInfo {
            id: snapshot.info.peer_id.to_hex(),
            address: snapshot.info.address.to_string(),
            direction: direction_str.to_string(),
            version: snapshot.info.version_string.clone(),
            overlay_version: snapshot.info.overlay_version,
            ledger_version: snapshot.info.ledger_version,
            messages_sent: snapshot.stats.messages_sent,
            messages_received: snapshot.stats.messages_received,
            bytes_sent: snapshot.stats.bytes_sent,
            bytes_received: snapshot.stats.bytes_received,
            elapsed_secs: snapshot.info.connected_at.elapsed().as_secs(),
        };
        match snapshot.info.direction {
            henyey_overlay::ConnectionDirection::Inbound => inbound.push(peer_info),
            henyey_overlay::ConnectionDirection::Outbound => outbound.push(peer_info),
        }
    }

    // Sort each group by peer ID for stable ordering
    inbound.sort_by(|a, b| a.id.cmp(&b.id));
    outbound.sort_by(|a, b| a.id.cmp(&b.id));

    let authenticated_count = inbound.len() + outbound.len();

    Json(PeersResponse {
        authenticated: PeersByDirection { inbound, outbound },
        authenticated_count,
        pending_count,
    })
}

pub(crate) async fn connect_handler(
    State(state): State<Arc<ServerState>>,
    Query(params): Query<ConnectParams>,
) -> impl IntoResponse {
    let addr = match parse_connect_params(&params) {
        Ok(addr) => addr,
        Err(message) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(SurveyCommandResponse {
                    success: false,
                    message,
                }),
            );
        }
    };

    match state.app.connect_peer(addr).await {
        Ok(peer_id) => (
            StatusCode::OK,
            Json(SurveyCommandResponse {
                success: true,
                message: format!("Connected to peer {}", peer_id),
            }),
        ),
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(SurveyCommandResponse {
                success: false,
                message: format!("Failed to connect: {}", err),
            }),
        ),
    }
}

pub(crate) async fn droppeer_handler(
    State(state): State<Arc<ServerState>>,
    Query(params): Query<DropPeerParams>,
) -> impl IntoResponse {
    let peer_id = match parse_peer_id_params(&params.peer_id, &params.node) {
        Ok(peer_id) => peer_id,
        Err(message) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(SurveyCommandResponse {
                    success: false,
                    message,
                }),
            );
        }
    };

    let ban_requested = params.ban.unwrap_or(0) == 1;
    if let Err(e) = state.app.disconnect_peer(&peer_id).await {
        (
            StatusCode::NOT_FOUND,
            Json(SurveyCommandResponse {
                success: false,
                message: format!("{e}"),
            }),
        )
    } else {
        if ban_requested {
            if let Err(err) = state.app.ban_peer(peer_id.clone()).await {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(SurveyCommandResponse {
                        success: false,
                        message: format!("Failed to ban peer: {}", err),
                    }),
                );
            }
        }
        let message = if ban_requested {
            format!("Disconnected and banned peer {}", peer_id)
        } else {
            format!("Disconnected peer {}", peer_id)
        };
        (
            StatusCode::OK,
            Json(SurveyCommandResponse {
                success: true,
                message,
            }),
        )
    }
}

pub(crate) async fn bans_handler(State(state): State<Arc<ServerState>>) -> Response {
    match state.app.banned_peers().await {
        Ok(bans) => {
            let bans = bans
                .into_iter()
                .filter_map(peer_id_to_strkey)
                .collect::<Vec<_>>();
            (StatusCode::OK, Json(BansResponse { bans })).into_response()
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(SurveyCommandResponse {
                success: false,
                message: format!("Failed to read bans: {}", err),
            }),
        )
            .into_response(),
    }
}

pub(crate) async fn unban_handler(
    State(state): State<Arc<ServerState>>,
    Query(params): Query<UnbanParams>,
) -> impl IntoResponse {
    let peer_id = match parse_peer_id_params(&params.peer_id, &params.node) {
        Ok(peer_id) => peer_id,
        Err(message) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(SurveyCommandResponse {
                    success: false,
                    message,
                }),
            );
        }
    };

    match state.app.unban_peer(&peer_id).await {
        Ok(removed) => {
            let message = if removed {
                format!("Unbanned peer {}", peer_id)
            } else {
                "Peer not found in ban list".to_string()
            };
            (
                StatusCode::OK,
                Json(SurveyCommandResponse {
                    success: removed,
                    message,
                }),
            )
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(SurveyCommandResponse {
                success: false,
                message: format!("Failed to unban peer: {}", err),
            }),
        ),
    }
}
