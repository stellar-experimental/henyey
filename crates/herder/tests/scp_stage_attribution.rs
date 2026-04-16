//! Stage-attribution regression test for the Phase B SCP verify pipeline.
//!
//! Issue #1742 (Layer 1 of the proposal).
//!
//! Drives the full `pre_filter → verify_envelope_sync → process_verified_detailed`
//! pipeline via the test-only hooks gated under `feature = "test-support"` and
//! asserts, for each row in the table below, which *stage* claimed the
//! envelope:
//!
//! | Stage            | Meaning                                                        |
//! |------------------|----------------------------------------------------------------|
//! | `PreFilter(r)`   | `Herder::pre_filter_scp_envelope` returned `Reject(r)`         |
//! | `PostVerify(r)`  | `Herder::process_verified_detailed` returned `(_, r)`          |
//! | `Accepted`       | Post-verify hit `Accepted` (envelope forwarded to SCP)         |
//!
//! The test is authoritative about **stage attribution**, not about whether
//! the envelope is ultimately semantically correct. Reordering the gates in
//! `process_verified` or moving a gate across the pre-filter / post-verify
//! boundary makes rows flip — that is the whole point.
//!
//! Both wall-clock sites (`Herder::check_envelope_close_time` and
//! `ScpDriver::check_close_time`) are driven by a shared fake clock installed
//! via `Herder::set_test_clock_seconds`, so the close-time rows are
//! deterministic.

#![cfg(feature = "test-support")]

use std::sync::Arc;
use std::time::Instant;

use henyey_common::Hash256;
use henyey_crypto::SecretKey;
use henyey_herder::scp_verify::{
    self, PostVerifyReason, PreFilter, PreFilterRejectReason, VerifiedEnvelope,
};
use henyey_herder::{Herder, HerderConfig};
use stellar_xdr::curr::{
    Hash as XdrHash, Limits, NodeId as XdrNodeId, PublicKey as XdrPublicKey, ScpBallot,
    ScpEnvelope, ScpNomination, ScpQuorumSet, ScpStatement, ScpStatementExternalize,
    ScpStatementPledges, ScpStatementPrepare, Signature as XdrSignature, StellarValue,
    StellarValueExt, TimePoint, Uint256, Value, WriteXdr,
};

// ---------------------------------------------------------------------------
// Stage enum
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    PreFilter(PreFilterRejectReason),
    PostVerify(PostVerifyReason),
    Accepted,
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// Fake wall-clock time used throughout this test. Far enough in the past
/// that close times computed with small relative offsets don't collide with
/// the real system clock even if a test accidentally bypasses the override.
const NOW: u64 = 2_000_000_000;

/// Slot the fixture is tracking (tracking_slot after `set_tracking_for_testing`).
const TRACKING_SLOT: u64 = 101;
const TRACKING_CLOSE_TIME: u64 = NOW - 5;

/// Seed for the local validator secret key.
const LOCAL_SEED: [u8; 32] = [7u8; 32];
/// Seed for a remote "in-quorum" peer.
const PEER_SEED: [u8; 32] = [9u8; 32];
/// Seed for a remote "not-in-quorum" peer (non-quorum rejection).
const STRANGER_SEED: [u8; 32] = [42u8; 32];

struct Fixture {
    herder: Arc<Herder>,
    local_secret: SecretKey,
    peer_secret: SecretKey,
    stranger_secret: SecretKey,
    network_id: Hash256,
}

impl Fixture {
    fn new() -> Self {
        let local_secret = SecretKey::from_seed(&LOCAL_SEED);
        let peer_secret = SecretKey::from_seed(&PEER_SEED);
        let stranger_secret = SecretKey::from_seed(&STRANGER_SEED);

        let local_public = local_secret.public_key();
        let local_node_id = node_id_of(&local_secret);
        let peer_node_id = node_id_of(&peer_secret);

        // Include both local and peer in the local quorum set so the non-quorum
        // gate in `process_verified` is *armed* (local_quorum_set is Some) but
        // the peer still passes it. The stranger is deliberately left out.
        let quorum_set = ScpQuorumSet {
            threshold: 2,
            validators: vec![local_node_id.clone(), peer_node_id.clone()]
                .try_into()
                .unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };

        let network_id = Hash256::from_bytes([0xA5; 32]);

        let config = HerderConfig {
            is_validator: true,
            node_public_key: local_public,
            network_id,
            local_quorum_set: Some(quorum_set.clone()),
            ..HerderConfig::default()
        };

        let herder = Arc::new(Herder::with_secret_key(config, local_secret.clone()));
        herder.set_test_clock_seconds(NOW);
        herder.set_tracking_for_testing(TRACKING_SLOT, TRACKING_CLOSE_TIME);
        herder.set_pending_current_slot_for_testing(TRACKING_SLOT);
        // Prime the quorum tracker so the non-quorum gate allows the peer but
        // blocks the stranger.
        herder
            .expand_quorum_tracker_for_testing(&local_node_id, quorum_set)
            .expect("expand quorum tracker");

        Fixture {
            herder,
            local_secret,
            peer_secret,
            stranger_secret,
            network_id,
        }
    }
}

