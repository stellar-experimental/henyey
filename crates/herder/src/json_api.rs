//! JSON API for Herder diagnostics and monitoring.
//!
//! This module provides JSON-serializable structures for exposing Herder
//! state through admin endpoints. It matches the stellar-core `getJsonInfo()`,
//! `getJsonQuorumInfo()`, and related methods in `HerderImpl`.
//!
//! # Parity
//!
//! This module corresponds to the JSON output methods in:
//! - `HerderImpl::getJsonInfo()`
//! - `HerderImpl::getJsonQuorumInfo()`
//! - `HerderImpl::getJsonTransitiveQuorumInfo()`
//! - `HerderImpl::getJsonTransitiveQuorumIntersectionInfo()`
//! - `PendingEnvelopes::getJsonInfo()`

use serde::{Deserialize, Serialize};
use stellar_xdr::curr::NodeId;

/// Complete Herder JSON info response.
///
/// This is the top-level structure returned by `getJsonInfo()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HerderJsonInfo {
    /// This node's public key.
    pub you: String,
    /// SCP state information.
    pub scp: ScpJsonInfo,
    /// Pending envelopes queue information.
    pub queue: PendingEnvelopesJsonInfo,
}

/// SCP state JSON information.
///
/// Contains slot-by-slot SCP state for debugging.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScpJsonInfo {
    /// Current slot index.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot: Option<u64>,
    /// Phase within the slot (nominate, prepare, confirm, externalize).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// Per-slot state information.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub slots: Vec<SlotJsonInfo>,
}

/// Per-slot SCP state information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotJsonInfo {
    /// Slot index.
    pub index: u64,
    /// Current phase.
    pub phase: String,
    /// Ballot information (if in ballot phase).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ballot: Option<BallotJsonInfo>,
    /// Nomination information.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nomination: Option<NominationJsonInfo>,
    /// Nodes that have voted/accepted.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub validators: Vec<String>,
}

/// Ballot phase JSON information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BallotJsonInfo {
    /// Current ballot counter.
    pub counter: u32,
    /// Value hash (abbreviated).
    pub value: String,
    /// Whether we've committed.
    pub committed: bool,
    /// Highest confirmed prepared ballot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub h: Option<u32>,
}

/// Nomination phase JSON information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NominationJsonInfo {
    /// Number of voted values.
    pub votes: usize,
    /// Number of accepted values.
    pub accepted: usize,
    /// Whether nomination is complete.
    pub complete: bool,
}

/// Pending envelopes queue JSON information.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PendingEnvelopesJsonInfo {
    /// Number of pending envelopes.
    pub pending: usize,
    /// Number of ready envelopes.
    pub ready: usize,
    /// Number of fetching envelopes.
    pub fetching: usize,
    /// Per-slot pending counts.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub slots: Vec<PendingSlotJsonInfo>,
}

/// Per-slot pending envelope information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingSlotJsonInfo {
    /// Slot index.
    pub slot: u64,
    /// Number of pending envelopes for this slot.
    pub count: usize,
}

/// Quorum information JSON response.
///
/// Returned by `getJsonQuorumInfo()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuorumJsonInfo {
    /// Node ID being queried.
    pub node: String,
    /// Quorum set information.
    pub qset: QuorumSetJsonInfo,
    /// Transitive quorum intersection info (if available).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transitive: Option<TransitiveQuorumJsonInfo>,
    /// Nodes that may be dead.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub maybe_dead_nodes: Vec<String>,
}

/// Quorum set JSON information.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QuorumSetJsonInfo {
    /// Threshold for this quorum set.
    pub threshold: u32,
    /// Direct validators in this quorum set.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub validators: Vec<String>,
    /// Inner quorum sets.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub inner_sets: Vec<QuorumSetJsonInfo>,
    /// Quorum set hash (abbreviated).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// Agreement status per slot.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub agree: Vec<SlotAgreementInfo>,
    /// Lag information per node.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lag_ms: Option<LagJsonInfo>,
    /// Cost information per validator.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<ValidatorCostJsonInfo>,
}

/// Slot agreement status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotAgreementInfo {
    /// Slot index.
    pub slot: u64,
    /// Whether the node agrees on this slot.
    pub agrees: bool,
    /// Phase the node is in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
}

