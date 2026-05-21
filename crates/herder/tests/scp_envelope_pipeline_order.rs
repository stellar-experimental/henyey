//! Ordering-contract integration tests for the SCP envelope reception pipeline.
//!
//! Verifies two parity-critical properties of the intake order:
//!
//! 1. **Validated intake rejects non-SIGNED values before fetch/relay** —
//!    stellar-core's `PendingEnvelopes::recvSCPEnvelope()` rejects envelopes
//!    with `STELLAR_VALUE_BASIC` or malformed values before `startFetch`,
//!    `envelopeReady`, or relay. This test verifies henyey matches that contract.
//!
//! 2. **Dependency-blocked envelopes do not count toward heard_from_quorum** —
//!    `slot_quorum_tracker.record_envelope(...)` fires only at the SCP-acceptance
//!    boundary (inside `process_scp_envelope_with_tx_set`), not at the
//!    pre-admission point in `process_verified`. This means envelopes parked in
//!    `FetchingEnvelopes` don't advance quorum diagnostics prematurely.
//!
//! Refs: #2870, stellar-core PendingEnvelopes.cpp:289-395

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

const NOW: u64 = 2_000_000_000;
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
    #[allow(dead_code)]
    local_secret: SecretKey,
    peer_secret: SecretKey,
    network_id: Hash256,
    broadcast_count: Arc<AtomicU64>,
    local_qs_hash: XdrHash,
    cached_tx_set_hash: XdrHash,
}

