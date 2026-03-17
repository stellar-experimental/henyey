//! Quorum set operations for SCP.
//!
//! This module provides core utilities for working with quorum sets in the
//! Stellar Consensus Protocol. Quorum sets define which validators a node
//! trusts and how consensus decisions are made.
//!
//! # Key Concepts
//!
//! ## Quorum Slice
//!
//! A quorum slice is a set of nodes that a particular node trusts for consensus.
//! Each node defines its own quorum slice. A slice is satisfied when at least
//! `threshold` of its members (validators or inner sets) agree.
//!
//! ## Quorum
//!
//! A quorum is a set of nodes where every member's quorum slice is satisfied
//! by other members of the set. Quorums are used to confirm values - when a
//! quorum agrees, the decision is irreversible.
//!
//! ## Blocking Set (V-Blocking)
//!
//! A blocking set intersects every quorum slice, meaning it can prevent
//! any quorum from being formed. Blocking sets are used to accept values -
//! if a blocking set accepts a value, the node must accept it too.
//!
//! # Quorum Set Structure
//!
//! A quorum set consists of:
//! - `threshold`: Minimum number of members that must agree
//! - `validators`: Direct validator nodes in the slice
//! - `inner_sets`: Nested quorum sets (for hierarchical trust)
//!
//! # Example
//!
//! ```ignore
//! // A 2-of-3 quorum set
//! let qs = simple_quorum_set(2, vec![node_a, node_b, node_c]);
//!
//! // Check if nodes form a quorum
//! let nodes = HashSet::from([node_a, node_b]);
//! if is_quorum(&qs, &nodes, get_quorum_set) {
//!     // Nodes form a quorum
//! }
//! ```

use std::collections::HashSet;

use henyey_common::Hash256;
use stellar_xdr::curr::{NodeId, ScpQuorumSet};

/// Maximum allowed nesting level for quorum sets.
///
/// This constant defines the maximum depth of nested inner sets allowed
/// in a quorum set. This limit prevents excessive recursion during
/// quorum set validation and ensures bounded memory usage.
///
/// The value matches the stellar-core implementation.
pub const MAXIMUM_QUORUM_NESTING_LEVEL: u32 = 4;

/// Maximum allowed number of nodes in a quorum set.
///
/// This constant defines the maximum total number of unique nodes
/// (validators) that can be referenced in a quorum set, including
/// all nested inner sets.
///
/// The value matches the stellar-core implementation.
pub const MAXIMUM_QUORUM_NODES: usize = 1000;

/// Check if a set of nodes satisfies a quorum slice.
///
/// A quorum slice is satisfied if at least `threshold` of its members
/// (validators + inner sets) are satisfied.
///
/// # Arguments
/// * `quorum_set` - The quorum set defining the slice
/// * `nodes` - The set of nodes to check
/// * `get_quorum_set` - Function to get quorum set for a node
///
/// # Returns
/// True if the nodes satisfy this quorum slice.
pub fn is_quorum_slice<F>(
    quorum_set: &ScpQuorumSet,
    nodes: &HashSet<NodeId>,
    _get_quorum_set: &F,
) -> bool
where
    F: Fn(&NodeId) -> Option<ScpQuorumSet>,
{
    fn is_quorum_slice_inner(quorum_set: &ScpQuorumSet, nodes: &HashSet<NodeId>) -> bool {
        let threshold = quorum_set.threshold as usize;
        if threshold == 0 {
            return true;
        }

        let mut count = 0;

        for validator in quorum_set.validators.iter() {
            if nodes.contains(validator) {
                count += 1;
                if count >= threshold {
                    return true;
                }
            }
        }

        for inner_set in quorum_set.inner_sets.iter() {
            if is_quorum_slice_inner(inner_set, nodes) {
                count += 1;
                if count >= threshold {
                    return true;
                }
            }
        }

        count >= threshold
    }

    is_quorum_slice_inner(quorum_set, nodes)
}

/// Check if a set of nodes forms a quorum.
///
/// Uses iterative pruning to find a quorum within the given set of nodes.
/// Repeatedly removes nodes whose quorum slices aren't satisfied by the
/// remaining set until the set stabilizes. Then checks if the local node's
/// quorum slice is satisfied by the surviving set.
///
/// This matches the stellar-core `LocalNode::isQuorum()` behavior, which allows
/// finding valid quorums as subsets of the input nodes.
///
/// # Arguments
/// * `quorum_set` - The local node's quorum set
/// * `nodes` - The set of nodes to check
/// * `get_quorum_set` - Function to get quorum set for a node
///
/// # Returns
/// True if a subset of the nodes forms a quorum that satisfies the local
/// node's quorum slice.
pub fn is_quorum<F>(quorum_set: &ScpQuorumSet, nodes: &HashSet<NodeId>, get_quorum_set: F) -> bool
where
    F: Fn(&NodeId) -> Option<ScpQuorumSet>,
{
    // Iteratively prune nodes whose quorum slices aren't satisfied
    // until the set stabilizes (matches stellar-core do-while loop in LocalNode::isQuorum)
    let mut remaining: Vec<NodeId> = nodes.iter().cloned().collect();

    loop {
        let count = remaining.len();

        // Keep only nodes whose quorum slices are satisfied by the current set
        let remaining_set: HashSet<NodeId> = remaining.iter().cloned().collect();
        remaining.retain(|node_id| {
            if let Some(qs) = get_quorum_set(node_id) {
                is_quorum_slice(&qs, &remaining_set, &get_quorum_set)
            } else {
                false
            }
        });

        if remaining.len() == count {
            break;
        }
    }

    // Check if the local node's quorum slice is satisfied by the surviving set
    let remaining_set: HashSet<NodeId> = remaining.into_iter().collect();
    is_quorum_slice(quorum_set, &remaining_set, &get_quorum_set)
}

/// Check if a set of nodes is a blocking set.
///
/// A set B is a blocking set if it intersects every quorum slice.
/// This means that no quorum can be formed without at least one
/// node from B participating.
///
/// # Arguments
/// * `quorum_set` - The quorum set to check against
/// * `nodes` - The set of nodes to check
///
/// # Returns
/// True if the nodes form a blocking set.
pub fn is_blocking_set(quorum_set: &ScpQuorumSet, nodes: &HashSet<NodeId>) -> bool {
    let total = quorum_set.validators.len() + quorum_set.inner_sets.len();
    let threshold = quorum_set.threshold as usize;

    // There is no v-blocking set for the empty set (matches stellar-core LocalNode::isVBlockingInternal)
    if threshold == 0 {
        return false;
    }

    // We need to block (total - threshold + 1) members to prevent any quorum slice
    let blocking_threshold = total.saturating_sub(threshold) + 1;

    let mut count = 0;

    // Count validators in the blocking set
    for validator in quorum_set.validators.iter() {
        if nodes.contains(validator) {
            count += 1;
        }
    }

    // Count inner sets that are blocked
    for inner_set in quorum_set.inner_sets.iter() {
        if is_blocking_set(inner_set, nodes) {
            count += 1;
        }
    }

    count >= blocking_threshold
}

