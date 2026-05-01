//! Runtime invariant checks for henyey.
//!
//! This crate provides the [`InvariantManager`] and [`Invariant`] trait, mirroring
//! stellar-core's `InvariantManagerImpl` / `Invariant` subsystem. Invariants are
//! read-only checks that run after each operation apply (and, in future, at other
//! hook points) to detect ledger corruption early.
//!
//! # Parity
//!
//! - stellar-core reference: `src/invariant/InvariantManagerImpl.cpp`, `src/invariant/Invariant.h`
//! - All invariants use `Invariant(false)` (non-strict) unless otherwise noted.
//! - Strict invariants panic on failure; non-strict invariants log and continue.

mod entry_valid;
mod sponsorship;
mod sub_entries;

pub use entry_valid::LedgerEntryIsValid;
pub use sponsorship::SponsorshipCountIsValid;
pub use sub_entries::AccountSubEntriesCountIsValid;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use stellar_xdr::curr::{
    ContractEvent, LedgerEntry, LedgerHeader, LedgerKey, Operation, OperationResult,
};

// ---------------------------------------------------------------------------
// Context types
// ---------------------------------------------------------------------------

/// Per-operation delta context passed to invariant checks.
///
/// Uses only `stellar-xdr` types so this crate avoids depending on higher-level
/// henyey crates. The caller (in `henyey-ledger`) constructs this from the
/// existing `DeltaSlice` and ledger headers.
pub struct OperationDelta<'a> {
    /// Entries created by this operation.
    pub created: &'a [LedgerEntry],
    /// Entries updated by this operation (post-state).
    pub updated: &'a [LedgerEntry],
    /// Pre-states for updated entries (parallel to `updated`).
    pub update_states: &'a [LedgerEntry],
    /// Keys of entries deleted by this operation.
    pub deleted: &'a [LedgerKey],
    /// Pre-states for deleted entries (parallel to `deleted`).
    pub delete_states: &'a [LedgerEntry],
    /// Current ledger sequence number.
    pub ledger_seq: u32,
    /// Current protocol version.
    pub ledger_version: u32,
    /// Current ledger header (after this operation's header mutations).
    /// `None` until ConservationOfLumens invariant is implemented.
    pub header_current: Option<&'a LedgerHeader>,
    /// Previous ledger header (before this ledger's transactions).
    /// `None` until ConservationOfLumens invariant is implemented.
    pub header_previous: Option<&'a LedgerHeader>,
    /// Network ID (SHA-256 of passphrase).
    pub network_id: &'a [u8; 32],
}

// ---------------------------------------------------------------------------
// Invariant trait
// ---------------------------------------------------------------------------

/// A single invariant check.
///
/// Implementations are stateless — all context is received via method parameters.
/// The trait requires `Send + Sync` because invariants may be invoked from
/// parallel Soroban cluster execution or background snapshot tasks.
///
/// # Parity
///
/// Maps to stellar-core's `Invariant` base class (`src/invariant/Invariant.h`).
pub trait Invariant: Send + Sync {
    /// Human-readable name (e.g. `"AccountSubEntriesCountIsValid"`).
    fn name(&self) -> &str;

    /// Whether failure should abort the process.
    ///
    /// - `true` (strict): failure panics (matching stellar-core `throw InvariantDoesNotHold`)
    /// - `false` (non-strict): failure logs an error and continues
    fn is_strict(&self) -> bool;

    /// Called after each successful operation apply.
    ///
    /// Returns `Ok(())` if the invariant holds, or `Err(message)` describing
    /// the violation.
    fn check_on_operation_apply(
        &self,
        _operation: &Operation,
        _op_result: &OperationResult,
        _delta: &OperationDelta<'_>,
        _events: &[ContractEvent],
    ) -> Result<(), String> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Failure tracking
// ---------------------------------------------------------------------------

/// Information about the last failure of an invariant.
#[derive(Debug, Clone)]
pub struct FailureInfo {
    pub last_failed_on_ledger: u32,
    pub last_failed_with_message: String,
}

// ---------------------------------------------------------------------------
// InvariantManager
// ---------------------------------------------------------------------------

/// Registry and dispatcher for runtime invariant checks.
///
/// # Parity
///
/// Maps to stellar-core's `InvariantManagerImpl` (`src/invariant/InvariantManagerImpl.cpp`).
pub struct InvariantManager {
    /// All registered invariants (name → impl).
    registered: HashMap<String, Arc<dyn Invariant>>,
    /// Enabled subset (order matches enable calls).
    enabled: Vec<Arc<dyn Invariant>>,
    /// Per-invariant failure information.
    failure_info: Mutex<HashMap<String, FailureInfo>>,
    /// Total failure count across all invariants.
    failure_count: AtomicU64,
}

impl InvariantManager {
    /// Create a new empty manager.
    pub fn new() -> Self {
        Self {
            registered: HashMap::new(),
            enabled: Vec::new(),
            failure_info: Mutex::new(HashMap::new()),
            failure_count: AtomicU64::new(0),
        }
    }

