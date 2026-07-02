//! Herder state machine for consensus participation.
//!
//! This module defines the state machine that governs how the Herder participates
//! in network consensus. The Herder progresses through distinct states as it
//! synchronizes with the network and begins tracking or validating ledgers.
//!
//! # State Transitions
//!
//! ```text
//! Booting -> Syncing -> Tracking
//! ```
//!
//! - **Booting**: Initial state after startup, not connected to the network
//! - **Syncing**: Catching up to the network via history archives
//! - **Tracking**: Synchronized and following consensus in real-time

use std::fmt;

/// The state of the Herder with respect to network consensus.
///
/// The Herder transitions through these states as it synchronizes with the network
/// and begins participating in consensus. Each state determines which operations
/// are permitted (e.g., receiving transactions, processing SCP envelopes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum HerderState {
    /// Initial state after startup.
    /// The node is booting up and hasn't yet connected to the network.
    #[default]
    Booting,

    /// Syncing state during catchup.
    /// The node is catching up to the current network state by downloading
    /// ledgers from history archives or peers.
    Syncing,

    /// Tracking state after catchup completes.
    /// The node is synchronized with the network and tracking consensus,
    /// receiving and processing SCP messages from validators.
    Tracking,
}

impl HerderState {
    /// Check if the herder is in a state where it can receive SCP envelopes.
    pub fn can_receive_scp(&self) -> bool {
        matches!(self, HerderState::Syncing | HerderState::Tracking)
    }

    /// Check if the herder is in a state where it can receive transactions.
    ///
    /// Includes `Syncing` (sustain fix): stellar-core's
    /// `HerderImpl::recvTransaction` never gates on tracking state — it always
    /// forwards to the queue. henyey previously required `Tracking`, so a
    /// momentary Tracking→Syncing blip (e.g. a slow slot or connection churn)
    /// made in-flight submissions fail with `TryAgainLater`; for
    /// `PAY_PREGENERATED` load generation ANY non-Added result aborts the run
    /// (parity #3638), which capped sustained-load tests ~20% below the burst
    /// ceiling. Only `Booting` still defers: the ledger manager may not be
    /// initialized yet, and the queue would reject with `txINTERNAL_ERROR`
    /// (aborting well-behaved clients such as friendbot) instead of a
    /// retryable signal.
    pub fn can_receive_transactions(&self) -> bool {
        matches!(self, HerderState::Syncing | HerderState::Tracking)
    }

    /// Check if the herder is fully synchronized with the network.
    pub fn is_tracking(&self) -> bool {
        matches!(self, HerderState::Tracking)
    }

    /// Check if the herder is still booting up.
    pub fn is_booting(&self) -> bool {
        matches!(self, HerderState::Booting)
    }

    /// Check if the herder is in catchup/syncing mode.
    pub fn is_syncing(&self) -> bool {
        matches!(self, HerderState::Syncing)
    }

    /// Get the next expected state in the normal progression.
    pub fn next_state(&self) -> Option<HerderState> {
        match self {
            HerderState::Booting => Some(HerderState::Syncing),
            HerderState::Syncing => Some(HerderState::Tracking),
            HerderState::Tracking => None,
        }
    }

    /// Check if a transition from the current state to `new_state` is allowed.
    ///
    /// Spec: HERDER_SPEC §4 — forbidden transitions:
    /// - TRACKING → BOOTING
    /// - SYNCING → BOOTING
    pub fn can_transition_to(&self, new_state: HerderState) -> bool {
        match (self, new_state) {
            (HerderState::Tracking, HerderState::Booting) => false,
            (HerderState::Syncing, HerderState::Booting) => false,
            _ => true,
        }
    }
}

impl fmt::Display for HerderState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HerderState::Booting => write!(f, "Booting"),
            HerderState::Syncing => write!(f, "Syncing"),
            HerderState::Tracking => write!(f, "Tracking"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_state_transitions() {
        let state = HerderState::default();
        assert!(state.is_booting());
        assert!(!state.can_receive_scp());
        assert!(!state.can_receive_transactions());

        let syncing = state.next_state().unwrap();
        assert!(syncing.is_syncing());
        assert!(syncing.can_receive_scp());
        // Sustain fix: Syncing forwards transactions to the queue (core
        // parity — recvTransaction never gates on tracking state).
        assert!(syncing.can_receive_transactions());

        let tracking = syncing.next_state().unwrap();
        assert!(tracking.is_tracking());
        assert!(tracking.can_receive_scp());
        assert!(tracking.can_receive_transactions());

        assert!(tracking.next_state().is_none());
    }

    #[test]
    fn test_state_display() {
        assert_eq!(HerderState::Booting.to_string(), "Booting");
        assert_eq!(HerderState::Syncing.to_string(), "Syncing");
        assert_eq!(HerderState::Tracking.to_string(), "Tracking");
    }

    /// Exhaustive 3x3 matrix test for can_transition_to per HERDER_SPEC §3.2.
    #[test]
    fn test_can_transition_to_exhaustive() {
        use HerderState::*;
        let states = [Booting, Syncing, Tracking];

        // (from, to, expected)
        let expected = [
            // From Booting
            (Booting, Booting, true),
            (Booting, Syncing, true),
            (Booting, Tracking, true),
            // From Syncing
            (Syncing, Booting, false), // forbidden
            (Syncing, Syncing, true),
            (Syncing, Tracking, true),
            // From Tracking
            (Tracking, Booting, false), // forbidden
            (Tracking, Syncing, true),
            (Tracking, Tracking, true),
        ];

        for (from, to, ok) in &expected {
            assert_eq!(
                from.can_transition_to(*to),
                *ok,
                "can_transition_to({from} -> {to}) should be {ok}"
            );
        }

        // Double-check: exactly 2 forbidden transitions out of 9
        let forbidden_count: usize = states
            .iter()
            .flat_map(|from| states.iter().map(move |to| from.can_transition_to(*to)))
            .filter(|allowed| !allowed)
            .count();
        assert_eq!(
            forbidden_count, 2,
            "exactly 2 transitions should be forbidden"
        );
    }
}
