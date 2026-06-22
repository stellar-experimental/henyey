//! Regression tests for the persist/purge race in `ScpPersistenceManager`.
//!
//! `purge_unreferenced_tx_sets()` historically performed three separate
//! connection checkouts against `SqliteScpPersistence`:
//!   1. `get_all_tx_set_hashes()`
//!   2. `load_all_scp_states()` (to compute referenced hashes)
//!   3. `delete_tx_sets_by_hashes(orphans)`
//!
//! With those steps unsynchronized, a concurrent `persist_scp_state` between
//! steps 2 and 3 could see its freshly-inserted tx-set deleted as an orphan.
//! Stellar-core has no equivalent race because all herder work runs on the
//! main thread (`HerderImpl.cpp` serializes persist and purge naturally).
//!
//! These tests exercise the atomic path added in #2770. See plan in the
//! converged-plan comment on #2770.
//!
//! Parity reference:
//! - `HerderImpl::purgeOldPersistedTxSets()` (HerderImpl.cpp:2448-2487)
//! - `HerderImpl::startTxSetGCTimer()` (HerderImpl.cpp:2440-2444)

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use henyey_db::Database;
use henyey_herder::{
    get_tx_set_hashes, PersistedSlotState, ScpPersistenceManager, ScpStatePersistence,
    SqliteScpPersistence,
};
use stellar_xdr::{
    Hash, Limits, NodeId, PublicKey, ScpEnvelope, ScpNomination, ScpStatement, ScpStatementPledges,
    Signature, StellarValue, StellarValueExt, TimePoint, Uint256, Value, WriteXdr,
};

/// Build an SCP NOMINATE envelope whose value references the given tx-set hash.
fn make_envelope_with_tx_set_hash(slot: u64, tx_set_hash: Hash) -> ScpEnvelope {
    let stellar_value = StellarValue {
        tx_set_hash,
        close_time: TimePoint(0),
        upgrades: vec![].try_into().unwrap(),
        ext: StellarValueExt::Basic,
    };
    let value = stellar_value.to_xdr(Limits::none()).unwrap();
    ScpEnvelope {
        statement: ScpStatement {
            node_id: NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32]))),
            slot_index: slot,
            pledges: ScpStatementPledges::Nominate(ScpNomination {
                quorum_set_hash: Hash([0u8; 32]),
                votes: vec![Value(value.try_into().unwrap())].try_into().unwrap(),
                accepted: vec![].try_into().unwrap(),
            }),
        },
        signature: Signature::default(),
    }
}

/// Direct SQLite-backend test of the new atomic trait method.
///
/// Structurally fails on `origin/main` because
/// `ScpStatePersistence::purge_unreferenced_tx_sets_atomic` does not exist
/// yet — the test fails to compile. Passes once the trait method is added.
#[test]
fn test_purge_atomic_on_sqlite_backend() {
    let db = Database::open_in_memory().unwrap();
    let persistence = SqliteScpPersistence::new(db);

    let referenced_hash = Hash([0xAA; 32]);
    let orphan_hash = Hash([0xBB; 32]);

    // Seed via the trait so we exercise the SQLite path end-to-end.
    let env = make_envelope_with_tx_set_hash(100, referenced_hash.clone());
    let mut state = PersistedSlotState::new();
    state.add_envelope(&env).unwrap();
    persistence.save_scp_state(100, &state).unwrap();
    persistence.save_tx_set(&referenced_hash, &[10u8]).unwrap();
    persistence.save_tx_set(&orphan_hash, &[20u8]).unwrap();

    // New atomic API — must exist as a trait method on ScpStatePersistence.
    persistence.purge_unreferenced_tx_sets_atomic().unwrap();

    assert!(
        persistence.has_tx_set(&referenced_hash).unwrap(),
        "Referenced tx-set should survive atomic purge"
    );
    assert!(
        !persistence.has_tx_set(&orphan_hash).unwrap(),
        "Orphan tx-set should be deleted by atomic purge"
    );
}

/// Race test: spawn a background thread that continuously persists new SCP
/// states (each with a fresh tx-set), while the main thread invokes
/// `purge_unreferenced_tx_sets()` repeatedly. The persisted tx-sets are all
/// referenced by their own envelopes, so the post-condition is: every
/// tx-set the persister claimed to have written must still be present at
/// the end.
///
/// Under the old non-atomic code, a tx-set inserted between
/// `load_all_scp_states` and `delete_tx_sets_by_hashes` would be visible
/// as a "stored hash" but not yet as a "referenced hash", and would be
/// deleted as an orphan. The race is non-deterministic — documented here
/// as a regression guard. The hard structural test for the new API is
/// `test_purge_atomic_on_sqlite_backend` above.
#[test]
fn test_purge_atomic_survives_concurrent_persist() {
    let db = Database::open_in_memory().unwrap();
    let persistence = SqliteScpPersistence::new(db);
    let manager = Arc::new(ScpPersistenceManager::new(Box::new(persistence)));

    // Pre-populate: slot 100 references `referenced_hash`.
    let referenced_hash = Hash([0x01; 32]);
    let env = make_envelope_with_tx_set_hash(100, referenced_hash.clone());
    manager
        .persist_scp_state(100, &[env], &[(referenced_hash.clone(), vec![10])], &[])
        .unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();
    let manager_clone = manager.clone();
    let persisted = Arc::new(parking_lot::Mutex::new(Vec::<Hash>::new()));
    let persisted_clone = persisted.clone();

    let persister = thread::spawn(move || {
        let mut next_slot: u64 = 200;
        while !stop_clone.load(Ordering::Relaxed) {
            let mut hash_bytes = [0u8; 32];
            hash_bytes[..8].copy_from_slice(&next_slot.to_le_bytes());
            let tx_hash = Hash(hash_bytes);
            let envelope = make_envelope_with_tx_set_hash(next_slot, tx_hash.clone());
            // Sanity: envelope must reference tx_hash.
            assert!(
                get_tx_set_hashes(&envelope).contains(&tx_hash),
                "envelope must reference tx_hash"
            );
            if manager_clone
                .persist_scp_state(
                    next_slot,
                    &[envelope],
                    &[(tx_hash.clone(), vec![next_slot as u8])],
                    &[],
                )
                .is_ok()
            {
                persisted_clone.lock().push(tx_hash);
            }
            next_slot += 1;
            thread::sleep(Duration::from_micros(50));
        }
    });

    for _ in 0..50 {
        manager.purge_unreferenced_tx_sets().unwrap();
        thread::sleep(Duration::from_millis(2));
    }

    stop.store(true, Ordering::Relaxed);
    persister.join().unwrap();

    // Final assertion: pre-populated tx-set + every claimed-persisted tx-set
    // must survive. All are referenced by their own envelopes.
    assert!(
        manager.has_tx_set(&referenced_hash).unwrap(),
        "pre-populated referenced tx-set must survive"
    );
    let persisted_hashes = persisted.lock().clone();
    for h in &persisted_hashes {
        assert!(
            manager.has_tx_set(h).unwrap(),
            "concurrently-persisted tx-set {:?} must survive purge",
            h
        );
    }
}
