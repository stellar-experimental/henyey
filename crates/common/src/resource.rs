//! Resource accounting utilities for surge pricing and transaction limits.
//!
//! This module provides types and functions for tracking and managing
//! computational resources in Stellar transactions. Resources are used
//! for both classic transactions (operation counts) and Soroban smart
//! contract transactions (CPU instructions, storage I/O, etc.).
//!
//! # Resource Types
//!
//! Different transaction types have different resource dimensions:
//!
//! - **Classic transactions**: 1 resource (operations) or 2 (operations + bytes)
//! - **Soroban transactions**: 7 resources (operations, instructions, bytes, disk I/O, etc.)
//!
//! # Surge Pricing
//!
//! Resources are used to implement surge pricing, where transactions compete
//! for limited network capacity. When demand exceeds capacity, fees increase.
//!
//! # Example
//!
//! ```rust
//! use henyey_common::resource::{Resource, ResourceType, NUM_SOROBAN_TX_RESOURCES};
//!
//! // Create an empty Soroban resource vector
//! let mut resources = Resource::make_empty_soroban();
//!
//! // Set some resource values
//! resources.set_val(ResourceType::Operations, 1);
//! resources.set_val(ResourceType::Instructions, 1_000_000);
//!
//! // Check resource values
//! assert_eq!(resources.get_val(ResourceType::Operations), 1);
//! assert!(!resources.is_zero());
//! ```

use std::cmp::Ordering;
use std::fmt;
use std::ops::{Add, AddAssign, Sub, SubAssign};

use crate::math::is_representable_as_i64;

/// Error type for checked Resource arithmetic operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceError {
    /// The two Resources have different numbers of dimensions.
    SizeMismatch { lhs: usize, rhs: usize },
    /// Addition would overflow i64 in the given dimension.
    Overflow { dimension: usize },
    /// Subtraction would underflow (go negative) in the given dimension.
    Underflow { dimension: usize },
}

impl fmt::Display for ResourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResourceError::SizeMismatch { lhs, rhs } => {
                write!(f, "resource size mismatch: {} vs {}", lhs, rhs)
            }
            ResourceError::Overflow { dimension } => {
                write!(f, "resource overflow in dimension {}", dimension)
            }
            ResourceError::Underflow { dimension } => {
                write!(f, "resource underflow in dimension {}", dimension)
            }
        }
    }
}

impl std::error::Error for ResourceError {}

/// Number of resource dimensions for classic transactions (operations only).
pub const NUM_CLASSIC_TX_RESOURCES: usize = 1;

/// Number of resource dimensions for classic transactions with byte tracking.
pub const NUM_CLASSIC_TX_BYTES_RESOURCES: usize = 2;

/// Number of resource dimensions for Soroban smart contract transactions.
///
/// Soroban transactions track:
/// 1. Operations
/// 2. CPU instructions
/// 3. Transaction byte size
/// 4. Disk read bytes
/// 5. Write bytes
/// 6. Read ledger entries
/// 7. Write ledger entries
pub const NUM_SOROBAN_TX_RESOURCES: usize = 7;

/// Enumeration of resource types tracked for transactions.
///
/// Each variant corresponds to a specific resource dimension that may be
/// limited or priced. The discriminant values are used as indices into
/// the [`Resource`] values vector.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ResourceType {
    /// Number of operations in the transaction.
    Operations = 0,
    /// CPU instructions consumed (Soroban only).
    Instructions = 1,
    /// Transaction size in bytes.
    TxByteSize = 2,
    /// Bytes read from persistent storage (Soroban only).
    DiskReadBytes = 3,
    /// Bytes written to storage (Soroban only).
    WriteBytes = 4,
    /// Number of ledger entries read (Soroban only).
    ReadLedgerEntries = 5,
    /// Number of ledger entries written (Soroban only).
    WriteLedgerEntries = 6,
}