    /// Register an invariant. Panics if the name is already registered.
    ///
    /// # Parity
    ///
    /// `InvariantManagerImpl::registerInvariant` (`InvariantManagerImpl.cpp:204-217`)
    pub fn register(&mut self, inv: Arc<dyn Invariant>) {
        let name = inv.name().to_string();
        if self.registered.contains_key(&name) {
            panic!("Invariant {} already registered", name);
        }
        self.registered.insert(name, inv);
    }

    /// Enable invariants whose names match `pattern` (case-insensitive full match).
    ///
    /// Uses the Rust `regex` crate. stellar-core uses ECMAScript regex; Rust regex
    /// does not support lookahead/lookbehind assertions. All known production
    /// patterns (`".*"`, exact invariant names) are compatible.
    ///
    /// # Errors
    ///
    /// - Empty pattern
    /// - Pattern fails to compile
    /// - No registered invariant matches
    /// - An invariant matching the pattern is already enabled
    ///
    /// # Parity
    ///
    /// `InvariantManagerImpl::enableInvariant` (`InvariantManagerImpl.cpp:220-281`)
    pub fn enable(&mut self, pattern: &str) -> Result<(), String> {
        if pattern.is_empty() {
            return Err("Invariant pattern must be non empty".to_string());
        }

        let re = regex::RegexBuilder::new(&format!("^(?:{})$", pattern))
            .case_insensitive(true)
            .build()
            .map_err(|e| {
                format!(
                    "Invalid invariant pattern '{}': {}. Note: henyey uses Rust regex \
                     which does not support lookahead/lookbehind assertions.",
                    pattern, e
                )
            })?;

        let mut enabled_some = false;
        // Collect matches first to avoid borrow issues.
        let matches: Vec<(String, Arc<dyn Invariant>)> = self
            .registered
            .iter()
            .filter(|(name, _)| re.is_match(name))
            .map(|(name, inv)| (name.clone(), Arc::clone(inv)))
            .collect();

        for (name, inv) in matches {
            if self.enabled.iter().any(|e| e.name() == name) {
                return Err(format!("Invariant {} already enabled", name));
            }
            enabled_some = true;
            tracing::info!(invariant = %name, "Enabled invariant");
            self.enabled.push(inv);
        }

        if !enabled_some {
            let registered: Vec<&str> = self.registered.keys().map(|s| s.as_str()).collect();
            let msg = if registered.is_empty() {
                format!(
                    "Invariant pattern '{}' did not match any invariants. \
                     There are no registered invariants",
                    pattern
                )
            } else {
                format!(
                    "Invariant pattern '{}' did not match any invariants. \
                     Registered invariants are: {}",
                    pattern,
                    registered.join(", ")
                )
            };
            return Err(msg);
        }

        Ok(())
    }

    /// List enabled invariant names.
    pub fn get_enabled_invariants(&self) -> Vec<String> {
        self.enabled
            .iter()
            .map(|inv| inv.name().to_string())
            .collect()
    }

    /// Dispatch `check_on_operation_apply` to all enabled invariants.
    ///
    /// On failure:
    /// - Non-strict: logs error, records failure info, continues
    /// - Strict: logs error, records failure info, panics
    ///
    /// # Parity
    ///
    /// `InvariantManagerImpl::checkOnOperationApply` (`InvariantManagerImpl.cpp:144-173`)
    pub fn check_on_operation_apply(
        &self,
        operation: &Operation,
        op_result: &OperationResult,
        delta: &OperationDelta<'_>,
        events: &[ContractEvent],
    ) {
        for inv in &self.enabled {
            match inv.check_on_operation_apply(operation, op_result, delta, events) {
                Ok(()) => {}
                Err(message) => {
                    let full_message = format!(
                        "Invariant \"{}\" does not hold on operation: {}",
                        inv.name(),
                        message
                    );
                    self.on_invariant_failure(
                        inv.name(),
                        inv.is_strict(),
                        &full_message,
                        delta.ledger_seq,
                    );
                }
            }
        }
    }

