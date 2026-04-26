//! Dense dynamic bitset for quorum intersection analysis.
//!
//! Matches stellar-core's `BitSet` used in `QuorumIntersectionCheckerImpl`.
//! Backed by `Vec<u64>` for compact storage and fast bitwise operations.

use std::hash::{Hash, Hasher};
use std::ops::{BitAnd, BitOrAssign, Sub};

/// Dense dynamic bitset backed by `Vec<u64>`.
///
/// Grows automatically to accommodate bits. All operations are O(n/64).
#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct BitSet {
    words: Vec<u64>,
}

impl BitSet {
    /// Create an empty bitset.
    pub fn new() -> Self {
        Self { words: Vec::new() }
    }

    /// Create a bitset with capacity for at least `n` bits.
    pub fn with_capacity(n: usize) -> Self {
        let num_words = n.div_ceil(64);
        Self {
            words: vec![0u64; num_words],
        }
    }

    /// Set bit `i`.
    pub fn set(&mut self, i: usize) {
        let word_idx = i / 64;
        if word_idx >= self.words.len() {
            self.words.resize(word_idx + 1, 0);
        }
        self.words[word_idx] |= 1u64 << (i % 64);
    }

    /// Unset bit `i`.
    pub fn unset(&mut self, i: usize) {
        let word_idx = i / 64;
        if word_idx < self.words.len() {
            self.words[word_idx] &= !(1u64 << (i % 64));
        }
    }

    /// Test if bit `i` is set.
    #[cfg(test)]
    pub fn get(&self, i: usize) -> bool {
        let word_idx = i / 64;
        if word_idx >= self.words.len() {
            return false;
        }
        (self.words[word_idx] >> (i % 64)) & 1 == 1
    }

    /// Count the number of set bits.
    pub fn count(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// Whether the bitset has no set bits.
    pub fn empty(&self) -> bool {
        self.words.iter().all(|&w| w == 0)
    }

    /// Count of bits set in both `self` and `other`.
    pub fn intersection_count(&self, other: &BitSet) -> usize {
        let min_len = self.words.len().min(other.words.len());
        let mut count = 0usize;
        for i in 0..min_len {
            count += (self.words[i] & other.words[i]).count_ones() as usize;
        }
        count
    }

    /// Whether `self` is a subset of (or equal to) `other`.
    pub fn is_subset_eq(&self, other: &BitSet) -> bool {
        for (i, &w) in self.words.iter().enumerate() {
            let other_w = other.words.get(i).copied().unwrap_or(0);
            if w & !other_w != 0 {
                return false;
            }
        }
        true
    }

    /// In-place union: `self |= other`.
    pub fn union_with(&mut self, other: &BitSet) {
        if other.words.len() > self.words.len() {
            self.words.resize(other.words.len(), 0);
        }
        for (i, &w) in other.words.iter().enumerate() {
            self.words[i] |= w;
        }
    }

    /// Returns the index of the maximum set bit, or 0 if empty.
    ///
    /// Matches stellar-core's `mRemaining.max()` usage as fallback for
    /// `pickSplitNode`.
    pub fn max(&self) -> usize {
        for (i, &w) in self.words.iter().enumerate().rev() {
            if w != 0 {
                return i * 64 + (63 - w.leading_zeros() as usize);
            }
        }
        0
    }

    /// Iterator over set bit indices, starting from `start` (inclusive).
    ///
    /// Matches stellar-core's `nextSet(i)` pattern:
    /// `for (size_t i = 0; bs.nextSet(i); ++i)`
    pub fn iter_set_from(&self, start: usize) -> NextSetIter<'_> {
        NextSetIter {
            bitset: self,
            pos: start,
        }
    }

    /// Iterator over all set bit indices.
    pub fn iter_set(&self) -> NextSetIter<'_> {
        self.iter_set_from(0)
    }
}

impl Default for BitSet {
    fn default() -> Self {
        Self::new()
    }
}

/// Bitwise AND: returns a new bitset with bits set in both operands.
impl BitAnd for &BitSet {
    type Output = BitSet;

    fn bitand(self, rhs: &BitSet) -> BitSet {
        let min_len = self.words.len().min(rhs.words.len());
        let mut words = Vec::with_capacity(min_len);
        for i in 0..min_len {
            words.push(self.words[i] & rhs.words[i]);
        }
        BitSet { words }
    }
}

