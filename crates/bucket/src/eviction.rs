//! Eviction scan implementation for Soroban state archival.
//!
//! This module implements the incremental eviction scan that matches
//! stellar-core's behavior. The eviction scan is responsible for
//! identifying expired Soroban entries (contract data, contract code)
//! and processing them for archival or deletion.
//!
//! ## Overview
//!
//! State archival in Soroban uses a time-to-live (TTL) mechanism where entries
//! have a `liveUntilLedger` value. When the current ledger exceeds this value,
//! the entry is considered expired and must be evicted:
//!
//! - **Temporary entries**: Deleted immediately (not archived)
//! - **Persistent entries**: Archived to the Hot Archive bucket list, then deleted
//!   from the live bucket list
//!
//! ## Incremental Scanning
//!
//! Unlike a full scan which would be expensive, eviction is performed incrementally:
//!
//! 1. Each ledger scans a limited number of bytes (default 100KB)
//! 2. Position is tracked with an `EvictionIterator`
//! 3. Scanning continues from where it left off on the next ledger
//! 4. When a bucket receives new data (spill), the iterator resets to the beginning
//!    of that bucket to ensure new entries are scanned
//!
//! ## Key Concepts
//!
//! - **EvictionIterator**: Tracks current scan position (level, curr/snap bucket, byte offset)
//! - **Scan Size**: Configurable bytes to scan per ledger (default 100KB)
//! - **Starting Level**: Minimum bucket list level to scan (default level 6, since lower
//!   levels update too frequently)
//! - **Spill Detection**: When a bucket receives new data from a level below spilling,
//!   the iterator resets to rescan from the beginning
//!
//! ## Bucket List Level Math
//!
//! The bucket list has a hierarchical structure where lower levels update more frequently:
//!
//! - `level_size(N)` = 4^(N+1): The idealized size boundary for level N
//! - `level_half(N)` = level_size(N) / 2: Half the level size
//! - `level_should_spill(ledger, N)`: Returns true when level N spills at the given ledger
//! - `bucket_update_period(N, is_curr)`: How often a bucket receives new data
//!
//! | Level | level_size | level_half | curr updates every | snap updates every |
//! |-------|------------|------------|--------------------|--------------------|
//! | 0     | 4          | 2          | 1 ledger           | 2 ledgers          |
//! | 1     | 16         | 8          | 2 ledgers          | 8 ledgers          |
//! | 2     | 64         | 32         | 8 ledgers          | 32 ledgers         |
//! | 3     | 256        | 128        | 32 ledgers         | 128 ledgers        |
//! | ...   | ...        | ...        | ...                | ...                |
//! | 6     | 16384      | 8192       | 2048 ledgers       | 8192 ledgers       |
//!
//! ## Example Usage
//!
//! ```ignore
//! use henyey_bucket::{EvictionIterator, update_starting_eviction_iterator};
//!
//! // Initialize iterator at default starting level (6)
//! let mut iter = EvictionIterator::with_default_level();
//!
//! // Before scanning each ledger, check if the iterator needs to reset
//! // (because the bucket received new data)
//! update_starting_eviction_iterator(&mut iter, 6, current_ledger);
//!
//! // Perform the scan (handled by BucketList::scan_for_eviction_incremental)
//! let result = bucket_list.scan_for_eviction_incremental(iter, current_ledger, ledger_version, &settings)?;
//!
//! // Update iterator for next ledger
//! iter = result.end_iterator;
//! ```
//!
//! ## References
//!
//! - stellar-core implementation: `src/bucket/BucketListBase.cpp`, `src/bucket/BucketManager.cpp`
//! - Eviction iterator: `src/ledger/NetworkConfig.h` (EvictionIterator struct)
//! - State archival CAP: CAP-0046 (Soroban State Archival)

use std::collections::HashSet;

use henyey_common::protocol::MIN_SOROBAN_PROTOCOL_VERSION;
use stellar_xdr::curr::{LedgerEntry, LedgerKey, StateArchivalSettings};

use crate::bucket::Bucket;
use crate::bucket_list::BUCKET_LIST_LEVELS;
use crate::entry::{get_ttl_key, is_ttl_expired, BucketEntry};
use henyey_common::{is_soroban_entry, is_temporary_entry};

/// Default eviction scan size in bytes per ledger (100 KB).
pub(crate) const DEFAULT_EVICTION_SCAN_SIZE: u32 = 100_000;

/// Default starting eviction scan level (level 6).
/// Lower levels update too frequently, so we start from level 6.
pub(crate) const DEFAULT_STARTING_EVICTION_SCAN_LEVEL: u32 = 6;

/// Re-export XDR EvictionIterator as the canonical type.
///
/// Tracks the current scan position in the bucket list. The iterator maintains
/// state between ledgers, allowing the eviction scan to resume where it left off.
///
/// # Scan Order
///
/// 1. Level N curr bucket → Level N snap bucket → Level N+1 curr bucket → ...
/// 2. Wraps back to starting level when reaching the top.
pub type EvictionIterator = stellar_xdr::curr::EvictionIterator;

/// Extension trait adding eviction-specific methods to the XDR `EvictionIterator`.
pub trait EvictionIteratorExt {
    /// Create a new eviction iterator starting at the given level.
    fn new(starting_level: u32) -> Self;

    /// Create a new eviction iterator at the default starting level (6).
    fn with_default_level() -> Self;

    /// Reset the iterator to start of the current bucket.
    fn reset_offset(&mut self);

    /// Move to the next bucket in the scan order.
    ///
    /// Order: level N curr -> level N snap -> level N+1 curr -> ...
    /// Wraps back to starting level when reaching the top.
    ///
    /// Returns true if we've wrapped back to the starting position (completed a full cycle).
    fn advance_to_next_bucket(&mut self, starting_level: u32) -> bool;
}

impl EvictionIteratorExt for EvictionIterator {
    fn new(starting_level: u32) -> Self {
        Self {
            bucket_file_offset: 0,
            bucket_list_level: starting_level,
            is_curr_bucket: true,
        }
    }

    fn with_default_level() -> Self {
        Self::new(DEFAULT_STARTING_EVICTION_SCAN_LEVEL)
    }

    fn reset_offset(&mut self) {
        self.bucket_file_offset = 0;
    }

    fn advance_to_next_bucket(&mut self, starting_level: u32) -> bool {
        let mut wrapped = false;
        let last_level = BUCKET_LIST_LEVELS as u32 - 1;

        if self.is_curr_bucket {
            // Move from curr to snap at same level, except for the last level
            if self.bucket_list_level != last_level {
                self.is_curr_bucket = false;
                self.bucket_file_offset = 0;
            } else {
                // Last level has no snap scan; wrap to starting level
                self.is_curr_bucket = true;
                self.bucket_file_offset = 0;
                self.bucket_list_level = starting_level;
                wrapped = true;
            }
        } else {
            // Move from snap to curr at next level
            self.bucket_list_level += 1;
            self.is_curr_bucket = true;
            self.bucket_file_offset = 0;

            // Wrap around at top level
            if self.bucket_list_level > last_level {
                self.bucket_list_level = starting_level;
                wrapped = true;
            }
        }

        wrapped
    }
}

/// A candidate entry for eviction, collected during the scan phase.
///
/// stellar-core uses a two-phase eviction approach:
/// 1. **Scan phase**: Collects ALL eligible candidates within the byte budget
/// 2. **Resolution phase**: Applies TTL filtering and max_entries limit
///
/// Each candidate tracks the entry data, keys, and the bucket list position
/// where it was found. The position is used for iterator adjustment when
/// the max_entries_to_archive limit is hit.
#[derive(Debug)]
pub struct EvictionCandidate {
    /// The data entry being evicted (newest version from bucket list).
    entry: LedgerEntry,
    /// The EvictionIterator position AFTER this entry (resume point).
    position: EvictionIterator,
    /// The TTL `liveUntilLedger` of the entry, captured from the TTL entry at
    /// scan time. Used at resolve time to compute the evicted entry's age
    /// (`ledger_seq - live_until_ledger`) for the eviction-cycle age metric.
    /// Mirrors stellar-core's `EvictionResultEntry::liveUntilLedger`.
    live_until_ledger: u32,
}

