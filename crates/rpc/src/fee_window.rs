//! Sliding-window fee statistics tracking.
//!
//! Mirrors the upstream `stellar-rpc` `feewindow` package, maintaining separate
//! fee distributions for classic (fee-per-operation) and Soroban (inclusion fee)
//! transactions over a configurable retention window.
//!
//! # Architecture
//!
//! [`FeeWindows`] holds two [`FeeWindow`] instances — one for classic fees and
//! one for Soroban inclusion fees. Each window is a ring buffer of per-ledger
//! fee vectors, sized by the RPC retention window (default 2880 ledgers ≈ 4h).
//!
//! On each new ledger, [`FeeWindows::ingest_ledger_close_meta`] parses the
//! `LedgerCloseMeta` XDR, extracts fees from every transaction, and appends
//! them to the appropriate window. The distribution is recomputed eagerly so
//! that [`FeeWindows::get_classic_distribution`] and
//! [`FeeWindows::get_soroban_distribution`] are O(1) reads.
//!
//! # Fee extraction
//!
//! - **Classic**: `feeCharged / numOps` (fee per operation)
//! - **Soroban**: `feeCharged - resourceFeeCharged`, where `resourceFeeCharged`
//!   = `totalNonRefundableResourceFeeCharged + totalRefundableResourceFeeCharged`
//!   from `SorobanTransactionMetaExtV1`.

use std::sync::RwLock;

use stellar_xdr::curr::{
    InnerTransactionResultResult, LedgerCloseMeta, Limits, ReadXdr, SorobanTransactionMetaExt,
    TransactionMeta, TransactionResultPair, TransactionResultResult,
};

// ---------------------------------------------------------------------------
// FeeDistribution
// ---------------------------------------------------------------------------

/// Computed fee distribution over the window, matching the upstream JSON shape.
#[derive(Clone, Debug, Default)]
pub(crate) struct FeeDistribution {
    pub max: u64,
    pub min: u64,
    pub mode: u64,
    pub p10: u64,
    pub p20: u64,
    pub p30: u64,
    pub p40: u64,
    pub p50: u64,
    pub p60: u64,
    pub p70: u64,
    pub p80: u64,
    pub p90: u64,
    pub p95: u64,
    pub p99: u64,
    pub fee_count: u32,
    pub ledger_count: u32,
}

/// Compute the nearest-rank percentile distribution for a set of fee values.
///
/// Algorithm matches upstream exactly:
/// - Sort fees ascending
/// - Mode = most-repeated value (ties broken by first occurrence)
/// - Percentile P: `kth = ceil(p * count / 100)`, value = `fees[kth - 1]`
pub(crate) fn compute_fee_distribution(fees: &mut [u64], ledger_count: u32) -> FeeDistribution {
    if fees.is_empty() {
        return FeeDistribution::default();
    }

    fees.sort_unstable();

    // Compute mode (value with highest frequency)
    let mut mode = fees[0];
    let mut max_repetitions = 0u32;
    let mut local_repetitions = 0u32;
    let mut last_val = fees[0];

    for &fee in &fees[1..] {
        if fee == last_val {
            local_repetitions += 1;
            continue;
        }
        // New cluster
        if local_repetitions > max_repetitions {
            max_repetitions = local_repetitions;
            mode = last_val;
        }
        last_val = fee;
        local_repetitions = 0;
    }
    // Check final cluster
    if local_repetitions > max_repetitions {
        mode = fees[fees.len() - 1];
    }

    let count = fees.len() as u64;
    let percentile = |p: u64| -> u64 {
        // ceiling(p * count / 100)
        let kth = (p * count).div_ceil(100);
        fees[(kth - 1) as usize]
    };

    FeeDistribution {
        max: fees[fees.len() - 1],
        min: fees[0],
        mode,
        p10: percentile(10),
        p20: percentile(20),
        p30: percentile(30),
        p40: percentile(40),
        p50: percentile(50),
        p60: percentile(60),
        p70: percentile(70),
        p80: percentile(80),
        p90: percentile(90),
        p95: percentile(95),
        p99: percentile(99),
        fee_count: fees.len() as u32,
        ledger_count,
    }
}

// ---------------------------------------------------------------------------
// Ring buffer (LedgerBucketWindow)
// ---------------------------------------------------------------------------

/// A single ledger's fee data.
struct LedgerBucket {
    ledger_seq: u32,
    fees: Vec<u64>,
}

