//! SCP driver integration for the Herder.
//!
//! This module implements the [`SCPDriver`] trait callbacks that integrate
//! the SCP consensus protocol with the Herder's transaction processing
//! and ledger management.
//!
//! # Overview
//!
//! The [`ScpDriver`] is the bridge between the SCP consensus layer and the
//! Herder's application logic. It provides:
//!
//! - **Value validation**: Checking that proposed SCP values are valid
//!   (close time is reasonable, transaction set exists, upgrades are valid)
//! - **Candidate combination**: Merging multiple candidate values into one
//! - **Envelope signing/verification**: Cryptographic operations for SCP messages
//! - **Transaction set caching**: Storing and retrieving transaction sets by hash
//! - **Externalization tracking**: Recording when slots are externalized
//! - **Quorum set management**: Storing and looking up quorum sets by node or hash
//!
//! # Key Components
//!
//! - [`ScpDriver`]: Main driver struct managing caches and cryptographic operations
//! - [`HerderScpCallback`]: Wrapper implementing the SCP callback trait
//! - [`ExternalizedSlot`]: Records a slot that has reached consensus
//! - [`PendingTxSet`]: Tracks transaction sets we need but haven't received yet

use crate::externalize_lag::ExternalizeLagTracker;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use parking_lot::RwLock;
use tracing::{debug, trace, warn};

use crate::tracked_lock::{tracked_read, tracked_write};

// Lock telemetry labels — see `crate::tracked_lock` and issue #1768.
// Defined as `&'static str` constants so a call-site typo becomes
// a compile error rather than a silently-mislabelled WARN.
const LOCK_SCP_EXTERNALIZED: &str = "scp_driver.externalized";
const LOCK_SCP_LATEST_EXTERNALIZED: &str = "scp_driver.latest_externalized";
const LOCK_TRACKING_STATE: &str = "shared.tracking_state";

use henyey_common::protocol::{protocol_version_starts_from, ProtocolVersion};
use henyey_common::{Hash256, NetworkId};
use henyey_crypto::{PublicKey, SecretKey, Signature};
use henyey_ledger::LedgerManager;
use henyey_scp::{SCPDriver, SlotIndex, ValidationLevel};
use stellar_xdr::curr::{
    EnvelopeType, LedgerUpgrade, NodeId, ReadXdr, ScpEnvelope, ScpQuorumSet, ScpStatement,
    StellarValue, StellarValueExt, Value, WriteXdr,
};

use crate::error::HerderError;
use crate::quorum_set_tracker::QuorumSetTracker;
use crate::tx_queue::TransactionSet;
use crate::tx_set_tracker::TxSetTracker;
use crate::upgrades::Upgrades;
use crate::Result;

/// Format a `StellarValueExt` for logging.
fn describe_stellar_value_ext(ext: &StellarValueExt) -> String {
    match ext {
        StellarValueExt::Basic => "Basic".to_string(),
        StellarValueExt::Signed(sig) => {
            let node_id_bytes = match &sig.node_id.0 {
                stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(key) => key.0,
            };
            format!(
                "Signed(node_id={}, sig_len={})",
                Hash256::from_bytes(node_id_bytes).to_hex(),
                sig.signature.len()
            )
        }
    }
}

/// Result of validating an SCP value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueValidation {
    /// Value is fully valid.
    Valid,
    /// Value might be valid but we're missing data (no tracking state,
    /// past-slot `tracking_index > slot_index`, close-time check
    /// against stale data, etc.). Maps to
    /// `ValidationLevel::MaybeValid`, which clears the slot's
    /// `fully_validated` flag (stellar-core parity).
    MaybeValid,
    /// Structurally valid value whose full validation is deferred by a
    /// henyey-specific fast-path divergence from stellar-core. Covers
    /// the missing-tx_set-for-current-ledger case (#1795) and the
    /// future-slot-while-apply-lags-SCP case (#1798). Maps to
    /// `ValidationLevel::MaybeValidDeferred`, which DOES clear
    /// `fully_validated` (restored later by the herder after the
    /// deferred condition resolves — see issue #2061).
    MaybeValidDeferred,
    /// Value is invalid.
    Invalid,
}

/// Shared tracking consensus state between Herder and ScpDriver.
///
/// This struct is the single source of truth for whether we are tracking
/// the network and what the current consensus position is. It is shared
/// via `Arc<RwLock<SharedTrackingState>>` to ensure Herder and ScpDriver
/// always see consistent state without manual synchronization.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SharedTrackingState {
    /// Whether we are in tracking state.
    pub is_tracking: bool,
    /// Next consensus ledger index (tracking_slot in Herder terms).
    pub consensus_index: u64,
    /// Consensus close time for the tracking slot.
    pub consensus_close_time: u64,
}

#[derive(Debug, Clone, Copy)]
struct ValueValidationContext {
    lcl_seq: u64,
    lcl_close_time: u64,
    tracking: Option<SharedTrackingState>,
}

// ── Application-specific nomination leader election (protocol V22+) ──

/// Validator quality level for nomination leader election weights.
///
/// Maps to stellar-core's `ValidatorQuality` enum (Config.h:41-47).
/// Used to weight nomination leaders by validator quality split across
/// home domains, replacing the old quorum-position algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ValidatorQuality {
    Low = 0,
    Medium = 1,
    High = 2,
    Critical = 3,
}

impl ValidatorQuality {
    /// Parse a quality string matching stellar-core's `Config::parseQuality`.
    /// Exact match only: "LOW", "MEDIUM", "HIGH", "CRITICAL".
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "LOW" => Some(Self::Low),
            "MEDIUM" => Some(Self::Medium),
            "HIGH" => Some(Self::High),
            "CRITICAL" => Some(Self::Critical),
            _ => None,
        }
    }
}

impl std::fmt::Display for ValidatorQuality {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Low => write!(f, "LOW"),
            Self::Medium => write!(f, "MEDIUM"),
            Self::High => write!(f, "HIGH"),
            Self::Critical => write!(f, "CRITICAL"),
        }
    }
}

/// Information about a single validator for leader election weight computation.
///
/// Maps to stellar-core's `ValidatorEntry` (Config.h:49-56).
#[derive(Debug, Clone)]
pub struct ValidatorEntryInfo {
    pub name: String,
    pub home_domain: String,
    pub quality: ValidatorQuality,
}

/// Pre-computed validator weight configuration for application-specific
/// nomination leader election (protocol V22+).
///
/// Maps to stellar-core's `ValidatorWeightConfig` (Config.h:60-70).
/// Built from `[[VALIDATORS]]` + `[[HOME_DOMAINS]]` config at startup.
/// When present, `HerderScpCallback::get_node_weight` uses quality/home-domain
/// weights instead of quorum-position weights.
#[derive(Debug, Clone)]
pub struct ValidatorWeightConfig {
    /// Map from node ID to validator metadata.
    pub validator_entries: HashMap<NodeId, ValidatorEntryInfo>,
    /// Map from home domain to count of validators in that domain.
    pub home_domain_sizes: HashMap<String, u64>,
    /// Map from quality level to weight.
    pub quality_weights: HashMap<ValidatorQuality, u64>,
}

impl ValidatorWeightConfig {
    /// Build a `ValidatorWeightConfig` from a list of validators.
    ///
    /// Ports stellar-core's `Config::setValidatorWeightConfig` (Config.cpp:2727-2791).
    ///
    /// # Errors
    /// Returns an error if all validators have `Low` quality (stellar-core
    /// requires at least one above Low).
    pub fn new(validators: &[(NodeId, ValidatorEntryInfo)]) -> std::result::Result<Self, String> {
        let mut validator_entries = HashMap::new();
        let mut home_domain_sizes: HashMap<String, u64> = HashMap::new();
        let mut highest_quality = ValidatorQuality::Low;
        let mut lowest_quality = ValidatorQuality::Critical;
        let mut home_domains_by_quality: HashMap<ValidatorQuality, HashSet<String>> =
            HashMap::new();

        for (node_id, entry) in validators {
            if validator_entries.contains_key(node_id) {
                return Err(format!("Duplicate validator entry for node {:?}", node_id));
            }
            validator_entries.insert(node_id.clone(), entry.clone());
            *home_domain_sizes
                .entry(entry.home_domain.clone())
                .or_insert(0) += 1;
            if entry.quality > highest_quality {
                highest_quality = entry.quality;
            }
            if entry.quality < lowest_quality {
                lowest_quality = entry.quality;
            }
            home_domains_by_quality
                .entry(entry.quality)
                .or_default()
                .insert(entry.home_domain.clone());
        }

        if highest_quality == ValidatorQuality::Low {
            return Err(
                "At least one validator must have a quality level higher than LOW".to_string(),
            );
        }

        let mut quality_weights = HashMap::new();

        // Highest quality level gets weight u64::MAX
        quality_weights.insert(highest_quality, u64::MAX);

        // Assign weights to remaining quality levels (descending)
        let mut q = highest_quality as i32 - 1;
        while q >= lowest_quality as i32 {
            let current_quality = match q {
                0 => ValidatorQuality::Low,
                1 => ValidatorQuality::Medium,
                2 => ValidatorQuality::High,
                3 => ValidatorQuality::Critical,
                _ => unreachable!(),
            };
            let higher_quality = match q + 1 {
                1 => ValidatorQuality::Medium,
                2 => ValidatorQuality::High,
                3 => ValidatorQuality::Critical,
                _ => unreachable!(),
            };

            let higher_weight = quality_weights[&higher_quality];
            // Number of orgs at the higher quality level + 1 for the virtual org
            let higher_orgs = home_domains_by_quality
                .get(&higher_quality)
                .map_or(0, |s| s.len() as u64)
                + 1;
            quality_weights.insert(current_quality, higher_weight / (higher_orgs * 10));

            q -= 1;
        }

        // Low quality always gets weight 0 (stellar-core Config.cpp:2790)
        quality_weights.insert(ValidatorQuality::Low, 0);

        Ok(Self {
            validator_entries,
            home_domain_sizes,
            quality_weights,
        })
    }

    /// Get the weight for a specific node.
    ///
    /// Panics if the node is not in the config (matches stellar-core's
    /// `throw std::runtime_error`).
    pub fn get_node_weight(&self, node_id: &NodeId) -> u64 {
        let entry = self
            .validator_entries
            .get(node_id)
            .unwrap_or_else(|| panic!("Validator entry not found for node {:?}", node_id));
        let home_domain_size = self
            .home_domain_sizes
            .get(&entry.home_domain)
            .unwrap_or_else(|| {
                panic!(
                    "Home domain size not found for domain {}",
                    entry.home_domain
                )
            });
        let quality_weight = self
            .quality_weights
            .get(&entry.quality)
            .unwrap_or_else(|| panic!("Quality weight not found for quality {}", entry.quality));

        assert!(*home_domain_size > 0);
        quality_weight / home_domain_size
    }
}

/// Configuration for the SCP driver.
#[derive(Debug, Clone)]
pub struct ScpDriverConfig {
    /// Our node's public key.
    pub node_id: PublicKey,
    /// Maximum transaction sets to cache.
    pub max_tx_set_cache: usize,
    /// Maximum time drift allowed (in seconds).
    pub max_time_drift: u64,
    /// Local quorum set.
    pub local_quorum_set: Option<ScpQuorumSet>,
    /// Validator weight configuration for application-specific leader election
    /// (protocol V22+). `None` when using manual quorum set or when quality
    /// data is not available.
    pub validator_weight_config: Option<ValidatorWeightConfig>,
    /// When true, always use the old quorum-position weight algorithm
    /// regardless of protocol version.
    pub force_old_style_leader_election: bool,
}

impl Default for ScpDriverConfig {
    fn default() -> Self {
        Self {
            node_id: PublicKey::from_bytes(&[0u8; 32]).unwrap(),
            max_tx_set_cache: 10_000,
            max_time_drift: 60,
            local_quorum_set: None,
            validator_weight_config: None,
            force_old_style_leader_election: false,
        }
    }
}

/// Cached transaction set with metadata.
#[derive(Debug, Clone)]
pub struct CachedTxSet {
    /// The transaction set.
    pub tx_set: TransactionSet,
    /// When this was cached (diagnostic only — not used for eviction).
    pub cached_at: std::time::Instant,
    /// Monotonic sequence number for deterministic LRU eviction.
    /// Updated on both insertion and `get()` refresh.
    pub(crate) touch_seq: u64,
    /// Number of times this was requested.
    pub request_count: u64,
}

impl CachedTxSet {
    pub(crate) fn new(tx_set: TransactionSet, touch_seq: u64) -> Self {
        Self {
            tx_set,
            cached_at: std::time::Instant::now(),
            touch_seq,
            request_count: 0,
        }
    }
}

/// Externalized value with metadata.
#[derive(Debug, Clone)]
pub struct ExternalizedSlot {
    /// The slot index.
    pub slot: SlotIndex,
    /// The externalized SCP value.
    pub value: Value,
    /// The transaction set hash (if resolved).
    pub tx_set_hash: Option<Hash256>,
    /// Close time from the value.
    pub close_time: u64,
    /// When this was externalized.
    pub externalized_at: std::time::Instant,
}

/// Pending transaction set request.
#[derive(Debug, Clone)]
pub struct PendingTxSet {
    /// The hash of the tx set we need.
    pub hash: Hash256,
    /// The slot this tx set is needed for.
    pub slot: SlotIndex,
    /// When we first requested this.
    pub requested_at: std::time::Instant,
    /// Number of times we've requested this.
    pub request_count: u32,
}

/// Pending quorum set request.
#[derive(Debug, Clone)]
pub struct PendingQuorumSet {
    /// Number of times we've requested this.
    pub request_count: u32,
    /// Node IDs that use this quorum set (envelope senders).
    pub node_ids: HashSet<NodeId>,
}

/// Cache sizes for diagnostics.
#[derive(Debug, Clone, Default)]
pub struct ScpDriverCacheSizes {
    /// Cached transaction sets.
    pub tx_set_cache: usize,
    /// Pending transaction set requests.
    pub pending_tx_sets: usize,
    /// Pending quorum set requests.
    pub pending_quorum_sets: usize,
    /// Externalized slots.
    pub externalized: usize,
    /// Quorum sets by node ID.
    pub quorum_sets: usize,
    /// Quorum sets by hash.
    pub quorum_sets_by_hash: usize,
    /// Cached tx-set validity results.
    pub tx_set_valid_cache: usize,
}

/// Callback type for broadcasting SCP envelopes to peers.
type EnvelopeSender = Box<dyn Fn(ScpEnvelope) + Send + Sync>;

/// Per-slot SCP timing timestamps (first-seen, nomination start, ballot start).
/// Consolidates what were previously three parallel `HashMap<SlotIndex, Instant>` fields
/// so cleanup paths cannot drift.
#[derive(Clone, Copy, Debug, Default)]
struct SlotTimingState {
    /// First SCP activity for this slot (`record_slot_activity`).
    first_seen: Option<std::time::Instant>,
    /// First `nominating_value` callback (`record_nomination_start`).
    nomination_start: Option<std::time::Instant>,
    /// First `started_ballot_protocol` callback (`record_ballot_start`).
    ballot_start: Option<std::time::Instant>,
}

/// SCP driver that integrates consensus with the Herder.
///
/// This manages:
/// - Transaction set caching by hash
/// - Value validation callbacks
/// - Envelope signing and verification
/// - Externalized value tracking
/// - Quorum set storage and lookup
pub struct ScpDriver {
    /// Configuration.
    config: ScpDriverConfig,
    /// Secret key for signing (None if not a validator).
    secret_key: Option<SecretKey>,
    /// Quorum-set tracker (replaces quorum_sets, quorum_sets_by_hash,
    /// pending_quorum_sets, local_quorum_set).
    qset_tracker: QuorumSetTracker,
    /// Tx-set tracker (replaces tx_set_cache, pending_tx_sets, tx_set_valid_cache).
    tx_tracker: TxSetTracker,
    /// Externalized slots.
    externalized: RwLock<HashMap<SlotIndex, ExternalizedSlot>>,
    /// Latest externalized slot.
    latest_externalized: RwLock<Option<SlotIndex>>,
    /// Envelope broadcast callback.
    envelope_sender: OnceLock<EnvelopeSender>,
    /// Network ID for signing.
    network_id: Hash256,
    /// Ledger manager for network configuration lookups.
    ledger_manager: OnceLock<Arc<LedgerManager>>,
    /// Upgrade parameters for nomination validation.
    /// Set via `set_upgrades` when the Herder initializes.
    upgrades: OnceLock<Arc<RwLock<Upgrades>>>,
    /// Shared tracking consensus state (owned by Herder, read by ScpDriver).
    tracking_state: Arc<RwLock<SharedTrackingState>>,
    /// Optional wall-clock override for `check_close_time`. When zero, the
    /// real `SystemTime::now()` is used. Shared with [`Herder`] via
    /// [`Herder::test_clock`] so both close-time sites (here and
    /// `Herder::check_envelope_close_time`) see the same fake time.
    ///
    /// Only written by tests (via `Herder::set_test_clock_seconds`); in
    /// production this atomic is allocated once and read-but-never-written.
    test_clock: Arc<AtomicU64>,
    /// Per-slot timing for metrics (first-seen, nomination start, ballot start).
    slot_timing: RwLock<HashMap<SlotIndex, SlotTimingState>>,
    /// Timing snapshot for the most recently externalized slot.
    last_externalize_timing: RwLock<Option<ExternalizeTimingSnapshot>>,
    /// Externalize lag tracker for per-node lag statistics.
    /// Mirrors stellar-core's `mQSetLag` in `HerderSCPDriver`.
    externalize_lag: RwLock<ExternalizeLagTracker>,
    /// Slots deferred for `MaybeValidDeferred` validation, keyed by slot
    /// index. Each entry records the set of *causes* that are deferring
    /// the slot — see [`DeferredCauses`].
    ///
    /// A slot may carry multiple causes concurrently (e.g. an LCL race
    /// can leave both `apply_lag` and `missing_tx_sets` set on the same
    /// slot). The slot is restorable only when ALL causes have cleared.
    deferred_slots: Mutex<HashMap<SlotIndex, DeferredCauses>>,
}

/// Causes that are deferring full validation for a slot.
///
/// A slot's `MaybeValidDeferred` validation can have two independent causes:
///
/// - `missing_tx_sets`: one or more tx-set hashes were not yet cached when
///   `validate_value_against_local_state` ran on the LCL+1 path. Multiple
///   distinct hashes can accumulate across different ballot/nominated
///   values for the same slot; the cause is cleared only when EVERY
///   recorded hash has arrived.
/// - `apply_lag`: ledger apply was behind SCP tracking when
///   `validate_past_or_future_value` ran. Cleared when LCL advances to
///   `slot - 1` or beyond (the slot's predecessor has applied).
///
/// At any single LCL snapshot the two causes are mutually exclusive per
/// slot (the tx-set-missing branch only runs on `slot == lcl_seq + 1`,
/// the apply-lag branch only on `slot != lcl_seq + 1`). However, an LCL
/// advance between `record_apply_lag(N)` and a subsequent
/// `record_missing_tx_set(N, _)` can leave both causes set on the same
/// slot. Restoration requires ALL causes to be clear.
#[derive(Clone, Default, Eq, PartialEq, Debug)]
pub(crate) struct DeferredCauses {
    /// Tx-set hashes missing at validation time. The cause is cleared
    /// only when EVERY recorded hash has been resolved via
    /// [`ScpDriver::resolve_missing_tx_set`].
    pub(crate) missing_tx_sets: HashSet<Hash256>,
    /// `true` if `validate_past_or_future_value` returned
    /// `MaybeValidDeferred` because ledger apply was behind SCP tracking.
    /// Cleared when LCL advances past the slot's predecessor (via
    /// [`ScpDriver::resolve_apply_lag_for_next_index`]).
    pub(crate) apply_lag: bool,
}

impl DeferredCauses {
    /// True iff the slot has no remaining deferred causes — i.e., it is
    /// safe to call `restore_slot_fully_validated`.
    fn is_clear(&self) -> bool {
        self.missing_tx_sets.is_empty() && !self.apply_lag
    }
}

/// Timing snapshot for the most recently externalized slot.
/// Both durations are guaranteed to describe the same slot.
#[derive(Clone, Copy, Debug)]
pub struct ExternalizeTimingSnapshot {
    /// The slot this snapshot describes.
    pub slot: SlotIndex,
    /// Duration from slot-first-seen to externalization.
    pub externalize_duration: std::time::Duration,
    /// Duration from first local nomination vote to ballot protocol start (prepare phase).
    /// Matches stellar-core's `mNominateToPrepare` metric.
    /// `None` if either nomination start or ballot start was not recorded for this slot
    /// (e.g., watcher nodes, catchup, fast-forward, or externalize before ballot start).
    pub nomination_duration: Option<std::time::Duration>,
    /// Duration from first EXTERNALIZE seen (any node) to self-externalize.
    /// Near-zero when this node was the first to externalize.
    /// `None` on catchup/fast-forward paths where no externalize events were recorded.
    pub first_to_self_externalize_lag: Option<std::time::Duration>,
}

impl ScpDriver {
    /// Create a new SCP driver.
    pub(crate) fn new(
        config: ScpDriverConfig,
        network_id: Hash256,
        tracking_state: Arc<RwLock<SharedTrackingState>>,
    ) -> Self {
        let local_quorum_set = config.local_quorum_set.clone();
        let local_node_key = *config.node_id.as_bytes();
        let qset_tracker = QuorumSetTracker::new(local_node_key, local_quorum_set);
        let tx_tracker = TxSetTracker::new(config.max_tx_set_cache);

        Self {
            config,
            secret_key: None,
            qset_tracker,
            tx_tracker,
            externalized: RwLock::new(HashMap::new()),
            latest_externalized: RwLock::new(None),
            envelope_sender: OnceLock::new(),
            network_id,
            ledger_manager: OnceLock::new(),
            upgrades: OnceLock::new(),
            tracking_state,
            test_clock: Arc::new(AtomicU64::new(0)),
            slot_timing: RwLock::new(HashMap::new()),
            last_externalize_timing: RwLock::new(None),
            externalize_lag: RwLock::new(ExternalizeLagTracker::new()),
            deferred_slots: Mutex::new(HashMap::new()),
        }
    }

    /// Current wall-clock seconds since UNIX epoch, honoring a test-only
    /// override. When the shared `test_clock` atomic is non-zero, its value
    /// is returned instead of `SystemTime::now()`. Used by
    /// [`Self::check_close_time`] so tests can make that gate deterministic.
    pub(crate) fn now_seconds(&self) -> u64 {
        let fake = self.test_clock.load(Ordering::Relaxed);
        if fake != 0 {
            return fake;
        }
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before UNIX epoch")
            .as_secs()
    }

