//! CLI entry point for quorum intersection analysis.
//!
//! Loads a network configuration from a JSON file and delegates to the
//! shared `henyey_scp::quorum_intersection` library for the actual analysis.
//!
//! # Usage
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

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use henyey_scp::quorum_config::parse_node_id;
use henyey_scp::quorum_intersection::{
    check_intersection, find_unsatisfiable_node, IntersectionResult,
};
use serde::Deserialize;
use stellar_xdr::curr::{NodeId, ScpQuorumSet};

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
fn load_quorum_map(path: &Path) -> anyhow::Result<HashMap<NodeId, Option<ScpQuorumSet>>> {
    let payload = fs::read_to_string(path)?;
    let json: QuorumIntersectionJson =
        serde_json::from_str(&payload).map_err(|e| anyhow::anyhow!("parse error: {}", e))?;

    let mut map = HashMap::new();
    for entry in json.nodes {
        let node_id = parse_node_id(&entry.node).map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let qset = parse_qset(&entry.qset)?;
        map.insert(node_id, Some(qset));
    }

    Ok(map)
}

/// Loads a quorum configuration from JSON and checks for quorum intersection.
///
/// This is the main entry point for quorum intersection analysis. It:
/// 1. Loads the network configuration from the JSON file
/// 2. Verifies each node has a satisfiable quorum slice in the network
/// 3. Checks that all quorums in the network intersect
///
/// # Returns
///
/// * `Ok(true)` - Network enjoys quorum intersection (safe)
/// * `Ok(false)` - Network does NOT enjoy quorum intersection (unsafe!)
/// * `Err(_)` - Configuration error or unsatisfiable quorum slice
pub fn check_quorum_intersection_from_json(path: &Path) -> anyhow::Result<bool> {
    let qmap = load_quorum_map(path)?;

    if let Some(node) = find_unsatisfiable_node(&qmap) {
        anyhow::bail!(
            "quorum set for {} has no slice in network",
            node_id_to_hex(&node)
        );
    }

    match check_intersection(&qmap) {
        IntersectionResult::Intersects => Ok(true),
        IntersectionResult::Split { .. } => Ok(false),
        IntersectionResult::TooLarge { node_count } => {
            anyhow::bail!(
                "network has {} nodes, exceeding the brute-force analysis limit",
                node_count
            );
        }
    }
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