/// A multi-dimensional resource usage vector.
///
/// This struct tracks resource consumption across multiple dimensions.
/// The number of dimensions depends on the transaction type:
/// - Classic transactions: 1-2 dimensions
/// - Soroban transactions: 7 dimensions
///
/// Resources support arithmetic operations and partial ordering. Two resources
/// are comparable only if they have the same number of dimensions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resource {
    values: Vec<i64>,
}

impl Resource {
    /// Creates a new resource vector from the given values.
    ///
    /// # Panics
    ///
    /// Panics if the length is not 1, 2, or 7 (the valid resource dimensions).
    pub fn new(values: Vec<i64>) -> Self {
        let len = values.len();
        assert!(
            len == NUM_CLASSIC_TX_RESOURCES
                || len == NUM_CLASSIC_TX_BYTES_RESOURCES
                || len == NUM_SOROBAN_TX_RESOURCES,
            "invalid resource length: {}",
            len
        );
        Self { values }
    }

    /// Creates a zero-initialized resource vector with the specified number of dimensions.
    ///
    /// # Panics
    ///
    /// Panics if `count` is not 1, 2, or 7.
    pub fn make_empty(count: usize) -> Self {
        Self::new(vec![0; count])
    }

    /// Creates a zero-initialized Soroban resource vector (7 dimensions).
    pub fn make_empty_soroban() -> Self {
        Self::make_empty(NUM_SOROBAN_TX_RESOURCES)
    }

    /// Creates a Soroban resource vector from named ledger-level limits.
    ///
    /// This constructor uses the canonical [`ResourceType`] ordering internally,
    /// preventing ordering mistakes that can arise from a raw `vec![]` literal.
    pub fn soroban_ledger_limits(
        tx_count: i64,
        instructions: i64,
        tx_size_bytes: i64,
        read_bytes: i64,
        write_bytes: i64,
        read_ledger_entries: i64,
        write_ledger_entries: i64,
    ) -> Self {
        let mut r = Self::make_empty_soroban();
        r.set_val(ResourceType::Operations, tx_count);
        r.set_val(ResourceType::Instructions, instructions);
        r.set_val(ResourceType::TxByteSize, tx_size_bytes);
        r.set_val(ResourceType::DiskReadBytes, read_bytes);
        r.set_val(ResourceType::WriteBytes, write_bytes);
        r.set_val(ResourceType::ReadLedgerEntries, read_ledger_entries);
        r.set_val(ResourceType::WriteLedgerEntries, write_ledger_entries);
        r
    }

    /// Returns `true` if all resource values are zero.
    pub fn is_zero(&self) -> bool {
        self.values.iter().all(|v| *v == 0)
    }

    /// Returns `true` if any resource value is positive.
    pub fn any_positive(&self) -> bool {
        self.values.iter().any(|v| *v > 0)
    }

    /// Returns the number of resource dimensions.
    pub fn size(&self) -> usize {
        self.values.len()
    }

    /// Gets the value for a specific resource type, returning `None` if out of bounds.
    ///
    /// This is the safe variant that does not panic.
    pub fn try_get_val(&self, ty: ResourceType) -> Option<i64> {
        self.values.get(ty as usize).copied()
    }

    /// Gets the value for a specific resource type.
    ///
    /// # Panics
    ///
    /// Panics if the resource type index is out of bounds for this resource vector.
    pub fn get_val(&self, ty: ResourceType) -> i64 {
        self.values[ty as usize]
    }

    /// Sets the value for a specific resource type.
    ///
    /// # Panics
    ///
    /// Panics if the resource type index is out of bounds for this resource vector.
    pub fn set_val(&mut self, ty: ResourceType, val: i64) {
        self.values[ty as usize] = val;
    }