/// Bitwise OR: returns a new bitset with bits set in either operand.
impl std::ops::BitOr for &BitSet {
    type Output = BitSet;

    fn bitor(self, rhs: &BitSet) -> BitSet {
        let max_len = self.words.len().max(rhs.words.len());
        let mut words = Vec::with_capacity(max_len);
        for i in 0..max_len {
            let a = self.words.get(i).copied().unwrap_or(0);
            let b = rhs.words.get(i).copied().unwrap_or(0);
            words.push(a | b);
        }
        BitSet { words }
    }
}

/// In-place OR: `self |= rhs`.
impl BitOrAssign<&BitSet> for BitSet {
    fn bitor_assign(&mut self, rhs: &BitSet) {
        self.union_with(rhs);
    }
}

/// Set difference: `self - rhs` (bits in self but not in rhs).
impl Sub for &BitSet {
    type Output = BitSet;

    fn sub(self, rhs: &BitSet) -> BitSet {
        let mut result = self.clone();
        for (i, &w) in rhs.words.iter().enumerate() {
            if i < result.words.len() {
                result.words[i] &= !w;
            }
        }
        result
    }
}

/// Hash implementation for use as cache keys.
impl Hash for BitSet {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Hash only the significant words (trim trailing zeros).
        let effective_len = self
            .words
            .iter()
            .rposition(|&w| w != 0)
            .map_or(0, |i| i + 1);
        effective_len.hash(state);
        for w in &self.words[..effective_len] {
            w.hash(state);
        }
    }
}

/// Iterator over set bit indices.
pub(crate) struct NextSetIter<'a> {
    bitset: &'a BitSet,
    pos: usize,
}

