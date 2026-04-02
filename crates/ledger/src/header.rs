//! Ledger header construction, hashing, and verification.
//!
//! This module provides utilities for working with Stellar ledger headers,
//! which form the cryptographic backbone of the ledger chain.
//!
//! # Header Hash
//!
//! Each ledger header is identified by its SHA-256 hash, computed over the
//! XDR-encoded header. This hash:
//!
//! - Links to the previous ledger via `previous_ledger_hash`
//! - Is referenced by the next ledger as its previous hash
//! - Provides integrity verification for the entire chain
//!
//! # Skip List
//!
//! The skip list enables O(log n) traversal backward through the ledger chain.
//! Each header contains 4 skip list entries pointing to:
//!
//! - Entry 0: Previous ledger (seq - 1)
//! - Entry 1: Ledger at seq - (seq mod 4), skipping back by up to 4
//! - Entry 2: Ledger at seq - (seq mod 16), skipping back by up to 16
//! - Entry 3: Ledger at seq - (seq mod 64), skipping back by up to 64
//!
//! This structure allows efficient verification of historical headers without
//! traversing every intermediate ledger.

use crate::{LedgerError, Result};
use henyey_common::Hash256;
use stellar_xdr::curr::{LedgerHeader, Limits, WriteXdr};

/// Number of entries in the skip list (fixed at 4 by protocol).
pub const SKIP_LIST_SIZE: usize = 4;

/// Skip list update intervals (from stellar-core).
/// The skip list stores bucket_list_hash values at these intervals.
pub const SKIP_1: u32 = 50;
pub const SKIP_2: u32 = 5000;
pub const SKIP_3: u32 = 50000;
pub const SKIP_4: u32 = 500000;

/// Variable fields needed to construct the next ledger header.
pub struct NextHeaderFields {
    pub close_time: u64,
    pub tx_set_hash: Hash256,
    pub bucket_list_hash: Hash256,
    pub tx_set_result_hash: Hash256,
    pub total_coins: i64,
    pub fee_pool: i64,
    pub inflation_seq: u32,
    pub stellar_value_ext: stellar_xdr::curr::StellarValueExt,
}

/// Compute the canonical hash of a ledger header.
///
/// The header hash is the SHA-256 digest of the XDR-encoded header.
/// This hash uniquely identifies the ledger and is used for:
///
/// - Chain linking via `previous_ledger_hash`
/// - Transaction set anchoring via SCP
/// - History archive verification
///
/// # Errors
///
/// Returns an error if XDR encoding fails (should not happen for valid headers).
pub fn compute_header_hash(header: &LedgerHeader) -> Result<Hash256> {
    let xdr_bytes = header.to_xdr(Limits::none())?;
    Ok(Hash256::hash(&xdr_bytes))
}

/// Update the skip list for a ledger header based on its bucket list hash.
///
/// The skip list contains bucket_list_hash values at specific ledger intervals,
/// enabling efficient verification of bucket list state at historical points.
///
/// # Algorithm (from stellar-core BucketManager::calculateSkipValues)
///
/// The skip list is only updated when ledger_seq is divisible by SKIP_1 (50).
/// At those points:
/// - `skipList[0]` = current bucket_list_hash
/// - `skipList[1]` = previous `skipList[0]` (when appropriate interval conditions are met)
/// - `skipList[2]` = previous `skipList[1]` (when appropriate interval conditions are met)
/// - `skipList[3]` = previous `skipList[2]` (when appropriate interval conditions are met)
///
/// The cascading updates happen based on complex interval conditions:
/// - `skipList[1]` updates when (seq - SKIP_1) % SKIP_2 == 0
/// - `skipList[2]` updates when additionally (seq - SKIP_2 - SKIP_1) % SKIP_3 == 0
/// - `skipList[3]` updates when additionally (seq - SKIP_3 - SKIP_2 - SKIP_1) % SKIP_4 == 0
///
/// # Arguments
///
/// * `header` - The ledger header to update (modified in place)
///
/// # Note
///
/// This must be called after setting the bucket_list_hash but before computing
/// the header hash.
pub fn calculate_skip_values(header: &mut LedgerHeader) {
    let seq = header.ledger_seq;

    if seq % SKIP_1 == 0 {
        let v = seq.saturating_sub(SKIP_1) as i64;
        if v > 0 && (v as u32) % SKIP_2 == 0 {
            let v2 = seq.saturating_sub(SKIP_2).saturating_sub(SKIP_1) as i64;
            if v2 > 0 && (v2 as u32) % SKIP_3 == 0 {
                let v3 = seq
                    .saturating_sub(SKIP_3)
                    .saturating_sub(SKIP_2)
                    .saturating_sub(SKIP_1) as i64;
                if v3 > 0 && (v3 as u32) % SKIP_4 == 0 {
                    header.skip_list[3] = header.skip_list[2].clone();
                }
                header.skip_list[2] = header.skip_list[1].clone();
            }
            header.skip_list[1] = header.skip_list[0].clone();
        }
        header.skip_list[0] = header.bucket_list_hash.clone();
    }
}

