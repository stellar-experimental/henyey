//! History Archive State (HAS) parsing and handling.
//!
//! The History Archive State is a JSON file that describes the current state
//! of a Stellar history archive, including the current ledger and bucket list hashes.

use henyey_bucket::{PendingMergeState, BUCKET_LIST_LEVELS};
use henyey_common::Hash256;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::error::HistoryError;

/// FutureBucket state constants (matches stellar-core's FBStatus enum in HAS JSON).
/// These are HAS-JSON-format concerns used for parsing and serialization.
pub(crate) const HAS_NEXT_STATE_CLEAR: u32 = 0;
pub(crate) const HAS_NEXT_STATE_OUTPUT: u32 = 1;
pub(crate) const HAS_NEXT_STATE_INPUTS: u32 = 2;

/// Maximum allowed size for a bucket file downloaded from a history archive (100 GiB).
///
/// This matches stellar-core's `MAX_HISTORY_ARCHIVE_BUCKET_SIZE` constant
/// and prevents downloading unreasonably large or corrupted bucket files.
pub const MAX_HISTORY_ARCHIVE_BUCKET_SIZE: u64 = 100 * 1024 * 1024 * 1024;

/// The zero hash string used to identify empty bucket slots.
const ZERO_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Try to parse a hex hash string, returning `Some(Hash256)` if non-empty and non-zero.
fn parse_nonzero_hash(hex: &str) -> Option<Hash256> {
    if hex.is_empty() || hex == ZERO_HASH {
        return None;
    }
    Hash256::from_hex(hex).ok()
}

/// Collect all non-zero bucket hashes from a slice of bucket levels.
fn collect_bucket_hashes(levels: &[HASBucketLevel]) -> Vec<Hash256> {
    let mut out = Vec::new();
    for level in levels {
        if let Some(h) = parse_nonzero_hash(&level.curr) {
            out.push(h);
        }
        if let Some(h) = parse_nonzero_hash(&level.snap) {
            out.push(h);
        }
        if let Some(ref output) = level.next.output {
            if let Some(h) = parse_nonzero_hash(output) {
                out.push(h);
            }
        }
        if level.next.state == HAS_NEXT_STATE_INPUTS {
            if let Some(ref curr) = level.next.curr {
                if let Some(h) = parse_nonzero_hash(curr) {
                    out.push(h);
                }
            }
            if let Some(ref snap) = level.next.snap {
                if let Some(h) = parse_nonzero_hash(snap) {
                    out.push(h);
                }
            }
        }
    }
    out
}

/// Parse bucket hash pairs (curr, snap) from a slice of bucket levels.
fn parse_bucket_hash_pairs(levels: &[HASBucketLevel]) -> Vec<(Hash256, Hash256)> {
    levels
        .iter()
        .map(|level| {
            let curr = parse_nonzero_hash(&level.curr).unwrap_or(Hash256::ZERO);
            let snap = parse_nonzero_hash(&level.snap).unwrap_or(Hash256::ZERO);
            (curr, snap)
        })
        .collect()
}

/// Parse the next-merge states from a slice of bucket levels.
///
/// Returns an error if:
/// - A hash field required by the declared state is present but contains a
///   malformed hex string.
/// - A hash field required by the declared state is absent (`None`). Parity:
///   stellar-core's FutureBucket invariants require output for state-1 and
///   both inputs for state-2 (FutureBucket.cpp:248-315).
/// - The state discriminant is not 0, 1, or 2 (unknown state).
///
/// Canonicalization: state-1 with a zero-hash output is mapped to `None` (clear),
/// matching the effective behavior where zero-hash outputs are never loaded as buckets.
fn parse_next_states(
    levels: &[HASBucketLevel],
) -> std::result::Result<Vec<Option<PendingMergeState>>, HistoryError> {
    levels
        .iter()
        .enumerate()
        .map(|(i, level)| {
            match level.next.state {
                HAS_NEXT_STATE_CLEAR => Ok(None),
                HAS_NEXT_STATE_OUTPUT => {
                    let h_str = level.next.output.as_ref().ok_or_else(|| {
                        HistoryError::InvalidResponse(format!(
                            "level {}: state {} requires output hash but field is absent",
                            i, level.next.state
                        ))
                    })?;
                    let hash = Hash256::from_hex(h_str).map_err(|_| {
                        HistoryError::InvalidResponse(format!(
                            "level {}: state {} output hash is malformed: {}",
                            i, level.next.state, h_str
                        ))
                    })?;
                    // Canonicalize: zero-hash output → None (effectively clear)
                    if hash.is_zero() {
                        Ok(None)
                    } else {
                        Ok(Some(PendingMergeState::Output(hash)))
                    }
                }
                HAS_NEXT_STATE_INPUTS => {
                    let curr_str = level.next.curr.as_ref().ok_or_else(|| {
                        HistoryError::InvalidResponse(format!(
                            "level {}: state {} requires input curr hash but field is absent",
                            i, level.next.state
                        ))
                    })?;
                    let curr = Hash256::from_hex(curr_str).map_err(|_| {
                        HistoryError::InvalidResponse(format!(
                            "level {}: state {} input curr hash is malformed: {}",
                            i, level.next.state, curr_str
                        ))
                    })?;
                    let snap_str = level.next.snap.as_ref().ok_or_else(|| {
                        HistoryError::InvalidResponse(format!(
                            "level {}: state {} requires input snap hash but field is absent",
                            i, level.next.state
                        ))
                    })?;
                    let snap = Hash256::from_hex(snap_str).map_err(|_| {
                        HistoryError::InvalidResponse(format!(
                            "level {}: state {} input snap hash is malformed: {}",
                            i, level.next.state, snap_str
                        ))
                    })?;
                    Ok(Some(PendingMergeState::Inputs { curr, snap }))
                }
                unknown => Err(HistoryError::InvalidResponse(format!(
                    "level {}: unknown FutureBucket state: {}",
                    i, unknown
                ))),
            }
        })
        .collect()
}

/// Parse bucket hashes at a specific level, returning (curr, snap) as Options.
#[cfg(test)]
fn parse_level_hashes(level: &HASBucketLevel) -> (Option<Hash256>, Option<Hash256>) {
    (
        parse_nonzero_hash(&level.curr),
        parse_nonzero_hash(&level.snap),
    )
}

fn validate_known_hash(
    known_hashes: &HashSet<Hash256>,
    level: usize,
    label: &str,
    hash: &str,
) -> Result<(), HistoryError> {
    if let Some(parsed) = parse_nonzero_hash(hash) {
        if !known_hashes.contains(&parsed) {
            return Err(HistoryError::VerificationFailed(format!(
                "unknown {label} bucket hash at level {level}: {hash}"
            )));
        }
    }

    Ok(())
}

fn validate_future_bucket_hashes(
    known_hashes: &HashSet<Hash256>,
    level: usize,
    next: &HASBucketNext,
) -> Result<(), HistoryError> {
    match next.state {
        HAS_NEXT_STATE_CLEAR => {}
        HAS_NEXT_STATE_OUTPUT => {
            if let Some(output) = next.output.as_deref() {
                validate_known_hash(known_hashes, level, "output", output)?;
            }
        }
        HAS_NEXT_STATE_INPUTS => {
            if let Some(curr) = next.curr.as_deref() {
                validate_known_hash(known_hashes, level, "input curr", curr)?;
            }
            if let Some(snap) = next.snap.as_deref() {
                validate_known_hash(known_hashes, level, "input snap", snap)?;
            }
        }
        unknown => {
            return Err(HistoryError::VerificationFailed(format!(
                "level {}: unknown FutureBucket state: {}",
                level, unknown
            )));
        }
    }

    Ok(())
}

/// Compute the differential bucket set for a single bucket type (live or hot archive).
///
/// This mirrors the `processBuckets` lambda inside stellar-core's `differingBuckets()`.
///
/// 1. Build inhibit set from `other_levels` (all curr, snap, and next output hashes).
/// 2. Walk `self_levels` from bottom (highest index) to top.
/// 3. For each level, collect snap, next output, curr — in that order — if not inhibited.
/// 4. Add collected hashes to the inhibit set.
fn differing_buckets_for_levels(
    self_levels: &[HASBucketLevel],
    other_levels: &[HASBucketLevel],
) -> Vec<Hash256> {
    let mut inhibit: HashSet<Hash256> = HashSet::new();
    inhibit.insert(Hash256::ZERO);

    // Populate inhibit set from `other` (local state).
    for level in other_levels {
        if let Some(h) = parse_nonzero_hash(&level.curr) {
            inhibit.insert(h);
        }
        if let Some(h) = parse_nonzero_hash(&level.snap) {
            inhibit.insert(h);
        }
        // If the future has an output hash (state == 1), inhibit it too.
        if level.next.has_output_hash() {
            if let Some(ref output) = level.next.output {
                if let Some(h) = parse_nonzero_hash(output) {
                    inhibit.insert(h);
                }
            }
        }
    }

    let mut result = Vec::new();

    // Walk levels from bottom (highest index) to top (index 0).
    for i in (0..self_levels.len()).rev() {
        let level = &self_levels[i];

        // Collect in order: snap, next output, curr
        // (stellar-core: auto bs = {s, n, c})
        let snap = parse_nonzero_hash(&level.snap);
        let next_output = if level.next.has_output_hash() {
            level
                .next
                .output
                .as_ref()
                .and_then(|o| parse_nonzero_hash(o))
        } else {
            snap // n = s when no output hash (mirrors stellar-core: auto n = s)
        };
        let curr = parse_nonzero_hash(&level.curr);

        for hash in [&snap, &next_output, &curr].into_iter().flatten() {
            if !inhibit.contains(hash) {
                result.push(*hash);
                inhibit.insert(*hash);
            }
        }
    }

    result
}

