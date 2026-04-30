//! Fee newtypes for type-safe fee arithmetic.
//!
//! These newtypes prevent the class of bugs where `TotalFee` is accidentally
//! used where `InclusionFee` is expected (or vice versa). All types have
//! private fields and explicit construction/extraction.
//!
//! # Fee decomposition
//!
//! For any transaction: `TotalFee = InclusionFee + ResourceFee`
//!
//! - **Classic transactions**: `ResourceFee` is always 0, so `TotalFee == InclusionFee`
//! - **Soroban transactions**: `ResourceFee` is declared in the transaction extension
//! - **Fee-bump inner Soroban**: `InclusionFee` may be negative (outer envelope covers shortfall)

use std::cmp::Ordering;
use std::fmt;

/// The total fee declared on a transaction envelope (or fee-bump outer fee).
///
/// May be negative in raw XDR (fee-bump outer fee is `Int64`), but validation
/// rejects negative values before execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct TotalFee(i64);

/// The inclusion fee portion (total fee minus Soroban resource fee).
///
/// For classic transactions, this equals `TotalFee`.
/// May be negative for fee-bump inner Soroban transactions where the outer
/// envelope covers the resource fee shortfall.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct InclusionFee(i64);

/// The Soroban resource fee (declared in the transaction extension).
///
/// Zero for non-Soroban transactions. Validation ensures this is non-negative
/// and within bounds before execution, but the constructor is unchecked to
/// allow validation code to inspect the raw value for error reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct ResourceFee(i64);

// --- TotalFee ---

impl TotalFee {
    /// Create a new `TotalFee` from a raw i64 value.
    #[inline]
    pub fn new(value: i64) -> Self {
        Self(value)
    }

    /// Extract the raw i64 value.
    #[inline]
    pub fn as_i64(self) -> i64 {
        self.0
    }

    /// Decompose into inclusion fee by subtracting resource fee (saturating).
    ///
    /// This is the primary decomposition used by `envelope_inclusion_fee`.
    #[inline]
    pub fn saturating_sub_resource(self, resource: ResourceFee) -> InclusionFee {
        InclusionFee(self.0.saturating_sub(resource.0))
    }

    /// Decompose into inclusion fee by subtracting resource fee (wrapping).
    ///
    /// Use only where inputs are validated and overflow is impossible.
    #[inline]
    pub fn wrapping_sub_resource(self, resource: ResourceFee) -> InclusionFee {
        InclusionFee(self.0.wrapping_sub(resource.0))
    }

    /// Compute resource fee by subtracting inclusion fee (saturating).
    ///
    /// Used in `tx_applying_fee` and `tx_queue_limiter` to recover the resource
    /// fee portion from total and inclusion fees.
    #[inline]
    pub fn saturating_sub_inclusion(self, inclusion: InclusionFee) -> ResourceFee {
        ResourceFee(self.0.saturating_sub(inclusion.0))
    }
}

impl fmt::Display for TotalFee {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// --- InclusionFee ---

impl InclusionFee {
    /// Create a new `InclusionFee` from a raw i64 value.
    ///
    /// May be negative for fee-bump inner Soroban transactions.
    #[inline]
    pub fn new(value: i64) -> Self {
        Self(value)
    }

    /// Extract the raw i64 value.
    #[inline]
    pub fn as_i64(self) -> i64 {
        self.0
    }

    /// Reconstruct total fee by adding resource fee (saturating).
    #[inline]
    pub fn saturating_add_resource(self, resource: ResourceFee) -> TotalFee {
        TotalFee(self.0.saturating_add(resource.0))
    }
}

impl fmt::Display for InclusionFee {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// --- ResourceFee ---

impl ResourceFee {
    /// Create a new `ResourceFee` from a raw i64 value.
    ///
    /// Unchecked: validation code must inspect the raw value to produce
    /// structured errors. Use `checked()` for post-validation construction.
    #[inline]
    pub fn new(value: i64) -> Self {
        Self(value)
    }

    /// Create a `ResourceFee` only if non-negative.
    #[inline]
    pub fn checked(value: i64) -> Option<Self> {
        if value >= 0 {
            Some(Self(value))
        } else {
            None
        }
    }

    /// Extract the raw i64 value.
    #[inline]
    pub fn as_i64(self) -> i64 {
        self.0
    }
}

impl fmt::Display for ResourceFee {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// --- FeeRate ---

/// A fee rate (inclusion fee per operation) used for surge pricing comparison.
///
/// Bundles the inclusion fee and operation count together so they cannot be
/// mismatched. Fields are private to prevent construction from mismatched sources.
///
/// Comparison uses cross-multiplication to avoid integer division:
/// `a.inclusion_fee * b.op_count` vs `b.inclusion_fee * a.op_count`
#[derive(Debug, Clone, Copy)]
pub struct FeeRate {
    inclusion_fee: InclusionFee,
    op_count: u32,
}

impl FeeRate {
    /// Construct from explicit values.
    ///
    /// Used by queue entries that cache the fee and op count.
    #[inline]
    pub fn new(inclusion_fee: InclusionFee, op_count: u32) -> Self {
        Self {
            inclusion_fee,
            op_count,
        }
    }

    /// Get the inclusion fee.
    #[inline]
    pub fn inclusion_fee(&self) -> InclusionFee {
        self.inclusion_fee
    }

    /// Get the operation count.
    #[inline]
    pub fn op_count(&self) -> u32 {
        self.op_count
    }