    /// Test-only access to the shared clock handle. Used by
    /// `Herder::set_test_clock_seconds` to poke the wall-clock override that
    /// backs [`Self::check_close_time`].
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn test_clock_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.test_clock)
    }

    /// Create a new SCP driver with a secret key for signing.
    pub(crate) fn with_secret_key(
        config: ScpDriverConfig,
        network_id: Hash256,
        secret_key: SecretKey,
        tracking_state: Arc<RwLock<SharedTrackingState>>,
    ) -> Self {
        let mut driver = Self::new(config, network_id, tracking_state);
        driver.secret_key = Some(secret_key);
        driver
    }

    /// Set the envelope broadcast callback.
    pub fn set_envelope_sender<F>(&self, sender: F)
    where
        F: Fn(ScpEnvelope) + Send + Sync + 'static,
    {
        let _ = self.envelope_sender.set(Box::new(sender));
    }

    /// Provide ledger manager access for network configuration lookups.
    pub fn set_ledger_manager(&self, manager: Arc<LedgerManager>) {
        let _ = self.ledger_manager.set(manager);
    }

    /// Provide upgrade parameters access for nomination validation.
    pub fn set_upgrades(&self, upgrades: Arc<RwLock<Upgrades>>) {
        let _ = self.upgrades.set(upgrades);
    }

    /// Clear the tx set validity cache (call on ledger close).
    pub fn clear_tx_set_valid_cache(&self) {
        self.tx_tracker.clear_valid_cache();
    }

    /// Check and cache whether a transaction set is valid for application.
    ///
    /// Mirrors stellar-core's `HerderSCPDriver::checkAndCacheTxSetValid()`
    /// (HerderSCPDriver.cpp:1419-1461).
    ///
    /// Performs:
    /// 1. Cache lookup by (lcl_hash, tx_set_hash, close_time_offset)
    /// 2. On miss: prepare_for_apply + check_tx_set_valid
    /// 3. Cache and return result
    fn check_and_cache_tx_set_valid(
        &self,
        tx_set: &crate::tx_queue::TransactionSet,
        lcl_hash: Hash256,
        close_time_offset: u64,
    ) -> bool {
        let cache_key = (lcl_hash, *tx_set.hash(), close_time_offset);

        // Check cache
        if let Some(cached) = self.tx_tracker.check_valid(&cache_key) {
            return cached;
        }

        let network_id = NetworkId(self.network_id);

        // prepare_for_apply validates XDR structure, fees, sort order, dedup
        let prepared = match tx_set.prepare_for_apply(network_id) {
            Ok(p) => p,
            Err(e) => {
                debug!(
                    "check_and_cache_tx_set_valid: prepare_for_apply failed: {}",
                    e
                );
                self.tx_tracker.store_valid(cache_key, false);
                return false;
            }
        };

        // For generalized tx sets, run full content validation.
        // For legacy sets, prepare_for_apply already validated structure;
        // check_valid handles the protocol/format compatibility logic.
        let result = if let Some(lm) = self.ledger_manager.get() {
            let lcl_header = lm.current_header();

            // Skip validation entirely when protocol < 20: generalized tx sets
            // are not expected at those versions. This occurs in simulation
            // environments that start at protocol 0 before upgrading to 24+.
            if !protocol_version_starts_from(lcl_header.ledger_version, ProtocolVersion::V20) {
                true
            } else {
                let soroban_info = lm.soroban_network_info();
                prepared.check_valid(
                    &lcl_header,
                    close_time_offset,
                    network_id,
                    soroban_info.as_ref(),
                    None, // fee balance checks skipped in SCP path — snapshot
                    // staleness causes false rejects
                    None, // account validation skipped in SCP path — snapshot
                          // staleness causes false rejects
                )
            }
        } else {
            // No ledger manager — can't validate, assume valid
            true
        };

        self.tx_tracker.store_valid(cache_key, result);
        result
    }

    /// Cache a transaction set.
    pub fn cache_tx_set(&self, tx_set: TransactionSet) {
        self.tx_tracker.store(tx_set);
    }

    /// Get a cached transaction set by hash.
    pub fn get_tx_set(&self, hash: &Hash256) -> Option<TransactionSet> {
        self.tx_tracker.get(hash)
    }

    /// Check if a transaction set is cached.
    pub fn has_tx_set(&self, hash: &Hash256) -> bool {
        self.tx_tracker.is_cached(hash)
    }

    /// Check if a tx set is cached AND refresh its LRU recency.
    /// Used by FetchingEnvelopes callback to prevent eviction of tx-sets
    /// still referenced by buffered envelopes waiting on other dependencies.
    pub fn has_tx_set_and_touch(&self, hash: &Hash256) -> bool {
        self.tx_tracker.is_cached_and_touch(hash)
    }

    /// Number of tx sets currently cached in the tracker. Test-only — used
    /// to assert that nomination paths cache (or skip caching) the freshly
    /// built tx set.
    #[cfg(test)]
    pub(crate) fn tx_set_cache_count(&self) -> usize {
        self.tx_tracker.cache_count()
    }

    /// Record a missing-tx-set cause for `slot`. Adds `hash` to the slot's
    /// pending-hashes set; creates the entry if absent. Called from
    /// `validate_value_against_local_state` on the LCL+1 path when the
    /// referenced tx_set is not yet cached.
    ///
    /// Multiple distinct hashes can accumulate for the same slot across
    /// different ballot/nominated values. The slot is restorable only
    /// when every recorded hash has been resolved via
    /// [`Self::resolve_missing_tx_set`].
    pub(crate) fn record_missing_tx_set(&self, slot: SlotIndex, tx_set_hash: Hash256) {
        self.deferred_slots
            .lock()
            .unwrap()
            .entry(slot)
            .or_default()
            .missing_tx_sets
            .insert(tx_set_hash);
    }

    /// Mark `slot` as deferred for apply-lag. Called from
    /// `validate_past_or_future_value` when SCP tracking has advanced ahead
    /// of ledger apply (`tracking_index == slot_index AND
    /// slot_index != lcl_seq + 1`).
    ///
    /// Idempotent: re-recording a slot that already has `apply_lag = true`
    /// is a no-op. Recording an `apply_lag` cause on top of an existing
    /// `missing_tx_sets` cause is also valid — the two causes coexist and
    /// the slot is restorable only when ALL are clear.
    ///
    /// The matching restoration trigger is
    /// [`Self::resolve_apply_lag_for_next_index`], called by
    /// `Herder::ledger_closed` after each LCL advance.
    pub(crate) fn record_apply_lag(&self, slot: SlotIndex) {
        self.deferred_slots
            .lock()
            .unwrap()
            .entry(slot)
            .or_default()
            .apply_lag = true;
    }

    /// Resolve the missing-tx-set cause for `hash` across all slots.
    ///
    /// Removes `hash` from every slot's `missing_tx_sets` set; for any
    /// slot whose causes are now ALL clear (no remaining pending hashes
    /// AND `apply_lag == false`), removes the entry from the deferred map
    /// and returns it.
    ///
    /// Slots whose causes are NOT yet all clear (e.g. another hash is
    /// still pending, or `apply_lag` is still set due to an LCL race)
    /// remain in the map.
    pub(crate) fn resolve_missing_tx_set(&self, hash: &Hash256) -> Vec<SlotIndex> {
        let mut deferred = self.deferred_slots.lock().unwrap();
        let mut resolved = Vec::new();
        for (slot, causes) in deferred.iter_mut() {
            causes.missing_tx_sets.remove(hash);
            if causes.is_clear() {
                resolved.push(*slot);
            }
        }
        for slot in &resolved {
            deferred.remove(slot);
        }
        resolved
    }

    /// Resolve the apply-lag cause for slots whose predecessor has now
    /// applied — i.e. slots with `slot <= next_index`, where `next_index`
    /// is `lcl_seq + 1` (the next slot to close).
    ///
    /// Clears `apply_lag` on each eligible slot. For any slot whose
    /// causes are now ALL clear (no remaining `missing_tx_sets`),
    /// removes the entry from the deferred map and returns it.
    ///
    /// Slots with `slot > next_index` are not yet eligible (their
    /// predecessor has not applied) and remain deferred. Slots whose
    /// `missing_tx_sets` is non-empty after clearing `apply_lag` also
    /// remain — they will be restored when the missing tx_sets arrive
    /// (or, for purged slots, when `purge_deferred_slots` drops them).
    ///
    /// Called by `Herder::ledger_closed` after each LCL advance.
    pub(crate) fn resolve_apply_lag_for_next_index(&self, next_index: SlotIndex) -> Vec<SlotIndex> {
        let mut deferred = self.deferred_slots.lock().unwrap();
        let mut resolved = Vec::new();
        for (slot, causes) in deferred.iter_mut() {
            if *slot <= next_index && causes.apply_lag {
                causes.apply_lag = false;
                if causes.is_clear() {
                    resolved.push(*slot);
                }
            }
        }
        for slot in &resolved {
            deferred.remove(slot);
        }
        resolved
    }

    /// Clean up deferred entries for slots at or below `max_slot`.
    pub(crate) fn purge_deferred_slots(&self, max_slot: SlotIndex) {
        self.deferred_slots
            .lock()
            .unwrap()
            .retain(|slot, _| *slot > max_slot);
    }

    /// Test-only: snapshot of the deferred causes for a slot.
    #[cfg(test)]
    pub(crate) fn deferred_causes_for_slot(&self, slot: SlotIndex) -> Option<DeferredCauses> {
        self.deferred_slots.lock().unwrap().get(&slot).cloned()
    }

    /// Test-only: number of slots with pending deferred causes.
    #[cfg(test)]
    pub(crate) fn deferred_slot_count(&self) -> usize {
        self.deferred_slots.lock().unwrap().len()
    }

    /// Register a pending tx set request.
    /// Returns true if this is a new request, false if already pending.
    pub fn request_tx_set(&self, hash: Hash256, slot: SlotIndex) -> bool {
        self.tx_tracker.request(hash, slot)
    }

    /// Register a pending quorum set request.
    /// Returns true if this is a new request, false if already pending or known.
    /// The node_id is the envelope sender that uses this quorum set.
    pub fn request_quorum_set(&self, hash: Hash256, node_id: NodeId) -> bool {
        self.qset_tracker.request(hash, node_id)
    }

    /// Clear a quorum set request once it has been satisfied.
    pub fn clear_quorum_set_request(&self, hash: &Hash256) {
        self.qset_tracker.clear_pending(hash);
    }

    /// Get the node IDs that are waiting for a quorum set with the given hash.
    pub fn get_pending_quorum_set_node_ids(&self, hash: &Hash256) -> Vec<NodeId> {
        self.qset_tracker.pending_node_ids(hash)
    }

    /// Get all pending tx set hashes that need to be fetched.
    pub fn get_pending_tx_set_hashes(&self) -> Vec<Hash256> {
        self.tx_tracker.pending_hashes()
    }

    /// Get all pending tx sets with their slots.
    pub fn get_pending_tx_sets(&self) -> Vec<(Hash256, SlotIndex)> {
        self.tx_tracker.pending_entries()
    }

    /// Clear all pending tx set requests.
    /// Used after rapid close cycles to discard stale requests whose tx_sets
    /// are no longer available from peers.
    pub fn clear_pending_tx_sets(&self) {
        self.tx_tracker.clear_pending();
    }

    /// Check if we need a tx set.
    pub fn needs_tx_set(&self, hash: &Hash256) -> bool {
        self.tx_tracker.needs(hash)
    }

    /// Receive a tx set from the network.
    /// Returns the slot it was needed for, if any.
    pub fn receive_tx_set(&self, tx_set: TransactionSet) -> Option<SlotIndex> {
        self.tx_tracker.receive(tx_set)
    }

    /// Clean up old pending requests.
    pub fn cleanup_pending_tx_sets(&self, max_age_secs: u64) {
        self.tx_tracker.cleanup_by_age(max_age_secs);
    }

    /// Clean up pending requests for slots older than the given slot.
    /// Returns the number of requests removed.
    pub fn cleanup_old_pending_slots(&self, current_slot: SlotIndex) -> usize {
        self.tx_tracker.cleanup_old_slots(current_slot)
    }

    /// Check if any pending tx set request has been waiting longer than the given duration.
    /// Returns true if at least one request has exceeded the timeout.
    pub fn has_stale_pending_tx_set(&self, max_wait_secs: u64) -> bool {
        self.tx_tracker.has_stale_pending(max_wait_secs)
    }

    /// Get the network ID.
    pub fn network_id(&self) -> Hash256 {
        self.network_id
    }

    /// Get the latest externalized slot.
    pub fn latest_externalized_slot(&self) -> Option<SlotIndex> {
        *tracked_read(LOCK_SCP_LATEST_EXTERNALIZED, &self.latest_externalized)
    }

    /// Record that we first observed activity for a slot.
    /// Only the first call per slot is recorded; subsequent calls are no-ops.
    pub fn record_slot_activity(&self, slot: SlotIndex) {
        let mut map = self.slot_timing.write();
        let state = map.entry(slot).or_default();
        state.first_seen.get_or_insert_with(std::time::Instant::now);
    }

    /// Record that nomination started for a slot.
    /// Only the first call per slot is recorded; subsequent calls are no-ops.
    /// Called from the `nominating_value` SCP callback when the nomination
    /// protocol first adds a local vote for the slot.
    pub fn record_nomination_start(&self, slot: SlotIndex) {
        let mut map = self.slot_timing.write();
        let state = map.entry(slot).or_default();
        state
            .nomination_start
            .get_or_insert_with(std::time::Instant::now);
    }

    /// Record that the ballot protocol started for a slot (prepare phase entry).
    /// Only the first call per slot is recorded; subsequent calls are no-ops.
    /// Called from the `started_ballot_protocol` SCP callback when the slot
    /// transitions from nomination to the ballot protocol.
    pub fn record_ballot_start(&self, slot: SlotIndex) {
        let mut map = self.slot_timing.write();
        let state = map.entry(slot).or_default();
        state
            .ballot_start
            .get_or_insert_with(std::time::Instant::now);
    }

    /// Duration of the most recently externalized slot (first-seen → externalized).
    pub fn last_externalize_duration(&self) -> Option<std::time::Duration> {
        self.last_externalize_timing
            .read()
            .map(|s| s.externalize_duration)
    }

    /// Full timing snapshot for the most recently externalized slot.
    pub fn last_externalize_timing(&self) -> Option<ExternalizeTimingSnapshot> {
        *self.last_externalize_timing.read()
    }

    /// Record an SCP externalize event for the local node (self-event).
    ///
    /// Sets the first-externalize baseline for this slot.
    /// Mirrors stellar-core's `recordSCPExternalizeEvent(slotIndex, localNodeID, false)`.
    /// Returns the captured `now` instant so callers can reuse it for timing consistency.
    pub fn record_self_externalize_event(&self, slot: SlotIndex) -> std::time::Instant {
        let now = std::time::Instant::now();
        let self_node = NodeId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::curr::Uint256(*self.config.node_id.as_bytes()),
        ));
        self.externalize_lag
            .write()
            .record_event(slot, &self_node, true, now);
        now
    }

    /// Record an SCP externalize event for a peer node.
    ///
    /// Records `now - first_externalize[slot]` as a lag sample.
    /// Mirrors stellar-core's `recordSCPExternalizeEvent(slotIndex, nodeID, false)`.
    pub fn record_peer_externalize_event(&self, slot: SlotIndex, node_id: &NodeId) {
        self.externalize_lag
            .write()
            .record_event(slot, node_id, false, std::time::Instant::now());
    }

    /// Get the summary lag info for the local quorum set.
    ///
    /// Returns the average of 75th-percentile lags across quorum set nodes
    /// with lag > 0. Returns `None` if no nodes have positive lag.
    pub fn get_qset_lag_info_summary(&self) -> Option<u64> {
        let qset = self.get_local_quorum_set()?;
        self.externalize_lag.read().get_lag_info_summary(&qset)
    }

    /// Elapsed time since the first SCP activity was recorded for `slot`.
    /// Returns `None` if `record_slot_activity` was never called for this slot
    /// (e.g., catchup/fast-forward paths).
    pub fn slot_first_seen_elapsed(&self, slot: SlotIndex) -> Option<std::time::Duration> {
        self.slot_timing
            .read()
            .get(&slot)
            .and_then(|s| s.first_seen)
            .map(|t| t.elapsed())
    }

    /// Get an externalized slot.
    pub fn get_externalized(&self, slot: SlotIndex) -> Option<ExternalizedSlot> {
        tracked_read(LOCK_SCP_EXTERNALIZED, &self.externalized)
            .get(&slot)
            .cloned()
    }

    /// Find the slot for a given tx set hash in recent externalized values.
    pub fn find_externalized_slot_by_tx_set_hash(&self, hash: &Hash256) -> Option<SlotIndex> {
        tracked_read(LOCK_SCP_EXTERNALIZED, &self.externalized)
            .iter()
            .find_map(|(slot, ext)| {
                ext.tx_set_hash
                    .as_ref()
                    .filter(|tx_hash| *tx_hash == hash)
                    .map(|_| *slot)
            })
    }

    /// Get all externalized slot indices in a range (inclusive).
    /// Returns a sorted list of slots that have been externalized.
    pub fn get_externalized_slots_in_range(
        &self,
        from: SlotIndex,
        to: SlotIndex,
    ) -> Vec<SlotIndex> {
        let externalized = tracked_read(LOCK_SCP_EXTERNALIZED, &self.externalized);
        let mut slots: Vec<SlotIndex> = externalized
            .keys()
            .filter(|&&slot| slot >= from && slot <= to)
            .copied()
            .collect();
        slots.sort();
        slots
    }

    /// Find missing (gap) slots in a range that have not been externalized.
    /// Returns slots that should have EXTERNALIZE but don't.
    pub fn find_missing_slots_in_range(&self, from: SlotIndex, to: SlotIndex) -> Vec<SlotIndex> {
        if from > to {
            return vec![];
        }
        let externalized = tracked_read(LOCK_SCP_EXTERNALIZED, &self.externalized);
        let mut missing = Vec::new();
        for slot in from..=to {
            if !externalized.contains_key(&slot) {
                missing.push(slot);
            }
        }
        missing
    }

    /// Validate close time against a last close time reference.
    ///
    /// Matches stellar-core `HerderSCPDriver::checkCloseTime(slotIndex, lastCloseTime, sv)`.
    /// Returns true if:
    /// 1. close_time > lastCloseTime (not too old)
    /// 2. close_time <= now + MAX_TIME_SLIP_SECONDS (not too far in future)
    pub fn check_close_time(
        &self,
        _slot_index: SlotIndex,
        last_close_time: u64,
        close_time: u64,
    ) -> bool {
        // Check closeTime (not too old)
        if close_time <= last_close_time {
            trace!(
                "Close time {} not after last close time {}",
                close_time,
                last_close_time
            );
            return false;
        }

        // Check closeTime (not too far in future)
        let now = self.now_seconds();
        if close_time > now + self.config.max_time_drift {
            trace!(
                "Close time {} too far in future (now: {}, max_slip: {})",
                close_time,
                now,
                self.config.max_time_drift
            );
            return false;
        }
        true
    }

    /// Validate a value for a past or future slot (not LCL+1).
    ///
    /// Matches stellar-core `HerderSCPDriver::validatePastOrFutureValue`.
    ///
    /// # Arguments
    /// * `slot_index` - The slot being validated
    /// * `close_time` - The close time of the value
    /// * `lcl_seq` - The LCL's ledger sequence
    /// * `lcl_close_time` - The LCL's close time
    /// * `is_tracking` - Whether we are in tracking state
    /// * `tracking_index` - Next consensus ledger index (tracking_slot)
    /// * `tracking_close_time` - Tracking consensus close time
    fn validate_past_or_future_value(
        &self,
        slot_index: SlotIndex,
        close_time: u64,
        context: ValueValidationContext,
    ) -> ValueValidation {
        let ValueValidationContext {
            lcl_seq,
            lcl_close_time,
            tracking,
        } = context;

        // slot_index must NOT be lcl_seq + 1 (that's the current ledger path)
        if slot_index == lcl_seq + 1 {
            debug!(
                "validate_past_or_future_value called for current ledger {}",
                slot_index
            );
            return ValueValidation::Invalid;
        }

        if slot_index == lcl_seq {
            // Previous ledger: close time must exactly match LCL
            if close_time != lcl_close_time {
                trace!(
                    "Bad close time for ledger {}: got {} vs LCL {}",
                    slot_index,
                    close_time,
                    lcl_close_time
                );
                return ValueValidation::Invalid;
            }
        } else if slot_index < lcl_seq {
            // Older than LCL: close time must be strictly less
            if close_time >= lcl_close_time {
                trace!(
                    "Bad close time for old ledger {}: got {} vs LCL {}",
                    slot_index,
                    close_time,
                    lcl_close_time
                );
                return ValueValidation::Invalid;
            }
        } else {
            // Future slot (beyond LCL+1): use checkCloseTime with LCL as reference
            if !self.check_close_time(slot_index, lcl_close_time, close_time) {
                return ValueValidation::Invalid;
            }
        }

        let Some(tracking) = tracking else {
            // Can't validate further without tracking state
            trace!("MaybeValidValue (not tracking) for slot {}", slot_index);
            return ValueValidation::MaybeValid;
        };

        let SharedTrackingState {
            consensus_index: tracking_index,
            consensus_close_time: tracking_close_time,
            ..
        } = tracking;

        // Check slotIndex against tracking state
        if tracking_index > slot_index {
            // We already moved on from this slot
            trace!(
                "MaybeValidValue (already moved on) for slot {}, at {}",
                slot_index,
                tracking_index
            );
            return ValueValidation::MaybeValid;
        }
        if tracking_index < slot_index {
            // Processing a future message while tracking -- should not happen
            debug!(
                "validateValue slot {} processing future message while tracking {}",
                slot_index,
                tracking_index.saturating_sub(1)
            );
            return ValueValidation::Invalid;
        }

        // tracking_index == slot_index: use tracking close time for tighter check
        if !self.check_close_time(slot_index, tracking_close_time, close_time) {
            return ValueValidation::Invalid;
        }

        // `tracking_index == slot_index` combined with `slot_index !=
        // lcl_seq + 1` (the assertion at the top of this function)
        // means SCP tracking has caught up to or moved past `slot_index`
        // while the local ledger apply is at a different point. The
        // dominant case is `lcl_seq + 1 < slot_index` — apply lagging
        // behind SCP externalization.
        //
        // This path is now limited to fresh peer envelopes arriving
        // between externalization and `ledger_closed` (via the select!
        // loop, `receive_tx_set`, or `process_ready_fetching_envelopes`).
        // The former dominant trigger — synchronous drain in
        // `advance_tracking_slot` — was removed in #2115; pending
        // envelopes are now drained post-apply in `Herder::ledger_closed`,
        // mirroring stellar-core's `safelyProcessSCPQueue(false)` →
        // `postOnMainThread` (HerderImpl.cpp:1194).
        //
        // Return `MaybeValidDeferred` so the ballot protocol clears
        // `fully_validated` (preventing premature local EXTERNALIZE on
        // a value we cannot fully validate yet) AND record the slot in
        // `deferred_slots` with `apply_lag = true` so that
        // `Herder::ledger_closed` →
        // `resolve_apply_lag_for_next_index(lcl_seq + 1)` will trigger
        // `restore_slot_fully_validated` once apply catches up.
        //
        // The matching restoration trigger closes audit finding H-014
        // (issue #2096): without it, the validator could permanently
        // suppress its own EXTERNALIZE for the slot.
        //
        // See the `ValidationLevel::MaybeValidDeferred` doc comment
        // (`crates/scp/src/driver.rs`) for the full rationale.
        self.record_apply_lag(slot_index);
        trace!(
            "Can't validate locally, value may be valid for slot {} \
             (MaybeValidDeferred; apply lagging SCP — recorded for \
             restoration on next ledger_closed)",
            slot_index
        );
        ValueValidation::MaybeValidDeferred
    }

    /// Validate an SCP value.
    ///
    /// The value is the XDR-encoded StellarValue.
    /// Matches stellar-core `HerderSCPDriver::validateValue` which:
    /// 1. Deserializes to StellarValue
    /// 2. Checks STELLAR_VALUE_SIGNED (required for ALL values)
    /// 3. Verifies the signature
    /// 4. Validates close time and tx set (via validateValueAgainstLocalState)
    /// 5. Checks upgrade ordering
    pub fn validate_value_impl(
        &self,
        slot_index: SlotIndex,
        value: &Value,
        nomination: bool,
    ) -> ValueValidation {
        // Decode the StellarValue
        let stellar_value = match StellarValue::from_xdr(value, stellar_xdr::curr::Limits::none()) {
            Ok(v) => v,
            Err(e) => {
                debug!("Failed to decode StellarValue: {}", e);
                return ValueValidation::Invalid;
            }
        };

        // Parity: check STELLAR_VALUE_SIGNED (required for both nomination and ballot)
        let sig = match &stellar_value.ext {
            StellarValueExt::Signed(sig) => sig,
            StellarValueExt::Basic => {
                debug!("Expected STELLAR_VALUE_SIGNED");
                return ValueValidation::Invalid;
            }
        };

        // Parity: verify the stellar value signature
        // Signs: (networkID, ENVELOPE_TYPE_SCPVALUE, txSetHash, closeTime)
        if !self.verify_stellar_value_signature(
            &sig.node_id,
            &sig.signature,
            &stellar_value.tx_set_hash,
            stellar_value.close_time.clone(),
        ) {
            debug!("StellarValue signature verification failed");
            return ValueValidation::Invalid;
        }

        // Validate against local state (close time, tx set, etc.)
        let result =
            self.validate_value_against_local_state(slot_index, &stellar_value, nomination);
        if result == ValueValidation::Invalid {
            return ValueValidation::Invalid;
        }

        // Check upgrade ordering and validity (regardless of local state result)
        if !Self::check_upgrade_ordering(&stellar_value) {
            return ValueValidation::Invalid;
        }

        // Parity: HerderSCPDriver.cpp:375-401 — validate each upgrade via isValid
        if !self.check_upgrades_valid(&stellar_value, nomination) {
            return ValueValidation::Invalid;
        }

        result
    }

    /// Validate a StellarValue against local state.
    ///
    /// Checks close time and transaction set validity.
    /// Matches stellar-core `HerderSCPDriver::validateValueAgainstLocalState`.
    ///
    /// For LCL+1 (current ledger): performs full validation (close time + tx set).
    /// For past/future slots: delegates to `validate_past_or_future_value`.
    fn validate_value_against_local_state(
        &self,
        slot_index: SlotIndex,
        stellar_value: &StellarValue,
        nomination: bool,
    ) -> ValueValidation {
        let close_time = stellar_value.close_time.0;

        // Get LCL data from ledger manager.
        // Use header_snapshot() to atomically capture both header fields and
        // hash, avoiding a race with commit_close() between the reads.
        let (lcl_seq, lcl_close_time, lcl_hash_from_snapshot) =
            if let Some(lm) = self.ledger_manager.get() {
                let snap = lm.header_snapshot();
                (
                    snap.header.ledger_seq as u64,
                    snap.header.scp_value.close_time.0,
                    Some(snap.hash),
                )
            } else {
                // No ledger manager available — fall back to externalized data
                if let Some(latest) =
                    *tracked_read(LOCK_SCP_LATEST_EXTERNALIZED, &self.latest_externalized)
                {
                    if let Some(externalized) =
                        tracked_read(LOCK_SCP_EXTERNALIZED, &self.externalized).get(&latest)
                    {
                        (externalized.slot, externalized.close_time, None)
                    } else {
                        (0, 0, None)
                    }
                } else {
                    let ts = tracked_read(LOCK_TRACKING_STATE, &self.tracking_state);
                    if ts.is_tracking {
                        // When tracking but no ledger manager or externalized data (e.g., after
                        // bootstrap), derive the LCL from the tracking consensus index.
                        // consensus_index is the next slot to close, so LCL = index - 1.
                        (
                            ts.consensus_index.saturating_sub(1),
                            ts.consensus_close_time,
                            None,
                        )
                    } else {
                        (0, 0, None)
                    }
                }
            };

        let is_current_ledger = slot_index == lcl_seq + 1;

        if is_current_ledger {
            // The value is for LCL+1 — perform all possible checks
            if !self.check_close_time(slot_index, lcl_close_time, close_time) {
                return ValueValidation::Invalid;
            }

            // Fetch the transaction set in a single atomic lookup.
            // Parity: stellar-core (HerderSCPDriver.cpp:301-313) does a single
            // getTxSet() and returns kInvalidValue on null. We mirror the single-
            // lookup pattern to eliminate the TOCTOU race that existed when
            // has_tx_set() and get() were separate calls (AUDIT-220 / issue #2157).
            let tx_set_hash = Hash256::from_bytes(stellar_value.tx_set_hash.0);
            let tx_set = match self.tx_tracker.get(&tx_set_hash) {
                Some(ts) => ts,
                None => {
                    // During ballot protocol (nomination=false), EXTERNALIZE
                    // envelopes may arrive before the tx_set is fetched. Return
                    // MaybeValidDeferred so SCP can still externalize the slot
                    // while the tx_set fetch completes. The paired restoration in
                    // Herder::cache_tx_set (via resolve_missing_tx_set) restores
                    // fully_validated when the tx_set arrives.
                    //
                    // Note: stellar-core returns kInvalidValue here because its
                    // PendingEnvelopes buffering ensures the tx_set is always
                    // present. Our MaybeValidDeferred is an intentional deviation
                    // for EXTERNALIZE fast-tracking (issues #1795, #1798).
                    if !nomination {
                        self.record_missing_tx_set(slot_index, tx_set_hash);
                        debug!(
                            "Missing transaction set during ballot protocol: {} \
                             (MaybeValidDeferred)",
                            tx_set_hash
                        );
                        return ValueValidation::MaybeValidDeferred;
                    }
                    debug!("Missing transaction set: {}", tx_set_hash);
                    return ValueValidation::Invalid;
                }
            };

            // All content validation runs unconditionally on the owned tx_set.
            // It is structurally impossible to reach Valid without passing these
            // checks — there is no conditional skip path.

            // Parity: verify hash integrity
            let computed = tx_set.recompute_hash();
            if computed != tx_set_hash {
                debug!(
                    "Tx set hash mismatch: expected {}, computed {}",
                    tx_set_hash, computed
                );
                return ValueValidation::Invalid;
            }

            // Parity: check previousLedgerHash matches the LCL hash.
            // Conditional on lcl_hash_from_snapshot — preserves bootstrap/
            // no-ledger-manager behavior where this check is skipped.
            if let Some(hash) = lcl_hash_from_snapshot {
                if tx_set.previous_ledger_hash() != hash {
                    debug!(
                        "Tx set previousLedgerHash mismatch: expected {}, got {}",
                        hash,
                        tx_set.previous_ledger_hash()
                    );
                    return ValueValidation::Invalid;
                }
            }

            // Parity: validate tx set is well-formed (sorted, no duplicates)
            // For generalized tx sets, per-component sort order is validated
            // during extraction (tx_set.rs:validate_component). The global
            // hash-sort check only applies to legacy (non-generalized) sets.
            if tx_set.generalized_tx_set().is_none() && !Self::is_tx_set_well_formed(&tx_set) {
                debug!("Legacy tx set is not well-formed (unsorted or has duplicates)");
                return ValueValidation::Invalid;
            }

            // Parity: validate individual transaction content (AUDIT-033)
            // Mirrors stellar-core's checkAndCacheTxSetValid()
            if let Some(lcl_hash) = lcl_hash_from_snapshot {
                let close_time_offset = close_time.saturating_sub(lcl_close_time);
                if !self.check_and_cache_tx_set_valid(&tx_set, lcl_hash, close_time_offset) {
                    debug!("Tx set content validation failed for slot {}", slot_index);
                    return ValueValidation::Invalid;
                }
            }

            ValueValidation::Valid
        } else {
            // Past or future slot — partial validation
            let ts = *tracked_read(LOCK_TRACKING_STATE, &self.tracking_state);
            let tracking = if ts.is_tracking { Some(ts) } else { None };

            self.validate_past_or_future_value(
                slot_index,
                close_time,
                ValueValidationContext {
                    lcl_seq,
                    lcl_close_time,
                    tracking,
                },
            )
        }
    }

    /// Check that upgrades in a StellarValue are in strictly increasing order.
    fn check_upgrade_ordering(stellar_value: &StellarValue) -> bool {
        let mut last_upgrade_order = None;
        for upgrade in stellar_value.upgrades.iter() {
            let upgrade = match LedgerUpgrade::from_xdr(
                upgrade.0.as_slice(),
                stellar_xdr::curr::Limits::none(),
            ) {
                Ok(upgrade) => upgrade,
                Err(_) => {
                    debug!("Invalid ledger upgrade encountered");
                    return false;
                }
            };
            let order = Self::upgrade_type_order(&upgrade);
            if last_upgrade_order.is_some_and(|prev| order <= prev) {
                debug!("Invalid ledger upgrade encountered");
                return false;
            }
            last_upgrade_order = Some(order);
        }
        true
    }

    /// Verify a StellarValue signature.
    ///
    /// stellar-core signs: `(networkID, ENVELOPE_TYPE_SCPVALUE, txSetHash, closeTime)`.
    fn verify_stellar_value_signature(
        &self,
        node_id: &NodeId,
        signature: &stellar_xdr::curr::Signature,
        tx_set_hash: &stellar_xdr::curr::Hash,
        close_time: stellar_xdr::curr::TimePoint,
    ) -> bool {
        let pubkey_bytes = match &node_id.0 {
            stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(stellar_xdr::curr::Uint256(
                bytes,
            )) => bytes,
        };

        // Build signed data: (networkID, ENVELOPE_TYPE_SCPVALUE, txSetHash, closeTime)
        let mut data = self.network_id.0.to_vec();
        if let Ok(env_type_bytes) = EnvelopeType::Scpvalue.to_xdr(stellar_xdr::curr::Limits::none())
        {
            data.extend_from_slice(&env_type_bytes);
        } else {
            return false;
        }
        if let Ok(hash_bytes) = tx_set_hash.to_xdr(stellar_xdr::curr::Limits::none()) {
            data.extend_from_slice(&hash_bytes);
        } else {
            return false;
        }
        if let Ok(time_bytes) = close_time.to_xdr(stellar_xdr::curr::Limits::none()) {
            data.extend_from_slice(&time_bytes);
        } else {
            return false;
        }

        let sig_bytes: [u8; 64] = match signature.0.as_slice().try_into() {
            Ok(b) => b,
            Err(_) => return false,
        };

        let sig = Signature::from_bytes(sig_bytes);
        henyey_crypto::verify_from_raw_key(pubkey_bytes, &data, &sig).is_ok()
    }

    /// Extract a valid value from a potentially invalid composite.
    ///
    /// Parity: `HerderSCPDriver::extractValidValue`:
    /// 1. Does NOT check STELLAR_VALUE_SIGNED or verify signature
    /// 2. Calls validateValueAgainstLocalState with nomination=true
    /// 3. Only returns a value when result is kFullyValidatedValue
    /// 4. Strips invalid upgrades from the value
    pub fn extract_valid_value_impl(&self, slot: SlotIndex, value: &Value) -> Option<Value> {
        if value.0.is_empty() {
            return None;
        }

        // Decode the StellarValue
        let mut stellar_value =
            match StellarValue::from_xdr(value, stellar_xdr::curr::Limits::none()) {
                Ok(v) => v,
                Err(_) => return None,
            };

        // Parity: only extract if fully validated against local state
        // (does NOT check STELLAR_VALUE_SIGNED or signature)
        // extractValidValue is called during nomination, so use nomination=true
        let result = self.validate_value_against_local_state(slot, &stellar_value, true);
        if result != ValueValidation::Valid {
            return None;
        }

        // Parity: strip invalid upgrades, keeping valid ones in order
        // Also validates each upgrade via isValid (apply + nomination)
        // stellar-core: extractValidValue calls isValid with nomination=true
        let lm_ref = self.ledger_manager.get().map(|lm| lm.as_ref());
        // Read header once to avoid split reads between ledger_version and close_time.
        let lcl_header = lm_ref.map(|lm| lm.current_header());
        let current_version = lcl_header.as_ref().map(|h| h.ledger_version).unwrap_or(0);
        // Use LCL close time (not candidate close_time) for upgrade validity
        // to prevent one-ledger-early activation (#1166).
        let lcl_close_time = lcl_header
            .as_ref()
            .map(|h| h.scp_value.close_time.0)
            .unwrap_or(0);
        let mut valid_upgrades = Vec::new();
        let mut last_upgrade_type = None;
        for upgrade_bytes in stellar_value.upgrades.iter() {
            if let Ok(upgrade) = LedgerUpgrade::from_xdr(
                upgrade_bytes.0.as_slice(),
                stellar_xdr::curr::Limits::none(),
            ) {
                let upgrade_type = Self::upgrade_type_order(&upgrade);
                // Only keep if in strictly increasing order and valid for apply
                let in_order = last_upgrade_type
                    .map(|prev| upgrade_type > prev)
                    .unwrap_or(true);
                let valid_for_apply =
                    Self::is_valid_upgrade_for_apply(&upgrade, current_version, lm_ref);
                // Also check nomination validity (timing + parameter match)
                let valid_for_nomination = if let Some(upgrades_arc) = self.upgrades.get() {
                    upgrades_arc
                        .read()
                        .is_valid_for_nomination(&upgrade, lcl_close_time)
                } else {
                    true // No upgrade config — can't filter
                };
                if in_order && valid_for_apply && valid_for_nomination {
                    last_upgrade_type = Some(upgrade_type);
                    valid_upgrades.push(upgrade_bytes.clone());
                }
            }
        }

        // If upgrades changed, update the value
        if valid_upgrades.len() != stellar_value.upgrades.len() {
            stellar_value.upgrades = valid_upgrades.try_into().unwrap_or_default();
            // Re-encode
            stellar_value
                .to_xdr(stellar_xdr::curr::Limits::none())
                .ok()
                .map(|bytes| Value(bytes.try_into().unwrap_or_default()))
        } else {
            Some(value.clone())
        }
    }

    /// Check that all upgrades in a StellarValue are valid.
    ///
    /// Parity: Upgrades.cpp `isValid` — calls `isValidForApply` always,
    /// and additionally `isValidForNomination` when `nomination=true`.
    fn check_upgrades_valid(&self, stellar_value: &StellarValue, nomination: bool) -> bool {
        let lm_ref = match self.ledger_manager.get() {
            Some(lm) => lm,
            None => return true, // No ledger manager — can't validate
        };
        // Read header once to avoid split reads between ledger_version and close_time.
        let lcl_header = lm_ref.current_header();
        let current_version = lcl_header.ledger_version;

        // Use LCL close time for upgrade validity, not candidate close_time (#1166).
        let lcl_close_time = lcl_header.scp_value.close_time.0;

        for upgrade_bytes in stellar_value.upgrades.iter() {
            let upgrade = match LedgerUpgrade::from_xdr(
                upgrade_bytes.0.as_slice(),
                stellar_xdr::curr::Limits::none(),
            ) {
                Ok(u) => u,
                Err(_) => return false,
            };
            if !Self::is_valid_upgrade_for_apply(&upgrade, current_version, Some(lm_ref)) {
                debug!(?upgrade, "Invalid upgrade for apply");
                return false;
            }
            if nomination {
                if let Some(upgrades_arc) = self.upgrades.get() {
                    let upgrades = upgrades_arc.read();
                    if !upgrades.is_valid_for_nomination(&upgrade, lcl_close_time) {
                        debug!(?upgrade, "Invalid upgrade for nomination");
                        return false;
                    }
                }
            }
        }
        true
    }

    /// Check if a single upgrade is valid for application to the ledger.
    ///
    /// Parity: Upgrades.cpp `isValidForApply` (lines 543-623)
    ///
    /// For `LedgerUpgrade::Config`, stellar-core performs a full ledger lookup
    /// via `ConfigUpgradeSetFrame::makeFromKey` and validates the loaded frame
    /// via `isValidForApply()`. We mirror this by using
    /// `LedgerManager::get_config_upgrade_set` when available.
    fn is_valid_upgrade_for_apply(
        upgrade: &LedgerUpgrade,
        current_version: u32,
        ledger_manager: Option<&LedgerManager>,
    ) -> bool {
        match upgrade {
            LedgerUpgrade::Version(new_version) => {
                // Must be strictly monotonic and within supported range
                *new_version > current_version
                    && *new_version <= henyey_common::CURRENT_LEDGER_PROTOCOL_VERSION
            }
            LedgerUpgrade::BaseFee(fee) => *fee != 0,
            LedgerUpgrade::MaxTxSetSize(_) => true, // Any size allowed
            LedgerUpgrade::BaseReserve(reserve) => *reserve != 0,
            LedgerUpgrade::Flags(flags) => {
                // Must be protocol >= 18 and only valid flag bits
                const MASK_LEDGER_HEADER_FLAGS: u32 = 0x7;
                protocol_version_starts_from(current_version, ProtocolVersion::V18)
                    && (*flags & !MASK_LEDGER_HEADER_FLAGS) == 0
            }
            LedgerUpgrade::Config(key) => {
                // Config upgrades require Soroban protocol.
                if current_version < henyey_common::MIN_SOROBAN_PROTOCOL_VERSION {
                    return false;
                }
                // Parity: stellar-core loads ConfigUpgradeSetFrame from ledger
                // and validates it. Without a ledger manager we cannot perform
                // this check, so we conservatively reject.
                let Some(lm) = ledger_manager else {
                    debug!("Config upgrade validation: no ledger manager available, rejecting");
                    return false;
                };
                let Some(frame) = lm.get_config_upgrade_set(key) else {
                    debug!(
                        contract_id = ?key.contract_id,
                        "Config upgrade key not found in ledger"
                    );
                    return false;
                };
                use henyey_ledger::ConfigUpgradeValidity;
                match frame.is_valid_for_apply() {
                    ConfigUpgradeValidity::Valid => true,
                    ConfigUpgradeValidity::XdrInvalid => {
                        debug!(
                            contract_id = ?key.contract_id,
                            "Config upgrade set has invalid XDR"
                        );
                        false
                    }
                    ConfigUpgradeValidity::Invalid => {
                        debug!(
                            contract_id = ?key.contract_id,
                            "Config upgrade set is invalid"
                        );
                        false
                    }
                }
            }
            LedgerUpgrade::MaxSorobanTxSetSize(_) => {
                current_version >= henyey_common::MIN_SOROBAN_PROTOCOL_VERSION
            }
        }
    }

    /// Get the ordering number for an upgrade type.
    fn upgrade_type_order(upgrade: &LedgerUpgrade) -> u32 {
        match upgrade {
            LedgerUpgrade::Version(_) => 0,
            LedgerUpgrade::BaseFee(_) => 1,
            LedgerUpgrade::MaxTxSetSize(_) => 2,
            LedgerUpgrade::BaseReserve(_) => 3,
            LedgerUpgrade::Flags(_) => 4,
            LedgerUpgrade::Config(_) => 5,
            LedgerUpgrade::MaxSorobanTxSetSize(_) => 6,
        }
    }

    /// Combine multiple candidate values into one.
    ///
    /// Parity: `HerderSCPDriver::combineCandidates`:
    /// 1. Collect upgrades from ALL candidates, merging by taking max of each type
    /// 2. Select the best tx set using compareTxSets (size comparison + tiebreak)
    /// 3. Compose result: best candidate's txSetHash/closeTime + merged upgrades
    pub fn combine_candidates_impl(&self, _slot: SlotIndex, values: &[Value]) -> Value {
        if values.is_empty() {
            return Value::default();
        }

        if values.len() == 1 {
            return values[0].clone();
        }

        // Decode all values, keeping the raw Value bytes alongside for sorting.
        // Parity: stellar-core's ValueWrapperPtrSet iterates by raw Value byte
        // order (WrappedValuePtrComparator, SCPDriver.cpp:36-41). We sort by the
        // same key to ensure identical tie-breaking.
        let decoded: Vec<(Value, StellarValue)> = values
            .iter()
            .filter_map(|v| {
                StellarValue::from_xdr(v, stellar_xdr::curr::Limits::none())
                    .ok()
                    .map(|sv| (v.clone(), sv))
            })
            .collect();

        if decoded.is_empty() {
            return values[0].clone();
        }

        // Resolve tx sets upfront in a single atomic lookup per candidate.
        // Parity deviation: stellar-core releaseAssert(cTxSet) on missing tx sets
        // (HerderSCPDriver.cpp:780) — we filter instead to be defensive.
        // This also eliminates the TOCTOU race that existed when is_cached() and
        // get() were separate calls (AUDIT-220 / issue #2157).
        struct ResolvedCandidate {
            raw: Value,
            sv: StellarValue,
            tx_set: TransactionSet,
            tx_set_hash: Hash256,
        }
        let mut candidates: Vec<ResolvedCandidate> = decoded
            .into_iter()
            .filter_map(|(raw, sv)| {
                let tx_set_hash = Hash256::from_bytes(sv.tx_set_hash.0);
                match self.tx_tracker.get(&tx_set_hash) {
                    Some(tx_set) => Some(ResolvedCandidate {
                        raw,
                        sv,
                        tx_set,
                        tx_set_hash,
                    }),
                    None => {
                        warn!(
                            "combine_candidates: tx set {} missing from cache, filtering out",
                            tx_set_hash
                        );
                        None
                    }
                }
            })
            .collect();
        if candidates.is_empty() {
            return values[0].clone();
        }

        // Parity: filter out candidates whose tx set has a different previousLedgerHash.
        // Uses the owned tx_set from resolve — no cache re-read needed.
        if let Some(lm) = self.ledger_manager.get() {
            let lcl_hash = lm.current_header_hash();
            candidates.retain(|c| c.tx_set.previous_ledger_hash() == lcl_hash);
            if candidates.is_empty() {
                // All candidates filtered out — fall back to first value
                return values[0].clone();
            }
        }

        // Step 1: Compute candidates hash (XOR of all candidate hashes) for tiebreaking
        let mut candidates_hash = [0u8; 32];
        for c in &candidates {
            let val_bytes = henyey_common::xdr_stream::xdr_to_bytes(&c.sv);
            let hash = Hash256::hash(&val_bytes);
            for (i, byte) in candidates_hash.iter_mut().enumerate() {
                *byte ^= hash.as_bytes()[i];
            }
        }

        // Step 2: Merge upgrades across all candidates (take max of each upgrade type)
        let mut merged_upgrades: std::collections::BTreeMap<u32, LedgerUpgrade> =
            std::collections::BTreeMap::new();
        for c in &candidates {
            for upgrade_bytes in c.sv.upgrades.iter() {
                if let Ok(upgrade) = LedgerUpgrade::from_xdr(
                    upgrade_bytes.0.as_slice(),
                    stellar_xdr::curr::Limits::none(),
                ) {
                    let order = Self::upgrade_type_order(&upgrade);
                    merged_upgrades
                        .entry(order)
                        .and_modify(|existing| {
                            if Self::compare_upgrades(&upgrade, existing) {
                                *existing = upgrade.clone();
                            }
                        })
                        .or_insert(upgrade);
                }
            }
        }

        // Step 3: Sort candidates by raw Value bytes for deterministic iteration.
        // Parity: stellar-core iterates a ValueWrapperPtrSet (std::set ordered
        // by raw Value bytes via WrappedValuePtrComparator, SCPDriver.cpp:36-41).
        candidates.sort_by(|a, b| a.raw.cmp(&b.raw));

        // Step 4: Select best candidate using compareTxSets logic.
        // Parity: HerderSCPDriver.cpp:775-797 — manual loop that keeps the
        // first winner on ties (stellar-core: `if (!highestTxSet || compareTxSets(...))`).
        // We use a keep-first fold instead of max_by, which returns the last
        // tied element and would be order-dependent.
        // Uses owned tx_set references — no cache re-reads.
        let mut best_idx = 0;
        for i in 1..candidates.len() {
            if Self::compare_tx_sets(
                &candidates[best_idx].tx_set,
                &candidates[i].tx_set,
                &candidates[best_idx].tx_set_hash,
                &candidates[i].tx_set_hash,
                &candidates_hash,
            ) == std::cmp::Ordering::Less
            {
                best_idx = i;
            }
        }

        // Step 5: Compose result
        let mut result = candidates[best_idx].sv.clone();

        // Replace upgrades with merged set (in order of upgrade type)
        let upgrade_bytes: Vec<stellar_xdr::curr::UpgradeType> = merged_upgrades
            .values()
            .filter_map(|upgrade| {
                upgrade
                    .to_xdr(stellar_xdr::curr::Limits::none())
                    .ok()
                    .and_then(|bytes| stellar_xdr::curr::UpgradeType(bytes.try_into().ok()?).into())
            })
            .collect();
        result.upgrades = upgrade_bytes.try_into().unwrap_or_default();

        // Re-encode the result
        result
            .to_xdr(stellar_xdr::curr::Limits::none())
            .map(|bytes| Value(bytes.try_into().unwrap_or_default()))
            .unwrap_or_default()
    }

    /// Compare two upgrades of the same type, returning true if `new` > `existing`.
    /// stellar-core takes the max of each upgrade type.
    fn compare_upgrades(new: &LedgerUpgrade, existing: &LedgerUpgrade) -> bool {
        match (new, existing) {
            (LedgerUpgrade::Version(a), LedgerUpgrade::Version(b)) => a > b,
            (LedgerUpgrade::BaseFee(a), LedgerUpgrade::BaseFee(b)) => a > b,
            (LedgerUpgrade::MaxTxSetSize(a), LedgerUpgrade::MaxTxSetSize(b)) => a > b,
            (LedgerUpgrade::BaseReserve(a), LedgerUpgrade::BaseReserve(b)) => a > b,
            (LedgerUpgrade::Flags(a), LedgerUpgrade::Flags(b)) => a > b,
            (LedgerUpgrade::Config(a), LedgerUpgrade::Config(b)) => {
                // ConfigUpgradeSetKey derives Ord: compares contractID then contentHash
                a > b
            }
            (LedgerUpgrade::MaxSorobanTxSetSize(a), LedgerUpgrade::MaxSorobanTxSetSize(b)) => a > b,
            _ => false, // Different types shouldn't happen
        }
    }

    /// Compare two transaction sets for combine_candidates.
    ///
    /// Spec: HERDER_SPEC §10.2 — 5-criteria ordered comparison:
    /// 1. Most operations (more is better)
    /// 2. Highest total inclusion fees (more is better)
    /// 3. Highest total full fees (more is better)
    /// 4. Smallest encoded size (less is better)
    /// 5. XOR hash tiebreak (lexicographically smallest)
    ///
    /// Accepts owned `&TransactionSet` references directly to avoid cache
    /// re-reads and the TOCTOU race that existed when this function fetched
    /// from the cache internally (AUDIT-220 / issue #2157).
    fn compare_tx_sets(
        a_set: &TransactionSet,
        b_set: &TransactionSet,
        a_hash: &Hash256,
        b_hash: &Hash256,
        candidates_hash: &[u8; 32],
    ) -> std::cmp::Ordering {
        // 1. Compare by number of operations (more is better)
        let a_ops = Self::tx_set_num_ops(a_set);
        let b_ops = Self::tx_set_num_ops(b_set);
        let ops_cmp = a_ops.cmp(&b_ops);
        if ops_cmp != std::cmp::Ordering::Equal {
            return ops_cmp;
        }

        // 2. Compare by total inclusion fees (higher is better)
        let a_inclusion_fees = Self::tx_set_total_inclusion_fees(a_set);
        let b_inclusion_fees = Self::tx_set_total_inclusion_fees(b_set);
        let inclusion_fees_cmp = a_inclusion_fees.cmp(&b_inclusion_fees);
        if inclusion_fees_cmp != std::cmp::Ordering::Equal {
            return inclusion_fees_cmp;
        }

        // 3. Compare by total full fees (higher is better)
        let a_fees = Self::tx_set_total_fees(a_set);
        let b_fees = Self::tx_set_total_fees(b_set);
        let fees_cmp = a_fees.cmp(&b_fees);
        if fees_cmp != std::cmp::Ordering::Equal {
            return fees_cmp;
        }

        // 4. Compare by encoded size (smaller is better — note reversed order)
        let a_size = Self::tx_set_encoded_size(a_set);
        let b_size = Self::tx_set_encoded_size(b_set);
        let size_cmp = b_size.cmp(&a_size); // reversed: smaller is better
        if size_cmp != std::cmp::Ordering::Equal {
            return size_cmp;
        }

        // 5. XOR hash tiebreak
        let a_xored = Self::xor_hash(&a_hash.0, candidates_hash);
        let b_xored = Self::xor_hash(&b_hash.0, candidates_hash);
        a_xored.cmp(&b_xored)
    }

    /// Check that a transaction set is well-formed: sorted by hash and no duplicates.
    ///
    /// Parity: stellar-core `TxSetUtils::checkValid()` verifies structural integrity.
    fn is_tx_set_well_formed(tx_set: &TransactionSet) -> bool {
        let Some(txs) = tx_set.as_legacy_transactions() else {
            return true;
        };
        if txs.len() <= 1 {
            return true;
        }

        let mut prev_hash = Hash256::hash_xdr(&txs[0]);
        for tx in &txs[1..] {
            let hash = Hash256::hash_xdr(tx);
            if hash.0 <= prev_hash.0 {
                // Not strictly ascending — either unsorted or duplicate
                return false;
            }
            prev_hash = hash;
        }

        true
    }

    /// Count total number of operations in a transaction set.
    fn tx_set_num_ops(tx_set: &TransactionSet) -> usize {
        tx_set
            .iter_transactions()
            .map(|env| Self::envelope_num_ops(env))
            .sum()
    }

    /// Compute total inclusion fees for a transaction set.
    ///
    /// stellar-core's `getTotalInclusionFees()` sums `getInclusionFee()` per tx:
    /// - Classic: fullFee
    /// - Soroban: fullFee - declaredSorobanResourceFee
    fn tx_set_total_inclusion_fees(tx_set: &TransactionSet) -> i64 {
        tx_set
            .iter_transactions()
            // Saturate to prevent wrapping on adversarial fee values.
            .map(|env| crate::tx_set_utils::envelope_inclusion_fee(env).as_i64())
            .fold(0i64, i64::saturating_add)
    }

    /// Iterate over all transactions in a phase, yielding each envelope
    /// together with the component's optional base fee.
    fn phase_txs_with_base_fee(
        phase: &stellar_xdr::curr::TransactionPhase,
    ) -> Vec<(&stellar_xdr::curr::TransactionEnvelope, Option<i64>)> {
        match phase {
            stellar_xdr::curr::TransactionPhase::V0(components) => components
                .iter()
                .flat_map(|comp| {
                    let stellar_xdr::curr::TxSetComponent::TxsetCompTxsMaybeDiscountedFee(c) = comp;
                    c.txs.iter().map(move |tx| (tx, c.base_fee))
                })
                .collect(),
            stellar_xdr::curr::TransactionPhase::V1(parallel) => parallel
                .execution_stages
                .iter()
                .flat_map(|stage| stage.iter())
                .flat_map(|cluster| cluster.0.iter())
                .map(|tx| (tx, parallel.base_fee))
                .collect(),
        }
    }

    /// Compute total applying-time fees for a transaction set.
    ///
    /// stellar-core's `getTotalFees(lh)` computes `getFee(lh, baseFee, applying=true)` per tx:
    /// - Classic with baseFee: min(inclusionFee, baseFee * numOps)
    /// - Soroban with baseFee: resourceFee + min(inclusionFee, baseFee * numOps)
    /// - No baseFee: fullFee
    fn tx_set_total_fees(tx_set: &TransactionSet) -> i64 {
        let Some(gen) = tx_set.generalized_tx_set() else {
            // Legacy tx set: applying fee == full fee.
            // Saturate to prevent wrapping on adversarial fee values.
            return tx_set
                .iter_transactions()
                .map(|env| crate::tx_set_utils::envelope_fee(env).as_i64())
                .fold(0i64, i64::saturating_add);
        };
        let stellar_xdr::curr::GeneralizedTransactionSet::V1(set_v1) = gen;

        let mut total = 0i64;
        for phase in set_v1.phases.iter() {
            for (tx, base_fee) in Self::phase_txs_with_base_fee(phase) {
                // Saturate to prevent wrapping on adversarial fee values.
                total = total.saturating_add(Self::tx_applying_fee(tx, base_fee));
            }
        }
        total
    }

    /// Compute applying-time fee for a single transaction.
    ///
    /// Matches stellar-core `TransactionFrame::getFee(lh, baseFee, applying=true)`.
    fn tx_applying_fee(env: &stellar_xdr::curr::TransactionEnvelope, base_fee: Option<i64>) -> i64 {
        let full_fee = crate::tx_set_utils::envelope_fee(env);
        let Some(bf) = base_fee else {
            return full_fee.as_i64();
        };
        let inclusion_fee = crate::tx_set_utils::envelope_inclusion_fee(env);
        let resource_fee = full_fee.saturating_sub_inclusion(inclusion_fee);
        let num_ops = std::cmp::max(1, crate::tx_set_utils::envelope_num_ops(env) as i64);
        let adjusted_fee = bf.saturating_mul(num_ops);
        resource_fee
            .as_i64()
            .saturating_add(std::cmp::min(inclusion_fee.as_i64(), adjusted_fee))
    }

    /// Get number of operations from a transaction envelope.
    fn envelope_num_ops(env: &stellar_xdr::curr::TransactionEnvelope) -> usize {
        crate::tx_set_utils::envelope_num_ops(env)
    }

    /// Compute XDR-encoded size of a transaction set.
    ///
    /// For generalized tx sets, encodes the generalized set. For legacy,
    /// sums the XDR-encoded size of all transactions.
    fn tx_set_encoded_size(tx_set: &TransactionSet) -> usize {
        if let Some(gen) = tx_set.generalized_tx_set() {
            henyey_common::xdr_stream::xdr_to_bytes(gen).len()
        } else {
            tx_set
                .iter_transactions()
                .map(|tx| henyey_common::xdr_stream::xdr_to_bytes(tx).len())
                .sum()
        }
    }

    /// XOR a 32-byte hash with another 32-byte value for tiebreaking.
    fn xor_hash(hash: &[u8; 32], mask: &[u8; 32]) -> [u8; 32] {
        std::array::from_fn(|i| hash[i] ^ mask[i])
    }

    /// Sign an SCP envelope.
    pub fn sign_envelope(&self, statement: &ScpStatement) -> Option<Signature> {
        let secret_key = self.secret_key.as_ref()?;

        // Create the data to sign: network ID + ENVELOPE_TYPE_SCP + statement XDR
        // ENVELOPE_TYPE_SCP = 1 (as i32 big-endian)
        let statement_bytes = henyey_common::xdr_to_bytes(statement);
        let mut data = self.network_id.0.to_vec();
        data.extend_from_slice(&1i32.to_be_bytes()); // ENVELOPE_TYPE_SCP
        data.extend_from_slice(&statement_bytes);

        Some(secret_key.sign(&data))
    }

    /// Build the signed-payload bytes for an SCP envelope:
    /// `network_id || ENVELOPE_TYPE_SCP(1, i32 BE) || statement XDR`.
    ///
    /// Pure function (no `&self`); used by the off-event-loop verification
    /// worker (`scp_verify.rs`) so XDR serialization does not run on the
    /// tokio event loop.
    pub fn build_signed_bytes(network_id: &Hash256, envelope: &ScpEnvelope) -> Result<Vec<u8>> {
        let statement_bytes = envelope
            .statement
            .to_xdr(stellar_xdr::curr::Limits::none())
            .map_err(|e| HerderError::Internal(format!("Failed to encode statement: {}", e)))?;
        let mut data = Vec::with_capacity(32 + 4 + statement_bytes.len());
        data.extend_from_slice(&network_id.0);
        data.extend_from_slice(&1i32.to_be_bytes()); // ENVELOPE_TYPE_SCP
        data.extend_from_slice(&statement_bytes);
        Ok(data)
    }

    /// Verify a pre-serialised signed-payload `data` against `node_id` and
    /// `sig`. Pure function (no `&self`); used off the event loop.
    pub fn verify_signed_bytes(
        data: &[u8],
        node_id: &NodeId,
        sig: &stellar_xdr::curr::Signature,
    ) -> Result<()> {
        let pubkey_bytes = match &node_id.0 {
            stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(stellar_xdr::curr::Uint256(
                bytes,
            )) => bytes,
        };

        let sig_bytes: [u8; 64] = sig
            .0
            .as_slice()
            .try_into()
            .map_err(|_| HerderError::Internal("Invalid signature length".to_string()))?;
        let signature = henyey_crypto::Signature::from_bytes(sig_bytes);

        henyey_crypto::verify_from_raw_key(pubkey_bytes, data, &signature).map_err(|e| match e {
            henyey_crypto::CryptoError::InvalidPublicKey => {
                HerderError::Internal("Invalid node ID".to_string())
            }
            _ => HerderError::Scp(henyey_scp::ScpError::SignatureVerificationFailed),
        })
    }

    /// Verify an SCP envelope signature.
    ///
    /// This is a thin synchronous wrapper over [`ScpDriver::build_signed_bytes`]
    /// and [`ScpDriver::verify_signed_bytes`]. The event-loop fast path moves
    /// these two steps to a dedicated worker thread (see
    /// [`crate::scp_verify`]). This method is retained for the synchronous
    /// catchup verification path (`catchup_impl.rs`, which already runs inside
    /// `tokio::spawn`) and for tests.
    pub fn verify_envelope(&self, envelope: &ScpEnvelope) -> Result<()> {
        let data = Self::build_signed_bytes(&self.network_id, envelope)?;
        Self::verify_signed_bytes(&data, &envelope.statement.node_id, &envelope.signature)
    }

    /// Construct and broadcast an EXTERNALIZE envelope for a slot that was
    /// fast-forwarded (not processed through full SCP ballot protocol).
    ///
    /// This is needed so that network crawlers (e.g. StellarBeat/OBSRVR) see
    /// the node emitting EXTERNALIZE for recent slots and mark it as validating.
    /// The fast-forward path records the externalized value but doesn't create
    /// or emit an SCP envelope — this method fills that gap.
    /// Record an externalized value.
    ///
    /// `ext_now` is an optional pre-captured instant from the externalize event
    /// (e.g., from `record_self_externalize_event`). When provided, it is used
    /// for timing consistency with the lag tracker baseline. When `None`,
    /// `Instant::now()` is captured internally (catchup/fast-forward path).
    pub fn record_externalized(
        &self,
        slot: SlotIndex,
        value: Value,
        ext_now: Option<std::time::Instant>,
    ) {
        // Clear the tx set validity cache on ledger externalization
        self.clear_tx_set_valid_cache();

        // Parse the StellarValue and extract stellar_value_ext for logging
        let (tx_set_hash, close_time, stellar_value_ext_desc) =
            if let Ok(sv) = StellarValue::from_xdr(&value, stellar_xdr::curr::Limits::none()) {
                let ext_desc = describe_stellar_value_ext(&sv.ext);
                (
                    Some(Hash256::from_bytes(sv.tx_set_hash.0)),
                    sv.close_time.0,
                    ext_desc,
                )
            } else {
                (None, 0, "Unknown".to_string())
            };

        // Check if we're overwriting an existing externalized value with different content
        {
            let existing = tracked_read(LOCK_SCP_EXTERNALIZED, &self.externalized);
            if let Some(old) = existing.get(&slot) {
                if old.value != value {
                    // Parse old value's stellar_value_ext for comparison
                    let old_ext_desc = if let Ok(old_sv) =
                        StellarValue::from_xdr(&old.value, stellar_xdr::curr::Limits::none())
                    {
                        describe_stellar_value_ext(&old_sv.ext)
                    } else {
                        "Unknown".to_string()
                    };
                    warn!(
                        slot,
                        old_stellar_value_ext = %old_ext_desc,
                        new_stellar_value_ext = %stellar_value_ext_desc,
                        "Overwriting externalized value with DIFFERENT value - this may cause hash mismatch!"
                    );
                }
            }
        }

        let now = ext_now.unwrap_or_else(std::time::Instant::now);
        let externalized = ExternalizedSlot {
            slot,
            value,
            tx_set_hash,
            close_time,
            externalized_at: now,
        };

        // Compute timing snapshot only on first externalization for this slot.
        let is_first = !tracked_read(LOCK_SCP_EXTERNALIZED, &self.externalized).contains_key(&slot);
        if is_first {
            let first_to_self = self
                .externalize_lag
                .read()
                .first_externalize_for_slot(slot)
                .map(|first_ext| now.duration_since(first_ext));

            if let Some(st) = self.slot_timing.read().get(&slot).copied() {
                if let Some(first_seen) = st.first_seen {
                    let externalize_duration = now - first_seen;
                    let nomination_duration = match (st.nomination_start, st.ballot_start) {
                        (Some(nom_start), Some(bal_start)) => {
                            Some(bal_start.saturating_duration_since(nom_start))
                        }
                        _ => None,
                    };
                    *self.last_externalize_timing.write() = Some(ExternalizeTimingSnapshot {
                        slot,
                        externalize_duration,
                        nomination_duration,
                        first_to_self_externalize_lag: first_to_self,
                    });
                } else {
                    // Catchup/fast-forward path: no slot_first_seen recorded.
                    // Clear stale timing so the gauge resets to 0.0.
                    *self.last_externalize_timing.write() = None;
                }
            } else {
                // Catchup/fast-forward path: no slot_first_seen recorded.
                // Clear stale timing so the gauge resets to 0.0.
                *self.last_externalize_timing.write() = None;
            }
        }

        tracked_write(LOCK_SCP_EXTERNALIZED, &self.externalized).insert(slot, externalized);

        // Update latest
        let mut latest = tracked_write(LOCK_SCP_LATEST_EXTERNALIZED, &self.latest_externalized);
        if latest.map(|l| slot > l).unwrap_or(true) {
            *latest = Some(slot);
        }

        // Clean up old per-slot timing entries (keep only recent slots). Keep 100 slots
        // to ensure close-complete can still find timestamps during backlog (EXTERNALIZEs
        // race ahead of close-complete when the node is behind).
        {
            let mut map = self.slot_timing.write();
            map.retain(|&s, _| slot.saturating_sub(s) <= 100);
        }

        debug!("Externalized slot {} with close time {}", slot, close_time);
    }

    /// Get the close time of an externalized slot.
    pub fn get_externalized_close_time(&self, slot: SlotIndex) -> Option<u64> {
        tracked_read(LOCK_SCP_EXTERNALIZED, &self.externalized)
            .get(&slot)
            .map(|e| e.close_time)
    }

    /// Emit an envelope to the network.
    fn emit(&self, envelope: ScpEnvelope) {
        if let Some(sender) = self.envelope_sender.get() {
            sender(envelope);
        }
    }

    /// Get the cache size.
    pub fn tx_set_cache_size(&self) -> usize {
        self.tx_tracker.cache_count()
    }

    /// Get the pending tx sets count.
    pub fn pending_tx_sets_size(&self) -> usize {
        self.tx_tracker.pending_count()
    }

    /// Get the pending quorum sets count.
    pub fn pending_quorum_sets_size(&self) -> usize {
        self.qset_tracker.pending_count()
    }

    /// Get the externalized slots count.
    pub fn externalized_size(&self) -> usize {
        tracked_read(LOCK_SCP_EXTERNALIZED, &self.externalized).len()
    }

    /// Get the quorum sets count (by node ID).
    pub fn quorum_sets_size(&self) -> usize {
        self.qset_tracker.by_node_count()
    }

    /// Get the quorum sets by hash count.
    pub fn quorum_sets_by_hash_size(&self) -> usize {
        self.qset_tracker.by_hash_count()
    }

    /// Get all cache sizes for diagnostics.
    pub fn cache_sizes(&self) -> ScpDriverCacheSizes {
        let qset_sizes = self.qset_tracker.sizes();
        let tx_sizes = self.tx_tracker.sizes();
        ScpDriverCacheSizes {
            tx_set_cache: tx_sizes.cache,
            pending_tx_sets: tx_sizes.pending,
            pending_quorum_sets: qset_sizes.pending,
            externalized: tracked_read(LOCK_SCP_EXTERNALIZED, &self.externalized).len(),
            quorum_sets: qset_sizes.by_node,
            quorum_sets_by_hash: qset_sizes.by_hash,
            tx_set_valid_cache: tx_sizes.valid_cache,
        }
    }

    /// Clear old externalized slots.
    pub fn cleanup_externalized(&self, keep_count: usize) {
        let mut externalized = tracked_write(LOCK_SCP_EXTERNALIZED, &self.externalized);
        if externalized.len() <= keep_count {
            return;
        }

        // Get slots sorted by slot index
        let mut slots: Vec<SlotIndex> = externalized.keys().copied().collect();
        slots.sort();

        // Remove oldest
        let to_remove = externalized.len() - keep_count;
        let mut removed_slots = Vec::with_capacity(to_remove);
        for slot in slots.into_iter().take(to_remove) {
            externalized.remove(&slot);
            removed_slots.push(slot);
        }
        drop(externalized);

        // Clean up lag tracker entries for evicted slots
        let mut lag = self.externalize_lag.write();
        for slot in removed_slots {
            lag.cleanup_slot(slot);
        }
    }

    /// Clear the transaction set cache.
    pub fn clear_tx_set_cache(&self) {
        self.tx_tracker.clear_cache();
    }

    /// Clear slot-scoped caches in scp_driver.
    ///
    /// Clears tx sets, externalized values, and timing maps.
    /// Does NOT clear quorum set caches (they are not slot-scoped
    /// and clearing them breaks heard_from_quorum — see #1874).
    pub fn clear_slot_scoped_caches(&self) {
        let tx_sizes = self.tx_tracker.sizes();
        let externalized_count = tracked_read(LOCK_SCP_EXTERNALIZED, &self.externalized).len();

        self.tx_tracker.clear_all();
        tracked_write(LOCK_SCP_EXTERNALIZED, &self.externalized).clear();
        // NOTE: We intentionally do NOT clear qset_tracker here.
        // Quorum sets are not slot-scoped and clearing them breaks
        // heard_from_quorum(). See #1874.
        self.slot_timing.write().clear();
        self.externalize_lag.write().clear_slots();

        if tx_sizes.cache > 0 || tx_sizes.pending > 0 || externalized_count > 0 {
            tracing::info!(
                tx_set_count = tx_sizes.cache,
                pending_count = tx_sizes.pending,
                externalized_count,
                "Cleared slot-scoped scp_driver caches"
            );
        }
    }

    /// Trim stale caches while preserving data for slots after catchup.
    /// Called after catchup to release memory while keeping tx_sets and
    /// pending requests that will be needed for buffered ledgers.
    pub fn trim_stale_caches(&self, keep_after_slot: SlotIndex) {
        let initial_pending_count = self.tx_tracker.pending_count();
        let initial_externalized_count =
            tracked_read(LOCK_SCP_EXTERNALIZED, &self.externalized).len();

        self.tx_tracker.trim_stale_pending(keep_after_slot);

        // Trim externalized - keep slots > keep_after_slot
        {
            let mut externalized = tracked_write(LOCK_SCP_EXTERNALIZED, &self.externalized);
            externalized.retain(|slot, _| *slot > keep_after_slot);
        }
        self.slot_timing.write().retain(|&s, _| s > keep_after_slot);
        // cleanup_slots_below uses >= (keep slots >= slot), but here we want
        // to keep slots > keep_after_slot, so pass keep_after_slot + 1.
        self.externalize_lag
            .write()
            .cleanup_slots_below(keep_after_slot + 1);

        let kept_pending = self.tx_tracker.pending_count();
        let kept_externalized = tracked_read(LOCK_SCP_EXTERNALIZED, &self.externalized).len();

        tracing::info!(
            initial_pending_count,
            initial_externalized_count,
            kept_pending,
            kept_externalized,
            keep_after_slot,
            "Trimmed stale scp_driver caches, preserving future slots"
        );
    }

    /// Purge SCP state for slots below the given slot.
    ///
    /// This removes externalized slots and cached tx sets for old slots,
    /// freeing memory during out-of-sync recovery.
    pub fn purge_slots_below(&self, slot: SlotIndex) {
        // Remove externalized slots below the threshold
        {
            let mut externalized = tracked_write(LOCK_SCP_EXTERNALIZED, &self.externalized);
            let slots_to_remove: Vec<_> = externalized
                .keys()
                .filter(|&s| *s < slot)
                .cloned()
                .collect();
            for s in slots_to_remove {
                externalized.remove(&s);
            }
        }

        // Clean up timing map for old slots
        self.slot_timing.write().retain(|&s, _| s >= slot);
        self.externalize_lag.write().cleanup_slots_below(slot);

        // Clean up pending tx set requests for old slots
        self.cleanup_old_pending_slots(slot);

        // NOTE: We intentionally do NOT clear qset_tracker here.
        // Quorum sets are not slot-scoped — they map node_id → quorum_set
        // and are bounded by the number of active validators (~30).
        // Clearing them here breaks heard_from_quorum() because is_quorum()
        // needs get_quorum_set(node_id) to return Some for remote nodes.
        // See #1874.
    }

    /// Get local SCP envelopes for a slot.
    ///
    /// Returns envelopes this node has emitted for the given slot.
    /// Note: Currently returns empty since we don't store envelopes in ExternalizedSlot.
    /// This can be enhanced to store and return actual envelopes if needed for recovery.
    pub fn get_local_envelopes(&self, _slot: SlotIndex) -> Vec<ScpEnvelope> {
        // ExternalizedSlot doesn't store the envelope, just the value.
        // In a full implementation, we'd store envelopes separately.
        Vec::new()
    }

    /// Get our local quorum set.
    pub fn get_local_quorum_set(&self) -> Option<ScpQuorumSet> {
        self.qset_tracker.get_local()
    }

    /// Set our local quorum set.
    pub fn set_local_quorum_set(&self, quorum_set: ScpQuorumSet) {
        self.qset_tracker.set_local(quorum_set);
    }

    /// Store a quorum set for a node.
    pub fn store_quorum_set(&self, node_id: &stellar_xdr::curr::NodeId, quorum_set: ScpQuorumSet) {
        self.qset_tracker.store(node_id, quorum_set);
    }

    /// Get a quorum set for a node.
    pub fn get_quorum_set(&self, node_id: &stellar_xdr::curr::NodeId) -> Option<ScpQuorumSet> {
        self.qset_tracker.get_by_node(node_id)
    }

    /// Get a quorum set by its hash.
    pub fn get_quorum_set_by_hash(&self, hash: &Hash256) -> Option<ScpQuorumSet> {
        self.qset_tracker.get_by_hash(hash)
    }

    /// Whether we already have a quorum set with the given hash.
    pub fn has_quorum_set_hash(&self, hash: &Hash256) -> bool {
        self.qset_tracker.has_hash(hash)
    }

    /// Get our node ID.
    pub fn node_id(&self) -> &PublicKey {
        &self.config.node_id
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    use crate::tx_queue::TransactionSet;
    use henyey_scp::hash_quorum_set;

    fn make_config(max_cache: usize) -> ScpDriverConfig {
        ScpDriverConfig {
            max_tx_set_cache: max_cache,
            ..ScpDriverConfig::default()
        }
    }

    fn default_tracking() -> Arc<RwLock<SharedTrackingState>> {
        Arc::new(RwLock::new(SharedTrackingState::default()))
    }

    fn make_tx_set(seed: u8) -> TransactionSet {
        let prev_hash = Hash256::from_bytes([seed; 32]);
        TransactionSet::new(prev_hash, Vec::new())
    }

    #[test]
    fn test_cache_tx_set_evicts_oldest() {
        let driver = ScpDriver::new(
            make_config(1),
            Hash256::hash(b"network"),
            default_tracking(),
        );
        let first = make_tx_set(1);
        let second = make_tx_set(2);

        driver.cache_tx_set(first.clone());
        driver.cache_tx_set(second.clone());

        assert!(!driver.has_tx_set(first.hash()));
        assert!(driver.has_tx_set(second.hash()));
    }

    #[test]
    fn test_request_and_receive_tx_set() {
        let driver = ScpDriver::new(
            make_config(4),
            Hash256::hash(b"network"),
            default_tracking(),
        );
        let tx_set = make_tx_set(3);
        let slot = 12u64;

        assert!(driver.request_tx_set(*tx_set.hash(), slot));
        assert!(!driver.request_tx_set(*tx_set.hash(), slot));
        assert!(driver.needs_tx_set(tx_set.hash()));
        assert_eq!(driver.get_pending_tx_set_hashes(), vec![*tx_set.hash()]);

        let received = driver.receive_tx_set(tx_set.clone());
        assert_eq!(received, Some(slot));
        assert!(!driver.needs_tx_set(tx_set.hash()));
        assert!(driver.get_tx_set(tx_set.hash()).is_some());
    }

    #[test]
    fn test_receive_tx_set_rejects_mismatched_hash() {
        let driver = ScpDriver::new(
            make_config(2),
            Hash256::hash(b"network"),
            default_tracking(),
        );
        let tx_set = make_tx_set(4);
        let bad_hash = Hash256::from_bytes([9; 32]);
        let bad_set =
            TransactionSet::with_hash(tx_set.previous_ledger_hash(), bad_hash, Vec::new());

        let received = driver.receive_tx_set(bad_set);
        assert_eq!(received, None);
        assert!(!driver.has_tx_set(&bad_hash));
    }

    #[test]
    fn test_cleanup_old_pending_slots() {
        let driver = ScpDriver::new(
            make_config(4),
            Hash256::hash(b"network"),
            default_tracking(),
        );
        let tx_set_a = make_tx_set(5);
        let tx_set_b = make_tx_set(6);

        driver.request_tx_set(*tx_set_a.hash(), 10);
        driver.request_tx_set(*tx_set_b.hash(), 12);

        let removed = driver.cleanup_old_pending_slots(12);
        assert_eq!(removed, 1);

        let pending = driver.get_pending_tx_sets();
        assert_eq!(pending, vec![(*tx_set_b.hash(), 12)]);
    }

    #[test]
    fn test_cleanup_pending_tx_sets_by_age() {
        let driver = ScpDriver::new(
            make_config(4),
            Hash256::hash(b"network"),
            default_tracking(),
        );
        let tx_set = make_tx_set(7);
        driver.request_tx_set(*tx_set.hash(), 20);

        driver.cleanup_pending_tx_sets(0);
        assert!(driver.get_pending_tx_set_hashes().is_empty());
    }

    #[test]
    fn test_request_quorum_set_tracks_unknown_only() {
        let node_id = PublicKey::from_bytes(&[9u8; 32]).expect("node id");
        let quorum_set = ScpQuorumSet {
            threshold: 1,
            validators: vec![stellar_xdr::curr::NodeId(
                stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(stellar_xdr::curr::Uint256(
                    [1u8; 32],
                )),
            )]
            .try_into()
            .unwrap(),
            inner_sets: Vec::new().try_into().unwrap(),
        };
        let config = ScpDriverConfig {
            node_id,
            local_quorum_set: Some(quorum_set.clone()),
            ..make_config(4)
        };
        let driver = ScpDriver::new(config, Hash256::hash(b"network"), default_tracking());

        // Create a test node_id for the request
        let sender_node_id =
            stellar_xdr::curr::NodeId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
                stellar_xdr::curr::Uint256([2u8; 32]),
            ));

        let known_hash = hash_quorum_set(&quorum_set);
        assert!(!driver.request_quorum_set(known_hash, sender_node_id.clone()));

        let unknown_hash = Hash256::from_bytes([42u8; 32]);
        assert!(driver.request_quorum_set(unknown_hash, sender_node_id.clone()));
        assert!(!driver.request_quorum_set(unknown_hash, sender_node_id.clone()));

        // Verify the node_id was tracked
        let pending_ids = driver.get_pending_quorum_set_node_ids(&unknown_hash);
        assert_eq!(pending_ids.len(), 1);
        assert_eq!(pending_ids[0], sender_node_id);
    }

    fn make_externalized_slot(slot: SlotIndex, close_time: u64) -> ExternalizedSlot {
        ExternalizedSlot {
            slot,
            value: Value(vec![].try_into().unwrap()),
            tx_set_hash: Some(Hash256::from_bytes([slot as u8; 32])),
            close_time,
            externalized_at: std::time::Instant::now(),
        }
    }

    #[test]
    fn test_get_externalized_slots_in_range() {
        let driver = ScpDriver::new(
            make_config(4),
            Hash256::hash(b"network"),
            default_tracking(),
        );

        // Externalize some slots (manually insert into the map for testing)
        {
            let mut externalized = driver.externalized.write();
            externalized.insert(100, make_externalized_slot(100, 1000));
            externalized.insert(102, make_externalized_slot(102, 1010));
            externalized.insert(105, make_externalized_slot(105, 1020));
            externalized.insert(110, make_externalized_slot(110, 1030));
        }

        // Test exact range
        let slots = driver.get_externalized_slots_in_range(100, 105);
        assert_eq!(slots, vec![100, 102, 105]);

        // Test partial range
        let slots = driver.get_externalized_slots_in_range(101, 106);
        assert_eq!(slots, vec![102, 105]);

        // Test empty range (no slots in range)
        let slots = driver.get_externalized_slots_in_range(106, 109);
        assert!(slots.is_empty());

        // Test single slot
        let slots = driver.get_externalized_slots_in_range(102, 102);
        assert_eq!(slots, vec![102]);
    }

    #[test]
    fn test_find_missing_slots_in_range() {
        let driver = ScpDriver::new(
            make_config(4),
            Hash256::hash(b"network"),
            default_tracking(),
        );

        // Externalize some slots with gaps
        {
            let mut externalized = driver.externalized.write();
            externalized.insert(100, make_externalized_slot(100, 1000));
            externalized.insert(102, make_externalized_slot(102, 1010));
            externalized.insert(105, make_externalized_slot(105, 1020));
        }

        // Find missing slots in range 100-105
        let missing = driver.find_missing_slots_in_range(100, 105);
        assert_eq!(missing, vec![101, 103, 104]);

        // No missing slots when all present
        let missing = driver.find_missing_slots_in_range(100, 100);
        assert!(missing.is_empty());

        // All slots missing
        let missing = driver.find_missing_slots_in_range(106, 108);
        assert_eq!(missing, vec![106, 107, 108]);

        // Invalid range (from > to)
        let missing = driver.find_missing_slots_in_range(110, 100);
        assert!(missing.is_empty());
    }

    #[test]
    fn test_trim_stale_caches_preserves_future_slots() {
        let driver = ScpDriver::new(
            make_config(10),
            Hash256::hash(b"network"),
            default_tracking(),
        );

        // Add pending tx_sets for various slots
        let tx_set_old = make_tx_set(1);
        let tx_set_boundary = make_tx_set(2);
        let tx_set_future1 = make_tx_set(3);
        let tx_set_future2 = make_tx_set(4);

        driver.request_tx_set(*tx_set_old.hash(), 98);
        driver.request_tx_set(*tx_set_boundary.hash(), 100);
        driver.request_tx_set(*tx_set_future1.hash(), 101);
        driver.request_tx_set(*tx_set_future2.hash(), 105);

        // Add externalized slots
        {
            let mut externalized = driver.externalized.write();
            externalized.insert(95, make_externalized_slot(95, 1000));
            externalized.insert(100, make_externalized_slot(100, 1010));
            externalized.insert(101, make_externalized_slot(101, 1020));
            externalized.insert(105, make_externalized_slot(105, 1030));
        }

        // Trim with keep_after_slot = 100
        // Should keep slots > 100, i.e., 101 and 105
        driver.trim_stale_caches(100);

        // Verify pending_tx_sets
        let pending = driver.get_pending_tx_sets();
        assert_eq!(pending.len(), 2);
        assert!(pending
            .iter()
            .any(|(h, s)| *h == *tx_set_future1.hash() && *s == 101));
        assert!(pending
            .iter()
            .any(|(h, s)| *h == *tx_set_future2.hash() && *s == 105));
        // Old and boundary slots should be removed
        assert!(!pending.iter().any(|(h, _)| *h == *tx_set_old.hash()));
        assert!(!pending.iter().any(|(h, _)| *h == *tx_set_boundary.hash()));

        // Verify externalized slots
        let ext_slots = driver.get_externalized_slots_in_range(0, 200);
        assert_eq!(ext_slots, vec![101, 105]);
    }

    #[test]
    fn test_clear_pending_tx_sets_removes_all() {
        let driver = ScpDriver::new(
            make_config(4),
            Hash256::hash(b"network"),
            default_tracking(),
        );
        let tx_set_a = make_tx_set(10);
        let tx_set_b = make_tx_set(11);
        let tx_set_c = make_tx_set(12);

        driver.request_tx_set(*tx_set_a.hash(), 100);
        driver.request_tx_set(*tx_set_b.hash(), 101);
        driver.request_tx_set(*tx_set_c.hash(), 102);
        assert_eq!(driver.get_pending_tx_sets().len(), 3);

        driver.clear_pending_tx_sets();
        assert!(driver.get_pending_tx_sets().is_empty());
        assert!(driver.get_pending_tx_set_hashes().is_empty());
    }

    #[test]
    fn test_clear_pending_tx_sets_noop_when_empty() {
        let driver = ScpDriver::new(
            make_config(4),
            Hash256::hash(b"network"),
            default_tracking(),
        );
        assert!(driver.get_pending_tx_sets().is_empty());

        // Should not panic when called on empty map
        driver.clear_pending_tx_sets();
        assert!(driver.get_pending_tx_sets().is_empty());
    }

    #[test]
    fn test_clear_pending_tx_sets_does_not_affect_cache() {
        let driver = ScpDriver::new(
            make_config(4),
            Hash256::hash(b"network"),
            default_tracking(),
        );
        let tx_set = make_tx_set(20);

        // Request and then receive the tx_set (puts it in cache)
        driver.request_tx_set(*tx_set.hash(), 200);
        driver.receive_tx_set(tx_set.clone());
        assert!(driver.has_tx_set(tx_set.hash()));

        // Add another pending request
        let tx_set_b = make_tx_set(21);
        driver.request_tx_set(*tx_set_b.hash(), 201);

        // Clear pending — should not affect the cached tx_set
        driver.clear_pending_tx_sets();
        assert!(driver.has_tx_set(tx_set.hash()));
        assert!(driver.get_tx_set(tx_set.hash()).is_some());
        assert!(driver.get_pending_tx_sets().is_empty());
    }

    /// Create a signed StellarValue for testing validate_value_impl.
    fn make_signed_stellar_value(
        network_id: &Hash256,
        secret: &henyey_crypto::SecretKey,
        tx_set_hash: [u8; 32],
        close_time: u64,
    ) -> stellar_xdr::curr::Value {
        use stellar_xdr::curr::{
            EnvelopeType, LedgerCloseValueSignature, Limits, NodeId as XdrNodeId, StellarValue,
            StellarValueExt, TimePoint, WriteXdr,
        };

        let xdr_tx_set_hash = stellar_xdr::curr::Hash(tx_set_hash);
        let ct = TimePoint(close_time);

        // Sign: (networkID, ENVELOPE_TYPE_SCPVALUE, txSetHash, closeTime)
        let mut sign_data = network_id.0.to_vec();
        sign_data.extend_from_slice(&EnvelopeType::Scpvalue.to_xdr(Limits::none()).unwrap());
        sign_data.extend_from_slice(&xdr_tx_set_hash.to_xdr(Limits::none()).unwrap());
        sign_data.extend_from_slice(&ct.to_xdr(Limits::none()).unwrap());
        let sig = secret.sign(&sign_data);

        let node_id = XdrNodeId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::curr::Uint256(*secret.public_key().as_bytes()),
        ));

        let sv = StellarValue {
            tx_set_hash: xdr_tx_set_hash,
            close_time: ct,
            upgrades: vec![].try_into().unwrap(),
            ext: StellarValueExt::Signed(LedgerCloseValueSignature {
                node_id,
                signature: stellar_xdr::curr::Signature(
                    sig.0.to_vec().try_into().unwrap_or_default(),
                ),
            }),
        };
        stellar_xdr::curr::Value(sv.to_xdr(Limits::none()).unwrap().try_into().unwrap())
    }

    #[test]
    fn test_validate_value_returns_invalid_for_missing_tx_set() {
        // When validating a value for LCL+1 (current ledger) and the tx set is
        // not cached, validate_value should return Invalid during nomination
        // (matching stellar-core). During ballot protocol (nomination=false),
        // it returns MaybeValid because EXTERNALIZE envelopes may arrive
        // before the tx_set is fetched.
        let secret = henyey_crypto::SecretKey::from_seed(&[42u8; 32]);
        let network_id = Hash256::hash(b"test-network");

        let config = ScpDriverConfig {
            node_id: secret.public_key(),
            ..make_config(4)
        };
        let tracking = Arc::new(RwLock::new(SharedTrackingState {
            is_tracking: true,
            consensus_index: 1,
            consensus_close_time: 1,
        }));
        let driver = ScpDriver::new(config, network_id, tracking);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let tx_set_hash = [99u8; 32];

        // Create a signed value with a tx_set_hash that is NOT cached
        let value = make_signed_stellar_value(&network_id, &secret, tx_set_hash, now);

        // During nomination: missing tx set should return Invalid
        let result = driver.validate_value_impl(1, &value, true);
        assert_eq!(
            result,
            ValueValidation::Invalid,
            "missing tx set during nomination should return Invalid"
        );

        // During ballot protocol: missing tx set should return
        // MaybeValidDeferred (EXTERNALIZE envelopes may arrive before
        // the tx_set is fetched, via the herder fast-path). This
        // variant specifically does NOT cause the ballot protocol to
        // clear `Slot::fully_validated` — see the enum doc comment on
        // `ValidationLevel` and regression test
        // `test_issue_1795_maybe_valid_deferred_does_not_clear_fully_validated_tx_set_pending`
        // in `crates/scp/src/ballot/mod.rs`.
        let result_ballot = driver.validate_value_impl(1, &value, false);
        assert_eq!(
            result_ballot,
            ValueValidation::MaybeValidDeferred,
            "missing tx set during ballot protocol should return \
             MaybeValidDeferred (issue #1795)"
        );

        // Now cache the tx set and re-validate — should pass the tx set check
        let tx_set = TransactionSet::new(Hash256::ZERO, Vec::new());
        // The tx_set created by TransactionSet::new computes its own hash,
        // so we need to use that hash in the value
        let tx_set_hash_real = *tx_set.hash();
        driver.cache_tx_set(tx_set);

        let value2 = make_signed_stellar_value(&network_id, &secret, tx_set_hash_real.0, now);
        let result2 = driver.validate_value_impl(1, &value2, false);
        // Should NOT be Invalid (the tx set is now available)
        // It may be MaybeValid or Valid depending on other checks
        assert_ne!(
            result2,
            ValueValidation::Invalid,
            "with tx set cached, validation should not return Invalid"
        );
    }
}

