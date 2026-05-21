//! Integration tests for the SCP envelope relay path through FetchingEnvelopes.
//!
//! Verifies that the broadcast callback (wired via `set_fetching_broadcast`)
//! fires correctly when envelopes pass through `Herder::process_verified()`:
//!
//! 1. Peer envelopes trigger broadcast exactly once when deps are satisfied
//! 2. Immediate-ready envelopes fire broadcast during recv_envelope_validated
//! 3. Deferred-ready envelopes fire broadcast when deps arrive
//! 4. Self-messages are rejected before reaching FetchingEnvelopes (no broadcast)
//! 5. Closing-gate replay does NOT re-trigger broadcast
//! 6. Duplicate envelopes do NOT trigger additional broadcast
//!
//! Scope: peer-originated, post-verify, FetchingEnvelopes-driven relay path only.
//! Locally-generated envelopes and out-of-sync recovery broadcast are separate
//! paths not covered here.
//!
//! Parity: mirrors stellar-core's `PendingEnvelopes::envelopeReady()` which
//! broadcasts once `isFullyFetched()` is true (PendingEnvelopes.cpp:545-562).
//!
//! Related: #2329 (fix), #2336 (this issue), AUDIT-258

#![cfg(feature = "test-support")]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use henyey_common::Hash256;
use henyey_crypto::SecretKey;
use henyey_herder::scp_verify::PostVerifyReason;
use henyey_herder::{EnvelopeState, Herder, HerderConfig, TimerManagerHandle};
use henyey_ledger::{LedgerManager, LedgerManagerConfig};
use stellar_xdr::curr::{
    Hash as XdrHash, LedgerCloseValueSignature, Limits, NodeId as XdrNodeId,
    PublicKey as XdrPublicKey, ScpEnvelope, ScpNomination, ScpQuorumSet, ScpStatement,
    ScpStatementPledges, Signature as XdrSignature, StellarValue, StellarValueExt, TimePoint,
    Uint256, Value, WriteXdr,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Fake wall-clock time for all tests.
const NOW: u64 = 2_000_000_000;
/// Slot the fixture tracks.
const TRACKING_SLOT: u64 = 101;
const TRACKING_CLOSE_TIME: u64 = NOW - 5;

const LOCAL_SEED: [u8; 32] = [7u8; 32];
const PEER_SEED: [u8; 32] = [9u8; 32];

// ---------------------------------------------------------------------------
// LedgerManager helper
// ---------------------------------------------------------------------------

fn make_default_lm() -> Arc<LedgerManager> {
    use stellar_xdr::curr::{
        Hash, LedgerHeader, LedgerHeaderExt, StellarValue as XdrStellarValue,
        StellarValueExt as XdrStellarValueExt, TimePoint as XdrTimePoint, VecM,
    };
    let config = LedgerManagerConfig {
        validate_bucket_hash: false,
        ..Default::default()
    };
    let lm = LedgerManager::new("Test Network".to_string(), config);
    let header = LedgerHeader {
        ledger_version: 24,
        previous_ledger_hash: Hash([0u8; 32]),
        scp_value: XdrStellarValue {
            tx_set_hash: Hash([0u8; 32]),
            close_time: XdrTimePoint(100),
            upgrades: VecM::default(),
            ext: XdrStellarValueExt::Basic,
        },
        tx_set_result_hash: Hash([0u8; 32]),
        bucket_list_hash: Hash([0u8; 32]),
        ledger_seq: 1,
        total_coins: 1_000_000_000_000,
        fee_pool: 0,
        inflation_seq: 0,
        id_pool: 0,
        base_fee: 100,
        base_reserve: 5_000_000,
        max_tx_set_size: 100,
        skip_list: [
            Hash([0u8; 32]),
            Hash([0u8; 32]),
            Hash([0u8; 32]),
            Hash([0u8; 32]),
        ],
        ext: LedgerHeaderExt::V0,
    };
    let header_hash = henyey_ledger::compute_header_hash(&header).expect("hash");
    lm.initialize(
        henyey_bucket::BucketList::new(),
        henyey_bucket::HotArchiveBucketList::new(),
        header,
        header_hash,
    )
    .expect("init");
    Arc::new(lm)
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

struct Fixture {
    herder: Arc<Herder>,
    local_secret: SecretKey,
    peer_secret: SecretKey,
    network_id: Hash256,
    broadcast_count: Arc<AtomicU64>,
    /// Hash of the local quorum set (pre-cached in FetchingEnvelopes at construction)
    local_qs_hash: XdrHash,
    /// Hash of a pre-cached empty tx_set (satisfies tx_set dependency check)
    cached_tx_set_hash: XdrHash,
}

impl Fixture {
    fn new() -> Self {
        let local_secret = SecretKey::from_seed(&LOCAL_SEED);
        let peer_secret = SecretKey::from_seed(&PEER_SEED);

        let local_public = local_secret.public_key();
        let local_node_id = node_id_of(&local_secret);
        let peer_node_id = node_id_of(&peer_secret);

        let quorum_set = ScpQuorumSet {
            threshold: 2,
            validators: vec![local_node_id.clone(), peer_node_id.clone()]
                .try_into()
                .unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };

        let qs_hash = Hash256::hash_xdr(&quorum_set);
        let local_qs_hash = XdrHash(qs_hash.0);

        let network_id = Hash256::from_bytes([0xA5; 32]);

        let config = HerderConfig {
            is_validator: true,
            node_public_key: local_public,
            network_id,
            local_quorum_set: Some(quorum_set.clone()),
            ..HerderConfig::default()
        };

        let lm = make_default_lm();
        let herder = Arc::new(Herder::with_secret_key(
            config,
            local_secret.clone(),
            lm,
            TimerManagerHandle::no_op(),
        ));

        // Transition to a state that can receive SCP envelopes.
        herder.start_syncing();
        herder.bootstrap((TRACKING_SLOT - 1) as u32);
        herder.set_tracking_for_testing(TRACKING_SLOT, TRACKING_CLOSE_TIME);
        herder.set_test_clock_seconds(NOW);
        herder.set_pending_current_slot_for_testing(TRACKING_SLOT);

        // Prime the quorum tracker so peer passes the non-quorum gate.
        herder
            .expand_quorum_tracker_for_testing(&local_node_id, quorum_set)
            .expect("expand quorum tracker");

        // Cache an empty tx_set so envelopes referencing it pass the tx_set
        // dependency check in FetchingEnvelopes::check_dependencies().
        let tx_set = henyey_herder::TransactionSet::new(Hash256::from_bytes([0u8; 32]), Vec::new());
        let cached_tx_set_hash = XdrHash(tx_set.hash().0);
        herder.scp_driver().cache_tx_set(tx_set);

        // Wire broadcast callback.
        let broadcast_count = Arc::new(AtomicU64::new(0));
        let count_clone = broadcast_count.clone();
        herder.set_fetching_broadcast(move |_env| {
            count_clone.fetch_add(1, Ordering::SeqCst);
        });

        Fixture {
            herder,
            local_secret,
            peer_secret,
            network_id,
            broadcast_count,
            local_qs_hash,
            cached_tx_set_hash,
        }
    }

    fn broadcast_count(&self) -> u64 {
        self.broadcast_count.load(Ordering::SeqCst)
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

fn value_with_close_time(close_time: u64, tx_set_hash: &XdrHash) -> Value {
    let sv = StellarValue {
        tx_set_hash: tx_set_hash.clone(),
        close_time: TimePoint(close_time),
        upgrades: vec![].try_into().unwrap(),
        ext: StellarValueExt::Signed(LedgerCloseValueSignature {
            node_id: XdrNodeId(XdrPublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32]))),
            signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
        }),
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

/// Create a nominate envelope signed by `signer` at `slot` using `qs_hash`.
/// Includes a vote with the given `tx_set_hash` so the close-time pre-filter
/// passes and the tx_set dependency is satisfied (assuming the tx_set is cached).
fn nominate_envelope_with_qs(
    slot: u64,
    close_time: u64,
    signer: &SecretKey,
    network_id: &Hash256,
    qs_hash: &XdrHash,
    tx_set_hash: &XdrHash,
) -> ScpEnvelope {
    let statement = ScpStatement {
        node_id: node_id_of(signer),
        slot_index: slot,
        pledges: ScpStatementPledges::Nominate(ScpNomination {
            quorum_set_hash: qs_hash.clone(),
            votes: vec![value_with_close_time(close_time, tx_set_hash)]
                .try_into()
                .unwrap(),
            accepted: vec![].try_into().unwrap(),
        }),
    };
    sign(statement, signer, network_id)
}

// ---------------------------------------------------------------------------
// Pipeline driver
// ---------------------------------------------------------------------------

/// Run the full pre_filter → verify → process_verified pipeline.
/// Returns (EnvelopeState, PostVerifyReason).
fn run_pipeline(fix: &Fixture, envelope: ScpEnvelope) -> (EnvelopeState, PostVerifyReason) {
    fix.herder.receive_scp_envelope_detailed(envelope)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Peer envelope with cached quorum set fires broadcast exactly once.
#[test]
fn test_relay_fires_for_peer_envelope_immediate_ready() {
    let fix = Fixture::new();

    // Envelope uses the local quorum set hash (pre-cached at Herder construction).
    let env = nominate_envelope_with_qs(
        TRACKING_SLOT,
        NOW,
        &fix.peer_secret,
        &fix.network_id,
        &fix.local_qs_hash,
        &fix.cached_tx_set_hash,
    );

    let (state, reason) = run_pipeline(&fix, env);

    assert_eq!(reason, PostVerifyReason::Accepted);
    // SCP returns Valid (not ValidNew) because nomination hasn't started locally
    // for this slot. Herder maps Valid → Duplicate.
    assert_eq!(state, EnvelopeState::Duplicate);
    assert_eq!(fix.broadcast_count(), 1, "broadcast must fire exactly once");
}

/// Envelope with missing quorum set: no broadcast until deps arrive.
#[test]
fn test_relay_fires_for_deferred_ready_quorum_set() {
    let fix = Fixture::new();

    // Use a quorum set hash NOT cached in FetchingEnvelopes.
    let unknown_qs_hash = XdrHash([0xBB; 32]);
    let env = nominate_envelope_with_qs(
        TRACKING_SLOT,
        NOW,
        &fix.peer_secret,
        &fix.network_id,
        &unknown_qs_hash,
        &fix.cached_tx_set_hash,
    );

    let (state, reason) = run_pipeline(&fix, env);

    assert_eq!(reason, PostVerifyReason::Accepted);
    assert_eq!(state, EnvelopeState::Fetching);
    assert_eq!(fix.broadcast_count(), 0, "no broadcast while deps missing");

    // Deliver the quorum set via recv_quorum_set.
    let unknown_qs = ScpQuorumSet {
        threshold: 1,
        validators: vec![node_id_of(&fix.peer_secret)].try_into().unwrap(),
        inner_sets: vec![].try_into().unwrap(),
    };
    let delivered = fix
        .herder
        .recv_quorum_set(Hash256::from_bytes(unknown_qs_hash.0), unknown_qs);
    assert!(delivered, "recv_quorum_set should indicate envelopes ready");

    assert_eq!(
        fix.broadcast_count(),
        1,
        "broadcast must fire once deps are satisfied"
    );
}

/// Self-messages are rejected before reaching FetchingEnvelopes → no broadcast.
#[test]
fn test_relay_not_fired_for_self_message() {
    let fix = Fixture::new();

    // Envelope signed by and from the local node.
    let env = nominate_envelope_with_qs(
        TRACKING_SLOT,
        NOW,
        &fix.local_secret,
        &fix.network_id,
        &fix.local_qs_hash,
        &fix.cached_tx_set_hash,
    );

    let (state, reason) = run_pipeline(&fix, env);

    assert_eq!(reason, PostVerifyReason::SelfMessage);
    assert_eq!(state, EnvelopeState::Invalid);
    assert_eq!(fix.broadcast_count(), 0, "self-messages must not broadcast");
}

/// Duplicate submission while fetching: second returns Fetching (not error),
/// but only one broadcast fires when deps arrive.
#[test]
fn test_relay_exactly_once_duplicate_while_fetching() {
    let fix = Fixture::new();

    let unknown_qs_hash = XdrHash([0xCC; 32]);
    let env = nominate_envelope_with_qs(
        TRACKING_SLOT,
        NOW,
        &fix.peer_secret,
        &fix.network_id,
        &unknown_qs_hash,
        &fix.cached_tx_set_hash,
    );

    // First submission — fetching.
    let (state1, reason1) = run_pipeline(&fix, env.clone());
    assert_eq!(reason1, PostVerifyReason::Accepted);
    assert_eq!(state1, EnvelopeState::Fetching);
    assert_eq!(fix.broadcast_count(), 0);

    // Second submission of same envelope — also returns Fetching (dedup at
    // FetchingEnvelopes level returns Fetching for already-tracked envelopes).
    let (state2, reason2) = run_pipeline(&fix, env);
    assert_eq!(reason2, PostVerifyReason::Accepted);
    assert_eq!(state2, EnvelopeState::Fetching);
    assert_eq!(fix.broadcast_count(), 0);

    // Deliver deps — only one broadcast fires.
    let qs = ScpQuorumSet {
        threshold: 1,
        validators: vec![node_id_of(&fix.peer_secret)].try_into().unwrap(),
        inner_sets: vec![].try_into().unwrap(),
    };
    fix.herder
        .recv_quorum_set(Hash256::from_bytes(unknown_qs_hash.0), qs);

    assert_eq!(
        fix.broadcast_count(),
        1,
        "duplicate-while-fetching must not cause double broadcast"
    );
}

/// Duplicate submission after ready: second returns Duplicate, broadcast
/// count stays at 1.
#[test]
fn test_relay_exactly_once_duplicate_after_ready() {
    let fix = Fixture::new();

    let env = nominate_envelope_with_qs(
        TRACKING_SLOT,
        NOW,
        &fix.peer_secret,
        &fix.network_id,
        &fix.local_qs_hash,
        &fix.cached_tx_set_hash,
    );

    // First submission — immediately ready, broadcast fires.
    let (state1, reason1) = run_pipeline(&fix, env.clone());
    assert_eq!(reason1, PostVerifyReason::Accepted);
    // SCP returns Valid (no state change since nomination not started) → Duplicate.
    assert_eq!(state1, EnvelopeState::Duplicate);
    assert_eq!(fix.broadcast_count(), 1);

    // Second submission of same envelope — duplicate, no additional broadcast.
    let (state2, reason2) = run_pipeline(&fix, env);
    assert_eq!(reason2, PostVerifyReason::Accepted);
    assert_eq!(state2, EnvelopeState::Duplicate);
    assert_eq!(
        fix.broadcast_count(),
        1,
        "duplicate-after-ready must not broadcast again"
    );
}

/// Closing-gate replay does NOT re-trigger broadcast. The gate defers the
/// envelope for SCP processing, but FetchingEnvelopes already fired broadcast
/// during initial admission. Replay via ledger_closed drains through
/// process_scp_envelope_with_tx_set directly, bypassing FetchingEnvelopes.
#[test]
fn test_closing_gate_replay_no_additional_broadcast() {
    let fix = Fixture::new();

    // Arm the closing gate for TRACKING_SLOT (simulates externalization of
    // TRACKING_SLOT - 1 which sets gate.slot = TRACKING_SLOT).
    fix.herder.set_closing_gate_for_testing(TRACKING_SLOT);

    let env = nominate_envelope_with_qs(
        TRACKING_SLOT,
        NOW,
        &fix.peer_secret,
        &fix.network_id,
        &fix.local_qs_hash,
        &fix.cached_tx_set_hash,
    );

    // Submit envelope — FetchingEnvelopes fires broadcast (deps satisfied),
    // then process_scp_envelope_with_tx_set hits the gate → Deferred.
    let (state, reason) = run_pipeline(&fix, env);
    assert_eq!(reason, PostVerifyReason::Accepted);
    assert_eq!(state, EnvelopeState::Deferred);
    assert_eq!(
        fix.broadcast_count(),
        1,
        "broadcast fires during FetchingEnvelopes admission"
    );

    // Replay via ledger_closed (which drains the gate buffer).
    // The gate was armed for TRACKING_SLOT; ledger_closed for
    // TRACKING_SLOT - 1 is the call that clears it (matching production:
    // externalize slot N-1 → gate N → ledger_closed(N-1) drains gate).
    fix.herder
        .ledger_closed(TRACKING_SLOT - 1, &[], &[], TRACKING_CLOSE_TIME);

    assert_eq!(
        fix.broadcast_count(),
        1,
        "closing-gate replay must not re-trigger broadcast"
    );
}