impl EvictionCandidate {
    /// Create an EvictionCandidate.
    ///
    /// `live_until_ledger` is the entry's TTL `liveUntilLedger`, captured from
    /// the TTL entry during the scan, so the resolve phase can record the
    /// evicted entry's age without re-looking up the TTL.
    ///
    /// # Panics
    /// Panics if the entry is not a Soroban entry with a derivable TTL key.
    /// This constructor is `pub(crate)` — only the eviction scan path calls
    /// it, and that path filters to Soroban entries before reaching here.
    /// The panic is defense-in-depth against internal misuse.
    pub(crate) fn new(
        entry: LedgerEntry,
        position: EvictionIterator,
        live_until_ledger: u32,
    ) -> Self {
        let data_key = henyey_common::entry_to_key(&entry);
        assert!(
            get_ttl_key(&data_key).is_some(),
            "EvictionCandidate entry must be a Soroban entry with a derivable TTL key"
        );
        Self {
            entry,
            position,
            live_until_ledger,
        }
    }

    /// The data entry being evicted.
    pub fn entry(&self) -> &LedgerEntry {
        &self.entry
    }

    /// The data entry's key (derived from entry).
    pub fn data_key(&self) -> LedgerKey {
        henyey_common::entry_to_key(&self.entry)
    }

    /// The corresponding TTL key (derived from entry via SHA-256 hash).
    pub fn ttl_key(&self) -> LedgerKey {
        get_ttl_key(&self.data_key()).unwrap()
    }

    /// Whether this is a temporary entry (vs persistent).
    pub fn is_temporary(&self) -> bool {
        is_temporary_entry(&self.entry)
    }

    /// The EvictionIterator position AFTER this entry (resume point).
    pub fn position(&self) -> &EvictionIterator {
        &self.position
    }

    /// The entry's TTL `liveUntilLedger` captured at scan time.
    pub fn live_until_ledger(&self) -> u32 {
        self.live_until_ledger
    }

    /// Consume self, returning the owned entry and position.
    pub(crate) fn into_parts(self) -> (LedgerEntry, EvictionIterator) {
        (self.entry, self.position)
    }
}

/// Result of the scan phase of eviction for a single ledger.
///
/// Contains eviction candidates and the scan region end position.
/// Call `resolve()` to apply TTL filtering and max_entries limit,
/// producing the final eviction results.
#[derive(Debug, Default)]
pub struct EvictionResult {
    /// Eviction candidates collected during the scan.
    /// These need to be resolved (filtered + limited) before use.
    pub candidates: Vec<EvictionCandidate>,
    /// EvictionIterator at the end of the scan region.
    /// This is where the next scan should start if no max_entries limit was hit.
    pub end_iterator: EvictionIterator,
    /// Total bytes of entry data scanned during this ledger.
    pub bytes_scanned: u64,
    /// Whether the scan completed its byte quota (vs hitting bucket end early).
    pub scan_complete: bool,
    /// Ledger sequence the scan was based on (captured at scan start).
    ///
    /// Used by [`EvictionResult::is_valid`] to reject a stale background scan
    /// whose ledger no longer matches the resolve-time ledger.
    pub initial_ledger_seq: u32,
    /// Protocol version the scan was based on (captured at scan start).
    ///
    /// A background scan for ledger N is started immediately after N-1 closes,
    /// so this is the *just-closed* ledger's protocol version. At resolve time
    /// it is compared against the resolve ledger's pre-transaction version
    /// (post any upgrade) to detect a crossing into the first protocol that
    /// supports persistent eviction (V23).
    pub initial_ledger_version: u32,
    /// State archival settings the scan was based on (captured at scan start).
    ///
    /// Used by [`EvictionResult::is_valid`] to reject a scan whose eviction
    /// parameters changed via a config upgrade before resolve.
    pub initial_archival_settings: StateArchivalSettings,
}

/// Result of resolving eviction candidates.
///
/// Produced by `EvictionResult::resolve()` after applying TTL filtering
/// and the max_entries_to_archive limit.
///
/// The separation of `deleted_keys` and `archived_entries` mirrors stellar-core's
/// `resolveBackgroundEvictionScan` which returns `EvictedStateVectors{deletedKeys, archivedEntries}`.
pub struct ResolvedEviction {
    /// Persistent entries to archive to the hot archive bucket list.
    pub archived_entries: Vec<LedgerEntry>,
    /// Keys deleted from the live BucketList: temporary data keys + ALL TTL keys
    /// (both temporary and persistent).
    ///
    /// Matches stellar-core's `deletedKeys` from `resolveBackgroundEvictionScan`.
    /// Does NOT include persistent data keys (those are derived from `archived_entries`).
    pub deleted_keys: Vec<LedgerKey>,
    /// The resolved EvictionIterator for the next scan.
    pub end_iterator: EvictionIterator,
}

impl ResolvedEviction {
    /// Build all evicted keys matching stellar-core's `populateEvictedEntries` ordering:
    /// `deleted_keys` first (temp data keys + all TTL keys in scan order), then persistent
    /// data keys derived from `archived_entries`.
    ///
    /// Parity: LedgerCloseMetaFrame.cpp:170-187 iterates `deletedKeys` then `archivedEntries`.
    pub fn evicted_keys(&self) -> Vec<LedgerKey> {
        let mut keys = self.deleted_keys.clone();
        for entry in &self.archived_entries {
            keys.push(henyey_common::entry_to_key(entry));
        }
        keys
    }
}

impl EvictionResult {
    /// Returns true if this scan is still a valid basis for eviction at the
    /// given current ledger / protocol version / archival settings.
    ///
    /// Mirrors stellar-core `EvictionResultCandidates::isValid`
    /// (`src/bucket/BucketUtils.cpp`, v26.0.1). A background eviction scan for
    /// ledger N is started immediately after N-1 closes, but ledger N may apply
    /// a network upgrade that changes eviction behavior. Such a scan must be
    /// discarded and restarted with the post-upgrade state.
    ///
    /// Invalid (returns false) when ANY of:
    /// - The protocol crosses *into* V23 (the first protocol supporting
    ///   persistent eviction) between scan start and resolve. Other protocol
    ///   upgrades do not affect eviction scans.
    /// - The resolve ledger_seq differs from the scan's captured ledger_seq.
    /// - Any of the three eviction-relevant archival-settings fields changed:
    ///   `max_entries_to_archive`, `eviction_scan_size`,
    ///   `starting_eviction_scan_level`.
    ///
    /// Parity: compares ONLY those three settings fields — NOT the full
    /// `StateArchivalSettings` struct. stellar-core deliberately ignores
    /// TTL/rent fields (e.g. `min_temporary_ttl`) here; a full-struct equality
    /// check would be over-strict and diverge from core.
    ///
    /// Version mapping: `curr_ledger_seq` / `curr_ledger_vers` / `curr_sas`
    /// are the resolve ledger's pre-transaction values (core's `currLedger*`),
    /// while `initial_*` were captured at scan start (the just-closed ledger).
    pub fn is_valid(
        &self,
        curr_ledger_seq: u32,
        curr_ledger_vers: u32,
        curr_sas: &StateArchivalSettings,
    ) -> bool {
        use henyey_common::protocol::{
            protocol_version_is_before, protocol_version_starts_from, ProtocolVersion,
        };

        // If the scan started before V23 and the resolve ledger crosses into
        // V23, eviction behavior changed mid-scan — restart.
        if protocol_version_is_before(self.initial_ledger_version, ProtocolVersion::V23)
            && protocol_version_starts_from(curr_ledger_vers, ProtocolVersion::V23)
        {
            return false;
        }

        self.initial_ledger_seq == curr_ledger_seq
            && self.initial_archival_settings.max_entries_to_archive
                == curr_sas.max_entries_to_archive
            && self.initial_archival_settings.eviction_scan_size == curr_sas.eviction_scan_size
            && self.initial_archival_settings.starting_eviction_scan_level
                == curr_sas.starting_eviction_scan_level
    }

