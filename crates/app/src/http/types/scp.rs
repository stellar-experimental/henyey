//! Types for SCP and quorum endpoints.

use serde::{Deserialize, Serialize};

/// Response for the /scp endpoint.
#[derive(Serialize)]
pub struct ScpInfoResponse {
    pub node: String,
    pub slots: Vec<ScpSlotInfo>,
}

/// Summary of SCP slot state.
#[derive(Serialize)]
pub struct ScpSlotInfo {
    pub slot_index: u64,
    pub is_externalized: bool,
    pub is_nominating: bool,
    pub fully_validated: Option<bool>,
    pub ballot_phase: String,
    pub nomination_round: u32,
    pub ballot_round: Option<u32>,
    pub envelope_count: usize,
}

/// Response for the /quorum endpoint.
#[derive(Serialize)]
pub struct QuorumResponse {
    pub local: Option<QuorumSetResponse>,
}

/// JSON representation of a quorum set.
#[derive(Serialize)]
pub struct QuorumSetResponse {
    pub hash: String,
    pub threshold: u32,
    pub validators: Vec<String>,
    pub inner_sets: Vec<QuorumSetResponse>,
}

/// Query parameters for the /scp endpoint.
#[derive(Deserialize)]
pub struct ScpParams {
    pub limit: Option<usize>,
}