impl Iterator for NextSetIter<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        let total_bits = self.bitset.words.len() * 64;
        while self.pos < total_bits {
            let word_idx = self.pos / 64;
            let bit_idx = self.pos % 64;
            // Mask off bits below our current position within this word.
            let masked = self.bitset.words[word_idx] >> bit_idx;
            if masked != 0 {
                let offset = masked.trailing_zeros() as usize;
                let result = self.pos + offset;
                self.pos = result + 1;
                return Some(result);
            }
            // Skip to next word.
            self.pos = (word_idx + 1) * 64;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::hash_map::DefaultHasher;

    fn hash_bitset(bs: &BitSet) -> u64 {
        let mut hasher = DefaultHasher::new();
        bs.hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn test_empty() {
        let bs = BitSet::new();
        assert!(bs.empty());
        assert_eq!(bs.count(), 0);
        assert!(!bs.get(0));
        assert!(!bs.get(100));
    }

    #[test]
    fn test_set_get_unset() {
        let mut bs = BitSet::new();
        bs.set(0);
        assert!(bs.get(0));
        assert!(!bs.get(1));
        assert_eq!(bs.count(), 1);

        bs.set(63);
        assert!(bs.get(63));
        assert_eq!(bs.count(), 2);

        bs.set(64);
        assert!(bs.get(64));
        assert_eq!(bs.count(), 3);

        bs.unset(0);
        assert!(!bs.get(0));
        assert_eq!(bs.count(), 2);

        bs.unset(200); // no-op
        assert_eq!(bs.count(), 2);
    }

    #[test]
    fn test_large_bits() {
        let mut bs = BitSet::new();
        bs.set(200);
        bs.set(500);
        assert!(bs.get(200));
        assert!(bs.get(500));
        assert!(!bs.get(201));
        assert_eq!(bs.count(), 2);
    }

    #[test]
    fn test_with_capacity() {
        let mut bs = BitSet::with_capacity(128);
        bs.set(0);
        bs.set(127);
        assert!(bs.get(0));
        assert!(bs.get(127));
        assert_eq!(bs.count(), 2);
    }

    #[test]
    fn test_intersection_count() {
        let mut a = BitSet::new();
        a.set(0);
        a.set(1);
        a.set(2);

        let mut b = BitSet::new();
        b.set(1);
        b.set(2);
        b.set(3);

        assert_eq!(a.intersection_count(&b), 2);
        assert_eq!(b.intersection_count(&a), 2);

        let c = BitSet::new();
        assert_eq!(a.intersection_count(&c), 0);
    }

    #[test]
    fn test_is_subset_eq() {
        let mut a = BitSet::new();
        a.set(1);
        a.set(2);

        let mut b = BitSet::new();
        b.set(0);
        b.set(1);
        b.set(2);
        b.set(3);

        assert!(a.is_subset_eq(&b));
        assert!(!b.is_subset_eq(&a));
        assert!(a.is_subset_eq(&a));

        let empty = BitSet::new();
        assert!(empty.is_subset_eq(&a));
    }

    #[test]
    fn test_bitand() {
        let mut a = BitSet::new();
        a.set(0);
        a.set(1);
        a.set(2);

        let mut b = BitSet::new();
        b.set(1);
        b.set(2);
        b.set(3);

        let c = &a & &b;
        assert!(!c.get(0));
        assert!(c.get(1));
        assert!(c.get(2));
        assert!(!c.get(3));
        assert_eq!(c.count(), 2);
    }

    #[test]
    fn test_bitor_assign() {
        let mut a = BitSet::new();
        a.set(0);
        a.set(1);

        let mut b = BitSet::new();
        b.set(2);
        b.set(3);

        a |= &b;
        assert!(a.get(0));
        assert!(a.get(1));
        assert!(a.get(2));
        assert!(a.get(3));
        assert_eq!(a.count(), 4);
    }

    #[test]
    fn test_sub() {
        let mut a = BitSet::new();
        a.set(0);
        a.set(1);
        a.set(2);

        let mut b = BitSet::new();
        b.set(1);
        b.set(2);
        b.set(3);

        let c = &a - &b;
        assert!(c.get(0));
        assert!(!c.get(1));
        assert!(!c.get(2));
        assert!(!c.get(3));
        assert_eq!(c.count(), 1);
    }

    #[test]
    fn test_max() {
        let mut bs = BitSet::new();
        assert_eq!(bs.max(), 0);

        bs.set(5);
        assert_eq!(bs.max(), 5);

        bs.set(100);
        assert_eq!(bs.max(), 100);

        bs.unset(100);
        assert_eq!(bs.max(), 5);
    }

    #[test]
    fn test_iter_set() {
        let mut bs = BitSet::new();
        bs.set(0);
        bs.set(3);
        bs.set(64);
        bs.set(65);
        bs.set(200);

        let bits: Vec<usize> = bs.iter_set().collect();
        assert_eq!(bits, vec![0, 3, 64, 65, 200]);
    }

    #[test]
    fn test_iter_set_from() {
        let mut bs = BitSet::new();
        bs.set(0);
        bs.set(3);
        bs.set(64);
        bs.set(65);

        let bits: Vec<usize> = bs.iter_set_from(3).collect();
        assert_eq!(bits, vec![3, 64, 65]);

        let bits: Vec<usize> = bs.iter_set_from(4).collect();
        assert_eq!(bits, vec![64, 65]);
    }

    #[test]
    fn test_iter_set_empty() {
        let bs = BitSet::new();
        let bits: Vec<usize> = bs.iter_set().collect();
        assert!(bits.is_empty());
    }

    #[test]
    fn test_hash_consistency() {
        let mut a = BitSet::new();
        a.set(1);
        a.set(5);

        let mut b = BitSet::new();
        b.set(1);
        b.set(5);

        assert_eq!(hash_bitset(&a), hash_bitset(&b));
    }

    #[test]
    fn test_hash_differs() {
        let mut a = BitSet::new();
        a.set(1);

        let mut b = BitSet::new();
        b.set(2);

        assert_ne!(hash_bitset(&a), hash_bitset(&b));
    }

    #[test]
    fn test_hash_trailing_zeros_normalized() {
        // Two bitsets with same data but different internal lengths should hash the same.
        let mut a = BitSet::with_capacity(64);
        a.set(1);

        let mut b = BitSet::with_capacity(256);
        b.set(1);

        assert_eq!(hash_bitset(&a), hash_bitset(&b));
    }

    #[test]
    fn test_clone_independence() {
        let mut a = BitSet::new();
        a.set(5);
        let mut b = a.clone();
        b.set(10);
        assert!(!a.get(10));
        assert!(b.get(10));
    }
}