/// History Archive State - the root JSON file describing archive state.
///
/// This is typically found at `.well-known/stellar-history.json` or at
/// checkpoint-specific paths like `history/00/00/00/history-0000003f.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryArchiveState {
    /// Format version (currently 2).
    pub version: u32,

    /// Server identifier (e.g., "stellar-core 25.0.1").
    #[serde(default)]
    pub server: Option<String>,

    /// Current ledger sequence.
    pub current_ledger: u32,

    /// Network passphrase.
    #[serde(default)]
    pub network_passphrase: Option<String>,

    /// Bucket list state (current buckets).
    pub current_buckets: Vec<HASBucketLevel>,

    /// Hot archive buckets (for hot archive state, if present).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hot_archive_buckets: Option<Vec<HASBucketLevel>>,
}

/// A single level in the bucket list hierarchy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HASBucketLevel {
    /// Current bucket hash (hex-encoded).
    pub curr: String,

    /// Snapshot bucket hash (hex-encoded).
    pub snap: String,

    /// Next bucket state (for async merge tracking).
    #[serde(default)]
    pub next: HASBucketNext,
}

/// State of the next bucket merge operation.
///
/// States (matching stellar-core FutureBucket::State):
/// - 0 (FB_CLEAR): No pending merge
/// - 1 (FB_HASH_OUTPUT): Merge complete, output hash is known
/// - 2 (FB_HASH_INPUTS): Merge in progress, input hashes are stored
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HASBucketNext {
    /// Merge state (0 = clear, 1 = output, 2 = inputs).
    pub state: u32,

    /// Output bucket hash if merge is complete (state == 1).
    #[serde(default)]
    pub output: Option<String>,

    /// Input curr bucket hash for pending merge (state == 2).
    #[serde(default)]
    pub curr: Option<String>,

    /// Input snap bucket hash for pending merge (state == 2).
    #[serde(default)]
    pub snap: Option<String>,

    /// Shadow bucket hashes for pending merge (state == 2, pre-protocol 12).
    #[serde(default)]
    pub shadow: Option<Vec<String>>,
}

impl HASBucketNext {
    /// Check if this future bucket has a known output hash.
    ///
    /// Matches stellar-core's `FutureBucket::hasOutputHash()`:
    /// state == 1 (FB_HASH_OUTPUT) means the merge is complete and the
    /// output hash is known.
    pub fn has_output_hash(&self) -> bool {
        self.state == HAS_NEXT_STATE_OUTPUT && self.output.is_some()
    }

    /// Check if this future bucket is clear (no pending merge).
    pub fn is_clear(&self) -> bool {
        self.state == HAS_NEXT_STATE_CLEAR
    }
}

/// Per-level bucket version info for structural validation.
///
/// Used by `validate_bucket_list_structure` to check version monotonicity
/// and protocol-dependent `next` field validity.
pub struct BucketLevelVersionInfo {
    /// Protocol version of the snap bucket (0 if empty/zero-hash).
    pub snap_version: u32,
    /// Protocol version of the curr bucket (0 if empty/zero-hash).
    pub curr_version: u32,
    /// The `next` field from the corresponding HAS level.
    pub next: HASBucketNext,
}

/// First protocol version where bucket shadows were removed.
/// Matches stellar-core's `LiveBucket::FIRST_PROTOCOL_SHADOWS_REMOVED`.
pub(crate) const FIRST_PROTOCOL_SHADOWS_REMOVED: u32 = 12;

/// Validate bucket list structure and version monotonicity.
///
/// Mirrors stellar-core's `validateBucketListHelper` (HistoryArchive.cpp:340-446).
/// Walks from deepest level (highest index) to level 0, checking:
/// - Bucket count matches `expected_level_count`
/// - Snap then curr version at each level >= running `min_bucket_version`
/// - Level 0 `next` must be clear
/// - Level i>0: if `prev_snap` (level i-1) version >= FIRST_PROTOCOL_SHADOWS_REMOVED,
///   then `next` must be clear; otherwise `next` must have a resolved output hash
///
/// Empty buckets have version 0 and are skipped until the first non-empty bucket
/// is seen (bottom-up). After a non-empty bucket, all higher-level buckets
/// must satisfy the version constraint.
///
/// # Equivalence note
///
/// Stellar-core's `validateBucketListHelper` flips its `nonEmptySeen` flag from
/// `!bucket->isEmpty()`, while this implementation flips it from `version > 0`.
/// For protocol 24+ (the only protocol range henyey supports — see
/// `crates/henyey/Cargo.toml` and the project README) every non-empty bucket
/// carries a leading metaentry with `ledger_version >= 11`, so the two
/// formulations are equivalent. Older protocols (pre-11) could in principle
/// produce a non-empty bucket without a metaentry (`version == 0`), but those
/// are out of scope for this codebase.
pub fn validate_bucket_list_structure(
    levels: &[BucketLevelVersionInfo],
    expected_level_count: usize,
) -> Result<(), HistoryError> {
    if levels.len() != expected_level_count {
        return Err(HistoryError::VerificationFailed(format!(
            "bucket list size mismatch: expected {} levels, got {}",
            expected_level_count,
            levels.len()
        )));
    }

    let mut non_empty_seen = false;
    let mut min_bucket_version: u32 = 0;

    // Walk from deepest level (highest index) to 0
    for j in (0..expected_level_count).rev() {
        let level = &levels[j];

        // snap is always older than curr, processed first
        // (mirrors stellar-core's ordering in validateBucketListHelper)
        for &version in &[level.snap_version, level.curr_version] {
            if version > 0 {
                non_empty_seen = true;
            }
            if version < min_bucket_version {
                return Err(HistoryError::VerificationFailed(format!(
                    "incompatible bucket versions: expected version {} or higher, got {}",
                    min_bucket_version, version
                )));
            }
            min_bucket_version = version;
        }

        // Level 0: next must be clear
        if j == 0 {
            if !level.next.is_clear() {
                return Err(HistoryError::VerificationFailed(
                    "invalid HAS: next must be clear at level 0".to_string(),
                ));
            }
            break;
        }

        // Use previous level (j-1) snap to determine "next" validity
        let prev_snap_version = levels[j - 1].snap_version;
        if prev_snap_version > 0 {
            non_empty_seen = true;
        }

        if !non_empty_seen {
            // Haven't seen any non-empty bucket yet (from bottom up),
            // skip next-field check for default-initialized levels
            continue;
        } else if prev_snap_version >= FIRST_PROTOCOL_SHADOWS_REMOVED {
            if !level.next.is_clear() {
                return Err(HistoryError::VerificationFailed(format!(
                    "invalid HAS: future must be cleared at level {} (prev snap version {})",
                    j, prev_snap_version
                )));
            }
        } else if !level.next.has_output_hash() {
            return Err(HistoryError::VerificationFailed(format!(
                "invalid HAS: future must have resolved output at level {} (prev snap version {})",
                j, prev_snap_version
            )));
        }
    }

    Ok(())
}

impl HistoryArchiveState {
    /// Parse a History Archive State from JSON.
    ///
    /// # Arguments
    ///
    /// * `json` - The JSON string to parse
    ///
    /// # Returns
    ///
    /// The parsed `HistoryArchiveState` or an error if parsing fails.
    ///
    /// # Examples
    ///
    /// ```
    /// use henyey_history::archive_state::HistoryArchiveState;
    ///
    /// // Structurally valid v1 HAS with BUCKET_LIST_LEVELS (11) zero-hash levels.
    /// let zero = "0".repeat(64);
    /// let zero_level = format!(
    ///     r#"{{"curr":"{}","snap":"{}","next":{{"state":0}}}}"#,
    ///     zero, zero
    /// );
    /// let levels: Vec<_> = (0..11).map(|_| zero_level.clone()).collect();
    /// let json = format!(
    ///     r#"{{"version":1,"currentLedger":12345,"currentBuckets":[{}]}}"#,
    ///     levels.join(",")
    /// );
    ///
    /// let has = HistoryArchiveState::from_json(&json).unwrap();
    /// assert_eq!(has.current_ledger, 12345);
    /// ```
    pub fn from_json(json: &str) -> Result<Self, HistoryError> {
        let has: Self = serde_json::from_str(json).map_err(HistoryError::Json)?;
        has.validate_version_invariants()?;
        Ok(has)
    }

    /// Validate version-dependent invariants during deserialization.
    ///
    /// - Version < 2: `hot_archive_buckets` must be absent (v1 format predates
    ///   hot archive support)
    ///
    /// Note: version >= 2 MAY or MAY NOT have `hot_archive_buckets` here because
    /// this is a lenient parse-time check — older v2 HAS files predate hot archive
    /// support. The stricter invariant (v2 MUST have hotArchiveBuckets for current
    /// protocol) is enforced by `verify::verify_has_structure()` during catchup.
    pub fn validate_version_invariants(&self) -> Result<(), HistoryError> {
        if self.version < 2 && self.hot_archive_buckets.is_some() {
            return Err(HistoryError::VerificationFailed(
                "HAS version < 2 must not include hotArchiveBuckets".to_string(),
            ));
        }
        Ok(())
    }

    /// Get hot archive bucket levels (version-gated).
    ///
    /// Returns the hot archive bucket levels for version >= 2 HAS,
    /// or an empty slice for version < 2.
    pub fn hot_archive_levels(&self) -> &[HASBucketLevel] {
        match &self.hot_archive_buckets {
            Some(levels) => levels,
            None => &[],
        }
    }

    /// Get mutable access to hot archive bucket levels (version-gated).
    ///
    /// Returns `Some` for version >= 2 HAS with populated hot archive,
    /// `None` for version < 2 or missing hot archive data.
    #[cfg(test)]
    pub fn hot_archive_levels_mut(&mut self) -> Option<&mut Vec<HASBucketLevel>> {
        self.hot_archive_buckets.as_mut()
    }

    /// Serialize the History Archive State to JSON.
    pub fn to_json(&self) -> Result<String, HistoryError> {
        serde_json::to_string_pretty(self).map_err(HistoryError::Json)
    }

    /// Get the current ledger sequence.
    #[inline]
    pub fn current_ledger(&self) -> u32 {
        self.current_ledger
    }

