//! SCP envelope signature verification worker.
//!
//! Phase B of issue #1734: moves the Ed25519 verification (~50–100µs per envelope)
//! and the XDR serialization of the signed bytes off the tokio event loop onto a
//! dedicated `std::thread`. The event loop dispatches pre-filtered intakes to this
//! worker via a bounded channel and consumes verified results on another channel.
//!
//! # Pipeline
//!
//! ```text
//! pre_filter -> verifier_tx (bounded, 2000)
//!                 │
//!                 ▼
//!             std::thread "scp-verify"
//!                 │ XDR-serialise + Ed25519 verify
//!                 ▼
//!             verified_tx -> verified_rx (unbounded, event loop)
//! ```
//!
//! # Liveness
//!
//! The worker exposes three atomics used by the event-loop watchdog:
//! - `state`: `Running` / `Stopping` / `Dead`
//! - `heartbeat`: incremented every iteration (even for signature rejects)
//! - `backlog`: `verifier_rx.len()` sampled on each dequeue
//!
//! The worker transitions to `Dead` and returns on any of:
//! - **Input channel close**: all `Sender` halves dropped → `blocking_recv()` returns `None`
//! - **Output receiver close**: `verified_tx.send()` returns `Err` → loop breaks
//! - **Caught panic**: `catch_unwind` fires → emits `Verdict::Panic` → returns early
//!
//! [`SpawnedVerifier::join_handle`] can be used to deterministically wait for
//! the worker thread to exit (e.g. in tests).
//!
//! The watchdog also fires if `backlog > 0` while the heartbeat is stuck for
//! several ticks.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use henyey_common::Hash256;
use stellar_xdr::curr::ScpEnvelope;

use crate::error::HerderError;
use crate::scp_driver::ScpDriver;

/// Default capacity of the verifier input channel.
///
/// Chosen to smooth SCP bursts: a 200-envelope burst from 24 validators fits
/// comfortably without blocking `pump_scp_intake.reserve().await`.
pub const DEFAULT_VERIFIER_QUEUE_CAPACITY: usize = 2000;

/// Event-loop-side verification state.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifierState {
    Running = 0,
    Stopping = 1,
    Dead = 2,
}

impl VerifierState {
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => VerifierState::Running,
            1 => VerifierState::Stopping,
            _ => VerifierState::Dead,
        }
    }
}

/// Reason a pre-filter rejected an envelope.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreFilterRejectReason {
    CannotReceiveScp = 0,
    CloseTime = 1,
    Range = 2,
}

impl PreFilterRejectReason {
    /// All variants in discriminant order. This is the single source of truth
    /// for iteration, counter allocation, and Prometheus label generation.
    pub const ALL: [Self; 3] = [Self::CannotReceiveScp, Self::CloseTime, Self::Range];

    /// Prometheus metric label for this reason.
    pub const fn label(&self) -> &'static str {
        match self {
            Self::CannotReceiveScp => "cannot_receive",
            Self::CloseTime => "close_time",
            Self::Range => "range",
        }
    }
}

// Compile-time: ALL is complete, ordered, and covers every discriminant.
const _: () = {
    let mut i = 0;
    while i < PreFilterRejectReason::ALL.len() {
        assert!(PreFilterRejectReason::ALL[i] as usize == i);
        i += 1;
    }
    assert!(PreFilterRejectReason::ALL.len() == PreFilterRejectReason::Range as usize + 1);
};

/// Fixed-size counter array indexed by [`PreFilterRejectReason`].
///
/// Wraps `[T; N]` so callers never perform raw ordinal arithmetic.
/// Used for both the live `AtomicU64` counters in `App` and the
/// plain `u64` snapshot in `ScpVerifyMetrics`.
#[derive(Debug)]
pub struct PreFilterCounters<T>([T; PreFilterRejectReason::ALL.len()]);

impl<T: Default> Default for PreFilterCounters<T> {
    fn default() -> Self {
        Self(std::array::from_fn(|_| T::default()))
    }
}

impl<T: Clone> Clone for PreFilterCounters<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T: Copy> Copy for PreFilterCounters<T> {}

impl<T> PreFilterCounters<T> {
    /// Build from a function that maps each reason to a value.
    pub fn from_fn(mut f: impl FnMut(PreFilterRejectReason) -> T) -> Self {
        Self(std::array::from_fn(|i| f(PreFilterRejectReason::ALL[i])))
    }

