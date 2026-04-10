//! Ledger close data for consensus output.
//!
//! This module provides the [`LedgerCloseData`] struct that encapsulates all
//! the information needed to close a ledger after SCP consensus has been
//! reached. This matches the stellar-core `LedgerCloseData` class.
//!
//! The key difference from the existing [`crate::LedgerCloseInfo`] is that
//! `LedgerCloseData` includes the full XDR representation and expected hash
//! validation for replay and verification.

use henyey_common::Hash256;
use henyey_common::LedgerSeq;
use stellar_xdr::curr::{Limits, ReadXdr, StellarValue, StoredDebugTransactionSet};

#[cfg(feature = "test-utils")]
use stellar_xdr::curr::TransactionResultSet;

use crate::tx_queue::TransactionSet;

/// Complete ledger close information from SCP consensus.
///
/// This structure wraps all the data needed to close a ledger, including:
/// - The ledger sequence number
/// - The transaction set (as XDR for wire transmission)
/// - The consensus value (StellarValue with close time, upgrades, etc.)
/// - Optional expected ledger hash for validation
///
/// # Relationship to LedgerCloseInfo
///
/// While [`crate::LedgerCloseInfo`] is used for in-memory ledger close
/// processing, `LedgerCloseData` is designed for:
/// - XDR serialization for storage and transmission
/// - Expected hash validation during replay
/// - Test fixtures with expected results
///
/// # Example
///
/// ```ignore
/// use henyey_herder::{LedgerCloseData, TransactionSet};
/// use stellar_xdr::curr::StellarValue;
///
/// let tx_set = TransactionSet::new(prev_hash, transactions);
/// let stellar_value = /* ... */;
///
/// let lcd = LedgerCloseData::new(100, tx_set, stellar_value, None);
///
/// // Serialize for storage
/// let xdr = lcd.to_xdr();
///
/// // Deserialize from storage
/// let restored = LedgerCloseData::from_xdr(xdr)?;
/// ```
#[derive(Debug, Clone)]
pub struct LedgerCloseData {
    /// The ledger sequence number this close is for.
    ledger_seq: LedgerSeq,
    /// The transaction set agreed upon by consensus.
    tx_set: TransactionSet,
    /// The consensus value containing close time, tx set hash, and upgrades.
    value: StellarValue,
    /// Optional expected ledger hash for validation during replay.
    expected_ledger_hash: Option<Hash256>,
    /// Optional expected transaction results for test validation.
    #[cfg(feature = "test-utils")]
    expected_results: Option<TransactionResultSet>,
}

impl LedgerCloseData {
    /// Create new ledger close data.
    ///
    /// # Arguments
    ///
    /// * `ledger_seq` - The sequence number of the ledger being closed
    /// * `tx_set` - The transaction set from consensus
    /// * `value` - The StellarValue containing close time and upgrades
    /// * `expected_ledger_hash` - Optional expected hash for validation
    ///
    /// # Panics
    ///
    /// Panics in debug mode if the transaction set hash doesn't match
    /// the tx_set_hash in the StellarValue.
    pub fn new(
        ledger_seq: LedgerSeq,
        tx_set: TransactionSet,
        value: StellarValue,
        expected_ledger_hash: Option<Hash256>,
    ) -> Self {
        // Verify the tx set hash matches the value's tx_set_hash
        debug_assert_eq!(
            tx_set.hash.0, value.tx_set_hash.0,
            "Transaction set hash mismatch"
        );

        Self {
            ledger_seq,
            tx_set,
            value,
            expected_ledger_hash,
            #[cfg(feature = "test-utils")]
            expected_results: None,
        }
    }

    /// Create new ledger close data with expected results (for testing).
    #[cfg(feature = "test-utils")]
    pub fn with_expected_results(
        ledger_seq: LedgerSeq,
        tx_set: TransactionSet,
        value: StellarValue,
        expected_ledger_hash: Option<Hash256>,
        expected_results: Option<TransactionResultSet>,
    ) -> Self {
        debug_assert_eq!(
            tx_set.hash.0, value.tx_set_hash.0,
            "Transaction set hash mismatch"
        );

        Self {
            ledger_seq,
            tx_set,
            value,
            expected_ledger_hash,
            expected_results,
        }
    }

    /// Get the ledger sequence number.
    pub fn ledger_seq(&self) -> LedgerSeq {
        self.ledger_seq
    }

    /// Get the transaction set.
    pub fn tx_set(&self) -> &TransactionSet {
        &self.tx_set
    }

    /// Get the consensus value.
    pub fn value(&self) -> &StellarValue {
        &self.value
    }

    /// Get the expected ledger hash (if set).
    pub fn expected_hash(&self) -> Option<&Hash256> {
        self.expected_ledger_hash.as_ref()
    }

