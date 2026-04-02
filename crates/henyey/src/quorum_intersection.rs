//! Quorum intersection analysis for Stellar Consensus Protocol (SCP).
//!
//! This module provides functionality to verify that a network of SCP nodes
//! enjoys quorum intersection - a critical safety property that ensures all
//! quorums in the network share at least one common node.
//!
//! # Background
//!
//! In SCP, a quorum is a set of nodes where each node's quorum slice requirements
//! are satisfied. For the network to maintain consensus safety, any two quorums
//! must overlap (share at least one node). If this property doesn't hold,
//! different parts of the network could agree on conflicting values.
//!
//! # Usage
//!
//! The main entry point is [`check_quorum_intersection_from_json`], which loads
//! a network configuration from a JSON file and verifies the intersection property.
//!
//! ```text
//! // Example JSON format:
//! {
//!     "nodes": [
//!         {
//!             "node": "GDKXE2OZM...",  // Public key in strkey format
//!             "qset": {
//!                 "t": 2,              // Threshold
//!                 "v": ["GCEZWKCA5...", "GBLJNN7HG..."]  // Validators
//!             }
//!         }
//!     ]
//! }
//! ```
//!
//! # Algorithm Complexity
//!
//! The algorithm enumerates all possible subsets of nodes (2^n) to find quorums,
//! then checks all pairs for intersection. This is exponential in the number of
//! nodes and is only practical for small networks (roughly < 20 nodes).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use henyey_scp::quorum_config::parse_node_id;
use henyey_scp::{is_quorum, is_quorum_slice};
use serde::Deserialize;
use stellar_xdr::curr::{NodeId, ScpQuorumSet};

type QuorumMap = HashMap<NodeId, ScpQuorumSet>;

/// JSON representation of the network configuration for quorum intersection analysis.
#[derive(Debug, Deserialize)]
struct QuorumIntersectionJson {
    /// List of nodes with their quorum set configurations.
    nodes: Vec<NodeEntry>,
}

/// A single node entry from the JSON configuration.
#[derive(Debug, Deserialize)]
struct NodeEntry {
    /// Node public key in strkey format (e.g., "GDKXE2OZM...").
    node: String,
    /// The node's quorum set configuration.
    qset: QsetEntry,
}

/// Quorum set configuration from JSON.
#[derive(Debug, Deserialize)]
struct QsetEntry {
    /// Threshold - minimum number of validators that must agree.
    t: u32,
    /// List of validator public keys in strkey format.
    v: Vec<String>,
}

/// Parses a JSON quorum set entry into an SCP quorum set structure.
///
/// Converts validator public keys from strkey format to `NodeId` and
/// constructs the corresponding `ScpQuorumSet`.
fn parse_qset(entry: &QsetEntry) -> anyhow::Result<ScpQuorumSet> {
    let mut validators = Vec::with_capacity(entry.v.len());
    for node in &entry.v {
        let parsed = parse_node_id(node).map_err(|e| anyhow::anyhow!(e.to_string()))?;
        validators.push(parsed);
    }

    Ok(ScpQuorumSet {
        threshold: entry.t,
        validators: validators.try_into().unwrap_or_default(),
        inner_sets: Vec::new().try_into().unwrap_or_default(),
    })
}

/// Loads a quorum map from a JSON file.
///
/// Reads the JSON file at the given path and constructs a mapping from
/// node IDs to their quorum sets.
///
/// # Errors
///
/// Returns an error if:
/// - The file cannot be read
/// - The JSON is malformed
/// - Any node ID or quorum set is invalid
fn load_quorum_map(path: &Path) -> anyhow::Result<QuorumMap> {
    let payload = fs::read_to_string(path)?;
    let json: QuorumIntersectionJson =
        serde_json::from_str(&payload).map_err(|e| anyhow::anyhow!("parse error: {}", e))?;

    let mut map = HashMap::new();
    for entry in json.nodes {
        let node_id = parse_node_id(&entry.node).map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let qset = parse_qset(&entry.qset)?;
        map.insert(node_id, qset);
    }

    Ok(map)
}

/// Checks if a set of nodes forms a valid quorum.
///
/// A set of nodes is a quorum if every node in the set has its quorum slice
/// requirements satisfied by the set. This function picks an arbitrary node
/// from the set and uses SCP's `is_quorum` check.
fn is_quorum_for_set(nodes: &HashSet<NodeId>, qmap: &QuorumMap) -> bool {
    let Some(first) = nodes.iter().next() else {
        return false;
    };
    let Some(local_qset) = qmap.get(first) else {
        return false;
    };

    is_quorum(local_qset, nodes, |node_id| qmap.get(node_id).cloned())
}

/// Maximum number of nodes supported for quorum intersection analysis.
///
/// The algorithm is O(2^n * n^2), so we cap at 20 nodes to prevent
/// runaway computation (2^20 ≈ 1 million subsets).
const MAX_QUORUM_INTERSECTION_NODES: usize = 20;

