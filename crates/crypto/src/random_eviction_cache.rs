//! Generic random-two-choice eviction cache.
//!
//! Implements the same eviction policy as stellar-core's `RandomEvictionCache`
//! (`stellar-core/src/util/RandomEvictionCache.h`): when the cache is full,
//! randomly pick two entries and evict the less-recently-used one. This
//! degrades more gracefully under adversarial load patterns than FIFO or LRU,
//! with less bookkeeping than full LRU.

use std::collections::HashMap;
use std::hash::Hash;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

/// A fixed-size cache with random-two-choice eviction.
///
/// On each insertion that would exceed capacity, two entries are randomly
/// selected and the one with the older last-access generation is evicted.
/// Cache hits update the entry's last-access generation, giving frequently
/// accessed entries better survival odds.
///
/// Matches stellar-core's `RandomEvictionCache<K, V>` in
/// `src/util/RandomEvictionCache.h`.
pub struct RandomEvictionCache<K, V> {
    map: HashMap<K, CacheEntry<V>>,
    keys: Vec<K>,
    generation: u64,
    capacity: usize,
    rng: StdRng,
}

struct CacheEntry<V> {
    value: V,
    last_access: u64,
    key_index: usize,
}

impl<K: Eq + Hash + Clone, V> RandomEvictionCache<K, V> {
    /// Creates a new cache with the given capacity, seeded with 0.
    ///
    /// Matches stellar-core's `randomEvictionCacheSeed{0}`.
    pub fn new(capacity: usize) -> Self {
        Self::with_seed(capacity, 0)
    }

    /// Creates a new cache with the given capacity and RNG seed.
    ///
    /// Use this in tests for deterministic behavior.
    pub fn with_seed(capacity: usize, seed: u64) -> Self {
        Self {
            map: HashMap::with_capacity(capacity.saturating_add(1)),
            keys: Vec::with_capacity(capacity.saturating_add(1)),
            generation: 0,
            capacity,
            rng: StdRng::seed_from_u64(seed),
        }
    }

    /// Looks up a key and updates its last-access generation on hit.
    ///
    /// Matches stellar-core's `maybeGet()` which increments the generation
    /// counter and records it on the accessed entry.
    pub fn get(&mut self, key: &K) -> Option<&V> {
        // We need to do a two-step lookup to satisfy the borrow checker:
        // first check existence, then mutate.
        if !self.map.contains_key(key) {
            return None;
        }
        self.generation += 1;
        let entry = self.map.get_mut(key).unwrap();
        entry.last_access = self.generation;
        Some(&entry.value)
    }

    /// Inserts or updates a key-value pair, evicting if over capacity.
    ///
    /// Matches stellar-core's `put()`:
    /// 1. Increment generation.
    /// 2. If key exists: update value and last_access.
    /// 3. If key is new: insert and evict if over capacity.
    pub fn put(&mut self, key: K, value: V) {
        self.generation += 1;
        let generation = self.generation;

        if let Some(entry) = self.map.get_mut(&key) {
            // Key exists — update value and last_access (no size change).
            entry.value = value;
            entry.last_access = generation;
        } else {
            // New key — insert.
            let key_index = self.keys.len();
            self.keys.push(key.clone());
            self.map.insert(
                key,
                CacheEntry {
                    value,
                    last_access: generation,
                    key_index,
                },
            );

            // Evict if over capacity.
            if self.keys.len() > self.capacity {
                self.evict_one();
            }
        }
    }