    /// Record a failure and handle according to strictness.
    fn on_invariant_failure(&self, name: &str, is_strict: bool, message: &str, ledger: u32) {
        self.failure_count.fetch_add(1, Ordering::Relaxed);
        metrics::counter!("stellar_ledger_invariant_failure_total").increment(1);

        {
            let mut info = self.failure_info.lock().unwrap();
            info.insert(
                name.to_string(),
                FailureInfo {
                    last_failed_on_ledger: ledger,
                    last_failed_with_message: message.to_string(),
                },
            );
        }

        if is_strict {
            tracing::error!(invariant = name, ledger = ledger, "FATAL: {}", message);
            tracing::error!("unexpected error: please report this bug along with logs");
            panic!("{}", message);
        } else {
            tracing::error!(invariant = name, ledger = ledger, "{}", message);
            tracing::error!("unexpected error: please report this bug along with logs");
        }
    }

    /// JSON info for the `/info` endpoint.
    ///
    /// Returns `{}` when no failures. Otherwise:
    /// ```json
    /// {
    ///   "InvariantName": {
    ///     "last_failed_on_ledger": 12345,
    ///     "last_failed_with_message": "..."
    ///   },
    ///   "count": 3
    /// }
    /// ```
    ///
    /// # Parity
    ///
    /// `InvariantManagerImpl::getJsonInfo` (`InvariantManagerImpl.cpp:57-74`)
    pub fn get_json_info(&self) -> serde_json::Value {
        let info = self.failure_info.lock().unwrap();
        if info.is_empty() {
            return serde_json::json!({});
        }

        let mut result = serde_json::Map::new();
        for (name, fi) in info.iter() {
            result.insert(
                name.clone(),
                serde_json::json!({
                    "last_failed_on_ledger": fi.last_failed_on_ledger,
                    "last_failed_with_message": fi.last_failed_with_message,
                }),
            );
        }
        result.insert(
            "count".to_string(),
            serde_json::json!(self.failure_count.load(Ordering::Relaxed)),
        );
        serde_json::Value::Object(result)
    }
}

impl Default for InvariantManager {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal invariant for testing.
    struct TestInvariant {
        name: String,
        strict: bool,
        fail: bool,
    }

    impl Invariant for TestInvariant {
        fn name(&self) -> &str {
            &self.name
        }
        fn is_strict(&self) -> bool {
            self.strict
        }
        fn check_on_operation_apply(
            &self,
            _operation: &Operation,
            _op_result: &OperationResult,
            _delta: &OperationDelta<'_>,
            _events: &[ContractEvent],
        ) -> Result<(), String> {
            if self.fail {
                Err("test failure".to_string())
            } else {
                Ok(())
            }
        }
    }

    fn test_inv(name: &str, strict: bool, fail: bool) -> Arc<dyn Invariant> {
        Arc::new(TestInvariant {
            name: name.to_string(),
            strict,
            fail,
        })
    }

    #[test]
    fn test_register_and_enable_exact_name() {
        let mut mgr = InvariantManager::new();
        mgr.register(test_inv("FooInvariant", false, false));
        mgr.register(test_inv("BarInvariant", false, false));

        mgr.enable("FooInvariant").unwrap();
        assert_eq!(mgr.get_enabled_invariants(), vec!["FooInvariant"]);
    }

    #[test]
    fn test_enable_wildcard() {
        let mut mgr = InvariantManager::new();
        mgr.register(test_inv("FooInvariant", false, false));
        mgr.register(test_inv("BarInvariant", false, false));

        mgr.enable(".*").unwrap();
        let mut enabled = mgr.get_enabled_invariants();
        enabled.sort();
        assert_eq!(enabled, vec!["BarInvariant", "FooInvariant"]);
    }

    #[test]
    fn test_enable_case_insensitive() {
        let mut mgr = InvariantManager::new();
        mgr.register(test_inv("FooInvariant", false, false));

        mgr.enable("fooinvariant").unwrap();
        assert_eq!(mgr.get_enabled_invariants(), vec!["FooInvariant"]);
    }