/// Checks if the network enjoys quorum intersection.
///
/// Enumerates all possible node subsets, identifies which ones form valid
/// quorums, and then verifies that every pair of quorums shares at least
/// one common node.
///
/// # Algorithm
///
/// 1. Generate all 2^n - 1 non-empty subsets of nodes
/// 2. For each subset, check if it forms a valid quorum
/// 3. For all pairs of quorums, verify they are not disjoint
///
/// # Returns
///
/// `true` if all quorum pairs intersect, `false` otherwise.
///
/// # Panics
///
/// Panics if the network has more than [`MAX_QUORUM_INTERSECTION_NODES`] nodes.
fn network_enjoys_quorum_intersection(qmap: &QuorumMap) -> bool {
    let nodes: Vec<NodeId> = qmap.keys().cloned().collect();
    if nodes.is_empty() {
        return false;
    }

    assert!(
        nodes.len() <= MAX_QUORUM_INTERSECTION_NODES,
        "Quorum intersection analysis only supports up to {} nodes, got {}",
        MAX_QUORUM_INTERSECTION_NODES,
        nodes.len()
    );

    // Enumerate all possible quorums by checking every subset
    let mut quorums: Vec<HashSet<NodeId>> = Vec::new();
    let total = nodes.len();
    for mask in 1..(1u64 << total) {
        let mut subset = HashSet::new();
        for (idx, node) in nodes.iter().enumerate() {
            if (mask >> idx) & 1 == 1 {
                subset.insert(node.clone());
            }
        }
        if is_quorum_for_set(&subset, qmap) {
            quorums.push(subset);
        }
    }

    // Check all pairs of quorums for intersection
    for i in 0..quorums.len() {
        for j in (i + 1)..quorums.len() {
            if quorums[i].is_disjoint(&quorums[j]) {
                return false;
            }
        }
    }

    true
}

/// Loads a quorum configuration from JSON and checks for quorum intersection.
///
/// This is the main entry point for quorum intersection analysis. It:
/// 1. Loads the network configuration from the JSON file
/// 2. Verifies each node has a satisfiable quorum slice in the network
/// 3. Checks that all quorums in the network intersect
///
/// # Arguments
///
/// * `path` - Path to the JSON configuration file
///
/// # Returns
///
/// * `Ok(true)` - Network enjoys quorum intersection (safe)
/// * `Ok(false)` - Network does NOT enjoy quorum intersection (unsafe!)
/// * `Err(_)` - Configuration error or unsatisfiable quorum slice
///
/// # Errors
///
/// Returns an error if any node's quorum set cannot be satisfied by the
/// network (i.e., the node would be stuck and unable to reach consensus).
pub fn check_quorum_intersection_from_json(path: &Path) -> anyhow::Result<bool> {
    let qmap = load_quorum_map(path)?;
    let nodes: HashSet<NodeId> = qmap.keys().cloned().collect();
    for (node, qset) in &qmap {
        if !is_quorum_slice(qset, &nodes, &|id| qmap.get(id).cloned()) {
            anyhow::bail!(
                "quorum set for {} has no slice in network",
                node_id_to_hex(node)
            );
        }
    }

    Ok(network_enjoys_quorum_intersection(&qmap))
}

/// Converts a node ID to its hexadecimal representation for display.
fn node_id_to_hex(node: &NodeId) -> String {
    use stellar_xdr::curr::PublicKey;
    match node.0 {
        PublicKey::PublicKeyTypeEd25519(ref key) => hex::encode(key.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn testdata_path(name: &str) -> PathBuf {
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("..");
        path.push("..");
        path.push("testdata");
        path.push("check-quorum-intersection-json");
        path.push(name);
        path
    }

    #[test]
    fn test_enjoys_quorum_intersection() {
        let path = testdata_path("enjoys-intersection.json");
        let enjoys = check_quorum_intersection_from_json(&path).expect("check quorum intersection");
        assert!(enjoys);
    }

    #[test]
    fn test_no_quorum_intersection() {
        let path = testdata_path("no-intersection.json");
        let enjoys = check_quorum_intersection_from_json(&path).expect("check quorum intersection");
        assert!(!enjoys);
    }

    #[test]
    fn test_bad_key() {
        let path = testdata_path("bad-key.json");
        let err = check_quorum_intersection_from_json(&path).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("invalid public key") || msg.contains("Invalid public key"),
            "{msg}"
        );
    }

    #[test]
    fn test_bad_threshold_type() {
        let path = testdata_path("bad-threshold-type.json");
        let err = check_quorum_intersection_from_json(&path).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("parse"), "{msg}");
    }

    #[test]
    fn test_missing_file() {
        let path = testdata_path("no-file.json");
        let err = check_quorum_intersection_from_json(&path).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("No such file") || msg.contains("read"),
            "{msg}"
        );
    }
}