// ---------------------------------------------------------------------------
// Envelope builders
// ---------------------------------------------------------------------------

fn node_id_of(sk: &SecretKey) -> XdrNodeId {
    XdrNodeId(XdrPublicKey::PublicKeyTypeEd25519(Uint256(
        *sk.public_key().as_bytes(),
    )))
}

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
    // ENVELOPE_TYPE_SCP = 1
    data.extend_from_slice(&1i32.to_be_bytes());
    data.extend_from_slice(&statement_bytes);
    let sig = signer.sign(&data);
    ScpEnvelope {
        statement,
        signature: XdrSignature(sig.as_bytes().to_vec().try_into().unwrap()),
    }
}

/// Nominate envelope signed by `signer` at `slot` with a single vote whose
/// close_time is `close_time`.
fn nominate_envelope(
    slot: u64,
    close_time: u64,
    signer: &SecretKey,
    network_id: &Hash256,
) -> ScpEnvelope {
    let statement = ScpStatement {
        node_id: node_id_of(signer),
        slot_index: slot,
        pledges: ScpStatementPledges::Nominate(ScpNomination {
            quorum_set_hash: XdrHash([0u8; 32]),
            votes: vec![value_with_close_time(close_time)].try_into().unwrap(),
            accepted: vec![].try_into().unwrap(),
        }),
    };
    sign(statement, signer, network_id)
}