    /// Compare fee rates via cross-multiplication.
    ///
    /// Matches stellar-core's `feeRate3WayCompare(int64_t, uint32_t, int64_t, uint32_t)`
    /// at `numeric.cpp:127`.
    ///
    /// Asserts that fees are non-negative (stellar-core's `bigMultiply`
    /// release-asserts the same).
    pub fn cmp_rate(&self, other: &Self) -> Ordering {
        let a_fee = self.inclusion_fee.as_i64();
        let b_fee = other.inclusion_fee.as_i64();
        assert!(a_fee >= 0, "FeeRate::cmp_rate: negative fee {a_fee}");
        assert!(b_fee >= 0, "FeeRate::cmp_rate: negative fee {b_fee}");

        let lhs = a_fee as i128 * other.op_count as i128;
        let rhs = b_fee as i128 * self.op_count as i128;
        lhs.cmp(&rhs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_total_fee_construction_and_access() {
        let fee = TotalFee::new(1000);
        assert_eq!(fee.as_i64(), 1000);

        let negative = TotalFee::new(-500);
        assert_eq!(negative.as_i64(), -500);
    }

    #[test]
    fn test_inclusion_fee_allows_negative() {
        let fee = InclusionFee::new(-100);
        assert_eq!(fee.as_i64(), -100);

        let fee = InclusionFee::new(0);
        assert_eq!(fee.as_i64(), 0);
    }

    #[test]
    fn test_resource_fee_construction() {
        let fee = ResourceFee::new(500);
        assert_eq!(fee.as_i64(), 500);

        // Unchecked allows negative (for validation inspection)
        let neg = ResourceFee::new(-1);
        assert_eq!(neg.as_i64(), -1);
    }

    #[test]
    fn test_resource_fee_checked() {
        assert!(ResourceFee::checked(500).is_some());
        assert!(ResourceFee::checked(0).is_some());
        assert!(ResourceFee::checked(-1).is_none());
    }

    #[test]
    fn test_total_fee_sub_resource_gives_inclusion() {
        let total = TotalFee::new(1000);
        let resource = ResourceFee::new(300);
        let inclusion = total.saturating_sub_resource(resource);
        assert_eq!(inclusion.as_i64(), 700);
    }

    #[test]
    fn test_total_fee_sub_resource_saturating_overflow() {
        let total = TotalFee::new(i64::MIN);
        let resource = ResourceFee::new(1);
        let inclusion = total.saturating_sub_resource(resource);
        assert_eq!(inclusion.as_i64(), i64::MIN);
    }

    #[test]
    fn test_total_fee_wrapping_sub_resource() {
        let total = TotalFee::new(1000);
        let resource = ResourceFee::new(300);
        let inclusion = total.wrapping_sub_resource(resource);
        assert_eq!(inclusion.as_i64(), 700);
    }

    #[test]
    fn test_total_fee_sub_inclusion_gives_resource() {
        let total = TotalFee::new(1000);
        let inclusion = InclusionFee::new(700);
        let resource = total.saturating_sub_inclusion(inclusion);
        assert_eq!(resource.as_i64(), 300);
    }

    #[test]
    fn test_inclusion_add_resource_gives_total() {
        let inclusion = InclusionFee::new(700);
        let resource = ResourceFee::new(300);
        let total = inclusion.saturating_add_resource(resource);
        assert_eq!(total.as_i64(), 1000);
    }

    #[test]
    fn test_classic_tx_total_equals_inclusion() {
        // Classic: resource fee is 0
        let total = TotalFee::new(500);
        let resource = ResourceFee::new(0);
        let inclusion = total.saturating_sub_resource(resource);
        assert_eq!(inclusion.as_i64(), total.as_i64());
    }

    #[test]
    fn test_fee_rate_cmp_basic() {
        // 1000 fee / 2 ops = 500/op vs 600 fee / 1 op = 600/op
        let a = FeeRate::new(InclusionFee::new(1000), 2);
        let b = FeeRate::new(InclusionFee::new(600), 1);
        assert_eq!(a.cmp_rate(&b), Ordering::Less);
    }

    #[test]
    fn test_fee_rate_cmp_equal() {
        let a = FeeRate::new(InclusionFee::new(200), 2);
        let b = FeeRate::new(InclusionFee::new(100), 1);
        assert_eq!(a.cmp_rate(&b), Ordering::Equal);
    }

    #[test]
    fn test_fee_rate_cmp_greater() {
        let a = FeeRate::new(InclusionFee::new(1000), 1);
        let b = FeeRate::new(InclusionFee::new(500), 2);
        assert_eq!(a.cmp_rate(&b), Ordering::Greater);
    }

    #[test]
    #[should_panic(expected = "negative fee")]
    fn test_fee_rate_cmp_panics_on_negative() {
        let a = FeeRate::new(InclusionFee::new(-1), 1);
        let b = FeeRate::new(InclusionFee::new(100), 1);
        a.cmp_rate(&b);
    }

    #[test]
    fn test_ordering() {
        let a = TotalFee::new(100);
        let b = TotalFee::new(200);
        assert!(a < b);

        let c = InclusionFee::new(-50);
        let d = InclusionFee::new(50);
        assert!(c < d);
    }

    #[test]
    fn test_display() {
        assert_eq!(format!("{}", TotalFee::new(1000)), "1000");
        assert_eq!(format!("{}", InclusionFee::new(-50)), "-50");
        assert_eq!(format!("{}", ResourceFee::new(300)), "300");
    }
}