    /// Returns `true` if adding `other` to this resource would not overflow.
    ///
    /// # Panics
    ///
    /// Panics if `self` and `other` have different numbers of dimensions
    /// (matches stellar-core's `releaseAssert`).
    pub fn can_add(&self, other: &Resource) -> bool {
        assert_eq!(
            self.values.len(),
            other.values.len(),
            "Resource::can_add size mismatch: {} vs {}",
            self.values.len(),
            other.values.len()
        );
        self.values
            .iter()
            .zip(other.values.iter())
            .all(|(a, b)| a.checked_add(*b).is_some())
    }

    /// Returns `true` if all values in `self` are less than or equal to corresponding values in `other`.
    ///
    /// This is used to check if a resource usage fits within a limit.
    ///
    /// # Panics
    ///
    /// Panics if `self` and `other` have different numbers of dimensions
    /// (matches stellar-core's `releaseAssert`).
    pub fn leq(&self, other: &Resource) -> bool {
        assert_eq!(
            self.values.len(),
            other.values.len(),
            "Resource::leq size mismatch: {} vs {}",
            self.values.len(),
            other.values.len()
        );
        self.values
            .iter()
            .zip(other.values.iter())
            .all(|(a, b)| a <= b)
    }

    /// Checked addition. Returns a new `Resource` or an error on size mismatch / overflow.
    pub fn checked_add(&self, other: &Resource) -> Result<Resource, ResourceError> {
        if self.values.len() != other.values.len() {
            return Err(ResourceError::SizeMismatch {
                lhs: self.values.len(),
                rhs: other.values.len(),
            });
        }
        let mut values = Vec::with_capacity(self.values.len());
        for (i, (a, b)) in self.values.iter().zip(other.values.iter()).enumerate() {
            values.push(
                a.checked_add(*b)
                    .ok_or(ResourceError::Overflow { dimension: i })?,
            );
        }
        Ok(Resource { values })
    }

    /// Checked subtraction. Returns a new `Resource` or an error on size mismatch / underflow.
    ///
    /// Underflow means any dimension of `self` is less than the corresponding
    /// dimension of `other` (result would be negative).
    pub fn checked_sub(&self, other: &Resource) -> Result<Resource, ResourceError> {
        if self.values.len() != other.values.len() {
            return Err(ResourceError::SizeMismatch {
                lhs: self.values.len(),
                rhs: other.values.len(),
            });
        }
        let mut values = Vec::with_capacity(self.values.len());
        for (i, (a, b)) in self.values.iter().zip(other.values.iter()).enumerate() {
            if *a < *b {
                return Err(ResourceError::Underflow { dimension: i });
            }
            values.push(*a - *b);
        }
        Ok(Resource { values })
    }
}

impl AddAssign for Resource {
    /// # Panics
    ///
    /// Panics on size mismatch or if any dimension would overflow
    /// (matches stellar-core's `releaseAssert(canAdd(other))`).
    fn add_assign(&mut self, other: Self) {
        assert!(
            self.can_add(&other),
            "Resource addition overflow or size mismatch"
        );
        for (a, b) in self.values.iter_mut().zip(other.values) {
            *a += b;
        }
    }
}

impl SubAssign for Resource {
    /// # Panics
    ///
    /// Panics on size mismatch or if any dimension of `self` is less than
    /// the corresponding dimension of `other` (matches stellar-core's
    /// `releaseAssert(mResources[i] >= other.mResources[i])`).
    fn sub_assign(&mut self, other: Self) {
        assert_eq!(
            self.values.len(),
            other.values.len(),
            "Resource subtraction size mismatch: {} vs {}",
            self.values.len(),
            other.values.len()
        );
        for (a, b) in self.values.iter_mut().zip(other.values) {
            assert!(*a >= b, "Resource subtraction underflow: {} < {}", *a, b);
            *a -= b;
        }
    }
}

impl Add for Resource {
    type Output = Self;

    fn add(mut self, other: Self) -> Self::Output {
        self += other;
        self
    }
}

impl Sub for Resource {
    type Output = Self;

    fn sub(mut self, other: Self) -> Self::Output {
        self -= other;
        self
    }
}

