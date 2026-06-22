//! Ledger header field validation.
//!
//! LEDGER_SPEC §15 — Invariants and Safety Properties.
//!
//! Matches stellar-core `LedgerHeaderUtils::isValid()` bounds checks.
//! Placed in `henyey-common` so both `henyey-ledger` and `henyey-db` can
//! call it without introducing a circular crate dependency.

use stellar_xdr::LedgerHeader;

/// Validate ledger header field invariants.
///
/// Checks match stellar-core `LedgerHeaderUtils::isValid()`:
/// - `fee_pool >= 0`
/// - `ledger_seq <= i32::MAX`
/// - `id_pool <= i64::MAX`
/// - `close_time <= i64::MAX`
///
/// Returns `Ok(())` if all checks pass, or `Err(description)` on the first
/// failing check.
pub fn validate_header_fields(header: &LedgerHeader) -> Result<(), String> {
    if header.fee_pool < 0 {
        return Err(format!(
            "fee_pool is negative: {} (ledger_seq={})",
            header.fee_pool, header.ledger_seq
        ));
    }

    if header.ledger_seq > i32::MAX as u32 {
        return Err(format!(
            "ledger_seq exceeds i32::MAX: {} > {}",
            header.ledger_seq,
            i32::MAX
        ));
    }

    // id_pool is u64 in XDR but spec bounds it to INT64_MAX
    if header.id_pool > i64::MAX as u64 {
        return Err(format!(
            "id_pool exceeds i64::MAX: {} > {} (ledger_seq={})",
            header.id_pool,
            i64::MAX,
            header.ledger_seq
        ));
    }

    // close_time is u64 (TimePoint) but spec bounds it to INT64_MAX
    if header.scp_value.close_time.0 > i64::MAX as u64 {
        return Err(format!(
            "close_time exceeds i64::MAX: {} > {} (ledger_seq={})",
            header.scp_value.close_time.0,
            i64::MAX,
            header.ledger_seq
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::{Hash, StellarValue, StellarValueExt};

    fn make_valid_header() -> LedgerHeader {
        LedgerHeader {
            ledger_version: 22,
            previous_ledger_hash: Hash([0u8; 32]),
            scp_value: StellarValue {
                tx_set_hash: Hash([0u8; 32]),
                close_time: stellar_xdr::TimePoint(1_000_000),
                upgrades: stellar_xdr::VecM::default(),
                ext: StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash([0u8; 32]),
            bucket_list_hash: Hash([0u8; 32]),
            ledger_seq: 100,
            total_coins: 1_000_000_000,
            fee_pool: 500,
            inflation_seq: 0,
            id_pool: 42,
            base_fee: 100,
            base_reserve: 100_000_000,
            max_tx_set_size: 100,
            skip_list: std::array::from_fn(|_| Hash([0u8; 32])),
            ext: stellar_xdr::LedgerHeaderExt::V0,
        }
    }

    #[test]
    fn test_valid_header_passes() {
        let header = make_valid_header();
        assert!(validate_header_fields(&header).is_ok());
    }

    #[test]
    fn test_zero_fee_pool_passes() {
        let mut header = make_valid_header();
        header.fee_pool = 0;
        assert!(validate_header_fields(&header).is_ok());
    }

    #[test]
    fn test_negative_fee_pool_fails() {
        let mut header = make_valid_header();
        header.fee_pool = -1;
        let err = validate_header_fields(&header).unwrap_err();
        assert!(err.contains("fee_pool is negative"), "{}", err);
    }

    #[test]
    fn test_ledger_seq_at_i32_max_passes() {
        let mut header = make_valid_header();
        header.ledger_seq = i32::MAX as u32;
        assert!(validate_header_fields(&header).is_ok());
    }

    #[test]
    fn test_ledger_seq_exceeds_i32_max_fails() {
        let mut header = make_valid_header();
        header.ledger_seq = i32::MAX as u32 + 1;
        let err = validate_header_fields(&header).unwrap_err();
        assert!(err.contains("ledger_seq exceeds i32::MAX"), "{}", err);
    }

    #[test]
    fn test_id_pool_at_i64_max_passes() {
        let mut header = make_valid_header();
        header.id_pool = i64::MAX as u64;
        assert!(validate_header_fields(&header).is_ok());
    }

    #[test]
    fn test_id_pool_exceeds_i64_max_fails() {
        let mut header = make_valid_header();
        header.id_pool = i64::MAX as u64 + 1;
        let err = validate_header_fields(&header).unwrap_err();
        assert!(err.contains("id_pool exceeds i64::MAX"), "{}", err);
    }

    #[test]
    fn test_close_time_at_i64_max_passes() {
        let mut header = make_valid_header();
        header.scp_value.close_time = stellar_xdr::TimePoint(i64::MAX as u64);
        assert!(validate_header_fields(&header).is_ok());
    }

    #[test]
    fn test_close_time_exceeds_i64_max_fails() {
        let mut header = make_valid_header();
        header.scp_value.close_time = stellar_xdr::TimePoint(i64::MAX as u64 + 1);
        let err = validate_header_fields(&header).unwrap_err();
        assert!(err.contains("close_time exceeds i64::MAX"), "{}", err);
    }

    #[test]
    fn test_genesis_header_passes() {
        // Genesis header has fee_pool=0, total_coins=large, seq=1
        let mut header = make_valid_header();
        header.ledger_seq = 1;
        header.fee_pool = 0;
        header.total_coins = 1_000_000_000_000_000_000;
        header.id_pool = 0;
        header.ledger_version = 0;
        assert!(validate_header_fields(&header).is_ok());
    }
}
