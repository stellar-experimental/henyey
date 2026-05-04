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

    // SCP messages are exempt from FloodGate-level dedup (see issue #2317
    // and the comment in peer_loop.rs `route_received_message`). Both the
    // unique and duplicate envelope must reach the SCP subscriber so that
    // downstream layers (`scp_scheduled_envelopes` in-flight dedup, herder
    // self-rejection) can see them and so that alternate peer provenance
    // is not lost.
    let first = timeout(Duration::from_secs(5), async {
        scp_rx_b.recv().await.expect("recv first scp")
    })
    .await
    .expect("timeout waiting first scp");
    assert!(matches!(first.message, StellarMessage::ScpMessage(_)));

    let second = timeout(Duration::from_secs(5), async {
        scp_rx_b.recv().await.expect("recv second scp")
    })
    .await
    .expect("timeout waiting second scp — duplicate SCP must be forwarded, not dropped");
    assert!(matches!(second.message, StellarMessage::ScpMessage(_)));
}

/// Regression test for issue #2317.
///
/// Reproduces the standalone single-validator failure mode introduced by
/// commit c6118f2c. When a node broadcasts its own SCP envelope, the
/// envelope hash is recorded in its FloodGate with `from_peer = None`.
/// If the same envelope later arrives back from a peer (e.g. via
/// `GetScpState` response, peer reconnect, or out-of-sync recovery),
/// FloodGate's `record_seen` returns `RelayRecord::Repeated` because the hash
/// is already present. Without the SCP exemption restored in this fix,
/// the overlay drops the envelope and the herder never sees the
/// peer-sourced provenance — which in standalone mode means the
/// validator stops closing ledgers entirely.
///
/// This test broadcasts an envelope from A (recording its hash in A's
/// FloodGate), then has B send the same envelope back to A via a
/// directed peer-to-peer send, and asserts A's SCP subscriber receives
/// the echo.
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

    let peer_addr_b = PeerAddress::new("127.0.0.1", port_b);
    let peer_id_b_on_a = manager_a.connect(&peer_addr_b).await.expect("connect");

    // A subscribes to its own SCP feed before any broadcast or echo.
    let mut scp_rx_a = manager_a.subscribe_scp().await.expect("subscribe_scp a");

    let message = StellarMessage::ScpMessage(make_test_envelope(42));

    // A broadcasts the envelope. This records the hash in A's FloodGate
    // with `from_peer = None` (the local-broadcast case). The broadcast
    // does NOT loop back into A's own subscribers — broadcasts go out to
    // peers only.
    manager_a
        .broadcast(message.clone())
        .await
        .expect("broadcast from a");

    // Wait for B to receive A's broadcast over the connection. Once B
    // has the inbound peer (i.e., A connected to B), B can address A
    // by that peer id and send the envelope back. We discover B's
    // inbound peer id by polling peer_infos.
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

    // B echoes the same envelope back to A directly. From A's perspective
    // this is a peer-sourced SCP message whose hash is already in A's
    // FloodGate.
    manager_b
        .try_send_to(&peer_id_a_on_b, message.clone())
        .expect("try_send_to a from b");

    // A must receive the echoed envelope on its SCP subscription. With
    // the FloodGate-level SCP drop introduced by c6118f2c, this would
    // time out — that is exactly the standalone tx-finalization regression
    // tracked by issue #2317.
    let received = timeout(Duration::from_secs(5), async {
        scp_rx_a.recv().await.expect("recv echoed scp")
    })
    .await
    .expect("timeout waiting for echoed SCP — self-echo must reach the subscriber");
    assert!(matches!(received.message, StellarMessage::ScpMessage(_)));

    // Suppress unused-variable warning for the captured peer id; future
    // assertions may use it directly to address A from B's side.
    let _ = peer_id_b_on_a;
}