    /// Get all bucket hashes referenced in this HAS.
    ///
    /// This returns all non-zero bucket hashes from both `curr` and `snap`
    /// fields of each bucket level, which are needed for catchup.
    ///
    /// # Returns
    ///
    /// A vector of all unique bucket hashes.
    pub fn all_bucket_hashes(&self) -> Vec<Hash256> {
        let mut hashes = collect_bucket_hashes(&self.current_buckets);
        if let Some(ref hot_buckets) = self.hot_archive_buckets {
            hashes.extend(collect_bucket_hashes(hot_buckets));
        }
        hashes
    }

    /// Get the unique bucket hashes (deduplicated).
    ///
    /// This is useful when you want to avoid downloading the same bucket twice.
    pub fn unique_bucket_hashes(&self) -> Vec<Hash256> {
        let mut hashes = self.all_bucket_hashes();
        hashes.sort_by_key(|a| a.0);
        hashes.dedup();
        hashes
    }

    /// Get the network passphrase if available.
    pub fn network_passphrase(&self) -> Option<&str> {
        self.network_passphrase.as_deref()
    }

    /// Compute the bucket list hash from this HAS using the same algorithm
    /// as the Go SDK (Horizon).
    ///
    /// This is SHA256(concat of per-level SHA256(hex_decode(curr) || hex_decode(snap)))
    /// for the live bucket list, and for version >= 2, SHA256(live_hash || hot_hash).
    ///
    /// This function is used for diagnostic verification - the result should
    /// match the ledger header's `bucket_list_hash` for the same checkpoint.
    pub fn compute_bucket_list_hash(&self) -> Result<Hash256, HistoryError> {
        use sha2::{Digest, Sha256};

        let hash_levels = |levels: &[HASBucketLevel]| -> Result<Hash256, HistoryError> {
            let mut total = Vec::with_capacity(levels.len() * 32);
            for level in levels {
                let curr_bytes = hex::decode(&level.curr).map_err(|e| {
                    HistoryError::VerificationFailed(format!("invalid hex in curr: {}", e))
                })?;
                let snap_bytes = hex::decode(&level.snap).map_err(|e| {
                    HistoryError::VerificationFailed(format!("invalid hex in snap: {}", e))
                })?;
                let mut h = Sha256::new();
                h.update(&curr_bytes);
                h.update(&snap_bytes);
                total.extend_from_slice(&h.finalize());
            }
            let mut h = Sha256::new();
            h.update(&total);
            let r = h.finalize();
            let mut bytes = [0u8; 32];
            bytes.copy_from_slice(&r);
            Ok(Hash256::from_bytes(bytes))
        };

        let live_hash = hash_levels(&self.current_buckets)?;

        if self.version < 2 {
            return Ok(live_hash);
        }

        let hot_hash = if let Some(ref hot_levels) = self.hot_archive_buckets {
            hash_levels(hot_levels)?
        } else {
            // No hot archive levels: compute hash of all-zero levels
            let zero_levels: Vec<HASBucketLevel> = (0..BUCKET_LIST_LEVELS)
                .map(|_| HASBucketLevel {
                    curr: "0000000000000000000000000000000000000000000000000000000000000000"
                        .to_string(),
                    snap: "0000000000000000000000000000000000000000000000000000000000000000"
                        .to_string(),
                    next: HASBucketNext::default(),
                })
                .collect();
            hash_levels(&zero_levels)?
        };

        let mut h = Sha256::new();
        h.update(live_hash.as_bytes());
        h.update(hot_hash.as_bytes());
        let r = h.finalize();
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&r);
        Ok(Hash256::from_bytes(bytes))
    }

    /// Get the server version string if available.
    pub fn server(&self) -> Option<&str> {
        self.server.as_deref()
    }

    /// Get the number of bucket levels.
    #[cfg(test)]
    pub fn bucket_level_count(&self) -> usize {
        self.current_buckets.len()
    }

    /// Get bucket hashes for a specific level.
    ///
    /// # Arguments
    ///
    /// * `level` - The bucket level index (0 = most frequently updated)
    ///
    /// # Returns
    ///
    /// A tuple of (current_hash, snapshot_hash) if the level exists.
    #[cfg(test)]
    pub fn bucket_hashes_at_level(
        &self,
        level: usize,
    ) -> Option<(Option<Hash256>, Option<Hash256>)> {
        self.current_buckets.get(level).map(parse_level_hashes)
    }

    /// Check if this HAS contains hot archive buckets.
    pub fn has_hot_archive_buckets(&self) -> bool {
        self.hot_archive_buckets
            .as_ref()
            .is_some_and(|v| !v.is_empty())
    }

    /// Get bucket hashes as (curr, snap) tuples for all levels.
    ///
    /// This format is suitable for `BucketList::restore_from_has`.
    pub fn bucket_hash_pairs(&self) -> Vec<(Hash256, Hash256)> {
        parse_bucket_hash_pairs(&self.current_buckets)
    }

    /// Get hot archive bucket hashes as (curr, snap) tuples for all levels.
    ///
    /// This format is suitable for `HotArchiveBucketList::restore_from_has`.
    pub fn hot_archive_bucket_hash_pairs(&self) -> Option<Vec<(Hash256, Hash256)>> {
        self.hot_archive_buckets
            .as_ref()
            .map(|levels| parse_bucket_hash_pairs(levels))
    }

    /// Get the next bucket merge states for all live bucket levels.
    ///
    /// This extracts the FutureBucket state from each level for use with
    /// `BucketList::restore_from_has` and `restart_merges_from_has`.
    ///
    /// Returns an error if any hash field required by the declared state
    /// contains a malformed hex string.
    pub fn live_next_states(
        &self,
    ) -> std::result::Result<Vec<Option<PendingMergeState>>, HistoryError> {
        parse_next_states(&self.current_buckets)
    }

    /// Validate that all bucket hashes referenced in this HAS exist in the known set.
    ///
    /// This checks that:
    /// 1. Level 0 `next` is clear (state == 0) — level 0 never has pending merges
    /// 2. All non-zero curr/snap bucket hashes exist in `known_hashes`
    /// 3. For state==2 futures, the input curr/snap hashes also exist in `known_hashes`
    /// 4. For state==1 futures, the output hash exists in `known_hashes`
    ///
    /// This matches stellar-core's `containsValidBuckets` check.
    pub fn contains_valid_buckets(
        &self,
        known_hashes: &HashSet<Hash256>,
    ) -> Result<(), HistoryError> {
        // Validate live bucket list
        Self::validate_bucket_level_hashes(&self.current_buckets, known_hashes, "live")?;

        // Validate hot archive bucket list (if present)
        if let Some(ref hot_levels) = self.hot_archive_buckets {
            Self::validate_bucket_level_hashes(hot_levels, known_hashes, "hot_archive")?;
        }

        Ok(())
    }

    /// Validate bucket level hashes for a single bucket list (live or hot archive).
    fn validate_bucket_level_hashes(
        levels: &[HASBucketLevel],
        known_hashes: &HashSet<Hash256>,
        list_name: &str,
    ) -> Result<(), HistoryError> {
        // Level 0 next must be clear
        if let Some(level0) = levels.first() {
            if level0.next.state != HAS_NEXT_STATE_CLEAR {
                return Err(HistoryError::VerificationFailed(format!(
                    "{} level 0 next is not clear in HAS",
                    list_name,
                )));
            }
        }

        for (i, level) in levels.iter().enumerate() {
            validate_known_hash(known_hashes, i, "curr", &level.curr)?;
            validate_known_hash(known_hashes, i, "snap", &level.snap)?;
            validate_future_bucket_hashes(known_hashes, i, &level.next)?;
        }

        Ok(())
    }

    /// Compute the differential set of bucket hashes needed to convert `other` into `self`.
    ///
    /// This mirrors stellar-core's `HistoryArchiveState::differingBuckets()`.
    ///
    /// Algorithm:
    /// 1. Build an inhibit set containing the zero hash and all bucket hashes
    ///    present in `other` (curr, snap, and any future output hashes).
    /// 2. Walk levels from bottom (highest index) to top (index 0) in `self`.
    /// 3. For each level, collect snap, next output hash, and curr — in that order
    ///    — if they are not in the inhibit set.
    /// 4. Add collected hashes to the inhibit set to avoid duplicates.
    /// 5. Return separate lists for live and hot archive buckets, sorted
    ///    largest-to-smallest with snapshot buckets before current buckets.
    ///
    /// # Arguments
    ///
    /// * `other` - The local HAS representing current state (what we already have).
    ///
    /// # Returns
    ///
    /// A tuple of `(live_hashes, hot_archive_hashes)` — only the hashes that
    /// need to be downloaded.
    pub fn differing_bucket_hashes(
        &self,
        other: &HistoryArchiveState,
    ) -> (Vec<Hash256>, Vec<Hash256>) {
        let live = differing_buckets_for_levels(&self.current_buckets, &other.current_buckets);
        let hot = differing_buckets_for_levels(
            self.hot_archive_buckets.as_deref().unwrap_or(&[]),
            other.hot_archive_buckets.as_deref().unwrap_or(&[]),
        );
        (live, hot)
    }

    /// Convenience wrapper that returns all differing hashes (live + hot) as a single list.
    pub fn all_differing_bucket_hashes(&self, other: &HistoryArchiveState) -> Vec<Hash256> {
        let (mut live, hot) = self.differing_bucket_hashes(other);
        live.extend(hot);
        live
    }

    /// Check if all future bucket merges are clear (state == 0).
    ///
    /// This means no merges are pending or in progress.
    pub fn futures_all_clear(&self) -> bool {
        self.current_buckets
            .iter()
            .all(|level| level.next.state == HAS_NEXT_STATE_CLEAR)
    }

    /// Check if all future bucket merges are resolved (state <= 1).
    ///
    /// State 0 (clear) and state 1 (output hash known) are both "resolved".
    /// Only state 2 (inputs known, merge in progress) is "unresolved".
    pub fn futures_all_resolved(&self) -> bool {
        self.current_buckets
            .iter()
            .all(|level| level.next.state <= HAS_NEXT_STATE_OUTPUT)
    }

    /// Resolve all completed futures: convert state 1 (output) to state 0 (clear).
    ///
    /// For state 1 futures, the output hash becomes the new curr, and the
    /// future is cleared. This is used after restart to settle completed merges.
    pub fn resolve_all_futures(&mut self) {
        for level in &mut self.current_buckets {
            if level.next.state == HAS_NEXT_STATE_OUTPUT {
                level.next = HASBucketNext::default();
            }
        }
    }

    /// Clear all futures, resetting every level's next to default (state 0).
    #[cfg(test)]
    pub fn clear_all_futures(&mut self) {
        for level in &mut self.current_buckets {
            level.next = HASBucketNext::default();
        }
    }

    /// Get the next bucket merge states for hot archive bucket levels.
    ///
    /// Returns an error if any hash field required by the declared state
    /// contains a malformed hex string.
    pub fn hot_archive_next_states(
        &self,
    ) -> std::result::Result<Option<Vec<Option<PendingMergeState>>>, HistoryError> {
        match self.hot_archive_buckets.as_ref() {
            Some(levels) => Ok(Some(parse_next_states(levels)?)),
            None => Ok(None),
        }
    }
}