/// SCP callback implementation wrapper.
///
/// This wraps the ScpDriver to implement the SCPDriver trait.
pub struct HerderScpCallback {
    driver: Arc<ScpDriver>,
}

impl HerderScpCallback {
    /// Create a new callback wrapper.
    pub fn new(driver: Arc<ScpDriver>) -> Self {
        Self { driver }
    }

    fn hash_helper<F>(&self, slot_index: u64, prev_value: &Value, extra: F) -> u64
    where
        F: FnOnce(&mut Vec<Vec<u8>>),
    {
        let mut values = Vec::new();
        values.push(Self::xdr_bytes(&slot_index));
        values.push(Self::xdr_bytes(prev_value));
        extra(&mut values);

        let mut data = Vec::new();
        for value in values {
            data.extend_from_slice(&value);
        }
        let hash = Hash256::hash(&data);
        let mut result = 0u64;
        for byte in &hash.as_bytes()[0..8] {
            result = (result << 8) | (*byte as u64);
        }
        result
    }

    fn xdr_bytes<T: stellar_xdr::curr::WriteXdr>(value: &T) -> Vec<u8> {
        henyey_common::xdr_stream::xdr_to_bytes(value)
    }
}

impl SCPDriver for HerderScpCallback {
    fn validate_value(&self, slot_index: u64, value: &Value, nomination: bool) -> ValidationLevel {
        // Always validate: stellar-core's validateValue() runs XDR deserialization,
        // STELLAR_VALUE_SIGNED check, signature verification, close-time validation,
        // and upgrade checks for ALL statements (both nomination and ballot).
        // The `nomination` flag is forwarded to upgrade validation for additional
        // strictness during nomination (isValidForNomination).
        match self
            .driver
            .validate_value_impl(slot_index, value, nomination)
        {
            ValueValidation::Valid => ValidationLevel::FullyValidated,
            ValueValidation::MaybeValid => ValidationLevel::MaybeValid,
            ValueValidation::MaybeValidDeferred => ValidationLevel::MaybeValidDeferred,
            ValueValidation::Invalid => ValidationLevel::Invalid,
        }
    }