/// Lag information for quorum set nodes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LagJsonInfo {
    /// Per-node lag in milliseconds.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub nodes: Vec<NodeLagInfo>,
    /// Summary statistics.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<LagSummary>,
}

/// Per-node lag information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeLagInfo {
    /// Node ID.
    pub node: String,
    /// Lag in milliseconds.
    pub lag_ms: u64,
}

/// Lag summary statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LagSummary {
    /// Minimum lag.
    pub min_ms: u64,
    /// Maximum lag.
    pub max_ms: u64,
    /// Average lag.
    pub avg_ms: u64,
}

/// Validator cost information.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ValidatorCostJsonInfo {
    /// Per-validator cost.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub validators: Vec<ValidatorCost>,
}

/// Per-validator cost entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidatorCost {
    /// Node ID.
    pub node: String,
    /// Total cost.
    pub cost: u64,
}

/// Transitive quorum intersection information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransitiveQuorumJsonInfo {
    /// Whether the network enjoys quorum intersection.
    pub intersection: bool,
    /// Number of nodes in the transitive quorum.
    pub node_count: u64,
    /// Ledger when last checked.
    pub last_check_ledger: u64,
    /// Critical node groups (if intersection exists).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub critical: Vec<Vec<String>>,
    /// Last good ledger (if no intersection).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_good_ledger: Option<u64>,
    /// Potential split (if no intersection).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub potential_split: Option<(Vec<String>, Vec<String>)>,
}

/// Format a NodeID as a string.
///
/// # Arguments
///
/// * `node_id` - The node ID to format
/// * `full_keys` - If true, return full key; otherwise abbreviate
///
/// # Returns
///
/// A string representation of the node ID.
pub fn format_node_id(node_id: &NodeId, full_keys: bool) -> String {
    use stellar_strkey::ed25519::PublicKey as StrPublicKey;

    match &node_id.0 {
        stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(key) => {
            let strkey = StrPublicKey(key.0).to_string();
            if full_keys {
                strkey
            } else {
                // Return first 5 characters for abbreviated form
                strkey.chars().take(5).collect()
            }
        }
    }
}

/// Format a hash as a string.
///
/// # Arguments
///
/// * `hash` - The 32-byte hash
/// * `full` - If true, return full hex; otherwise abbreviate to 6 hex chars
///
/// When `full` is false, returns the first 6 hex characters (3 bytes),
/// matching stellar-core's `hexAbbrev()`.
///
/// # Returns
///
/// A hex string representation of the hash.
pub fn format_hash(hash: &[u8; 32], full: bool) -> String {
    let hex = hex::encode(hash);
    if full {
        hex
    } else {
        // stellar-core's hexAbbrev() takes 3 bytes = 6 hex chars.
        hex.chars().take(6).collect()
    }
}

/// Quorum info snapshot for the `/info` endpoint.
///
/// Matches stellar-core's `HerderImpl::getJsonQuorumInfo()` output
/// (HerderImpl.cpp:1754-1777). The `node` field holds the short node
/// identity and `qset` holds the per-slot quorum info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InfoQuorumSnapshot {
    /// Short node identity (first 5 chars of strkey, matching `toStrKey(id, false)`).
    pub node: String,
    /// Per-slot quorum set snapshot.
    pub qset: InfoQuorumSetSnapshot,
    /// Transitive quorum intersection information.
    /// Present only when intersection analysis has completed at least once
    /// with an intersecting result (matching stellar-core's `hasAnyResults()`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transitive: Option<TransitiveQuorumJsonInfo>,
}

/// Per-slot quorum set info for the `/info` endpoint.
///
/// Combines `BallotProtocol::getJsonQuorumInfo()` (phase, hash, fail_at),
/// `Slot::getJsonQuorumInfo()` (validated), and `SCP::getJsonQuorumInfo()`
/// (agree, disagree, missing, delayed, ledger) into one struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InfoQuorumSetSnapshot {
    /// Ballot phase: "PREPARE", "CONFIRM", "EXTERNALIZE", "unknown", or "expired".
    pub phase: String,
    /// Abbreviated quorum set hash (6 hex chars, matching `hexAbbrev()`).
    /// Absent when the quorum set is expired.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// Minimum number of nodes whose failure would block quorum.
    /// Absent when the quorum set is expired.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fail_at: Option<u64>,
    /// Whether the slot is fully validated. Present only for validators,
    /// matching `Slot::getJsonQuorumInfo()` (Slot.cpp:401-403).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validated: Option<bool>,
    /// Number of nodes that agree with local consensus.
    pub agree: u64,
    /// Number of nodes that disagree.
    pub disagree: u64,
    /// Number of nodes not heard from.
    pub missing: u64,
    /// Number of nodes that are behind but on the same track.
    pub delayed: u64,
    /// Slot index (ledger sequence number).
    pub ledger: u64,
}