/// Check if a set of nodes is a v-blocking set for a given quorum set.
///
/// A set B is v-blocking for node v if B intersects all of v's quorum slices.
/// This is equivalent to is_blocking_set for the node's quorum set.
pub fn is_v_blocking(quorum_set: &ScpQuorumSet, nodes: &HashSet<NodeId>) -> bool {
    is_blocking_set(quorum_set, nodes)
}

/// Check if a quorum set is sane.
///
/// This validates structural constraints, duplicate nodes, and optionally
/// enforces a safety threshold (> 50%).
pub fn is_quorum_set_sane(quorum_set: &ScpQuorumSet, extra_checks: bool) -> Result<(), String> {
    let mut checker = QuorumSetSanityChecker {
        extra_checks,
        known_nodes: HashSet::new(),
        count: 0,
    };
    checker.check_sanity(quorum_set, 0)?;

    if checker.count < 1 || checker.count > MAXIMUM_QUORUM_NODES {
        return Err("Total number of nodes in a quorum must be within 1 and 1000".to_string());
    }

    Ok(())
}

struct QuorumSetSanityChecker {
    extra_checks: bool,
    known_nodes: HashSet<NodeId>,
    count: usize,
}

impl QuorumSetSanityChecker {
    fn check_sanity(&mut self, quorum_set: &ScpQuorumSet, depth: u32) -> Result<(), String> {
        if depth > MAXIMUM_QUORUM_NESTING_LEVEL {
            return Err("Maximum quorum nesting level exceeded".to_string());
        }

        if quorum_set.threshold < 1 {
            return Err("Threshold must be greater than 0".to_string());
        }

        let total = quorum_set.validators.len() + quorum_set.inner_sets.len();
        if quorum_set.threshold as usize > total {
            return Err("Threshold exceeds total number of entries".to_string());
        }

        let v_blocking_size = total.saturating_sub(quorum_set.threshold as usize) + 1;
        if self.extra_checks && (quorum_set.threshold as usize) < v_blocking_size {
            return Err("Threshold is lower than the v-blocking size (< 51%).".to_string());
        }

        self.count = self.count.saturating_add(quorum_set.validators.len());
        for node in quorum_set.validators.iter() {
            if !self.known_nodes.insert(node.clone()) {
                return Err("Duplicate node found in quorum configuration".to_string());
            }
        }

        for inner in quorum_set.inner_sets.iter() {
            self.check_sanity(inner, depth + 1)?;
        }

        Ok(())
    }
}

/// Find the closest v-blocking set for a quorum set.
///
/// Returns a minimal set of nodes from `nodes` that would v-block the quorum
/// set, excluding `excluded` if provided. An empty result means the set is
/// already v-blocking.
pub fn find_closest_v_blocking(
    quorum_set: &ScpQuorumSet,
    nodes: &HashSet<NodeId>,
    excluded: Option<&NodeId>,
) -> Vec<NodeId> {
    let mut left_till_block =
        1i64 + quorum_set.validators.len() as i64 + quorum_set.inner_sets.len() as i64
            - quorum_set.threshold as i64;

    if left_till_block <= 0 {
        return Vec::new();
    }

    let mut res = Vec::new();

    for validator in quorum_set.validators.iter() {
        if excluded == Some(validator) {
            continue;
        }

        if nodes.contains(validator) {
            res.push(validator.clone());
        } else {
            left_till_block -= 1;
            if left_till_block == 0 {
                return Vec::new();
            }
        }
    }

    let mut res_internals: Vec<(usize, usize, Vec<NodeId>)> = Vec::new();
    for (index, inner) in quorum_set.inner_sets.iter().enumerate() {
        let v = find_closest_v_blocking(inner, nodes, excluded);
        if v.is_empty() {
            left_till_block -= 1;
            if left_till_block == 0 {
                return Vec::new();
            }
        } else {
            res_internals.push((v.len(), index, v));
        }
    }

    if res.len() > left_till_block as usize {
        res.truncate(left_till_block as usize);
    }
    left_till_block -= res.len() as i64;

    res_internals.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let mut idx = 0;
    while left_till_block != 0 && idx < res_internals.len() {
        res.extend(res_internals[idx].2.iter().cloned());
        left_till_block -= 1;
        idx += 1;
    }

    res
}

/// Compute the hash of a quorum set.
///
/// This hash is used to reference quorum sets by their content
/// in SCP messages, allowing efficient comparison and storage.
pub fn hash_quorum_set(quorum_set: &ScpQuorumSet) -> Hash256 {
    Hash256::hash_xdr(quorum_set).unwrap_or(Hash256::ZERO)
}

/// Normalize a quorum set by sorting validators and inner sets.
///
/// This ensures consistent hashing regardless of the order in which
/// validators were added to the quorum set.
///
/// If `id_to_remove` is provided, removes that node from the quorum set
/// at all levels and adjusts thresholds accordingly. This is used during
/// the EXTERNALIZE phase and leader computation to exclude a node from
/// the quorum set while maintaining correct semantics.
pub fn normalize_quorum_set(quorum_set: &mut ScpQuorumSet) {
    normalize_quorum_set_with_remove(quorum_set, None);
}

/// Normalize a quorum set, optionally removing a node.
///
/// Like `normalize_quorum_set`, but accepts an optional `id_to_remove`
/// parameter that removes the specified node from validators at all levels
/// and decrements thresholds accordingly. Matches the stellar-core `normalizeQSet`
/// function signature.
pub fn normalize_quorum_set_with_remove(
    quorum_set: &mut ScpQuorumSet,
    id_to_remove: Option<&NodeId>,
) {
    normalize_quorum_set_simplify(quorum_set, id_to_remove);

    let mut validators: Vec<_> = quorum_set.validators.iter().cloned().collect();
    validators.sort_by(node_id_cmp);
    quorum_set.validators = validators.try_into().unwrap_or_default();

    let mut inner_sets: Vec<_> = quorum_set.inner_sets.iter().cloned().collect();
    for inner_set in &mut inner_sets {
        // Only pass id_to_remove for the simplify step (already done above recursively),
        // but the reorder step doesn't need it again
        normalize_quorum_set_reorder(inner_set);
    }

    inner_sets.sort_by(quorum_set_cmp);
    quorum_set.inner_sets = inner_sets.try_into().unwrap_or_default();
}

