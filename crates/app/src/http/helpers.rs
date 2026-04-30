//! Helper functions shared across HTTP handlers.

use henyey_crypto::PublicKey as CryptoPublicKey;
use henyey_overlay::{PeerAddress, PeerId};
use stellar_xdr::curr::LedgerUpgrade;

use super::types::{ConnectParams, UpgradeItem};

/// Parse connect endpoint parameters into a PeerAddress.
pub(super) fn parse_connect_params(params: &ConnectParams) -> Result<PeerAddress, String> {
    if let Some(addr) = params.addr.as_ref() {
        return crate::config::parse_peer_address(addr);
    }

    let Some(peer) = params.peer.as_ref() else {
        return Err("addr or peer/port must be provided".to_string());
    };
    let port = params
        .port
        .ok_or_else(|| "port must be provided".to_string())?;
    // Validate as if it were "peer:port"
    crate::config::parse_peer_address(&format!("{}:{}", peer, port))
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
    let (r#type, value) = match upgrade {
        LedgerUpgrade::Version(value) => ("protocol_version", value),
        LedgerUpgrade::BaseFee(value) => ("base_fee", value),
        LedgerUpgrade::BaseReserve(value) => ("base_reserve", value),
        LedgerUpgrade::MaxTxSetSize(value) => ("max_tx_set_size", value),
        _ => return None,
    };

    Some(UpgradeItem {
        r#type: r#type.to_string(),
        value,
    })
}