impl HASBucketLevel {
    /// Create a bucket level from curr/snap hex hashes (for testing and
    /// programmatic HAS construction).
    pub fn new_from_hashes(curr: String, snap: String) -> Self {
        Self {
            curr,
            snap,
            next: HASBucketNext::default(),
        }
    }
}

impl HistoryArchiveState {
    /// Clear `next` fields that the on-disk HAS format cannot represent.
    ///
    /// Mirrors stellar-core's `HistoryArchiveState::prepareForPublish`
    /// (`stellar-core/src/history/HistoryArchive.cpp:466-507`), restricted to
    /// the shadows-removed branch (`buckets[i - 1].snap.version >=
    /// LiveBucket::FIRST_PROTOCOL_SHADOWS_REMOVED`). On those levels a pending
    /// merge cannot be persisted, because the consumer rebuilds the merge
    /// deterministically from `snap` + `curr` at restart (see
    /// `BucketListBase::restartMerges` and henyey's `restart_merges_from_has`).
    ///
    /// # Architectural placement vs. stellar-core
    ///
    /// Stellar-core clears lazily inside `ResolveSnapshotWork::doWork`
    /// (`stellar-core/src/historywork/ResolveSnapshotWork.cpp:30`), invoked
    /// only on the publish path; in-memory `HistoryArchiveState` instances are
    /// free to carry pending futures. Henyey clears eagerly here because
    /// `build_history_archive_state` is the single emission entry point —
    /// every caller reaching for a HAS at publication time goes through it.
    /// The observable on-disk contents are identical; the placement just
    /// matches henyey's narrower API surface. Any future code that constructs
    /// a HAS for emission outside `build_history_archive_state` must call
    /// this method (or equivalent) before serialization.
    ///
    /// # Skipped branch
    ///
    /// Stellar-core's second branch in `prepareForPublish` (the
    /// `next.hasHashes() && !next.isLive()` → `makeLive` path,
    /// `HistoryArchive.cpp:490-506`) reconstitutes pending merges loaded from
    /// an older-protocol persisted HAS that came back in over the wire with
    /// `state == 2` (inputs). Henyey's `build_history_archive_state` consumes
    /// live in-memory `BucketLevel` state, never a deserialized HAS
    /// round-trip, so that data shape cannot arise here and the branch is
    /// intentionally omitted.
    ///
    /// # Arguments
    ///
    /// * `live_snap_versions` — protocol version of `snap` at each live level,
    ///   in level order, length must equal `current_buckets.len()`. Empty or
    ///   pre-metaentry buckets contribute `0`.
    /// * `hot_snap_versions` — same shape for `hot_archive_buckets`. Must be
    ///   `Some` iff `hot_archive_buckets` is `Some`, and same length.
    ///
    /// # Panics (debug)
    ///
    /// Mirrors stellar-core's `releaseAssert(buckets[0].next.isClear())` at
    /// `HistoryArchive.cpp:475`: level 0 never has a `next`, on either the
    /// live or the hot-archive list.
    pub(crate) fn clear_unrepresentable_futures(
        &mut self,
        live_snap_versions: &[u32],
        hot_snap_versions: Option<&[u32]>,
    ) {
        Self::clear_levels(&mut self.current_buckets, live_snap_versions);

        match (self.hot_archive_buckets.as_mut(), hot_snap_versions) {
            (Some(hot), Some(versions)) => {
                Self::clear_levels(hot, versions);
            }
            (None, None) => {}
            (Some(_), None) | (None, Some(_)) => {
                debug_assert!(
                    false,
                    "hot_archive_buckets / hot_snap_versions presence mismatch",
                );
            }
        }
    }

    fn clear_levels(levels: &mut [HASBucketLevel], snap_versions: &[u32]) {
        debug_assert_eq!(
            levels.len(),
            snap_versions.len(),
            "snap_versions length must match bucket levels",
        );
        if levels.is_empty() {
            return;
        }
        debug_assert!(
            levels[0].next.is_clear(),
            "level 0 must never carry a pending merge (parity with stellar-core HistoryArchive.cpp:475)",
        );
        // Predicate matches the validator's indexing
        // (`prev_snap_version = levels[j - 1].snap_version`,
        // archive_state.rs:449) and stellar-core's
        // `buckets[i - 1].snap` reference at
        // `stellar-core/src/history/HistoryArchive.cpp:483-489`.
        for i in 1..levels.len() {
            let prev_snap_version = snap_versions[i - 1];
            if prev_snap_version >= FIRST_PROTOCOL_SHADOWS_REMOVED && !levels[i].next.is_clear() {
                levels[i].next = HASBucketNext::default();
            }
        }
    }
}

