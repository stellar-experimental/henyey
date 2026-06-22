//! Cross-herder crash-recovery integration test for #2769.
//!
//! Simulates a node restart: a first herder persists its in-flight local SCP
//! state for a future slot (`lcl + 1`) into a shared `ScpPersistenceManager`;
//! a fresh second herder is then constructed over the *same* persistence store
//! and runs the startup restore path. The future-slot SCP state must be
//! observable on the second herder immediately after restore — before any
//! network traffic — mirroring stellar-core's `HerderImpl::restoreSCPState()`
//! (HerderImpl.cpp:2239-2311), called from `HerderImpl::start()`
//! (HerderImpl.cpp:2455-2471).
//!
//! The regression boundary this guards: PR #2797 was CLOSED for causing a
//! consensus stall by replaying already-finalized `slot <= lcl` ballots. The
//! `slot > lcl` filter in `restore_persisted_scp_state` drops those; this test
//! asserts the *future*-slot envelope survives the filter and is replayed.

#![cfg(feature = "test-support")]

use std::sync::Arc;

use henyey_common::Hash256;
use henyey_crypto::SecretKey;
use henyey_herder::{
    persistence::ScpPersistenceManager, Herder, HerderConfig, TimerManagerHandle, TransactionSet,
};
use henyey_ledger::{LedgerManager, LedgerManagerConfig};
use stellar_xdr::{
    Hash as XdrHash, Hash, LedgerCloseValueSignature, LedgerHeader, LedgerHeaderExt, Limits,
    NodeId as XdrNodeId, PublicKey as XdrPublicKey, ScpEnvelope, ScpNomination, ScpQuorumSet,
    ScpStatement, ScpStatementPledges, Signature as XdrSignature, StellarValue,
    StellarValue as XdrStellarValue, StellarValueExt, StellarValueExt as XdrStellarValueExt,
    TimePoint, TimePoint as XdrTimePoint, Uint256, Value, VecM, WriteXdr,
};

const LOCAL_SEED: [u8; 32] = [3u8; 32];
const NETWORK_ID: [u8; 32] = [0xA5; 32];

