//! Utility functions for Herder operations.
//!
//! This module provides helper functions that match the stellar-core
//! `HerderUtils.h` functionality:
//!
//! - [`get_stellar_values`]: Extract StellarValue from SCP statements
//! - [`get_tx_set_hashes`]: Get transaction set hashes from SCP envelopes
//! - [`to_short_string`]: Render NodeID as a short human-readable string

use henyey_common::Hash256;
use henyey_scp::Slot;
use stellar_xdr::curr::{Limits, NodeId, ReadXdr, ScpEnvelope, ScpStatement, StellarValue};

/// Extract all StellarValues from an SCP statement.
///
/// Parses the opaque `Value` blobs from the statement (votes, accepted values,
/// ballot values) and deserializes them as `StellarValue` structures.
///
/// # Arguments
///
/// * `statement` - The SCP statement to extract values from
///
/// # Returns
///
/// A vector of successfully parsed `StellarValue` structures. Values that fail
/// to parse are silently skipped.
///
/// # Example
///
/// ```ignore
/// use stellar_xdr::curr::ScpStatement;
/// use henyey_herder::get_stellar_values;
///
/// let values = get_stellar_values(&statement);
/// for sv in values {
///     println!("TxSet hash: {:?}, close time: {}", sv.tx_set_hash, sv.close_time.0);
/// }
/// ```
pub fn get_stellar_values(statement: &ScpStatement) -> Vec<StellarValue> {
    let values = Slot::get_statement_values(statement);
    values
        .into_iter()
        .filter_map(|v| StellarValue::from_xdr(&v.0, Limits::none()).ok())
        .collect()
}

/// Extract all transaction set hashes from an SCP envelope.
///
/// This is a convenience wrapper around [`get_stellar_values`] that extracts
/// just the transaction set hashes.
///
/// # Arguments
///
/// * `envelope` - The SCP envelope to extract tx set hashes from
///
/// # Returns
///
/// A vector of transaction set hashes (as `Hash256`) from all values in the envelope.
///
/// # Example
///
/// ```ignore
/// use stellar_xdr::curr::ScpEnvelope;
/// use henyey_herder::get_tx_set_hashes_from_envelope;
///
/// let hashes = get_tx_set_hashes_from_envelope(&envelope);
/// for hash in hashes {
///     println!("TxSet hash: {}", hash);
/// }
/// ```
pub fn get_tx_set_hashes_from_envelope(envelope: &ScpEnvelope) -> Vec<Hash256> {
    get_stellar_values(&envelope.statement)
        .into_iter()
        .map(|sv| Hash256::from_bytes(sv.tx_set_hash.0))
        .collect()
}

/// Render a NodeID as a short human-readable string.
///
/// Returns the first 5 characters of the hex-encoded public key.
/// This matches the stellar-core `toShortString` behavior when no config is provided.
///
/// # Arguments
///
/// * `node_id` - The NodeID to render
///
/// # Returns
///
/// A short string representation of the node ID (first 5 hex characters).
///
/// # Example
///
/// ```ignore
/// use stellar_xdr::curr::NodeId;
/// use henyey_herder::to_short_string;
///
/// let short = to_short_string(&node_id);
/// println!("Node: {}", short); // e.g., "GABCD"
/// ```
pub fn to_short_string(node_id: &NodeId) -> String {
    match &node_id.0 {
        stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(key) => {
            // Convert to hex and take first 5 characters
            let hex = hex::encode(key.0);
            hex.chars().take(5).collect()
        }
    }
}

/// Render a NodeID as a short string using Stellar key encoding.
///
/// Returns the first 5 characters of the strkey-encoded public key (G...).
/// This provides a more recognizable format for Stellar public keys.
///
/// # Arguments
///
/// * `node_id` - The NodeID to render
///
/// # Returns
///
/// A short string representation of the node ID using strkey format.
pub fn to_short_strkey(node_id: &NodeId) -> String {
    use stellar_strkey::ed25519::PublicKey as StrPublicKey;

    match &node_id.0 {
        stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(key) => {
            let strkey = StrPublicKey(key.0).to_string();
            strkey.chars().take(5).collect()
        }
    }
}