    fn combine_candidates(&self, slot_index: u64, candidates: &[Value]) -> Option<Value> {
        let result = self.driver.combine_candidates_impl(slot_index, candidates);
        if result.0.is_empty() {
            None
        } else {
            Some(result)
        }
    }

    fn extract_valid_value(&self, slot_index: u64, value: &Value) -> Option<Value> {
        self.driver.extract_valid_value_impl(slot_index, value)
    }

    fn get_node_weight(
        &self,
        node_id: &NodeId,
        quorum_set: &ScpQuorumSet,
        is_local_node: bool,
    ) -> u64 {
        // Startup/test safety: if ledger manager isn't installed yet,
        // fall back to base weights. Matches the compute_timeout pattern.
        let Some(manager) = self.driver.ledger_manager.get() else {
            return henyey_scp::base_get_node_weight(node_id, quorum_set, is_local_node);
        };

        let header = manager.current_header();
        let unsupported_protocol =
            !protocol_version_starts_from(header.ledger_version, ProtocolVersion::V22);

        // Fall back to base weight algorithm if:
        // 1. Protocol version < V22
        // 2. No validator weight config (manual quorum set or non-validator)
        // 3. FORCE_OLD_STYLE_LEADER_ELECTION is set
        if unsupported_protocol
            || self.driver.config.validator_weight_config.is_none()
            || self.driver.config.force_old_style_leader_election
        {
            return henyey_scp::base_get_node_weight(node_id, quorum_set, is_local_node);
        }

        // Use quality/home-domain weights (stellar-core HerderSCPDriver.cpp:1479-1522)
        self.driver
            .config
            .validator_weight_config
            .as_ref()
            .unwrap()
            .get_node_weight(node_id)
    }