fn make_default_lm() -> Arc<LedgerManager> {
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

fn node_id_of(sk: &SecretKey) -> XdrNodeId {
    XdrNodeId(XdrPublicKey::PublicKeyTypeEd25519(Uint256(
        *sk.public_key().as_bytes(),
    )))
}

fn make_validator_herder(lcl: u32) -> (Arc<Herder>, SecretKey, ScpQuorumSet) {
    let secret = SecretKey::from_seed(&LOCAL_SEED);
    let public = secret.public_key();
    let node_id = node_id_of(&secret);

    let quorum_set = ScpQuorumSet {
        threshold: 1,
        validators: vec![node_id].try_into().unwrap(),
        inner_sets: vec![].try_into().unwrap(),
    };

    let config = HerderConfig {
        is_validator: true,
        node_public_key: public,
        network_id: Hash256::from_bytes(NETWORK_ID),
        local_quorum_set: Some(quorum_set.clone()),
        ..HerderConfig::default()
    };

    let herder = Arc::new(Herder::with_secret_key(
        config,
        secret.clone(),
        make_default_lm(),
        TimerManagerHandle::no_op(),
    ));
    herder.start_syncing();
    herder.bootstrap(lcl);
    (herder, secret, quorum_set)
}

/// Build a signed local NOMINATE envelope for `slot` referencing a tx set
/// built on the herder's LCL. Returns the envelope plus the persisted deps.
fn local_nominate_with_deps(
    herder: &Herder,
    secret: &SecretKey,
    quorum_set: &ScpQuorumSet,
    slot: u64,
) -> (ScpEnvelope, XdrHash, Vec<u8>, XdrHash, ScpQuorumSet) {
    // Tx set on top of the LCL the herder reports (default LM header hash).
    let lcl_hash = Hash256::from_bytes([0u8; 32]);
    let tx_set = TransactionSet::new(lcl_hash, Vec::new());
    let tx_set_hash = XdrHash(tx_set.hash().0);
    let stored_bytes = tx_set
        .to_xdr_stored_set()
        .to_xdr(Limits::none())
        .expect("encode StoredTransactionSet");

    let close_time = TimePoint(1);
    let network_id = Hash256::from_bytes(NETWORK_ID);
    let mut sign_data = network_id.0.to_vec();
    sign_data.extend_from_slice(
        &stellar_xdr::EnvelopeType::Scpvalue
            .to_xdr(Limits::none())
            .unwrap(),
    );
    sign_data.extend_from_slice(&tx_set_hash.to_xdr(Limits::none()).unwrap());
    sign_data.extend_from_slice(&close_time.to_xdr(Limits::none()).unwrap());
    let value_sig = secret.sign(&sign_data);

    let stellar_value = StellarValue {
        tx_set_hash: tx_set_hash.clone(),
        close_time,
        upgrades: vec![].try_into().unwrap(),
        ext: StellarValueExt::Signed(LedgerCloseValueSignature {
            node_id: node_id_of(secret),
            signature: XdrSignature(value_sig.0.to_vec().try_into().unwrap_or_default()),
        }),
    };
    let value = Value(
        stellar_value
            .to_xdr(Limits::none())
            .unwrap()
            .try_into()
            .unwrap(),
    );

    let qs_hash = XdrHash(henyey_common::Hash256::hash_xdr(quorum_set).0);

    let statement = ScpStatement {
        node_id: node_id_of(secret),
        slot_index: slot,
        pledges: ScpStatementPledges::Nominate(ScpNomination {
            quorum_set_hash: qs_hash.clone(),
            votes: vec![value].try_into().unwrap(),
            accepted: vec![].try_into().unwrap(),
        }),
    };

    let statement_bytes = statement.to_xdr(Limits::none()).unwrap();
    let mut data = network_id.0.to_vec();
    data.extend_from_slice(&1i32.to_be_bytes()); // ENVELOPE_TYPE_SCP = 1
    data.extend_from_slice(&statement_bytes);
    let sig = secret.sign(&data);
    let envelope = ScpEnvelope {
        statement,
        signature: XdrSignature(sig.as_bytes().to_vec().try_into().unwrap()),
    };

    (
        envelope,
        tx_set_hash,
        stored_bytes,
        qs_hash,
        quorum_set.clone(),
    )
}

#[test]
fn test_restored_future_slot_observable_after_restart() {
    let lcl = 100u64;

    // --- First node: build its in-flight local future-slot state and persist
    // it into a SHARED persistence manager (the "before crash" state). ---
    let manager = Arc::new(ScpPersistenceManager::in_memory());
    let (herder1, secret, quorum_set) = make_validator_herder(lcl as u32);
    assert!(herder1.set_scp_persistence(Arc::clone(&manager)).is_ok());

    let (env, tx_hash, tx_bytes, qs_hash, qs) =
        local_nominate_with_deps(&herder1, &secret, &quorum_set, lcl + 1);
    manager
        .persist_scp_state(
            lcl + 1,
            &[env],
            &[(tx_hash.clone(), tx_bytes)],
            &[(qs_hash.clone(), qs)],
        )
        .expect("persist future-slot state");
    drop(herder1); // simulate process exit; the persistence store survives.

    // --- Second node: fresh herder over the SAME store, run startup restore. ---
    let (herder2, _secret2, _qs2) = make_validator_herder(lcl as u32);
    assert!(herder2.set_scp_persistence(Arc::clone(&manager)).is_ok());

    // Restore BEFORE any network traffic — exactly the startup ordering.
    herder2.restore_persisted_scp_state(lcl);

    // The future-slot envelope must be observable immediately, with no peers.
    assert!(
        !herder2.get_current_state_for_slot(lcl + 1).is_empty(),
        "restored future-slot (lcl+1) SCP state must be observable after restart"
    );
    // Dependencies hydrated.
    assert!(
        herder2.has_tx_set(&Hash256(tx_hash.0)),
        "referenced tx set must be cached after restore"
    );
    assert!(
        herder2
            .get_quorum_set_by_hash(&Hash256(qs_hash.0))
            .is_some(),
        "referenced quorum set must be cached after restore"
    );
}
