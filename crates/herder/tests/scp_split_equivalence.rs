//! Split-path equivalence divergence test for the Phase B SCP verify pipeline.
//!
//! Issue #1742 (Layer 2 of the proposal).
//!
//! Generates a batch of randomised SCP envelopes with a hand-rolled
//! deterministic RNG (no proptest dependency) and asserts, for every input,
//! that the **synchronous wrapper**
//!
//! ```text
//! Herder::receive_scp_envelope(env) -> EnvelopeState
//! ```
//!
//! produces the same `EnvelopeState` as the **explicit split composition**
//!
//! ```text
//! pre_filter_scp_envelope(env)
//!   -> verify_envelope_sync(network_id, pf)
//!   -> process_verified_detailed(ve)
//! ```
//!
//! # Scope
//!
//! This is *not* a consensus-safety proof. It only asserts divergence between
//! the two code paths that a Phase B refactor could accidentally introduce
//! (e.g., adding a new gate to the wrapper without porting it into
//! `process_verified`, or vice versa). Other tests cover the correctness of
//! the shared gates themselves.
//!
//! Each iteration constructs two identical herders — one for each path — so
//! state accumulated by the wrapper in iteration *i* does not pollute the
//! split path on the same envelope.

#![cfg(feature = "test-support")]

use std::sync::Arc;

use henyey_common::Hash256;
use henyey_crypto::SecretKey;
use henyey_herder::scp_verify::{self, PreFilter};
use henyey_herder::{EnvelopeState, Herder, HerderConfig};
use stellar_xdr::curr::{
    Hash as XdrHash, Limits, NodeId as XdrNodeId, PublicKey as XdrPublicKey, ScpBallot,
    ScpEnvelope, ScpNomination, ScpQuorumSet, ScpStatement, ScpStatementExternalize,
    ScpStatementPledges, ScpStatementPrepare, Signature as XdrSignature, StellarValue,
    StellarValueExt, TimePoint, Uint256, Value, WriteXdr,
};

// ---------------------------------------------------------------------------
// Hand-rolled xorshift64 RNG — deterministic, no external dep.
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.max(1))
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn range_u64(&mut self, lo: u64, hi: u64) -> u64 {
        debug_assert!(lo < hi);
        lo + self.next_u64() % (hi - lo)
    }
    fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[(self.next_u64() as usize) % xs.len()]
    }
    fn bool(&mut self) -> bool {
        self.next_u64() & 1 == 1
    }
}

// ---------------------------------------------------------------------------
// Shared config / fixture constants.
// ---------------------------------------------------------------------------

const NOW: u64 = 2_000_000_000;
const TRACKING_SLOT: u64 = 101;
const TRACKING_CLOSE_TIME: u64 = NOW - 5;
const NUM_ITERATIONS: usize = 200;

fn local_secret() -> SecretKey {
    SecretKey::from_seed(&[7u8; 32])
}
fn peer_secret() -> SecretKey {
    SecretKey::from_seed(&[9u8; 32])
}
fn stranger_secret() -> SecretKey {
    SecretKey::from_seed(&[42u8; 32])
}

fn node_id_of(sk: &SecretKey) -> XdrNodeId {
    XdrNodeId(XdrPublicKey::PublicKeyTypeEd25519(Uint256(
        *sk.public_key().as_bytes(),
    )))
}

fn build_herder(network_id: Hash256) -> Arc<Herder> {
    let local = local_secret();
    let peer_node_id = node_id_of(&peer_secret());
    let local_node_id = node_id_of(&local);
    let quorum_set = ScpQuorumSet {
        threshold: 2,
        validators: vec![local_node_id.clone(), peer_node_id]
            .try_into()
            .unwrap(),
        inner_sets: vec![].try_into().unwrap(),
    };
    let config = HerderConfig {
        is_validator: true,
        node_public_key: local.public_key(),
        network_id,
        local_quorum_set: Some(quorum_set.clone()),
        ..HerderConfig::default()
    };
    let herder = Arc::new(Herder::with_secret_key(config, local));
    herder.set_test_clock_seconds(NOW);
    herder.start_syncing();
    herder.bootstrap((TRACKING_SLOT - 1) as u32);
    herder.set_tracking_for_testing(TRACKING_SLOT, TRACKING_CLOSE_TIME);
    herder.set_pending_current_slot_for_testing(TRACKING_SLOT);
    herder
        .expand_quorum_tracker_for_testing(&local_node_id, quorum_set)
        .expect("expand quorum tracker");
    herder
}

// ---------------------------------------------------------------------------
// Envelope synthesis — exercises every branch a Phase B refactor might touch.
// ---------------------------------------------------------------------------

fn value_with_close_time(close_time: u64) -> Value {
    let sv = StellarValue {
        tx_set_hash: XdrHash([0u8; 32]),
        close_time: TimePoint(close_time),
        upgrades: vec![].try_into().unwrap(),
        ext: StellarValueExt::Basic,
    };
    Value(sv.to_xdr(Limits::none()).unwrap().try_into().unwrap())
}

fn sign(statement: ScpStatement, signer: &SecretKey, network_id: &Hash256) -> ScpEnvelope {
    let statement_bytes = statement.to_xdr(Limits::none()).unwrap();
    let mut data = network_id.0.to_vec();
    data.extend_from_slice(&1i32.to_be_bytes());
    data.extend_from_slice(&statement_bytes);
    let sig = signer.sign(&data);
    ScpEnvelope {
        statement,
        signature: XdrSignature(sig.as_bytes().to_vec().try_into().unwrap()),
    }
}

#[derive(Debug, Clone, Copy)]
enum Signer {
    Local,
    Peer,
    Stranger,
}

#[derive(Debug, Clone, Copy)]
enum Pledges {
    Nominate,
    Prepare,
    Externalize,
}