/// Calculate the target ledger sequence for a skip list entry.
///
/// Given a ledger sequence and skip list index, computes which historical
/// ledger that skip list entry points to.
///
/// # Arguments
///
/// * `current_seq` - The ledger sequence containing the skip list
/// * `skip_index` - Which skip list entry (0-3)
///
/// # Returns
///
/// The target ledger sequence, or `None` if:
/// - The index is out of range (>= 4)
/// - The target would be before genesis (sequence 0)
pub fn skip_list_target_seq(current_seq: u32, skip_index: usize) -> Option<u32> {
    if skip_index >= SKIP_LIST_SIZE {
        return None;
    }

    fn round_back(current_seq: u32, interval: u32) -> u32 {
        let rem = current_seq % interval;
        if rem == 0 { interval } else { rem }
    }

    let delta = match skip_index {
        0 => 1,
        1 => round_back(current_seq, 4),
        2 => round_back(current_seq, 16),
        3 => round_back(current_seq, 64),
        _ => return None,
    };

    current_seq.checked_sub(delta)
}

/// Verify that a ledger header correctly chains to its predecessor.
///
/// This validates the cryptographic chain integrity by checking:
///
/// 1. **Sequence continuity**: The new ledger's sequence is exactly one
///    greater than the previous ledger's sequence.
///
/// 2. **Hash linking**: The new ledger's `previous_ledger_hash` matches
///    the computed hash of the previous header.
///
/// # Errors
///
/// - [`LedgerError::InvalidSequence`]: Sequence numbers are not consecutive
/// - [`LedgerError::HashMismatch`]: Previous hash doesn't match
pub fn verify_header_chain(
    prev_header: &LedgerHeader,
    prev_header_hash: &Hash256,
    current_header: &LedgerHeader,
) -> Result<()> {
    let expected_seq = prev_header.ledger_seq + 1;
    if current_header.ledger_seq != expected_seq {
        return Err(LedgerError::InvalidSequence {
            expected: expected_seq,
            actual: current_header.ledger_seq,
        });
    }

    let current_prev_hash = Hash256::from(current_header.previous_ledger_hash.0);
    if &current_prev_hash != prev_header_hash {
        return Err(LedgerError::HashMismatch {
            expected: prev_header_hash.to_hex(),
            actual: current_prev_hash.to_hex(),
        });
    }

    Ok(())
}

/// Verify the skip list entries against historical headers.
///
/// This validates that each non-zero skip list entry correctly points
/// to the expected historical header hash.
///
/// # Arguments
///
/// * `header` - The header whose skip list to verify
/// * `get_header_at_seq` - Function to look up historical header hashes
///
/// # Verification
///
/// For each skip list entry, if the target sequence exists and can be
/// looked up, the stored hash must match the actual header hash at that
/// sequence. Zero hashes are skipped (valid for genesis/early ledgers).
pub fn verify_skip_list(
    header: &LedgerHeader,
    get_header_at_seq: impl Fn(u32) -> Option<Hash256>,
) -> Result<()> {
    for (i, skip_hash) in header.skip_list.iter().enumerate() {
        let skip_hash256 = Hash256::from(skip_hash.0);

        if skip_hash256.is_zero() {
            continue;
        }

        if let Some(target_seq) = skip_list_target_seq(header.ledger_seq, i) {
            if let Some(expected_hash) = get_header_at_seq(target_seq) {
                if skip_hash256 != expected_hash {
                    return Err(LedgerError::InvalidHeaderChain(format!(
                        "skip list entry {} mismatch at seq {}: expected {}, got {}",
                        i,
                        target_seq,
                        expected_hash.to_hex(),
                        skip_hash256.to_hex()
                    )));
                }
            }
        }
    }

    Ok(())
}

