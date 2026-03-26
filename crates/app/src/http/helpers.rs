//! Helper functions shared across HTTP handlers.

use henyey_crypto::PublicKey as CryptoPublicKey;
use henyey_overlay::{PeerAddress, PeerId};
use stellar_xdr::curr::LedgerUpgrade;

use super::types::{ConnectParams, UpgradeItem};

/// Parse connect endpoint parameters into a PeerAddress.
pub(super) fn parse_connect_params(params: &ConnectParams) -> Result<PeerAddress, String> {
    if let Some(addr) = params.addr.as_ref() {
        let (host, port) = addr
            .split_once(':')
            .ok_or_else(|| "addr must be host:port".to_string())?;
        let port = port
            .parse::<u16>()
            .map_err(|_| "invalid port".to_string())?;
        return Ok(PeerAddress::new(host.to_string(), port));
    }

    let Some(peer) = params.peer.as_ref() else {
        return Err("addr or peer/port must be provided".to_string());
    };
    let port = params
        .port
        .ok_or_else(|| "port must be provided".to_string())?;
    Ok(PeerAddress::new(peer.to_string(), port))
}

/// Parse a peer_id or node parameter into a PeerId.
pub(super) fn parse_peer_id_params(
    peer_id: &Option<String>,
    node: &Option<String>,
) -> Result<PeerId, String> {
    let value = peer_id
        .as_ref()
        .or(node.as_ref())
        .ok_or_else(|| "peer_id or node must be provided".to_string())?;
    parse_peer_id(value)
}

/// Parse a string (hex or strkey) into a PeerId.
pub(super) fn parse_peer_id(value: &str) -> Result<PeerId, String> {
    if let Ok(bytes) = hex::decode(value) {
        if let Ok(raw) = <[u8; 32]>::try_from(bytes.as_slice()) {
            return Ok(PeerId::from_bytes(raw));
        }
    }

    let key = CryptoPublicKey::from_strkey(value)
        .map_err(|_| "invalid peer_id (expected 32-byte hex or strkey)".to_string())?;
    Ok(PeerId::from_bytes(*key.as_bytes()))
}

/// Convert a NodeId to its strkey representation.
pub(super) fn node_id_to_strkey(node_id: &stellar_xdr::curr::NodeId) -> Option<String> {
    match &node_id.0 {
        stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(key) => {
            CryptoPublicKey::from_bytes(&key.0)
                .ok()
                .map(|pk| pk.to_strkey())
        }
    }
}

/// Convert a PeerId to its strkey representation.
pub(super) fn peer_id_to_strkey(peer_id: PeerId) -> Option<String> {
    node_id_to_strkey(&stellar_xdr::curr::NodeId(peer_id.0))
}

/// Map a LedgerUpgrade to an UpgradeItem for JSON serialization.
pub(super) fn map_upgrade_item(upgrade: LedgerUpgrade) -> Option<UpgradeItem> {
    match upgrade {
        LedgerUpgrade::Version(value) => Some(UpgradeItem {
            r#type: "protocol_version".to_string(),
            value,
        }),
        LedgerUpgrade::BaseFee(value) => Some(UpgradeItem {
            r#type: "base_fee".to_string(),
            value,
        }),
        LedgerUpgrade::BaseReserve(value) => Some(UpgradeItem {
            r#type: "base_reserve".to_string(),
            value,
        }),
        LedgerUpgrade::MaxTxSetSize(value) => Some(UpgradeItem {
            r#type: "max_tx_set_size".to_string(),
            value,
        }),
        _ => None,
    }
}