    /// Returns the number of entries in the cache.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Randomly pick two entries and evict the less-recently-used one.
    ///
    /// Uses strict `<` for the comparison, matching stellar-core:
    /// `vp1->second.mLastAccess < vp2->second.mLastAccess ? vp1 : vp2`
    fn evict_one(&mut self) {
        let sz = self.keys.len();
        if sz == 0 {
            return;
        }

        let i1 = self.rng.gen_range(0..sz);
        let i2 = self.rng.gen_range(0..sz);

        // Determine victim: entry with strictly lower last_access loses.
        // If equal (including same index), evict the second pick (matching
        // stellar-core's else branch).
        let la1 = self.map[&self.keys[i1]].last_access;
        let la2 = self.map[&self.keys[i2]].last_access;
        let victim_idx = if la1 < la2 { i1 } else { i2 };

        // Remove victim from map.
        let victim_key = self.keys[victim_idx].clone();
        self.map.remove(&victim_key);

        // Swap-remove from keys vec.
        let last_idx = self.keys.len() - 1;
        if victim_idx != last_idx {
            self.keys.swap(victim_idx, last_idx);
            // Update the swapped entry's key_index.
            let swapped_key = &self.keys[victim_idx];
            self.map.get_mut(swapped_key).unwrap().key_index = victim_idx;
        }
        self.keys.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_insert_and_get() {
        let mut cache = RandomEvictionCache::new(10);
        cache.put([1u8; 32], true);
        cache.put([2u8; 32], false);

        assert_eq!(cache.get(&[1u8; 32]), Some(&true));
        assert_eq!(cache.get(&[2u8; 32]), Some(&false));
    }

    #[test]
    fn test_get_returns_none_for_missing() {
        let mut cache: RandomEvictionCache<[u8; 32], bool> = RandomEvictionCache::new(10);
        assert_eq!(cache.get(&[99u8; 32]), None);
    }

    #[test]
    fn test_capacity_not_exceeded() {
        let capacity = 100;
        let mut cache = RandomEvictionCache::new(capacity);

        for i in 0..500u32 {
            let mut key = [0u8; 32];
            key[..4].copy_from_slice(&i.to_le_bytes());
            cache.put(key, i % 2 == 0);
            assert!(cache.len() <= capacity);
        }
    }

    #[test]
    fn test_duplicate_put_updates_value() {
        let mut cache = RandomEvictionCache::new(10);
        let key = [42u8; 32];

        cache.put(key, false);
        assert_eq!(cache.get(&key), Some(&false));

        cache.put(key, true);
        assert_eq!(cache.get(&key), Some(&true));

        // Size should not have increased.
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn test_zero_capacity() {
        let mut cache = RandomEvictionCache::new(0);

        for i in 0..10u8 {
            cache.put([i; 32], true);
        }

        // Cache should stay empty — each insert triggers immediate eviction.
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn test_capacity_one() {
        let mut cache = RandomEvictionCache::new(1);
        cache.put([1u8; 32], true);
        assert_eq!(cache.len(), 1);

        cache.put([2u8; 32], false);
        // Only one entry should remain.
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn test_recently_accessed_entries_survive_better() {
        // Invariant test: entries that are accessed via get() should survive
        // eviction at a higher rate than entries that are never accessed.
        // We use a large capacity and moderate churn to make the statistical
        // difference observable.
        let capacity = 200;
        let mut cache = RandomEvictionCache::with_seed(capacity, 12345);

        // Fill cache to capacity.
        let mut keys: Vec<[u8; 32]> = Vec::new();
        for i in 0..capacity as u32 {
            let mut key = [0u8; 32];
            key[..4].copy_from_slice(&i.to_le_bytes());
            keys.push(key);
            cache.put(key, true);
        }

        // Repeatedly access the first 20 entries to keep their last_access high.
        for _ in 0..50 {
            for key in &keys[..20] {
                cache.get(key);
            }
        }

        // Churn with 100 new inserts (causing ~100 evictions from a pool of 200).
        for i in capacity as u32..(capacity as u32 + 100) {
            let mut key = [0u8; 32];
            key[..4].copy_from_slice(&i.to_le_bytes());
            cache.put(key, true);
        }

        // Count survivors from accessed vs non-accessed groups.
        let accessed_survivors = keys[..20].iter().filter(|k| cache.get(k).is_some()).count();
        let non_accessed_survivors = keys[20..capacity]
            .iter()
            .filter(|k| cache.get(k).is_some())
            .count();

        // Accessed entries (20 total) should survive at a higher rate than
        // non-accessed entries (180 total).
        let accessed_rate = accessed_survivors as f64 / 20.0;
        let non_accessed_rate = non_accessed_survivors as f64 / 180.0;

        assert!(
            accessed_rate > non_accessed_rate,
            "accessed survival rate ({accessed_rate:.2}) should exceed \
             non-accessed rate ({non_accessed_rate:.2})"
        );
    }

    #[test]
    fn test_eviction_does_not_corrupt_indices() {
        // Stress test: insert and evict many entries, verify internal
        // consistency (len matches, all stored keys are retrievable).
        let capacity = 20;
        let mut cache = RandomEvictionCache::with_seed(capacity, 99);

        for i in 0..1000u32 {
            let mut key = [0u8; 32];
            key[..4].copy_from_slice(&i.to_le_bytes());
            cache.put(key, i % 3 == 0);
            assert!(cache.len() <= capacity);
        }

        // Verify all entries in the cache are retrievable.
        let final_len = cache.len();
        let mut found = 0;
        for i in 0..1000u32 {
            let mut key = [0u8; 32];
            key[..4].copy_from_slice(&i.to_le_bytes());
            if cache.get(&key).is_some() {
                found += 1;
            }
        }
        assert_eq!(found, final_len);
    }
}