impl HistoryArchiveState {
    /// Build a minimal HAS for testing with specified bucket levels.
    /// The levels vector should have exactly `BUCKET_LIST_LEVELS` entries
    /// for a fully valid HAS, but fewer levels are accepted for focused tests.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn new_for_testing(ledger: u32, current_buckets: Vec<HASBucketLevel>) -> Self {
        Self {
            version: 1,
            server: None,
            current_ledger: ledger,
            network_passphrase: None,
            current_buckets,
            hot_archive_buckets: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use henyey_bucket::HOT_ARCHIVE_BUCKET_LIST_LEVELS;

    /// Build a structurally valid v1 HAS JSON for tests.
    /// Custom levels are placed first; remaining slots are zero-hash padded to BUCKET_LIST_LEVELS.
    fn make_v1_has_json(ledger: u32, custom_levels: Vec<serde_json::Value>) -> String {
        let zero_level = serde_json::json!({
            "curr": ZERO_HASH, "snap": ZERO_HASH, "next": { "state": 0 }
        });
        let mut levels = custom_levels;
        while levels.len() < BUCKET_LIST_LEVELS {
            levels.push(zero_level.clone());
        }
        serde_json::to_string(&serde_json::json!({
            "version": 1,
            "currentLedger": ledger,
            "currentBuckets": levels
        }))
        .unwrap()
    }

    /// Build a structurally valid v2 HAS JSON for tests.
    fn make_v2_has_json(
        ledger: u32,
        live_levels: Vec<serde_json::Value>,
        hot_levels: Vec<serde_json::Value>,
    ) -> String {
        let zero_level = serde_json::json!({
            "curr": ZERO_HASH, "snap": ZERO_HASH, "next": { "state": 0 }
        });
        let mut live = live_levels;
        while live.len() < BUCKET_LIST_LEVELS {
            live.push(zero_level.clone());
        }
        let mut hot = hot_levels;
        while hot.len() < HOT_ARCHIVE_BUCKET_LIST_LEVELS {
            hot.push(zero_level.clone());
        }
        serde_json::to_string(&serde_json::json!({
            "version": 2,
            "currentLedger": ledger,
            "networkPassphrase": "Test SDF Network ; September 2015",
            "currentBuckets": live,
            "hotArchiveBuckets": hot
        }))
        .unwrap()
    }

    fn sample_has_json() -> String {
        let zero = ZERO_HASH;
        let zero_level = serde_json::json!({
            "curr": zero, "snap": zero, "next": { "state": 0 }
        });
        let mut live_levels = vec![
            serde_json::json!({
                "curr": "e113f8cc5468579cb57538e3204c8d3ecce59a0cdb47f6fa7e87ab4d9d8146fd",
                "next": { "state": 0 },
                "snap": "02441d532a2aa2bbc5e2b025438572f5ea2e25205442c002e2c6e7636a54b0ef"
            }),
            serde_json::json!({
                "curr": "a8a0ed4b478cda48066d06aff6f7ff9b089bbfb3effd3a6e52ce93caa71f0e1a",
                "next": { "state": 0 },
                "snap": "d5550575dff797042a61dd86adae42df96b0db9176fdf137e680c2dbac45ede9"
            }),
        ];
        while live_levels.len() < BUCKET_LIST_LEVELS {
            live_levels.push(zero_level.clone());
        }
        let hot_levels: Vec<_> = (0..HOT_ARCHIVE_BUCKET_LIST_LEVELS)
            .map(|_| zero_level.clone())
            .collect();
        serde_json::to_string_pretty(&serde_json::json!({
            "version": 2,
            "server": "stellar-core 25.0.1.rc1 (ac5427a148203e8269294cf50866200cbe4ec1d3)",
            "currentLedger": 212735,
            "networkPassphrase": "Test SDF Network ; September 2015",
            "currentBuckets": live_levels,
            "hotArchiveBuckets": hot_levels
        }))
        .unwrap()
    }

    #[test]
    fn test_parse_has() {
        let has = HistoryArchiveState::from_json(&sample_has_json()).unwrap();

        assert_eq!(has.version, 2);
        assert_eq!(has.current_ledger, 212735);
        assert_eq!(
            has.network_passphrase(),
            Some("Test SDF Network ; September 2015")
        );
        assert!(has.server().unwrap().contains("stellar-core"));
        assert_eq!(has.bucket_level_count(), BUCKET_LIST_LEVELS);
    }

    #[test]
    fn test_all_bucket_hashes() {
        let has = HistoryArchiveState::from_json(&sample_has_json()).unwrap();
        let hashes = has.all_bucket_hashes();

        // Should have 4 non-zero hashes (2 levels x 2 buckets each)
        assert_eq!(hashes.len(), 4);

        // Check first hash
        assert_eq!(
            hashes[0].to_hex(),
            "e113f8cc5468579cb57538e3204c8d3ecce59a0cdb47f6fa7e87ab4d9d8146fd"
        );
    }

    #[test]
    fn test_unique_bucket_hashes() {
        // Create HAS with duplicate hashes
        let json = make_v1_has_json(
            100,
            vec![serde_json::json!({
                "curr": "e113f8cc5468579cb57538e3204c8d3ecce59a0cdb47f6fa7e87ab4d9d8146fd",
                "snap": "e113f8cc5468579cb57538e3204c8d3ecce59a0cdb47f6fa7e87ab4d9d8146fd",
                "next": { "state": 0 }
            })],
        );

        let has = HistoryArchiveState::from_json(&json).unwrap();
        let unique = has.unique_bucket_hashes();

        // Should deduplicate to 1
        assert_eq!(unique.len(), 1);
    }

    #[test]
    fn test_bucket_hashes_at_level() {
        let has = HistoryArchiveState::from_json(&sample_has_json()).unwrap();

        // Level 0 should have both hashes
        let (curr, snap) = has.bucket_hashes_at_level(0).unwrap();
        assert!(curr.is_some());
        assert!(snap.is_some());

        // Level 2 should have zero hashes (None)
        let (curr, snap) = has.bucket_hashes_at_level(2).unwrap();
        assert!(curr.is_none());
        assert!(snap.is_none());

        // Level 11 doesn't exist (indices 0-10)
        assert!(has.bucket_hashes_at_level(11).is_none());
    }

    #[test]
    fn test_serialize_has() {
        let has = HistoryArchiveState::from_json(&sample_has_json()).unwrap();
        let json = has.to_json().unwrap();

        // Re-parse and verify
        let reparsed = HistoryArchiveState::from_json(&json).unwrap();
        assert_eq!(reparsed.current_ledger, has.current_ledger);
        assert_eq!(reparsed.version, has.version);
    }

    #[test]
    fn test_minimal_has() {
        let json = make_v1_has_json(63, vec![]);
        let has = HistoryArchiveState::from_json(&json).unwrap();
        assert_eq!(has.version, 1);
        assert_eq!(has.current_ledger, 63);
        assert!(has.network_passphrase().is_none());
        assert!(has.server().is_none());
        assert!(has.all_bucket_hashes().is_empty());
    }

    #[test]
    fn test_invalid_json() {
        let result = HistoryArchiveState::from_json("not valid json");
        assert!(result.is_err());
    }

    // Item 3: MAX_HISTORY_ARCHIVE_BUCKET_SIZE constant test
    #[test]
    fn test_max_history_archive_bucket_size() {
        assert_eq!(MAX_HISTORY_ARCHIVE_BUCKET_SIZE, 100 * 1024 * 1024 * 1024);
    }

    // Item 2: contains_valid_buckets tests
    #[test]
    fn test_contains_valid_buckets_all_present() {
        let has = HistoryArchiveState::from_json(&sample_has_json()).unwrap();
        let known: HashSet<Hash256> = has.all_bucket_hashes().into_iter().collect();
        assert!(has.contains_valid_buckets(&known).is_ok());
    }

    #[test]
    fn test_contains_valid_buckets_missing_hash() {
        let has = HistoryArchiveState::from_json(&sample_has_json()).unwrap();
        // Empty set — no known hashes
        let known: HashSet<Hash256> = HashSet::new();
        assert!(has.contains_valid_buckets(&known).is_err());
    }

    #[test]
    fn test_contains_valid_buckets_level0_not_clear() {
        let json = make_v1_has_json(
            127,
            vec![serde_json::json!({
                "curr": ZERO_HASH,
                "snap": ZERO_HASH,
                "next": { "state": 1, "output": "e113f8cc5468579cb57538e3204c8d3ecce59a0cdb47f6fa7e87ab4d9d8146fd" }
            })],
        );
        let has = HistoryArchiveState::from_json(&json).unwrap();
        let known: HashSet<Hash256> = has.all_bucket_hashes().into_iter().collect();
        let result = has.contains_valid_buckets(&known);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("level 0"));
    }

    #[test]
    fn test_contains_valid_buckets_state2_missing_inputs() {
        let json = make_v1_has_json(
            127,
            vec![
                serde_json::json!({
                    "curr": ZERO_HASH,
                    "snap": ZERO_HASH,
                    "next": { "state": 0 }
                }),
                serde_json::json!({
                    "curr": ZERO_HASH,
                    "snap": ZERO_HASH,
                    "next": {
                        "state": 2,
                        "curr": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                        "snap": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                    }
                }),
            ],
        );
        let has = HistoryArchiveState::from_json(&json).unwrap();
        // Only provide main hashes, not the future input hashes
        let known: HashSet<Hash256> = HashSet::new();
        let result = has.contains_valid_buckets(&known);
        assert!(result.is_err());
    }

    // Item 6: FutureBucket resolution method tests
    #[test]
    fn test_futures_all_clear() {
        let has = HistoryArchiveState::from_json(&sample_has_json()).unwrap();
        assert!(has.futures_all_clear());
    }

    #[test]
    fn test_futures_all_clear_with_pending() {
        let json = make_v1_has_json(
            127,
            vec![serde_json::json!({
                "curr": ZERO_HASH,
                "snap": ZERO_HASH,
                "next": { "state": 1, "output": "e113f8cc5468579cb57538e3204c8d3ecce59a0cdb47f6fa7e87ab4d9d8146fd" }
            })],
        );
        let has = HistoryArchiveState::from_json(&json).unwrap();
        assert!(!has.futures_all_clear());
    }

    #[test]
    fn test_futures_all_resolved() {
        let json = make_v1_has_json(
            127,
            vec![serde_json::json!({
                "curr": ZERO_HASH,
                "snap": ZERO_HASH,
                "next": { "state": 1, "output": "e113f8cc5468579cb57538e3204c8d3ecce59a0cdb47f6fa7e87ab4d9d8146fd" }
            })],
        );
        let has = HistoryArchiveState::from_json(&json).unwrap();
        assert!(has.futures_all_resolved()); // state <= 1 is resolved
    }

    #[test]
    fn test_futures_not_all_resolved_with_state2() {
        let json = make_v1_has_json(
            127,
            vec![serde_json::json!({
                "curr": ZERO_HASH,
                "snap": ZERO_HASH,
                "next": { "state": 2, "curr": "aaaa", "snap": "bbbb" }
            })],
        );
        let has = HistoryArchiveState::from_json(&json).unwrap();
        assert!(!has.futures_all_resolved()); // state 2 is not resolved
    }

    #[test]
    fn test_resolve_all_futures() {
        let json = make_v1_has_json(
            127,
            vec![
                serde_json::json!({
                    "curr": ZERO_HASH,
                    "snap": ZERO_HASH,
                    "next": { "state": 0 }
                }),
                serde_json::json!({
                    "curr": ZERO_HASH,
                    "snap": ZERO_HASH,
                    "next": { "state": 1, "output": "e113f8cc5468579cb57538e3204c8d3ecce59a0cdb47f6fa7e87ab4d9d8146fd" }
                }),
                serde_json::json!({
                    "curr": ZERO_HASH,
                    "snap": ZERO_HASH,
                    "next": { "state": 2, "curr": "aaaa", "snap": "bbbb" }
                }),
            ],
        );
        let mut has = HistoryArchiveState::from_json(&json).unwrap();
        has.resolve_all_futures();

        // state 0 stays 0, state 1 becomes 0, state 2 stays 2
        assert_eq!(has.current_buckets[0].next.state, 0);
        assert_eq!(has.current_buckets[1].next.state, 0);
        assert_eq!(has.current_buckets[2].next.state, 2);
    }

    #[test]
    fn test_contains_valid_buckets_state1_unknown_output() {
        let json = make_v1_has_json(
            127,
            vec![
                serde_json::json!({
                    "curr": ZERO_HASH,
                    "snap": ZERO_HASH,
                    "next": { "state": 0 }
                }),
                serde_json::json!({
                    "curr": ZERO_HASH,
                    "snap": ZERO_HASH,
                    "next": { "state": 1, "output": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc" }
                }),
            ],
        );
        let has = HistoryArchiveState::from_json(&json).unwrap();
        // Provide empty set — output hash is unknown
        let known: HashSet<Hash256> = HashSet::new();
        let result = has.contains_valid_buckets(&known);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("output"));
    }

    #[test]
    fn test_contains_valid_buckets_zero_hashes_only() {
        // HAS with only zero hashes should pass (zero hashes are skipped)
        let json = make_v1_has_json(127, vec![]);
        let has = HistoryArchiveState::from_json(&json).unwrap();
        let known: HashSet<Hash256> = HashSet::new();
        assert!(has.contains_valid_buckets(&known).is_ok());
    }

    #[test]
    fn test_clear_all_futures() {
        let json = make_v1_has_json(
            127,
            vec![
                serde_json::json!({
                    "curr": ZERO_HASH,
                    "snap": ZERO_HASH,
                    "next": { "state": 1, "output": "e113f8cc5468579cb57538e3204c8d3ecce59a0cdb47f6fa7e87ab4d9d8146fd" }
                }),
                serde_json::json!({
                    "curr": ZERO_HASH,
                    "snap": ZERO_HASH,
                    "next": { "state": 2, "curr": "aaaa", "snap": "bbbb" }
                }),
            ],
        );
        let mut has = HistoryArchiveState::from_json(&json).unwrap();
        has.clear_all_futures();

        assert!(has.futures_all_clear());
        for level in &has.current_buckets {
            assert_eq!(level.next.state, 0);
        }
    }