/// Sleep until the given instant, or forever if `None`.
///
/// When `instant` is `Some(when)`, sleeps until `when` (returning immediately
/// if `when` is already past). When `instant` is `None`, awaits
/// `std::future::pending()` — effectively sleeping forever until cancelled
/// via `tokio::select!`.
pub(crate) async fn sleep_until_or_forever(instant: Option<tokio::time::Instant>) {
    match instant {
        Some(when) => {
            let now = tokio::time::Instant::now();
            if when > now {
                tokio::time::sleep(when - now).await;
            }
        }
        None => {
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{
        Limits, ScpBallot, ScpNomination, ScpStatement, ScpStatementExternalize,
        ScpStatementPledges, StellarValue, StellarValueExt, TimePoint, Uint256, Value, WriteXdr,
    };

    fn make_test_stellar_value(tx_set_hash: [u8; 32], close_time: u64) -> StellarValue {
        StellarValue {
            tx_set_hash: stellar_xdr::curr::Hash(tx_set_hash),
            close_time: TimePoint(close_time),
            upgrades: vec![].try_into().unwrap(),
            ext: StellarValueExt::Basic,
        }
    }

    fn make_test_node_id(seed: u8) -> NodeId {
        let mut key = [0u8; 32];
        key[0] = seed;
        NodeId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(Uint256(
            key,
        )))
    }

    fn encode_value(sv: &StellarValue) -> Value {
        let bytes = sv.to_xdr(Limits::none()).unwrap();
        Value(bytes.try_into().unwrap())
    }

    #[test]
    fn test_get_stellar_values_from_nominate() {
        let sv1 = make_test_stellar_value([1u8; 32], 100);
        let sv2 = make_test_stellar_value([2u8; 32], 200);

        let statement = ScpStatement {
            node_id: make_test_node_id(1),
            slot_index: 1,
            pledges: ScpStatementPledges::Nominate(ScpNomination {
                quorum_set_hash: stellar_xdr::curr::Hash([0u8; 32]),
                votes: vec![encode_value(&sv1)].try_into().unwrap(),
                accepted: vec![encode_value(&sv2)].try_into().unwrap(),
            }),
        };

        let values = get_stellar_values(&statement);
        assert_eq!(values.len(), 2);
        assert_eq!(values[0].tx_set_hash.0, [1u8; 32]);
        assert_eq!(values[0].close_time.0, 100);
        assert_eq!(values[1].tx_set_hash.0, [2u8; 32]);
        assert_eq!(values[1].close_time.0, 200);
    }

    #[test]
    fn test_get_stellar_values_from_externalize() {
        let sv = make_test_stellar_value([3u8; 32], 300);

        let statement = ScpStatement {
            node_id: make_test_node_id(1),
            slot_index: 1,
            pledges: ScpStatementPledges::Externalize(ScpStatementExternalize {
                commit: ScpBallot {
                    counter: 1,
                    value: encode_value(&sv),
                },
                n_h: 1,
                commit_quorum_set_hash: stellar_xdr::curr::Hash([0u8; 32]),
            }),
        };

        let values = get_stellar_values(&statement);
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].tx_set_hash.0, [3u8; 32]);
        assert_eq!(values[0].close_time.0, 300);
    }

    #[test]
    fn test_get_tx_set_hashes_from_envelope() {
        let sv = make_test_stellar_value([4u8; 32], 400);

        let envelope = ScpEnvelope {
            statement: ScpStatement {
                node_id: make_test_node_id(1),
                slot_index: 1,
                pledges: ScpStatementPledges::Externalize(ScpStatementExternalize {
                    commit: ScpBallot {
                        counter: 1,
                        value: encode_value(&sv),
                    },
                    n_h: 1,
                    commit_quorum_set_hash: stellar_xdr::curr::Hash([0u8; 32]),
                }),
            },
            signature: stellar_xdr::curr::Signature(vec![0u8; 64].try_into().unwrap()),
        };

        let hashes = get_tx_set_hashes_from_envelope(&envelope);
        assert_eq!(hashes.len(), 1);
        assert_eq!(hashes[0].0, [4u8; 32]);
    }

    #[test]
    fn test_to_short_string() {
        let node_id = make_test_node_id(0xAB);
        let short = to_short_string(&node_id);
        // First byte is 0xAB, rest are 0x00
        assert_eq!(short.len(), 5);
        assert!(short.starts_with("ab")); // hex encoding of 0xAB
    }

    #[test]
    fn test_to_short_strkey() {
        let node_id = make_test_node_id(0);
        let short = to_short_strkey(&node_id);
        assert_eq!(short.len(), 5);
        // Strkey format starts with 'G'
        assert!(short.starts_with('G'));
    }

    #[test]
    fn test_get_stellar_values_with_invalid_data() {
        // Create a statement with invalid (non-StellarValue) data
        let statement = ScpStatement {
            node_id: make_test_node_id(1),
            slot_index: 1,
            pledges: ScpStatementPledges::Nominate(ScpNomination {
                quorum_set_hash: stellar_xdr::curr::Hash([0u8; 32]),
                votes: vec![Value(vec![1, 2, 3].try_into().unwrap())]
                    .try_into()
                    .unwrap(),
                accepted: vec![].try_into().unwrap(),
            }),
        };

        // Should return empty vec (invalid values are skipped)
        let values = get_stellar_values(&statement);
        assert!(values.is_empty());
    }
}
