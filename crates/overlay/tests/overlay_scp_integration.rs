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

    let mut scp_rx_b = manager_b
        .subscribe_scp()
        .await
        .expect("subscribe_scp");
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

    let first = timeout(Duration::from_secs(5), async {
        scp_rx_b.recv().await.expect("recv first scp")
    })
    .await
    .expect("timeout waiting first scp");

    let second = timeout(Duration::from_secs(5), async {
        scp_rx_b.recv().await.expect("recv second scp")
    })
    .await
    .expect("timeout waiting duplicate scp");

    assert!(matches!(first.message, StellarMessage::ScpMessage(_)));
    assert!(matches!(second.message, StellarMessage::ScpMessage(_)));
}