/// Circular buffer of per-ledger fee vectors, sized by the retention window.
struct LedgerBucketWindow {
    buckets: Vec<LedgerBucket>,
    start: usize,
    capacity: u32,
}

impl LedgerBucketWindow {
    fn new(capacity: u32) -> Self {
        Self {
            buckets: Vec::with_capacity(capacity as usize),
            start: 0,
            capacity,
        }
    }

    fn len(&self) -> u32 {
        self.buckets.len() as u32
    }

    fn get(&self, i: u32) -> &LedgerBucket {
        let index = (self.start + i as usize) % self.buckets.len();
        &self.buckets[index]
    }

    /// Returns the latest ledger sequence in the window, or 0 if empty.
    fn latest_ledger(&self) -> u32 {
        if self.buckets.is_empty() {
            0
        } else {
            self.get(self.len() - 1).ledger_seq
        }
    }

    /// Append a new ledger's fees. Returns error if ledgers are not contiguous.
    fn append(&mut self, seq: u32, fees: Vec<u64>) -> Result<(), String> {
        let length = self.len();
        if length > 0 {
            let expected = self.get(0).ledger_seq + length;
            if expected != seq {
                return Err(format!(
                    "ledgers not contiguous: expected {expected} but got {seq}"
                ));
            }
        }

        let bucket = LedgerBucket {
            ledger_seq: seq,
            fees,
        };

        if length < self.capacity {
            self.buckets.push(bucket);
        } else {
            // Overwrite oldest and advance start
            self.buckets[self.start] = bucket;
            self.start = (self.start + 1) % self.buckets.len();
        }

        Ok(())
    }

    /// Collect all fees across all ledgers in the window.
    fn all_fees(&self) -> Vec<u64> {
        let mut all = Vec::new();
        for i in 0..self.len() {
            all.extend_from_slice(&self.get(i).fees);
        }
        all
    }

    /// Reset the window, clearing all data.
    fn reset(&mut self) {
        self.buckets.clear();
        self.start = 0;
    }
}

// ---------------------------------------------------------------------------
// FeeWindow (single window with lock)
// ---------------------------------------------------------------------------

/// A single fee window (either classic or Soroban) with interior mutability.
struct FeeWindow {
    inner: RwLock<FeeWindowInner>,
}

struct FeeWindowInner {
    window: LedgerBucketWindow,
    distribution: FeeDistribution,
}

impl FeeWindow {
    fn new(retention: u32) -> Self {
        Self {
            inner: RwLock::new(FeeWindowInner {
                window: LedgerBucketWindow::new(retention),
                distribution: FeeDistribution::default(),
            }),
        }
    }

    fn append(&self, seq: u32, fees: Vec<u64>) -> Result<(), String> {
        let mut inner = self.inner.write().unwrap();
        inner.window.append(seq, fees)?;
        let mut all_fees = inner.window.all_fees();
        inner.distribution = compute_fee_distribution(&mut all_fees, inner.window.len());
        Ok(())
    }

    fn distribution(&self) -> FeeDistribution {
        self.inner.read().unwrap().distribution.clone()
    }

    fn latest_ledger(&self) -> u32 {
        self.inner.read().unwrap().window.latest_ledger()
    }

    fn reset(&self) {
        let mut inner = self.inner.write().unwrap();
        inner.window.reset();
        inner.distribution = FeeDistribution::default();
    }
}

// ---------------------------------------------------------------------------
// FeeWindows (the public-facing composite)
// ---------------------------------------------------------------------------

/// Combined fee windows for classic and Soroban inclusion fees.
///
/// Thread-safe: each window uses an internal `RwLock`. Multiple readers can
/// query distributions concurrently while a single writer ingests new ledgers.
pub(crate) struct FeeWindows {
    classic: FeeWindow,
    soroban: FeeWindow,
}

impl FeeWindows {
    /// Create new fee windows with the given retention (number of ledgers).
    pub(crate) fn new(retention: u32) -> Self {
        Self {
            classic: FeeWindow::new(retention),
            soroban: FeeWindow::new(retention),
        }
    }

    /// Get the classic fee distribution.
    pub(crate) fn get_classic_distribution(&self) -> FeeDistribution {
        self.classic.distribution()
    }

