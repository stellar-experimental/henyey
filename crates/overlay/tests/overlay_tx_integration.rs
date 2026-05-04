use std::time::Duration;

use henyey_crypto::SecretKey;
use henyey_overlay::{LocalNode, OverlayConfig, OverlayManager, PeerAddress};
use stellar_xdr::curr::{
    Memo, MuxedAccount, Preconditions, SequenceNumber, StellarMessage, Transaction,
    TransactionEnvelope, TransactionV1Envelope, Uint256,
};
use tokio::time::timeout;

fn allocate_port() -> Option<u16> {
    let listener = match std::net::TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => return None,
        Err(err) => panic!("bind: {err}"),
    };
    let addr = listener.local_addr().expect("addr");
    drop(listener);
    Some(addr.port())
}

fn make_test_transaction() -> TransactionEnvelope {
    TransactionEnvelope::Tx(TransactionV1Envelope {
        tx: Transaction {
            source_account: MuxedAccount::Ed25519(Uint256([1u8; 32])),
            fee: 100,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![].try_into().unwrap(),
            ext: stellar_xdr::curr::TransactionExt::V0,
        },
        signatures: vec![].try_into().unwrap(),
    })
}

/// Regression test for issue #2327: Transaction messages must NOT be dropped
/// at the FloodGate layer when a duplicate is observed.
///
/// stellar-core parity: OverlayManagerImpl::recvTransaction
/// (OverlayManagerImpl.cpp:1215-1248) calls recvFloodedMsgID for relay
/// tracking, then unconditionally processes the transaction. There is no
/// FloodGate-based drop on the Tx receive path.
///
/// This test broadcasts a Transaction from A (recording its hash in A's
/// FloodGate), then has B send the same Transaction back to A via a
/// directed peer-to-peer send, and asserts A's generic subscriber receives
/// the echo. Before the fix, the duplicate Transaction was silently dropped
/// at peer_loop.rs line 814.
#[tokio::test]
async fn test_tx_duplicate_not_dropped_after_broadcast() {
    let Some(port_a) = allocate_port() else {
        eprintln!("skipping test: tcp bind not permitted in this environment");
        return;
    };
    let Some(port_b) = allocate_port() else {
        eprintln!("skipping test: tcp bind not permitted in this environment");
        return;
    };

    let secret_a = SecretKey::generate();
    let secret_b = SecretKey::generate();

    let local_a = LocalNode::new_testnet(secret_a);
    let local_b = LocalNode::new_testnet(secret_b);

    let mut config_a = OverlayConfig::testnet();
    config_a.listen_port = port_a;
    config_a.listen_enabled = true;
    config_a.known_peers.clear();
    config_a.connect_timeout_secs = 5;

    let mut config_b = OverlayConfig::testnet();
    config_b.listen_port = port_b;
    config_b.listen_enabled = true;
    config_b.known_peers.clear();
    config_b.connect_timeout_secs = 5;

    let mut manager_a = OverlayManager::new(config_a, local_a).expect("manager a");
    let mut manager_b = OverlayManager::new(config_b, local_b).expect("manager b");

    manager_a.start().await.expect("start a");
    manager_b.start().await.expect("start b");

    let peer_addr_b = PeerAddress::new("127.0.0.1", port_b);
    let _peer_id_b_on_a = manager_a.connect(&peer_addr_b).await.expect("connect");

    // A subscribes to the generic broadcast channel (Transaction messages
    // route here, not to the dedicated SCP channel).
    let mut generic_rx = manager_a.subscribe();

    let message = StellarMessage::Transaction(make_test_transaction());

    // A broadcasts the Transaction. This records the hash in A's FloodGate
    // with `from_peer = None`. The broadcast does NOT loop back into A's
    // own subscribers.
    manager_a
        .broadcast(message.clone())
        .await
        .expect("broadcast from a");

    // Wait for B to see A as a connected peer.
    let peer_id_a_on_b = timeout(Duration::from_secs(5), async {
        loop {
            if let Some(p) = manager_b.connected_peers().into_iter().next() {
                return p;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("timeout waiting for b to see a as inbound peer");

    // B echoes the same Transaction back to A. From A's perspective this is
    // a peer-sourced Transaction whose hash is already in A's FloodGate.
    manager_b
        .try_send_to(&peer_id_a_on_b, message.clone())
        .expect("try_send_to a from b");

    // A must receive the echoed Transaction on its generic subscription.
    // The generic broadcast channel is lossy and may carry unrelated
    // messages, so we loop/filter until we see a Transaction or time out.
    let received = timeout(Duration::from_secs(5), async {
        loop {
            match generic_rx.recv().await {
                Ok(overlay_msg) => {
                    if matches!(overlay_msg.message, StellarMessage::Transaction(_)) {
                        return overlay_msg;
                    }
                    // Not our message, keep polling.
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    eprintln!("broadcast channel lagged by {n}, continuing");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    panic!("broadcast channel closed unexpectedly");
                }
            }
        }
    })
    .await
    .expect("timeout waiting for echoed Transaction — duplicate must not be dropped at FloodGate");

    assert!(matches!(received.message, StellarMessage::Transaction(_)));
}
