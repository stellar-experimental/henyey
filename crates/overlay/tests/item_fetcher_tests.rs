//! Integration tests for ItemFetcher functionality.

use henyey_overlay::{ItemFetcher, ItemFetcherConfig, ItemType, PeerId};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use stellar_xdr::curr::{
    Hash, NodeId as XdrNodeId, PublicKey, ScpEnvelope, ScpNomination, ScpStatement,
    ScpStatementPledges, Signature, Uint256,
};

fn make_test_envelope(slot: u64, node_seed: u8) -> ScpEnvelope {
    let node_id = XdrNodeId(PublicKey::PublicKeyTypeEd25519(Uint256([node_seed; 32])));

    ScpEnvelope {
        statement: ScpStatement {
            node_id,
            slot_index: slot,
            pledges: ScpStatementPledges::Nominate(ScpNomination {
                quorum_set_hash: Hash([1u8; 32]),
                votes: vec![].try_into().unwrap(),
                accepted: vec![].try_into().unwrap(),
            }),
        },
        signature: Signature(vec![0u8; 64].try_into().unwrap()),
    }
}

fn make_test_peer(seed: u8) -> PeerId {
    PeerId::from_bytes([seed; 32])
}

#[test]
fn test_fetch_immediately_invokes_callback() {
    let callback_count = Arc::new(AtomicUsize::new(0));
    let callback_count_clone = callback_count.clone();

    let mut fetcher = ItemFetcher::new(ItemType::TxSet, ItemFetcherConfig::default());

    // Set up callback
    fetcher.set_ask_peer(Box::new(move |_peer, _hash, _item_type| {
        callback_count_clone.fetch_add(1, Ordering::SeqCst);
    }));

    // Set available peers
    let peers = vec![make_test_peer(1), make_test_peer(2), make_test_peer(3)];
    fetcher.set_available_peers(peers);

    // Fetch an item - should immediately invoke callback
    let hash = Hash([0xab; 32]);
    let envelope = make_test_envelope(100, 1);
    fetcher.fetch(hash.clone(), &envelope);

    // Callback should have been invoked immediately
    assert_eq!(callback_count.load(Ordering::SeqCst), 1);

    // Fetching same item again should not invoke callback (already tracking)
    let envelope2 = make_test_envelope(100, 2);
    fetcher.fetch(hash.clone(), &envelope2);

    // Still only 1 invocation
    assert_eq!(callback_count.load(Ordering::SeqCst), 1);
}

#[test]
fn test_fetch_without_peers_no_callback() {
    let callback_count = Arc::new(AtomicUsize::new(0));
    let callback_count_clone = callback_count.clone();

    let mut fetcher = ItemFetcher::new(ItemType::QuorumSet, ItemFetcherConfig::default());

    fetcher.set_ask_peer(Box::new(move |_peer, _hash, _item_type| {
        callback_count_clone.fetch_add(1, Ordering::SeqCst);
    }));

    // No peers set
    fetcher.set_available_peers(vec![]);

    // Fetch - should not invoke callback (no peers available)
    let hash = Hash([0xcd; 32]);
    let envelope = make_test_envelope(100, 1);
    fetcher.fetch(hash, &envelope);

    // Callback not invoked (no peers)
    assert_eq!(callback_count.load(Ordering::SeqCst), 0);
}

#[test]
fn test_process_pending_invokes_callback_on_timeout() {
    let callback_count = Arc::new(AtomicUsize::new(0));
    let callback_count_clone = callback_count.clone();

    // Use very short timeout for testing
    let config = ItemFetcherConfig {
        fetch_reply_timeout: std::time::Duration::from_millis(1),
        ..Default::default()
    };

    let mut fetcher = ItemFetcher::new(ItemType::TxSet, config);

    fetcher.set_ask_peer(Box::new(move |_peer, _hash, _item_type| {
        callback_count_clone.fetch_add(1, Ordering::SeqCst);
    }));

    // Set up peers
    let peers = vec![make_test_peer(1), make_test_peer(2)];
    fetcher.set_available_peers(peers);

    // Fetch - first callback invocation
    let hash = Hash([0xef; 32]);
    let envelope = make_test_envelope(100, 1);
    fetcher.fetch(hash, &envelope);

    assert_eq!(callback_count.load(Ordering::SeqCst), 1);

    // Wait for timeout
    std::thread::sleep(std::time::Duration::from_millis(10));

    // Process pending - should retry with next peer
    let sent = fetcher.process_pending();
    assert_eq!(sent, 1);

    // Callback should have been invoked again (retry)
    assert_eq!(callback_count.load(Ordering::SeqCst), 2);
}

