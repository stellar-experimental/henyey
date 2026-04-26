use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use henyey_scp::quorum_config::parse_node_id;
use henyey_scp::{is_quorum, is_quorum_slice};
use serde::Deserialize;
use stellar_xdr::curr::{NodeId, ScpQuorumSet};

#[derive(Debug, Deserialize)]
struct QuorumIntersectionJson {
    nodes: Vec<NodeEntry>,
}

#[derive(Debug, Deserialize)]
struct NodeEntry {
    node: String,
    qset: QsetEntry,
}

#[derive(Debug, Deserialize)]
struct QsetEntry {
    t: u32,
    v: Vec<String>,
}

fn testdata_path(name: &str) -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("..");
    path.push("..");
    path.push("testdata");
    path.push("check-quorum-intersection-json");
    path.push(name);
    path
}

fn parse_qset(entry: &QsetEntry) -> Result<ScpQuorumSet, String> {
    let mut validators = Vec::with_capacity(entry.v.len());
    for node in &entry.v {
        let parsed = parse_node_id(node).map_err(|e| e.to_string())?;
        validators.push(parsed);
    }
    Ok(ScpQuorumSet {
        threshold: entry.t,
        validators: validators.try_into().unwrap_or_default(),
        inner_sets: Vec::new().try_into().unwrap_or_default(),
    })
}

fn load_quorum_map(name: &str) -> Result<HashMap<NodeId, ScpQuorumSet>, String> {
    let path = testdata_path(name);
    let payload = fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let json: QuorumIntersectionJson =
        serde_json::from_str(&payload).map_err(|e| format!("parse {}: {e}", path.display()))?;

    let mut map = HashMap::new();
    for entry in json.nodes {
        let node_id = parse_node_id(&entry.node).map_err(|e| e.to_string())?;
        let qset = parse_qset(&entry.qset)?;
        map.insert(node_id, qset);
    }

    Ok(map)
}

fn is_quorum_for_set(nodes: &HashSet<NodeId>, qmap: &HashMap<NodeId, ScpQuorumSet>) -> bool {
    let Some(first) = nodes.iter().next() else {
        return false;
    };
    let Some(local_qset) = qmap.get(first) else {
        return false;
    };

    is_quorum(local_qset, nodes, |node_id| qmap.get(node_id).cloned())
}

fn network_enjoys_quorum_intersection(qmap: &HashMap<NodeId, ScpQuorumSet>) -> bool {
    let nodes: Vec<NodeId> = qmap.keys().cloned().collect();
    if nodes.is_empty() {
        return false;
    }

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

    for i in 0..quorums.len() {
        for j in (i + 1)..quorums.len() {
            if quorums[i].is_disjoint(&quorums[j]) {
                return false;
            }
        }
    }

    true
}

#[test]
fn test_enjoys_quorum_intersection() {
    let qmap = load_quorum_map("enjoys-intersection.json").expect("load quorum map");
    assert!(network_enjoys_quorum_intersection(&qmap));
}

#[test]
fn test_no_quorum_intersection() {
    let qmap = load_quorum_map("no-intersection.json").expect("load quorum map");
    assert!(!network_enjoys_quorum_intersection(&qmap));
}

#[test]
fn test_bad_key() {
    let err = load_quorum_map("bad-key.json").unwrap_err();
    assert!(
        err.contains("invalid public key") || err.contains("Invalid public key"),
        "{err}"
    );
}

#[test]
fn test_bad_threshold_type() {
    let err = load_quorum_map("bad-threshold-type.json").unwrap_err();
    assert!(err.contains("parse"), "{err}");
}

#[test]
fn test_missing_file() {
    let err = load_quorum_map("no-file.json").unwrap_err();
    assert!(err.contains("read"), "{err}");
}

#[test]
fn test_quorum_slice_is_consistent() {
    let qmap = load_quorum_map("enjoys-intersection.json").expect("load quorum map");
    let nodes: HashSet<NodeId> = qmap.keys().cloned().collect();
    for (node, qset) in &qmap {
        let ok = is_quorum_slice(qset, &nodes, &|id| qmap.get(id).cloned());
        assert!(ok, "node {:?} should have a slice in full set", node);
    }
}

/// Cross-validate the efficient checker against the brute-force oracle.
#[test]
fn test_efficient_checker_agrees_with_brute_force_oracle() {
    use henyey_scp::quorum_intersection::{check_intersection, IntersectionResult};

    for name in &["enjoys-intersection.json", "no-intersection.json"] {
        let qmap = load_quorum_map(name).expect("load quorum map");
        let oracle = network_enjoys_quorum_intersection(&qmap);

        // Build the Option<ScpQuorumSet> map for the checker.
        let opt_map: HashMap<NodeId, Option<ScpQuorumSet>> = qmap
            .into_iter()
            .map(|(id, qset)| (id, Some(qset)))
            .collect();
        let checker = matches!(check_intersection(&opt_map), IntersectionResult::Intersects);

        assert_eq!(
            oracle, checker,
            "disagreement on {}: oracle={}, checker={}",
            name, oracle, checker
        );
    }
}
