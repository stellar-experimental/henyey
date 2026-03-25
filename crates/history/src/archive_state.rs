//! History Archive State (HAS) parsing and handling.
//!
//! The History Archive State is a JSON file that describes the current state
//! of a Stellar history archive, including the current ledger and bucket list hashes.

use henyey_common::Hash256;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::error::HistoryError;

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
fn collect_bucket_hashes(levels: &[HASBucketLevel], out: &mut Vec<Hash256>) {
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
        if level.next.state == 2 {
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
fn parse_next_states(levels: &[HASBucketLevel]) -> Vec<LiveBucketNextState> {
    levels
        .iter()
        .map(|level| LiveBucketNextState {
            state: level.next.state,
            output: level
                .next
                .output
                .as_ref()
                .and_then(|h| Hash256::from_hex(h).ok()),
            input_curr: level
                .next
                .curr
                .as_ref()
                .and_then(|h| Hash256::from_hex(h).ok()),
            input_snap: level
                .next
                .snap
                .as_ref()
                .and_then(|h| Hash256::from_hex(h).ok()),
        })
        .collect()
}

/// Parse bucket hashes at a specific level, returning (curr, snap) as Options.
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
        1 => {
            if let Some(output) = next.output.as_deref() {
                validate_known_hash(known_hashes, level, "output", output)?;
            }
        }
        2 => {
            if let Some(curr) = next.curr.as_deref() {
                validate_known_hash(known_hashes, level, "input curr", curr)?;
            }
            if let Some(snap) = next.snap.as_deref() {
                validate_known_hash(known_hashes, level, "input snap", snap)?;
            }
        }
        _ => {}
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
    #[serde(default)]
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
        self.state == 1 && self.output.is_some()
    }
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
    /// let json = r#"{
    ///     "version": 2,
    ///     "server": "stellar-core 25.0.1",
    ///     "currentLedger": 12345,
    ///     "networkPassphrase": "Test SDF Network ; September 2015",
    ///     "currentBuckets": [
    ///         {
    ///             "curr": "e113f8cc5468579cb57538e3204c8d3ecce59a0cdb47f6fa7e87ab4d9d8146fd",
    ///             "snap": "0000000000000000000000000000000000000000000000000000000000000000",
    ///             "next": { "state": 0 }
    ///         }
    ///     ]
    /// }"#;
    ///
    /// let has = HistoryArchiveState::from_json(json).unwrap();
    /// assert_eq!(has.current_ledger, 12345);
    /// ```
    pub fn from_json(json: &str) -> Result<Self, HistoryError> {
        serde_json::from_str(json).map_err(HistoryError::Json)
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
        let mut hashes = Vec::new();
        collect_bucket_hashes(&self.current_buckets, &mut hashes);
        if let Some(ref hot_buckets) = self.hot_archive_buckets {
            collect_bucket_hashes(hot_buckets, &mut hashes);
        }
        hashes
    }

    /// Get the unique bucket hashes (deduplicated).
    ///
    /// This is useful when you want to avoid downloading the same bucket twice.
    pub fn unique_bucket_hashes(&self) -> Vec<Hash256> {
        let mut hashes = self.all_bucket_hashes();
        hashes.sort_by(|a, b| a.0.cmp(&b.0));
        hashes.dedup();
        hashes
    }

    /// Get the network passphrase if available.
    pub fn network_passphrase(&self) -> Option<&str> {
        self.network_passphrase.as_deref()
    }

    /// Get the server version string if available.
    pub fn server(&self) -> Option<&str> {
        self.server.as_deref()
    }

    /// Get the number of bucket levels.
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
    pub fn bucket_hashes_at_level(
        &self,
        level: usize,
    ) -> Option<(Option<Hash256>, Option<Hash256>)> {
        self.current_buckets.get(level).map(parse_level_hashes)
    }

    /// Get hot archive bucket hashes for a specific level.
    ///
    /// # Arguments
    ///
    /// * `level` - The bucket level index (0 = most frequently updated)
    ///
    /// # Returns
    ///
    /// A tuple of (current_hash, snapshot_hash) if the level exists.
    pub fn hot_archive_bucket_hashes_at_level(
        &self,
        level: usize,
    ) -> Option<(Option<Hash256>, Option<Hash256>)> {
        let hot_buckets = self.hot_archive_buckets.as_ref()?;
        hot_buckets.get(level).map(parse_level_hashes)
    }

    /// Check if this HAS contains hot archive buckets.
    pub fn has_hot_archive_buckets(&self) -> bool {
        self.hot_archive_buckets
            .as_ref()
            .is_some_and(|v| !v.is_empty())
    }

    /// Get the number of hot archive bucket levels.
    pub fn hot_archive_bucket_level_count(&self) -> usize {
        self.hot_archive_buckets.as_ref().map_or(0, |v| v.len())
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
    pub fn live_next_states(&self) -> Vec<LiveBucketNextState> {
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
        // Level 0 next must be clear
        if let Some(level0) = self.current_buckets.first() {
            if level0.next.state != 0 {
                return Err(HistoryError::VerificationFailed(
                    "level 0 next is not clear in HAS".to_string(),
                ));
            }
        }

        for (i, level) in self.current_buckets.iter().enumerate() {
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
            .all(|level| level.next.state == 0)
    }

    /// Check if all future bucket merges are resolved (state <= 1).
    ///
    /// State 0 (clear) and state 1 (output hash known) are both "resolved".
    /// Only state 2 (inputs known, merge in progress) is "unresolved".
    pub fn futures_all_resolved(&self) -> bool {
        self.current_buckets
            .iter()
            .all(|level| level.next.state <= 1)
    }

    /// Resolve all completed futures: convert state 1 (output) to state 0 (clear).
    ///
    /// For state 1 futures, the output hash becomes the new curr, and the
    /// future is cleared. This is used after restart to settle completed merges.
    pub fn resolve_all_futures(&mut self) {
        for level in &mut self.current_buckets {
            if level.next.state == 1 {
                level.next = HASBucketNext::default();
            }
        }
    }

    /// Clear all futures, resetting every level's next to default (state 0).
    pub fn clear_all_futures(&mut self) {
        for level in &mut self.current_buckets {
            level.next = HASBucketNext::default();
        }
    }

    /// Get the next bucket merge states for hot archive bucket levels.
    pub fn hot_archive_next_states(&self) -> Option<Vec<LiveBucketNextState>> {
        self.hot_archive_buckets
            .as_ref()
            .map(|levels| parse_next_states(levels))
    }
}

/// State of a pending bucket merge from History Archive State.
///
/// This is a local copy of the structure expected by `BucketList::restore_from_has`
/// to avoid cross-crate dependencies.
#[derive(Clone, Debug, Default)]
pub struct LiveBucketNextState {
    /// Merge state (0 = clear, 1 = output, 2 = inputs)
    pub state: u32,
    /// Output bucket hash if merge is complete (state == 1)
    pub output: Option<Hash256>,
    /// Input curr bucket hash for pending merge (state == 2)
    pub input_curr: Option<Hash256>,
    /// Input snap bucket hash for pending merge (state == 2)
    pub input_snap: Option<Hash256>,
}

impl From<LiveBucketNextState> for henyey_bucket::HasNextState {
    fn from(s: LiveBucketNextState) -> Self {
        Self {
            state: s.state,
            output: s.output,
            input_curr: s.input_curr,
            input_snap: s.input_snap,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_has_json() -> &'static str {
        r#"{
            "version": 2,
            "server": "stellar-core 25.0.1.rc1 (ac5427a148203e8269294cf50866200cbe4ec1d3)",
            "currentLedger": 212735,
            "networkPassphrase": "Test SDF Network ; September 2015",
            "currentBuckets": [
                {
                    "curr": "e113f8cc5468579cb57538e3204c8d3ecce59a0cdb47f6fa7e87ab4d9d8146fd",
                    "next": { "state": 0 },
                    "snap": "02441d532a2aa2bbc5e2b025438572f5ea2e25205442c002e2c6e7636a54b0ef"
                },
                {
                    "curr": "a8a0ed4b478cda48066d06aff6f7ff9b089bbfb3effd3a6e52ce93caa71f0e1a",
                    "next": { "state": 0 },
                    "snap": "d5550575dff797042a61dd86adae42df96b0db9176fdf137e680c2dbac45ede9"
                },
                {
                    "curr": "0000000000000000000000000000000000000000000000000000000000000000",
                    "next": { "state": 0 },
                    "snap": "0000000000000000000000000000000000000000000000000000000000000000"
                }
            ]
        }"#
    }

    #[test]
    fn test_parse_has() {
        let has = HistoryArchiveState::from_json(sample_has_json()).unwrap();

        assert_eq!(has.version, 2);
        assert_eq!(has.current_ledger, 212735);
        assert_eq!(
            has.network_passphrase(),
            Some("Test SDF Network ; September 2015")
        );
        assert!(has.server().unwrap().contains("stellar-core"));
        assert_eq!(has.bucket_level_count(), 3);
    }

    #[test]
    fn test_all_bucket_hashes() {
        let has = HistoryArchiveState::from_json(sample_has_json()).unwrap();
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
        let json = r#"{
            "version": 2,
            "currentLedger": 100,
            "currentBuckets": [
                {
                    "curr": "e113f8cc5468579cb57538e3204c8d3ecce59a0cdb47f6fa7e87ab4d9d8146fd",
                    "snap": "e113f8cc5468579cb57538e3204c8d3ecce59a0cdb47f6fa7e87ab4d9d8146fd",
                    "next": { "state": 0 }
                }
            ]
        }"#;

        let has = HistoryArchiveState::from_json(json).unwrap();
        let unique = has.unique_bucket_hashes();

        // Should deduplicate to 1
        assert_eq!(unique.len(), 1);
    }

    #[test]
    fn test_bucket_hashes_at_level() {
        let has = HistoryArchiveState::from_json(sample_has_json()).unwrap();

        // Level 0 should have both hashes
        let (curr, snap) = has.bucket_hashes_at_level(0).unwrap();
        assert!(curr.is_some());
        assert!(snap.is_some());

        // Level 2 should have zero hashes (None)
        let (curr, snap) = has.bucket_hashes_at_level(2).unwrap();
        assert!(curr.is_none());
        assert!(snap.is_none());

        // Level 10 doesn't exist
        assert!(has.bucket_hashes_at_level(10).is_none());
    }

    #[test]
    fn test_serialize_has() {
        let has = HistoryArchiveState::from_json(sample_has_json()).unwrap();
        let json = has.to_json().unwrap();

        // Re-parse and verify
        let reparsed = HistoryArchiveState::from_json(&json).unwrap();
        assert_eq!(reparsed.current_ledger, has.current_ledger);
        assert_eq!(reparsed.version, has.version);
    }

    #[test]
    fn test_minimal_has() {
        let json = r#"{
            "version": 2,
            "currentLedger": 63,
            "currentBuckets": []
        }"#;

        let has = HistoryArchiveState::from_json(json).unwrap();
        assert_eq!(has.version, 2);
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
        let has = HistoryArchiveState::from_json(sample_has_json()).unwrap();
        let known: HashSet<Hash256> = has.all_bucket_hashes().into_iter().collect();
        assert!(has.contains_valid_buckets(&known).is_ok());
    }

    #[test]
    fn test_contains_valid_buckets_missing_hash() {
        let has = HistoryArchiveState::from_json(sample_has_json()).unwrap();
        // Empty set — no known hashes
        let known: HashSet<Hash256> = HashSet::new();
        assert!(has.contains_valid_buckets(&known).is_err());
    }

    #[test]
    fn test_contains_valid_buckets_level0_not_clear() {
        let json = r#"{
            "version": 2,
            "currentLedger": 127,
            "currentBuckets": [
                {
                    "curr": "0000000000000000000000000000000000000000000000000000000000000000",
                    "snap": "0000000000000000000000000000000000000000000000000000000000000000",
                    "next": { "state": 1, "output": "e113f8cc5468579cb57538e3204c8d3ecce59a0cdb47f6fa7e87ab4d9d8146fd" }
                }
            ]
        }"#;
        let has = HistoryArchiveState::from_json(json).unwrap();
        let known: HashSet<Hash256> = has.all_bucket_hashes().into_iter().collect();
        let result = has.contains_valid_buckets(&known);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("level 0"));
    }

    #[test]
    fn test_contains_valid_buckets_state2_missing_inputs() {
        let json = r#"{
            "version": 2,
            "currentLedger": 127,
            "currentBuckets": [
                {
                    "curr": "0000000000000000000000000000000000000000000000000000000000000000",
                    "snap": "0000000000000000000000000000000000000000000000000000000000000000",
                    "next": { "state": 0 }
                },
                {
                    "curr": "0000000000000000000000000000000000000000000000000000000000000000",
                    "snap": "0000000000000000000000000000000000000000000000000000000000000000",
                    "next": {
                        "state": 2,
                        "curr": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                        "snap": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                    }
                }
            ]
        }"#;
        let has = HistoryArchiveState::from_json(json).unwrap();
        // Only provide main hashes, not the future input hashes
        let known: HashSet<Hash256> = HashSet::new();
        let result = has.contains_valid_buckets(&known);
        assert!(result.is_err());
    }

    // Item 6: FutureBucket resolution method tests
    #[test]
    fn test_futures_all_clear() {
        let has = HistoryArchiveState::from_json(sample_has_json()).unwrap();
        assert!(has.futures_all_clear());
    }

    #[test]
    fn test_futures_all_clear_with_pending() {
        let json = r#"{
            "version": 2,
            "currentLedger": 127,
            "currentBuckets": [
                {
                    "curr": "0000000000000000000000000000000000000000000000000000000000000000",
                    "snap": "0000000000000000000000000000000000000000000000000000000000000000",
                    "next": { "state": 1, "output": "e113f8cc5468579cb57538e3204c8d3ecce59a0cdb47f6fa7e87ab4d9d8146fd" }
                }
            ]
        }"#;
        let has = HistoryArchiveState::from_json(json).unwrap();
        assert!(!has.futures_all_clear());
    }

    #[test]
    fn test_futures_all_resolved() {
        let json = r#"{
            "version": 2,
            "currentLedger": 127,
            "currentBuckets": [
                {
                    "curr": "0000000000000000000000000000000000000000000000000000000000000000",
                    "snap": "0000000000000000000000000000000000000000000000000000000000000000",
                    "next": { "state": 1, "output": "e113f8cc5468579cb57538e3204c8d3ecce59a0cdb47f6fa7e87ab4d9d8146fd" }
                }
            ]
        }"#;
        let has = HistoryArchiveState::from_json(json).unwrap();
        assert!(has.futures_all_resolved()); // state <= 1 is resolved
    }

    #[test]
    fn test_futures_not_all_resolved_with_state2() {
        let json = r#"{
            "version": 2,
            "currentLedger": 127,
            "currentBuckets": [
                {
                    "curr": "0000000000000000000000000000000000000000000000000000000000000000",
                    "snap": "0000000000000000000000000000000000000000000000000000000000000000",
                    "next": { "state": 2, "curr": "aaaa", "snap": "bbbb" }
                }
            ]
        }"#;
        let has = HistoryArchiveState::from_json(json).unwrap();
        assert!(!has.futures_all_resolved()); // state 2 is not resolved
    }

    #[test]
    fn test_resolve_all_futures() {
        let json = r#"{
            "version": 2,
            "currentLedger": 127,
            "currentBuckets": [
                {
                    "curr": "0000000000000000000000000000000000000000000000000000000000000000",
                    "snap": "0000000000000000000000000000000000000000000000000000000000000000",
                    "next": { "state": 0 }
                },
                {
                    "curr": "0000000000000000000000000000000000000000000000000000000000000000",
                    "snap": "0000000000000000000000000000000000000000000000000000000000000000",
                    "next": { "state": 1, "output": "e113f8cc5468579cb57538e3204c8d3ecce59a0cdb47f6fa7e87ab4d9d8146fd" }
                },
                {
                    "curr": "0000000000000000000000000000000000000000000000000000000000000000",
                    "snap": "0000000000000000000000000000000000000000000000000000000000000000",
                    "next": { "state": 2, "curr": "aaaa", "snap": "bbbb" }
                }
            ]
        }"#;
        let mut has = HistoryArchiveState::from_json(json).unwrap();
        has.resolve_all_futures();

        // state 0 stays 0, state 1 becomes 0, state 2 stays 2
        assert_eq!(has.current_buckets[0].next.state, 0);
        assert_eq!(has.current_buckets[1].next.state, 0);
        assert_eq!(has.current_buckets[2].next.state, 2);
    }

    #[test]
    fn test_contains_valid_buckets_state1_unknown_output() {
        let json = r#"{
            "version": 2,
            "currentLedger": 127,
            "currentBuckets": [
                {
                    "curr": "0000000000000000000000000000000000000000000000000000000000000000",
                    "snap": "0000000000000000000000000000000000000000000000000000000000000000",
                    "next": { "state": 0 }
                },
                {
                    "curr": "0000000000000000000000000000000000000000000000000000000000000000",
                    "snap": "0000000000000000000000000000000000000000000000000000000000000000",
                    "next": { "state": 1, "output": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc" }
                }
            ]
        }"#;
        let has = HistoryArchiveState::from_json(json).unwrap();
        // Provide empty set — output hash is unknown
        let known: HashSet<Hash256> = HashSet::new();
        let result = has.contains_valid_buckets(&known);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("output"));
    }

    #[test]
    fn test_contains_valid_buckets_zero_hashes_only() {
        // HAS with only zero hashes should pass (zero hashes are skipped)
        let json = r#"{
            "version": 2,
            "currentLedger": 127,
            "currentBuckets": [
                {
                    "curr": "0000000000000000000000000000000000000000000000000000000000000000",
                    "snap": "0000000000000000000000000000000000000000000000000000000000000000",
                    "next": { "state": 0 }
                }
            ]
        }"#;
        let has = HistoryArchiveState::from_json(json).unwrap();
        let known: HashSet<Hash256> = HashSet::new();
        assert!(has.contains_valid_buckets(&known).is_ok());
    }

    #[test]
    fn test_clear_all_futures() {
        let json = r#"{
            "version": 2,
            "currentLedger": 127,
            "currentBuckets": [
                {
                    "curr": "0000000000000000000000000000000000000000000000000000000000000000",
                    "snap": "0000000000000000000000000000000000000000000000000000000000000000",
                    "next": { "state": 1, "output": "e113f8cc5468579cb57538e3204c8d3ecce59a0cdb47f6fa7e87ab4d9d8146fd" }
                },
                {
                    "curr": "0000000000000000000000000000000000000000000000000000000000000000",
                    "snap": "0000000000000000000000000000000000000000000000000000000000000000",
                    "next": { "state": 2, "curr": "aaaa", "snap": "bbbb" }
                }
            ]
        }"#;
        let mut has = HistoryArchiveState::from_json(json).unwrap();
        has.clear_all_futures();

        assert!(has.futures_all_clear());
        for level in &has.current_buckets {
            assert_eq!(level.next.state, 0);
        }
    }

    // ── differing_bucket_hashes tests ──────────────────────────────────

    /// Helper: create a minimal HAS with given bucket levels (curr, snap pairs).
    fn make_has(levels: Vec<(&str, &str)>) -> HistoryArchiveState {
        let buckets = levels
            .into_iter()
            .map(|(curr, snap)| HASBucketLevel {
                curr: curr.to_string(),
                snap: snap.to_string(),
                next: HASBucketNext::default(),
            })
            .collect();
        HistoryArchiveState {
            version: 2,
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
                let zero_levels: Vec<HASBucketLevel> = (0..11)
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
        let levels: Vec<HASBucketLevel> = (0..11)
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
            network_passphrase: None,
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
        for _ in 0..11 {
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
        let mut levels: Vec<HASBucketLevel> = (0..11)
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
            network_passphrase: None,
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
        for i in 0..11 {
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
}