fn normalize_quorum_set_reorder(quorum_set: &mut ScpQuorumSet) {
    let mut validators: Vec<_> = quorum_set.validators.iter().cloned().collect();
    validators.sort_by(node_id_cmp);
    quorum_set.validators = validators.try_into().unwrap_or_default();

    let mut inner_sets: Vec<_> = quorum_set.inner_sets.iter().cloned().collect();
    for inner_set in &mut inner_sets {
        normalize_quorum_set_reorder(inner_set);
    }

    inner_sets.sort_by(quorum_set_cmp);
    quorum_set.inner_sets = inner_sets.try_into().unwrap_or_default();
}

fn normalize_quorum_set_simplify(quorum_set: &mut ScpQuorumSet, id_to_remove: Option<&NodeId>) {
    let mut inner_sets: Vec<ScpQuorumSet> = quorum_set.inner_sets.iter().cloned().collect();
    let mut merged_validators: Vec<NodeId> = quorum_set.validators.iter().cloned().collect();

    // Remove the specified node from validators and adjust threshold
    if let Some(id) = id_to_remove {
        let original_len = merged_validators.len();
        merged_validators.retain(|n| n != id);
        let removed_count = original_len - merged_validators.len();
        quorum_set.threshold = quorum_set.threshold.saturating_sub(removed_count as u32);
    }

    let mut idx = 0;
    while idx < inner_sets.len() {
        normalize_quorum_set_simplify(&mut inner_sets[idx], id_to_remove);

        let is_singleton = inner_sets[idx].threshold == 1
            && inner_sets[idx].validators.len() == 1
            && inner_sets[idx].inner_sets.is_empty();
        if is_singleton {
            merged_validators.push(inner_sets[idx].validators[0].clone());
            inner_sets.remove(idx);
        } else {
            idx += 1;
        }
    }

    quorum_set.validators = merged_validators.try_into().unwrap_or_default();
    quorum_set.inner_sets = inner_sets.try_into().unwrap_or_default();

    if quorum_set.threshold == 1
        && quorum_set.validators.is_empty()
        && quorum_set.inner_sets.len() == 1
    {
        let inner = quorum_set.inner_sets[0].clone();
        *quorum_set = inner;
    }
}

fn node_id_bytes(node_id: &NodeId) -> [u8; 32] {
    match &node_id.0 {
        stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(stellar_xdr::curr::Uint256(bytes)) => {
            *bytes
        }
    }
}

fn node_id_cmp(a: &NodeId, b: &NodeId) -> std::cmp::Ordering {
    node_id_bytes(a).cmp(&node_id_bytes(b))
}

fn quorum_set_cmp(a: &ScpQuorumSet, b: &ScpQuorumSet) -> std::cmp::Ordering {
    let mut index = 0;
    let a_vals: Vec<NodeId> = a.validators.iter().cloned().collect();
    let b_vals: Vec<NodeId> = b.validators.iter().cloned().collect();
    while index < a_vals.len() && index < b_vals.len() {
        let ord = node_id_cmp(&a_vals[index], &b_vals[index]);
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
        index += 1;
    }
    if a_vals.len() != b_vals.len() {
        return a_vals.len().cmp(&b_vals.len());
    }

    let mut inner_index = 0;
    let a_inners: Vec<ScpQuorumSet> = a.inner_sets.iter().cloned().collect();
    let b_inners: Vec<ScpQuorumSet> = b.inner_sets.iter().cloned().collect();
    while inner_index < a_inners.len() && inner_index < b_inners.len() {
        let ord = quorum_set_cmp(&a_inners[inner_index], &b_inners[inner_index]);
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
        inner_index += 1;
    }
    if a_inners.len() != b_inners.len() {
        return a_inners.len().cmp(&b_inners.len());
    }

    a.threshold.cmp(&b.threshold)
}

/// Check if a quorum set is valid.
///
/// A quorum set is valid if:
/// 1. Threshold is <= number of validators + inner sets
/// 2. All inner sets are valid
pub fn is_valid_quorum_set(quorum_set: &ScpQuorumSet) -> bool {
    let total = quorum_set.validators.len() + quorum_set.inner_sets.len();
    let threshold = quorum_set.threshold as usize;

    if threshold > total {
        return false;
    }

    // Check inner sets
    for inner_set in quorum_set.inner_sets.iter() {
        if !is_valid_quorum_set(inner_set) {
            return false;
        }
    }

    true
}

/// Get all node IDs referenced in a quorum set.
pub fn get_all_nodes(quorum_set: &ScpQuorumSet) -> HashSet<NodeId> {
    let mut nodes = HashSet::new();
    collect_nodes(quorum_set, &mut nodes);
    nodes
}

fn collect_nodes(quorum_set: &ScpQuorumSet, nodes: &mut HashSet<NodeId>) {
    for validator in quorum_set.validators.iter() {
        nodes.insert(validator.clone());
    }

    for inner_set in quorum_set.inner_sets.iter() {
        collect_nodes(inner_set, nodes);
    }
}

/// Create a simple quorum set with just validators.
pub fn simple_quorum_set(threshold: u32, validators: Vec<NodeId>) -> ScpQuorumSet {
    ScpQuorumSet {
        threshold,
        validators: validators.try_into().unwrap_or_default(),
        inner_sets: Vec::new().try_into().unwrap_or_default(),
    }
}

/// Create a singleton quorum set containing just one node.
///
/// A singleton quorum set has threshold 1 and contains only the given node.
/// This is useful when a node needs to represent itself in quorum calculations.
pub fn singleton_quorum_set(node_id: NodeId) -> ScpQuorumSet {
    simple_quorum_set(1, vec![node_id])
}

/// Cache for singleton quorum sets to avoid repeated allocations.
///
/// This struct provides efficient caching of singleton quorum sets,
/// matching the stellar-core `getSingletonQSet()` optimization.
#[derive(Debug, Default)]
pub struct SingletonQuorumSetCache {
    cache: std::sync::RwLock<std::collections::HashMap<NodeId, ScpQuorumSet>>,
}

impl SingletonQuorumSetCache {
    /// Create a new empty cache.
    pub fn new() -> Self {
        Self {
            cache: std::sync::RwLock::new(std::collections::HashMap::new()),
        }
    }