fn synthesize(rng: &mut Rng, network_id: &Hash256) -> (ScpEnvelope, bool) {
    // Pick each axis independently to cover the cross-product the two paths
    // must agree on.
    let signer = rng.pick(&[Signer::Local, Signer::Peer, Signer::Stranger]);
    let pledges = rng.pick(&[Pledges::Nominate, Pledges::Prepare, Pledges::Externalize]);
    // Slot coverage: below-range, in-range, current, above-range.
    let slot = rng.pick(&[
        1u64, // too old
        TRACKING_SLOT.saturating_sub(3),
        TRACKING_SLOT,
        TRACKING_SLOT + 1,
        TRACKING_SLOT + 10_000, // too new
    ]);
    // Close-time coverage: way past, just past, valid, too-future.
    let close_time_choice = rng.range_u64(0, 4);
    let close_time = match close_time_choice {
        0 => TRACKING_CLOSE_TIME.saturating_sub(500),
        1 => TRACKING_CLOSE_TIME.saturating_sub(1),
        2 => NOW,
        _ => NOW + 10_000,
    };
    let tamper_signature = rng.bool() && rng.bool(); // ~25% bad sigs

    let signer_sk = match signer {
        Signer::Local => local_secret(),
        Signer::Peer => peer_secret(),
        Signer::Stranger => stranger_secret(),
    };
    let node_id = node_id_of(&signer_sk);

    let pledges = match pledges {
        Pledges::Nominate => ScpStatementPledges::Nominate(ScpNomination {
            quorum_set_hash: XdrHash([0u8; 32]),
            votes: vec![value_with_close_time(close_time)].try_into().unwrap(),
            accepted: vec![].try_into().unwrap(),
        }),
        Pledges::Prepare => ScpStatementPledges::Prepare(ScpStatementPrepare {
            quorum_set_hash: XdrHash([0u8; 32]),
            ballot: ScpBallot {
                counter: 1,
                value: value_with_close_time(close_time),
            },
            prepared: None,
            prepared_prime: None,
            n_c: 0,
            n_h: 0,
        }),
        Pledges::Externalize => ScpStatementPledges::Externalize(ScpStatementExternalize {
            commit: ScpBallot {
                counter: 1,
                value: value_with_close_time(close_time),
            },
            n_h: 1,
            commit_quorum_set_hash: XdrHash([0u8; 32]),
        }),
    };

    let statement = ScpStatement {
        node_id,
        slot_index: slot,
        pledges,
    };
    let mut env = sign(statement, &signer_sk, network_id);
    if tamper_signature {
        let mut bytes = env.signature.0.to_vec();
        bytes[0] ^= 0xFF;
        env.signature = XdrSignature(bytes.try_into().unwrap());
    }
    (env, tamper_signature)
}

// ---------------------------------------------------------------------------
// The divergence test.
// ---------------------------------------------------------------------------

/// Run the wrapper path on a fresh herder and return its outcome.
fn run_wrapper(network_id: Hash256, envelope: ScpEnvelope) -> EnvelopeState {
    let herder = build_herder(network_id);
    herder.receive_scp_envelope(envelope)
}

/// Run the explicit split path on a fresh herder and return its outcome.
fn run_split(network_id: Hash256, envelope: ScpEnvelope) -> EnvelopeState {
    let herder = build_herder(network_id);
    let pf = herder.pre_filter_scp_envelope(&envelope);
    match scp_verify::verify_envelope_sync(&network_id, pf) {
        Err(reason) => {
            // Pre-filter rejection — mirror the wrapper's mapping.
            use scp_verify::PreFilterRejectReason as R;
            match reason {
                R::Range => EnvelopeState::TooOld,
                R::CannotReceiveScp | R::CloseTime => EnvelopeState::Invalid,
            }
        }
        Ok(ve) => herder.process_verified_detailed(ve).0,
    }
}

#[test]
fn split_path_matches_wrapper_over_randomised_inputs() {
    let network_id = Hash256::from_bytes([0xA5; 32]);
    let mut rng = Rng::new(0xDEAD_BEEF_CAFE_F00D);

    let mut divergences: Vec<String> = Vec::new();
    for i in 0..NUM_ITERATIONS {
        let (env, tampered) = synthesize(&mut rng, &network_id);
        let slot = env.statement.slot_index;
        let wrapper = run_wrapper(network_id, env.clone());
        let split = run_split(network_id, env);
        if wrapper != split {
            divergences.push(format!(
                "iter {i}: slot={slot} tampered_sig={tampered} wrapper={:?} split={:?}",
                wrapper, split
            ));
        }
    }

    assert!(
        divergences.is_empty(),
        "wrapper ↔ split divergence detected in {}/{} inputs:\n{}",
        divergences.len(),
        NUM_ITERATIONS,
        divergences.join("\n")
    );
}

/// Sanity check: confirm the two pre-filter return paths ARE exercised by
/// the randomised generator (so the test doesn't silently become vacuous if
/// somebody narrows the input space).
#[test]
fn generator_covers_accept_and_reject_prefilter() {
    let network_id = Hash256::from_bytes([0xA5; 32]);
    let mut rng = Rng::new(0x12345);
    let herder = build_herder(network_id);
    let mut saw_accept = false;
    let mut saw_reject = false;
    for _ in 0..NUM_ITERATIONS {
        let (env, _) = synthesize(&mut rng, &network_id);
        match herder.pre_filter_scp_envelope(&env) {
            PreFilter::Accept(_) => saw_accept = true,
            PreFilter::Reject(_) => saw_reject = true,
        }
    }
    assert!(
        saw_accept && saw_reject,
        "generator failed to cover both pre-filter branches (accept={}, reject={})",
        saw_accept,
        saw_reject
    );
}