/// Create a new ledger header for the next ledger in the chain.
///
/// Constructs a complete ledger header with all required fields populated.
/// The skip list is automatically computed based on the previous header.
///
/// # Arguments
///
/// * `prev_header` - The previous ledger's header (for version, fees, reserves)
/// * `prev_header_hash` - Hash of the previous header (for linking)
/// * `close_time` - Unix timestamp for this ledger's close
/// * `tx_set_hash` - Hash of the transaction set applied
/// * `bucket_list_hash` - Root hash of the updated bucket list
/// * `tx_set_result_hash` - Hash of the transaction results
/// * `total_coins` - Updated total coins in circulation
/// * `fee_pool` - Updated fee pool balance
/// * `inflation_seq` - Inflation sequence counter
/// * `stellar_value_ext` - The StellarValue extension from consensus (Basic or Signed)
///
/// # Note
///
/// The returned header inherits `base_fee`, `base_reserve`, and `max_tx_set_size`
/// from the previous header. These can be modified by protocol upgrades after
/// header creation.
///
/// The skip_list is copied from the previous header and then updated via
/// `calculate_skip_values` based on the new bucket_list_hash.
pub fn create_next_header(
    prev_header: &LedgerHeader,
    prev_header_hash: Hash256,
    fields: NextHeaderFields,
) -> LedgerHeader {
    let new_seq = prev_header.ledger_seq + 1;

    let mut header = LedgerHeader {
        ledger_version: prev_header.ledger_version,
        previous_ledger_hash: prev_header_hash.into(),
        scp_value: stellar_xdr::curr::StellarValue {
            tx_set_hash: fields.tx_set_hash.into(),
            close_time: stellar_xdr::curr::TimePoint(fields.close_time),
            upgrades: stellar_xdr::curr::VecM::default(),
            ext: fields.stellar_value_ext,
        },
        tx_set_result_hash: fields.tx_set_result_hash.into(),
        bucket_list_hash: fields.bucket_list_hash.into(),
        ledger_seq: new_seq,
        total_coins: fields.total_coins,
        fee_pool: fields.fee_pool,
        inflation_seq: fields.inflation_seq,
        id_pool: prev_header.id_pool,
        base_fee: prev_header.base_fee,
        base_reserve: prev_header.base_reserve,
        max_tx_set_size: prev_header.max_tx_set_size,
        skip_list: prev_header.skip_list.clone(),
        ext: stellar_xdr::curr::LedgerHeaderExt::V0,
    };

    calculate_skip_values(&mut header);

    header
}

/// Extract the ledger close time from a header.
///
/// Returns the Unix timestamp (seconds since epoch) when this ledger was closed.
pub fn close_time(header: &LedgerHeader) -> u64 {
    header.scp_value.close_time.0
}

/// Extract the protocol version from a header.
///
/// The protocol version determines which features and behaviors are active
/// for transactions processed in this ledger.
pub fn protocol_version(header: &LedgerHeader) -> u32 {
    header.ledger_version
}

/// Check if a header predates a given protocol version.
///
/// Useful for conditional logic based on protocol capabilities.
///
/// # Example
///
/// ```ignore
/// if is_before_protocol_version(header, 20) {
///     // Soroban not available
/// }
/// ```
pub fn is_before_protocol_version(header: &LedgerHeader, version: u32) -> bool {
    header.ledger_version < version
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::Hash;

    fn create_test_header(seq: u32) -> LedgerHeader {
        LedgerHeader {
            ledger_version: 20,
            previous_ledger_hash: Hash([0u8; 32]),
            scp_value: stellar_xdr::curr::StellarValue {
                tx_set_hash: Hash([0u8; 32]),
                close_time: stellar_xdr::curr::TimePoint(1000 + seq as u64),
                upgrades: stellar_xdr::curr::VecM::default(),
                ext: stellar_xdr::curr::StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash([0u8; 32]),
            bucket_list_hash: Hash([0u8; 32]),
            ledger_seq: seq,
            total_coins: 100_000_000_000_000_000,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 5_000_000,
            max_tx_set_size: 1000,
            skip_list: std::array::from_fn(|_| Hash([0u8; 32])),
            ext: stellar_xdr::curr::LedgerHeaderExt::V0,
        }
    }

    #[test]
    fn test_compute_header_hash() {
        let header = create_test_header(1);
        let hash = compute_header_hash(&header).unwrap();
        assert!(!hash.is_zero());

        let hash2 = compute_header_hash(&header).unwrap();
        assert_eq!(hash, hash2);

        let header2 = create_test_header(2);
        let hash3 = compute_header_hash(&header2).unwrap();
        assert_ne!(hash, hash3);
    }

    #[test]
    fn test_calculate_skip_values() {
        let mut header = create_test_header(5);
        header.bucket_list_hash = Hash([1u8; 32]);
        calculate_skip_values(&mut header);
        assert_eq!(header.skip_list[0], Hash([0u8; 32]));

        let mut header = create_test_header(SKIP_1);
        header.bucket_list_hash = Hash([2u8; 32]);
        calculate_skip_values(&mut header);
        assert_eq!(header.skip_list[0], Hash([2u8; 32]));

        let mut header = create_test_header(SKIP_2 + SKIP_1);
        header.skip_list[0] = Hash([3u8; 32]);
        header.bucket_list_hash = Hash([4u8; 32]);
        calculate_skip_values(&mut header);
        assert_eq!(header.skip_list[0], Hash([4u8; 32]));
        assert_eq!(header.skip_list[1], Hash([3u8; 32]));
    }

    #[test]
    fn test_skip_list_target_seq() {
        assert_eq!(skip_list_target_seq(10, 0), Some(9));
        assert_eq!(skip_list_target_seq(1, 0), Some(0));
        assert_eq!(skip_list_target_seq(8, 1), Some(4));
        assert_eq!(skip_list_target_seq(10, 1), Some(8));
        assert_eq!(skip_list_target_seq(0, 0), None);
    }
}