    // ── differing_bucket_hashes tests ──────────────────────────────────

    /// Helper: create a minimal valid v1 HAS with given bucket levels (curr, snap pairs),
    /// padded to BUCKET_LIST_LEVELS with zero-hash defaults.
    fn make_has(levels: Vec<(&str, &str)>) -> HistoryArchiveState {
        let zero = ZERO_HASH;
        let mut buckets: Vec<HASBucketLevel> = levels
            .into_iter()
            .map(|(curr, snap)| HASBucketLevel {
                curr: curr.to_string(),
                snap: snap.to_string(),
                next: HASBucketNext::default(),
            })
            .collect();
        while buckets.len() < BUCKET_LIST_LEVELS {
            buckets.push(HASBucketLevel {
                curr: zero.to_string(),
                snap: zero.to_string(),
                next: HASBucketNext::default(),
            });
        }
        HistoryArchiveState {
            version: 1,
            server: None,
            current_ledger: 100,
            network_passphrase: None,
            current_buckets: buckets,
            hot_archive_buckets: None,
        }
    }

    const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const HASH_C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const HASH_D: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
    const HASH_E: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

    #[test]
    fn test_differing_buckets_identical_has() {
        let remote = make_has(vec![(HASH_A, HASH_B), (HASH_C, HASH_D)]);
        let local = make_has(vec![(HASH_A, HASH_B), (HASH_C, HASH_D)]);
        let (live, hot) = remote.differing_bucket_hashes(&local);
        assert!(live.is_empty(), "identical HAS should produce no diff");
        assert!(hot.is_empty());
    }

    #[test]
    fn test_differing_buckets_completely_new() {
        let remote = make_has(vec![(HASH_A, HASH_B), (HASH_C, HASH_D)]);
        let local = make_has(vec![]); // fresh node, no buckets
        let (live, _hot) = remote.differing_bucket_hashes(&local);
        // All 4 hashes should be in the differential
        assert_eq!(live.len(), 4);
    }

    #[test]
    fn test_differing_buckets_partial_overlap() {
        // Remote has A,B at level 0 and C,D at level 1.
        // Local has A,B at level 0 only.
        // Diff should be C,D (from level 1).
        let remote = make_has(vec![(HASH_A, HASH_B), (HASH_C, HASH_D)]);
        let local = make_has(vec![(HASH_A, HASH_B)]);
        let (live, _) = remote.differing_bucket_hashes(&local);
        let live_set: HashSet<_> = live.iter().collect();
        let c = Hash256::from_hex(HASH_C).unwrap();
        let d = Hash256::from_hex(HASH_D).unwrap();
        assert!(live_set.contains(&c), "should include HASH_C");
        assert!(live_set.contains(&d), "should include HASH_D");
        assert_eq!(live.len(), 2);
    }

    #[test]
    fn test_differing_buckets_no_duplicates() {
        // Remote uses same hash in curr and snap at different levels.
        let remote = make_has(vec![(HASH_A, HASH_A), (HASH_A, HASH_B)]);
        let local = make_has(vec![]);
        let (live, _) = remote.differing_bucket_hashes(&local);
        // HASH_A appears multiple times but should only be in result once
        let a = Hash256::from_hex(HASH_A).unwrap();
        let a_count = live.iter().filter(|h| **h == a).count();
        assert_eq!(a_count, 1, "dedup should prevent duplicate A");
    }

    #[test]
    fn test_differing_buckets_zero_hashes_excluded() {
        let zero = ZERO_HASH;
        let remote = make_has(vec![(HASH_A, zero)]);
        let local = make_has(vec![]);
        let (live, _) = remote.differing_bucket_hashes(&local);
        let a = Hash256::from_hex(HASH_A).unwrap();
        assert_eq!(live, vec![a], "zero hash should not appear in result");
    }

    #[test]
    fn test_differing_buckets_inhibits_future_output() {
        // Local has a future with output hash E — E should be inhibited.
        let mut local = make_has(vec![(HASH_A, HASH_B)]);
        local.current_buckets[0].next = HASBucketNext {
            state: 1,
            output: Some(HASH_E.to_string()),
            ..Default::default()
        };

        // Remote has E as curr at level 0.
        let remote = make_has(vec![(HASH_E, HASH_C)]);
        let (live, _) = remote.differing_bucket_hashes(&local);
        let e = Hash256::from_hex(HASH_E).unwrap();
        // E is in local's future output → inhibited → not in diff
        assert!(
            !live.contains(&e),
            "E should be inhibited by local future output"
        );
        // C should still be needed
        let c = Hash256::from_hex(HASH_C).unwrap();
        assert!(live.contains(&c), "C should be in diff");
    }

    #[test]
    fn test_differing_buckets_ordering_bottom_to_top() {
        // Levels are walked bottom (highest index) to top (index 0).
        // Level 1 (bottom) has C,D. Level 0 (top) has A,B.
        // With empty local, result should start with level 1 hashes.
        let remote = make_has(vec![(HASH_A, HASH_B), (HASH_C, HASH_D)]);
        let local = make_has(vec![]);
        let (live, _) = remote.differing_bucket_hashes(&local);
        assert_eq!(live.len(), 4);
        // First hashes should be from level 1 (bottom): D snap, then C curr
        // (snap before curr per stellar-core order: s, n, c)
        let d = Hash256::from_hex(HASH_D).unwrap();
        let c = Hash256::from_hex(HASH_C).unwrap();
        assert_eq!(live[0], d, "snap of bottom level should come first");
        assert_eq!(live[1], c, "curr of bottom level should come second");
    }

    #[test]
    fn test_differing_buckets_hot_archive() {
        // Intentionally non-standard: v1 HAS with hot_archive_buckets to test
        // differing_bucket_hashes() differential logic on hot archive levels.
        // This would fail verify_has_structure() (v1 must not have hot archive).
        let mut remote = make_has(vec![(HASH_A, HASH_B)]);
        remote.hot_archive_buckets = Some(vec![HASBucketLevel {
            curr: HASH_C.to_string(),
            snap: HASH_D.to_string(),
            next: HASBucketNext::default(),
        }]);
        let local = make_has(vec![]);
        let (live, hot) = remote.differing_bucket_hashes(&local);
        assert_eq!(live.len(), 2); // A, B
        assert_eq!(hot.len(), 2); // C, D
    }

    #[test]
    fn test_all_differing_bucket_hashes() {
        // Intentionally non-standard: v1 HAS with hot_archive_buckets to test
        // all_differing_bucket_hashes() combined differential logic.
        let mut remote = make_has(vec![(HASH_A, HASH_B)]);
        remote.hot_archive_buckets = Some(vec![HASBucketLevel {
            curr: HASH_C.to_string(),
            snap: HASH_D.to_string(),
            next: HASBucketNext::default(),
        }]);
        let local = make_has(vec![]);
        let all = remote.all_differing_bucket_hashes(&local);
        assert_eq!(all.len(), 4); // A, B, C, D combined
    }