/// Slot-level quorum info summary.
///
/// Intermediate result from `Slot::get_quorum_info_summary()`, before
/// the SCP-level assembly adds agree/disagree/missing/delayed counts.
#[derive(Debug, Clone)]
pub struct SlotQuorumInfoSummary {
    /// Ballot phase: "PREPARE", "CONFIRM", "EXTERNALIZE", "unknown", or "expired".
    pub phase: String,
    /// Abbreviated quorum set hash (6 hex chars).
    pub hash: String,
    /// Minimum number of nodes whose failure would block quorum.
    pub fail_at: usize,
    /// Whether the slot is fully validated (`Some(bool)` for validators, `None` for watchers).
    pub validated: Option<bool>,
}

/// Builder for constructing HerderJsonInfo.
#[derive(Debug, Default)]
pub struct HerderJsonInfoBuilder {
    you: Option<String>,
    scp: ScpJsonInfo,
    queue: PendingEnvelopesJsonInfo,
}

impl HerderJsonInfoBuilder {
    /// Create a new builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the local node ID.
    pub fn you(mut self, node_id: &NodeId, full_keys: bool) -> Self {
        self.you = Some(format_node_id(node_id, full_keys));
        self
    }

    /// Set the local node ID from a string.
    pub fn you_str(mut self, you: String) -> Self {
        self.you = Some(you);
        self
    }

    /// Set the SCP info.
    pub fn scp(mut self, scp: ScpJsonInfo) -> Self {
        self.scp = scp;
        self
    }

    /// Set the pending envelopes info.
    pub fn queue(mut self, queue: PendingEnvelopesJsonInfo) -> Self {
        self.queue = queue;
        self
    }

    /// Build the HerderJsonInfo.
    pub fn build(self) -> HerderJsonInfo {
        HerderJsonInfo {
            you: self.you.unwrap_or_else(|| "unknown".to_string()),
            scp: self.scp,
            queue: self.queue,
        }
    }
}

/// Builder for constructing QuorumJsonInfo.
#[derive(Debug, Default)]
pub struct QuorumJsonInfoBuilder {
    node: Option<String>,
    qset: QuorumSetJsonInfo,
    transitive: Option<TransitiveQuorumJsonInfo>,
    maybe_dead_nodes: Vec<String>,
}

impl QuorumJsonInfoBuilder {
    /// Create a new builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the node ID.
    pub fn node(mut self, node_id: &NodeId, full_keys: bool) -> Self {
        self.node = Some(format_node_id(node_id, full_keys));
        self
    }

    /// Set the quorum set info.
    pub fn qset(mut self, qset: QuorumSetJsonInfo) -> Self {
        self.qset = qset;
        self
    }

    /// Set the transitive quorum info.
    pub fn transitive(mut self, transitive: TransitiveQuorumJsonInfo) -> Self {
        self.transitive = Some(transitive);
        self
    }

    /// Add a maybe-dead node.
    pub fn add_maybe_dead_node(mut self, node_id: &NodeId, full_keys: bool) -> Self {
        self.maybe_dead_nodes
            .push(format_node_id(node_id, full_keys));
        self
    }

