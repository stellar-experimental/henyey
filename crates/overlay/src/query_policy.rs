//! Centralized query rate-limiting policy.
//!
//! Defines which `StellarMessage` types are subject to per-peer rate limiting,
//! their per-kind limits, and the window computation. The overlay's per-peer
//! `QueryRateLimiter` (in `manager/peer_loop.rs`) is the sole runtime enforcer;
//! this module is the single source of truth for policy constants and classification.
//!
//! Parity: stellar-core Peer.cpp — `QueryInfo`, `process()` (1423-1438),
//! `GET_SCP_STATE_MAX_RATE` (61), `QUERY_RESPONSE_MULTIPLIER` (136).

use std::time::Duration;
use stellar_xdr::curr::StellarMessage;

/// Maximum number of ledger slots used for per-peer rate-limit windows.
/// Matches stellar-core's `Config::MAX_SLOTS_TO_REMEMBER` (default 12).
const MAX_SLOTS_TO_REMEMBER: u64 = 12;

/// Fixed max rate for GetScpState queries per window.
/// Matches stellar-core's `GET_SCP_STATE_MAX_RATE` (Peer.cpp:61).
const GET_SCP_STATE_MAX_RATE: u32 = 10;

/// Multiplier for computing queries-per-window from window duration in seconds.
/// Matches stellar-core's `QUERY_RESPONSE_MULTIPLIER` (Peer.cpp:136).
const QUERY_RESPONSE_MULTIPLIER: u32 = 5;

/// Rate-limited query message types.
///
/// Parity: stellar-core Peer.cpp uses separate `QueryInfo` instances for
/// GetTxSet, GetScpQuorumset, and GetScpState, each checked via
/// `Peer::process()` (Peer.cpp:1423-1438).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum QueryKind {
    TxSet,
    ScpQuorumSet,
    ScpState,
}

impl QueryKind {
    /// Classify a message as a rate-limited query, if applicable.
    pub(crate) fn classify(message: &StellarMessage) -> Option<Self> {
        match message {
            StellarMessage::GetTxSet(_) => Some(Self::TxSet),
            StellarMessage::GetScpQuorumset(_) => Some(Self::ScpQuorumSet),
            StellarMessage::GetScpState(_) => Some(Self::ScpState),
            _ => None,
        }
    }

    /// Per-kind max queries allowed in the given window.
    ///
    /// Encodes the complete rate-limit policy in one place:
    /// - `ScpState` → fixed `GET_SCP_STATE_MAX_RATE` (10)
    /// - Others → `window_secs * QUERY_RESPONSE_MULTIPLIER`
    ///
    /// Parity: stellar-core Peer.cpp:1423-1438, Peer.cpp:1686.
    pub(crate) fn max_queries(self, window: Duration) -> u32 {
        match self {
            Self::ScpState => GET_SCP_STATE_MAX_RATE,
            _ => window.as_secs() as u32 * QUERY_RESPONSE_MULTIPLIER,
        }
    }
}

/// Compute the query rate-limit window from the ledger close duration.
///
/// Parity: stellar-core Peer.cpp:1426-1429 — multiplies the millisecond
/// close time by `MAX_SLOTS_TO_REMEMBER`, then truncates to whole seconds
/// with `duration_cast<std::chrono::seconds>`. We replicate that exact
/// sequence: multiply in ms first, then integer-divide by 1000 to truncate.
pub fn query_rate_limit_window(close_duration: Duration) -> Duration {
    let total_ms = close_duration.as_millis() as u64 * MAX_SLOTS_TO_REMEMBER;
    Duration::from_secs(total_ms / 1000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{Hello, Uint256};

    #[test]
    fn classify_returns_correct_variants() {
        assert_eq!(
            QueryKind::classify(&StellarMessage::GetTxSet(Uint256([0; 32]))),
            Some(QueryKind::TxSet)
        );
        assert_eq!(
            QueryKind::classify(&StellarMessage::GetScpQuorumset(Uint256([0; 32]))),
            Some(QueryKind::ScpQuorumSet)
        );
        assert_eq!(
            QueryKind::classify(&StellarMessage::GetScpState(0)),
            Some(QueryKind::ScpState)
        );
    }

    #[test]
    fn classify_returns_none_for_non_query_messages() {
        let hello = StellarMessage::Hello(Hello {
            ledger_version: 0,
            overlay_version: 0,
            overlay_min_version: 0,
            network_id: stellar_xdr::curr::Hash([0; 32]),
            version_str: stellar_xdr::curr::StringM::default(),
            listening_port: 0,
            peer_id: stellar_xdr::curr::NodeId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
                Uint256([0; 32]),
            )),
            cert: stellar_xdr::curr::AuthCert {
                pubkey: stellar_xdr::curr::Curve25519Public { key: [0; 32] },
                expiration: 0,
                sig: stellar_xdr::curr::Signature::default(),
            },
            nonce: Uint256([0; 32]),
        });
        assert_eq!(QueryKind::classify(&hello), None);
    }

    #[test]
    fn max_queries_scp_state_is_fixed() {
        // ScpState always returns GET_SCP_STATE_MAX_RATE regardless of window
        assert_eq!(QueryKind::ScpState.max_queries(Duration::from_secs(0)), 10);
        assert_eq!(QueryKind::ScpState.max_queries(Duration::from_secs(60)), 10);
        assert_eq!(
            QueryKind::ScpState.max_queries(Duration::from_secs(1000)),
            10
        );
    }

    #[test]
    fn max_queries_others_scale_with_window() {
        // TxSet and ScpQuorumSet use window_secs * QUERY_RESPONSE_MULTIPLIER (5)
        assert_eq!(QueryKind::TxSet.max_queries(Duration::from_secs(60)), 300);
        assert_eq!(
            QueryKind::ScpQuorumSet.max_queries(Duration::from_secs(60)),
            300
        );
        assert_eq!(QueryKind::TxSet.max_queries(Duration::from_secs(0)), 0);
        assert_eq!(QueryKind::TxSet.max_queries(Duration::from_secs(1)), 5);
    }

    #[test]
    fn query_rate_limit_window_truncates_correctly() {
        // 5000ms * 12 = 60000ms = 60s
        assert_eq!(
            query_rate_limit_window(Duration::from_millis(5000)),
            Duration::from_secs(60)
        );
        // 7500ms * 12 = 90000ms = 90s
        assert_eq!(
            query_rate_limit_window(Duration::from_millis(7500)),
            Duration::from_secs(90)
        );
        // 5100ms * 12 = 61200ms = 61s (truncated)
        assert_eq!(
            query_rate_limit_window(Duration::from_millis(5100)),
            Duration::from_secs(61)
        );
        // 0ms → 0s
        assert_eq!(
            query_rate_limit_window(Duration::ZERO),
            Duration::from_secs(0)
        );
    }
}