    /// Iterate `(reason, &value)` pairs in discriminant order.
    pub fn iter(&self) -> impl Iterator<Item = (PreFilterRejectReason, &T)> {
        PreFilterRejectReason::ALL
            .iter()
            .map(|&r| (r, &self.0[r as usize]))
    }
}

impl<T> std::ops::Index<PreFilterRejectReason> for PreFilterCounters<T> {
    type Output = T;
    fn index(&self, reason: PreFilterRejectReason) -> &T {
        &self.0[reason as usize]
    }
}

impl<T> std::ops::IndexMut<PreFilterRejectReason> for PreFilterCounters<T> {
    fn index_mut(&mut self, reason: PreFilterRejectReason) -> &mut T {
        &mut self.0[reason as usize]
    }
}

/// Envelope intake payload carried from the event loop across the verifier
/// channel. Contains everything `process_verified` needs downstream.
#[derive(Debug)]
pub struct PipelinedIntake {
    pub envelope: ScpEnvelope,
    pub slot: u64,
    pub is_externalize: bool,
    /// Peer that delivered this envelope (for metrics and fetch routing).
    pub peer_id: Option<henyey_overlay::PeerId>,
    /// Enqueue timestamp for latency histograms.
    pub enqueue_at: Instant,
    /// BLAKE2 hash of the full `StellarMessage::ScpMessage(envelope)`.
    /// FloodGate key for `forget_flooded_msg` calls on discard.
    /// Set by `pump_scp_intake`; `None` for non-overlay paths (catchup, tests).
    pub flood_msg_hash: Option<henyey_common::Hash256>,
}

/// Result of the pre-filter stage that runs on the event loop.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum PreFilter {
    Accept(PipelinedIntake),
    Reject(PreFilterRejectReason),
}

/// Outcome of the worker's cryptographic verification.
#[derive(Debug)]
pub enum Verdict {
    Ok,
    InvalidSignature,
    /// The worker panicked while processing this envelope (caught and reported
    /// as a rejection; the worker itself then terminates).
    Panic,
}

/// A verified (or rejected) envelope emitted by the worker.
#[derive(Debug)]
pub struct VerifiedEnvelope {
    pub intake: PipelinedIntake,
    pub verdict: Verdict,
}

/// Attribution for a post-verify-stage gate outcome.
///
/// Returned alongside [`crate::EnvelopeState`] by
/// [`crate::Herder::process_verified`] so callers can attribute outcomes to
/// specific gates for metrics and diagnostics. The ordering of variants
/// mirrors the in-code gate evaluation order:
///
/// 1. Verdict-driven short-circuits    → `InvalidSignature`, `PanicVerdict`
/// 2. Drift recheck (pre-filter rerun) → `GateDrift*`
/// 3. Self-message skip                → `SelfMessage`
/// 4. Non-quorum reject                → `NonQuorum`
/// 5. `pending_envelopes.add` outcomes → `PendingAdd*`
/// 6. Accepted (reached SCP / processed directly) → `Accepted`
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostVerifyReason {
    InvalidSignature = 0,
    PanicVerdict = 1,
    GateDriftRange = 2,
    GateDriftCloseTime = 3,
    GateDriftCannotReceive = 4,
    SelfMessage = 5,
    NonQuorum = 6,
    PendingAddBuffered = 7,
    PendingAddDuplicate = 8,
    PendingAddTooFar = 9,
    PendingAddBufferFull = 10,
    PendingAddProcessedDirectly = 11,
    Accepted = 12,
}

impl PostVerifyReason {
    /// All variants in discriminant order. This is the single source of truth
    /// for iteration, counter allocation, and Prometheus label generation.
    pub const ALL: [Self; 13] = [
        Self::InvalidSignature,
        Self::PanicVerdict,
        Self::GateDriftRange,
        Self::GateDriftCloseTime,
        Self::GateDriftCannotReceive,
        Self::SelfMessage,
        Self::NonQuorum,
        Self::PendingAddBuffered,
        Self::PendingAddDuplicate,
        Self::PendingAddTooFar,
        Self::PendingAddBufferFull,
        Self::PendingAddProcessedDirectly,
        Self::Accepted,
    ];