impl PartialOrd for Resource {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        if self.values.len() != other.values.len() {
            return None; // incomparable — different dimensions
        }
        let all_le = self.leq(other);
        let all_ge = other.leq(self);
        if all_le && all_ge {
            Some(Ordering::Equal)
        } else if all_le {
            Some(Ordering::Less)
        } else if all_ge {
            Some(Ordering::Greater)
        } else {
            None
        }
    }
}

/// Returns `true` if any dimension of `lhs` is greater than the corresponding dimension of `rhs`.
///
/// This is useful for detecting when a resource vector exceeds a limit in any dimension.
///
/// # Panics
///
/// Panics if `lhs` and `rhs` have different numbers of dimensions
/// (matches stellar-core's `releaseAssert`).
pub fn any_greater(lhs: &Resource, rhs: &Resource) -> bool {
    assert_eq!(
        lhs.values.len(),
        rhs.values.len(),
        "any_greater size mismatch: {} vs {}",
        lhs.values.len(),
        rhs.values.len()
    );
    lhs.values.iter().zip(rhs.values.iter()).any(|(a, b)| a > b)
}

/// Subtracts `rhs` from `lhs`, clamping each dimension to a minimum of 0.
///
/// This is useful for computing remaining capacity after deducting usage.
///
/// # Panics
///
/// Panics if `lhs` and `rhs` have different numbers of dimensions
/// (matches stellar-core's `releaseAssert`).
pub fn subtract_non_negative(lhs: &Resource, rhs: &Resource) -> Resource {
    assert_eq!(
        lhs.values.len(),
        rhs.values.len(),
        "subtract_non_negative size mismatch: {} vs {}",
        lhs.values.len(),
        rhs.values.len()
    );
    Resource::new(
        lhs.values
            .iter()
            .zip(rhs.values.iter())
            .map(|(a, b)| (a - b).max(0))
            .collect(),
    )
}

impl std::fmt::Display for Resource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let values: Vec<String> = self.values.iter().map(|v| v.to_string()).collect();
        write!(f, "{}", values.join(", "))
    }
}

impl std::fmt::Display for ResourceType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ResourceType {
    /// Returns all resource types as a slice.
    pub const fn all() -> &'static [ResourceType] {
        &[
            ResourceType::Operations,
            ResourceType::Instructions,
            ResourceType::TxByteSize,
            ResourceType::DiskReadBytes,
            ResourceType::WriteBytes,
            ResourceType::ReadLedgerEntries,
            ResourceType::WriteLedgerEntries,
        ]
    }

    /// Returns the resource type name as a string.
    pub const fn as_str(&self) -> &'static str {
        match self {
            ResourceType::Operations => "Operations",
            ResourceType::Instructions => "Instructions",
            ResourceType::TxByteSize => "TxByteSize",
            ResourceType::DiskReadBytes => "DiskReadBytes",
            ResourceType::WriteBytes => "WriteBytes",
            ResourceType::ReadLedgerEntries => "ReadLedgerEntries",
            ResourceType::WriteLedgerEntries => "WriteLedgerEntries",
        }
    }
}

/// Multiplies each dimension of a resource by a double.
///
/// # Panics
///
/// Panics if:
/// - The result is negative
/// - The result is not representable as i64 (overflow)
///
/// # Example
///
/// ```rust
/// use henyey_common::resource::{Resource, multiply_by_double};
///
/// let r = Resource::new(vec![100, 200]);
/// let scaled = multiply_by_double(&r, 1.5);
/// assert_eq!(scaled.get_val(henyey_common::resource::ResourceType::Operations), 150);
/// ```
pub fn multiply_by_double(res: &Resource, m: f64) -> Resource {
    scale_by_double(res, m, |result| {
        assert!(
            is_representable_as_i64(result),
            "multiply_by_double: result not representable as i64"
        );
        result as i64
    })
}