    /// Get the expected transaction results (if set, test only).
    #[cfg(feature = "test-utils")]
    pub fn expected_results(&self) -> Option<&TransactionResultSet> {
        self.expected_results.as_ref()
    }

    /// Get the close time from the StellarValue.
    pub fn close_time(&self) -> u64 {
        self.value.close_time.0
    }

    /// Get the upgrades from the StellarValue.
    pub fn upgrades(&self) -> &[stellar_xdr::curr::UpgradeType] {
        &self.value.upgrades
    }

    /// Get the transaction set hash.
    pub fn tx_set_hash(&self) -> Hash256 {
        self.tx_set.hash
    }

    /// Serialize to StoredDebugTransactionSet XDR.
    ///
    /// This format is used for storing ledger close data for replay/debugging.
    pub fn to_xdr(&self) -> StoredDebugTransactionSet {
        StoredDebugTransactionSet {
            tx_set: self.tx_set.to_xdr_stored_set(),
            scp_value: self.value.clone(),
            ledger_seq: self.ledger_seq.get(),
        }
    }

    /// Deserialize from StoredDebugTransactionSet XDR.
    ///
    /// # Arguments
    ///
    /// * `sts` - The stored debug transaction set
    ///
    /// # Returns
    ///
    /// A `LedgerCloseData` if deserialization succeeds.
    pub fn from_xdr(sts: StoredDebugTransactionSet) -> Result<Self, LedgerCloseDataError> {
        let tx_set = TransactionSet::from_xdr_stored_set(&sts.tx_set)
            .map_err(LedgerCloseDataError::TxSetDecodeError)?;

        // Verify hash matches
        if tx_set.hash.0 != sts.scp_value.tx_set_hash.0 {
            return Err(LedgerCloseDataError::TxSetHashMismatch);
        }

        Ok(Self {
            ledger_seq: sts.ledger_seq.into(),
            tx_set,
            value: sts.scp_value,
            expected_ledger_hash: None,
            #[cfg(feature = "test-utils")]
            expected_results: None,
        })
    }

    /// Check if this ledger close matches an expected hash.
    ///
    /// Returns `true` if no expected hash is set, or if the provided hash
    /// matches the expected hash.
    pub fn validate_hash(&self, actual_hash: &Hash256) -> bool {
        match &self.expected_ledger_hash {
            Some(expected) => expected == actual_hash,
            None => true,
        }
    }
}

/// Errors that can occur when working with LedgerCloseData.
#[derive(Debug, Clone, thiserror::Error)]
pub enum LedgerCloseDataError {
    /// Transaction set hash doesn't match the StellarValue tx_set_hash.
    #[error("transaction set hash mismatch")]
    TxSetHashMismatch,

    /// Failed to decode the transaction set.
    #[error("failed to decode transaction set: {0}")]
    TxSetDecodeError(String),

    /// XDR encoding/decoding error.
    #[error("XDR error: {0}")]
    XdrError(String),
}

impl From<stellar_xdr::curr::Error> for LedgerCloseDataError {
    fn from(e: stellar_xdr::curr::Error) -> Self {
        LedgerCloseDataError::XdrError(e.to_string())
    }
}

/// Render a StellarValue as a human-readable string.
///
/// This matches the stellar-core `stellarValueToString` function.
///
/// # Arguments
///
/// * `sv` - The StellarValue to render
/// * `short_node_id` - Optional function to shorten node IDs (for signed values)
///
/// # Returns
///
/// A string representation of the StellarValue.
pub fn stellar_value_to_string<F>(sv: &StellarValue, short_node_id: Option<F>) -> String
where
    F: Fn(&stellar_xdr::curr::NodeId) -> String,
{
    use stellar_xdr::curr::{LedgerUpgrade, StellarValueExt};

    let mut res = String::from("[");

    // Handle signed values
    if let StellarValueExt::Signed(sig) = &sv.ext {
        if let Some(ref formatter) = short_node_id {
            res.push_str(&format!(" SIGNED@{}", formatter(&sig.node_id)));
        } else {
            res.push_str(" SIGNED");
        }
    }

    // Transaction set hash (abbreviated)
    let tx_hash_hex = hex::encode(sv.tx_set_hash.0);
    let short_hash = &tx_hash_hex[..8.min(tx_hash_hex.len())];
    res.push_str(&format!(" txH: {}", short_hash));

    // Close time
    res.push_str(&format!(", ct: {}", sv.close_time.0));

    // Upgrades
    res.push_str(", upgrades: [");
    for (i, upgrade) in sv.upgrades.iter().enumerate() {
        if i > 0 {
            res.push_str(", ");
        }
        if upgrade.0.is_empty() {
            res.push_str("<empty>");
        } else {
            match LedgerUpgrade::from_xdr(&upgrade.0, Limits::none()) {
                Ok(lupgrade) => {
                    res.push_str(&upgrade_to_string(&lupgrade));
                }
                Err(_) => {
                    res.push_str("<unknown>");
                }
            }
        }
    }
    res.push_str(" ] ]");

    res
}