    /// Prometheus metric label for this reason.
    pub const fn label(&self) -> &'static str {
        match self {
            Self::InvalidSignature => "invalid_sig",
            Self::PanicVerdict => "panic",
            Self::GateDriftRange => "drift_range",
            Self::GateDriftCloseTime => "drift_close_time",
            Self::GateDriftCannotReceive => "drift_cannot_receive",
            Self::SelfMessage => "self_message",
            Self::NonQuorum => "non_quorum",
            Self::PendingAddBuffered => "buffered",
            Self::PendingAddDuplicate => "duplicate",
            Self::PendingAddTooFar => "too_far",
            Self::PendingAddBufferFull => "buffer_full",
            Self::PendingAddProcessedDirectly => "processed_directly",
            Self::Accepted => "accepted",
        }
    }
}

// Compile-time: ALL is complete, ordered, and covers every discriminant.
const _: () = {
    let mut i = 0;
    while i < PostVerifyReason::ALL.len() {
        assert!(PostVerifyReason::ALL[i] as usize == i);
        i += 1;
    }
    // Last discriminant + 1 == ALL.len() — catches a variant added but not in ALL.
    assert!(PostVerifyReason::ALL.len() == PostVerifyReason::Accepted as usize + 1);
};

/// Fixed-size counter array indexed by [`PostVerifyReason`].
///
/// Wraps `[T; N]` so callers never perform raw ordinal arithmetic.
/// Used for both the live `AtomicU64` counters in `App` and the
/// plain `u64` snapshot in `ScpVerifyMetrics`.
#[derive(Debug)]
pub struct PostVerifyCounters<T>([T; PostVerifyReason::ALL.len()]);

impl<T: Default> Default for PostVerifyCounters<T> {
    fn default() -> Self {
        Self(std::array::from_fn(|_| T::default()))
    }
}

impl<T: Clone> Clone for PostVerifyCounters<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T: Copy> Copy for PostVerifyCounters<T> {}

impl<T> PostVerifyCounters<T> {
    /// Build from a function that maps each reason to a value.
    pub fn from_fn(mut f: impl FnMut(PostVerifyReason) -> T) -> Self {
        Self(std::array::from_fn(|i| f(PostVerifyReason::ALL[i])))
    }

    /// Iterate `(reason, &value)` pairs in discriminant order.
    pub fn iter(&self) -> impl Iterator<Item = (PostVerifyReason, &T)> {
        PostVerifyReason::ALL
            .iter()
            .map(|&r| (r, &self.0[r as usize]))
    }
}

impl<T> std::ops::Index<PostVerifyReason> for PostVerifyCounters<T> {
    type Output = T;
    fn index(&self, reason: PostVerifyReason) -> &T {
        &self.0[reason as usize]
    }
}

impl<T> std::ops::IndexMut<PostVerifyReason> for PostVerifyCounters<T> {
    fn index_mut(&mut self, reason: PostVerifyReason) -> &mut T {
        &mut self.0[reason as usize]
    }
}

/// Synchronous equivalent of the worker's verify step.
///
/// Takes the `network_id` explicitly since [`PreFilter`] does not carry it
/// (the production path gets it from [`crate::scp_driver::ScpDriver::build_signed_bytes`]
/// via the worker's `Arc<Herder>`). Returns the resulting
/// [`VerifiedEnvelope`] (which may carry a [`Verdict::InvalidSignature`]
/// outcome) or an error if the pre-filter rejected the envelope before
/// verification.
///
/// Used by the Phase B parity tests under `crates/herder/tests/` and by
/// the synchronous catchup path — not part of the async event-loop pipeline.
pub fn verify_envelope_sync(
    network_id: &Hash256,
    pf: PreFilter,
) -> Result<VerifiedEnvelope, PreFilterRejectReason> {
    use crate::scp_driver::ScpDriver;
    match pf {
        PreFilter::Reject(reason) => Err(reason),
        PreFilter::Accept(intake) => {
            let verdict = match ScpDriver::build_signed_bytes(network_id, &intake.envelope) {
                Ok(signed_bytes) => {
                    match ScpDriver::verify_signed_bytes(
                        &signed_bytes,
                        &intake.envelope.statement.node_id,
                        &intake.envelope.signature,
                    ) {
                        Ok(()) => Verdict::Ok,
                        Err(_) => Verdict::InvalidSignature,
                    }
                }
                Err(_) => Verdict::InvalidSignature,
            };
            Ok(VerifiedEnvelope { intake, verdict })
        }
    }
}