/// Multiplies each dimension of a resource by a double, saturating at i64::MAX.
///
/// Unlike [`multiply_by_double`], this function clamps the result to i64::MAX
/// instead of panicking on overflow.
///
/// # Panics
///
/// Panics if the result is negative.
///
/// # Example
///
/// ```rust
/// use henyey_common::resource::{Resource, saturated_multiply_by_double};
///
/// let r = Resource::new(vec![i64::MAX / 2, 100]);
/// let scaled = saturated_multiply_by_double(&r, 3.0);
/// // First dimension saturates to i64::MAX
/// ```
pub fn saturated_multiply_by_double(res: &Resource, m: f64) -> Resource {
    scale_by_double(res, m, |result| {
        if is_representable_as_i64(result) {
            result as i64
        } else {
            i64::MAX
        }
    })
}

fn scale_by_double(res: &Resource, multiplier: f64, convert: impl Fn(f64) -> i64) -> Resource {
    let values: Vec<i64> = res
        .values
        .iter()
        .map(|&v| {
            let result = (v as f64) * multiplier;
            assert!(result >= 0.0, "scale_by_double: negative result");
            convert(result)
        })
        .collect();
    Resource::new(values)
}

/// Divides each dimension of a resource using big integer division.
///
/// Computes `(resource * B) / C` for each dimension using 128-bit
/// intermediate arithmetic to avoid overflow.
///
/// # Arguments
///
/// * `res` - The resource to divide
/// * `b` - The numerator multiplier
/// * `c` - The denominator
/// * `rounding` - Rounding mode (up or down)
///
/// # Errors
///
/// Returns an error if any division would overflow or produce an invalid result.
///
/// # Example
///
/// ```rust
/// use henyey_common::resource::{Resource, big_divide_resource};
/// use henyey_common::math::Rounding;
///
/// let r = Resource::new(vec![1000, 2000]);
/// let divided = big_divide_resource(&r, 1, 10, Rounding::Down).unwrap();
/// ```
pub fn big_divide_resource(
    res: &Resource,
    b: i64,
    c: i64,
    rounding: crate::math::Rounding,
) -> Result<Resource, crate::math::MathError> {
    let mut values = Vec::with_capacity(res.values.len());
    for &v in &res.values {
        values.push(crate::math::big_divide(v, b, c, rounding)?);
    }
    Ok(Resource::new(values))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resource_display() {
        let r = Resource::new(vec![100, 200]);
        assert_eq!(format!("{}", r), "100, 200");

        let r = Resource::make_empty_soroban();
        assert_eq!(format!("{}", r), "0, 0, 0, 0, 0, 0, 0");
    }

    #[test]
    fn test_resource_type_display() {
        assert_eq!(format!("{}", ResourceType::Operations), "Operations");
        assert_eq!(format!("{}", ResourceType::Instructions), "Instructions");
        assert_eq!(format!("{}", ResourceType::TxByteSize), "TxByteSize");
    }

    #[test]
    fn test_resource_type_as_str() {
        assert_eq!(ResourceType::Operations.as_str(), "Operations");
        assert_eq!(
            ResourceType::WriteLedgerEntries.as_str(),
            "WriteLedgerEntries"
        );
    }

    #[test]
    fn test_resource_type_all() {
        let all = ResourceType::all();
        assert_eq!(all.len(), 7);
        assert_eq!(all[0], ResourceType::Operations);
        assert_eq!(all[6], ResourceType::WriteLedgerEntries);
    }

    #[test]
    fn test_multiply_by_double() {
        let r = Resource::new(vec![100, 200]);
        let scaled = multiply_by_double(&r, 1.5);
        assert_eq!(scaled.get_val(ResourceType::Operations), 150);
        assert_eq!(scaled.get_val(ResourceType::Instructions), 300);
    }

    #[test]
    fn test_multiply_by_double_zero() {
        let r = Resource::new(vec![100, 200]);
        let scaled = multiply_by_double(&r, 0.0);
        assert!(scaled.is_zero());
    }

    #[test]
    fn test_multiply_by_double_fractional() {
        let r = Resource::new(vec![100, 200]);
        let scaled = multiply_by_double(&r, 0.1);
        assert_eq!(scaled.get_val(ResourceType::Operations), 10);
        assert_eq!(scaled.get_val(ResourceType::Instructions), 20);
    }

    #[test]
    #[should_panic(expected = "negative result")]
    fn test_multiply_by_double_negative_panics() {
        let r = Resource::new(vec![100, 200]);
        multiply_by_double(&r, -1.0);
    }

    #[test]
    fn test_saturated_multiply_by_double() {
        let r = Resource::new(vec![100, 200]);
        let scaled = saturated_multiply_by_double(&r, 1.5);
        assert_eq!(scaled.get_val(ResourceType::Operations), 150);
        assert_eq!(scaled.get_val(ResourceType::Instructions), 300);
    }

    #[test]
    fn test_saturated_multiply_by_double_saturates() {
        let r = Resource::new(vec![i64::MAX / 2, 100]);
        let scaled = saturated_multiply_by_double(&r, 3.0);
        assert_eq!(scaled.get_val(ResourceType::Operations), i64::MAX);
        assert_eq!(scaled.get_val(ResourceType::Instructions), 300);
    }

    #[test]
    fn test_big_divide_resource() {
        use crate::math::Rounding;

        let r = Resource::new(vec![1000, 2000]);
        let divided = big_divide_resource(&r, 1, 10, Rounding::Down).unwrap();
        assert_eq!(divided.get_val(ResourceType::Operations), 100);
        assert_eq!(divided.get_val(ResourceType::Instructions), 200);
    }

    #[test]
    fn test_big_divide_resource_round_up() {
        use crate::math::Rounding;

        let r = Resource::new(vec![1001, 2001]);
        let divided = big_divide_resource(&r, 1, 10, Rounding::Up).unwrap();
        assert_eq!(divided.get_val(ResourceType::Operations), 101);
        assert_eq!(divided.get_val(ResourceType::Instructions), 201);
    }

    #[test]
    fn test_big_divide_resource_with_multiplier() {
        use crate::math::Rounding;

        // (100 * 3) / 2 = 150
        let r = Resource::new(vec![100, 200]);
        let divided = big_divide_resource(&r, 3, 2, Rounding::Down).unwrap();
        assert_eq!(divided.get_val(ResourceType::Operations), 150);
        assert_eq!(divided.get_val(ResourceType::Instructions), 300);
    }

    // ── Hardened operator tests (parity with stellar-core releaseAssert) ──

    #[test]
    #[should_panic(expected = "overflow")]
    fn test_add_assign_overflow_panics() {
        let mut a = Resource::new(vec![i64::MAX, 0]);
        let b = Resource::new(vec![1, 0]);
        a += b;
    }

    #[test]
    #[should_panic(expected = "underflow")]
    fn test_sub_assign_underflow_panics() {
        let mut a = Resource::new(vec![0, 5]);
        let b = Resource::new(vec![1, 0]);
        a -= b;
    }

    #[test]
    #[should_panic(expected = "size mismatch")]
    fn test_add_assign_size_mismatch_panics() {
        let mut a = Resource::new(vec![1, 2]);
        let b = Resource::make_empty_soroban();
        a += b;
    }

    #[test]
    #[should_panic(expected = "size mismatch")]
    fn test_sub_assign_size_mismatch_panics() {
        let mut a = Resource::new(vec![1, 2]);
        let b = Resource::make_empty_soroban();
        a -= b;
    }

    #[test]
    #[should_panic(expected = "size mismatch")]
    fn test_can_add_size_mismatch_panics() {
        let a = Resource::new(vec![1, 2]);
        let b = Resource::make_empty_soroban();
        a.can_add(&b);
    }

    #[test]
    #[should_panic(expected = "size mismatch")]
    fn test_leq_size_mismatch_panics() {
        let a = Resource::new(vec![1, 2]);
        let b = Resource::make_empty_soroban();
        a.leq(&b);
    }

    #[test]
    #[should_panic(expected = "size mismatch")]
    fn test_any_greater_size_mismatch_panics() {
        let a = Resource::new(vec![1, 2]);
        let b = Resource::make_empty_soroban();
        any_greater(&a, &b);
    }

    #[test]
    #[should_panic(expected = "size mismatch")]
    fn test_subtract_non_negative_size_mismatch_panics() {
        let a = Resource::new(vec![1, 2]);
        let b = Resource::make_empty_soroban();
        subtract_non_negative(&a, &b);
    }

    #[test]
    fn test_partial_ord_size_mismatch_returns_none() {
        let a = Resource::new(vec![1, 2]);
        let b = Resource::make_empty_soroban();
        assert_eq!(a.partial_cmp(&b), None);
    }

    #[test]
    fn test_add_assign_valid() {
        let mut a = Resource::new(vec![10, 20]);
        let b = Resource::new(vec![5, 3]);
        a += b;
        assert_eq!(a.get_val(ResourceType::Operations), 15);
        assert_eq!(a.get_val(ResourceType::Instructions), 23);
    }

    #[test]
    fn test_sub_assign_valid() {
        let mut a = Resource::new(vec![10, 20]);
        let b = Resource::new(vec![5, 3]);
        a -= b;
        assert_eq!(a.get_val(ResourceType::Operations), 5);
        assert_eq!(a.get_val(ResourceType::Instructions), 17);
    }

    #[test]
    fn test_sub_assign_to_zero() {
        let mut a = Resource::new(vec![10, 20]);
        let b = Resource::new(vec![10, 20]);
        a -= b;
        assert!(a.is_zero());
    }

    // ── checked_add / checked_sub tests ──

    #[test]
    fn test_checked_add_valid() {
        let a = Resource::new(vec![10, 20]);
        let b = Resource::new(vec![5, 3]);
        let result = a.checked_add(&b).unwrap();
        assert_eq!(result.get_val(ResourceType::Operations), 15);
        assert_eq!(result.get_val(ResourceType::Instructions), 23);
    }

    #[test]
    fn test_checked_add_overflow() {
        let a = Resource::new(vec![i64::MAX, 0]);
        let b = Resource::new(vec![1, 0]);
        let err = a.checked_add(&b).unwrap_err();
        assert_eq!(err, ResourceError::Overflow { dimension: 0 });
    }

    #[test]
    fn test_checked_add_size_mismatch() {
        let a = Resource::new(vec![1, 2]);
        let b = Resource::make_empty_soroban();
        let err = a.checked_add(&b).unwrap_err();
        assert_eq!(err, ResourceError::SizeMismatch { lhs: 2, rhs: 7 });
    }

    #[test]
    fn test_checked_sub_valid() {
        let a = Resource::new(vec![10, 20]);
        let b = Resource::new(vec![5, 3]);
        let result = a.checked_sub(&b).unwrap();
        assert_eq!(result.get_val(ResourceType::Operations), 5);
        assert_eq!(result.get_val(ResourceType::Instructions), 17);
    }

    #[test]
    fn test_checked_sub_underflow() {
        let a = Resource::new(vec![0, 5]);
        let b = Resource::new(vec![1, 0]);
        let err = a.checked_sub(&b).unwrap_err();
        assert_eq!(err, ResourceError::Underflow { dimension: 0 });
    }

    #[test]
    fn test_checked_sub_size_mismatch() {
        let a = Resource::new(vec![1, 2]);
        let b = Resource::make_empty_soroban();
        let err = a.checked_sub(&b).unwrap_err();
        assert_eq!(err, ResourceError::SizeMismatch { lhs: 2, rhs: 7 });
    }
}
