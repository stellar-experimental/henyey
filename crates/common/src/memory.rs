//! Heap-estimation helpers for per-component memory tracking.
//!
//! This module provides [`ComponentMemory`] for reporting a component's
//! memory usage, plus helper functions for estimating the heap footprint of
//! standard collections.
//!
//! # Design
//!
//! Estimates should be O(1) — read capacities, lengths, and atomic
//! counters, never iterate entries. Estimates should be conservative
//! (slight over-count is acceptable) and exclude shared references (`Arc`)
//! that may be counted by other components.

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

/// A subsystem that can report its heap footprint as named components.
///
/// Implemented by long-lived subsystems (herder, overlay, …) whose owned
/// allocations live outside the ledger manager's own report call site and so
/// cannot be attributed by [`crate::memory`] helpers alone. The ledger
/// manager holds a registry of `Weak<dyn MemoryReporter>` and folds each
/// reporter's components into the periodic memory report (see #3845).
///
/// Implementations MUST follow the same discipline as the built-in
/// components: `memory_components()` is O(1) per component (read
/// capacities/lengths/counters, never iterate entries), conservative
/// (slight over-count acceptable), and excludes `Arc`-shared ledger state to
/// avoid double-counting. It MUST NOT acquire any lock already held by the
/// ledger-close path that invokes it.
///
/// Object-safe: used only through `dyn MemoryReporter`.
pub trait MemoryReporter: Send + Sync {
    /// Return this subsystem's per-component heap estimates.
    fn memory_components(&self) -> Vec<ComponentMemory>;
}

/// Estimate heap bytes for a `BTreeMap` with `len` entries.
///
/// `BTreeMap` stores entries in B-tree nodes (Rust's `B = 6`, so each node
/// holds up to `2*B - 1 = 11` key/value pairs). We approximate the footprint
/// as the key/value payload plus per-node bookkeeping (child pointers + a
/// length field). The result is monotonic in `len` and conservative.
///
/// `BTreeSet<K>` is a `BTreeMap<K, ()>`, so pass `value_size = 0`.
pub fn btreemap_heap_bytes(len: usize, key_size: usize, value_size: usize) -> usize {
    if len == 0 {
        return 0;
    }
    // 2*B - 1 with B = 6 (libstd's BTree branching factor).
    const NODE_CAPACITY: usize = 11;
    let nodes = len.div_ceil(NODE_CAPACITY);
    let payload = len * (key_size + value_size);
    // Over-count with the internal-node layout for every node (each holds up
    // to NODE_CAPACITY+1 child pointers plus a length field) — conservative.
    let per_node_overhead =
        (NODE_CAPACITY + 1) * std::mem::size_of::<usize>() + std::mem::size_of::<u16>();
    payload + nodes * per_node_overhead
}

/// Estimate heap bytes for a `VecDeque` with the given capacity and element size.
///
/// `VecDeque` backs its ring buffer with a single contiguous allocation of
/// `capacity` elements, so the footprint is `capacity * element_size`.
pub fn vecdeque_heap_bytes(capacity: usize, element_size: usize) -> usize {
    capacity * element_size
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

    #[test]
    fn test_btreemap_heap_bytes_zero() {
        assert_eq!(btreemap_heap_bytes(0, 32, 64), 0);
    }

    #[test]
    fn test_btreemap_heap_bytes_monotonic() {
        // Empty is zero; any entry is strictly positive; more entries is more.
        let one = btreemap_heap_bytes(1, 32, 64);
        let many = btreemap_heap_bytes(100, 32, 64);
        assert!(one > 0);
        assert!(many > one);
        // Larger key/value sizes cost strictly more for the same len.
        assert!(btreemap_heap_bytes(100, 64, 128) > btreemap_heap_bytes(100, 32, 64));
    }

    #[test]
    fn test_btreemap_heap_bytes_set_like() {
        // A BTreeSet is a BTreeMap<K, ()>; value_size 0 still counts the keys.
        assert!(btreemap_heap_bytes(50, 8, 0) > 0);
    }

    #[test]
    fn test_vecdeque_heap_bytes() {
        assert_eq!(vecdeque_heap_bytes(0, 16), 0);
        assert_eq!(vecdeque_heap_bytes(64, 16), 1024);
        assert!(vecdeque_heap_bytes(128, 16) > vecdeque_heap_bytes(64, 16));
    }
}