/// Shared handle used by the event loop to enqueue envelopes and by the
/// watchdog to monitor liveness.
#[derive(Clone)]
pub struct SignatureVerifierHandle {
    pub tx: tokio::sync::mpsc::Sender<PipelinedIntake>,
    pub state: Arc<AtomicU8>,
    pub heartbeat: Arc<AtomicU64>,
    pub backlog: Arc<AtomicUsize>,
}

impl SignatureVerifierHandle {
    pub fn state(&self) -> VerifierState {
        VerifierState::from_u8(self.state.load(Ordering::Relaxed))
    }

    pub fn heartbeat(&self) -> u64 {
        self.heartbeat.load(Ordering::Relaxed)
    }

    pub fn backlog(&self) -> usize {
        self.backlog.load(Ordering::Relaxed)
    }

    /// Currently-used slots in the verifier input channel (approx queue depth).
    pub fn queue_len(&self) -> usize {
        self.tx.max_capacity() - self.tx.capacity()
    }
}

/// Outputs returned from [`spawn_scp_verifier`].
pub struct SpawnedVerifier {
    pub handle: SignatureVerifierHandle,
    pub verified_rx: tokio::sync::mpsc::UnboundedReceiver<VerifiedEnvelope>,
    /// Worker thread join handle. `join()` blocks until the worker thread
    /// returns (after setting `state` to [`VerifierState::Dead`]).
    ///
    /// **Precondition**: the worker only returns when its input channel
    /// closes, its output send fails, or it catches a panic. Calling
    /// `join()` while input senders are still alive and no panic has
    /// occurred will block indefinitely.
    pub join_handle: std::thread::JoinHandle<()>,
}

/// Spawn the verifier worker thread.
///
/// The worker owns its input receiver and output sender; when the input channel
/// is closed it drains and exits, setting `state` to `Dead`. The returned
/// [`SpawnedVerifier::join_handle`] can be used to wait for the thread to exit.
pub fn spawn_scp_verifier(
    network_id: Hash256,
    capacity: usize,
    scp_metrics: Arc<crate::metrics::ScpMetrics>,
) -> Result<SpawnedVerifier, std::io::Error> {
    let (tx, rx) = tokio::sync::mpsc::channel::<PipelinedIntake>(capacity);
    let (verified_tx, verified_rx) = tokio::sync::mpsc::unbounded_channel::<VerifiedEnvelope>();
    let state = Arc::new(AtomicU8::new(VerifierState::Running as u8));
    let heartbeat = Arc::new(AtomicU64::new(0));
    let backlog = Arc::new(AtomicUsize::new(0));

    let state_worker = Arc::clone(&state);
    let heartbeat_worker = Arc::clone(&heartbeat);
    let backlog_worker = Arc::clone(&backlog);

    let join_handle = std::thread::Builder::new()
        .name("scp-verify".into())
        .spawn(move || {
            scp_verify_worker(
                network_id,
                rx,
                verified_tx,
                state_worker,
                heartbeat_worker,
                backlog_worker,
                scp_metrics,
            );
        })?;

    Ok(SpawnedVerifier {
        handle: SignatureVerifierHandle {
            tx,
            state,
            heartbeat,
            backlog,
        },
        verified_rx,
        join_handle,
    })
}