/// Convert a LedgerUpgrade to a human-readable string.
fn upgrade_to_string(upgrade: &stellar_xdr::curr::LedgerUpgrade) -> String {
    use stellar_xdr::curr::LedgerUpgrade;

    match upgrade {
        LedgerUpgrade::Version(v) => format!("version={}", v),
        LedgerUpgrade::BaseFee(f) => format!("baseFee={}", f),
        LedgerUpgrade::MaxTxSetSize(s) => format!("maxTxSetSize={}", s),
        LedgerUpgrade::BaseReserve(r) => format!("baseReserve={}", r),
        LedgerUpgrade::Flags(f) => format!("flags={}", f),
        LedgerUpgrade::Config(c) => {
            // Config upgrades have contract_id and content_hash
            let hash_hex = hex::encode(c.content_hash.0);
            let short_hash = &hash_hex[..8.min(hash_hex.len())];
            format!("config(hash={})", short_hash)
        }
        LedgerUpgrade::MaxSorobanTxSetSize(s) => format!("maxSorobanTxSetSize={}", s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{Hash, Limits, StellarValueExt, TimePoint, UpgradeType, WriteXdr};

    fn make_test_value(tx_set_hash: [u8; 32], close_time: u64) -> StellarValue {
        StellarValue {
            tx_set_hash: Hash(tx_set_hash),
            close_time: TimePoint(close_time),
            upgrades: vec![].try_into().unwrap(),
            ext: StellarValueExt::Basic,
        }
    }

    #[test]
    fn test_ledger_close_data_new() {
        let prev_hash = [1u8; 32];
        let tx_set = TransactionSet::new(Hash256::from_bytes(prev_hash), Vec::new());
        // Use the computed hash from the tx_set
        let value = make_test_value(tx_set.hash.0, 1000);

        let lcd = LedgerCloseData::new(100.into(), tx_set, value, None);

        assert_eq!(lcd.ledger_seq(), 100);
        assert_eq!(lcd.close_time(), 1000);
        assert!(lcd.expected_hash().is_none());
    }

    #[test]
    fn test_ledger_close_data_with_expected_hash() {
        let prev_hash = [2u8; 32];
        let expected = Hash256::from_bytes([3u8; 32]);
        let tx_set = TransactionSet::new(Hash256::from_bytes(prev_hash), Vec::new());
        // Use the computed hash from the tx_set
        let value = make_test_value(tx_set.hash.0, 2000);

        let lcd = LedgerCloseData::new(200.into(), tx_set, value, Some(expected));

        assert_eq!(lcd.expected_hash(), Some(&expected));
        assert!(lcd.validate_hash(&expected));
        assert!(!lcd.validate_hash(&Hash256::ZERO));
    }

    #[test]
    fn test_stellar_value_to_string_basic() {
        let value = make_test_value([0xABu8; 32], 12345);
        let s = stellar_value_to_string::<fn(&stellar_xdr::curr::NodeId) -> String>(&value, None);

        assert!(s.contains("txH: "));
        assert!(s.contains("ct: 12345"));
        assert!(s.contains("upgrades:"));
    }

    #[test]
    fn test_stellar_value_to_string_with_upgrades() {
        use stellar_xdr::curr::LedgerUpgrade;

        let upgrade = LedgerUpgrade::Version(25);
        let upgrade_bytes = upgrade.to_xdr(Limits::none()).unwrap();

        let value = StellarValue {
            tx_set_hash: Hash([0u8; 32]),
            close_time: TimePoint(1000),
            upgrades: vec![UpgradeType(upgrade_bytes.try_into().unwrap())]
                .try_into()
                .unwrap(),
            ext: StellarValueExt::Basic,
        };

        let s = stellar_value_to_string::<fn(&stellar_xdr::curr::NodeId) -> String>(&value, None);
        assert!(s.contains("version=25"));
    }

    #[test]
    fn test_upgrade_to_string() {
        use stellar_xdr::curr::LedgerUpgrade;

        assert_eq!(upgrade_to_string(&LedgerUpgrade::Version(25)), "version=25");
        assert_eq!(
            upgrade_to_string(&LedgerUpgrade::BaseFee(100)),
            "baseFee=100"
        );
        assert_eq!(
            upgrade_to_string(&LedgerUpgrade::MaxTxSetSize(1000)),
            "maxTxSetSize=1000"
        );
    }
}