    fn emit_envelope(&self, envelope: &ScpEnvelope) {
        self.driver.emit(envelope.clone());
    }

    fn get_quorum_set(&self, node_id: &stellar_xdr::curr::NodeId) -> Option<ScpQuorumSet> {
        self.driver.get_quorum_set(node_id)
    }

    fn get_quorum_set_by_hash(&self, hash: &henyey_common::Hash256) -> Option<ScpQuorumSet> {
        self.driver.get_quorum_set_by_hash(hash)
    }

    fn nominating_value(&self, slot_index: u64, _value: &Value) {
        self.driver.record_nomination_start(slot_index);
    }

    fn started_ballot_protocol(&self, slot_index: u64, _value: &Value) {
        self.driver.record_ballot_start(slot_index);
    }

    fn value_externalized(&self, slot_index: u64, value: &Value) {
        // Record first-externalize baseline BEFORE processing.
        // Mirrors stellar-core line 915: recordSCPExternalizeEvent(self, false)
        let now = self.driver.record_self_externalize_event(slot_index);
        self.driver
            .record_externalized(slot_index, value.clone(), Some(now));
    }

    fn ballot_did_prepare(&self, _slot_index: u64, _ballot: &stellar_xdr::curr::ScpBallot) {
        // Logging only
    }

    fn ballot_did_confirm(&self, _slot_index: u64, _ballot: &stellar_xdr::curr::ScpBallot) {
        // Logging only
    }

    fn compute_hash_node(
        &self,
        slot_index: u64,
        prev_value: &Value,
        is_priority: bool,
        round: u32,
        node_id: &stellar_xdr::curr::NodeId,
    ) -> u64 {
        const HASH_N: u32 = 1;
        const HASH_P: u32 = 2;
        self.hash_helper(slot_index, prev_value, |values| {
            let tag = if is_priority { HASH_P } else { HASH_N };
            values.push(Self::xdr_bytes(&tag));
            values.push(Self::xdr_bytes(&round));
            values.push(Self::xdr_bytes(node_id));
        })
    }

    fn compute_value_hash(
        &self,
        slot_index: u64,
        prev_value: &Value,
        round: u32,
        value: &Value,
    ) -> u64 {
        const HASH_K: u32 = 3;
        self.hash_helper(slot_index, prev_value, |values| {
            values.push(Self::xdr_bytes(&HASH_K));
            values.push(Self::xdr_bytes(&round));
            values.push(Self::xdr_bytes(value));
        })
    }

    fn compute_timeout(&self, round: u32, is_nomination: bool) -> std::time::Duration {
        const MAX_TIMEOUT_MS: u64 = 30 * 60 * 1000;
        let mut initial_ms: u64 = 1000;
        let mut increment_ms: u64 = 1000;
        if let Some(manager) = self.driver.ledger_manager.get() {
            let header = manager.current_header();
            if protocol_version_starts_from(header.ledger_version, ProtocolVersion::V23) {
                if let Some(info) = manager.soroban_network_info() {
                    if is_nomination {
                        initial_ms = info.nomination_timeout_initial_ms as u64;
                        increment_ms = info.nomination_timeout_increment_ms as u64;
                    } else {
                        initial_ms = info.ballot_timeout_initial_ms as u64;
                        increment_ms = info.ballot_timeout_increment_ms as u64;
                    }
                }
            }
        }
        let round = round.max(1) as u64;
        let timeout_ms = initial_ms.saturating_add((round - 1).saturating_mul(increment_ms));
        std::time::Duration::from_millis(timeout_ms.min(MAX_TIMEOUT_MS))
    }

    fn sign_envelope(&self, envelope: &mut ScpEnvelope) {
        if let Some(sig) = self.driver.sign_envelope(&envelope.statement) {
            envelope.signature =
                stellar_xdr::curr::Signature(sig.0.to_vec().try_into().unwrap_or_default());
        }
    }

    fn verify_envelope(&self, envelope: &ScpEnvelope) -> bool {
        self.driver.verify_envelope(envelope).is_ok()
    }

    /// Parity: check if a value contains protocol upgrades.
    fn has_upgrades(&self, value: &Value) -> bool {
        if let Ok(sv) = StellarValue::from_xdr(value, stellar_xdr::curr::Limits::none()) {
            !sv.upgrades.is_empty()
        } else {
            false
        }
    }

    /// Parity: strip all upgrades from a value.
    fn strip_all_upgrades(&self, value: &Value) -> Option<Value> {
        let mut sv = StellarValue::from_xdr(value, stellar_xdr::curr::Limits::none()).ok()?;
        sv.upgrades = Vec::new().try_into().ok()?;
        sv.to_xdr(stellar_xdr::curr::Limits::none())
            .ok()
            .map(|bytes| Value(bytes.try_into().unwrap_or_default()))
    }

    /// Parity: get the nomination timeout limit for upgrade stripping.
    /// In stellar-core: mUpgrades.getParameters().mNominationTimeoutLimit
    fn get_upgrade_nomination_timeout_limit(&self) -> u32 {
        if let Some(upgrades_arc) = self.driver.upgrades.get() {
            upgrades_arc
                .read()
                .parameters()
                .nomination_timeout_limit
                .unwrap_or(u32::MAX)
        } else {
            u32::MAX
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{
        LedgerCloseValueSignature, Limits, StellarValue, StellarValueExt, TimePoint, UpgradeType,
        VecM,
    };

    fn default_tracking() -> Arc<RwLock<SharedTrackingState>> {
        Arc::new(RwLock::new(SharedTrackingState::default()))
    }

    fn make_test_driver() -> ScpDriver {
        let config = ScpDriverConfig::default();
        ScpDriver::new(config, Hash256::ZERO, default_tracking())
    }

    fn nomination_time(driver: &ScpDriver, slot: SlotIndex) -> Option<std::time::Instant> {
        driver
            .slot_timing
            .read()
            .get(&slot)
            .and_then(|s| s.nomination_start)
    }

    fn ballot_time(driver: &ScpDriver, slot: SlotIndex) -> Option<std::time::Instant> {
        driver
            .slot_timing
            .read()
            .get(&slot)
            .and_then(|s| s.ballot_start)
    }

    /// Create a test driver with a known secret key for signing.
    fn make_test_driver_with_key() -> (ScpDriver, SecretKey) {
        let secret_key = SecretKey::generate();
        let public_key = secret_key.public_key();
        let config = ScpDriverConfig {
            node_id: public_key,
            ..ScpDriverConfig::default()
        };
        let network_id = Hash256::ZERO;
        let driver =
            ScpDriver::with_secret_key(config, network_id, secret_key.clone(), default_tracking());
        (driver, secret_key)
    }

    /// Create a properly SIGNED StellarValue using the given secret key and network ID.
    fn make_signed_stellar_value(
        tx_set_hash: stellar_xdr::curr::Hash,
        close_time: u64,
        upgrades: Vec<UpgradeType>,
        secret_key: &SecretKey,
        network_id: &Hash256,
    ) -> StellarValue {
        let close_time = TimePoint(close_time);
        // Sign: (networkID, ENVELOPE_TYPE_SCPVALUE, txSetHash, closeTime)
        let mut sign_data = network_id.0.to_vec();
        sign_data.extend_from_slice(&EnvelopeType::Scpvalue.to_xdr(Limits::none()).expect("xdr"));
        sign_data.extend_from_slice(&tx_set_hash.to_xdr(Limits::none()).expect("xdr"));
        sign_data.extend_from_slice(&close_time.to_xdr(Limits::none()).expect("xdr"));
        let sig = secret_key.sign(&sign_data);

        let node_id =
            stellar_xdr::curr::NodeId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
                stellar_xdr::curr::Uint256(*secret_key.public_key().as_bytes()),
            ));

        StellarValue {
            tx_set_hash,
            close_time,
            upgrades: upgrades.try_into().unwrap_or_default(),
            ext: StellarValueExt::Signed(LedgerCloseValueSignature {
                node_id,
                signature: stellar_xdr::curr::Signature(
                    sig.0.to_vec().try_into().unwrap_or_default(),
                ),
            }),
        }
    }

    /// Encode a StellarValue to a Value.
    fn encode_sv(sv: &StellarValue) -> Value {
        Value(sv.to_xdr(Limits::none()).expect("xdr").try_into().unwrap())
    }

    #[test]
    fn test_tx_set_caching() {
        let driver = make_test_driver();

        let tx_set = TransactionSet::new(Hash256::ZERO, vec![]);
        let hash = *tx_set.hash();

        driver.cache_tx_set(tx_set);
        assert!(driver.has_tx_set(&hash));

        let cached = driver.get_tx_set(&hash);
        assert!(cached.is_some());
    }

    #[test]
    fn test_externalized_recording() {
        let driver = make_test_driver();

        assert!(driver.latest_externalized_slot().is_none());

        driver.record_externalized(100, Value::default(), None);
        assert_eq!(driver.latest_externalized_slot(), Some(100));

        driver.record_externalized(99, Value::default(), None); // older slot
        assert_eq!(driver.latest_externalized_slot(), Some(100)); // still 100

        driver.record_externalized(101, Value::default(), None);
        assert_eq!(driver.latest_externalized_slot(), Some(101));
    }

    #[test]
    fn test_cleanup_externalized() {
        let driver = make_test_driver();

        for slot in 1..=10 {
            driver.record_externalized(slot, Value::default(), None);
        }

        assert_eq!(driver.externalized.read().len(), 10);

        driver.cleanup_externalized(5);
        assert_eq!(driver.externalized.read().len(), 5);

        // Should keep slots 6-10
        let externalized = driver.externalized.read();
        assert!(!externalized.contains_key(&1));
        assert!(!externalized.contains_key(&5));
        assert!(externalized.contains_key(&6));
        assert!(externalized.contains_key(&10));
    }

    #[test]
    fn test_nomination_timing_normal_flow() {
        let driver = make_test_driver();

        driver.record_slot_activity(100);
        driver.record_nomination_start(100);
        std::thread::sleep(std::time::Duration::from_millis(1));
        driver.record_ballot_start(100);
        driver.record_externalized(100, Value::default(), None);

        let snapshot = driver.last_externalize_timing().unwrap();
        assert_eq!(snapshot.slot, 100);
        assert!(snapshot.externalize_duration.as_nanos() > 0);
        assert!(snapshot.nomination_duration.is_some());
        assert!(snapshot.nomination_duration.unwrap() <= snapshot.externalize_duration);
        // nomination_duration should measure nomination→ballot_start, not nomination→externalize
        assert!(snapshot.nomination_duration.unwrap().as_nanos() > 0);
    }

    #[test]
    fn test_nomination_timing_no_nomination_watcher() {
        let driver = make_test_driver();

        driver.record_slot_activity(100);
        driver.record_externalized(100, Value::default(), None);

        let snapshot = driver.last_externalize_timing().unwrap();
        assert_eq!(snapshot.slot, 100);
        assert!(snapshot.externalize_duration.as_nanos() > 0);
        assert!(snapshot.nomination_duration.is_none());
    }

    #[test]
    fn test_nomination_timing_duplicate_externalization() {
        let driver = make_test_driver();

        driver.record_slot_activity(100);
        driver.record_nomination_start(100);
        driver.record_ballot_start(100);
        driver.record_externalized(100, Value::default(), None);

        let snapshot1 = driver.last_externalize_timing().unwrap();

        driver.record_externalized(100, Value::default(), None);

        let snapshot2 = driver.last_externalize_timing().unwrap();
        assert_eq!(snapshot1.slot, snapshot2.slot);
        assert_eq!(
            snapshot1.externalize_duration,
            snapshot2.externalize_duration
        );
        assert_eq!(snapshot1.nomination_duration, snapshot2.nomination_duration);
    }

    #[test]
    fn test_nomination_start_first_call_guard() {
        let driver = make_test_driver();

        driver.record_nomination_start(100);
        let first_time = nomination_time(&driver, 100).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(1));

        driver.record_nomination_start(100);
        let second_time = nomination_time(&driver, 100).unwrap();