/// Worker body. XDR-serialises the signed payload and verifies the Ed25519
/// signature for each incoming envelope, emitting [`VerifiedEnvelope`] results
/// on `verified_tx`. Both stages run here (off the event loop).
pub(crate) fn scp_verify_worker(
    network_id: Hash256,
    mut rx: tokio::sync::mpsc::Receiver<PipelinedIntake>,
    verified_tx: tokio::sync::mpsc::UnboundedSender<VerifiedEnvelope>,
    state: Arc<AtomicU8>,
    heartbeat: Arc<AtomicU64>,
    backlog: Arc<AtomicUsize>,
    scp_metrics: Arc<crate::metrics::ScpMetrics>,
) {
    // `blocking_recv` parks the std::thread until a new intake arrives. This is
    // appropriate because we are OUTSIDE the tokio runtime.
    while let Some(intake) = rx.blocking_recv() {
        backlog.store(rx.len(), Ordering::Relaxed);

        let verdict = match catch_unwind(AssertUnwindSafe(|| {
            // Test-only deterministic panic trigger. Used by
            // `test_worker_panics_marks_dead` to assert the catch_unwind +
            // Dead-state path without relying on real undefined behavior.
            #[cfg(test)]
            if intake.slot == u64::MAX - 1 {
                panic!("test panic trigger");
            }
            let signed_bytes = ScpDriver::build_signed_bytes(&network_id, &intake.envelope)?;
            ScpDriver::verify_signed_bytes(
                &signed_bytes,
                &intake.envelope.statement.node_id,
                &intake.envelope.signature,
            )
        })) {
            Ok(Ok(())) => Verdict::Ok,
            Ok(Err(HerderError::Scp(henyey_scp::ScpError::SignatureVerificationFailed))) => {
                Verdict::InvalidSignature
            }
            Ok(Err(_)) => Verdict::InvalidSignature,
            Err(_payload) => {
                tracing::error!("scp-verify worker caught panic during verification");
                // Emit a Panic verdict for this envelope so the event loop can
                // account for it, then terminate the worker.
                // Increment heartbeat before send so that recv() on the other
                // end guarantees the heartbeat is visible (sequenced-before on
                // this thread + channel synchronization = happens-before).
                heartbeat.fetch_add(1, Ordering::Relaxed);
                let _ = verified_tx.send(VerifiedEnvelope {
                    intake,
                    verdict: Verdict::Panic,
                });
                state.store(VerifierState::Dead as u8, Ordering::Relaxed);
                return;
            }
        };

        match verdict {
            Verdict::Ok => scp_metrics.inc_envelope_validsig(),
            Verdict::InvalidSignature => scp_metrics.inc_envelope_invalidsig(),
            Verdict::Panic => {} // unreachable here (handled above)
        }

        heartbeat.fetch_add(1, Ordering::Relaxed);
        if verified_tx
            .send(VerifiedEnvelope { intake, verdict })
            .is_err()
        {
            // Event loop dropped its receiver — shut down cleanly.
            break;
        }
    }

    state.store(VerifierState::Dead as u8, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn network_id() -> Hash256 {
        Hash256::from_bytes([7u8; 32])
    }

    fn dummy_envelope() -> ScpEnvelope {
        use stellar_xdr::curr::{
            NodeId, PublicKey as XdrPublicKey, ScpBallot, ScpStatement, ScpStatementPledges,
            ScpStatementPrepare, Signature, Uint256, Value,
        };

        let node_id = NodeId(XdrPublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32])));
        let value = Value(vec![].try_into().unwrap());
        let pledges = ScpStatementPledges::Prepare(ScpStatementPrepare {
            quorum_set_hash: stellar_xdr::curr::Hash([0u8; 32]),
            ballot: ScpBallot {
                counter: 1,
                value: value.clone(),
            },
            prepared: None,
            prepared_prime: None,
            n_c: 0,
            n_h: 0,
        });
        let statement = ScpStatement {
            node_id,
            slot_index: 1,
            pledges,
        };
        ScpEnvelope {
            statement,
            signature: Signature(vec![0u8; 64].try_into().unwrap()),
        }
    }

    #[test]
    fn test_worker_heartbeat_advances() {
        let spawned = spawn_scp_verifier(
            network_id(),
            16,
            Arc::new(crate::metrics::ScpMetrics::new()),
        )
        .expect("spawn");
        let h = spawned.handle.clone();
        let mut verified_rx = spawned.verified_rx;

        let intake = PipelinedIntake {
            envelope: dummy_envelope(),
            slot: 1,
            is_externalize: false,
            peer_id: None,
            enqueue_at: Instant::now(),
            flood_msg_hash: None,
        };
        assert_eq!(h.state(), VerifierState::Running);
        h.tx.blocking_send(intake).unwrap();

        // Drain verdict — heartbeat is incremented before send in the worker,
        // so receiving a verdict deterministically proves heartbeat advanced.
        let ve = verified_rx.blocking_recv().expect("verified envelope");
        assert!(
            h.heartbeat() >= 1,
            "heartbeat should advance after processing"
        );
        assert!(matches!(ve.verdict, Verdict::InvalidSignature));
    }
}