    /// Resolve eviction candidates by applying TTL filtering and max_entries limit.
    ///
    /// This matches stellar-core's `resolveBackgroundEvictionScan`:
    /// 1. Filter out entries whose TTL was modified by transactions in this ledger
    /// 2. For remaining entries, check if the live entry key was modified — if so,
    ///    log an internal bug (this should not happen in a correct system)
    /// 3. Evict up to `max_entries_to_archive` entries from the filtered set
    /// 4. Set the iterator position:
    ///    - If the entry limit was hit: resume from the last evicted entry's position
    ///    - Otherwise (including max_entries=0): advance to end of scan region
    /// `ledger_seq` is the resolve-time current ledger; together with each
    /// evicted candidate's `live_until_ledger` it yields the entry age
    /// (`ledger_seq - live_until_ledger`) recorded into `counters` for the
    /// eviction-cycle age gauge. `counters`, when `Some`, also receives the
    /// per-type evicted count. Both mirror stellar-core
    /// `BucketManager::resolveBackgroundEvictionScan` (`BucketManager.cpp:1287-1289`):
    /// the `entriesEvicted` counter and `recordEvictedEntry(age)` happen in the
    /// resolve phase, per actually-evicted entry (after TTL filter + max cap).
    pub fn resolve(
        self,
        max_entries_to_archive: u32,
        modified_keys: &std::collections::HashSet<LedgerKey>,
        ledger_seq: u32,
        counters: Option<&crate::metrics::EvictionCounters>,
    ) -> ResolvedEviction {
        let scan_end_iterator = self.end_iterator;

        // Single-pass resolution: filter + collect in one loop.
        //
        // Parity: stellar-core's `resolveBackgroundEvictionScan` uses a
        // single `modifiedKeys` set containing ALL keys touched in the
        // current ledger (data + TTL). For each candidate:
        //   - If TTL key is in modifiedKeys → filter out (don't evict).
        //   - Else if data key is in modifiedKeys → log internal bug
        //     (a data-key write without a TTL update should not happen).
        //
        // stellar-core builds two separate vectors:
        //   - deletedKeys: temp data keys + ALL TTL keys (both temp and persistent)
        //   - archivedEntries: full LedgerEntry for persistent entries
        // We mirror this separation in deleted_keys + archived_entries.
        let mut archived_entries = Vec::new();
        let mut deleted_keys = Vec::new();
        let mut last_evicted_position = None;
        let mut remaining = max_entries_to_archive;

        for candidate in self.candidates {
            let data_key = candidate.data_key();
            let ttl_key = candidate.ttl_key();

            if modified_keys.contains(&ttl_key) {
                // TTL was modified this ledger — skip eviction.
                continue;
            }

            // Parity: stellar-core checks if the live entry key was
            // modified while the TTL was not. This should never happen
            // in a correct system (a restore or write would also touch
            // the TTL). Log it as an internal bug.
            if modified_keys.contains(&data_key) {
                tracing::error!(
                    key = ?data_key,
                    "Eviction attempted on modified live entry — this is an internal bug"
                );
            }

            if remaining == 0 {
                break;
            }

            let is_temporary = candidate.is_temporary();
            let live_until_ledger = candidate.live_until_ledger();
            let (entry, position) = candidate.into_parts();

            if is_temporary {
                deleted_keys.push(data_key);
            } else {
                // Persistent entries go to hot archive
                archived_entries.push(entry);
            }
            // TTL key is always added to deleted_keys for both types
            deleted_keys.push(ttl_key);

            // Record eviction telemetry per actually-evicted entry.
            // Parity: stellar-core records these in the resolve phase, after the
            // TTL filter and max-cap, per evicted entry (BucketManager.cpp:1287-1289).
            // age = ledger_seq - live_until_ledger; saturating_sub is defensive
            // (expired entries already guarantee live_until_ledger < ledger_seq).
            if let Some(counters) = counters {
                let (temp_count, persistent_count) = if is_temporary { (1, 0) } else { (0, 1) };
                counters.record_evicted(1, temp_count, persistent_count);
                counters.record_evicted_entry_age(u64::from(
                    ledger_seq.saturating_sub(live_until_ledger),
                ));
            }

            last_evicted_position = Some(position);
            remaining -= 1;
        }

        // Set iterator position.
        // stellar-core logic from resolveBackgroundEvictionScan:
        //   newEvictionIterator is initialized to endOfRegionIterator
        //   Each eviction updates it to the evicted entry's position
        //   After the loop: if (remainingEntriesToEvict != 0) { use endOfRegionIterator }
        //
        // This means:
        // - If we exhausted the budget (remaining == 0 AND max > 0): use last evicted position
        // - If we didn't exhaust the budget (remaining > 0): use end of scan region
        // - If max_entries == 0: remaining starts at 0, loop never runs, use end of scan region
        let end_iterator = if max_entries_to_archive > 0 && remaining == 0 {
            // We hit the eviction limit — resume from last evicted position next time
            last_evicted_position.unwrap_or(scan_end_iterator)
        } else {
            // Didn't hit limit (or max_entries=0) — advance to end of scan region
            scan_end_iterator
        };

        ResolvedEviction {
            archived_entries,
            deleted_keys,
            end_iterator,
        }
    }
}

/// Default maximum entries to archive per ledger.
pub(crate) const DEFAULT_MAX_ENTRIES_TO_ARCHIVE: u32 = 1000;

/// Create default `StateArchivalSettings` for eviction scanning.
///
/// Uses the XDR `StateArchivalSettings` type directly with default eviction
/// parameters and zero values for unrelated fields (TTL, rent rates).
pub fn default_state_archival_settings() -> StateArchivalSettings {
    StateArchivalSettings {
        max_entry_ttl: 0,
        min_temporary_ttl: 0,
        min_persistent_ttl: 0,
        persistent_rent_rate_denominator: 0,
        temp_rent_rate_denominator: 0,
        max_entries_to_archive: DEFAULT_MAX_ENTRIES_TO_ARCHIVE,
        live_soroban_state_size_window_sample_size: 0,
        live_soroban_state_size_window_sample_period: 0,
        eviction_scan_size: DEFAULT_EVICTION_SCAN_SIZE,
        starting_eviction_scan_level: DEFAULT_STARTING_EVICTION_SCAN_LEVEL,
    }
}

/// Calculate the idealized size of a bucket list level.
///
/// Formula: 4^(level+1) = 1 << (2 * (level + 1))
///
/// - Level 0: 4
/// - Level 1: 16
/// - Level 2: 64
/// - Level 3: 256
/// - ...
pub(crate) fn level_size(level: u32) -> u64 {
    1u64 << (2 * (level + 1))
}

/// Calculate half of the level size.
pub fn level_half(level: u32) -> u64 {
    level_size(level) >> 1
}

/// Round down a value to the nearest multiple of a power-of-2 modulo.
///
/// Formula: value & ~(modulo - 1)
fn round_down(value: u64, modulo: u64) -> u64 {
    value & !(modulo - 1)
}

/// Check if a level should spill at the given ledger.
///
/// A level spills when the ledger number is at a levelHalf or levelSize boundary.
/// The top level (level 10) never spills.
pub(crate) fn level_should_spill(ledger: u32, level: u32) -> bool {
    if level >= BUCKET_LIST_LEVELS as u32 - 1 {
        return false; // Top level never spills
    }

    let ledger = ledger as u64;
    let half = level_half(level);
    let size = level_size(level);

    ledger == round_down(ledger, half) || ledger == round_down(ledger, size)
}

/// Calculate how frequently a bucket receives new data (update period in ledgers).
///
/// - Level 0 curr: 1 ledger
/// - Level 0 snap: 2 ledgers
/// - Level 1 curr: 2 ledgers
/// - Level 1 snap: 8 ledgers
/// - Level N curr: 2^(2*N - 1) ledgers
pub(crate) fn bucket_update_period(level: u32, is_curr: bool) -> u32 {
    if !is_curr {
        // Snap bucket updates when the level below spills
        return bucket_update_period(level + 1, true);
    }

    if level == 0 {
        return 1;
    }

    // Formula: 2^(2*level - 1)
    1u32 << (2 * level - 1)
}

/// Check whether the eviction scan can finish scanning the bucket at the
/// iterator's current position before that bucket next receives an update.
///
/// Mirrors stellar-core `LiveBucketList::checkIfEvictionScanIsStuck`
/// (`LiveBucketList.cpp:158-173`): if `bucketUpdatePeriod(level, isCurr) *
/// scanSize < bucket.getSize()`, the bucket is too large to fully scan within
/// its update period, so the scan can never complete a pass over it. Returns
/// `true` in that "stuck" case so the caller can record an incomplete-scan.
pub(crate) fn eviction_scan_is_stuck(
    iter: &EvictionIterator,
    scan_size: u32,
    bucket_byte_size: u64,
) -> bool {
    let period = bucket_update_period(iter.bucket_list_level, iter.is_curr_bucket) as u64;
    period.saturating_mul(scan_size as u64) < bucket_byte_size
}