    #[test]
    fn test_enable_empty_pattern_error() {
        let mut mgr = InvariantManager::new();
        mgr.register(test_inv("Foo", false, false));
        assert!(mgr.enable("").is_err());
    }

    #[test]
    fn test_enable_no_match_error() {
        let mut mgr = InvariantManager::new();
        mgr.register(test_inv("Foo", false, false));
        let err = mgr.enable("NonExistent").unwrap_err();
        assert!(err.contains("did not match"));
        assert!(err.contains("Foo"));
    }

    #[test]
    fn test_enable_duplicate_error() {
        let mut mgr = InvariantManager::new();
        mgr.register(test_inv("Foo", false, false));
        mgr.enable("Foo").unwrap();
        let err = mgr.enable("Foo").unwrap_err();
        assert!(err.contains("already enabled"));
    }

    #[test]
    #[should_panic(expected = "already registered")]
    fn test_register_duplicate_panics() {
        let mut mgr = InvariantManager::new();
        mgr.register(test_inv("Foo", false, false));
        mgr.register(test_inv("Foo", false, false));
    }

    #[test]
    fn test_json_info_empty_when_no_failures() {
        let mgr = InvariantManager::new();
        assert_eq!(mgr.get_json_info(), serde_json::json!({}));
    }

    #[test]
    fn test_non_strict_failure_logs_but_continues() {
        let mut mgr = InvariantManager::new();
        mgr.register(test_inv("NonStrict", false, true));
        mgr.enable("NonStrict").unwrap();

        let header = default_header(100);
        let prev_header = default_header(99);
        let network_id = [0u8; 32];
        let delta = OperationDelta {
            created: &[],
            updated: &[],
            update_states: &[],
            deleted: &[],
            delete_states: &[],
            ledger_seq: 100,
            ledger_version: 24,
            header_current: Some(&header),
            header_previous: Some(&prev_header),
            network_id: &network_id,
        };

        let op = dummy_operation();
        let op_result = dummy_op_result();

        // Should not panic.
        mgr.check_on_operation_apply(&op, &op_result, &delta, &[]);

        let info = mgr.get_json_info();
        assert!(info.get("NonStrict").is_some());
        assert_eq!(info.get("count").unwrap(), 1);
    }

    #[test]
    #[should_panic(expected = "test failure")]
    fn test_strict_failure_panics() {
        let mut mgr = InvariantManager::new();
        mgr.register(test_inv("Strict", true, true));
        mgr.enable("Strict").unwrap();

        let header = default_header(100);
        let prev_header = default_header(99);
        let network_id = [0u8; 32];
        let delta = OperationDelta {
            created: &[],
            updated: &[],
            update_states: &[],
            deleted: &[],
            delete_states: &[],
            ledger_seq: 100,
            ledger_version: 24,
            header_current: Some(&header),
            header_previous: Some(&prev_header),
            network_id: &network_id,
        };

        let op = dummy_operation();
        let op_result = dummy_op_result();
        mgr.check_on_operation_apply(&op, &op_result, &delta, &[]);
    }

    // Helpers

    fn default_header(seq: u32) -> LedgerHeader {
        use stellar_xdr::curr::*;
        LedgerHeader {
            ledger_version: 24,
            previous_ledger_hash: Hash([0; 32]),
            scp_value: StellarValue {
                tx_set_hash: Hash([0; 32]),
                close_time: TimePoint(0),
                upgrades: vec![].try_into().unwrap(),
                ext: StellarValueExt::Basic,
            },
            tx_set_result_hash: Hash([0; 32]),
            bucket_list_hash: Hash([0; 32]),
            ledger_seq: seq,
            total_coins: 0,
            fee_pool: 0,
            inflation_seq: 0,
            id_pool: 0,
            base_fee: 100,
            base_reserve: 5000000,
            max_tx_set_size: 1000,
            skip_list: [Hash([0; 32]), Hash([0; 32]), Hash([0; 32]), Hash([0; 32])],
            ext: LedgerHeaderExt::V0,
        }
    }

    fn dummy_operation() -> Operation {
        use stellar_xdr::curr::*;
        Operation {
            source_account: None,
            body: OperationBody::Inflation,
        }
    }

    fn dummy_op_result() -> OperationResult {
        use stellar_xdr::curr::*;
        OperationResult::OpInner(OperationResultTr::Inflation(InflationResult::NotTime))
    }
}