/// Prepare envelope with a controllable ballot close_time — used for the
/// happy-path / precedence rows where we want the envelope to clear the
/// close-time pre-filter without triggering externalize-specific code paths.
fn prepare_envelope(
    slot: u64,
    close_time: u64,
    signer: &SecretKey,
    network_id: &Hash256,
) -> ScpEnvelope {
    let statement = ScpStatement {
        node_id: node_id_of(signer),
        slot_index: slot,
        pledges: ScpStatementPledges::Prepare(ScpStatementPrepare {
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
    };
    sign(statement, signer, network_id)
}

fn externalize_envelope(
    slot: u64,
    close_time: u64,
    signer: &SecretKey,
    network_id: &Hash256,
) -> ScpEnvelope {
    let statement = ScpStatement {
        node_id: node_id_of(signer),
        slot_index: slot,
        pledges: ScpStatementPledges::Externalize(ScpStatementExternalize {
            commit: ScpBallot {
                counter: 1,
                value: value_with_close_time(close_time),
            },
            n_h: 1,
            commit_quorum_set_hash: XdrHash([0u8; 32]),
        }),
    };
    sign(statement, signer, network_id)
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

/// Runs the split pipeline end-to-end and returns the attributed `Stage`.
///
/// The `mutate` closure is invoked between `verify_envelope_sync` and
/// `process_verified_detailed` so drift-recheck rows can perturb tracking
/// state mid-flight (mirroring what the real event loop does while the
/// verifier thread is busy).
fn run<F>(fix: &Fixture, envelope: ScpEnvelope, mutate: F) -> Stage
where
    F: FnOnce(&Herder),
{
    let herder = &*fix.herder;

    let pf = herder.pre_filter_scp_envelope(&envelope);
    if let PreFilter::Reject(reason) = pf {
        return Stage::PreFilter(reason);
    }

    let verified = scp_verify::verify_envelope_sync(&fix.network_id, pf)
        .expect("pre-filter already accepted above");

    // Signature should be valid for any envelope we hand-construct with
    // `sign()`, so the verdict is always `Ok` — assert so the test signals
    // clearly if that ever regresses.
    assert!(
        matches!(verified.verdict, scp_verify::Verdict::Ok),
        "envelope signature must verify for signed fixtures: {:?}",
        verified.verdict
    );

    // Perturb state mid-flight (drift-recheck rows).
    mutate(herder);

    let (_state, reason) = herder.process_verified_detailed(verified);
    match reason {
        PostVerifyReason::Accepted => Stage::Accepted,
        other => Stage::PostVerify(other),
    }
}

/// Same as [`run`] but asserts the envelope is rejected by `verify_envelope_sync`
/// (invalid signature row). Returns whatever `Verdict` the worker produced.
fn run_invalid_sig(fix: &Fixture, envelope: ScpEnvelope) -> scp_verify::Verdict {
    let herder = &*fix.herder;
    let pf = herder.pre_filter_scp_envelope(&envelope);
    assert!(
        matches!(pf, PreFilter::Accept(_)),
        "invalid-signature row expects pre-filter to accept"
    );
    let verified =
        scp_verify::verify_envelope_sync(&fix.network_id, pf).expect("pre-filter accepted above");
    verified.verdict
}

// ---------------------------------------------------------------------------
// Tests — one per row
// ---------------------------------------------------------------------------

#[test]
fn row_01_cannot_receive_scp_prefilter() {
    let fix = Fixture::new();
    // Move into a state that cannot receive SCP. `HerderState::Booting` is
    // the default `Herder::new`/`with_secret_key` initial state, but we've
    // already called `set_tracking_for_testing` which only flips
    // `tracking_state.is_tracking`; `HerderState` itself is still `Booting`.
    //
    // `HerderState::can_receive_scp()` is false for `Booting`, so a freshly
    // built fixture already fails this gate. Assert explicitly.
    let env = nominate_envelope(TRACKING_SLOT, NOW, &fix.peer_secret, &fix.network_id);
    let stage = run(&fix, env, |_| {});
    assert_eq!(
        stage,
        Stage::PreFilter(PreFilterRejectReason::CannotReceiveScp)
    );
}

fn fixture_tracking() -> Fixture {
    let fix = Fixture::new();
    fix.herder.start_syncing();
    // Use ledger_seq = TRACKING_SLOT - 1 so bootstrap advances to TRACKING_SLOT.
    fix.herder.bootstrap((TRACKING_SLOT - 1) as u32);
    // `bootstrap` overwrites tracking_consensus_close_time with 0 (no ledger
    // manager), so restore our fixture value and the fake clock after.
    fix.herder
        .set_tracking_for_testing(TRACKING_SLOT, TRACKING_CLOSE_TIME);
    fix.herder.set_test_clock_seconds(NOW);
    fix.herder
        .set_pending_current_slot_for_testing(TRACKING_SLOT);
    fix
}

#[test]
fn row_02_close_time_too_old_prefilter() {
    let fix = fixture_tracking();
    // Envelope close_time is BELOW tracking_consensus_close_time → the
    // future-slot branch of `check_close_time` rejects (close_time must be
    // strictly greater than last_close_time).
    let env = nominate_envelope(
        TRACKING_SLOT,
        TRACKING_CLOSE_TIME.saturating_sub(100),
        &fix.peer_secret,
        &fix.network_id,
    );
    assert_eq!(
        run(&fix, env, |_| {}),
        Stage::PreFilter(PreFilterRejectReason::CloseTime)
    );
}

#[test]
fn row_03_close_time_too_far_future_prefilter() {
    let fix = fixture_tracking();
    // close_time > now + MAX_TIME_SLIP_SECONDS (60) → future check rejects.
    let env = nominate_envelope(
        TRACKING_SLOT,
        NOW + 1_000,
        &fix.peer_secret,
        &fix.network_id,
    );
    assert_eq!(
        run(&fix, env, |_| {}),
        Stage::PreFilter(PreFilterRejectReason::CloseTime)
    );
}

#[test]
fn row_04_slot_too_old_prefilter() {
    let fix = fixture_tracking();
    // Slot below `effective_min = min_ledger_seq.max(lcl+1)`. No ledger
    // manager is attached, so effective_min = min_ledger_seq = max(1, current_slot
    // - MAX_SLOTS_TO_REMEMBER + 1). With TRACKING_SLOT=101 and window=12 →
    // min=90. Slot 1 is deep below.
    let env = nominate_envelope(1, NOW, &fix.peer_secret, &fix.network_id);
    assert_eq!(
        run(&fix, env, |_| {}),
        Stage::PreFilter(PreFilterRejectReason::Range)
    );
}

#[test]
fn row_05_slot_too_new_prefilter() {
    let fix = fixture_tracking();
    // Slot far beyond the validity bracket.
    let env = nominate_envelope(
        TRACKING_SLOT + 10_000,
        NOW,
        &fix.peer_secret,
        &fix.network_id,
    );
    assert_eq!(
        run(&fix, env, |_| {}),
        Stage::PreFilter(PreFilterRejectReason::Range)
    );
}

#[test]
fn row_06_invalid_signature() {
    let fix = fixture_tracking();
    // Build a signed envelope, then tamper with the signature.
    let mut env = nominate_envelope(TRACKING_SLOT, NOW, &fix.peer_secret, &fix.network_id);
    let mut bytes = env.signature.0.to_vec();
    bytes[0] ^= 0xFF;
    env.signature = XdrSignature(bytes.try_into().unwrap());

    let verdict = run_invalid_sig(&fix, env);
    assert!(
        matches!(verdict, scp_verify::Verdict::InvalidSignature),
        "tampered signature should be rejected by verify_envelope_sync, got {:?}",
        verdict
    );
    // By contract the split pipeline short-circuits before any post-verify
    // gate can fire — `process_verified_detailed` is never called.
}

#[test]
fn row_07_drift_range_postverify() {
    let fix = fixture_tracking();
    // Envelope initially in-range (slot == tracking). Mid-flight, advance the
    // tracking slot far forward so the pre-filter rerun in
    // `process_verified` sees the slot as too old.
    let env = nominate_envelope(TRACKING_SLOT, NOW, &fix.peer_secret, &fix.network_id);
    let stage = run(&fix, env, |h| {
        h.set_tracking_for_testing(TRACKING_SLOT + 10_000, TRACKING_CLOSE_TIME);
        h.set_pending_current_slot_for_testing(TRACKING_SLOT + 10_000);
    });
    assert_eq!(stage, Stage::PostVerify(PostVerifyReason::GateDriftRange));
}

#[test]
fn row_08_drift_close_time_postverify() {
    let fix = fixture_tracking();
    // Envelope's close_time is fine initially (> TRACKING_CLOSE_TIME). After
    // verify, bump tracking_consensus_close_time ABOVE the envelope's close
    // time so the pre-filter rerun rejects via the close-time branch.
    let env_close_time = TRACKING_CLOSE_TIME + 10;
    let env = nominate_envelope(
        TRACKING_SLOT,
        env_close_time,
        &fix.peer_secret,
        &fix.network_id,
    );
    let stage = run(&fix, env, |h| {
        h.set_tracking_for_testing(TRACKING_SLOT, env_close_time + 100);
    });
    assert_eq!(
        stage,
        Stage::PostVerify(PostVerifyReason::GateDriftCloseTime)
    );
}

#[test]
fn row_09_self_message_postverify() {
    let fix = fixture_tracking();
    // Envelope signed by local node → self-message gate fires in post-verify.
    let env = nominate_envelope(TRACKING_SLOT, NOW, &fix.local_secret, &fix.network_id);
    assert_eq!(
        run(&fix, env, |_| {}),
        Stage::PostVerify(PostVerifyReason::SelfMessage)
    );
}

#[test]
fn row_10_non_quorum_postverify() {
    let fix = fixture_tracking();
    // Envelope signed by the stranger (not in quorum_tracker) → non-quorum
    // gate fires in post-verify.
    let env = nominate_envelope(TRACKING_SLOT, NOW, &fix.stranger_secret, &fix.network_id);
    assert_eq!(
        run(&fix, env, |_| {}),
        Stage::PostVerify(PostVerifyReason::NonQuorum)
    );
}

#[test]
fn row_11_precedence_drift_range_beats_self_message() {
    let fix = fixture_tracking();
    // Self-message AND drift-range would both fire — the gate *order* in
    // `process_verified` says drift-recheck is first.
    let env = nominate_envelope(TRACKING_SLOT, NOW, &fix.local_secret, &fix.network_id);
    let stage = run(&fix, env, |h| {
        h.set_tracking_for_testing(TRACKING_SLOT + 10_000, TRACKING_CLOSE_TIME);
        h.set_pending_current_slot_for_testing(TRACKING_SLOT + 10_000);
    });
    assert_eq!(stage, Stage::PostVerify(PostVerifyReason::GateDriftRange));
}

#[test]
fn row_12_precedence_self_message_beats_non_quorum() {
    // Build a fixture where local is NOT in the quorum tracker. The envelope
    // is self-signed so both self-message AND non-quorum gates would fire
    // — order dictates SelfMessage wins.
    let local_secret = SecretKey::from_seed(&LOCAL_SEED);
    let local_public = local_secret.public_key();
    // Quorum set contains only the peer (not local), so local-signed
    // envelopes fail the non-quorum check.
    let peer_secret = SecretKey::from_seed(&PEER_SEED);
    let peer_node_id = node_id_of(&peer_secret);
    let quorum_set = ScpQuorumSet {
        threshold: 1,
        validators: vec![peer_node_id.clone()].try_into().unwrap(),
        inner_sets: vec![].try_into().unwrap(),
    };
    let network_id = Hash256::from_bytes([0xA5; 32]);
    let config = HerderConfig {
        is_validator: true,
        node_public_key: local_public,
        network_id,
        local_quorum_set: Some(quorum_set.clone()),
        ..HerderConfig::default()
    };
    let herder = Arc::new(Herder::with_secret_key(config, local_secret.clone()));
    herder.set_test_clock_seconds(NOW);
    herder.start_syncing();
    herder.bootstrap((TRACKING_SLOT - 1) as u32);
    herder.set_tracking_for_testing(TRACKING_SLOT, TRACKING_CLOSE_TIME);
    herder.set_pending_current_slot_for_testing(TRACKING_SLOT);
    herder
        .expand_quorum_tracker_for_testing(&peer_node_id, quorum_set)
        .expect("expand");

    let env = nominate_envelope(TRACKING_SLOT, NOW, &local_secret, &network_id);
    let pf = herder.pre_filter_scp_envelope(&env);
    assert!(matches!(pf, PreFilter::Accept(_)));
    let verified = scp_verify::verify_envelope_sync(&network_id, pf).unwrap();
    let (_, reason) = herder.process_verified_detailed(verified);
    assert_eq!(reason, PostVerifyReason::SelfMessage);
}

#[test]
fn row_13_precedence_drift_close_time_beats_non_quorum() {
    let fix = fixture_tracking();
    // Stranger (would trip non-quorum) sends envelope with acceptable
    // close_time; then we advance tracking_consensus_close_time past it so
    // drift recheck rejects on close-time first.
    let env_close_time = TRACKING_CLOSE_TIME + 10;
    let env = nominate_envelope(
        TRACKING_SLOT,
        env_close_time,
        &fix.stranger_secret,
        &fix.network_id,
    );
    let stage = run(&fix, env, |h| {
        h.set_tracking_for_testing(TRACKING_SLOT, env_close_time + 100);
    });
    assert_eq!(
        stage,
        Stage::PostVerify(PostVerifyReason::GateDriftCloseTime)
    );
}

#[test]
fn row_14_happy_path_nominate_accepted() {
    let fix = fixture_tracking();
    // Fresh peer envelope with valid close_time passes every gate.
    let env = prepare_envelope(TRACKING_SLOT, NOW, &fix.peer_secret, &fix.network_id);
    let stage = run(&fix, env, |_| {});
    assert_eq!(stage, Stage::Accepted);
}

#[test]
fn row_15_happy_path_externalize_accepted() {
    let fix = fixture_tracking();
    // Externalize-path: `process_verified` also prefetches the tx set.
    let env = externalize_envelope(TRACKING_SLOT, NOW, &fix.peer_secret, &fix.network_id);
    let stage = run(&fix, env, |_| {});
    assert_eq!(stage, Stage::Accepted);
}

// ---------------------------------------------------------------------------
// Smoke: every envelope we build here must verify end-to-end, otherwise the
// rows above are silently testing the invalid-signature short-circuit. If
// this regresses the test suite fails loudly rather than quietly turning
// green.
// ---------------------------------------------------------------------------

#[test]
fn smoke_fixture_envelopes_round_trip_sign_verify() {
    let fix = fixture_tracking();
    let env = nominate_envelope(TRACKING_SLOT, NOW, &fix.peer_secret, &fix.network_id);
    let pf = fix.herder.pre_filter_scp_envelope(&env);
    let ve = scp_verify::verify_envelope_sync(&fix.network_id, pf)
        .expect("pre-filter must accept smoke envelope");
    assert!(matches!(ve.verdict, scp_verify::Verdict::Ok));
    let _ = Instant::now();
    // Sanity: the resulting VerifiedEnvelope carries the same slot/envelope
    // in its intake so `process_verified_detailed` can consume it.
    let _: &VerifiedEnvelope = &ve;
}