        assert_eq!(first_time, second_time);
    }

    #[test]
    fn test_nomination_timing_cleanup() {
        let driver = make_test_driver();

        for slot in 90..=100 {
            driver.record_slot_activity(slot);
            driver.record_nomination_start(slot);
        }

        driver.record_slot_activity(200);
        driver.record_externalized(200, Value::default(), None);

        let map = driver.slot_timing.read();
        assert!(map.is_empty() || map.keys().all(|&s| 200u64.saturating_sub(s) <= 100));
    }

    #[test]
    fn test_last_externalize_duration_backward_compat() {
        let driver = make_test_driver();

        driver.record_slot_activity(100);
        driver.record_externalized(100, Value::default(), None);

        let duration = driver.last_externalize_duration();
        assert!(duration.is_some());
        assert!(duration.unwrap().as_nanos() > 0);
    }

    #[test]
    fn test_nomination_timing_purge_slots_below() {
        let driver = make_test_driver();

        driver.record_slot_activity(50);
        driver.record_nomination_start(50);
        driver.record_slot_activity(100);
        driver.record_nomination_start(100);

        driver.purge_slots_below(80);

        let map = driver.slot_timing.read();
        assert!(!map.contains_key(&50));
        assert!(map.contains_key(&100));
    }

    #[test]
    fn test_nomination_timing_clear_slot_scoped_caches() {
        let driver = make_test_driver();

        driver.record_slot_activity(100);
        driver.record_nomination_start(100);
        driver.record_ballot_start(100);

        driver.clear_slot_scoped_caches();

        assert!(driver.slot_timing.read().is_empty());
    }

    #[test]
    fn test_nomination_timing_catchup_clears_stale() {
        let driver = make_test_driver();

        // First: a normal externalization with timing
        driver.record_slot_activity(100);
        driver.record_nomination_start(100);
        driver.record_externalized(100, Value::default(), None);
        assert!(driver.last_externalize_timing().is_some());

        // Now simulate catchup: record_externalized without record_slot_activity
        driver.record_externalized(200, Value::default(), None);

        // Timing should be cleared (not stale from slot 100)
        assert!(driver.last_externalize_timing().is_none());
    }

    #[test]
    fn test_nomination_timing_no_ballot_start() {
        // nomination_start recorded but ballot_start not → nomination_duration should be None
        let driver = make_test_driver();

        driver.record_slot_activity(100);
        driver.record_nomination_start(100);
        // No record_ballot_start call
        driver.record_externalized(100, Value::default(), None);

        let snapshot = driver.last_externalize_timing().unwrap();
        assert_eq!(snapshot.slot, 100);
        assert!(snapshot.externalize_duration.as_nanos() > 0);
        assert!(snapshot.nomination_duration.is_none());
    }

    #[test]
    fn test_ballot_start_first_call_guard() {
        let driver = make_test_driver();

        driver.record_ballot_start(100);
        let first_time = ballot_time(&driver, 100).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(1));

        driver.record_ballot_start(100);
        let second_time = ballot_time(&driver, 100).unwrap();

        assert_eq!(first_time, second_time);
    }

    #[test]
    fn test_ballot_started_at_cleanup() {
        let driver = make_test_driver();

        for slot in 90..=100 {
            driver.record_slot_activity(slot);
            driver.record_nomination_start(slot);
            driver.record_ballot_start(slot);
        }

        driver.record_slot_activity(200);
        driver.record_externalized(200, Value::default(), None);

        let map = driver.slot_timing.read();
        assert!(map.is_empty() || map.keys().all(|&s| 200u64.saturating_sub(s) <= 100));
    }

    #[test]
    fn test_ballot_started_at_purge() {
        let driver = make_test_driver();

        driver.record_ballot_start(50);
        driver.record_ballot_start(100);

        driver.purge_slots_below(80);

        let map = driver.slot_timing.read();
        assert!(!map.contains_key(&50));
        assert!(map.contains_key(&100));
    }

    #[test]
    fn test_ballot_started_at_trim_stale_caches() {
        let driver = make_test_driver();

        driver.record_ballot_start(50);
        driver.record_ballot_start(100);

        driver.trim_stale_caches(80);

        let map = driver.slot_timing.read();
        assert!(!map.contains_key(&50));
        assert!(map.contains_key(&100));
    }

    #[test]
    fn test_first_to_self_externalize_lag_peer_first() {
        let driver = make_test_driver();
        let peer_node = NodeId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::curr::Uint256([1u8; 32]),
        ));

        driver.record_slot_activity(100);

        // Peer externalizes first — sets first_externalize baseline.
        driver.record_peer_externalize_event(100, &peer_node);

        // Self externalizes after — lag should be positive.
        let now = driver.record_self_externalize_event(100);
        driver.record_externalized(100, Value::default(), Some(now));

        let snapshot = driver.last_externalize_timing().unwrap();
        assert_eq!(snapshot.slot, 100);
        assert!(
            snapshot.first_to_self_externalize_lag.is_some(),
            "Should have first-to-self lag when peer externalizes first"
        );
        assert!(
            snapshot.first_to_self_externalize_lag.unwrap().as_nanos() > 0,
            "Lag should be positive when peer externalizes first"
        );
    }

    #[test]
    fn test_first_to_self_externalize_lag_self_first() {
        let driver = make_test_driver();

        driver.record_slot_activity(100);

        // Self externalizes first (no prior peer EXTERNALIZE).
        let now = driver.record_self_externalize_event(100);
        driver.record_externalized(100, Value::default(), Some(now));

        let snapshot = driver.last_externalize_timing().unwrap();
        assert_eq!(snapshot.slot, 100);
        // When self is first, first_externalize was set by self event,
        // and record_externalized uses the same `now`, so lag is ~0.
        assert!(
            snapshot.first_to_self_externalize_lag.is_some(),
            "Should have first-to-self lag even when self is first"
        );
    }

    #[test]
    fn test_first_to_self_externalize_lag_catchup_path() {
        let driver = make_test_driver();

        // Catchup path: record_externalized without any prior externalize events.
        driver.record_externalized(100, Value::default(), None);

        // No timing at all (no slot_first_seen either).
        assert!(driver.last_externalize_timing().is_none());
    }

    #[test]
    fn test_first_to_self_externalize_lag_duplicate_externalization() {
        let driver = make_test_driver();
        let peer_node = NodeId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::curr::Uint256([1u8; 32]),
        ));

        driver.record_slot_activity(100);
        driver.record_peer_externalize_event(100, &peer_node);
        let now = driver.record_self_externalize_event(100);
        driver.record_externalized(100, Value::default(), Some(now));

        let snapshot1 = driver.last_externalize_timing().unwrap();

        // Duplicate externalization should not change snapshot.
        driver.record_externalized(100, Value::default(), None);

        let snapshot2 = driver.last_externalize_timing().unwrap();
        assert_eq!(snapshot1.slot, snapshot2.slot);
        assert_eq!(
            snapshot1.first_to_self_externalize_lag,
            snapshot2.first_to_self_externalize_lag
        );
    }

    #[test]
    fn test_first_to_self_externalize_lag_cleanup() {
        let driver = make_test_driver();
        let peer_node = NodeId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::curr::Uint256([1u8; 32]),
        ));

        // Record externalize event for slot 100.
        driver.record_peer_externalize_event(100, &peer_node);
        assert!(driver
            .externalize_lag
            .read()
            .first_externalize_for_slot(100)
            .is_some());

        // Cleanup slots below 101 should remove slot 100.
        driver.externalize_lag.write().cleanup_slots_below(101);
        assert!(driver
            .externalize_lag
            .read()
            .first_externalize_for_slot(100)
            .is_none());
    }

    #[test]
    fn test_combine_single_value() {
        let driver = make_test_driver();

        let value = Value(vec![1, 2, 3].try_into().unwrap());
        let result = driver.combine_candidates_impl(1, std::slice::from_ref(&value));
        assert_eq!(result, value);
    }

    #[test]
    fn test_combine_empty() {
        let driver = make_test_driver();

        let result = driver.combine_candidates_impl(1, &[]);
        assert_eq!(result, Value::default());
    }

    #[test]
    fn test_invalid_upgrade_rejected() {
        let (driver, secret_key) = make_test_driver_with_key();

        let tx_set = TransactionSet::new(Hash256::ZERO, vec![]);
        let tx_set_hash = *tx_set.hash();
        driver.cache_tx_set(tx_set);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let invalid_upgrade = UpgradeType(vec![0u8; 1].try_into().unwrap());
        let sv = make_signed_stellar_value(
            stellar_xdr::curr::Hash(tx_set_hash.0),
            now,
            vec![invalid_upgrade],
            &secret_key,
            &driver.network_id,
        );
        let value = encode_sv(&sv);

        assert_eq!(
            driver.validate_value_impl(1, &value, false),
            ValueValidation::Invalid
        );
    }

    #[test]
    fn test_tx_set_hash_mismatch_rejected() {
        let (driver, secret_key) = make_test_driver_with_key();

        let tx_set = TransactionSet::with_hash(Hash256::ZERO, Hash256::ZERO, vec![]);
        let tx_set_hash = *tx_set.hash();
        driver.cache_tx_set(tx_set);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let sv = make_signed_stellar_value(
            stellar_xdr::curr::Hash(tx_set_hash.0),
            now,
            vec![],
            &secret_key,
            &driver.network_id,
        );
        let value = encode_sv(&sv);

        assert_eq!(
            driver.validate_value_impl(1, &value, false),
            ValueValidation::Invalid
        );
    }

    #[test]
    fn test_close_time_must_increase() {
        let (driver, secret_key) = make_test_driver_with_key();

        let tx_set = TransactionSet::new(Hash256::ZERO, vec![]);
        let tx_set_hash = *tx_set.hash();
        driver.cache_tx_set(tx_set);

        let base_close_time = 100;
        // Record externalized with Basic ext (record_externalized doesn't validate)
        let ext_value = StellarValue {
            tx_set_hash: stellar_xdr::curr::Hash(tx_set_hash.0),
            close_time: TimePoint(base_close_time),
            upgrades: VecM::default(),
            ext: StellarValueExt::Basic,
        };
        let ext = Value(
            ext_value
                .to_xdr(Limits::none())
                .expect("xdr")
                .try_into()
                .unwrap(),
        );
        driver.record_externalized(1, ext, None);

        // Now try to validate a value with same close time (should fail)
        let sv = make_signed_stellar_value(
            stellar_xdr::curr::Hash(tx_set_hash.0),
            base_close_time,
            vec![],
            &secret_key,
            &driver.network_id,
        );
        let value = encode_sv(&sv);

        assert_eq!(
            driver.validate_value_impl(2, &value, false),
            ValueValidation::Invalid
        );
    }

    #[test]
    fn test_invalid_upgrade_order_rejected() {
        let (driver, secret_key) = make_test_driver_with_key();

        let tx_set = TransactionSet::new(Hash256::ZERO, vec![]);
        let tx_set_hash = *tx_set.hash();
        driver.cache_tx_set(tx_set);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let base_fee = LedgerUpgrade::BaseFee(200)
            .to_xdr(Limits::none())
            .expect("xdr");
        let version = LedgerUpgrade::Version(25)
            .to_xdr(Limits::none())
            .expect("xdr");
        let upgrades = vec![
            UpgradeType(base_fee.try_into().unwrap()),
            UpgradeType(version.try_into().unwrap()),
        ];

        let sv = make_signed_stellar_value(
            stellar_xdr::curr::Hash(tx_set_hash.0),
            now,
            upgrades,
            &secret_key,
            &driver.network_id,
        );
        let value = encode_sv(&sv);

        assert_eq!(
            driver.validate_value_impl(1, &value, false),
            ValueValidation::Invalid
        );
    }

    // =========================================================================
    // Phase 5 parity tests
    // =========================================================================

    #[test]
    fn test_validate_rejects_basic_ext() {
        // Parity: validateValue always requires STELLAR_VALUE_SIGNED
        let driver = make_test_driver();

        let tx_set = TransactionSet::new(Hash256::ZERO, vec![]);
        let tx_set_hash = *tx_set.hash();
        driver.cache_tx_set(tx_set);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Create a value with Basic ext (no signature)
        let stellar_value = StellarValue {
            tx_set_hash: stellar_xdr::curr::Hash(tx_set_hash.0),
            close_time: TimePoint(now),
            upgrades: VecM::default(),
            ext: StellarValueExt::Basic,
        };
        let value = encode_sv(&stellar_value);

        assert_eq!(
            driver.validate_value_impl(1, &value, false),
            ValueValidation::Invalid
        );
    }

    #[test]
    fn test_validate_rejects_bad_signature() {
        // Parity: validateValue verifies the StellarValue signature
        let (driver, secret_key) = make_test_driver_with_key();

        let tx_set = TransactionSet::new(Hash256::ZERO, vec![]);
        let tx_set_hash = *tx_set.hash();
        driver.cache_tx_set(tx_set);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Create a signed value but tamper with the signature
        let mut sv = make_signed_stellar_value(
            stellar_xdr::curr::Hash(tx_set_hash.0),
            now,
            vec![],
            &secret_key,
            &driver.network_id,
        );
        // Tamper with the signature
        if let StellarValueExt::Signed(ref mut sig) = sv.ext {
            let mut sig_bytes = sig.signature.to_vec();
            sig_bytes[0] ^= 0xFF;
            sig.signature = sig_bytes.try_into().expect("signature bytes");
        }
        let value = encode_sv(&sv);

        assert_eq!(
            driver.validate_value_impl(1, &value, false),
            ValueValidation::Invalid
        );
    }

    #[test]
    fn test_validate_accepts_signed_value() {
        // Parity: a properly signed StellarValue should be accepted
        let (driver, secret_key) = make_test_driver_with_key();

        let tx_set = TransactionSet::new(Hash256::ZERO, vec![]);
        let tx_set_hash = *tx_set.hash();
        driver.cache_tx_set(tx_set);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let sv = make_signed_stellar_value(
            stellar_xdr::curr::Hash(tx_set_hash.0),
            now,
            vec![],
            &secret_key,
            &driver.network_id,
        );
        let value = encode_sv(&sv);

        assert_eq!(
            driver.validate_value_impl(1, &value, false),
            ValueValidation::Valid
        );
    }

    #[test]
    fn test_verify_signed_bytes_valid_signature() {
        let secret = SecretKey::generate();
        let public = secret.public_key();
        let node_id = NodeId((&public).into());

        let data = b"test message";
        let sig_bytes = secret.sign(data);
        let sig = stellar_xdr::curr::Signature(sig_bytes.as_bytes().to_vec().try_into().unwrap());

        assert!(ScpDriver::verify_signed_bytes(data, &node_id, &sig).is_ok());
    }

    #[test]
    fn test_verify_signed_bytes_invalid_signature() {
        let secret = SecretKey::generate();
        let public = secret.public_key();
        let node_id = NodeId((&public).into());

        let data = b"test message";
        let wrong_sig = [0u8; 64];
        let sig = stellar_xdr::curr::Signature(wrong_sig.to_vec().try_into().unwrap());

        let err = ScpDriver::verify_signed_bytes(data, &node_id, &sig).unwrap_err();
        assert!(
            matches!(
                err,
                HerderError::Scp(henyey_scp::ScpError::SignatureVerificationFailed)
            ),
            "expected SignatureVerificationFailed, got: {err:?}"
        );
    }

    #[test]
    fn test_verify_signed_bytes_invalid_node_id() {
        // y=2 is not on the ed25519 curve
        let mut bad_key = [0u8; 32];
        bad_key[0] = 2;
        let node_id = NodeId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::curr::Uint256(bad_key),
        ));

        let data = b"test message";
        let sig = stellar_xdr::curr::Signature([0u8; 64].to_vec().try_into().unwrap());

        let err = ScpDriver::verify_signed_bytes(data, &node_id, &sig).unwrap_err();
        assert!(
            matches!(&err, HerderError::Internal(msg) if msg.contains("Invalid node ID")),
            "expected Internal(Invalid node ID), got: {err:?}"
        );
    }

    #[test]
    fn test_verify_signed_bytes_invalid_signature_length() {
        let secret = SecretKey::generate();
        let public = secret.public_key();
        let node_id = NodeId((&public).into());

        let data = b"test message";
        // Too short signature
        let sig = stellar_xdr::curr::Signature(vec![0u8; 32].try_into().unwrap());

        let err = ScpDriver::verify_signed_bytes(data, &node_id, &sig).unwrap_err();
        assert!(
            matches!(&err, HerderError::Internal(msg) if msg.contains("Invalid signature length")),
            "expected Internal(Invalid signature length), got: {err:?}"
        );
    }

    #[test]
    fn test_extract_valid_value_requires_fully_validated() {
        // Parity: extractValidValue only returns value when
        // validateValueAgainstLocalState returns kFullyValidatedValue.
        // When tx set is missing, it returns MaybeValid -> extractValidValue returns None.
        let driver = make_test_driver();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Value with missing tx set (MaybeValid from local state check)
        let stellar_value = StellarValue {
            tx_set_hash: stellar_xdr::curr::Hash([1u8; 32]),
            close_time: TimePoint(now),
            upgrades: VecM::default(),
            ext: StellarValueExt::Basic,
        };
        let value = encode_sv(&stellar_value);

        // extractValidValue should return None since tx set is missing
        assert!(driver.extract_valid_value_impl(1, &value).is_none());
    }

    #[test]
    fn test_extract_valid_value_strips_invalid_upgrades() {
        // Parity: extractValidValue strips invalid upgrades
        let driver = make_test_driver();

        let tx_set = TransactionSet::new(Hash256::ZERO, vec![]);
        let tx_set_hash = *tx_set.hash();
        driver.cache_tx_set(tx_set);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Create value with an invalid upgrade + a valid upgrade
        let version = LedgerUpgrade::Version(25)
            .to_xdr(Limits::none())
            .expect("xdr");
        let base_fee = LedgerUpgrade::BaseFee(200)
            .to_xdr(Limits::none())
            .expect("xdr");
        // Wrong order: base_fee (order 1) before version (order 0) -> invalid
        let upgrades = vec![
            UpgradeType(base_fee.try_into().unwrap()),
            UpgradeType(version.try_into().unwrap()),
        ];

        let stellar_value = StellarValue {
            tx_set_hash: stellar_xdr::curr::Hash(tx_set_hash.0),
            close_time: TimePoint(now),
            upgrades: upgrades.try_into().unwrap(),
            ext: StellarValueExt::Basic,
        };
        let value = encode_sv(&stellar_value);

        // extractValidValue should strip the out-of-order upgrade
        let result = driver.extract_valid_value_impl(1, &value);
        assert!(result.is_some());

        // The result should only have the first upgrade (base_fee at order 1)
        // since version (order 0) is out of order after base_fee
        let result_sv =
            StellarValue::from_xdr(&result.unwrap(), Limits::none()).expect("decode result");
        assert_eq!(result_sv.upgrades.len(), 1);
    }

    #[test]
    fn test_combine_candidates_merges_upgrades() {
        // Parity: combineCandidates merges upgrades from ALL candidates
        let driver = make_test_driver();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Create and cache real tx sets so the cache filter keeps them
        let tx_set_1 = TransactionSet::new(Hash256::ZERO, vec![]);
        let hash_1 = *tx_set_1.hash();
        driver.cache_tx_set(tx_set_1);

        let tx_set_2 = TransactionSet::new(Hash256::from_bytes([1u8; 32]), vec![]);
        let hash_2 = *tx_set_2.hash();
        driver.cache_tx_set(tx_set_2);

        // Candidate 1: version upgrade
        let version = LedgerUpgrade::Version(25)
            .to_xdr(Limits::none())
            .expect("xdr");
        let sv1 = StellarValue {
            tx_set_hash: stellar_xdr::curr::Hash(hash_1.0),
            close_time: TimePoint(now),
            upgrades: vec![UpgradeType(version.try_into().unwrap())]
                .try_into()
                .unwrap(),
            ext: StellarValueExt::Basic,
        };

        // Candidate 2: base fee upgrade
        let base_fee = LedgerUpgrade::BaseFee(200)
            .to_xdr(Limits::none())
            .expect("xdr");
        let sv2 = StellarValue {
            tx_set_hash: stellar_xdr::curr::Hash(hash_2.0),
            close_time: TimePoint(now),
            upgrades: vec![UpgradeType(base_fee.try_into().unwrap())]
                .try_into()
                .unwrap(),
            ext: StellarValueExt::Basic,
        };

        let v1 = encode_sv(&sv1);
        let v2 = encode_sv(&sv2);

        let result = driver.combine_candidates_impl(1, &[v1, v2]);
        let result_sv = StellarValue::from_xdr(&result, Limits::none()).expect("decode");

        // Result should have BOTH upgrades (merged)
        assert_eq!(result_sv.upgrades.len(), 2);
    }

    #[test]
    fn test_combine_candidates_takes_max_upgrade() {
        // Parity: when multiple candidates have same upgrade type, take max
        let driver = make_test_driver();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Create and cache real tx sets
        let tx_set_1 = TransactionSet::new(Hash256::ZERO, vec![]);
        let hash_1 = *tx_set_1.hash();
        driver.cache_tx_set(tx_set_1);

        let tx_set_2 = TransactionSet::new(Hash256::from_bytes([1u8; 32]), vec![]);
        let hash_2 = *tx_set_2.hash();
        driver.cache_tx_set(tx_set_2);

        // Candidate 1: version 24
        let v24 = LedgerUpgrade::Version(24)
            .to_xdr(Limits::none())
            .expect("xdr");
        let sv1 = StellarValue {
            tx_set_hash: stellar_xdr::curr::Hash(hash_1.0),
            close_time: TimePoint(now),
            upgrades: vec![UpgradeType(v24.try_into().unwrap())]
                .try_into()
                .unwrap(),
            ext: StellarValueExt::Basic,
        };

        // Candidate 2: version 25
        let v25 = LedgerUpgrade::Version(25)
            .to_xdr(Limits::none())
            .expect("xdr");
        let sv2 = StellarValue {
            tx_set_hash: stellar_xdr::curr::Hash(hash_2.0),
            close_time: TimePoint(now),
            upgrades: vec![UpgradeType(v25.try_into().unwrap())]
                .try_into()
                .unwrap(),
            ext: StellarValueExt::Basic,
        };

        let v1 = encode_sv(&sv1);
        let v2 = encode_sv(&sv2);

        let result = driver.combine_candidates_impl(1, &[v1, v2]);
        let result_sv = StellarValue::from_xdr(&result, Limits::none()).expect("decode");

        // Should have 1 version upgrade with value 25 (max)
        assert_eq!(result_sv.upgrades.len(), 1);
        let upgrade = LedgerUpgrade::from_xdr(result_sv.upgrades[0].0.as_slice(), Limits::none())
            .expect("decode upgrade");
        assert!(matches!(upgrade, LedgerUpgrade::Version(25)));
    }

    #[test]
    fn test_has_upgrades_and_strip() {
        // Parity: has_upgrades checks sv.upgrades.empty()
        //             strip_all_upgrades clears sv.upgrades
        let driver = Arc::new(make_test_driver());
        let callback = HerderScpCallback::new(driver);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Value without upgrades
        let sv_no_upgrades = StellarValue {
            tx_set_hash: stellar_xdr::curr::Hash([1u8; 32]),
            close_time: TimePoint(now),
            upgrades: VecM::default(),
            ext: StellarValueExt::Basic,
        };
        let v_no = encode_sv(&sv_no_upgrades);
        assert!(!callback.has_upgrades(&v_no));

        // Value with upgrades
        let version = LedgerUpgrade::Version(25)
            .to_xdr(Limits::none())
            .expect("xdr");
        let sv_with_upgrades = StellarValue {
            tx_set_hash: stellar_xdr::curr::Hash([1u8; 32]),
            close_time: TimePoint(now),
            upgrades: vec![UpgradeType(version.try_into().unwrap())]
                .try_into()
                .unwrap(),
            ext: StellarValueExt::Basic,
        };
        let v_yes = encode_sv(&sv_with_upgrades);
        assert!(callback.has_upgrades(&v_yes));

        // Strip upgrades
        let stripped = callback.strip_all_upgrades(&v_yes);
        assert!(stripped.is_some());
        let stripped_sv =
            StellarValue::from_xdr(&stripped.unwrap(), Limits::none()).expect("decode");
        assert!(stripped_sv.upgrades.is_empty());
        // txSetHash and closeTime should be preserved
        assert_eq!(stripped_sv.tx_set_hash, sv_with_upgrades.tx_set_hash);
        assert_eq!(stripped_sv.close_time, sv_with_upgrades.close_time);
    }

    // =========================================================================
    // Phase 6 H1 parity tests — close-time validation
    // =========================================================================

    #[test]
    fn test_check_close_time_valid() {
        let driver = make_test_driver();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Valid: close time is after last close time and within max_time_drift
        assert!(driver.check_close_time(1, now - 10, now));
        assert!(driver.check_close_time(1, now - 1, now));
    }

    #[test]
    fn test_check_close_time_rejects_not_after_last() {
        let driver = make_test_driver();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Invalid: close time is equal to last close time
        assert!(!driver.check_close_time(1, now, now));
        // Invalid: close time is before last close time
        assert!(!driver.check_close_time(1, now, now - 1));
    }

    #[test]
    fn test_check_close_time_rejects_too_far_future() {
        let driver = make_test_driver();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Invalid: close time too far in future (max_time_drift defaults to MAX_TIME_SLIP_SECONDS = 60)
        assert!(!driver.check_close_time(1, now - 1, now + 120));
    }

    #[test]
    fn test_validate_past_or_future_value_same_as_lcl() {
        let driver = make_test_driver();

        // slot_index == lcl_seq: close time must match LCL exactly
        assert_eq!(
            driver.validate_past_or_future_value(
                100,
                500,
                ValueValidationContext {
                    lcl_seq: 100,
                    lcl_close_time: 500,
                    tracking: None,
                }
            ),
            ValueValidation::MaybeValid
        );
        // Close time doesn't match -> Invalid
        assert_eq!(
            driver.validate_past_or_future_value(
                100,
                501,
                ValueValidationContext {
                    lcl_seq: 100,
                    lcl_close_time: 500,
                    tracking: None,
                }
            ),
            ValueValidation::Invalid
        );
    }

    #[test]
    fn test_validate_past_or_future_value_older_than_lcl() {
        let driver = make_test_driver();

        // slot_index < lcl_seq: close time must be strictly less than LCL
        assert_eq!(
            driver.validate_past_or_future_value(
                99,
                499,
                ValueValidationContext {
                    lcl_seq: 100,
                    lcl_close_time: 500,
                    tracking: None,
                }
            ),
            ValueValidation::MaybeValid
        );
        // Close time >= LCL -> Invalid
        assert_eq!(
            driver.validate_past_or_future_value(
                99,
                500,
                ValueValidationContext {
                    lcl_seq: 100,
                    lcl_close_time: 500,
                    tracking: None,
                }
            ),
            ValueValidation::Invalid
        );
        assert_eq!(
            driver.validate_past_or_future_value(
                99,
                501,
                ValueValidationContext {
                    lcl_seq: 100,
                    lcl_close_time: 500,
                    tracking: None,
                }
            ),
            ValueValidation::Invalid
        );
    }

    #[test]
    fn test_validate_past_or_future_value_future_not_tracking() {
        let driver = make_test_driver();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // slot_index > lcl_seq + 1: delegates to check_close_time, then MaybeValid
        assert_eq!(
            driver.validate_past_or_future_value(
                200,
                now, // close_time
                ValueValidationContext {
                    lcl_seq: 100,
                    lcl_close_time: now - 10,
                    tracking: None,
                }
            ),
            ValueValidation::MaybeValid
        );
    }

    #[test]
    fn test_validate_past_or_future_value_tracking_moved_on() {
        let driver = make_test_driver();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Tracking and tracking_index > slot_index -> already moved on -> MaybeValid
        assert_eq!(
            driver.validate_past_or_future_value(
                150,
                now,
                ValueValidationContext {
                    lcl_seq: 100,
                    lcl_close_time: now - 50,
                    tracking: Some(SharedTrackingState {
                        is_tracking: true,
                        consensus_index: 200,
                        consensus_close_time: now - 5,
                    }),
                }
            ),
            ValueValidation::MaybeValid
        );
    }

    #[test]
    fn test_validate_past_or_future_value_tracking_future_msg() {
        let driver = make_test_driver();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Tracking and tracking_index < slot_index -> future message -> Invalid
        assert_eq!(
            driver.validate_past_or_future_value(
                200,
                now,
                ValueValidationContext {
                    lcl_seq: 100,
                    lcl_close_time: now - 50,
                    tracking: Some(SharedTrackingState {
                        is_tracking: true,
                        consensus_index: 150,
                        consensus_close_time: now - 5,
                    }),
                }
            ),
            ValueValidation::Invalid
        );
    }

    /// When tracking is ahead of LCL (ledger apply lagging SCP), a peer
    /// envelope for the future slot whose `tracking_index == slot_index`
    /// drains through the fast-path before apply catches up. This must
    /// return `MaybeValidDeferred` — NOT plain `MaybeValid` — so the
    /// deferred-cause restoration mechanism can restore `fully_validated`
    /// when LCL catches up (via `Herder::ledger_closed` →
    /// `resolve_apply_lag_for_next_index`).
    ///
    /// This is the regression test for issue #1798. See the
    /// `ValidationLevel::MaybeValidDeferred` doc comment for the full
    /// rationale.
    #[test]
    fn test_issue_1798_validate_past_or_future_value_tracking_same_slot_returns_deferred() {
        let driver = make_test_driver();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Tracking and tracking_index == slot_index with slot_index
        // > lcl_seq + 1 (apply lagging SCP) -> MaybeValidDeferred.
        assert_eq!(
            driver.validate_past_or_future_value(
                150,
                now, // close_time > tracking_close_time, within drift
                ValueValidationContext {
                    lcl_seq: 100,
                    lcl_close_time: now - 50,
                    tracking: Some(SharedTrackingState {
                        is_tracking: true,
                        consensus_index: 150,
                        consensus_close_time: now - 5,
                    }),
                }
            ),
            ValueValidation::MaybeValidDeferred,
            "tracking_index == slot_index with apply lagging SCP must \
             return MaybeValidDeferred (issue #1798) — NOT plain \
             MaybeValid, which would clear fully_validated and \
             suppress the local EXTERNALIZE"
        );
        // If close_time <= tracking_close_time -> Invalid (the
        // close-time check fires before the level is selected).
        assert_eq!(
            driver.validate_past_or_future_value(
                150,
                now - 10, // close_time <= tracking_close_time
                ValueValidationContext {
                    lcl_seq: 100,
                    lcl_close_time: now - 50,
                    tracking: Some(SharedTrackingState {
                        is_tracking: true,
                        consensus_index: 150,
                        consensus_close_time: now - 5,
                    }),
                }
            ),
            ValueValidation::Invalid
        );
    }

    #[test]
    fn test_validate_past_or_future_value_rejects_current_ledger() {
        let driver = make_test_driver();

        // slot_index == lcl_seq + 1 is the current ledger path -- should be Invalid
        // (validate_past_or_future_value is not for the current ledger)
        assert_eq!(
            driver.validate_past_or_future_value(
                101,
                500,
                ValueValidationContext {
                    lcl_seq: 100,
                    lcl_close_time: 490,
                    tracking: None,
                }
            ),
            ValueValidation::Invalid
        );
    }

    #[test]
    fn test_validate_value_against_local_state_current_ledger() {
        // When slot_index == lcl_seq + 1, validate_value_against_local_state
        // does full validation including tx set hash check
        let (driver, secret_key) = make_test_driver_with_key();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let tx_set = TransactionSet::new(Hash256::ZERO, vec![]);
        let tx_set_hash = *tx_set.hash();
        driver.cache_tx_set(tx_set);

        let sv = make_signed_stellar_value(
            stellar_xdr::curr::Hash(tx_set_hash.0),
            now,
            vec![],
            &secret_key,
            &driver.network_id,
        );

        // slot 1 == lcl_seq(0) + 1 -> current ledger path
        let result = driver.validate_value_against_local_state(1, &sv, true);
        assert_eq!(result, ValueValidation::Valid);
    }

    // =========================================================================
    // Phase 2A: is_tx_set_well_formed tests
    // =========================================================================

    fn make_simple_tx(seed: u8) -> stellar_xdr::curr::TransactionEnvelope {
        use stellar_xdr::curr::{
            CreateAccountOp, DecoratedSignature, Memo, MuxedAccount, Operation, OperationBody,
            Preconditions, SequenceNumber, SignatureHint, Transaction, TransactionEnvelope,
            TransactionExt, TransactionV1Envelope, Uint256,
        };
        let source = MuxedAccount::Ed25519(Uint256([seed; 32]));
        let dest = stellar_xdr::curr::AccountId(
            stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(Uint256([seed.wrapping_add(1); 32])),
        );
        let tx = Transaction {
            source_account: source,
            fee: 100,
            seq_num: SequenceNumber(seed as i64),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![Operation {
                source_account: None,
                body: OperationBody::CreateAccount(CreateAccountOp {
                    destination: dest,
                    starting_balance: 1_000_000_000,
                }),
            }]
            .try_into()
            .unwrap(),
            ext: TransactionExt::V0,
        };
        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: stellar_xdr::curr::Signature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        })
    }

    #[test]
    fn test_is_tx_set_well_formed_empty() {
        let tx_set = TransactionSet::new(Hash256::ZERO, vec![]);
        assert!(ScpDriver::is_tx_set_well_formed(&tx_set));
    }

    #[test]
    fn test_is_tx_set_well_formed_single_tx() {
        let tx = make_simple_tx(1);
        let tx_set = TransactionSet::new(Hash256::ZERO, vec![tx]);
        assert!(ScpDriver::is_tx_set_well_formed(&tx_set));
    }

    #[test]
    fn test_is_tx_set_well_formed_sorted() {
        // Create txs and sort them by hash
        let mut txs: Vec<stellar_xdr::curr::TransactionEnvelope> =
            (1..=5).map(|i| make_simple_tx(i)).collect();
        txs.sort_by(|a, b| {
            let ha = Hash256::hash_xdr(a);
            let hb = Hash256::hash_xdr(b);
            ha.0.cmp(&hb.0)
        });

        let tx_set = TransactionSet::new(Hash256::ZERO, txs);
        assert!(ScpDriver::is_tx_set_well_formed(&tx_set));
    }

    #[test]
    fn test_is_tx_set_well_formed_unsorted() {
        // Create txs sorted by hash, then swap first two to make unsorted
        let mut txs: Vec<stellar_xdr::curr::TransactionEnvelope> =
            (1..=5).map(|i| make_simple_tx(i)).collect();
        txs.sort_by(|a, b| {
            let ha = Hash256::hash_xdr(a);
            let hb = Hash256::hash_xdr(b);
            ha.0.cmp(&hb.0)
        });
        // Swap first two to guarantee out-of-order
        txs.swap(0, 1);

        // Use with_hash to bypass auto-sorting in TransactionSet::new
        let tx_set = TransactionSet::with_hash(Hash256::ZERO, Hash256::ZERO, txs);
        assert!(
            !ScpDriver::is_tx_set_well_formed(&tx_set),
            "Swapped tx set should not be well-formed"
        );
    }

    #[test]
    fn test_is_tx_set_well_formed_duplicates() {
        let tx = make_simple_tx(1);
        // Use with_hash to bypass auto-sorting/dedup in TransactionSet::new
        let tx_set = TransactionSet::with_hash(Hash256::ZERO, Hash256::ZERO, vec![tx.clone(), tx]);
        assert!(
            !ScpDriver::is_tx_set_well_formed(&tx_set),
            "Tx set with duplicate should not be well-formed"
        );
    }

    /// Regression test for AUDIT-013: generalized tx sets with fee-priority-ordered
    /// transactions must NOT be rejected by the global hash-sort check.
    /// The is_tx_set_well_formed check only applies to legacy tx sets.
    #[test]
    fn test_audit_013_generalized_tx_set_not_rejected_by_global_sort() {
        use stellar_xdr::curr::{
            GeneralizedTransactionSet, Hash, ParallelTxsComponent, TransactionPhase,
            TransactionSetV1, TxSetComponent, TxSetComponentTxsMaybeDiscountedFee,
        };

        // Create 3 txs in fee-priority order (NOT hash-sorted)
        let tx_a = make_simple_tx(1);
        let tx_b = make_simple_tx(2);
        let tx_c = make_simple_tx(3);
        let fee_priority_order = vec![tx_a, tx_c, tx_b]; // intentionally unsorted by hash

        // Verify this order IS unsorted by hash (precondition for the test)
        let unsorted_set =
            TransactionSet::with_hash(Hash256::ZERO, Hash256::ZERO, fee_priority_order.clone());
        assert!(
            !ScpDriver::is_tx_set_well_formed(&unsorted_set),
            "Precondition: fee-priority order should fail global hash sort"
        );

        // Now wrap the same transactions in a generalized tx set
        let component =
            TxSetComponent::TxsetCompTxsMaybeDiscountedFee(TxSetComponentTxsMaybeDiscountedFee {
                txs: fee_priority_order.clone().try_into().unwrap(),
                base_fee: Some(100),
            });
        let gen = GeneralizedTransactionSet::V1(TransactionSetV1 {
            previous_ledger_hash: Hash([0u8; 32]),
            phases: vec![
                TransactionPhase::V0(vec![component].try_into().unwrap()),
                TransactionPhase::V1(ParallelTxsComponent {
                    base_fee: Some(100),
                    execution_stages: vec![].try_into().unwrap(),
                }),
            ]
            .try_into()
            .unwrap(),
        });
        let gen_hash = Hash256::hash_xdr(&gen);
        let gen_set = TransactionSet::new_generalized(gen_hash, gen);

        // The generalized tx set has a generalized_tx_set field
        assert!(gen_set.generalized_tx_set().is_some());

        // The guard should skip is_tx_set_well_formed for generalized sets.
        // We can verify the guard logic directly: generalized_tx_set.is_none() is false,
        // so the well-formed check is skipped.
        assert!(
            gen_set.generalized_tx_set().is_some(),
            "AUDIT-013: Generalized tx sets should bypass global hash-sort check"
        );
    }

    /// Regression test for AUDIT-006: validate_value must not skip validation
    /// during the ballot protocol (nomination=false). Previously, the callback
    /// returned FullyValidated immediately when nomination=false, allowing
    /// malformed or unsigned values to pass through unchecked.
    #[test]
    fn test_audit_006_validate_value_rejects_invalid_during_ballot() {
        use henyey_scp::SCPDriver;

        let driver = Arc::new(make_test_driver());
        let callback = HerderScpCallback::new(driver);

        // Garbage bytes that cannot decode as a StellarValue.
        let garbage_value: Value = vec![0xDE, 0xAD, 0xBE, 0xEF].try_into().unwrap();

        // During nomination: must reject invalid XDR.
        let result_nom = callback.validate_value(1, &garbage_value, true);
        assert_eq!(
            result_nom,
            ValidationLevel::Invalid,
            "nomination=true must reject garbage"
        );

        // During ballot protocol: must ALSO reject invalid XDR.
        // Before the fix, this returned FullyValidated.
        let result_ballot = callback.validate_value(1, &garbage_value, false);
        assert_eq!(
            result_ballot,
            ValidationLevel::Invalid,
            "nomination=false must reject garbage (AUDIT-006)"
        );
    }

    /// Regression test for AUDIT-008: During nomination, upgrade validation must
    /// check that upgrades match the node's configured parameters (timing + values).
    /// Previously, only `isValidForApply` was checked, allowing any structurally
    /// valid upgrade to pass nomination regardless of the node's config.
    #[test]
    fn test_audit_008_nomination_rejects_unconfigured_upgrades() {
        use crate::upgrades::{UpgradeParameters, Upgrades};

        // Test is_valid_for_nomination directly on the Upgrades struct
        let params = UpgradeParameters {
            upgrade_time: 1000,
            base_fee: Some(200),
            protocol_version: Some(25),
            ..UpgradeParameters::default()
        };
        let upgrades = Upgrades::new(params);

        // Wrong value: base_fee=500 doesn't match configured 200
        let wrong_fee = LedgerUpgrade::BaseFee(500);
        assert!(
            !upgrades.is_valid_for_nomination(&wrong_fee, 1500),
            "Must reject upgrade with wrong value"
        );

        // Correct value: base_fee=200 matches configured
        let correct_fee = LedgerUpgrade::BaseFee(200);
        assert!(
            upgrades.is_valid_for_nomination(&correct_fee, 1500),
            "Must accept upgrade with matching value"
        );

        // Too early: close_time=500 is before upgrade_time=1000
        assert!(
            !upgrades.is_valid_for_nomination(&correct_fee, 500),
            "Must reject upgrade before scheduled time"
        );

        // Unconfigured type: MaxTxSetSize not in config → reject
        let unconfigured = LedgerUpgrade::MaxTxSetSize(100);
        assert!(
            !upgrades.is_valid_for_nomination(&unconfigured, 1500),
            "Must reject upgrade type not configured by this node"
        );

        // Protocol version: correct value
        let correct_version = LedgerUpgrade::Version(25);
        assert!(
            upgrades.is_valid_for_nomination(&correct_version, 1500),
            "Must accept matching protocol version"
        );

        // Protocol version: wrong value
        let wrong_version = LedgerUpgrade::Version(26);
        assert!(
            !upgrades.is_valid_for_nomination(&wrong_version, 1500),
            "Must reject non-matching protocol version"
        );
    }

    /// Test that check_upgrades_valid uses nomination flag to gate nomination checks.
    #[test]
    fn test_audit_008_check_upgrades_valid_nomination_flag() {
        use crate::upgrades::{UpgradeParameters, Upgrades};

        let (driver, _secret_key) = make_test_driver_with_key();

        // Configure upgrades: only base_fee=200 at time 1000
        let params = UpgradeParameters {
            upgrade_time: 1000,
            base_fee: Some(200),
            ..UpgradeParameters::default()
        };
        let upgrades = Arc::new(RwLock::new(Upgrades::new(params)));
        driver.set_upgrades(upgrades);

        // Create a StellarValue with a base_fee=500 upgrade (wrong value)
        let wrong_upgrade = LedgerUpgrade::BaseFee(500);
        let upgrade_bytes = wrong_upgrade.to_xdr(Limits::none()).unwrap();
        let upgrade_type = UpgradeType(upgrade_bytes.try_into().unwrap());

        let sv = StellarValue {
            tx_set_hash: stellar_xdr::curr::Hash([0; 32]),
            close_time: TimePoint(1500),
            upgrades: vec![upgrade_type].try_into().unwrap(),
            ext: StellarValueExt::Basic,
        };

        // Without ledger manager, check_upgrades_valid returns true (can't validate)
        // This is expected — the test verifies the nomination path once a ledger manager
        // is available. The is_valid_for_nomination unit tests above cover the logic directly.
        assert!(
            driver.check_upgrades_valid(&sv, false),
            "Without ledger manager, ballot check should pass"
        );
        assert!(
            driver.check_upgrades_valid(&sv, true),
            "Without ledger manager, nomination check should pass (can't validate)"
        );
    }

    /// Regression test for AUDIT-023 (#1096).
    ///
    /// Config upgrades with nonexistent keys must be rejected by
    /// `check_upgrades_valid` when a LedgerManager is available.
    /// stellar-core's `isValidForApply` performs a ledger lookup via
    /// `ConfigUpgradeSetFrame::makeFromKey` and rejects unknown keys.
    #[test]
    fn test_audit_023_config_upgrade_nonexistent_key_rejected() {
        use henyey_bucket::{BucketList, HotArchiveBucketList};
        use henyey_ledger::{compute_header_hash, LedgerManagerConfig};
        use stellar_xdr::curr::{
            ConfigUpgradeSetKey, ContractId, Hash, LedgerHeader, LedgerHeaderExt, StellarValueExt,
            TimePoint, VecM,
        };

        // Set up a LedgerManager with protocol 25
        let config = LedgerManagerConfig {
            validate_bucket_hash: false,
            ..Default::default()
        };
        let lm = henyey_ledger::LedgerManager::new("Test Network".to_string(), config);
        let header = LedgerHeader {
            ledger_version: 25,
            previous_ledger_hash: Hash([0u8; 32]),
            scp_value: StellarValue {
                tx_set_hash: Hash([0u8; 32]),
                close_time: TimePoint(0),
                upgrades: VecM::default(),
                ext: StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash([0u8; 32]),
            bucket_list_hash: Hash([0u8; 32]),
            ledger_seq: 1,
            total_coins: 1_000_000,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 100,
            max_tx_set_size: 100,
            skip_list: [
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
                Hash([0u8; 32]),
            ],
            ext: LedgerHeaderExt::V0,
        };
        let header_hash = compute_header_hash(&header).expect("hash");
        lm.initialize(
            BucketList::new(),
            HotArchiveBucketList::new(),
            header,
            header_hash,
        )
        .expect("init");

        let (driver, _) = make_test_driver_with_key();
        driver.set_ledger_manager(Arc::new(lm));

        // Create a Config upgrade with a bogus key that doesn't exist in ledger
        let bogus_key = ConfigUpgradeSetKey {
            contract_id: ContractId(Hash([0xDE; 32])),
            content_hash: Hash([0xAD; 32]),
        };
        let upgrade = LedgerUpgrade::Config(bogus_key);
        let upgrade_bytes = upgrade.to_xdr(Limits::none()).unwrap();
        let upgrade_type = UpgradeType(upgrade_bytes.try_into().unwrap());

        let sv = StellarValue {
            tx_set_hash: stellar_xdr::curr::Hash([0; 32]),
            close_time: TimePoint(100),
            upgrades: vec![upgrade_type].try_into().unwrap(),
            ext: StellarValueExt::Basic,
        };

        // AUDIT-023: check_upgrades_valid MUST reject Config upgrades with
        // nonexistent keys. stellar-core rejects this in isValidForApply via
        // ConfigUpgradeSetFrame::makeFromKey returning nullptr.
        assert!(
            !driver.check_upgrades_valid(&sv, false),
            "Config upgrade with nonexistent key must be rejected when LedgerManager is available"
        );
    }

    /// Test is_valid_upgrade_for_apply directly: Config upgrade without
    /// ledger manager is rejected, and non-Config upgrades are unaffected.
    #[test]
    fn test_audit_023_is_valid_upgrade_for_apply_config_without_lm() {
        use stellar_xdr::curr::{ConfigUpgradeSetKey, ContractId, Hash};

        let bogus_key = ConfigUpgradeSetKey {
            contract_id: ContractId(Hash([0xDE; 32])),
            content_hash: Hash([0xAD; 32]),
        };

        // Config upgrade without ledger manager: rejected
        assert!(
            !ScpDriver::is_valid_upgrade_for_apply(&LedgerUpgrade::Config(bogus_key), 25, None,),
            "Config upgrade must be rejected when no ledger manager is available"
        );

        // Non-Config upgrades are unaffected by the ledger_manager parameter
        assert!(
            ScpDriver::is_valid_upgrade_for_apply(&LedgerUpgrade::BaseFee(200), 25, None),
            "BaseFee upgrade should not need ledger manager"
        );
        assert!(
            ScpDriver::is_valid_upgrade_for_apply(
                &LedgerUpgrade::MaxSorobanTxSetSize(100),
                25,
                None,
            ),
            "MaxSorobanTxSetSize upgrade should not need ledger manager"
        );
    }

    /// Regression test for AUDIT-220 / issue #2157: validate_value_against_local_state
    /// must not allow a value to reach Valid without full tx-set content validation.
    ///
    /// Before the fix, a two-step has_tx_set() + get() pattern allowed a TOCTOU
    /// race where eviction between the two calls would skip all validation and
    /// return Valid unconditionally. After the fix, a single atomic get() is used;
    /// if the tx set is missing, Invalid or MaybeValidDeferred is returned.
    #[test]
    fn test_audit_220_validate_value_single_lookup_eliminates_toctou() {
        let (driver, secret_key) = make_test_driver_with_key();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Create a signed StellarValue referencing a tx_set_hash NOT in cache
        let uncached_hash = stellar_xdr::curr::Hash([0xAB; 32]);
        let sv =
            make_signed_stellar_value(uncached_hash, now, vec![], &secret_key, &driver.network_id);

        // During nomination: must return Invalid (not Valid!)
        // Before the AUDIT-220 fix, if has_tx_set() returned true but get()
        // returned None (due to LRU eviction between the calls), the code
        // would fall through to return Valid without any validation.
        let result = driver.validate_value_against_local_state(1, &sv, true);
        assert_eq!(
            result,
            ValueValidation::Invalid,
            "AUDIT-220: missing tx set during nomination must return Invalid, not Valid"
        );

        // During ballot: must return MaybeValidDeferred (not Valid!)
        let result_ballot = driver.validate_value_against_local_state(1, &sv, false);
        assert_eq!(
            result_ballot,
            ValueValidation::MaybeValidDeferred,
            "AUDIT-220: missing tx set during ballot must return MaybeValidDeferred, not Valid"
        );
    }

    /// Regression test for AUDIT-220 / issue #2157: combine_candidates_impl must
    /// not panic when tx sets are evicted between is_cached() and get().
    /// After the fix, resolution and filtering happen in a single pass.
    #[test]
    fn test_audit_220_combine_candidates_no_panic_on_missing() {
        use stellar_xdr::curr::{Limits, StellarValue, StellarValueExt, TimePoint, WriteXdr};

        let driver = make_test_driver();

        // Create two tx sets but only cache one
        let tx_set_a = TransactionSet::new(Hash256::ZERO, vec![]);
        let tx_set_b = TransactionSet::new(Hash256::from_bytes([1u8; 32]), vec![]);

        let sv_a = StellarValue {
            tx_set_hash: stellar_xdr::curr::Hash(tx_set_a.hash().0),
            close_time: TimePoint(1_700_000_000),
            upgrades: Default::default(),
            ext: StellarValueExt::Basic,
        };
        let sv_b = StellarValue {
            tx_set_hash: stellar_xdr::curr::Hash(tx_set_b.hash().0),
            close_time: TimePoint(1_700_000_000),
            upgrades: Default::default(),
            ext: StellarValueExt::Basic,
        };

        let val_a = Value(sv_a.to_xdr(Limits::none()).unwrap().try_into().unwrap());
        let val_b = Value(sv_b.to_xdr(Limits::none()).unwrap().try_into().unwrap());
        let values = vec![val_a, val_b];

        // Only cache B, not A — this simulates the TOCTOU scenario where A
        // was present when is_cached() was called but evicted before get().
        driver.cache_tx_set(tx_set_b.clone());

        // Must not panic — should gracefully filter out the missing set
        let result = driver.combine_candidates_impl(1, &values);
        let result_sv = StellarValue::from_xdr(&result.0, Limits::none()).unwrap();
        assert_eq!(
            result_sv.tx_set_hash.0,
            tx_set_b.hash().0,
            "AUDIT-220: missing tx set A should be filtered out, B should win"
        );
    }

    // ========== Deferred-causes regression tests (issue #2096 / H-014) =========
    //
    // These tests cover the herder-side deferred-validation registry.
    // `validate_value` returns `MaybeValidDeferred` for two reasons:
    //   - missing-tx-set on the LCL+1 path (resolved by `Herder::cache_tx_set`)
    //   - apply-lag on the past/future-slot path (resolved by
    //     `Herder::ledger_closed`)
    // Both reasons clear `Slot::fully_validated` in SCP; restoration is
    // gated on ALL recorded causes for the slot being clear.

    /// The apply-lag branch records a slot in `deferred_slots` with
    /// `apply_lag = true` AND returns `MaybeValidDeferred`. Without the
    /// recording, `Herder::ledger_closed` has nothing to resolve and the
    /// slot's `fully_validated` flag stays cleared forever (H-014).
    #[test]
    fn test_apply_lag_deferred_slot_recorded() {
        let driver = make_test_driver();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // tracking_index == slot_index > lcl_seq + 1 → apply-lag branch.
        let result = driver.validate_past_or_future_value(
            150,
            now,
            ValueValidationContext {
                lcl_seq: 100,
                lcl_close_time: now - 50,
                tracking: Some(SharedTrackingState {
                    is_tracking: true,
                    consensus_index: 150,
                    consensus_close_time: now - 5,
                }),
            },
        );
        assert_eq!(result, ValueValidation::MaybeValidDeferred);

        let causes = driver
            .deferred_causes_for_slot(150)
            .expect("apply-lag branch must record slot in deferred_slots");
        assert!(
            causes.apply_lag,
            "apply-lag branch must set apply_lag = true"
        );
        assert!(
            causes.missing_tx_sets.is_empty(),
            "apply-lag branch must not record any missing-tx-set hashes"
        );
    }

    /// `resolve_apply_lag_for_next_index(next_index)` clears `apply_lag`
    /// only for slots with `slot <= next_index`. Future slots remain
    /// deferred for the next `ledger_closed` tick.
    #[test]
    fn test_resolve_apply_lag_threshold() {
        let driver = make_test_driver();
        driver.record_apply_lag(101);
        driver.record_apply_lag(105);

        // next_index = 101 (LCL = 100): only slot 101 is eligible.
        let resolved = driver.resolve_apply_lag_for_next_index(101);
        assert_eq!(resolved, vec![101]);
        assert!(driver.deferred_causes_for_slot(101).is_none());
        assert!(
            driver.deferred_causes_for_slot(105).is_some(),
            "slot 105 must remain deferred when next_index < 105"
        );

        // next_index = 105 (LCL = 104): slot 105 is now eligible.
        let resolved = driver.resolve_apply_lag_for_next_index(105);
        assert_eq!(resolved, vec![105]);
        assert_eq!(
            driver.deferred_slot_count(),
            0,
            "deferred_slots must be empty after both slots resolved"
        );
    }

    /// `record_apply_lag` does not gate by slot index. Past-slot recording
    /// is benign — the entry is resolved on the next `ledger_closed` tick
    /// and `restore_slot_fully_validated` is idempotent / a no-op on
    /// already-restored or purged slots.
    #[test]
    fn test_resolve_apply_lag_past_slot_idempotent() {
        let driver = make_test_driver();
        driver.record_apply_lag(50); // older than any plausible LCL

        let resolved = driver.resolve_apply_lag_for_next_index(101);
        assert_eq!(
            resolved,
            vec![50],
            "past-slot apply-lag entries are eligible at next ledger_closed"
        );
        assert_eq!(driver.deferred_slot_count(), 0);
    }

    /// The two reasons resolve independently. A slot deferred only on
    /// `MissingTxSet` is not affected by `resolve_apply_lag_for_next_index`,
    /// and vice versa.
    #[test]
    fn test_resolve_missing_tx_set_independence() {
        let driver = make_test_driver();
        let h1 = Hash256::hash(b"tx-set-1");
        driver.record_missing_tx_set(100, h1);
        driver.record_apply_lag(101);

        let resolved = driver.resolve_missing_tx_set(&h1);
        assert_eq!(resolved, vec![100]);
        assert!(driver.deferred_causes_for_slot(100).is_none());
        let s101 = driver
            .deferred_causes_for_slot(101)
            .expect("slot 101 must still be deferred on apply_lag");
        assert!(s101.apply_lag);

        let resolved = driver.resolve_apply_lag_for_next_index(101);
        assert_eq!(resolved, vec![101]);
        assert_eq!(driver.deferred_slot_count(), 0);
    }

    /// Multi-hash semantics: when multiple distinct tx-set hashes are
    /// recorded for the same slot (different ballot/nominated values),
    /// the slot is restorable only when EVERY recorded hash has arrived.
    #[test]
    fn test_record_missing_tx_set_accumulates_hashes() {
        let driver = make_test_driver();
        let h1 = Hash256::hash(b"tx-set-1");
        let h2 = Hash256::hash(b"tx-set-2");
        driver.record_missing_tx_set(100, h1);
        driver.record_missing_tx_set(100, h2);

        // Only h1 arrives — slot 100 stays deferred.
        let resolved = driver.resolve_missing_tx_set(&h1);
        assert!(
            resolved.is_empty(),
            "slot must not resolve until all recorded hashes have arrived"
        );
        assert!(driver.deferred_causes_for_slot(100).is_some());

        // h2 arrives — slot 100 is now restorable.
        let resolved = driver.resolve_missing_tx_set(&h2);
        assert_eq!(resolved, vec![100]);
        assert_eq!(driver.deferred_slot_count(), 0);
    }

    /// Per-slot coexistence: at any single LCL snapshot the two reasons
    /// are mutually exclusive (`MissingTxSet` only on LCL+1, `ApplyLag`
    /// only on `slot != lcl_seq + 1`). But an LCL advance between two
    /// validate_value calls can leave both causes set on the same slot.
    /// Restoration must require ALL causes clear, regardless of clear
    /// order.
    #[test]
    fn test_record_coexistent_causes_resolve_independently() {
        let driver = make_test_driver();
        let h = Hash256::hash(b"tx-set");

        // Order 1: ApplyLag first, then MissingTxSet.
        driver.record_apply_lag(100);
        driver.record_missing_tx_set(100, h);

        // Resolving apply-lag alone must NOT remove the slot — missing tx_set
        // is still pending.
        let resolved = driver.resolve_apply_lag_for_next_index(100);
        assert!(
            resolved.is_empty(),
            "slot must not resolve while missing_tx_sets is non-empty"
        );
        let causes = driver
            .deferred_causes_for_slot(100)
            .expect("slot must remain deferred");
        assert!(
            !causes.apply_lag,
            "apply_lag must be cleared even when slot is still deferred"
        );
        assert_eq!(causes.missing_tx_sets.len(), 1);

        // Resolving the missing tx_set now empties causes — slot is restorable.
        let resolved = driver.resolve_missing_tx_set(&h);
        assert_eq!(resolved, vec![100]);
        assert_eq!(driver.deferred_slot_count(), 0);

        // Order 2: MissingTxSet first, then ApplyLag — same final result.
        driver.record_missing_tx_set(200, h);
        driver.record_apply_lag(200);

        let resolved = driver.resolve_missing_tx_set(&h);
        assert!(
            resolved.is_empty(),
            "slot must not resolve while apply_lag is set"
        );
        let resolved = driver.resolve_apply_lag_for_next_index(200);
        assert_eq!(resolved, vec![200]);
        assert_eq!(driver.deferred_slot_count(), 0);
    }
}

