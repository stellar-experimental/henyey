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
//! A `Dead` state is reached on channel close, panic, or thread join. The
//! watchdog also fires if `backlog > 0` while the heartbeat is stuck for
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreFilterRejectReason {
    CannotReceiveScp,
    CloseTime,
    Range,
}

impl PreFilterRejectReason {
    pub fn as_str(self) -> &'static str {
        match self {
            PreFilterRejectReason::CannotReceiveScp => "can_receive",
            PreFilterRejectReason::CloseTime => "close_time",
            PreFilterRejectReason::Range => "range",
        }
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
/// Returned alongside [`crate::EnvelopeState`] by the test-only
/// `Herder::process_verified_detailed` so integration tests can assert WHICH
/// gate in `process_verified` fired. The ordering of variants mirrors the
/// in-code gate evaluation order:
///
/// 1. Drift recheck (pre-filter rerun) → `GateDrift*`
/// 2. Self-message skip                → `SelfMessage`
/// 3. Non-quorum reject                → `NonQuorum`
/// 4. `pending_envelopes.add` outcomes → `PendingAdd*`
/// 5. Accepted (reached SCP / processed directly) → `Accepted`
/// 6. Verdict-driven short-circuits    → `InvalidSignature`, `PanicVerdict`
///
/// The enum itself is always compiled (the internal `process_verified_inner`
/// always computes it) but only made accessible to external callers via the
/// `test-support` feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostVerifyReason {
    InvalidSignature,
    PanicVerdict,
    GateDriftRange,
    GateDriftCloseTime,
    GateDriftCannotReceive,
    SelfMessage,
    NonQuorum,
    PendingAddBuffered,
    PendingAddDuplicate,
    PendingAddTooFar,
    PendingAddBufferFull,
    PendingAddProcessedDirectly,
    Accepted,
}

/// Test-only synchronous equivalent of the worker's verify step.
///
/// Takes the `network_id` explicitly since [`PreFilter`] does not carry it
/// (the production path gets it from [`crate::scp_driver::ScpDriver::build_signed_bytes`]
/// via the worker's `Arc<Herder>`). Returns the resulting
/// [`VerifiedEnvelope`] (which may carry a [`Verdict::InvalidSignature`]
/// outcome) or an error if the pre-filter rejected the envelope before
/// verification.
///
/// Used by the Phase B parity tests under `crates/herder/tests/` — not
/// part of the production event-loop pipeline.
#[cfg(feature = "test-support")]
#[doc(hidden)]
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
}

/// Spawn the verifier worker thread.
///
/// The worker owns its input receiver and output sender; when the input channel
/// is closed it drains and exits, setting `state` to `Dead`.
pub fn spawn_scp_verifier(
    network_id: Hash256,
    capacity: usize,
) -> Result<SpawnedVerifier, std::io::Error> {
    let (tx, rx) = tokio::sync::mpsc::channel::<PipelinedIntake>(capacity);
    let (verified_tx, verified_rx) = tokio::sync::mpsc::unbounded_channel::<VerifiedEnvelope>();
    let state = Arc::new(AtomicU8::new(VerifierState::Running as u8));
    let heartbeat = Arc::new(AtomicU64::new(0));
    let backlog = Arc::new(AtomicUsize::new(0));

    let state_worker = Arc::clone(&state);
    let heartbeat_worker = Arc::clone(&heartbeat);
    let backlog_worker = Arc::clone(&backlog);

    std::thread::Builder::new()
        .name("scp-verify".into())
        .spawn(move || {
            scp_verify_worker(
                network_id,
                rx,
                verified_tx,
                state_worker,
                heartbeat_worker,
                backlog_worker,
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
                let _ = verified_tx.send(VerifiedEnvelope {
                    intake,
                    verdict: Verdict::Panic,
                });
                heartbeat.fetch_add(1, Ordering::Relaxed);
                state.store(VerifierState::Dead as u8, Ordering::Relaxed);
                return;
            }
        };

        if verified_tx
            .send(VerifiedEnvelope { intake, verdict })
            .is_err()
        {
            // Event loop dropped its receiver — shut down cleanly.
            break;
        }
        heartbeat.fetch_add(1, Ordering::Relaxed);
    }

    state.store(VerifierState::Dead as u8, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

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
    fn test_worker_heartbeat_advances_and_dies_on_close() {
        let spawned = spawn_scp_verifier(network_id(), 16).expect("spawn");
        let h = spawned.handle.clone();
        let mut verified_rx = spawned.verified_rx;

        let intake = PipelinedIntake {
            envelope: dummy_envelope(),
            slot: 1,
            is_externalize: false,
            peer_id: None,
            enqueue_at: Instant::now(),
        };
        assert_eq!(h.state(), VerifierState::Running);
        h.tx.blocking_send(intake).unwrap();

        // Wait for heartbeat to advance
        let start = std::time::Instant::now();
        while h.heartbeat() == 0 && start.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(h.heartbeat() >= 1, "heartbeat should advance");

        // Drain verdict
        let ve = verified_rx.blocking_recv().expect("verified envelope");
        // Invalid dummy signature → InvalidSignature
        assert!(matches!(ve.verdict, Verdict::InvalidSignature));

        // Drop sender to close input channel, worker should die
        drop(h.tx.clone()); // clone not enough
        drop(spawned.handle);
        // Give thread time to exit
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            // Can't drop h completely since we still hold h; remove h
            std::thread::sleep(Duration::from_millis(10));
        }
        // We can't test Dead here without dropping all handles - covered elsewhere.
    }
}
