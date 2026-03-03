//! Handler for /tx (transaction submission).

use std::sync::Arc;

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json},
};
use henyey_common::NetworkId;
use henyey_tx::TransactionFrame;

use super::super::types::{SubmitTxRequest, SubmitTxResponse, TxStatus};
use super::super::ServerState;

pub(crate) async fn submit_tx_handler(
    State(state): State<Arc<ServerState>>,
    Json(request): Json<SubmitTxRequest>,
) -> impl IntoResponse {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use stellar_xdr::curr::{Limits, ReadXdr, TransactionEnvelope};

    // Decode and validate the transaction
    let tx_bytes = match STANDARD.decode(&request.tx) {
        Ok(bytes) => bytes,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(SubmitTxResponse {
                    status: TxStatus::Error,
                    hash: None,
                    error: Some(format!("Invalid base64: {}", e)),
                }),
            );
        }
    };

    // Parse the transaction envelope
    let tx_env = match TransactionEnvelope::from_xdr(&tx_bytes, Limits::none()) {
        Ok(env) => env,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(SubmitTxResponse {
                    status: TxStatus::Error,
                    hash: None,
                    error: Some(format!("Invalid XDR: {}", e)),
                }),
            );
        }
    };

    let network_id = NetworkId::from_passphrase(&state.app.info().network_passphrase);
    let mut frame = TransactionFrame::with_network(tx_env.clone(), network_id);
    let hash = frame.compute_hash(&network_id).ok();
    let result = state.app.submit_transaction(tx_env);

    let (status, error) = match result {
        henyey_herder::TxQueueResult::Added => (TxStatus::Pending, None),
        henyey_herder::TxQueueResult::Duplicate => (TxStatus::Duplicate, None),
        henyey_herder::TxQueueResult::QueueFull => (TxStatus::TryAgainLater, None),
        henyey_herder::TxQueueResult::FeeTooLow => {
            (TxStatus::Error, Some("txInsufficientFee".to_string()))
        }
        henyey_herder::TxQueueResult::Invalid(code) => {
            let error_str = code.map_or("txInternalError".to_string(), |c| c.name().to_string());
            (TxStatus::Error, Some(error_str))
        }
        henyey_herder::TxQueueResult::Banned => {
            (TxStatus::Error, Some("txBadAuth".to_string()))
        }
        henyey_herder::TxQueueResult::Filtered => (TxStatus::Filtered, None),
        henyey_herder::TxQueueResult::TryAgainLater => (TxStatus::TryAgainLater, None),
    };

    (
        StatusCode::OK,
        Json(SubmitTxResponse {
            status,
            hash: hash.map(|value| value.to_hex()),
            error,
        }),
    )
}