    /// Build the QuorumJsonInfo.
    pub fn build(self) -> QuorumJsonInfo {
        QuorumJsonInfo {
            node: self.node.unwrap_or_else(|| "unknown".to_string()),
            qset: self.qset,
            transitive: self.transitive,
            maybe_dead_nodes: self.maybe_dead_nodes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::Uint256;

    fn make_test_node_id(seed: u8) -> NodeId {
        let mut key = [0u8; 32];
        key[0] = seed;
        NodeId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(Uint256(
            key,
        )))
    }

    #[test]
    fn test_format_node_id_full() {
        let node_id = make_test_node_id(0);
        let formatted = format_node_id(&node_id, true);
        assert!(formatted.starts_with('G'));
        assert_eq!(formatted.len(), 56); // Full strkey length
    }

    #[test]
    fn test_format_node_id_abbreviated() {
        let node_id = make_test_node_id(0);
        let formatted = format_node_id(&node_id, false);
        assert!(formatted.starts_with('G'));
        assert_eq!(formatted.len(), 5);
    }

    #[test]
    fn test_format_hash_full() {
        let hash = [0xABu8; 32];
        let formatted = format_hash(&hash, true);
        assert_eq!(formatted.len(), 64); // Full hex length
        assert!(formatted.starts_with("abab"));
    }

    #[test]
    fn test_format_hash_abbreviated() {
        let hash = [0xABu8; 32];
        let formatted = format_hash(&hash, false);
        assert_eq!(formatted.len(), 6);
        assert_eq!(formatted, "ababab");
    }

    #[test]
    fn test_herder_json_info_builder() {
        let node_id = make_test_node_id(1);
        let info = HerderJsonInfoBuilder::new()
            .you(&node_id, false)
            .scp(ScpJsonInfo::default())
            .queue(PendingEnvelopesJsonInfo::default())
            .build();

        assert_eq!(info.you.len(), 5);
        assert!(info.you.starts_with('G'));
    }

    #[test]
    fn test_quorum_json_info_builder() {
        let node_id = make_test_node_id(2);
        let info = QuorumJsonInfoBuilder::new()
            .node(&node_id, true)
            .qset(QuorumSetJsonInfo::default())
            .build();

        assert_eq!(info.node.len(), 56);
    }

    #[test]
    fn test_serialization() {
        let info = HerderJsonInfo {
            you: "GABCD".to_string(),
            scp: ScpJsonInfo {
                slot: Some(100),
                phase: Some("externalize".to_string()),
                slots: vec![],
            },
            queue: PendingEnvelopesJsonInfo {
                pending: 5,
                ready: 2,
                fetching: 1,
                slots: vec![],
            },
        };

        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("GABCD"));
        assert!(json.contains("externalize"));
        assert!(json.contains("\"pending\":5"));

        // Verify deserialization
        let parsed: HerderJsonInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.you, "GABCD");
        assert_eq!(parsed.scp.slot, Some(100));
    }

    #[test]
    fn test_transitive_quorum_serialization() {
        let info = TransitiveQuorumJsonInfo {
            intersection: true,
            node_count: 10,
            last_check_ledger: 12345,
            critical: vec![vec!["GABCD".to_string(), "GDEFG".to_string()]],
            last_good_ledger: None,
            potential_split: None,
        };

        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("\"intersection\":true"));
        assert!(json.contains("\"node_count\":10"));
        assert!(json.contains("GABCD"));
    }

    #[test]
    fn test_info_quorum_snapshot_serialization() {
        let snapshot = InfoQuorumSnapshot {
            node: "GABCD".to_string(),
            qset: InfoQuorumSetSnapshot {
                phase: "PREPARE".to_string(),
                hash: Some("abcdef".to_string()),
                fail_at: Some(2),
                validated: Some(true),
                agree: 3,
                disagree: 0,
                missing: 1,
                delayed: 1,
                ledger: 42,
            },
            transitive: None,
        };

        let value = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(value["node"], "GABCD");
        assert_eq!(value["qset"]["phase"], "PREPARE");
        assert_eq!(value["qset"]["hash"], "abcdef");
        assert_eq!(value["qset"]["fail_at"], 2);
        assert_eq!(value["qset"]["validated"], true);
        assert_eq!(value["qset"]["agree"], 3);
        assert_eq!(value["qset"]["disagree"], 0);
        assert_eq!(value["qset"]["missing"], 1);
        assert_eq!(value["qset"]["delayed"], 1);
        assert_eq!(value["qset"]["ledger"], 42);
    }

    #[test]
    fn test_info_quorum_snapshot_validated_absent_for_watcher() {
        let snapshot = InfoQuorumSnapshot {
            node: "GABCD".to_string(),
            qset: InfoQuorumSetSnapshot {
                phase: "CONFIRM".to_string(),
                hash: Some("123456".to_string()),
                fail_at: Some(1),
                validated: None,
                agree: 5,
                disagree: 0,
                missing: 0,
                delayed: 0,
                ledger: 100,
            },
            transitive: None,
        };

        let value = serde_json::to_value(&snapshot).unwrap();
        // validated should be absent (skip_serializing_if = "Option::is_none")
        assert!(
            value["qset"].get("validated").is_none(),
            "validated should be absent for watcher nodes"
        );
    }

    #[test]
    fn test_info_quorum_snapshot_validated_nested_under_qset() {
        let snapshot = InfoQuorumSnapshot {
            node: "GABCD".to_string(),
            qset: InfoQuorumSetSnapshot {
                phase: "EXTERNALIZE".to_string(),
                hash: Some("aabbcc".to_string()),
                fail_at: Some(0),
                validated: Some(true),
                agree: 4,
                disagree: 0,
                missing: 0,
                delayed: 0,
                ledger: 200,
            },
            transitive: None,
        };

        let value = serde_json::to_value(&snapshot).unwrap();
        // validated must be inside qset, not at top level
        assert!(
            value.get("validated").is_none(),
            "validated must not be at top level"
        );
        assert!(
            value["qset"].get("validated").is_some(),
            "validated must be inside qset"
        );
    }

    #[test]
    fn test_info_quorum_snapshot_with_transitive_intersecting() {
        let snapshot = InfoQuorumSnapshot {
            node: "GABCD".to_string(),
            qset: InfoQuorumSetSnapshot {
                phase: "EXTERNALIZE".to_string(),
                hash: Some("aabbcc".to_string()),
                fail_at: Some(0),
                validated: Some(true),
                agree: 4,
                disagree: 0,
                missing: 0,
                delayed: 0,
                ledger: 200,
            },
            transitive: Some(TransitiveQuorumJsonInfo {
                intersection: true,
                node_count: 5,
                last_check_ledger: 200,
                critical: vec![],
                last_good_ledger: None,
                potential_split: None,
            }),
        };

        let value = serde_json::to_value(&snapshot).unwrap();
        let t = &value["transitive"];
        assert_eq!(t["intersection"], true);
        assert_eq!(t["node_count"], 5);
        assert_eq!(t["last_check_ledger"], 200);
        // critical is empty → omitted
        assert!(t.get("critical").is_none());
        // last_good_ledger and potential_split omitted when intersecting
        assert!(t.get("last_good_ledger").is_none());
        assert!(t.get("potential_split").is_none());
    }

    #[test]
    fn test_info_quorum_snapshot_with_transitive_split() {
        let snapshot = InfoQuorumSnapshot {
            node: "GABCD".to_string(),
            qset: InfoQuorumSetSnapshot {
                phase: "EXTERNALIZE".to_string(),
                hash: Some("aabbcc".to_string()),
                fail_at: Some(0),
                validated: Some(true),
                agree: 4,
                disagree: 0,
                missing: 0,
                delayed: 0,
                ledger: 300,
            },
            transitive: Some(TransitiveQuorumJsonInfo {
                intersection: false,
                node_count: 6,
                last_check_ledger: 300,
                critical: vec![],
                last_good_ledger: Some(250),
                potential_split: Some((
                    vec!["GAAA".to_string(), "GBBB".to_string()],
                    vec!["GCCC".to_string()],
                )),
            }),
        };

        let value = serde_json::to_value(&snapshot).unwrap();
        let t = &value["transitive"];
        assert_eq!(t["intersection"], false);
        assert_eq!(t["node_count"], 6);
        assert_eq!(t["last_check_ledger"], 300);
        assert_eq!(t["last_good_ledger"], 250);
        // potential_split is a pair of arrays
        let split = t.get("potential_split").expect("split should be present");
        assert!(split.is_array());
        assert_eq!(split[0][0], "GAAA");
        assert_eq!(split[1][0], "GCCC");
    }

    #[test]
    fn test_info_quorum_snapshot_transitive_absent_when_none() {
        let snapshot = InfoQuorumSnapshot {
            node: "GABCD".to_string(),
            qset: InfoQuorumSetSnapshot {
                phase: "EXTERNALIZE".to_string(),
                hash: None,
                fail_at: None,
                validated: None,
                agree: 0,
                disagree: 0,
                missing: 0,
                delayed: 0,
                ledger: 0,
            },
            transitive: None,
        };

        let value = serde_json::to_value(&snapshot).unwrap();
        assert!(
            value.get("transitive").is_none(),
            "transitive should be absent when None"
        );
    }
}