    /// Get the Soroban inclusion fee distribution.
    pub(crate) fn get_soroban_distribution(&self) -> FeeDistribution {
        self.soroban.distribution()
    }

    /// Get the latest ledger sequence processed by either window.
    pub(crate) fn latest_ledger(&self) -> u32 {
        self.classic
            .latest_ledger()
            .max(self.soroban.latest_ledger())
    }

    /// Reset both windows (e.g., on discontinuity).
    pub(crate) fn reset(&self) {
        self.classic.reset();
        self.soroban.reset();
    }

    /// Ingest fees from a raw `LedgerCloseMeta` XDR blob.
    ///
    /// Extracts classic and Soroban fees from each transaction in the meta
    /// and appends them to the appropriate window.
    pub(crate) fn ingest_ledger_close_meta(&self, meta_bytes: &[u8]) -> Result<(), String> {
        let lcm = LedgerCloseMeta::from_xdr(meta_bytes, Limits::none())
            .map_err(|e| format!("failed to parse LedgerCloseMeta: {e}"))?;

        let ledger_seq = match &lcm {
            LedgerCloseMeta::V0(v0) => v0.ledger_header.header.ledger_seq,
            LedgerCloseMeta::V1(v1) => v1.ledger_header.header.ledger_seq,
            LedgerCloseMeta::V2(v2) => v2.ledger_header.header.ledger_seq,
        };

        let (classic_fees, soroban_fees) = extract_fees_from_lcm(&lcm);

        self.classic
            .append(ledger_seq, classic_fees)
            .map_err(|e| format!("classic window: {e}"))?;
        self.soroban
            .append(ledger_seq, soroban_fees)
            .map_err(|e| format!("soroban window: {e}"))?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Fee extraction from LedgerCloseMeta
// ---------------------------------------------------------------------------

/// Extract classic and Soroban fees from a `LedgerCloseMeta`.
///
/// Returns `(classic_fees, soroban_fees)` — vectors of per-transaction fee values.
fn extract_fees_from_lcm(lcm: &LedgerCloseMeta) -> (Vec<u64>, Vec<u64>) {
    let mut classic_fees = Vec::new();
    let mut soroban_fees = Vec::new();

    let mut extract_tx_fees = |result: &TransactionResultPair, meta: &TransactionMeta| {
        let num_ops = count_ops_from_result(&result.result.result);
        if num_ops == 0 {
            return;
        }

        let fee_charged = result.result.fee_charged as u64;

        // Check if this is a Soroban transaction by looking for SorobanTransactionMetaExtV1
        if let Some(inclusion_fee) = extract_soroban_inclusion_fee(meta, fee_charged) {
            soroban_fees.push(inclusion_fee);
        } else {
            // Classic: fee per operation
            let fee_per_op = fee_charged / num_ops as u64;
            classic_fees.push(fee_per_op);
        }
    };

    match lcm {
        LedgerCloseMeta::V0(v0) => {
            for tx in v0.tx_processing.iter() {
                extract_tx_fees(&tx.result, &tx.tx_apply_processing);
            }
        }
        LedgerCloseMeta::V1(v1) => {
            for tx in v1.tx_processing.iter() {
                extract_tx_fees(&tx.result, &tx.tx_apply_processing);
            }
        }
        LedgerCloseMeta::V2(v2) => {
            for tx in v2.tx_processing.iter() {
                extract_tx_fees(&tx.result, &tx.tx_apply_processing);
            }
        }
    }

    (classic_fees, soroban_fees)
}

fn soroban_resource_fee(ext: &SorobanTransactionMetaExt) -> Option<u64> {
    match ext {
        SorobanTransactionMetaExt::V1(v1) => Some(
            (v1.total_non_refundable_resource_fee_charged
                + v1.total_refundable_resource_fee_charged) as u64,
        ),
        SorobanTransactionMetaExt::V0 => None,
    }
}

/// Try to extract the Soroban inclusion fee from transaction metadata.
///
/// Returns `Some(inclusionFee)` if the transaction has Soroban meta with V1 ext
/// (fee breakdown), or `None` if it's a classic transaction.
fn extract_soroban_inclusion_fee(meta: &TransactionMeta, fee_charged: u64) -> Option<u64> {
    let resource_fee = match meta {
        TransactionMeta::V3(v3) => soroban_resource_fee(&v3.soroban_meta.as_ref()?.ext),
        TransactionMeta::V4(v4) => soroban_resource_fee(&v4.soroban_meta.as_ref()?.ext),
        _ => None,
    }?;

    Some(fee_charged.saturating_sub(resource_fee))
}

/// Count the number of operations from a `TransactionResultResult`.
///
/// This avoids needing the transaction envelope — the result carries the
/// operation results, so we can count them.
fn count_ops_from_result(result: &TransactionResultResult) -> usize {
    match result {
        TransactionResultResult::TxSuccess(ops) | TransactionResultResult::TxFailed(ops) => {
            ops.len()
        }
        TransactionResultResult::TxFeeBumpInnerSuccess(inner)
        | TransactionResultResult::TxFeeBumpInnerFailed(inner) => match &inner.result.result {
            InnerTransactionResultResult::TxSuccess(ops)
            | InnerTransactionResultResult::TxFailed(ops) => ops.len(),
            _ => 0,
        },
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_fee_distribution_empty() {
        let dist = compute_fee_distribution(&mut vec![], 0);
        assert_eq!(dist.fee_count, 0);
        assert_eq!(dist.ledger_count, 0);
        assert_eq!(dist.min, 0);
        assert_eq!(dist.max, 0);
    }

    #[test]
    fn test_compute_fee_distribution_single() {
        let mut fees = vec![100];
        let dist = compute_fee_distribution(&mut fees, 1);
        assert_eq!(dist.min, 100);
        assert_eq!(dist.max, 100);
        assert_eq!(dist.mode, 100);
        assert_eq!(dist.p50, 100);
        assert_eq!(dist.p99, 100);
        assert_eq!(dist.fee_count, 1);
        assert_eq!(dist.ledger_count, 1);
    }

    #[test]
    fn test_compute_fee_distribution_sorted() {
        // 10 fees: 1..=10
        let mut fees: Vec<u64> = (1..=10).collect();
        let dist = compute_fee_distribution(&mut fees, 1);
        assert_eq!(dist.min, 1);
        assert_eq!(dist.max, 10);
        assert_eq!(dist.mode, 1); // all unique, first value wins
                                  // P50: ceil(50 * 10 / 100) = 5 → fees[4] = 5
        assert_eq!(dist.p50, 5);
        // P90: ceil(90 * 10 / 100) = 9 → fees[8] = 9
        assert_eq!(dist.p90, 9);
        // P99: ceil(99 * 10 / 100) = ceil(9.9) = 10 → fees[9] = 10
        assert_eq!(dist.p99, 10);
        assert_eq!(dist.fee_count, 10);
    }

    #[test]
    fn test_compute_fee_distribution_mode() {
        // 100 appears 5 times, 200 appears 3 times
        let mut fees = vec![100, 200, 100, 200, 100, 200, 100, 100, 300];
        let dist = compute_fee_distribution(&mut fees, 1);
        assert_eq!(dist.mode, 100);
    }

    #[test]
    fn test_compute_fee_distribution_mode_last_cluster() {
        // Mode should pick last cluster if it has the most repetitions
        let mut fees = vec![100, 200, 200, 200, 200, 200];
        let dist = compute_fee_distribution(&mut fees, 1);
        assert_eq!(dist.mode, 200);
    }

    #[test]
    fn test_compute_fee_distribution_percentile_nearest_rank() {
        // Verify nearest-rank: ceiling(p * N / 100)
        // 100 values: 1..=100
        let mut fees: Vec<u64> = (1..=100).collect();
        let dist = compute_fee_distribution(&mut fees, 1);
        // P10: ceil(10 * 100 / 100) = 10 → fees[9] = 10
        assert_eq!(dist.p10, 10);
        // P95: ceil(95 * 100 / 100) = 95 → fees[94] = 95
        assert_eq!(dist.p95, 95);
        // P99: ceil(99 * 100 / 100) = 99 → fees[98] = 99
        assert_eq!(dist.p99, 99);
    }

    #[test]
    fn test_ring_buffer_basic() {
        let mut buf = LedgerBucketWindow::new(3);
        assert_eq!(buf.len(), 0);
        assert_eq!(buf.latest_ledger(), 0);

        buf.append(10, vec![100]).unwrap();
        assert_eq!(buf.len(), 1);
        assert_eq!(buf.latest_ledger(), 10);

        buf.append(11, vec![200]).unwrap();
        buf.append(12, vec![300]).unwrap();
        assert_eq!(buf.len(), 3);
        assert_eq!(buf.latest_ledger(), 12);

        // Now full — next append evicts oldest
        buf.append(13, vec![400]).unwrap();
        assert_eq!(buf.len(), 3);
        assert_eq!(buf.get(0).ledger_seq, 11);
        assert_eq!(buf.latest_ledger(), 13);
    }

    #[test]
    fn test_ring_buffer_contiguity_check() {
        let mut buf = LedgerBucketWindow::new(10);
        buf.append(100, vec![]).unwrap();
        let err = buf.append(102, vec![]).unwrap_err();
        assert!(err.contains("not contiguous"));
    }

    #[test]
    fn test_ring_buffer_all_fees() {
        let mut buf = LedgerBucketWindow::new(3);
        buf.append(1, vec![10, 20]).unwrap();
        buf.append(2, vec![30]).unwrap();
        buf.append(3, vec![40, 50, 60]).unwrap();
        let all = buf.all_fees();
        assert_eq!(all, vec![10, 20, 30, 40, 50, 60]);
    }

    #[test]
    fn test_fee_window_distribution_updates() {
        let fw = FeeWindow::new(10);
        fw.append(1, vec![100, 200, 300]).unwrap();
        let dist = fw.distribution();
        assert_eq!(dist.min, 100);
        assert_eq!(dist.max, 300);
        assert_eq!(dist.fee_count, 3);
        assert_eq!(dist.ledger_count, 1);

        fw.append(2, vec![400]).unwrap();
        let dist = fw.distribution();
        assert_eq!(dist.fee_count, 4);
        assert_eq!(dist.ledger_count, 2);
        assert_eq!(dist.max, 400);
    }

    #[test]
    fn test_fee_windows_composite() {
        let fw = FeeWindows::new(10);
        assert_eq!(fw.latest_ledger(), 0);
        assert_eq!(fw.get_classic_distribution().fee_count, 0);
        assert_eq!(fw.get_soroban_distribution().fee_count, 0);
    }

    #[test]
    fn test_count_ops_from_result_success() {
        use stellar_xdr::curr::{OperationResult, TransactionResultResult, VecM};
        let ops: VecM<OperationResult> = vec![
            OperationResult::OpNotSupported,
            OperationResult::OpNotSupported,
        ]
        .try_into()
        .unwrap();
        let result = TransactionResultResult::TxSuccess(ops);
        assert_eq!(count_ops_from_result(&result), 2);
    }

    #[test]
    fn test_count_ops_from_result_error_codes() {
        use stellar_xdr::curr::TransactionResultResult;
        // Error codes that carry no operation results should return 0
        assert_eq!(
            count_ops_from_result(&TransactionResultResult::TxTooEarly),
            0
        );
        assert_eq!(count_ops_from_result(&TransactionResultResult::TxBadSeq), 0);
    }

    #[test]
    fn test_ring_buffer_gap_detection() {
        // Simulate a catchup gap: ledgers 10-12, then gap, then 20-22
        let mut buf = LedgerBucketWindow::new(100);
        buf.append(10, vec![100]).unwrap();
        buf.append(11, vec![200]).unwrap();
        buf.append(12, vec![300]).unwrap();

        // Gap: trying to append 20 should fail
        let err = buf.append(20, vec![400]).unwrap_err();
        assert!(err.contains("not contiguous"));
        assert!(err.contains("expected 13 but got 20"));

        // After reset, can start fresh from the post-gap range
        buf.reset();
        assert_eq!(buf.len(), 0);
        buf.append(20, vec![400]).unwrap();
        buf.append(21, vec![500]).unwrap();
        assert_eq!(buf.len(), 2);
        assert_eq!(buf.latest_ledger(), 21);
    }

    #[test]
    fn test_fee_windows_reset_on_gap() {
        let fw = FeeWindows::new(100);

        // Pre-gap range
        fw.classic.append(10, vec![100]).unwrap();
        fw.soroban.append(10, vec![]).unwrap();
        assert_eq!(fw.latest_ledger(), 10);

        // Reset simulates the gap-handling behavior
        fw.reset();
        assert_eq!(fw.latest_ledger(), 0);

        // Post-gap range works fine
        fw.classic.append(20, vec![200]).unwrap();
        fw.soroban.append(20, vec![]).unwrap();
        assert_eq!(fw.latest_ledger(), 20);
    }
}