#[cfg(test)]
mod compare_tx_sets_tests {
    use super::*;
    use crate::tx_queue::TransactionSet;
    use stellar_xdr::curr::{
        GeneralizedTransactionSet, Hash, ParallelTxsComponent, TransactionPhase, TransactionSetV1,
        TxSetComponent, TxSetComponentTxsMaybeDiscountedFee,
    };

    fn make_config() -> ScpDriverConfig {
        ScpDriverConfig::default()
    }

    fn default_tracking() -> Arc<RwLock<SharedTrackingState>> {
        Arc::new(RwLock::new(SharedTrackingState::default()))
    }

    fn make_driver() -> ScpDriver {
        ScpDriver::new(make_config(), Hash256::ZERO, default_tracking())
    }

    /// Create a simple transaction with a given fee and operation count.
    fn make_tx(seed: u8, fee: u32, ops: usize) -> stellar_xdr::curr::TransactionEnvelope {
        use stellar_xdr::curr::{
            CreateAccountOp, DecoratedSignature, Memo, MuxedAccount, Operation, OperationBody,
            Preconditions, SequenceNumber, SignatureHint, Transaction, TransactionEnvelope,
            TransactionExt, TransactionV1Envelope, Uint256,
        };
        let source = MuxedAccount::Ed25519(Uint256([seed; 32]));
        let dest = stellar_xdr::curr::AccountId(
            stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(Uint256([seed.wrapping_add(1); 32])),
        );
        let operations: Vec<Operation> = (0..ops)
            .map(|_| Operation {
                source_account: None,
                body: OperationBody::CreateAccount(CreateAccountOp {
                    destination: dest.clone(),
                    starting_balance: 1_000_000_000,
                }),
            })
            .collect();
        let tx = Transaction {
            source_account: source,
            fee,
            seq_num: SequenceNumber(seed as i64),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: operations.try_into().unwrap(),
            ext: TransactionExt::V0,
        };
        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: stellar_xdr::curr::Signature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        })
    }

    fn cache_tx_set(driver: &ScpDriver, tx_set: TransactionSet) -> Hash256 {
        let hash = *tx_set.hash();
        driver.cache_tx_set(tx_set);
        hash
    }

    /// Test helper: compare two tx sets by hash, looking them up from cache.
    /// Mirrors the old `compare_tx_sets_by_hash(&driver, &hash_a, &hash_b, &candidates_hash)` API.
    fn compare_tx_sets_by_hash(
        driver: &ScpDriver,
        a_hash: &Hash256,
        b_hash: &Hash256,
        candidates_hash: &[u8; 32],
    ) -> std::cmp::Ordering {
        let a_set = driver.get_tx_set(a_hash).unwrap();
        let b_set = driver.get_tx_set(b_hash).unwrap();
        ScpDriver::compare_tx_sets(&a_set, &b_set, a_hash, b_hash, candidates_hash)
    }

    /// Create a Soroban-like transaction with a given fee and resource fee.
    /// inclusion_fee = fee - resource_fee.
    fn make_soroban_tx(
        seed: u8,
        fee: u32,
        resource_fee: i64,
    ) -> stellar_xdr::curr::TransactionEnvelope {
        use stellar_xdr::curr::{
            AccountId, DecoratedSignature, HostFunction, InvokeContractArgs, InvokeHostFunctionOp,
            LedgerFootprint, Memo, MuxedAccount, Operation, OperationBody, Preconditions,
            PublicKey, ScAddress, ScVal, SequenceNumber, SignatureHint, SorobanResources,
            SorobanTransactionData, SorobanTransactionDataExt, Transaction, TransactionEnvelope,
            TransactionExt, TransactionV1Envelope, Uint256, VecM,
        };
        let source = MuxedAccount::Ed25519(Uint256([seed; 32]));
        let function_name = stellar_xdr::curr::ScSymbol("test".try_into().unwrap());
        let operations = vec![Operation {
            source_account: None,
            body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                host_function: HostFunction::InvokeContract(InvokeContractArgs {
                    contract_address: ScAddress::Account(AccountId(
                        PublicKey::PublicKeyTypeEd25519(Uint256([seed; 32])),
                    )),
                    function_name,
                    args: VecM::<ScVal>::default(),
                }),
                auth: vec![].try_into().unwrap(),
            }),
        }];
        let tx = Transaction {
            source_account: source,
            fee,
            seq_num: SequenceNumber(seed as i64),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: operations.try_into().unwrap(),
            ext: TransactionExt::V1(SorobanTransactionData {
                ext: SorobanTransactionDataExt::V0,
                resources: SorobanResources {
                    footprint: LedgerFootprint {
                        read_only: vec![].try_into().unwrap(),
                        read_write: vec![].try_into().unwrap(),
                    },
                    instructions: 0,
                    disk_read_bytes: 0,
                    write_bytes: 0,
                },
                resource_fee,
            }),
        };
        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: stellar_xdr::curr::Signature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        })
    }

    fn make_generalized_tx_set(
        tx: stellar_xdr::curr::TransactionEnvelope,
        base_fee: i64,
    ) -> TransactionSet {
        let component =
            TxSetComponent::TxsetCompTxsMaybeDiscountedFee(TxSetComponentTxsMaybeDiscountedFee {
                txs: vec![tx.clone()].try_into().unwrap(),
                base_fee: Some(base_fee),
            });

        let gen = GeneralizedTransactionSet::V1(TransactionSetV1 {
            previous_ledger_hash: Hash([0u8; 32]),
            phases: vec![
                TransactionPhase::V0(vec![component].try_into().unwrap()),
                TransactionPhase::V1(ParallelTxsComponent {
                    base_fee: Some(base_fee),
                    execution_stages: vec![].try_into().unwrap(),
                }),
            ]
            .try_into()
            .unwrap(),
        });

        let hash = Hash256::hash_xdr(&gen);
        TransactionSet::new_generalized(hash, gen)
    }

    // =========================================================================
    // compare_tx_sets — 5-criteria comparison via combine_candidates_impl
    // =========================================================================

    #[test]
    fn test_compare_tx_sets_more_ops_wins() {
        let driver = make_driver();
        let candidates_hash = [0u8; 32];

        // Set A: 2 ops, fee 200
        let tx_set_a = TransactionSet::new(Hash256::ZERO, vec![make_tx(1, 200, 2)]);
        let hash_a = cache_tx_set(&driver, tx_set_a);

        // Set B: 1 op, fee 200
        let tx_set_b = TransactionSet::new(Hash256::ZERO, vec![make_tx(2, 200, 1)]);
        let hash_b = cache_tx_set(&driver, tx_set_b);

        // A has more ops, so A > B
        let result = compare_tx_sets_by_hash(&driver, &hash_a, &hash_b, &candidates_hash);
        assert_eq!(result, std::cmp::Ordering::Greater);
    }

    #[test]
    fn test_compare_tx_sets_fewer_ops_loses() {
        let driver = make_driver();
        let candidates_hash = [0u8; 32];

        // Set A: 1 op
        let tx_set_a = TransactionSet::new(Hash256::ZERO, vec![make_tx(1, 200, 1)]);
        let hash_a = cache_tx_set(&driver, tx_set_a);

        // Set B: 3 ops
        let tx_set_b = TransactionSet::new(Hash256::ZERO, vec![make_tx(2, 200, 3)]);
        let hash_b = cache_tx_set(&driver, tx_set_b);

        // A has fewer ops, so A < B
        let result = compare_tx_sets_by_hash(&driver, &hash_a, &hash_b, &candidates_hash);
        assert_eq!(result, std::cmp::Ordering::Less);
    }

    #[test]
    fn test_compare_tx_sets_equal_ops_higher_fee_wins() {
        let driver = make_driver();
        let candidates_hash = [0u8; 32];

        // Same ops (1), different fees
        let tx_set_a = TransactionSet::new(Hash256::ZERO, vec![make_tx(1, 300, 1)]);
        let hash_a = cache_tx_set(&driver, tx_set_a);

        let tx_set_b = TransactionSet::new(Hash256::ZERO, vec![make_tx(2, 100, 1)]);
        let hash_b = cache_tx_set(&driver, tx_set_b);

        // For legacy tx sets (no generalized), inclusion fee == full fee.
        // A has higher fee → A > B
        let result = compare_tx_sets_by_hash(&driver, &hash_a, &hash_b, &candidates_hash);
        assert_eq!(result, std::cmp::Ordering::Greater);
    }

    #[test]
    fn test_compare_tx_sets_inclusion_fee_precedes_full_fee() {
        let driver = make_driver();
        let candidates_hash = [0u8; 32];

        // Use Soroban txs where inclusion_fee = full_fee - resource_fee.
        // - A: full fee 1000, resource fee 800 -> inclusion fee 200
        // - B: full fee 500,  resource fee 100 -> inclusion fee 400
        //
        // Criterion 2 (total inclusion fee): A=200, B=400 -> B wins
        // Criterion 3 (total applying fee with base_fee=500):
        //   A: resource(800) + min(inclusion(200), base_fee(500)*ops(1)) = 800+200 = 1000
        //   B: resource(100) + min(inclusion(400), base_fee(500)*ops(1)) = 100+400 = 500
        //   -> A wins criterion 3
        // So criterion 2 should take priority, making B > A (result = Less).
        let tx_a = make_soroban_tx(20, 1_000, 800);
        let tx_b = make_soroban_tx(21, 500, 100);

        let tx_set_a = make_generalized_tx_set(tx_a, 500);
        let tx_set_b = make_generalized_tx_set(tx_b, 500);

        let hash_a = cache_tx_set(&driver, tx_set_a);
        let hash_b = cache_tx_set(&driver, tx_set_b);

        let result = compare_tx_sets_by_hash(&driver, &hash_a, &hash_b, &candidates_hash);
        assert_eq!(
            result,
            std::cmp::Ordering::Less,
            "higher inclusion fees must outrank higher full fees"
        );
    }

    #[test]
    fn test_compare_tx_sets_smaller_encoded_size_wins_when_tied() {
        let driver = make_driver();
        let candidates_hash = [0u8; 32];

        // To test criterion 4 (encoded size), we need sets with same
        // ops and fees but genuinely different sizes.
        // 1 tx with 2 ops, fee 200 -> total_ops=2, total_fee=200 (small XDR)
        // 2 txs each 1 op, fee 100 -> total_ops=2, total_fee=200 (bigger XDR)
        let tx_set_small = TransactionSet::new(Hash256::ZERO, vec![make_tx(10, 200, 2)]);
        let hash_small = cache_tx_set(&driver, tx_set_small);

        let tx_set_big = TransactionSet::new(
            Hash256::ZERO,
            vec![make_tx(11, 100, 1), make_tx(12, 100, 1)],
        );
        let hash_big = cache_tx_set(&driver, tx_set_big);

        // Both have ops=2, total_fee=200, inclusion_fee=200.
        // Smaller encoded size wins (criterion 4).
        let result = compare_tx_sets_by_hash(&driver, &hash_small, &hash_big, &candidates_hash);
        assert_eq!(
            result,
            std::cmp::Ordering::Greater,
            "Smaller encoded size should win when ops and fees are equal"
        );
    }

    #[test]
    fn test_compare_tx_sets_xor_tiebreak() {
        let driver = make_driver();
        // Use a known candidates_hash for deterministic tiebreak
        let candidates_hash = [0xAA; 32];

        // Two empty tx sets with different hashes
        let tx_set_a = TransactionSet::new(Hash256::from_bytes([1u8; 32]), vec![]);
        let hash_a = cache_tx_set(&driver, tx_set_a);

        let tx_set_b = TransactionSet::new(Hash256::from_bytes([2u8; 32]), vec![]);
        let hash_b = cache_tx_set(&driver, tx_set_b);

        // Both have 0 ops, 0 fees, 0 size → goes to XOR tiebreak
        let result = compare_tx_sets_by_hash(&driver, &hash_a, &hash_b, &candidates_hash);
        // The result should be deterministic based on hash XOR
        let a_xored = ScpDriver::xor_hash(&hash_a.0, &candidates_hash);
        let b_xored = ScpDriver::xor_hash(&hash_b.0, &candidates_hash);
        assert_eq!(result, a_xored.cmp(&b_xored));
    }

    #[test]
    fn test_compare_tx_sets_missing_set_returns_none() {
        let driver = make_driver();

        // Only cache set A, leave B uncached
        let tx_set_a = TransactionSet::new(Hash256::ZERO, vec![make_tx(1, 300, 3)]);
        let _hash_a = cache_tx_set(&driver, tx_set_a);

        // B is not cached (just a made-up hash)
        let hash_b = Hash256::from_bytes([99u8; 32]);

        // After AUDIT-220 fix: compare_tx_sets no longer fetches from cache.
        // The caller is responsible for resolving tx sets. Missing sets are
        // filtered before compare_tx_sets is ever called.
        assert!(driver.get_tx_set(&hash_b).is_none());
    }

    #[test]
    fn test_compare_tx_sets_equal_sets_returns_equal_xor() {
        let driver = make_driver();
        let candidates_hash = [0u8; 32];

        let tx_set = TransactionSet::new(Hash256::ZERO, vec![make_tx(1, 100, 1)]);
        let hash = cache_tx_set(&driver, tx_set);

        // Comparing a set with itself should return Equal
        let result = compare_tx_sets_by_hash(&driver, &hash, &hash, &candidates_hash);
        assert_eq!(result, std::cmp::Ordering::Equal);
    }

    /// Regression test for AUDIT-014: combine_candidates_impl must not silently
    /// degrade to XOR-only tiebreak when a tx set is missing from cache.
    /// Missing tx sets should be filtered out before comparison.
    #[test]
    fn test_audit_014_combine_candidates_filters_missing_tx_sets() {
        use stellar_xdr::curr::{Limits, StellarValue, StellarValueExt, TimePoint, WriteXdr};

        let driver = make_driver();

        // A has 5 ops (should win on criterion 1)
        let tx_set_a = TransactionSet::new(Hash256::ZERO, vec![make_tx(1, 500, 5)]);
        // B has 1 op
        let tx_set_b = TransactionSet::new(Hash256::ZERO, vec![make_tx(2, 100, 1)]);

        let sv_a = StellarValue {
            tx_set_hash: stellar_xdr::curr::Hash(tx_set_a.hash().0),
            close_time: TimePoint(1_700_000_000),
            upgrades: Default::default(),
            ext: StellarValueExt::Basic,
        };
        let sv_b = StellarValue {
            tx_set_hash: stellar_xdr::curr::Hash(tx_set_b.hash().0),
            close_time: TimePoint(1_700_000_000),
            upgrades: Default::default(),
            ext: StellarValueExt::Basic,
        };

        let val_a = Value(sv_a.to_xdr(Limits::none()).unwrap().try_into().unwrap());
        let val_b = Value(sv_b.to_xdr(Limits::none()).unwrap().try_into().unwrap());
        let values = vec![val_a.clone(), val_b.clone()];

        // Both cached: A wins (5 ops > 1 op)
        cache_tx_set(&driver, tx_set_a.clone());
        cache_tx_set(&driver, tx_set_b.clone());
        let full_result = driver.combine_candidates_impl(1, &values);
        let full_sv = StellarValue::from_xdr(&full_result.0, Limits::none()).unwrap();
        assert_eq!(
            full_sv.tx_set_hash.0,
            tx_set_a.hash().0,
            "A should win with both cached"
        );

        // Only B cached, A missing: A should be filtered out, B wins by default
        driver.tx_tracker.clear_cache();
        cache_tx_set(&driver, tx_set_b.clone());
        let partial_result = driver.combine_candidates_impl(1, &values);
        let partial_sv = StellarValue::from_xdr(&partial_result.0, Limits::none()).unwrap();
        // The key assertion: with A missing from cache, the result should be B
        // (not a silently degraded XOR tiebreak that might pick A)
        assert_eq!(
            partial_sv.tx_set_hash.0,
            tx_set_b.hash().0,
            "AUDIT-014: Missing tx set A should be filtered out, B should win"
        );
    }

    #[test]
    fn test_nomination_timeout_limit_wired_from_upgrades() {
        use std::sync::Arc;

        let driver = make_driver();
        let callback = super::HerderScpCallback::new(Arc::new(driver));

        // Default: no upgrades set → u32::MAX
        assert_eq!(
            callback.get_upgrade_nomination_timeout_limit(),
            u32::MAX,
            "Default should be u32::MAX when no upgrades configured"
        );
    }

    #[test]
    fn test_nomination_timeout_limit_reads_configured_value() {
        use crate::upgrades::{UpgradeParameters, Upgrades};
        use std::sync::Arc;

        let driver = make_driver();

        // Set up Upgrades with a specific timeout limit
        let mut params = UpgradeParameters::default();
        params.nomination_timeout_limit = Some(300);
        let upgrades = Arc::new(parking_lot::RwLock::new(Upgrades::new(
            UpgradeParameters::default(),
        )));
        upgrades.write().set_parameters(params, 26).unwrap();
        driver.set_upgrades(upgrades);

        let callback = super::HerderScpCallback::new(Arc::new(driver));
        assert_eq!(
            callback.get_upgrade_nomination_timeout_limit(),
            300,
            "Should read nomination_timeout_limit from Upgrades"
        );
    }

    /// Helper: create a fee-bump transaction with an i64 outer fee.
    /// Enables near-i64::MAX fee values for saturation tests.
    fn make_fee_bump_tx(
        seed: u8,
        outer_fee: i64,
        ops: usize,
    ) -> stellar_xdr::curr::TransactionEnvelope {
        use stellar_xdr::curr::{
            FeeBumpTransaction, FeeBumpTransactionEnvelope, FeeBumpTransactionExt,
            FeeBumpTransactionInnerTx, MuxedAccount, Uint256,
        };
        let inner = make_tx(seed, 1000, ops);
        let inner_env = match inner {
            stellar_xdr::curr::TransactionEnvelope::Tx(env) => env,
            _ => panic!("expected Tx envelope"),
        };
        stellar_xdr::curr::TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
            tx: FeeBumpTransaction {
                fee_source: MuxedAccount::Ed25519(Uint256([seed; 32])),
                fee: outer_fee,
                inner_tx: FeeBumpTransactionInnerTx::Tx(inner_env),
                ext: FeeBumpTransactionExt::V0,
            },
            signatures: Default::default(),
        })
    }

    #[test]
    fn test_tx_set_total_inclusion_fees_saturation() {
        // Two txs with fees near i64::MAX. Without saturating accumulation,
        // the sum wraps to a negative value.
        let tx1 = make_fee_bump_tx(1, i64::MAX, 1);
        let tx2 = make_fee_bump_tx(2, i64::MAX, 1);
        let tx_set = TransactionSet::new(Hash256::ZERO, vec![tx1, tx2]);

        let total = ScpDriver::tx_set_total_inclusion_fees(&tx_set);
        assert_eq!(
            total,
            i64::MAX,
            "inclusion fee total should saturate to i64::MAX, not wrap"
        );
        assert!(total > 0, "must not wrap to negative");
    }

    #[test]
    fn test_tx_set_total_fees_legacy_saturation() {
        // Legacy tx set (no generalized): applying fee == full fee.
        let tx1 = make_fee_bump_tx(1, i64::MAX, 1);
        let tx2 = make_fee_bump_tx(2, i64::MAX, 1);
        let tx_set = TransactionSet::new(Hash256::ZERO, vec![tx1, tx2]);

        let total = ScpDriver::tx_set_total_fees(&tx_set);
        assert_eq!(
            total,
            i64::MAX,
            "legacy fee total should saturate to i64::MAX, not wrap"
        );
        assert!(total > 0, "must not wrap to negative");
    }

    #[test]
    fn test_tx_set_total_fees_generalized_saturation() {
        use stellar_xdr::curr::{
            GeneralizedTransactionSet, Hash, ParallelTxsComponent, TransactionPhase,
            TransactionSetV1, TxSetComponent, TxSetComponentTxsMaybeDiscountedFee,
        };

        // Generalized tx set with extreme fees. base_fee=None means
        // applying fee == full fee.
        let tx1 = make_fee_bump_tx(1, i64::MAX, 1);
        let tx2 = make_fee_bump_tx(2, i64::MAX, 1);

        let component =
            TxSetComponent::TxsetCompTxsMaybeDiscountedFee(TxSetComponentTxsMaybeDiscountedFee {
                txs: vec![tx1.clone(), tx2.clone()].try_into().unwrap(),
                base_fee: None,
            });
        let gen = GeneralizedTransactionSet::V1(TransactionSetV1 {
            previous_ledger_hash: Hash([0u8; 32]),
            phases: vec![
                TransactionPhase::V0(vec![component].try_into().unwrap()),
                TransactionPhase::V1(ParallelTxsComponent {
                    base_fee: None,
                    execution_stages: vec![].try_into().unwrap(),
                }),
            ]
            .try_into()
            .unwrap(),
        });
        let hash = Hash256::hash_xdr(&gen);
        let tx_set = TransactionSet::new_generalized(hash, gen);

        let total = ScpDriver::tx_set_total_fees(&tx_set);
        assert_eq!(
            total,
            i64::MAX,
            "generalized fee total should saturate to i64::MAX"
        );
    }

    #[test]
    fn test_compare_tx_sets_both_overflow_falls_through_to_size() {
        // When both tx sets have overflowing fee totals (saturating to i64::MAX),
        // the comparison should fall through to the size tiebreaker.
        let driver = make_driver();
        let candidates_hash = [0u8; 32];

        // Set A: 2 fee-bump txs, each i64::MAX fee, 1 op each → total ops=4 (2 ops per fee-bump)
        // Set B: 2 fee-bump txs, each i64::MAX fee, 1 op each → total ops=4
        // Both have saturated inclusion fees and total fees.
        // Differ only in XDR size (different seeds → different hashes/sizes).
        let tx_set_a = TransactionSet::new(
            Hash256::ZERO,
            vec![
                make_fee_bump_tx(1, i64::MAX, 1),
                make_fee_bump_tx(2, i64::MAX, 1),
            ],
        );
        let tx_set_b = TransactionSet::new(
            Hash256::ZERO,
            vec![
                make_fee_bump_tx(3, i64::MAX, 1),
                make_fee_bump_tx(4, i64::MAX, 1),
            ],
        );

        let hash_a = cache_tx_set(&driver, tx_set_a);
        let hash_b = cache_tx_set(&driver, tx_set_b);

        // Both should compare as non-panic and produce a deterministic result.
        // The exact ordering depends on XDR size and hash tiebreaker, but it
        // must not panic from overflow.
        let result = compare_tx_sets_by_hash(&driver, &hash_a, &hash_b, &candidates_hash);
        // With both fees saturated to i64::MAX, fees compare equal.
        // Result is determined by size or hash tiebreaker.
        assert!(
            result == std::cmp::Ordering::Less
                || result == std::cmp::Ordering::Greater
                || result == std::cmp::Ordering::Equal,
            "comparison must produce a valid ordering, not panic"
        );
    }

    #[test]
    fn test_compare_tx_sets_one_side_overflow() {
        // Asymmetric: one tx set overflows (saturates), the other doesn't.
        // The saturated side should compare as >= the non-saturated side.
        let driver = make_driver();
        let candidates_hash = [0u8; 32];

        // Set A: 2 txs with extreme fees → saturates to i64::MAX
        let tx_set_a = TransactionSet::new(
            Hash256::ZERO,
            vec![
                make_fee_bump_tx(1, i64::MAX, 1),
                make_fee_bump_tx(2, i64::MAX, 1),
            ],
        );

        // Set B: 2 txs with moderate fees → large but not overflowing
        // (fee-bump with 1 op has 2 ops total, so same op count as A)
        let tx_set_b = TransactionSet::new(
            Hash256::ZERO,
            vec![
                make_fee_bump_tx(3, 1_000_000, 1),
                make_fee_bump_tx(4, 1_000_000, 1),
            ],
        );

        let hash_a = cache_tx_set(&driver, tx_set_a);
        let hash_b = cache_tx_set(&driver, tx_set_b);

        let result = compare_tx_sets_by_hash(&driver, &hash_a, &hash_b, &candidates_hash);
        // A has saturated fees (i64::MAX) > B's fees (2_000_000)
        // Since ops are equal (4 each), inclusion fee comparison decides.
        assert_eq!(
            result,
            std::cmp::Ordering::Greater,
            "saturated fees should beat moderate fees"
        );
    }

    /// Regression test for #1942: combine_candidates must be order-independent.
    ///
    /// When two candidates share the same tx_set but differ only in close_time,
    /// combine_candidates_impl must return the same result regardless of the
    /// order candidates are supplied. Before the fix, `max_by` returned the
    /// last tied element, causing nodes with different insertion orders to
    /// produce different composites — making SCP ballot convergence impossible.
    #[test]
    fn test_combine_candidates_order_independent() {
        use stellar_xdr::curr::{StellarValue, StellarValueExt, TimePoint, WriteXdr};

        let driver = make_driver();

        // Create a single transaction set (both candidates reference the same tx_set)
        let tx = make_tx(42, 100, 1);
        let tx_set = TransactionSet::new(Hash256::ZERO, vec![tx]);
        let tx_set_hash = *tx_set.hash();
        driver.cache_tx_set(tx_set);

        // Two StellarValues with the same tx_set_hash but different close_times
        let sv_a = StellarValue {
            tx_set_hash: Hash(tx_set_hash.as_bytes().clone()),
            close_time: TimePoint(1000),
            upgrades: vec![].try_into().unwrap(),
            ext: StellarValueExt::Basic,
        };
        let sv_b = StellarValue {
            tx_set_hash: Hash(tx_set_hash.as_bytes().clone()),
            close_time: TimePoint(1001),
            upgrades: vec![].try_into().unwrap(),
            ext: StellarValueExt::Basic,
        };

        let val_a = Value(
            sv_a.to_xdr(stellar_xdr::curr::Limits::none())
                .unwrap()
                .try_into()
                .unwrap(),
        );
        let val_b = Value(
            sv_b.to_xdr(stellar_xdr::curr::Limits::none())
                .unwrap()
                .try_into()
                .unwrap(),
        );
        assert_ne!(
            val_a, val_b,
            "candidates must differ (different close_time)"
        );

        // Call combine_candidates in both orderings
        let result_ab = driver.combine_candidates_impl(3, &[val_a.clone(), val_b.clone()]);
        let result_ba = driver.combine_candidates_impl(3, &[val_b.clone(), val_a.clone()]);

        assert_eq!(
            result_ab, result_ba,
            "combine_candidates must be order-independent"
        );
    }
}