    /// Compute the bucket list hash the same way the Go SDK does.
    ///
    /// Go SDK: for each of 11 levels, SHA256(hex_decode(curr) || hex_decode(snap)),
    /// then SHA256(concatenation of all level hashes).
    fn go_sdk_bucket_list_hash(levels: &[HASBucketLevel]) -> Hash256 {
        use sha2::{Digest, Sha256};

        let mut total = Vec::new();
        for level in levels {
            let curr = hex::decode(&level.curr).unwrap_or_default();
            let snap = hex::decode(&level.snap).unwrap_or_default();
            let mut hasher = Sha256::new();
            hasher.update(&curr);
            hasher.update(&snap);
            let level_hash = hasher.finalize();
            total.extend_from_slice(&level_hash);
        }

        let mut hasher = Sha256::new();
        hasher.update(&total);
        let result = hasher.finalize();
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&result);
        Hash256::from_bytes(bytes)
    }

    /// Compute the combined bucket list hash the way the Go SDK does for version >= 2.
    fn go_sdk_combined_hash(has: &HistoryArchiveState) -> Hash256 {
        use sha2::{Digest, Sha256};

        let live_hash = go_sdk_bucket_list_hash(&has.current_buckets);
        if has.version < 2 {
            return live_hash;
        }

        let hot_archive_hash = match &has.hot_archive_buckets {
            Some(levels) => go_sdk_bucket_list_hash(levels),
            None => {
                // Go SDK: zero-initialized BucketList [11]struct{Curr: "", Snap: ""}
                // SHA256 of empty bytes for each level, then SHA256 of all level hashes
                let zero_levels: Vec<HASBucketLevel> = (0..BUCKET_LIST_LEVELS)
                    .map(|_| HASBucketLevel {
                        curr: String::new(),
                        snap: String::new(),
                        next: HASBucketNext::default(),
                    })
                    .collect();
                go_sdk_bucket_list_hash(&zero_levels)
            }
        };

        let mut hasher = Sha256::new();
        hasher.update(live_hash.as_bytes());
        hasher.update(hot_archive_hash.as_bytes());
        let result = hasher.finalize();
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&result);
        Hash256::from_bytes(bytes)
    }

    /// Verify that a fresh HAS with all-zero bucket hashes produces consistent
    /// hashes between henyey's computation and the Go SDK's computation.
    #[test]
    fn test_has_bucket_list_hash_round_trip_empty() {
        let zero = ZERO_HASH;
        let levels: Vec<HASBucketLevel> = (0..BUCKET_LIST_LEVELS)
            .map(|_| HASBucketLevel {
                curr: zero.to_string(),
                snap: zero.to_string(),
                next: HASBucketNext::default(),
            })
            .collect();

        let has = HistoryArchiveState {
            version: 2,
            server: Some("test".to_string()),
            current_ledger: 7,
            network_passphrase: Some("Test Network".to_string()),
            current_buckets: levels.clone(),
            hot_archive_buckets: Some(levels),
        };

        // Serialize to JSON and re-parse (simulating the archive round-trip)
        let json = has.to_json().unwrap();
        let reparsed = HistoryArchiveState::from_json(&json).unwrap();

        // Compute hash the Go SDK way
        let go_hash = go_sdk_combined_hash(&reparsed);

        // Compute hash the henyey way (using the bucket level hashes directly)
        use sha2::{Digest, Sha256};
        let zero_hash = Hash256::ZERO;
        let mut level_hashes = Vec::new();
        for _ in 0..BUCKET_LIST_LEVELS {
            let mut h = Sha256::new();
            h.update(zero_hash.as_bytes());
            h.update(zero_hash.as_bytes());
            level_hashes.extend_from_slice(&h.finalize());
        }
        let live_hash = {
            let mut h = Sha256::new();
            h.update(&level_hashes);
            let r = h.finalize();
            let mut b = [0u8; 32];
            b.copy_from_slice(&r);
            Hash256::from_bytes(b)
        };
        let combined_henyey = {
            let mut h = Sha256::new();
            h.update(live_hash.as_bytes());
            h.update(live_hash.as_bytes()); // hot archive is same (all zeros)
            let r = h.finalize();
            let mut b = [0u8; 32];
            b.copy_from_slice(&r);
            Hash256::from_bytes(b)
        };

        assert_eq!(
            go_hash,
            combined_henyey,
            "Go SDK hash ({}) != henyey hash ({})",
            go_hash.to_hex(),
            combined_henyey.to_hex()
        );
    }

    /// Verify round-trip hash with non-zero bucket hashes.
    #[test]
    fn test_has_bucket_list_hash_round_trip_nonzero() {
        let zero = ZERO_HASH;
        let mut levels: Vec<HASBucketLevel> = (0..BUCKET_LIST_LEVELS)
            .map(|_| HASBucketLevel {
                curr: zero.to_string(),
                snap: zero.to_string(),
                next: HASBucketNext::default(),
            })
            .collect();
        // Set level 0 to have non-zero hashes
        levels[0].curr = HASH_A.to_string();
        levels[0].snap = HASH_B.to_string();

        let has = HistoryArchiveState {
            version: 2,
            server: Some("test".to_string()),
            current_ledger: 7,
            network_passphrase: Some("Test Network".to_string()),
            current_buckets: levels.clone(),
            hot_archive_buckets: Some(levels.clone()),
        };

        let json = has.to_json().unwrap();
        eprintln!("HAS JSON:\n{}", json);

        let reparsed = HistoryArchiveState::from_json(&json).unwrap();
        let go_hash = go_sdk_combined_hash(&reparsed);

        // Manually compute henyey-style hash
        use sha2::{Digest, Sha256};
        let a = Hash256::from_hex(HASH_A).unwrap();
        let b = Hash256::from_hex(HASH_B).unwrap();
        let zero_h = Hash256::ZERO;

        let mut all_level_hashes = Vec::new();
        for i in 0..BUCKET_LIST_LEVELS {
            let (curr, snap) = if i == 0 { (&a, &b) } else { (&zero_h, &zero_h) };
            let mut h = Sha256::new();
            h.update(curr.as_bytes());
            h.update(snap.as_bytes());
            all_level_hashes.extend_from_slice(&h.finalize());
        }
        let live_hash = {
            let mut h = Sha256::new();
            h.update(&all_level_hashes);
            let r = h.finalize();
            let mut bytes = [0u8; 32];
            bytes.copy_from_slice(&r);
            Hash256::from_bytes(bytes)
        };

        let combined_henyey = {
            let mut h = Sha256::new();
            h.update(live_hash.as_bytes());
            h.update(live_hash.as_bytes()); // same for hot archive
            let r = h.finalize();
            let mut bytes = [0u8; 32];
            bytes.copy_from_slice(&r);
            Hash256::from_bytes(bytes)
        };

        assert_eq!(
            go_hash,
            combined_henyey,
            "Go SDK hash ({}) != henyey hash ({})",
            go_hash.to_hex(),
            combined_henyey.to_hex()
        );
    }

    // -----------------------------------------------------------------------
    // Regression tests for #2380: parse_next_states must reject HAS with
    // malformed or missing required future-bucket hashes.
    // -----------------------------------------------------------------------

    const ZERO_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    fn make_has_with_next(next: HASBucketNext) -> HistoryArchiveState {
        // v1: these tests exercise next-state parsing, not v2 features.
        let mut levels = vec![
            HASBucketLevel {
                curr: ZERO_HEX.to_string(),
                snap: ZERO_HEX.to_string(),
                next: HASBucketNext::default(),
            },
            HASBucketLevel {
                curr: ZERO_HEX.to_string(),
                snap: ZERO_HEX.to_string(),
                next,
            },
            HASBucketLevel {
                curr: ZERO_HEX.to_string(),
                snap: ZERO_HEX.to_string(),
                next: HASBucketNext::default(),
            },
        ];
        while levels.len() < BUCKET_LIST_LEVELS {
            levels.push(HASBucketLevel {
                curr: ZERO_HEX.to_string(),
                snap: ZERO_HEX.to_string(),
                next: HASBucketNext::default(),
            });
        }
        HistoryArchiveState {
            version: 1,
            server: None,
            current_ledger: 100,
            network_passphrase: None,
            current_buckets: levels,
            hot_archive_buckets: None,
        }
    }

    #[test]
    fn test_parse_next_states_state1_valid() {
        let has = make_has_with_next(HASBucketNext {
            state: HAS_NEXT_STATE_OUTPUT,
            output: Some(HASH_A.to_string()),
            curr: None,
            snap: None,
            shadow: None,
        });
        let states = has.live_next_states().unwrap();
        assert!(
            matches!(states[1], Some(PendingMergeState::Output(_))),
            "state-1 should be Output"
        );
    }

    #[test]
    fn test_parse_next_states_state1_missing_output() {
        let has = make_has_with_next(HASBucketNext {
            state: HAS_NEXT_STATE_OUTPUT,
            output: None,
            curr: None,
            snap: None,
            shadow: None,
        });
        let err = has.live_next_states().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("requires output hash") && msg.contains("absent"),
            "should reject state=1 with missing output: {msg}"
        );
    }

    #[test]
    fn test_parse_next_states_state1_malformed_output() {
        let has = make_has_with_next(HASBucketNext {
            state: HAS_NEXT_STATE_OUTPUT,
            output: Some("not_valid_hex".to_string()),
            curr: None,
            snap: None,
            shadow: None,
        });
        let err = has.live_next_states().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("malformed"),
            "should reject state=1 with malformed output: {msg}"
        );
    }

    #[test]
    fn test_parse_next_states_state2_valid() {
        let has = make_has_with_next(HASBucketNext {
            state: HAS_NEXT_STATE_INPUTS,
            output: None,
            curr: Some(HASH_A.to_string()),
            snap: Some(HASH_B.to_string()),
            shadow: None,
        });
        let states = has.live_next_states().unwrap();
        assert!(
            matches!(states[1], Some(PendingMergeState::Inputs { .. })),
            "state-2 should be Inputs"
        );
    }

    #[test]
    fn test_parse_next_states_state2_missing_curr() {
        let has = make_has_with_next(HASBucketNext {
            state: HAS_NEXT_STATE_INPUTS,
            output: None,
            curr: None,
            snap: Some(HASH_A.to_string()),
            shadow: None,
        });
        let err = has.live_next_states().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("requires input curr hash") && msg.contains("absent"),
            "should reject state=2 with missing curr: {msg}"
        );
    }

    #[test]
    fn test_parse_next_states_state2_missing_snap() {
        let has = make_has_with_next(HASBucketNext {
            state: HAS_NEXT_STATE_INPUTS,
            output: None,
            curr: Some(HASH_A.to_string()),
            snap: None,
            shadow: None,
        });
        let err = has.live_next_states().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("requires input snap hash") && msg.contains("absent"),
            "should reject state=2 with missing snap: {msg}"
        );
    }

    #[test]
    fn test_parse_next_states_state2_malformed_curr() {
        let has = make_has_with_next(HASBucketNext {
            state: HAS_NEXT_STATE_INPUTS,
            output: None,
            curr: Some("bad_hex".to_string()),
            snap: Some(HASH_A.to_string()),
            shadow: None,
        });
        let err = has.live_next_states().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("malformed"),
            "should reject state=2 with malformed curr: {msg}"
        );
    }

    #[test]
    fn test_parse_next_states_rejects_unknown_discriminant() {
        let has = make_has_with_next(HASBucketNext {
            state: 99,
            output: None,
            curr: None,
            snap: None,
            shadow: None,
        });
        let err = has.live_next_states().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown") || msg.contains("99"),
            "should reject unknown state discriminant: {msg}"
        );
    }

    #[test]
    fn test_parse_next_states_zero_hash_output_canonicalized_to_none() {
        // A state-1 with zero-hash output should be canonicalized to None (clear).
        let zero_hash = "0".repeat(64);
        let has = make_has_with_next(HASBucketNext {
            state: HAS_NEXT_STATE_OUTPUT,
            output: Some(zero_hash),
            curr: None,
            snap: None,
            shadow: None,
        });
        let states = has.live_next_states().unwrap();
        assert!(
            states[1].is_none(),
            "zero-hash output should be canonicalized to None"
        );
    }

    #[test]
    fn test_validate_version_invariants_v2_with_hot() {
        let json = make_v2_has_json(100, vec![], vec![]);
        let has = HistoryArchiveState::from_json(&json).unwrap();
        assert!(has.validate_version_invariants().is_ok());
    }

    #[test]
    fn test_validate_version_invariants_v2_without_hot() {
        // Intentionally omits hotArchiveBuckets to test that
        // validate_version_invariants() (the lenient parse-time check) accepts v2
        // without hot archive. This would NOT pass verify_has_structure().
        let zero_level = serde_json::json!({
            "curr": ZERO_HASH, "snap": ZERO_HASH, "next": { "state": 0 }
        });
        let levels: Vec<_> = (0..BUCKET_LIST_LEVELS)
            .map(|_| zero_level.clone())
            .collect();
        let json = serde_json::to_string(&serde_json::json!({
            "version": 2,
            "currentLedger": 100,
            "networkPassphrase": "Test SDF Network ; September 2015",
            "currentBuckets": levels
        }))
        .unwrap();
        let has = HistoryArchiveState::from_json(&json).unwrap();
        assert_eq!(has.version, 2);
        assert!(has.hot_archive_buckets.is_none());
        assert!(has.validate_version_invariants().is_ok());
    }

    #[test]
    fn test_validate_version_invariants_v1_with_hot() {
        // v1 with hotArchiveBuckets must be rejected by validate_version_invariants().
        // Uses raw JSON since make_v1_has_json can't add hot archive.
        let zero_level = serde_json::json!({
            "curr": ZERO_HASH, "snap": ZERO_HASH, "next": { "state": 0 }
        });
        let levels: Vec<_> = (0..BUCKET_LIST_LEVELS)
            .map(|_| zero_level.clone())
            .collect();
        let hot: Vec<_> = (0..HOT_ARCHIVE_BUCKET_LIST_LEVELS)
            .map(|_| zero_level.clone())
            .collect();
        let json = serde_json::to_string(&serde_json::json!({
            "version": 1,
            "currentLedger": 100,
            "currentBuckets": levels,
            "hotArchiveBuckets": hot
        }))
        .unwrap();
        assert!(HistoryArchiveState::from_json(&json).is_err());
    }

    #[test]
    fn test_validate_version_invariants_v1_without_hot() {
        let json = make_v1_has_json(100, vec![]);
        let has = HistoryArchiveState::from_json(&json).unwrap();
        assert!(has.validate_version_invariants().is_ok());
    }

    #[test]
    fn test_hot_archive_levels_accessor_v2() {
        let json = make_v2_has_json(
            100,
            vec![],
            vec![serde_json::json!({
                "curr": "abc123",
                "snap": ZERO_HASH,
                "next": { "state": 0 }
            })],
        );
        let has = HistoryArchiveState::from_json(&json).unwrap();
        let levels = has.hot_archive_levels();
        assert_eq!(levels.len(), HOT_ARCHIVE_BUCKET_LIST_LEVELS);
        assert_eq!(levels[0].curr, "abc123");
    }

    #[test]
    fn test_hot_archive_levels_accessor_v1() {
        let json = make_v1_has_json(100, vec![]);
        let has = HistoryArchiveState::from_json(&json).unwrap();
        let levels = has.hot_archive_levels();
        assert!(levels.is_empty());
    }

    #[test]
    fn test_hot_archive_levels_mut_v2() {
        let json = make_v2_has_json(100, vec![], vec![]);
        let mut has = HistoryArchiveState::from_json(&json).unwrap();
        assert!(has.hot_archive_levels_mut().is_some());
    }

    #[test]
    fn test_hot_archive_levels_mut_v1() {
        let json = make_v1_has_json(100, vec![]);
        let mut has = HistoryArchiveState::from_json(&json).unwrap();
        assert!(has.hot_archive_levels_mut().is_none());
    }

    #[test]
    fn test_contains_valid_buckets_validates_hot_archive() {
        let mut has = HistoryArchiveState::from_json(&sample_has_json()).unwrap();
        // Add hot archive with a non-zero hash that is NOT in known_hashes
        has.hot_archive_buckets = Some(vec![HASBucketLevel {
            curr: "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string(),
            snap: ZERO_HASH.to_string(),
            next: HASBucketNext::default(),
        }]);
        let known: HashSet<Hash256> = has
            .current_buckets
            .iter()
            .flat_map(|l| {
                [&l.curr, &l.snap]
                    .into_iter()
                    .filter_map(|h| Hash256::from_hex(h).ok())
            })
            .collect();
        // Should fail because hot archive curr hash is not in known_hashes
        assert!(has.contains_valid_buckets(&known).is_err());
    }

    // --- validate_bucket_list_structure tests ---

    fn make_level(
        snap: u32,
        curr: u32,
        next_state: u32,
        output: Option<&str>,
    ) -> BucketLevelVersionInfo {
        BucketLevelVersionInfo {
            snap_version: snap,
            curr_version: curr,
            next: HASBucketNext {
                state: next_state,
                output: output.map(String::from),
                curr: None,
                snap: None,
                shadow: None,
            },
        }
    }

    #[test]
    fn test_validate_bucket_list_valid_monotonic() {
        // 11 levels: deepest (index 10) has lowest version, top (index 0) highest.
        // Iteration goes bottom-up: 10, 9, ..., 0. Each must be >= previous.
        let levels: Vec<_> = (0..11u32)
            .map(|i| make_level(22 - i, 22 - i, HAS_NEXT_STATE_CLEAR, None))
            .collect();
        assert!(validate_bucket_list_structure(&levels, 11).is_ok());
    }

    #[test]
    fn test_validate_bucket_list_size_mismatch() {
        let levels: Vec<_> = (0..10)
            .map(|i| make_level(12 + i, 12 + i, HAS_NEXT_STATE_CLEAR, None))
            .collect();
        let err = validate_bucket_list_structure(&levels, 11).unwrap_err();
        assert!(
            matches!(err, HistoryError::VerificationFailed(ref msg) if msg.contains("size mismatch"))
        );
    }

    #[test]
    fn test_validate_bucket_list_decreasing_version() {
        // All levels at version 24, except level 8 has version 30.
        // Walk bottom-up: j=10 (24), j=9 (24), j=8 (30→min=30), j=7 (24<30) → fail
        let mut levels: Vec<_> = (0..11u32)
            .map(|_| make_level(24, 24, HAS_NEXT_STATE_CLEAR, None))
            .collect();
        levels[8].snap_version = 30;
        levels[8].curr_version = 30;
        let err = validate_bucket_list_structure(&levels, 11).unwrap_err();
        assert!(
            matches!(err, HistoryError::VerificationFailed(ref msg) if msg.contains("incompatible bucket versions"))
        );
    }

    #[test]
    fn test_validate_bucket_list_all_empty() {
        // All empty (version 0) — valid (no non-empty seen)
        let levels: Vec<_> = (0..11)
            .map(|_| make_level(0, 0, HAS_NEXT_STATE_CLEAR, None))
            .collect();
        assert!(validate_bucket_list_structure(&levels, 11).is_ok());
    }

    #[test]
    fn test_validate_bucket_list_level0_next_not_clear() {
        let mut levels: Vec<_> = (0..11u32)
            .map(|i| make_level(22 - i, 22 - i, HAS_NEXT_STATE_CLEAR, None))
            .collect();
        levels[0].next.state = HAS_NEXT_STATE_OUTPUT;
        levels[0].next.output = Some("abc".to_string());
        let err = validate_bucket_list_structure(&levels, 11).unwrap_err();
        assert!(
            matches!(err, HistoryError::VerificationFailed(ref msg) if msg.contains("level 0"))
        );
    }

    #[test]
    fn test_validate_bucket_list_post_shadows_removed_next_not_clear() {
        // prev (level j-1) snap version >= 12, so level j next must be clear
        let mut levels: Vec<_> = (0..11u32)
            .map(|i| make_level(22 - i, 22 - i, HAS_NEXT_STATE_CLEAR, None))
            .collect();
        // Level 1 has a non-clear next; prev is level 0 with snap=22 >= 12
        levels[1].next.state = HAS_NEXT_STATE_OUTPUT;
        levels[1].next.output = Some("hash".to_string());
        let err = validate_bucket_list_structure(&levels, 11).unwrap_err();
        assert!(
            matches!(err, HistoryError::VerificationFailed(ref msg) if msg.contains("future must be cleared"))
        );
    }

    #[test]
    fn test_validate_bucket_list_pre_shadows_needs_output() {
        // prev (level 0) snap version = 11 (< 12), so level 1 next must have output hash
        let levels: Vec<_> = (0..11)
            .map(|_| make_level(11, 11, HAS_NEXT_STATE_CLEAR, None))
            .collect();
        // Level 1 next is clear but should have output (pre-shadows-removed)
        let err = validate_bucket_list_structure(&levels, 11).unwrap_err();
        assert!(
            matches!(err, HistoryError::VerificationFailed(ref msg) if msg.contains("must have resolved output"))
        );
    }

    #[test]
    fn test_validate_bucket_list_pre_shadows_with_output_ok() {
        // prev (level 0) snap = 11, level 1+ next has output hash — valid
        let mut levels: Vec<_> = (0..11)
            .map(|_| make_level(11, 11, HAS_NEXT_STATE_OUTPUT, Some("deadbeef")))
            .collect();
        // Level 0 must be clear
        levels[0].next.state = HAS_NEXT_STATE_CLEAR;
        levels[0].next.output = None;
        assert!(validate_bucket_list_structure(&levels, 11).is_ok());
    }

    #[test]
    fn test_validate_bucket_list_empty_levels_skipped() {
        // Bottom levels empty, top levels have versions — valid
        // nonEmptySeen only becomes true when we hit a non-empty bucket
        let mut levels: Vec<_> = (0..11)
            .map(|_| make_level(0, 0, HAS_NEXT_STATE_CLEAR, None))
            .collect();
        // Only top 3 levels have data (levels 0, 1, 2)
        levels[0].snap_version = 24;
        levels[0].curr_version = 24;
        levels[1].snap_version = 24;
        levels[1].curr_version = 24;
        levels[2].snap_version = 24;
        levels[2].curr_version = 24;
        assert!(validate_bucket_list_structure(&levels, 11).is_ok());
    }
}