/// Update the eviction iterator based on bucket spills.
///
/// This resets the iterator's byte offset when a bucket has received new data
/// (invalidating the current scan position).
///
/// Returns true if the iterator was reset.
pub fn update_starting_eviction_iterator(
    iter: &mut EvictionIterator,
    first_scan_level: u32,
    ledger_seq: u32,
) -> bool {
    let mut was_reset = false;

    // If iterator level is below the minimum, reset to minimum
    if iter.bucket_list_level < first_scan_level {
        iter.bucket_file_offset = 0;
        iter.is_curr_bucket = true;
        iter.bucket_list_level = first_scan_level;
        was_reset = true;
    }

    // stellar-core checks spill activity from the previous ledger because the iterator
    // is persisted before spills are applied.
    let prev_ledger = ledger_seq.saturating_sub(1);

    // Check if the bucket we're scanning has received new data
    if iter.is_curr_bucket {
        // Curr bucket receives data when the level below spills.
        // Parity: stellar-core asserts `iter.bucketListLevel > 0` here
        // (LiveBucketList.cpp:92-101). Level 0 curr is unreachable in production
        // because the minimum starting scan level is always >= 1. We warn rather
        // than assert because tests may exercise level 0 for simplicity.
        if iter.bucket_list_level == 0 {
            tracing::warn!(
                "eviction iterator scanning level 0 curr bucket; \
                 this is unreachable in production"
            );
        }
        if iter.bucket_list_level > 0 {
            let level_below = iter.bucket_list_level - 1;
            if level_should_spill(prev_ledger, level_below) {
                iter.bucket_file_offset = 0;
                was_reset = true;
            }
        } else {
            // Level 0 curr receives data every ledger
            iter.bucket_file_offset = 0;
            was_reset = true;
        }
    } else {
        // Snap bucket receives data when its own level spills
        if level_should_spill(prev_ledger, iter.bucket_list_level) {
            iter.bucket_file_offset = 0;
            was_reset = true;
        }
    }

    was_reset
}