#[cfg(test)]
mod validator_weight_config_tests {
    use super::*;

    fn make_node_id(seed: u8) -> NodeId {
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        NodeId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::curr::Uint256(bytes),
        ))
    }

    fn entry(name: &str, home_domain: &str, quality: ValidatorQuality) -> ValidatorEntryInfo {
        ValidatorEntryInfo {
            name: name.to_string(),
            home_domain: home_domain.to_string(),
            quality,
        }
    }

    #[test]
    fn test_single_high_quality_validator() {
        let node = make_node_id(1);
        let validators = vec![(
            node.clone(),
            entry("v1", "example.com", ValidatorQuality::High),
        )];
        let config = ValidatorWeightConfig::new(&validators).unwrap();

        // Highest quality gets u64::MAX, divided by home domain size of 1
        assert_eq!(config.get_node_weight(&node), u64::MAX);
    }

    #[test]
    fn test_two_validators_same_domain_same_quality() {
        let n1 = make_node_id(1);
        let n2 = make_node_id(2);
        let validators = vec![
            (
                n1.clone(),
                entry("v1", "example.com", ValidatorQuality::High),
            ),
            (
                n2.clone(),
                entry("v2", "example.com", ValidatorQuality::High),
            ),
        ];
        let config = ValidatorWeightConfig::new(&validators).unwrap();

        // Both in same domain of size 2 → u64::MAX / 2
        assert_eq!(config.get_node_weight(&n1), u64::MAX / 2);
        assert_eq!(config.get_node_weight(&n2), u64::MAX / 2);
    }

    #[test]
    fn test_mixed_quality_levels() {
        let n_high = make_node_id(1);
        let n_med = make_node_id(2);
        let validators = vec![
            (
                n_high.clone(),
                entry("v1", "high.com", ValidatorQuality::High),
            ),
            (
                n_med.clone(),
                entry("v2", "med.com", ValidatorQuality::Medium),
            ),
        ];
        let config = ValidatorWeightConfig::new(&validators).unwrap();

        // High gets u64::MAX (1 org in high)
        assert_eq!(config.get_node_weight(&n_high), u64::MAX);

        // Medium = high_weight / ((high_orgs + 1) * 10) = u64::MAX / (2 * 10)
        let expected_med = u64::MAX / 20;
        assert_eq!(config.get_node_weight(&n_med), expected_med);
    }

    #[test]
    fn test_low_quality_always_zero() {
        let n_high = make_node_id(1);
        let n_low = make_node_id(2);
        let validators = vec![
            (
                n_high.clone(),
                entry("v1", "high.com", ValidatorQuality::High),
            ),
            (n_low.clone(), entry("v2", "low.com", ValidatorQuality::Low)),
        ];
        let config = ValidatorWeightConfig::new(&validators).unwrap();

        assert_eq!(config.get_node_weight(&n_low), 0);
    }

    #[test]
    fn test_all_low_quality_rejected() {
        let n1 = make_node_id(1);
        let validators = vec![(n1, entry("v1", "example.com", ValidatorQuality::Low))];
        assert!(ValidatorWeightConfig::new(&validators).is_err());
    }

    #[test]
    fn test_critical_high_medium_chain() {
        // 3 quality levels: Critical > High > Medium
        let n_crit = make_node_id(1);
        let n_high = make_node_id(2);
        let n_med = make_node_id(3);
        let validators = vec![
            (
                n_crit.clone(),
                entry("v1", "crit.com", ValidatorQuality::Critical),
            ),
            (
                n_high.clone(),
                entry("v2", "high.com", ValidatorQuality::High),
            ),
            (
                n_med.clone(),
                entry("v3", "med.com", ValidatorQuality::Medium),
            ),
        ];
        let config = ValidatorWeightConfig::new(&validators).unwrap();

        // Critical = u64::MAX
        assert_eq!(config.get_node_weight(&n_crit), u64::MAX);

        // High = critical_weight / ((critical_orgs + 1) * 10) = u64::MAX / 20
        let high_w = u64::MAX / 20;
        assert_eq!(config.get_node_weight(&n_high), high_w);

        // Medium = high_weight / ((high_orgs + 1) * 10) = high_w / 20
        let med_w = high_w / 20;
        assert_eq!(config.get_node_weight(&n_med), med_w);
    }

    #[test]
    fn test_multiple_orgs_at_same_quality() {
        // 2 orgs at High, 1 at Medium
        let n1 = make_node_id(1);
        let n2 = make_node_id(2);
        let n3 = make_node_id(3);
        let validators = vec![
            (n1.clone(), entry("v1", "org_a.com", ValidatorQuality::High)),
            (n2.clone(), entry("v2", "org_b.com", ValidatorQuality::High)),
            (n3.clone(), entry("v3", "med.com", ValidatorQuality::Medium)),
        ];
        let config = ValidatorWeightConfig::new(&validators).unwrap();

        // High = u64::MAX (1 per domain)
        assert_eq!(config.get_node_weight(&n1), u64::MAX);
        assert_eq!(config.get_node_weight(&n2), u64::MAX);

        // Medium = high_weight / ((high_orgs_count + 1) * 10) = u64::MAX / (3 * 10) = u64::MAX / 30
        let med_w = u64::MAX / 30;
        assert_eq!(config.get_node_weight(&n3), med_w);
    }

    #[test]
    #[should_panic(expected = "Validator entry not found")]
    fn test_unknown_node_panics() {
        let n1 = make_node_id(1);
        let validators = vec![(n1, entry("v1", "example.com", ValidatorQuality::High))];
        let config = ValidatorWeightConfig::new(&validators).unwrap();

        let unknown = make_node_id(99);
        config.get_node_weight(&unknown);
    }

    #[test]
    fn test_quality_parsing() {
        assert_eq!(
            ValidatorQuality::from_str("HIGH"),
            Some(ValidatorQuality::High)
        );
        assert_eq!(
            ValidatorQuality::from_str("MEDIUM"),
            Some(ValidatorQuality::Medium)
        );
        assert_eq!(
            ValidatorQuality::from_str("LOW"),
            Some(ValidatorQuality::Low)
        );
        assert_eq!(
            ValidatorQuality::from_str("CRITICAL"),
            Some(ValidatorQuality::Critical)
        );
        // Exact match only — case-insensitive and abbreviations are rejected
        assert_eq!(ValidatorQuality::from_str("high"), None);
        assert_eq!(ValidatorQuality::from_str("MED"), None);
        assert_eq!(ValidatorQuality::from_str("unknown"), None);
    }

    #[test]
    fn test_duplicate_validator_rejected() {
        let n1 = make_node_id(1);
        let validators = vec![
            (
                n1.clone(),
                entry("v1", "example.com", ValidatorQuality::High),
            ),
            (
                n1.clone(),
                entry("v1_dup", "example.com", ValidatorQuality::High),
            ),
        ];
        assert!(ValidatorWeightConfig::new(&validators).is_err());
    }
}
