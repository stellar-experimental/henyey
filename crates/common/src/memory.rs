//! Memory estimation trait and helpers for per-component heap tracking.
//!
//! This module provides the [`MemoryEstimate`] trait for types that can report
//! their approximate heap memory usage, plus helper functions for estimating
//! the heap footprint of standard collections.
//!
//! # Design
//!
//! Implementations should be O(1) — read capacities, lengths, and atomic
//! counters, never iterate entries. Estimates should be conservative
//! (slight over-count is acceptable) and exclude shared references (`Arc`)
//! that may be counted by other components.
//!
//! # Usage
//!
//! ```ignore
//! use henyey_common::memory::{MemoryEstimate, ComponentMemory, hashmap_heap_bytes};
//!
//! impl MemoryEstimate for MyCache {
//!     fn estimate_heap_bytes(&self) -> usize {
//!         hashmap_heap_bytes(self.map.capacity(), 32, 64)
//!     }
//! }
//! ```

/// Trait for types that can estimate their heap memory usage.
///
/// Implementations should return a conservative estimate of the heap
/// bytes owned by this value, excluding shared references (Arc) that
/// may be counted elsewhere. The estimate must be O(1) — read
/// capacities, lengths, and atomic counters, never iterate entries.
pub trait MemoryEstimate {
    fn estimate_heap_bytes(&self) -> usize;
}

/// A named memory measurement from a single component.
#[derive(Debug, Clone)]
pub struct ComponentMemory {
    pub name: &'static str,
    /// Size in bytes (heap-allocated or file-backed depending on `is_heap`).
    pub bytes: u64,
    pub entry_count: u64,
    /// Whether this component is heap-allocated (true) or file-backed/mmap (false).
    pub is_heap: bool,
}

impl ComponentMemory {
    pub fn new(name: &'static str, bytes: u64, entry_count: u64) -> Self {
        Self {
            name,
            bytes,
            entry_count,
            is_heap: true,
        }
    }

    /// Create a non-heap (file-backed/mmap) component measurement.
    pub fn new_non_heap(name: &'static str, bytes: u64, entry_count: u64) -> Self {
        Self {
            name,
            bytes,
            entry_count,
            is_heap: false,
        }
    }

    pub fn heap_mb(&self) -> f64 {
        self.bytes as f64 / (1024.0 * 1024.0)
    }
}

/// Estimate heap bytes for a `HashMap` with the given capacity and entry sizes.
///
/// Accounts for hashbrown's internal layout: each entry stores the key and
/// value inline in a flat array, plus one control byte per slot and some
/// alignment padding. This matches the std `HashMap` (backed by hashbrown).
pub fn hashmap_heap_bytes(capacity: usize, key_size: usize, value_size: usize) -> usize {
    if capacity == 0 {
        return 0;
    }
    // hashbrown allocates capacity rounded up to a power of 2 (or next group boundary).
    // Each slot: key + value bytes inline.
    // Control bytes: 1 per slot + 16 bytes (Group::WIDTH) sentinel padding.
    // We approximate by using the reported capacity directly since HashMap::capacity()
    // already returns the usable slot count.
    let entry_size = key_size + value_size;
    let data_bytes = capacity * entry_size;
    let control_bytes = capacity + 16; // 1 byte per slot + Group::WIDTH sentinel
    data_bytes + control_bytes
}

/// Estimate heap bytes for a `HashSet` with the given capacity and key size.
///
/// A `HashSet<K>` is internally a `HashMap<K, ()>`, so value_size is 0.
pub fn hashset_heap_bytes(capacity: usize, key_size: usize) -> usize {
    hashmap_heap_bytes(capacity, key_size, 0)
}

/// Estimate heap bytes for a `Vec` with the given capacity and element size.
pub fn vec_heap_bytes(capacity: usize, element_size: usize) -> usize {
    capacity * element_size
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hashmap_heap_bytes_zero() {
        assert_eq!(hashmap_heap_bytes(0, 32, 64), 0);
    }

    #[test]
    fn test_hashmap_heap_bytes_nonzero() {
        let bytes = hashmap_heap_bytes(100, 32, 64);
        // 100 * (32 + 64) + 100 + 16 = 9716
        assert_eq!(bytes, 9716);
    }

    #[test]
    fn test_hashset_heap_bytes() {
        let bytes = hashset_heap_bytes(100, 32);
        // 100 * 32 + 100 + 16 = 3316
        assert_eq!(bytes, 3316);
    }

    #[test]
    fn test_vec_heap_bytes() {
        assert_eq!(vec_heap_bytes(100, 8), 800);
        assert_eq!(vec_heap_bytes(0, 8), 0);
    }

    #[test]
    fn test_component_memory() {
        let cm = ComponentMemory::new("test", 1024 * 1024, 100);
        assert_eq!(cm.name, "test");
        assert!((cm.heap_mb() - 1.0).abs() < 0.001);
    }
}