    /// Get or create a singleton quorum set for the given node.
    ///
    /// If the quorum set is already cached, returns a clone.
    /// Otherwise, creates a new one and caches it.
    pub fn get_or_create(&self, node_id: &NodeId) -> ScpQuorumSet {
        // Try read first
        if let Ok(cache) = self.cache.read() {
            if let Some(qs) = cache.get(node_id) {
                return qs.clone();
            }
        }

        // Need to create and cache
        let qs = singleton_quorum_set(node_id.clone());
        if let Ok(mut cache) = self.cache.write() {
            cache.insert(node_id.clone(), qs.clone());
        }
        qs
    }

    /// Clear the cache.
    pub fn clear(&self) {
        if let Ok(mut cache) = self.cache.write() {
            cache.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{PublicKey, Uint256};

    fn make_node_id(seed: u8) -> NodeId {
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        NodeId(PublicKey::PublicKeyTypeEd25519(Uint256(bytes)))
    }

    fn make_simple_quorum_set(threshold: u32, node_ids: &[NodeId]) -> ScpQuorumSet {
        simple_quorum_set(threshold, node_ids.to_vec())
    }

    fn make_node_id_with_index(index: u16) -> NodeId {
        let mut bytes = [0u8; 32];
        bytes[0] = (index & 0xff) as u8;
        bytes[1] = (index >> 8) as u8;
        NodeId(PublicKey::PublicKeyTypeEd25519(Uint256(bytes)))
    }

    #[test]
    fn test_is_quorum_slice_simple() {
        let node1 = make_node_id(1);
        let node2 = make_node_id(2);
        let node3 = make_node_id(3);

        // 2-of-3 quorum set
        let qs = make_simple_quorum_set(2, &[node1.clone(), node2.clone(), node3.clone()]);

        let get_qs = |_: &NodeId| -> Option<ScpQuorumSet> { None };

        // 2 nodes should satisfy
        let mut nodes = HashSet::new();
        nodes.insert(node1.clone());
        nodes.insert(node2.clone());
        assert!(is_quorum_slice(&qs, &nodes, &get_qs));

        // 1 node should not satisfy
        let mut nodes = HashSet::new();
        nodes.insert(node1.clone());
        assert!(!is_quorum_slice(&qs, &nodes, &get_qs));

        // 3 nodes should satisfy
        let mut nodes = HashSet::new();
        nodes.insert(node1);
        nodes.insert(node2);
        nodes.insert(node3);
        assert!(is_quorum_slice(&qs, &nodes, &get_qs));
    }

    #[test]
    fn test_is_blocking_set() {
        let node1 = make_node_id(1);
        let node2 = make_node_id(2);
        let node3 = make_node_id(3);

        // 2-of-3 quorum set
        let qs = make_simple_quorum_set(2, &[node1.clone(), node2.clone(), node3.clone()]);

        // 2 nodes should be blocking (blocks all 2-of-3 combinations)
        let mut nodes = HashSet::new();
        nodes.insert(node1.clone());
        nodes.insert(node2.clone());
        assert!(is_blocking_set(&qs, &nodes));

        // 1 node should not be blocking
        let mut nodes = HashSet::new();
        nodes.insert(node1);
        assert!(!is_blocking_set(&qs, &nodes));
    }

    #[test]
    fn test_get_all_nodes() {
        let node1 = make_node_id(1);
        let node2 = make_node_id(2);

        let qs = make_simple_quorum_set(1, &[node1.clone(), node2.clone()]);
        let nodes = get_all_nodes(&qs);

        assert!(nodes.contains(&node1));
        assert!(nodes.contains(&node2));
        assert_eq!(nodes.len(), 2);
    }

    #[test]
    fn test_is_valid_quorum_set() {
        let node1 = make_node_id(1);
        let node2 = make_node_id(2);

        // Valid: 2-of-2
        let qs = make_simple_quorum_set(2, &[node1.clone(), node2.clone()]);
        assert!(is_valid_quorum_set(&qs));

        // Invalid: 3-of-2
        let qs = make_simple_quorum_set(3, &[node1, node2]);
        assert!(!is_valid_quorum_set(&qs));
    }

    #[test]
    fn test_is_quorum_set_sane_basic() {
        let node1 = make_node_id(1);
        let node2 = make_node_id(2);

        let qs = make_simple_quorum_set(1, &[node1.clone(), node2.clone()]);
        assert!(is_quorum_set_sane(&qs, false).is_ok());
    }

    #[test]
    fn test_is_quorum_set_sane_threshold_zero() {
        let mut qs = make_simple_quorum_set(1, &[]);
        qs.threshold = 0;
        assert!(is_quorum_set_sane(&qs, false).is_err());
    }

    #[test]
    fn test_is_quorum_set_sane_threshold_too_high() {
        let node1 = make_node_id(1);
        let mut qs = make_simple_quorum_set(1, &[node1]);
        qs.threshold = 2;
        assert!(is_quorum_set_sane(&qs, false).is_err());
    }

    #[test]
    fn test_is_quorum_set_sane_duplicate_nodes() {
        let node1 = make_node_id(1);
        let mut qs = make_simple_quorum_set(1, std::slice::from_ref(&node1));
        qs.validators = vec![node1.clone(), node1].try_into().unwrap_or_default();
        assert!(is_quorum_set_sane(&qs, false).is_err());
    }

    #[test]
    fn test_is_quorum_set_sane_max_depth() {
        let node1 = make_node_id(1);
        let mut qs = make_simple_quorum_set(1, std::slice::from_ref(&node1));
        for _ in 0..=MAXIMUM_QUORUM_NESTING_LEVEL {
            let inner = qs.clone();
            qs = ScpQuorumSet {
                threshold: 1,
                validators: Vec::new().try_into().unwrap_or_default(),
                inner_sets: vec![inner].try_into().unwrap_or_default(),
            };
        }
        assert!(is_quorum_set_sane(&qs, false).is_err());
    }

    #[test]
    fn test_is_quorum_set_sane_node_count_limit() {
        let mut validators = Vec::new();
        for idx in 0..=MAXIMUM_QUORUM_NODES {
            validators.push(make_node_id_with_index(idx as u16));
        }
        let qs = simple_quorum_set(1, validators);
        assert!(is_quorum_set_sane(&qs, false).is_err());
    }

    #[test]
    fn test_is_quorum_set_sane_node_count_limit_with_inner_sets() {
        let mut nodes = Vec::new();
        for idx in 0..=MAXIMUM_QUORUM_NODES {
            nodes.push(make_node_id_with_index(idx as u16));
        }

        let mut qs = ScpQuorumSet {
            threshold: 1,
            validators: vec![nodes[0].clone()].try_into().unwrap_or_default(),
            inner_sets: Vec::new().try_into().unwrap_or_default(),
        };

        let mut inners = Vec::new();
        for set_index in 0..10 {
            let start = 1 + set_index * 100;
            let end = start + 100;
            let slice: Vec<NodeId> = nodes[start..end].iter().cloned().collect();
            inners.push(simple_quorum_set(1, slice));
        }
        qs.inner_sets = inners.try_into().unwrap_or_default();

        assert!(is_quorum_set_sane(&qs, false).is_err());
    }

    #[test]
    fn test_is_quorum_set_sane_extra_checks() {
        let node1 = make_node_id(1);
        let node2 = make_node_id(2);

        let qs = make_simple_quorum_set(1, &[node1, node2]);
        assert!(is_quorum_set_sane(&qs, true).is_err());
    }

    #[test]
    fn test_is_quorum_set_sane_extra_checks_threshold_ok() {
        let node1 = make_node_id(1);
        let node2 = make_node_id(2);
        let node3 = make_node_id(3);

        let qs = make_simple_quorum_set(2, &[node1, node2, node3]);
        assert!(is_quorum_set_sane(&qs, true).is_ok());
    }

    #[test]
    fn test_normalize_quorum_set_merges_singletons() {
        let node0 = make_node_id(0);
        let node1 = make_node_id(1);

        let mut qs = ScpQuorumSet {
            threshold: 1,
            validators: vec![node0.clone()].try_into().unwrap_or_default(),
            inner_sets: vec![make_simple_quorum_set(1, std::slice::from_ref(&node1))]
                .try_into()
                .unwrap_or_default(),
        };

        normalize_quorum_set(&mut qs);

        assert_eq!(qs.threshold, 1);
        let validators: Vec<NodeId> = qs.validators.iter().cloned().collect();
        assert_eq!(validators, vec![node0, node1]);
        assert!(qs.inner_sets.is_empty());
    }

    #[test]
    fn test_normalize_quorum_set_flattens_nested_singletons() {
        let node0 = make_node_id(0);
        let node1 = make_node_id(1);
        let node2 = make_node_id(2);
        let node3 = make_node_id(3);

        let mut qs = ScpQuorumSet {
            threshold: 1,
            validators: Vec::new().try_into().unwrap_or_default(),
            inner_sets: vec![ScpQuorumSet {
                threshold: 1,
                validators: vec![node0.clone()].try_into().unwrap_or_default(),
                inner_sets: vec![
                    make_simple_quorum_set(1, std::slice::from_ref(&node1)),
                    make_simple_quorum_set(1, &[node2.clone(), node3.clone()]),
                ]
                .try_into()
                .unwrap_or_default(),
            }]
            .try_into()
            .unwrap_or_default(),
        };

        normalize_quorum_set(&mut qs);

        assert_eq!(qs.threshold, 1);
        let validators: Vec<NodeId> = qs.validators.iter().cloned().collect();
        assert_eq!(validators, vec![node0, node1]);
        assert_eq!(qs.inner_sets.len(), 1);
        let inner = &qs.inner_sets[0];
        assert_eq!(inner.threshold, 1);
        let inner_validators: Vec<NodeId> = inner.validators.iter().cloned().collect();
        assert_eq!(inner_validators, vec![node2, node3]);
    }

    #[test]
    fn test_normalize_quorum_set_promotes_single_inner() {
        let node0 = make_node_id(0);
        let mut qs = ScpQuorumSet {
            threshold: 1,
            validators: Vec::new().try_into().unwrap_or_default(),
            inner_sets: vec![make_simple_quorum_set(1, std::slice::from_ref(&node0))]
                .try_into()
                .unwrap_or_default(),
        };

        normalize_quorum_set(&mut qs);

        assert_eq!(qs.threshold, 1);
        let validators: Vec<NodeId> = qs.validators.iter().cloned().collect();
        assert_eq!(validators, vec![node0]);
        assert!(qs.inner_sets.is_empty());
    }

    #[test]
    fn test_normalize_quorum_set_sorts_validators() {
        let node0 = make_node_id(0);
        let node1 = make_node_id(1);
        let node2 = make_node_id(2);

        let mut qs = ScpQuorumSet {
            threshold: 2,
            validators: vec![node2.clone(), node0.clone(), node1.clone()]
                .try_into()
                .unwrap_or_default(),
            inner_sets: Vec::new().try_into().unwrap_or_default(),
        };

        normalize_quorum_set(&mut qs);

        let validators: Vec<NodeId> = qs.validators.iter().cloned().collect();
        assert_eq!(validators, vec![node0, node1, node2]);
    }

    #[test]
    fn test_normalize_quorum_set_sorts_inner_sets() {
        let node0 = make_node_id(0);
        let node1 = make_node_id(1);
        let node2 = make_node_id(2);
        let node3 = make_node_id(3);

        let inner_a = ScpQuorumSet {
            threshold: 2,
            validators: vec![node2.clone(), node3.clone()]
                .try_into()
                .unwrap_or_default(),
            inner_sets: Vec::new().try_into().unwrap_or_default(),
        };
        let inner_b = ScpQuorumSet {
            threshold: 2,
            validators: vec![node0.clone(), node1.clone()]
                .try_into()
                .unwrap_or_default(),
            inner_sets: Vec::new().try_into().unwrap_or_default(),
        };

        let mut qs = ScpQuorumSet {
            threshold: 2,
            validators: Vec::new().try_into().unwrap_or_default(),
            inner_sets: vec![inner_a, inner_b].try_into().unwrap_or_default(),
        };

        normalize_quorum_set(&mut qs);

        assert_eq!(qs.inner_sets.len(), 2);
        let first_validators: Vec<NodeId> = qs.inner_sets[0].validators.iter().cloned().collect();
        let second_validators: Vec<NodeId> = qs.inner_sets[1].validators.iter().cloned().collect();
        assert_eq!(first_validators, vec![node0, node1]);
        assert_eq!(second_validators, vec![node2, node3]);
    }

    #[test]
    fn test_vblocking_and_quorum() {
        let node0 = make_node_id(0);
        let node1 = make_node_id(1);
        let node2 = make_node_id(2);
        let node3 = make_node_id(3);

        let qs = make_simple_quorum_set(
            3,
            &[node0.clone(), node1.clone(), node2.clone(), node3.clone()],
        );

        let mut nodes = HashSet::new();
        nodes.insert(node0.clone());
        assert!(!is_quorum_slice(&qs, &nodes, &|_: &NodeId| None));
        assert!(!is_v_blocking(&qs, &nodes));

        nodes.insert(node2.clone());
        assert!(!is_quorum_slice(&qs, &nodes, &|_: &NodeId| None));
        assert!(is_v_blocking(&qs, &nodes));

        nodes.insert(node3.clone());
        assert!(is_quorum_slice(&qs, &nodes, &|_: &NodeId| None));
        assert!(is_v_blocking(&qs, &nodes));

        nodes.insert(node1.clone());
        assert!(is_quorum_slice(&qs, &nodes, &|_: &NodeId| None));
        assert!(is_v_blocking(&qs, &nodes));
    }

    #[test]
    fn test_find_closest_vblocking_distance() {
        let node0 = make_node_id(0);
        let node1 = make_node_id(1);
        let node2 = make_node_id(2);
        let node3 = make_node_id(3);
        let node4 = make_node_id(4);
        let node5 = make_node_id(5);
        let node6 = make_node_id(6);
        let node7 = make_node_id(7);

        let mut qs = make_simple_quorum_set(2, &[node0.clone(), node1.clone(), node2.clone()]);

        let mut good = HashSet::new();
        good.insert(node0.clone());

        let check = |q: &ScpQuorumSet, s: &HashSet<NodeId>, expected: usize| {
            let result = find_closest_v_blocking(q, s, None);
            assert_eq!(result.len(), expected);
        };

        check(&qs, &good, 0);

        good.insert(node1.clone());
        check(&qs, &good, 1);

        good.insert(node2.clone());
        check(&qs, &good, 2);

        let qsub1 = make_simple_quorum_set(1, &[node3.clone(), node4.clone(), node5.clone()]);
        qs.inner_sets = vec![qsub1].try_into().unwrap_or_default();

        good.insert(node3.clone());
        check(&qs, &good, 3);

        good.insert(node4.clone());
        check(&qs, &good, 3);

        qs.threshold = 1;
        check(&qs, &good, 5);

        good.insert(node5.clone());
        check(&qs, &good, 6);

        let qsub2 = make_simple_quorum_set(2, &[node6.clone(), node7.clone()]);
        let mut inner_sets: Vec<ScpQuorumSet> = qs.inner_sets.iter().cloned().collect();
        inner_sets.push(qsub2);
        qs.inner_sets = inner_sets.try_into().unwrap_or_default();

        check(&qs, &good, 6);

        good.insert(node6.clone());
        check(&qs, &good, 6);

        good.insert(node7.clone());
        check(&qs, &good, 7);

        qs.threshold = 4;
        check(&qs, &good, 2);

        qs.threshold = 3;
        check(&qs, &good, 3);

        qs.threshold = 2;
        check(&qs, &good, 4);
    }

    #[test]
    fn test_find_closest_vblocking_with_excluded() {
        let node0 = make_node_id(0);
        let node1 = make_node_id(1);
        let node2 = make_node_id(2);

        let qs = make_simple_quorum_set(2, &[node0.clone(), node1.clone(), node2.clone()]);
        let mut nodes = HashSet::new();
        nodes.insert(node0.clone());
        nodes.insert(node1.clone());

        let without_excluded = find_closest_v_blocking(&qs, &nodes, None);
        let with_excluded = find_closest_v_blocking(&qs, &nodes, Some(&node1));

        assert_eq!(without_excluded.len(), 1);
        assert_eq!(with_excluded.len(), 1);
        assert!(!with_excluded.contains(&node1));
    }

    // ==================== Tests for new parity features ====================

    #[test]
    fn test_singleton_quorum_set() {
        let node = make_node_id(42);
        let qs = singleton_quorum_set(node.clone());

        assert_eq!(qs.threshold, 1);
        assert_eq!(qs.validators.len(), 1);
        assert_eq!(&qs.validators[0], &node);
        assert!(qs.inner_sets.is_empty());
    }

    #[test]
    fn test_singleton_quorum_set_cache() {
        let cache = SingletonQuorumSetCache::new();
        let node1 = make_node_id(1);
        let node2 = make_node_id(2);

        // First access creates the quorum set
        let qs1a = cache.get_or_create(&node1);
        assert_eq!(qs1a.threshold, 1);
        assert_eq!(qs1a.validators.len(), 1);
        assert_eq!(&qs1a.validators[0], &node1);

        // Second access returns cached version
        let qs1b = cache.get_or_create(&node1);
        assert_eq!(qs1a.threshold, qs1b.threshold);
        assert_eq!(qs1a.validators.len(), qs1b.validators.len());

        // Different node gets different quorum set
        let qs2 = cache.get_or_create(&node2);
        assert_eq!(&qs2.validators[0], &node2);

        // Clear removes all cached entries
        cache.clear();

        // After clear, still creates correctly
        let qs1c = cache.get_or_create(&node1);
        assert_eq!(&qs1c.validators[0], &node1);
    }

    #[test]
    fn test_singleton_quorum_set_cache_thread_safe() {
        use std::sync::Arc;
        use std::thread;

        let cache = Arc::new(SingletonQuorumSetCache::new());
        let mut handles = vec![];

        // Spawn multiple threads accessing the cache concurrently
        for i in 0..10 {
            let cache_clone = Arc::clone(&cache);
            handles.push(thread::spawn(move || {
                let node = make_node_id(i);
                for _ in 0..100 {
                    let qs = cache_clone.get_or_create(&node);
                    assert_eq!(qs.threshold, 1);
                    assert_eq!(qs.validators.len(), 1);
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }
    }

    // ==================== Additional stellar-core parity tests ====================

    #[test]
    fn test_is_quorum_with_nested_sets() {
        let node0 = make_node_id(0);
        let node1 = make_node_id(1);
        let node2 = make_node_id(2);
        let node3 = make_node_id(3);

        // Create a quorum set with an inner set:
        // threshold=2, validators=[node0, node1], inner_sets=[{threshold=1, validators=[node2, node3]}]
        let inner = make_simple_quorum_set(1, &[node2.clone(), node3.clone()]);
        let qs = ScpQuorumSet {
            threshold: 2,
            validators: vec![node0.clone(), node1.clone()].try_into().unwrap(),
            inner_sets: vec![inner].try_into().unwrap(),
        };

        let get_qs = |_: &NodeId| -> Option<ScpQuorumSet> { None };

        // node0 + node1 = 2 validators, satisfies threshold
        let mut nodes = HashSet::new();
        nodes.insert(node0.clone());
        nodes.insert(node1.clone());
        assert!(is_quorum_slice(&qs, &nodes, &get_qs));

        // node0 + inner set satisfied = 2, satisfies threshold
        let mut nodes = HashSet::new();
        nodes.insert(node0.clone());
        nodes.insert(node2.clone()); // inner set is satisfied (1-of-2)
        assert!(is_quorum_slice(&qs, &nodes, &get_qs));

        // Only inner set satisfied = 1, doesn't satisfy threshold
        let mut nodes = HashSet::new();
        nodes.insert(node2.clone());
        nodes.insert(node3.clone());
        assert!(!is_quorum_slice(&qs, &nodes, &get_qs));
    }

    #[test]
    fn test_is_quorum_full() {
        let node0 = make_node_id(0);
        let node1 = make_node_id(1);
        let node2 = make_node_id(2);

        // All nodes have the same 2-of-3 quorum set
        let qs = make_simple_quorum_set(2, &[node0.clone(), node1.clone(), node2.clone()]);

        let get_qs = |_: &NodeId| -> Option<ScpQuorumSet> { Some(qs.clone()) };

        // 2 nodes form a quorum (each has their slice satisfied by the set)
        let mut nodes = HashSet::new();
        nodes.insert(node0.clone());
        nodes.insert(node1.clone());
        assert!(is_quorum(&qs, &nodes, &get_qs));

        // 1 node does not form a quorum
        let mut nodes = HashSet::new();
        nodes.insert(node0.clone());
        assert!(!is_quorum(&qs, &nodes, &get_qs));

        // All 3 nodes form a quorum
        let mut nodes = HashSet::new();
        nodes.insert(node0);
        nodes.insert(node1);
        nodes.insert(node2);
        assert!(is_quorum(&qs, &nodes, &get_qs));
    }

    #[test]
    fn test_is_quorum_asymmetric() {
        let node0 = make_node_id(0);
        let node1 = make_node_id(1);
        let node2 = make_node_id(2);

        // node0 trusts node1 and node2 (2-of-2)
        let qs0 = make_simple_quorum_set(2, &[node1.clone(), node2.clone()]);
        // node1 trusts node0 and node2 (2-of-2)
        let qs1 = make_simple_quorum_set(2, &[node0.clone(), node2.clone()]);
        // node2 trusts node0 and node1 (2-of-2)
        let qs2 = make_simple_quorum_set(2, &[node0.clone(), node1.clone()]);

        let get_qs = |n: &NodeId| -> Option<ScpQuorumSet> {
            if n == &node0 {
                Some(qs0.clone())
            } else if n == &node1 {
                Some(qs1.clone())
            } else if n == &node2 {
                Some(qs2.clone())
            } else {
                None
            }
        };

        // {node0, node1, node2} forms a quorum
        let mut nodes = HashSet::new();
        nodes.insert(node0.clone());
        nodes.insert(node1.clone());
        nodes.insert(node2.clone());
        assert!(is_quorum(&qs0, &nodes, &get_qs));

        // {node0, node1} doesn't form a quorum (node0's slice requires node2)
        let mut nodes = HashSet::new();
        nodes.insert(node0.clone());
        nodes.insert(node1.clone());
        assert!(!is_quorum(&qs0, &nodes, &get_qs));
    }

    #[test]
    fn test_blocking_set_with_nested() {
        let node0 = make_node_id(0);
        let node1 = make_node_id(1);
        let node2 = make_node_id(2);
        let node3 = make_node_id(3);

        // threshold=3, validators=[node0, node1], inner_sets=[{threshold=1, [node2, node3]}]
        // Total members = 3, need 3 to pass, so blocking threshold = 3-3+1 = 1
        let inner = make_simple_quorum_set(1, &[node2.clone(), node3.clone()]);
        let qs = ScpQuorumSet {
            threshold: 3,
            validators: vec![node0.clone(), node1.clone()].try_into().unwrap(),
            inner_sets: vec![inner].try_into().unwrap(),
        };

        // Any single validator blocks
        let mut nodes = HashSet::new();
        nodes.insert(node0.clone());
        assert!(is_blocking_set(&qs, &nodes));

        // If inner set is blocked, it blocks the outer
        let mut nodes = HashSet::new();
        nodes.insert(node2.clone());
        nodes.insert(node3.clone());
        assert!(is_blocking_set(&qs, &nodes));
    }

    #[test]
    fn test_hash_quorum_set_deterministic() {
        let node0 = make_node_id(0);
        let node1 = make_node_id(1);

        let qs1 = make_simple_quorum_set(1, &[node0.clone(), node1.clone()]);
        let qs2 = make_simple_quorum_set(1, &[node0.clone(), node1.clone()]);

        // Same quorum sets should have same hash
        assert_eq!(hash_quorum_set(&qs1), hash_quorum_set(&qs2));

        // Different threshold should have different hash
        let qs3 = make_simple_quorum_set(2, &[node0.clone(), node1.clone()]);
        assert_ne!(hash_quorum_set(&qs1), hash_quorum_set(&qs3));
    }

    #[test]
    fn test_get_all_nodes_with_nested() {
        let node0 = make_node_id(0);
        let node1 = make_node_id(1);
        let node2 = make_node_id(2);
        let node3 = make_node_id(3);

        let inner = make_simple_quorum_set(1, &[node2.clone(), node3.clone()]);
        let qs = ScpQuorumSet {
            threshold: 2,
            validators: vec![node0.clone(), node1.clone()].try_into().unwrap(),
            inner_sets: vec![inner].try_into().unwrap(),
        };

        let all_nodes = get_all_nodes(&qs);
        assert_eq!(all_nodes.len(), 4);
        assert!(all_nodes.contains(&node0));
        assert!(all_nodes.contains(&node1));
        assert!(all_nodes.contains(&node2));
        assert!(all_nodes.contains(&node3));
    }

    #[test]
    fn test_normalize_preserves_semantics() {
        let node0 = make_node_id(0);
        let node1 = make_node_id(1);
        let node2 = make_node_id(2);

        // Create an unnormalized quorum set (unsorted)
        let mut qs = ScpQuorumSet {
            threshold: 2,
            validators: vec![node2.clone(), node0.clone(), node1.clone()]
                .try_into()
                .unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };

        let _hash_before = hash_quorum_set(&qs);
        normalize_quorum_set(&mut qs);
        let _hash_after = hash_quorum_set(&qs);

        // Hash may change due to ordering, but semantics preserved
        // Validators should now be sorted
        let validators: Vec<_> = qs.validators.iter().cloned().collect();
        assert_eq!(validators[0], node0);
        assert_eq!(validators[1], node1);
        assert_eq!(validators[2], node2);

        // But both should function the same way
        let mut nodes = HashSet::new();
        nodes.insert(node0.clone());
        nodes.insert(node1.clone());
        assert!(is_quorum_slice(&qs, &nodes, &|_: &NodeId| None));
    }

    // ==================== Q1: Iterative pruning quorum tests ====================

    #[test]
    fn test_is_quorum_iterative_pruning() {
        // Test that is_quorum uses iterative pruning to find quorums
        // within the input set, matching stellar-core behavior.
        let node0 = make_node_id(0);
        let node1 = make_node_id(1);
        let node2 = make_node_id(2);
        let node3 = make_node_id(3); // node3 has no quorum set (unknown)

        // All known nodes have the same 2-of-3 quorum set
        let qs = make_simple_quorum_set(2, &[node0.clone(), node1.clone(), node2.clone()]);

        let get_qs = |n: &NodeId| -> Option<ScpQuorumSet> {
            if n == &node0 || n == &node1 || n == &node2 {
                Some(qs.clone())
            } else {
                None // node3 is unknown
            }
        };

        // {node0, node1, node3} - node3 is unknown, but after pruning node3,
        // {node0, node1} still forms a quorum. The iterative algorithm should find it.
        let mut nodes = HashSet::new();
        nodes.insert(node0.clone());
        nodes.insert(node1.clone());
        nodes.insert(node3.clone());
        assert!(is_quorum(&qs, &nodes, &get_qs));
    }

    #[test]
    fn test_is_quorum_iterative_pruning_cascade() {
        // Test cascading pruning: removing one node makes another's slice unsatisfied
        let node0 = make_node_id(0);
        let node1 = make_node_id(1);
        let node2 = make_node_id(2);
        let node3 = make_node_id(3);

        // node0 and node1 have 2-of-3 (node0, node1, node2) - they trust each other
        let qs01 = make_simple_quorum_set(2, &[node0.clone(), node1.clone(), node2.clone()]);

        // node2 requires all three nodes (3-of-3) - very strict
        let qs2 = make_simple_quorum_set(3, &[node0.clone(), node1.clone(), node2.clone()]);

        // node3 requires node2 and node3 (unknown quorum set)
        let get_qs = |n: &NodeId| -> Option<ScpQuorumSet> {
            if n == &node0 || n == &node1 {
                Some(qs01.clone())
            } else if n == &node2 {
                Some(qs2.clone())
            } else {
                None
            }
        };

        // {node0, node1, node2, node3}: node3 gets pruned (unknown), then node2's
        // slice (3-of-3 requiring node3) fails, so node2 gets pruned too.
        // Remaining {node0, node1} satisfies the local 2-of-3 slice.
        let mut nodes = HashSet::new();
        nodes.insert(node0.clone());
        nodes.insert(node1.clone());
        nodes.insert(node2.clone());
        nodes.insert(node3.clone());
        assert!(is_quorum(&qs01, &nodes, &get_qs));
    }

    // ==================== Q2: normalize_quorum_set_with_remove tests ====================

    #[test]
    fn test_normalize_quorum_set_with_remove_basic() {
        let node0 = make_node_id(0);
        let node1 = make_node_id(1);
        let node2 = make_node_id(2);

        // 2-of-3 quorum set: removing node1 should give 1-of-2
        let mut qs = make_simple_quorum_set(2, &[node0.clone(), node1.clone(), node2.clone()]);
        normalize_quorum_set_with_remove(&mut qs, Some(&node1));

        assert_eq!(qs.threshold, 1);
        let validators: Vec<NodeId> = qs.validators.iter().cloned().collect();
        assert_eq!(validators.len(), 2);
        assert!(validators.contains(&node0));
        assert!(validators.contains(&node2));
        assert!(!validators.contains(&node1));
    }

    #[test]
    fn test_normalize_quorum_set_with_remove_inner_set() {
        let node0 = make_node_id(0);
        let node1 = make_node_id(1);
        let node2 = make_node_id(2);
        let node3 = make_node_id(3);

        // Outer: threshold=2, validators=[node0, node1]
        // Inner: threshold=2, validators=[node1, node2, node3]
        // Removing node1 should:
        //   - Outer: threshold=1, validators=[node0]
        //   - Inner: threshold=1, validators=[node2, node3]
        let inner = make_simple_quorum_set(2, &[node1.clone(), node2.clone(), node3.clone()]);
        let mut qs = ScpQuorumSet {
            threshold: 2,
            validators: vec![node0.clone(), node1.clone()].try_into().unwrap(),
            inner_sets: vec![inner].try_into().unwrap(),
        };

        normalize_quorum_set_with_remove(&mut qs, Some(&node1));

        assert_eq!(qs.threshold, 1);
        let validators: Vec<NodeId> = qs.validators.iter().cloned().collect();
        assert!(!validators.contains(&node1));
        assert!(validators.contains(&node0));

        // Inner set should also have node1 removed
        assert_eq!(qs.inner_sets.len(), 1);
        let inner = &qs.inner_sets[0];
        assert_eq!(inner.threshold, 1);
        let inner_validators: Vec<NodeId> = inner.validators.iter().cloned().collect();
        assert!(!inner_validators.contains(&node1));
        assert!(inner_validators.contains(&node2));
        assert!(inner_validators.contains(&node3));
    }

    #[test]
    fn test_normalize_quorum_set_with_remove_none() {
        // Passing None should behave identically to normalize_quorum_set
        let node0 = make_node_id(0);
        let node1 = make_node_id(1);
        let node2 = make_node_id(2);

        let mut qs1 = ScpQuorumSet {
            threshold: 2,
            validators: vec![node2.clone(), node0.clone(), node1.clone()]
                .try_into()
                .unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        let mut qs2 = qs1.clone();

        normalize_quorum_set(&mut qs1);
        normalize_quorum_set_with_remove(&mut qs2, None);

        assert_eq!(hash_quorum_set(&qs1), hash_quorum_set(&qs2));
    }

    // ==================== Q3: is_v_blocking threshold==0 tests ====================

    #[test]
    fn test_is_v_blocking_empty_quorum_set() {
        // Empty quorum set (threshold=0) should NOT be v-blocking
        // (matches stellar-core "There is no v-blocking set for {\empty}")
        let qs = ScpQuorumSet {
            threshold: 0,
            validators: vec![].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };

        let nodes = HashSet::new();
        assert!(!is_v_blocking(&qs, &nodes));

        // Even with some nodes, threshold=0 means no v-blocking
        let mut nodes = HashSet::new();
        nodes.insert(make_node_id(1));
        assert!(!is_v_blocking(&qs, &nodes));
    }
}