/// Scan a region of a bucket for evictable entries (scan phase only).
///
/// Returns `(entries_scanned, bytes_used, finished_bucket)`.
///
/// This is the shared implementation used by both `BucketList::scan_bucket_region`
/// and `BucketListSnapshot::scan_bucket_region`. The `lookup` closure abstracts
/// over the different entry-lookup methods (`BucketList::get` vs
/// `BucketListSnapshot::get_result`).
///
/// Spec: BUCKETLISTDB_SPEC §12 — eviction scanning.
pub(crate) fn scan_bucket_region(
    bucket: &Bucket,
    iter: &mut EvictionIterator,
    max_bytes: u64,
    current_ledger: u32,
    candidates: &mut Vec<EvictionCandidate>,
    seen_keys: &mut HashSet<LedgerKey>,
    lookup: impl Fn(&LedgerKey) -> crate::Result<Option<LedgerEntry>>,
) -> crate::Result<(usize, u64, bool)> {
    let mut entries_scanned = 0;
    let mut bytes_used = 0u64;

    let bucket_protocol = bucket.protocol_version()?.unwrap_or(0);
    if bucket_protocol < MIN_SOROBAN_PROTOCOL_VERSION {
        iter.bucket_file_offset = 0;
        return Ok((entries_scanned, bytes_used, true));
    }

    // Mirror stellar-core: if (bytesToScan == 0) return Loop::COMPLETE
    if max_bytes == 0 {
        return Ok((entries_scanned, bytes_used, false));
    }

    let start_offset = iter.bucket_file_offset;

    for result in bucket.iter_from_offset_with_sizes(start_offset)? {
        let (entry, entry_size) = result?;
        bytes_used += entry_size;
        entries_scanned += 1;

        'process: {
            let live_entry = match &entry {
                BucketEntry::Liveentry(e) | BucketEntry::Initentry(e) => e,
                BucketEntry::Deadentry(_) => {
                    break 'process;
                }
                BucketEntry::Metaentry(_) => {
                    break 'process;
                }
            };

            if !is_soroban_entry(live_entry) {
                break 'process;
            }

            let key = henyey_common::entry_to_key(live_entry);

            if !seen_keys.insert(key.clone()) {
                break 'process;
            }

            let Some(ttl_key) = get_ttl_key(&key) else {
                break 'process;
            };

            let Some(ttl_entry) = lookup(&ttl_key)? else {
                break 'process;
            };

            let Some(is_expired) = is_ttl_expired(&ttl_entry, current_ledger) else {
                break 'process;
            };

            if !is_expired {
                break 'process;
            }

            // Capture the TTL liveUntilLedger so resolve can compute the
            // evicted entry's age (ledger_seq - live_until_ledger) for the
            // eviction-cycle age metric, without re-looking up the TTL.
            // is_ttl_expired already confirmed this is a TTL entry, so the
            // `unwrap_or` fallback (== current_ledger → age 0) is unreachable.
            let live_until_ledger =
                crate::entry::get_ttl_live_until(&ttl_entry).unwrap_or(current_ledger);

            // Entry is expired — collect as eviction candidate.
            // Spec: BUCKETLISTDB_SPEC §12.4 — Newest-version replacement for persistent
            // entries during eviction. Spec says P24+ only (pre-P24 preserved the
            // older-version bug). Henyey applies unconditionally — correct under P24+
            // scope. If pre-P24 support were ever added, a version guard would be needed.
            // For persistent entries, archive the NEWEST version from the bucket list.
            let is_temp = is_temporary_entry(live_entry);
            let entry_for_candidate = if !is_temp {
                // stellar-core asserts the newest version exists
                // (BucketListSnapshot.cpp:756). If lookup returns None,
                // a DEAD tombstone shadows the data while the TTL is
                // still alive — an invariant violation.
                lookup(&key)?.unwrap_or_else(|| {
                    panic!(
                        "BUG: persistent entry not found in bucket list during eviction scan: {:?}",
                        key
                    )
                })
            } else {
                live_entry.clone()
            };

            candidates.push(EvictionCandidate::new(
                entry_for_candidate,
                EvictionIterator {
                    bucket_list_level: iter.bucket_list_level,
                    is_curr_bucket: iter.is_curr_bucket,
                    bucket_file_offset: start_offset + bytes_used,
                },
                live_until_ledger,
            ));
        }

        if bytes_used >= max_bytes {
            break;
        }
    }

    let budget_exhausted = bytes_used >= max_bytes;
    iter.bucket_file_offset = start_offset + bytes_used;
    Ok((entries_scanned, bytes_used, !budget_exhausted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_level_size() {
        // Matches stellar-core BucketListBase::levelSize
        assert_eq!(level_size(0), 4);
        assert_eq!(level_size(1), 16);
        assert_eq!(level_size(2), 64);
        assert_eq!(level_size(3), 256);
        assert_eq!(level_size(4), 1024);
        assert_eq!(level_size(5), 4096);
        assert_eq!(level_size(6), 16384);
        assert_eq!(level_size(7), 65536);
        assert_eq!(level_size(8), 262144);
        assert_eq!(level_size(9), 1048576);
        assert_eq!(level_size(10), 4194304);
    }

    #[test]
    fn test_level_half() {
        // Matches stellar-core BucketListBase::levelHalf
        assert_eq!(level_half(0), 2);
        assert_eq!(level_half(1), 8);
        assert_eq!(level_half(2), 32);
        assert_eq!(level_half(3), 128);
        assert_eq!(level_half(4), 512);
    }

    #[test]
    fn test_level_should_spill() {
        // Level 0 spills at ledgers: 0, 2, 4, 6, 8...
        // (every levelHalf(0)=2 ledgers)
        assert!(level_should_spill(0, 0));
        assert!(!level_should_spill(1, 0));
        assert!(level_should_spill(2, 0));
        assert!(!level_should_spill(3, 0));
        assert!(level_should_spill(4, 0));

        // Level 1 spills at ledgers: 0, 8, 16, 24...
        // (every levelHalf(1)=8 ledgers)
        assert!(level_should_spill(0, 1));
        assert!(!level_should_spill(4, 1));
        assert!(level_should_spill(8, 1));
        assert!(!level_should_spill(12, 1));
        assert!(level_should_spill(16, 1));

        // Level 2 spills at ledgers: 0, 32, 64...
        assert!(level_should_spill(0, 2));
        assert!(!level_should_spill(16, 2));
        assert!(level_should_spill(32, 2));
        assert!(level_should_spill(64, 2));

        // Top level (10) never spills
        assert!(!level_should_spill(0, BUCKET_LIST_LEVELS as u32 - 1));
        assert!(!level_should_spill(1000000, BUCKET_LIST_LEVELS as u32 - 1));
    }

    #[test]
    fn test_bucket_update_period() {
        // Matches stellar-core bucketUpdatePeriod arithmetic test
        // Curr bucket at level 0 updates every ledger
        assert_eq!(bucket_update_period(0, true), 1);

        // Snap bucket at level 0 updates when level 1 curr updates
        assert_eq!(bucket_update_period(0, false), 2);

        // Curr bucket at level N (N>0) updates every 2^(2*N-1) ledgers
        assert_eq!(bucket_update_period(1, true), 2);
        assert_eq!(bucket_update_period(2, true), 8);
        assert_eq!(bucket_update_period(3, true), 32);
        assert_eq!(bucket_update_period(4, true), 128);
        assert_eq!(bucket_update_period(5, true), 512);
        assert_eq!(bucket_update_period(6, true), 2048);

        // Snap bucket at level N updates when level N+1 curr updates
        assert_eq!(bucket_update_period(1, false), 8);
        assert_eq!(bucket_update_period(2, false), 32);
        assert_eq!(bucket_update_period(3, false), 128);
    }

    #[test]
    fn test_bucket_update_period_arithmetic() {
        // Verify the relationship between update period and levelShouldSpill
        // This matches the stellar-core "bucketUpdatePeriod arithmetic" test
        for level in 0..BUCKET_LIST_LEVELS as u32 {
            let curr_period = bucket_update_period(level, true);
            let snap_period = bucket_update_period(level, false);

            // Curr bucket updates when level below spills (for level > 0)
            // or every ledger (for level 0)
            if level == 0 {
                assert_eq!(curr_period, 1);
            } else {
                // Verify spill occurs at multiples of period
                assert!(level_should_spill(curr_period, level - 1));
                if curr_period > 1 {
                    assert!(!level_should_spill(curr_period - 1, level - 1));
                }
            }

            // Snap bucket updates when its own level spills
            if level < BUCKET_LIST_LEVELS as u32 - 1 {
                assert!(level_should_spill(snap_period, level));
                if snap_period > 1 {
                    assert!(!level_should_spill(snap_period - 1, level));
                }
            }
        }
    }

    #[test]
    fn test_iterator_advance() {
        let mut iter = EvictionIterator::new(6);
        assert_eq!(iter.bucket_list_level, 6);
        assert!(iter.is_curr_bucket);

        // Advance: level 6 curr -> level 6 snap
        let wrapped = iter.advance_to_next_bucket(6);
        assert!(!wrapped);
        assert_eq!(iter.bucket_list_level, 6);
        assert!(!iter.is_curr_bucket);

        // Advance: level 6 snap -> level 7 curr
        let wrapped = iter.advance_to_next_bucket(6);
        assert!(!wrapped);
        assert_eq!(iter.bucket_list_level, 7);
        assert!(iter.is_curr_bucket);

        // Advance through remaining levels...
        for _ in 0..7 {
            // 7 snap, 8 curr, 8 snap, 9 curr, 9 snap, 10 curr, wrap to 6 curr
            iter.advance_to_next_bucket(6);
        }

        // Should be back at level 6 curr (wrapped)
        assert_eq!(iter.bucket_list_level, 6);
        assert!(iter.is_curr_bucket);
    }

    #[test]
    fn test_iterator_wrap_detection() {
        let mut iter = EvictionIterator::new(6);

        // Advance until we wrap
        let mut count = 0;
        loop {
            let wrapped = iter.advance_to_next_bucket(6);
            count += 1;
            if wrapped {
                break;
            }
            // Safety: prevent infinite loop
            assert!(count < 100, "Iterator didn't wrap");
        }

        // Should take 9 advances: 6c->6s, 6s->7c, 7c->7s, 7s->8c, 8c->8s,
        // 8s->9c, 9c->9s, 9s->10c, 10c->6c (wrap; last level has no snap)
        assert_eq!(count, 9);
    }

    #[test]
    fn test_iterator_different_starting_levels() {
        // Test starting at level 0
        let mut iter = EvictionIterator::new(0);
        let mut count = 0;
        loop {
            let wrapped = iter.advance_to_next_bucket(0);
            count += 1;
            if wrapped {
                break;
            }
            assert!(count < 100);
        }
        // All 11 levels, last level has no snap = 21 advances
        assert_eq!(count, 21);

        // Test starting at level 10 (top level)
        let mut iter = EvictionIterator::new(10);
        let wrapped = iter.advance_to_next_bucket(10);
        assert!(wrapped); // 10 curr -> wrap to 10 curr (last level has no snap)
        assert_eq!(iter.bucket_list_level, 10);
        assert!(iter.is_curr_bucket);
    }

    #[test]
    fn test_update_starting_eviction_iterator_level_reset() {
        // Test that iterator resets when below minimum level
        let mut iter = EvictionIterator {
            bucket_file_offset: 1000,
            bucket_list_level: 3,
            is_curr_bucket: false,
        };

        let was_reset = update_starting_eviction_iterator(&mut iter, 6, 100);
        assert!(was_reset);
        assert_eq!(iter.bucket_file_offset, 0);
        assert_eq!(iter.bucket_list_level, 6);
        assert!(iter.is_curr_bucket);
    }

    #[test]
    fn test_update_starting_eviction_iterator_curr_bucket_reset() {
        // Level 6 curr bucket receives data when level 5 spills
        // level_half(5) = 2048, so level 5 spills at ledgers 0, 2048, 4096...
        let mut iter = EvictionIterator {
            bucket_file_offset: 5000,
            bucket_list_level: 6,
            is_curr_bucket: true,
        };

        // Ledger 2047 - level 5 doesn't spill, iterator should NOT reset
        let was_reset = update_starting_eviction_iterator(&mut iter, 6, 2047);
        assert!(!was_reset);
        assert_eq!(iter.bucket_file_offset, 5000);

        // Ledger 2049 - previous ledger spills, iterator SHOULD reset
        let was_reset = update_starting_eviction_iterator(&mut iter, 6, 2049);
        assert!(was_reset);
        assert_eq!(iter.bucket_file_offset, 0);
    }

    #[test]
    fn test_update_starting_eviction_iterator_snap_bucket_reset() {
        // Level 6 snap bucket receives data when level 6 spills
        // level_half(6) = 8192, so level 6 spills at ledgers 0, 8192, 16384...
        let mut iter = EvictionIterator {
            bucket_file_offset: 5000,
            bucket_list_level: 6,
            is_curr_bucket: false,
        };

        // Ledger 8191 - level 6 doesn't spill, iterator should NOT reset
        let was_reset = update_starting_eviction_iterator(&mut iter, 6, 8191);
        assert!(!was_reset);
        assert_eq!(iter.bucket_file_offset, 5000);

        // Ledger 8193 - previous ledger spills, iterator SHOULD reset
        let was_reset = update_starting_eviction_iterator(&mut iter, 6, 8193);
        assert!(was_reset);
        assert_eq!(iter.bucket_file_offset, 0);
    }

    #[test]
    fn test_update_starting_eviction_iterator_level_0_always_resets() {
        // Level 0 curr is unreachable in production (minimum scan level >= 1).
        // Parity: stellar-core asserts `iter.bucketListLevel > 0`. We emit a
        // warning instead — verify the graceful fallback still works.
        let mut iter = EvictionIterator {
            bucket_file_offset: 5000,
            bucket_list_level: 0,
            is_curr_bucket: true,
        };

        // Any ledger should reset level 0 curr
        for ledger in [1, 2, 3, 100, 1000] {
            iter.bucket_file_offset = 5000;
            let was_reset = update_starting_eviction_iterator(&mut iter, 0, ledger);
            assert!(was_reset, "Level 0 curr should reset at ledger {}", ledger);
            assert_eq!(iter.bucket_file_offset, 0);
        }
    }

    #[test]
    fn test_update_starting_eviction_iterator_preserves_position() {
        // When bucket hasn't received new data, position should be preserved
        let mut iter = EvictionIterator {
            bucket_file_offset: 12345,
            bucket_list_level: 7,
            is_curr_bucket: true,
        };

        // Level 7 curr receives data when level 6 spills
        // Level 6 spills at multiples of levelHalf(6) = 2048
        // Ledger 100 is not a spill point for level 6
        let was_reset = update_starting_eviction_iterator(&mut iter, 6, 100);
        assert!(!was_reset);
        assert_eq!(iter.bucket_file_offset, 12345);
        assert_eq!(iter.bucket_list_level, 7);
        assert!(iter.is_curr_bucket);
    }

    #[test]
    fn test_iterator_offset_tracking() {
        let mut iter = EvictionIterator::new(6);

        // Set some offset
        iter.bucket_file_offset = 50000;

        // Advancing resets the offset
        iter.advance_to_next_bucket(6);
        assert_eq!(iter.bucket_file_offset, 0);

        // Manual reset
        iter.bucket_file_offset = 99999;
        iter.reset_offset();
        assert_eq!(iter.bucket_file_offset, 0);
    }

    #[test]
    fn test_default_settings() {
        let settings = default_state_archival_settings();
        assert_eq!(settings.eviction_scan_size, DEFAULT_EVICTION_SCAN_SIZE);
        assert_eq!(settings.eviction_scan_size, 100_000);
        assert_eq!(
            settings.starting_eviction_scan_level,
            DEFAULT_STARTING_EVICTION_SCAN_LEVEL
        );
        assert_eq!(settings.starting_eviction_scan_level, 6);
    }

    #[test]
    fn test_eviction_iterator_with_default_level() {
        let iter = EvictionIterator::with_default_level();
        assert_eq!(iter.bucket_file_offset, 0);
        assert_eq!(iter.bucket_list_level, DEFAULT_STARTING_EVICTION_SCAN_LEVEL);
        assert!(iter.is_curr_bucket);
    }

    #[test]
    fn test_round_down() {
        // Test internal round_down function behavior
        // round_down(value, modulo) = value & !(modulo - 1)
        assert_eq!(round_down(0, 4), 0);
        assert_eq!(round_down(1, 4), 0);
        assert_eq!(round_down(3, 4), 0);
        assert_eq!(round_down(4, 4), 4);
        assert_eq!(round_down(5, 4), 4);
        assert_eq!(round_down(7, 4), 4);
        assert_eq!(round_down(8, 4), 8);

        assert_eq!(round_down(100, 32), 96);
        assert_eq!(round_down(127, 64), 64);
        assert_eq!(round_down(128, 64), 128);
    }

    // --- EvictionResult::resolve() tests ---

    use stellar_xdr::curr::{
        ContractDataDurability, ContractDataEntry, ContractId, ExtensionPoint, Hash, LedgerEntry,
        LedgerEntryData, LedgerEntryExt, ScAddress, ScVal,
    };

    fn make_contract_data_candidate(key_bytes: [u8; 32], is_temporary: bool) -> EvictionCandidate {
        make_contract_data_candidate_with_ttl(key_bytes, is_temporary, 0)
    }

    fn make_contract_data_candidate_with_ttl(
        key_bytes: [u8; 32],
        is_temporary: bool,
        live_until_ledger: u32,
    ) -> EvictionCandidate {
        let entry = LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::ContractData(ContractDataEntry {
                ext: ExtensionPoint::V0,
                contract: ScAddress::Contract(ContractId(Hash(key_bytes))),
                key: ScVal::Void,
                durability: if is_temporary {
                    ContractDataDurability::Temporary
                } else {
                    ContractDataDurability::Persistent
                },
                val: ScVal::Void,
            }),
            ext: LedgerEntryExt::V0,
        };
        EvictionCandidate::new(
            entry,
            EvictionIterator::with_default_level(),
            live_until_ledger,
        )
    }

    #[test]
    fn test_resolve_filters_modified_ttl_keys() {
        let candidate = make_contract_data_candidate([1u8; 32], true);
        let ttl_key = candidate.ttl_key();

        let result = EvictionResult {
            candidates: vec![candidate],
            end_iterator: EvictionIterator::with_default_level(),
            bytes_scanned: 1000,
            scan_complete: true,
            ..Default::default()
        };

        let mut modified = std::collections::HashSet::new();
        modified.insert(ttl_key);

        let resolved = result.resolve(10, &modified, 0, None);
        assert!(
            resolved.deleted_keys.is_empty(),
            "candidate with modified TTL should be filtered out"
        );
    }

    #[test]
    fn test_resolve_keeps_unmodified_candidates() {
        let candidate = make_contract_data_candidate([1u8; 32], true);

        let result = EvictionResult {
            candidates: vec![candidate],
            end_iterator: EvictionIterator::with_default_level(),
            bytes_scanned: 1000,
            scan_complete: true,
            ..Default::default()
        };

        let modified = std::collections::HashSet::new(); // empty

        let resolved = result.resolve(10, &modified, 0, None);
        assert_eq!(
            resolved.deleted_keys.len(),
            2,
            "unmodified temp candidate should be evicted (data_key + ttl_key in deleted_keys)"
        );
    }

    #[test]
    fn test_resolve_logs_modified_live_entry_but_keeps_it() {
        // When a candidate's data key is modified but its TTL key is NOT,
        // the candidate should still be kept (not filtered), but an error
        // should be logged. This test verifies the candidate is NOT removed.
        let candidate = make_contract_data_candidate([1u8; 32], true);
        let data_key = candidate.data_key();

        let result = EvictionResult {
            candidates: vec![candidate],
            end_iterator: EvictionIterator::with_default_level(),
            bytes_scanned: 1000,
            scan_complete: true,
            ..Default::default()
        };

        let mut modified = std::collections::HashSet::new();
        modified.insert(data_key); // data key modified, but TTL key is NOT

        let resolved = result.resolve(10, &modified, 0, None);
        // Parity: stellar-core logs REPORT_INTERNAL_BUG but still proceeds
        // with eviction (++iter, not erase). The candidate should still be
        // evicted.
        assert_eq!(
            resolved.deleted_keys.len(),
            2,
            "candidate with modified data key but unmodified TTL should still be evicted"
        );
    }

    #[test]
    fn test_resolve_filters_ttl_not_data_when_both_modified() {
        // When BOTH the TTL key and data key are in modified_keys,
        // the TTL check fires first and filters out the candidate.
        let candidate = make_contract_data_candidate([1u8; 32], true);
        let data_key = candidate.data_key();
        let ttl_key = candidate.ttl_key();

        let result = EvictionResult {
            candidates: vec![candidate],
            end_iterator: EvictionIterator::with_default_level(),
            bytes_scanned: 1000,
            scan_complete: true,
            ..Default::default()
        };

        let mut modified = std::collections::HashSet::new();
        modified.insert(data_key);
        modified.insert(ttl_key);

        let resolved = result.resolve(10, &modified, 0, None);
        assert!(
            resolved.deleted_keys.is_empty(),
            "candidate should be filtered out because TTL was modified"
        );
    }

    #[test]
    fn test_resolve_max_entries_limit() {
        let c1 = make_contract_data_candidate([1u8; 32], true);
        let c2 = make_contract_data_candidate([2u8; 32], true);
        let c3 = make_contract_data_candidate([3u8; 32], true);

        let result = EvictionResult {
            candidates: vec![c1, c2, c3],
            end_iterator: EvictionIterator::with_default_level(),
            bytes_scanned: 3000,
            scan_complete: true,
            ..Default::default()
        };

        let modified = std::collections::HashSet::new();

        // Limit to 2 entries
        let resolved = result.resolve(2, &modified, 0, None);
        assert_eq!(
            resolved.deleted_keys.len(),
            4,
            "should evict 2 temp entries (4 keys in deleted_keys: 2 data + 2 TTL)"
        );
    }

    #[test]
    fn test_resolve_persistent_entries_archived() {
        let candidate = make_contract_data_candidate([1u8; 32], false); // persistent

        let result = EvictionResult {
            candidates: vec![candidate],
            end_iterator: EvictionIterator::with_default_level(),
            bytes_scanned: 1000,
            scan_complete: true,
            ..Default::default()
        };

        let modified = std::collections::HashSet::new();
        let resolved = result.resolve(10, &modified, 0, None);

        assert_eq!(
            resolved.archived_entries.len(),
            1,
            "persistent entry should be archived"
        );
        assert_eq!(
            resolved.deleted_keys.len(),
            1,
            "persistent entry should have only TTL key in deleted_keys"
        );
        assert_eq!(
            resolved.evicted_keys().len(),
            2,
            "evicted_keys() should include TTL key + persistent data key"
        );
    }

    #[test]
    fn test_resolve_evicted_keys_ordering_matches_stellar_core() {
        // Parity test: stellar-core builds evictedKeys as:
        //   deletedKeys (temp data + ALL TTL keys in scan order)
        //   ++ persistent data keys (from archivedEntries, in scan order)
        //
        // With candidates in scan order: [temp1, persistent2, temp3, persistent4],
        // the expected evicted_keys() ordering is:
        //   temp1_data, temp1_ttl, persistent2_ttl, temp3_data, temp3_ttl, persistent4_ttl,
        //   persistent2_data, persistent4_data
        let temp1 = make_contract_data_candidate([1u8; 32], true);
        let persistent2 = make_contract_data_candidate([2u8; 32], false);
        let temp3 = make_contract_data_candidate([3u8; 32], true);
        let persistent4 = make_contract_data_candidate([4u8; 32], false);

        let temp1_data = temp1.data_key();
        let temp1_ttl = temp1.ttl_key();
        let persistent2_data = persistent2.data_key();
        let persistent2_ttl = persistent2.ttl_key();
        let temp3_data = temp3.data_key();
        let temp3_ttl = temp3.ttl_key();
        let persistent4_data = persistent4.data_key();
        let persistent4_ttl = persistent4.ttl_key();

        let result = EvictionResult {
            candidates: vec![temp1, persistent2, temp3, persistent4],
            end_iterator: EvictionIterator::with_default_level(),
            bytes_scanned: 4000,
            scan_complete: true,
            ..Default::default()
        };

        let modified = std::collections::HashSet::new();
        let resolved = result.resolve(10, &modified, 0, None);

        let evicted = resolved.evicted_keys();

        // Phase 1: deleted_keys — temp data + ALL TTL keys in scan order
        // Phase 2: persistent data keys from archived_entries in scan order
        let expected = vec![
            temp1_data,
            temp1_ttl,
            persistent2_ttl,
            temp3_data,
            temp3_ttl,
            persistent4_ttl,
            persistent2_data,
            persistent4_data,
        ];

        assert_eq!(
            evicted, expected,
            "evicted_keys() must match stellar-core ordering: \
             deleted_keys (temp data + all TTL) first, then persistent data keys"
        );
    }

    #[test]
    fn test_resolve_evicted_keys_ordering_with_max_entries_limit() {
        // When max_entries_to_archive limits processing, the ordering rule
        // still applies to the subset that was processed.
        let temp1 = make_contract_data_candidate([1u8; 32], true);
        let persistent2 = make_contract_data_candidate([2u8; 32], false);
        let temp3 = make_contract_data_candidate([3u8; 32], true);

        let temp1_data = temp1.data_key();
        let temp1_ttl = temp1.ttl_key();
        let persistent2_data = persistent2.data_key();
        let persistent2_ttl = persistent2.ttl_key();

        let result = EvictionResult {
            candidates: vec![temp1, persistent2, temp3],
            end_iterator: EvictionIterator::with_default_level(),
            bytes_scanned: 3000,
            scan_complete: true,
            ..Default::default()
        };

        let modified = std::collections::HashSet::new();
        // Only process first 2 entries (temp1 + persistent2)
        let resolved = result.resolve(2, &modified, 0, None);

        let evicted = resolved.evicted_keys();
        let expected = vec![temp1_data, temp1_ttl, persistent2_ttl, persistent2_data];

        assert_eq!(
            evicted, expected,
            "with max_entries=2, only first 2 candidates should be processed, \
             still with correct two-phase ordering"
        );
    }

    #[test]
    fn test_resolve_evicted_keys_ordering_persistent_only() {
        // When all entries are persistent, deleted_keys has only TTL keys,
        // and persistent data keys are appended at end.
        let p1 = make_contract_data_candidate([1u8; 32], false);
        let p2 = make_contract_data_candidate([2u8; 32], false);

        let p1_data = p1.data_key();
        let p1_ttl = p1.ttl_key();
        let p2_data = p2.data_key();
        let p2_ttl = p2.ttl_key();

        let result = EvictionResult {
            candidates: vec![p1, p2],
            end_iterator: EvictionIterator::with_default_level(),
            bytes_scanned: 2000,
            scan_complete: true,
            ..Default::default()
        };

        let modified = std::collections::HashSet::new();
        let resolved = result.resolve(10, &modified, 0, None);

        let evicted = resolved.evicted_keys();
        let expected = vec![p1_ttl, p2_ttl, p1_data, p2_data];

        assert_eq!(
            evicted, expected,
            "persistent-only: TTL keys first in scan order, then data keys in scan order"
        );
    }

    // --- EvictionCandidate constructor tests ---

    #[test]
    fn test_eviction_candidate_constructor_derives_correctly() {
        let candidate = make_contract_data_candidate([42u8; 32], true);
        // data_key derived from entry
        assert_eq!(
            candidate.data_key(),
            henyey_common::entry_to_key(candidate.entry())
        );
        // ttl_key derived from data_key
        assert_eq!(
            candidate.ttl_key(),
            get_ttl_key(&candidate.data_key()).unwrap()
        );
        // is_temporary derived from entry
        assert!(candidate.is_temporary());
        assert_eq!(
            candidate.is_temporary(),
            is_temporary_entry(candidate.entry())
        );
    }

    #[test]
    fn test_eviction_candidate_persistent() {
        let candidate = make_contract_data_candidate([7u8; 32], false);
        assert!(!candidate.is_temporary());
        assert_eq!(
            candidate.is_temporary(),
            is_temporary_entry(candidate.entry())
        );
    }

    #[test]
    #[should_panic(expected = "Soroban entry")]
    fn test_eviction_candidate_rejects_non_soroban_entry() {
        use stellar_xdr::curr::{AccountEntry, AccountId, PublicKey, Thresholds, Uint256};
        let non_soroban_entry = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0; 32]))),
                balance: 100,
                seq_num: stellar_xdr::curr::SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: stellar_xdr::curr::String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: stellar_xdr::curr::VecM::default(),
                ext: stellar_xdr::curr::AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        };
        // Should panic — non-Soroban entries cannot be eviction candidates
        EvictionCandidate::new(non_soroban_entry, EvictionIterator::with_default_level(), 0);
    }

    // --- resolve() iterator edge case tests ---

    #[test]
    fn test_resolve_max_entries_zero_uses_scan_end() {
        let candidate = make_contract_data_candidate([1u8; 32], true);

        let scan_end = EvictionIterator {
            bucket_list_level: 3,
            is_curr_bucket: false,
            bucket_file_offset: 9999,
        };

        let result = EvictionResult {
            candidates: vec![candidate],
            end_iterator: scan_end.clone(),
            bytes_scanned: 1000,
            scan_complete: true,
            ..Default::default()
        };

        let modified = std::collections::HashSet::new();
        // max_entries_to_archive = 0 means no eviction, use scan end
        let resolved = result.resolve(0, &modified, 0, None);
        assert!(resolved.deleted_keys.is_empty());
        assert!(resolved.archived_entries.is_empty());
        assert_eq!(resolved.end_iterator, scan_end);
    }

    #[test]
    fn test_resolve_all_filtered_uses_scan_end() {
        let c1 = make_contract_data_candidate([1u8; 32], true);
        let c2 = make_contract_data_candidate([2u8; 32], true);
        let ttl1 = c1.ttl_key();
        let ttl2 = c2.ttl_key();

        let scan_end = EvictionIterator {
            bucket_list_level: 2,
            is_curr_bucket: true,
            bucket_file_offset: 5000,
        };

        let result = EvictionResult {
            candidates: vec![c1, c2],
            end_iterator: scan_end.clone(),
            bytes_scanned: 2000,
            scan_complete: true,
            ..Default::default()
        };

        // All TTL keys are modified — all candidates filtered out
        let mut modified = std::collections::HashSet::new();
        modified.insert(ttl1);
        modified.insert(ttl2);

        let resolved = result.resolve(10, &modified, 0, None);
        assert!(resolved.deleted_keys.is_empty());
        assert!(resolved.archived_entries.is_empty());
        // When all are filtered (remaining > 0), use scan end iterator
        assert_eq!(resolved.end_iterator, scan_end);
    }

    // --- EvictionResult::is_valid() tests (BUCKETLISTDB §12.5/§12.6) ---

    fn make_eviction_result_with_initial(
        initial_ledger_seq: u32,
        initial_ledger_version: u32,
        initial_archival_settings: StateArchivalSettings,
    ) -> EvictionResult {
        EvictionResult {
            candidates: Vec::new(),
            end_iterator: EvictionIterator::with_default_level(),
            bytes_scanned: 0,
            scan_complete: true,
            initial_ledger_seq,
            initial_ledger_version,
            initial_archival_settings,
        }
    }

    #[test]
    fn test_eviction_result_is_valid_rejects_ledger_seq_change() {
        let sas = default_state_archival_settings();
        let result = make_eviction_result_with_initial(100, 23, sas.clone());

        // Same ledger_seq → valid.
        assert!(result.is_valid(100, 23, &sas));
        // Different ledger_seq → invalid (scan was based on a different ledger).
        assert!(!result.is_valid(150, 23, &sas));
    }

    #[test]
    fn test_eviction_result_is_valid_rejects_v23_crossing() {
        let sas = default_state_archival_settings();

        // Scan started pre-V23 (22), resolve ledger crosses into V23 → invalid.
        let pre_v23 = make_eviction_result_with_initial(100, 22, sas.clone());
        assert!(!pre_v23.is_valid(100, 23, &sas));

        // Scan started at V23, resolve at V24 → no crossing into V23 → valid.
        let at_v23 = make_eviction_result_with_initial(100, 23, sas.clone());
        assert!(at_v23.is_valid(100, 24, &sas));
    }

    #[test]
    fn test_eviction_result_is_valid_rejects_archival_setting_change() {
        let base = default_state_archival_settings();
        let result = make_eviction_result_with_initial(100, 23, base.clone());

        // Each of the three relevant fields changing → invalid.
        let mut changed_max = base.clone();
        changed_max.max_entries_to_archive += 1;
        assert!(!result.is_valid(100, 23, &changed_max));

        let mut changed_scan_size = base.clone();
        changed_scan_size.eviction_scan_size += 1;
        assert!(!result.is_valid(100, 23, &changed_scan_size));

        let mut changed_level = base.clone();
        changed_level.starting_eviction_scan_level += 1;
        assert!(!result.is_valid(100, 23, &changed_level));

        // Changing an unrelated field (min_temporary_ttl) → still valid.
        // Parity: stellar-core's isValid compares only the three eviction fields.
        let mut changed_unrelated = base.clone();
        changed_unrelated.min_temporary_ttl += 1;
        assert!(result.is_valid(100, 23, &changed_unrelated));
    }

    #[test]
    fn test_eviction_result_captures_initial_state() {
        // New coverage: scan_for_eviction_incremental must populate the three
        // initial_* fields from its inputs so is_valid can be enforced later.
        use crate::bucket_list::BucketList;

        let bl = BucketList::new();
        let mut settings = default_state_archival_settings();
        settings.max_entries_to_archive = 7;
        let iter = EvictionIterator::with_default_level();

        let result = bl
            .scan_for_eviction_incremental(iter, 42, 23, &settings)
            .expect("scan should succeed on empty bucket list");

        assert_eq!(result.initial_ledger_seq, 42);
        assert_eq!(result.initial_ledger_version, 23);
        assert_eq!(result.initial_archival_settings.max_entries_to_archive, 7);
    }

    #[test]
    fn test_resolve_filtered_before_limit_uses_last_evicted_position() {
        // Candidates: [filtered, kept, kept] with max_entries=2
        // The filtered candidate should be skipped, and the limit should apply
        // to the unfiltered ones.
        let c1 = make_contract_data_candidate([1u8; 32], true);
        let c2 = make_contract_data_candidate([2u8; 32], true);
        let c3 = make_contract_data_candidate([3u8; 32], true);
        let ttl1 = c1.ttl_key();

        let scan_end = EvictionIterator {
            bucket_list_level: 1,
            is_curr_bucket: true,
            bucket_file_offset: 8000,
        };

        let result = EvictionResult {
            candidates: vec![c1, c2, c3],
            end_iterator: scan_end.clone(),
            bytes_scanned: 3000,
            scan_complete: true,
            ..Default::default()
        };

        // Only c1's TTL is modified — c1 filtered, c2 and c3 are evicted
        let mut modified = std::collections::HashSet::new();
        modified.insert(ttl1);

        let resolved = result.resolve(2, &modified, 0, None);
        // c2 and c3 evicted: 2 data_keys + 2 ttl_keys = 4
        assert_eq!(resolved.deleted_keys.len(), 4);
        // With max_entries=2, exactly 2 evicted, remaining=0 → use last position
        // (not scan_end, because we hit the limit)
        assert_ne!(resolved.end_iterator, scan_end);
    }

    // --- EvictionCounters wiring tests (#3168) ---

    #[test]
    fn test_scan_populates_incomplete_scan_on_oversized_bucket() {
        // `eviction_scan_is_stuck` is true when bucketUpdatePeriod * scanSize is
        // less than the bucket byte size. At level 6 curr the update period is
        // 2^(2*6-1) = 2048, so with scan_size=10 the budget is 20480 bytes.
        let iter = EvictionIterator::new(6);
        let scan_size = 10u32;

        // Oversized: 30000 > 2048 * 10 → stuck.
        assert!(
            eviction_scan_is_stuck(&iter, scan_size, 30_000),
            "an oversized bucket should be reported as stuck"
        );
        // Within budget: 5000 < 20480 → not stuck.
        assert!(
            !eviction_scan_is_stuck(&iter, scan_size, 5_000),
            "a bucket smaller than the scan budget should not be stuck"
        );

        // The scan records one incomplete_bucket_scan per stuck bucket it visits.
        let counters = crate::metrics::EvictionCounters::new();
        if eviction_scan_is_stuck(&iter, scan_size, 30_000) {
            counters.record_incomplete_scan();
        }
        assert_eq!(counters.snapshot().incomplete_bucket_scans, 1);
    }

    #[test]
    fn test_submit_cycle_period_on_wrap() {
        // Exercise the previously-discarded wrap signal: the eviction cycle
        // period is published as (curr_ledger - cycle_start_ledger) on the
        // SECOND wrap (the first establishes the baseline).
        let counters = crate::metrics::EvictionCounters::new();

        // First wrap at ledger 100: baseline, publishes nothing.
        counters.submit_metrics_and_restart_cycle(100);
        assert_eq!(counters.snapshot().eviction_cycle_period, 0);

        // Second wrap at ledger 142: period = 142 - 100 = 42.
        counters.submit_metrics_and_restart_cycle(142);
        assert_eq!(counters.snapshot().eviction_cycle_period, 42);
    }

    #[test]
    fn test_resolve_records_evicted_count_and_age() {
        // Two evicted temp candidates with known live_until_ledger; resolve at
        // ledger 100 should record entries_evicted == 2 and, after the cycle
        // closes, average age = ((100-90) + (100-80)) / 2 = (10 + 20) / 2 = 15.
        let c1 = make_contract_data_candidate_with_ttl([1u8; 32], true, 90);
        let c2 = make_contract_data_candidate_with_ttl([2u8; 32], true, 80);

        let result = EvictionResult {
            candidates: vec![c1, c2],
            end_iterator: EvictionIterator::with_default_level(),
            bytes_scanned: 2000,
            scan_complete: true,
            ..Default::default()
        };

        let counters = crate::metrics::EvictionCounters::new();
        // Establish the cycle baseline so the next submit publishes the gauge.
        counters.submit_metrics_and_restart_cycle(0);

        let modified = std::collections::HashSet::new();
        let resolved = result.resolve(10, &modified, 100, Some(&counters));
        // Both temp entries evicted: 2 data keys + 2 TTL keys.
        assert_eq!(resolved.deleted_keys.len(), 4);

        let snap = counters.snapshot();
        assert_eq!(snap.entries_evicted, 2, "two entries evicted");
        assert_eq!(snap.temp_entries_evicted, 2, "both were temporary");

        // Close the cycle to fold per-entry ages into the average gauge.
        counters.submit_metrics_and_restart_cycle(200);
        assert_eq!(
            counters.snapshot().average_evicted_entry_age,
            15,
            "average age = ((100-90)+(100-80))/2 = 15"
        );
    }

    #[test]
    fn test_resolve_does_not_record_for_filtered_or_overcap_candidates() {
        // Three candidates; one filtered by a modified TTL, cap = 1 → only one
        // actually evicted, so entries_evicted == 1 (not 3).
        let c1 = make_contract_data_candidate_with_ttl([1u8; 32], true, 90);
        let c2 = make_contract_data_candidate_with_ttl([2u8; 32], true, 90);
        let c3 = make_contract_data_candidate_with_ttl([3u8; 32], true, 90);
        let ttl1 = c1.ttl_key();

        let result = EvictionResult {
            candidates: vec![c1, c2, c3],
            end_iterator: EvictionIterator::with_default_level(),
            bytes_scanned: 3000,
            scan_complete: true,
            ..Default::default()
        };

        let counters = crate::metrics::EvictionCounters::new();
        let mut modified = std::collections::HashSet::new();
        modified.insert(ttl1); // c1 filtered out

        // max=1 → only c2 evicted (c3 over cap).
        let _ = result.resolve(1, &modified, 100, Some(&counters));
        assert_eq!(
            counters.snapshot().entries_evicted,
            1,
            "only the one actually-evicted candidate is counted"
        );
    }
}
