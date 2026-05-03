use std::time::Duration;

use henyey_crypto::SecretKey;
use henyey_overlay::{LocalNode, OverlayConfig, OverlayManager, PeerAddress};
use stellar_xdr::curr::{
    Hash, ScpEnvelope, ScpNomination, ScpStatement, ScpStatementPledges, StellarMessage, Uint256,
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

fn make_test_envelope(slot: u64) -> ScpEnvelope {
    ScpEnvelope {
        statement: ScpStatement {
            node_id: stellar_xdr::curr::NodeId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
                Uint256([0u8; 32]),
            )),
            slot_index: slot,
            pledges: ScpStatementPledges::Nominate(ScpNomination {
                quorum_set_hash: Hash([0u8; 32]),
                votes: vec![].try_into().unwrap(),
                accepted: vec![].try_into().unwrap(),
            }),
        },
        signature: stellar_xdr::curr::Signature(vec![0u8; 64].try_into().unwrap()),
    }
}

#[tokio::test]
async fn test_overlay_scp_message_roundtrip() {
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
    let _peer_id = manager_a.connect(&peer_addr_b).await.expect("connect");

    let mut scp_rx_b = manager_b.subscribe_scp().await.expect("subscribe_scp");
    let message = StellarMessage::ScpMessage(make_test_envelope(1));
    manager_a
        .broadcast(message.clone())
        .await
        .expect("broadcast");

    let received = timeout(Duration::from_secs(5), async {
        scp_rx_b.recv().await.expect("recv scp")
    })
    .await
    .expect("timeout");

    match received.message {
        StellarMessage::ScpMessage(_) => {}
        other => panic!("unexpected message: {:?}", other),
    }
}

#[tokio::test]
async fn test_overlay_scp_duplicate_is_forwarded_to_receiver() {
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
    let _peer_id = manager_a.connect(&peer_addr_b).await.expect("connect");

    let mut scp_rx_b = manager_b.subscribe_scp().await.expect("subscribe_scp");
    let message = StellarMessage::ScpMessage(make_test_envelope(7));

    manager_a
        .broadcast(message.clone())
        .await
        .expect("broadcast first");
    manager_a
        .broadcast(message.clone())
        .await
        .expect("broadcast duplicate");

    // First (unique) SCP message should arrive.
    let first = timeout(Duration::from_secs(5), async {
        scp_rx_b.recv().await.expect("recv first scp")
    })
    .await
    .expect("timeout waiting first scp");
    assert!(matches!(first.message, StellarMessage::ScpMessage(_)));

    // Duplicate SCP must also be forwarded — FloodGate exempts SCP from dropping.
    // Dedup happens downstream in pump_scp_intake (scp_scheduled_envelopes).
    let second = timeout(Duration::from_secs(5), async {
        scp_rx_b.recv().await.expect("recv second scp")
    })
    .await
    .expect("duplicate SCP message should have been forwarded, but was dropped at overlay");
    assert!(matches!(second.message, StellarMessage::ScpMessage(_)));
}

/// Regression test for issue #2317: In standalone single-validator mode,
/// the validator broadcasts its own SCP envelopes (recording in FloodGate).
/// When those same envelopes re-enter from a connected peer, they must NOT
/// be dropped — they carry peer provenance needed for tx-set/quorum-set fetches.
#[tokio::test]
async fn test_scp_self_echo_not_dropped_after_broadcast() {
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

    let peer_addr_a = PeerAddress::new("127.0.0.1", port_a);
    let _peer_id = manager_b.connect(&peer_addr_a).await.expect("connect b->a");

    // Subscribe to SCP on manager A (the "validator" that broadcast originally)
    let mut scp_rx_a = manager_a.subscribe_scp().await.expect("subscribe_scp");
    let envelope = make_test_envelope(42);
    let message = StellarMessage::ScpMessage(envelope.clone());

    // Simulate standalone validator: manager A broadcasts (records in FloodGate)
    manager_a
        .broadcast(message.clone())
        .await
        .expect("broadcast from A");

    // Now peer B sends the same SCP envelope back to A (simulating self-echo
    // from GetScpState or out-of-sync recovery in standalone mode)
    manager_b
        .broadcast(message.clone())
        .await
        .expect("broadcast from B");

    // Manager A must receive the SCP envelope from B despite having broadcast
    // the same message itself. FloodGate records it as duplicate but does NOT
    // drop it — SCP is exempt from the drop decision.
    let received = timeout(Duration::from_secs(5), async {
        scp_rx_a.recv().await.expect("recv scp on A")
    })
    .await
    .expect("self-echo SCP should reach subscriber but was dropped (issue #2317)");
    assert!(matches!(received.message, StellarMessage::ScpMessage(_)));
}