#[test]
fn test_recv_clears_tracker() {
    let fetcher = ItemFetcher::new(ItemType::TxSet, ItemFetcherConfig::default());

    fetcher.set_available_peers(vec![make_test_peer(1)]);

    let hash = Hash([0x11; 32]);
    let envelope = make_test_envelope(100, 1);
    fetcher.fetch(hash.clone(), &envelope);

    assert!(fetcher.is_tracking(&hash));

    // Receive the item - should remove tracker and return waiting envelopes
    let waiting = fetcher.recv(&hash);
    assert_eq!(waiting.unwrap().len(), 1);

    // Tracker is now removed from the map
    assert!(!fetcher.is_tracking(&hash));

    // Second recv returns None (tracker already removed)
    let waiting2 = fetcher.recv(&hash);
    assert!(waiting2.is_none());
}

#[test]
fn test_doesnt_have_triggers_retry() {
    let callback_count = Arc::new(AtomicUsize::new(0));
    let callback_count_clone = callback_count.clone();

    let mut fetcher = ItemFetcher::new(ItemType::QuorumSet, ItemFetcherConfig::default());

    fetcher.set_ask_peer(Box::new(move |_peer, _hash, _item_type| {
        callback_count_clone.fetch_add(1, Ordering::SeqCst);
    }));

    let peers = vec![make_test_peer(1), make_test_peer(2)];
    fetcher.set_available_peers(peers.clone());

    let hash = Hash([0x22; 32]);
    let envelope = make_test_envelope(100, 1);
    fetcher.fetch(hash.clone(), &envelope);

    // First callback invocation
    assert_eq!(callback_count.load(Ordering::SeqCst), 1);

    // Peer 1 doesn't have it - should try next peer via process_pending
    fetcher.doesnt_have(&hash, &peers[0]);

    // Process to actually retry
    let sent = fetcher.process_pending();
    assert!(sent >= 1);

    // Callback invoked again for retry
    assert!(callback_count.load(Ordering::SeqCst) >= 2);
}

#[test]
fn test_stop_fetching_below() {
    let fetcher = ItemFetcher::new(ItemType::TxSet, ItemFetcherConfig::default());

    fetcher.set_available_peers(vec![make_test_peer(1)]);

    // Add envelopes for different slots
    let hash1 = Hash([0x33; 32]);
    let hash2 = Hash([0x44; 32]);
    let hash3 = Hash([0x55; 32]);

    fetcher.fetch(hash1.clone(), &make_test_envelope(100, 1));
    fetcher.fetch(hash2.clone(), &make_test_envelope(101, 2));
    fetcher.fetch(hash3.clone(), &make_test_envelope(102, 3));

    assert_eq!(fetcher.num_trackers(), 3);

    // Stop fetching below slot 102, keeping slot 100
    fetcher.stop_fetching_below(102, 100);

    // Slot 100 kept, slot 101 removed, slot 102 kept
    // Trackers for hash1 and hash3 should remain
    assert!(fetcher.is_tracking(&hash1));
    assert!(!fetcher.is_tracking(&hash2)); // Slot 101 was removed
    assert!(fetcher.is_tracking(&hash3));
}

#[test]
fn test_item_type_passed_to_callback() {
    let received_type = Arc::new(std::sync::Mutex::new(None));
    let received_type_clone = received_type.clone();

    let mut fetcher = ItemFetcher::new(ItemType::QuorumSet, ItemFetcherConfig::default());

    fetcher.set_ask_peer(Box::new(move |_peer, _hash, item_type| {
        *received_type_clone.lock().unwrap() = Some(item_type);
    }));

    fetcher.set_available_peers(vec![make_test_peer(1)]);

    let hash = Hash([0x66; 32]);
    let envelope = make_test_envelope(100, 1);
    fetcher.fetch(hash, &envelope);

    assert_eq!(*received_type.lock().unwrap(), Some(ItemType::QuorumSet));
}

#[test]
fn test_stats() {
    let fetcher = ItemFetcher::new(ItemType::TxSet, ItemFetcherConfig::default());

    assert_eq!(fetcher.num_trackers(), 0);

    let hash1 = Hash([0x77; 32]);
    let hash2 = Hash([0x88; 32]);

    fetcher.fetch(hash1, &make_test_envelope(100, 1));
    fetcher.fetch(hash2, &make_test_envelope(101, 2));

    let stats = fetcher.get_stats();
    assert_eq!(stats.num_trackers, 2);
    assert_eq!(stats.item_type, ItemType::TxSet);
}