impl Fixture {
    fn new() -> Self {
        let local_secret = SecretKey::from_seed(&LOCAL_SEED);
        let peer_secret = SecretKey::from_seed(&PEER_SEED);

        let local_public = local_secret.public_key();
        let local_node_id = node_id_of(&local_secret);
        let peer_node_id = node_id_of(&peer_secret);

        // Threshold 1: one peer envelope is enough to satisfy quorum.
        let quorum_set = ScpQuorumSet {
            threshold: 1,
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

        herder.start_syncing();
        herder.bootstrap((TRACKING_SLOT - 1) as u32);
        herder.set_tracking_for_testing(TRACKING_SLOT, TRACKING_CLOSE_TIME);
        herder.set_test_clock_seconds(NOW);
        herder.set_pending_current_slot_for_testing(TRACKING_SLOT);

        // Prime the quorum tracker so peer passes the non-quorum gate.
        herder
            .expand_quorum_tracker_for_testing(&local_node_id, quorum_set.clone())
            .expect("expand quorum tracker");

        // Store quorum sets for both nodes so heard_from_quorum can resolve them.
        herder.store_quorum_set(&local_node_id, quorum_set.clone());
        herder.store_quorum_set(&peer_node_id, quorum_set);

        // Cache an empty tx_set so envelopes referencing it pass dependency check.
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

/// Build a Value with `StellarValueExt::Signed` (valid).
fn signed_value(close_time: u64, tx_set_hash: &XdrHash) -> Value {
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

/// Build a Value with `StellarValueExt::Basic` (invalid per upstream contract).
fn basic_value(close_time: u64, tx_set_hash: &XdrHash) -> Value {
    let sv = StellarValue {
        tx_set_hash: tx_set_hash.clone(),
        close_time: TimePoint(close_time),
        upgrades: vec![].try_into().unwrap(),
        ext: StellarValueExt::Basic,
    };
    Value(sv.to_xdr(Limits::none()).unwrap().try_into().unwrap())
}

fn sign_envelope(statement: ScpStatement, signer: &SecretKey, network_id: &Hash256) -> ScpEnvelope {
    let statement_bytes = statement.to_xdr(Limits::none()).unwrap();
    let mut data = network_id.0.to_vec();
    data.extend_from_slice(&1i32.to_be_bytes()); // ENVELOPE_TYPE_SCP
    data.extend_from_slice(&statement_bytes);
    let sig = signer.sign(&data);
    ScpEnvelope {
        statement,
        signature: XdrSignature(sig.as_bytes().to_vec().try_into().unwrap()),
    }
}

/// Nominate envelope with a SIGNED value (valid).
fn valid_nominate_envelope(fix: &Fixture) -> ScpEnvelope {
    let statement = ScpStatement {
        node_id: node_id_of(&fix.peer_secret),
        slot_index: TRACKING_SLOT,
        pledges: ScpStatementPledges::Nominate(ScpNomination {
            quorum_set_hash: fix.local_qs_hash.clone(),
            votes: vec![signed_value(NOW, &fix.cached_tx_set_hash)]
                .try_into()
                .unwrap(),
            accepted: vec![].try_into().unwrap(),
        }),
    };
    sign_envelope(statement, &fix.peer_secret, &fix.network_id)
}

/// Nominate envelope with a BASIC (unsigned) value — should be rejected.
fn basic_value_nominate_envelope(fix: &Fixture) -> ScpEnvelope {
    let statement = ScpStatement {
        node_id: node_id_of(&fix.peer_secret),
        slot_index: TRACKING_SLOT,
        pledges: ScpStatementPledges::Nominate(ScpNomination {
            quorum_set_hash: fix.local_qs_hash.clone(),
            votes: vec![basic_value(NOW, &fix.cached_tx_set_hash)]
                .try_into()
                .unwrap(),
            accepted: vec![].try_into().unwrap(),
        }),
    };
    sign_envelope(statement, &fix.peer_secret, &fix.network_id)
}

/// Nominate envelope with a valid signed value but referencing a tx_set that
/// is NOT cached — will park in FetchingEnvelopes waiting for deps.
fn dep_blocked_nominate_envelope(fix: &Fixture) -> ScpEnvelope {
    let unknown_tx_set_hash = XdrHash([0xBB; 32]);
    let statement = ScpStatement {
        node_id: node_id_of(&fix.peer_secret),
        slot_index: TRACKING_SLOT,
        pledges: ScpStatementPledges::Nominate(ScpNomination {
            quorum_set_hash: fix.local_qs_hash.clone(),
            votes: vec![signed_value(NOW, &unknown_tx_set_hash)]
                .try_into()
                .unwrap(),
            accepted: vec![].try_into().unwrap(),
        }),
    };
    sign_envelope(statement, &fix.peer_secret, &fix.network_id)
}

fn run_pipeline(fix: &Fixture, envelope: ScpEnvelope) -> (EnvelopeState, PostVerifyReason) {
    fix.herder.receive_scp_envelope_detailed(envelope)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// An envelope whose StellarValue is `Basic` (not `Signed`) should be rejected
/// by the validated intake path without triggering any fetch or relay side
/// effects.
///
/// Parity: stellar-core PendingEnvelopes::recvSCPEnvelope() rejects non-SIGNED
/// values before startFetch/envelopeReady.
///
/// This test fails on origin/main before the fix because `recv_envelope_validated`
/// skipped the strict signed-value gate.
#[test]
fn validated_intake_rejects_unsigned_values_before_fetch_or_relay() {
    let fix = Fixture::new();

    // Baseline: valid signed envelope goes through fine.
    let valid_env = valid_nominate_envelope(&fix);
    let (state, reason) = run_pipeline(&fix, valid_env);
    // SCP returns Valid (not ValidNew) because nomination hasn't started locally
    // for this slot. Herder maps Valid → Duplicate. The key assertion is that
    // the envelope reaches SCP (Accepted) and triggers broadcast.
    assert_eq!(state, EnvelopeState::Duplicate);
    assert_eq!(reason, PostVerifyReason::Accepted);
    assert_eq!(fix.broadcast_count(), 1, "valid envelope should broadcast");

    // Now send an envelope with Basic (non-signed) value.
    let basic_env = basic_value_nominate_envelope(&fix);
    let (state, reason) = run_pipeline(&fix, basic_env);

    // Should be rejected (Invalid/Discarded) — NOT Fetching or Ready.
    assert!(
        matches!(state, EnvelopeState::Invalid),
        "Basic-value envelope should be Invalid, got {:?}",
        state
    );
    assert_eq!(reason, PostVerifyReason::Accepted);
    // Broadcast count should NOT have increased.
    assert_eq!(
        fix.broadcast_count(),
        1,
        "Basic-value envelope must not trigger broadcast"
    );
}

/// A dependency-blocked envelope should NOT count toward `heard_from_quorum`
/// until its dependencies are satisfied and it passes through SCP.
///
/// With threshold=1, a single peer envelope would satisfy quorum. But if that
/// envelope is parked in FetchingEnvelopes waiting for a tx_set, quorum
/// diagnostics should remain false.
///
/// This test fails on origin/main before the fix because `process_verified`
/// called `slot_quorum_tracker.record_envelope(...)` before dependency admission.
#[test]
fn dependency_blocked_envelope_does_not_count_toward_heard_from_quorum_until_ready() {
    let fix = Fixture::new();

    // Verify baseline: quorum not yet satisfied.
    assert!(
        !fix.herder.heard_from_quorum(TRACKING_SLOT),
        "quorum should not be satisfied before any envelope"
    );

    // Send a dep-blocked envelope (unknown tx_set). It passes signature verify
    // and non-quorum gate, enters FetchingEnvelopes, but cannot reach SCP.
    let blocked_env = dep_blocked_nominate_envelope(&fix);
    let (state, _reason) = run_pipeline(&fix, blocked_env);
    assert_eq!(
        state,
        EnvelopeState::Fetching,
        "dep-blocked envelope should be Fetching"
    );

    // Quorum diagnostics should still be false — the envelope hasn't reached SCP.
    assert!(
        !fix.herder.heard_from_quorum(TRACKING_SLOT),
        "heard_from_quorum must remain false while envelope is dep-blocked"
    );
    assert!(
        !fix.herder.is_v_blocking(TRACKING_SLOT),
        "is_v_blocking must remain false while envelope is dep-blocked"
    );

    // Now satisfy the dependency by sending a valid envelope with a cached tx_set.
    // This will go through SCP and then quorum diagnostics should flip.
    let valid_env = valid_nominate_envelope(&fix);
    let (state, _reason) = run_pipeline(&fix, valid_env);
    // SCP returns Valid (not ValidNew) because nomination hasn't started locally.
    assert_eq!(state, EnvelopeState::Duplicate);

    // Now quorum should be satisfied (peer's envelope reached SCP).
    assert!(
        fix.herder.heard_from_quorum(TRACKING_SLOT),
        "heard_from_quorum should be true after envelope reaches SCP"
    );
}
